//! Chemin **natif RDNA 2** de `d3d12_xs` (sous-module de `d3d12_xs`, voir `docs/rdna2_pipeline.md`).
//!
//! Lorsqu'un PSO reçoit des shaders au format `SBR2` (`Rdna2Native`) :
//!
//! 1. **Création** : les métadonnées des conteneurs sont conservées avec le PSO ; le pipeline
//!    Vulkan est créé par `VulkanRenderer::create_native_*_pipeline`.
//! 2. **Enregistrement** : les arguments racine (`SetGraphicsRoot*` / `SetComputeRoot*`) sont
//!    suivis par la liste de commandes (`RootBinding`) ; au `Draw` / `Dispatch`, ils sont compactés
//!    en 32 user-SGPR (`root_signature::pack_user_sgprs`) et enregistrés avec la commande.
//! 3. **Exécution** (`ExecuteCommandLists`) : les V#/T#/S# sont **lus dans les mots matériels** à
//!    l'adresse des handles de tas (ou dans les user-SGPR / pointeurs racine), puis résolus :
//!    V# → copie des octets de la ressource propriétaire de l'adresse, T# → image créée par le
//!    bridge (cache adresse/format/dimensions + hachage du contenu), S# → échantillonneur Vulkan.
//!
//! Aucune adresse n'est lue sans appartenir à une ressource ou un tas de descripteurs créés par
//! le bridge (`ALLOCATED_BUFFER_SIZES`, `DESC_HEAP_RANGES`).

use super::*;
use crate::gfx10_descriptors::{
    ImageType, SEL_ONE, SEL_W, SEL_X, SEL_Y, SEL_Z, SEL_ZERO, decode_buffer, decode_image,
    decode_sampler,
};
use crate::root_signature::{ParsedRootSignature, RootParam, RootParamKind, pack_user_sgprs};
use sb_gpu::rdna2_native::{
    NativeResourceKind, NativeResourceLoc, NativeShader, NativeShaderMeta, is_native_container,
};
use sb_gpu::renderer::{
    NativeBinding, NativeDispatchCmd, NativeDrawCmd, NativeSamplerDesc, NativeStageState,
    native_format_texel_bytes,
};

/// Nombre de mots user-SGPR transmis à chaque étage.
pub(crate) const USER_DWORDS: usize = 32;
/// Taille maximale d'un tampon copié pour une liaison (garde-fou).
const MAX_BUFFER_COPY: usize = 256 << 20;
/// Taille maximale d'un tampon pointé par une racine (`Pointer`).
const MAX_POINTER_COPY: usize = 16 << 20;

// ==================== Signature racine ====================

/// Disposition d'une signature racine pour le chemin natif : nature et taille de chaque paramètre,
/// premier user-SGPR de chacun (compactage `pack_user_sgprs`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RootSigLayout {
    pub params: Vec<(RootParamKind, u32)>,
    pub first_sgpr: Vec<u32>,
}

impl RootSigLayout {
    /// `None` si la signature ne tient pas dans les 32 user-SGPR (le spill n'est pas modélisé).
    pub fn from_params(params: Vec<(RootParamKind, u32)>) -> Option<Self> {
        let sig = ParsedRootSignature {
            version: 2,
            flags: 0,
            params: params
                .iter()
                .map(|&(kind, num_values)| RootParam {
                    kind,
                    visibility: 0,
                    shader_register: 0,
                    register_space: 0,
                    num_values,
                    descriptor_flags: 0,
                    ranges: Vec::new(),
                })
                .collect(),
            static_samplers: Vec::new(),
        };
        let (first_sgpr, _) = pack_user_sgprs(&sig).ok()?;
        Some(Self { params, first_sgpr })
    }

    /// Décode `D3D12_ROOT_PARAMETER_TYPE` (0 = table, 1 = constantes, 2 = CBV, 3 = SRV, 4 = UAV).
    pub fn kind_from_d3d12(t: u32) -> Option<RootParamKind> {
        Some(match t {
            0 => RootParamKind::Table,
            1 => RootParamKind::Constants,
            2 => RootParamKind::Cbv,
            3 => RootParamKind::Srv,
            4 => RootParamKind::Uav,
            _ => return None,
        })
    }
}

/// Argument racine courant d'un paramètre.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum RootArg {
    #[default]
    Unset,
    Constants(Vec<u32>),
    /// Adresse virtuelle d'un CBV / SRV / UAV racine.
    Va(u64),
    /// Adresse GPU du premier descripteur d'une table.
    Table(u64),
}

/// Arguments racine d'un point de liaison (graphique ou compute).
#[derive(Debug, Default)]
pub(crate) struct RootBinding {
    pub layout: Option<Arc<RootSigLayout>>,
    pub args: Vec<RootArg>,
}

impl RootBinding {
    pub fn set_signature(&mut self, layout: Option<Arc<RootSigLayout>>) {
        self.args = vec![RootArg::Unset; layout.as_ref().map(|l| l.params.len()).unwrap_or(0)];
        self.layout = layout;
    }

    fn kind(&self, idx: u32) -> Option<(RootParamKind, u32)> {
        self.layout.as_ref()?.params.get(idx as usize).copied()
    }

    pub fn set_constants(&mut self, idx: u32, dest_offset: u32, values: &[u32]) {
        let Some((RootParamKind::Constants, n)) = self.kind(idx) else {
            return;
        };
        let end = dest_offset as usize + values.len();
        if end > n as usize {
            return; // hors du paramètre : ignoré (D3D12 : comportement indéfini)
        }
        let slot = &mut self.args[idx as usize];
        let mut cur = match std::mem::take(slot) {
            RootArg::Constants(v) => v,
            _ => vec![0; n as usize],
        };
        cur[dest_offset as usize..end].copy_from_slice(values);
        *slot = RootArg::Constants(cur);
    }

    pub fn set_va(&mut self, idx: u32, va: u64) {
        if matches!(
            self.kind(idx),
            Some((
                RootParamKind::Cbv | RootParamKind::Srv | RootParamKind::Uav,
                _
            ))
        ) {
            self.args[idx as usize] = RootArg::Va(va);
        }
    }

    pub fn set_table(&mut self, idx: u32, addr: u64) {
        if matches!(self.kind(idx), Some((RootParamKind::Table, _))) {
            self.args[idx as usize] = RootArg::Table(addr);
        }
    }

    /// Mots des 32 user-SGPR : constantes en clair, adresses (CBV/SRV/UAV, tables) sur 2 dwords.
    pub fn user_data(&self) -> Vec<u32> {
        let mut out = vec![0u32; USER_DWORDS];
        let Some(layout) = &self.layout else {
            return out;
        };
        for (i, arg) in self.args.iter().enumerate() {
            let first = layout.first_sgpr[i] as usize;
            match arg {
                RootArg::Unset => {}
                RootArg::Constants(v) => {
                    for (k, w) in v.iter().enumerate() {
                        if first + k < USER_DWORDS {
                            out[first + k] = *w;
                        }
                    }
                }
                RootArg::Va(a) | RootArg::Table(a) => {
                    if first + 1 < USER_DWORDS {
                        out[first] = *a as u32;
                        out[first + 1] = (*a >> 32) as u32;
                    }
                }
            }
        }
        out
    }
}

// ==================== PSO natif ====================

/// Métadonnées conservées avec un PSO natif (`[VS, PS]` ou `[CS]`).
#[derive(Debug)]
pub(crate) struct NativePso {
    pub stages: Vec<NativeShaderMeta>,
}

/// État natif d'une liste de commandes.
#[derive(Default)]
pub(crate) struct NativeListState {
    pub gfx: RootBinding,
    pub cs: RootBinding,
    pub gfx_pso: Option<(usize, Arc<NativePso>)>,
    pub cs_pso: Option<(usize, Arc<NativePso>)>,
}

impl NativeListState {
    /// Mémorise le PSO courant (`None` si le PSO n'est pas natif : le chemin DXBC reprend la main).
    pub fn note_pso(&mut self, id: usize, is_compute: bool, native: &Option<Arc<NativePso>>) {
        let v = native.as_ref().map(|n| (id, Arc::clone(n)));
        if is_compute {
            self.cs_pso = v;
        } else {
            self.gfx_pso = v;
        }
    }
}

fn new_pso_object(pipeline_id: usize, is_compute: bool, native: Arc<NativePso>) -> *mut c_void {
    Box::into_raw(Box::new(D3D12PsoObject {
        header: ComHeader::new(traced_vtbl(
            (&raw const D3D12_PSO_VTBL).cast(),
            "ID3D12PipelineState",
        )),
        pipeline_id,
        is_compute,
        native: Some(native),
    }))
    .cast()
}

/// `CreateGraphicsPipelineState` pour des shaders `SBR2` (VS + PS obligatoires et natifs).
pub(crate) fn create_native_graphics_pso(
    dev: &D3D12DeviceObject,
    vs: &[u8],
    ps: &[u8],
    cfg: &GraphicsPipelineConfig,
    out_pso: *mut *mut c_void,
) -> HResult {
    if !(is_native_container(vs) && is_native_container(ps)) {
        sb_warn!(
            CHANNEL,
            "CreateGraphicsPipelineState : VS et PS doivent tous deux être au format SBR2 (Rdna2Native)"
        );
        return E_INVALIDARG;
    }
    let (Ok(v), Ok(p)) = (NativeShader::parse(vs), NativeShader::parse(ps)) else {
        sb_warn!(
            CHANNEL,
            "CreateGraphicsPipelineState : conteneur SBR2 invalide"
        );
        return E_INVALIDARG;
    };
    let mut renderer = dev.shared.renderer.lock();
    match renderer.create_native_graphics_pipeline(vs, ps, cfg) {
        Ok(id) => {
            let native = Arc::new(NativePso {
                stages: vec![v.meta, p.meta],
            });
            // SAFETY: `out_pso` vérifié non-nul par l'appelant.
            unsafe { *out_pso = new_pso_object(id, false, native) };
            sb_info!(
                CHANNEL,
                "ID3D12Device::CreateGraphicsPipelineState (Rdna2Native) -> VkPipeline #{id} prêt"
            );
            S_OK
        }
        Err(e) => {
            sb_warn!(
                CHANNEL,
                "CreateGraphicsPipelineState (Rdna2Native) échoué : {e}"
            );
            E_FAIL
        }
    }
}

/// `CreateComputePipelineState` pour un shader `SBR2`.
pub(crate) fn create_native_compute_pso(
    dev: &D3D12DeviceObject,
    cs: &[u8],
    out_pso: *mut *mut c_void,
) -> HResult {
    let Ok(c) = NativeShader::parse(cs) else {
        sb_warn!(
            CHANNEL,
            "CreateComputePipelineState : conteneur SBR2 invalide"
        );
        return E_INVALIDARG;
    };
    let mut renderer = dev.shared.renderer.lock();
    match renderer.create_native_compute_pipeline(cs) {
        Ok(id) => {
            let native = Arc::new(NativePso {
                stages: vec![c.meta],
            });
            // SAFETY: `out_pso` vérifié non-nul par l'appelant.
            unsafe { *out_pso = new_pso_object(id, true, native) };
            sb_info!(
                CHANNEL,
                "ID3D12Device::CreateComputePipelineState (Rdna2Native) -> VkPipeline Compute #{id} prêt"
            );
            S_OK
        }
        Err(e) => {
            sb_warn!(
                CHANNEL,
                "CreateComputePipelineState (Rdna2Native) échoué : {e}"
            );
            E_FAIL
        }
    }
}

// ==================== Lecture de mémoire contrôlée ====================

/// Plage `(base, longueur)` de la ressource du bridge contenant `addr`.
fn owning_range(addr: usize) -> Option<(usize, usize)> {
    ALLOCATED_BUFFER_SIZES
        .lock()
        .iter()
        .find(|&(&b, &l)| addr >= b && addr - b < l)
        .map(|(&b, &l)| (b, l))
}

/// Copie `len` octets à `addr` s'ils sont entièrement dans une ressource du bridge.
fn read_bytes(addr: usize, len: usize) -> Option<Vec<u8>> {
    let (base, total) = owning_range(addr)?;
    if len == 0 || addr - base + len > total {
        return None;
    }
    // SAFETY: `addr..addr+len` est inclus dans la mémoire d'une ressource du bridge (vérifié).
    Some(unsafe { std::slice::from_raw_parts(addr as *const u8, len) }.to_vec())
}

/// Lit `n` dwords d'un descripteur s'ils sont dans un tas de descripteurs du bridge.
fn read_desc_words(addr: usize, n: usize) -> Option<Vec<u32>> {
    let bytes = n * 4;
    let known = DESC_HEAP_RANGES
        .lock()
        .iter()
        .any(|&(b, l)| addr >= b && l >= bytes && addr - b <= l - bytes);
    if !known {
        return None;
    }
    let mut out = vec![0u32; n];
    // SAFETY: `addr..addr+bytes` est dans un tas de descripteurs du bridge (mapping RW, vérifié).
    unsafe { ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr().cast::<u8>(), bytes) };
    Some(out)
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ==================== Conversions GFX10 -> Vulkan ====================

/// Formats GFX10 de T# supportés par le chemin natif, avec leur format Vulkan. Limités aux
/// formats dont l'empreinte mémoire du bridge (`texture_row_layout`) est exacte : le layout
/// console explicite des textures n'étant pas documenté, aucune autre disposition n'est devinée.
pub(crate) fn gfx10_to_vk_format(fmt: u32) -> Option<vk::Format> {
    Some(match fmt {
        56 => vk::Format::R8G8B8A8_UNORM,
        130 => vk::Format::R8G8B8A8_SRGB,
        71 => vk::Format::R16G16B16A16_SFLOAT,
        77 => vk::Format::R32G32B32A32_SFLOAT,
        29 => vk::Format::R16G16_SFLOAT,
        22 => vk::Format::R32_SFLOAT,
        20 => vk::Format::R32_UINT,
        _ => return None,
    })
}

pub(crate) fn swizzle_to_vk(s: [u8; 4]) -> [vk::ComponentSwizzle; 4] {
    let m = |v: u8| match v {
        SEL_ZERO => vk::ComponentSwizzle::ZERO,
        SEL_ONE => vk::ComponentSwizzle::ONE,
        SEL_X => vk::ComponentSwizzle::R,
        SEL_Y => vk::ComponentSwizzle::G,
        SEL_Z => vk::ComponentSwizzle::B,
        SEL_W => vk::ComponentSwizzle::A,
        _ => vk::ComponentSwizzle::IDENTITY,
    };
    [m(s[0]), m(s[1]), m(s[2]), m(s[3])]
}

/// `SQ_TEX_CLAMP` -> mode d'adressage Vulkan (les modes « mirror once » sont approchés).
fn address_mode(m: u32) -> vk::SamplerAddressMode {
    match m {
        0 => vk::SamplerAddressMode::REPEAT,
        1 | 3 | 5 => vk::SamplerAddressMode::MIRRORED_REPEAT,
        6 | 7 => vk::SamplerAddressMode::CLAMP_TO_BORDER,
        _ => vk::SamplerAddressMode::CLAMP_TO_EDGE,
    }
}

pub(crate) fn sampler_to_vk(words: &[u32; 4]) -> NativeSamplerDesc {
    let s = decode_sampler(words);
    let lin = |f: u32| {
        if f == 1 || f == 3 {
            vk::Filter::LINEAR
        } else {
            vk::Filter::NEAREST
        }
    };
    let aniso = s.mag_filter >= 2 || s.min_filter >= 2;
    let compare_op = if s.compare != 0 || s.compare_never {
        Some(match s.compare {
            0 => vk::CompareOp::NEVER,
            1 => vk::CompareOp::LESS,
            2 => vk::CompareOp::EQUAL,
            3 => vk::CompareOp::LESS_OR_EQUAL,
            4 => vk::CompareOp::GREATER,
            5 => vk::CompareOp::NOT_EQUAL,
            6 => vk::CompareOp::GREATER_OR_EQUAL,
            _ => vk::CompareOp::ALWAYS,
        })
    } else {
        None
    };
    NativeSamplerDesc {
        mag_filter: lin(s.mag_filter),
        min_filter: lin(s.min_filter),
        mipmap_mode: if s.mip_filter >= 2 {
            vk::SamplerMipmapMode::LINEAR
        } else {
            vk::SamplerMipmapMode::NEAREST
        },
        address_u: address_mode(s.address_u),
        address_v: address_mode(s.address_v),
        address_w: address_mode(s.address_w),
        mip_lod_bias: s.lod_bias,
        max_anisotropy: if aniso { s.max_anisotropy as f32 } else { 1.0 },
        compare_op,
        min_lod: s.min_lod,
        max_lod: if s.mip_filter == 0 {
            0.25
        } else {
            s.max_lod.max(s.min_lod)
        },
        border_color: match s.border_color {
            1 => vk::BorderColor::FLOAT_OPAQUE_BLACK,
            2 => vk::BorderColor::FLOAT_OPAQUE_WHITE,
            _ => vk::BorderColor::FLOAT_TRANSPARENT_BLACK,
        },
        unnormalized: s.unnormalized,
    }
}

// ==================== Cache d'images ====================

#[derive(Hash, PartialEq, Eq, Clone, Copy, Debug)]
pub(crate) struct ImageKey {
    addr: usize,
    width: u32,
    height: u32,
    format: i32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CachedImage {
    id: usize,
    hash: u64,
}

pub(crate) type ImageCache = Mutex<HashMap<ImageKey, CachedImage>>;

// ==================== Résolution ====================

/// Contexte de résolution des ressources d'un `Draw` / `Dispatch`.
pub(crate) struct ResolveCtx<'a> {
    pub renderer: &'a mut VulkanRenderer,
    /// Commandes de rendu en attente : vidées avant la mise à jour en place d'une image.
    pub pending: &'a mut Vec<RenderCommand>,
    pub descs: &'a HashMap<usize, DescriptorBinding>,
    pub images: &'a ImageCache,
}

/// Étage résolu + régions mémoire des tampons (pour la recopie sortante d'un dispatch).
pub(crate) struct ResolvedStage {
    pub state: NativeStageState,
    /// Par ressource : `(adresse, octets d'origine)` pour les V#.
    pub buffers: Vec<Option<(usize, Vec<u8>)>>,
}

fn user_ptr(user: &[u32], sgpr: u8) -> u64 {
    let i = usize::from(sgpr);
    match (user.get(i), user.get(i + 1)) {
        (Some(&lo), Some(&hi)) => u64::from(lo) | (u64::from(hi) << 32),
        _ => 0,
    }
}

/// Résout l'image d'un T# : adresse (mots matériels) -> ressource du bridge -> image Vulkan.
fn resolve_image(ctx: &mut ResolveCtx<'_>, words: &[u32], handle: Option<usize>) -> NativeBinding {
    let Ok(w8) = <[u32; 8]>::try_from(words) else {
        return NativeBinding::Unbound;
    };
    let Some(img) = decode_image(&w8) else {
        sb_warn!(CHANNEL, "Rdna2Native : T# invalide, image de repli liée");
        return NativeBinding::Unbound;
    };
    let Some(fmt) = gfx10_to_vk_format(img.format) else {
        sb_warn!(
            CHANNEL,
            "Rdna2Native : format GFX10 {} non supporté pour un T#",
            img.format
        );
        return NativeBinding::Unbound;
    };
    if img.image_type != ImageType::Image2D || img.first_mip != 0 || img.layer_count != 1 {
        sb_warn!(
            CHANNEL,
            "Rdna2Native : seuls les T# 2D (mip 0, 1 couche) sont supportés ({:?})",
            img.image_type
        );
        return NativeBinding::Unbound;
    }
    let Some(bpp) = native_format_texel_bytes(fmt) else {
        return NativeBinding::Unbound;
    };
    let (w, h) = (img.width, img.height);
    let need = (w as usize) * (h as usize) * bpp as usize;
    let taddr = img.address as usize;

    // 1. Ressource via la table `descriptors` (handle -> SRV), recoupée avec l'adresse du T#.
    let mut found: Option<(usize, Option<usize>)> = None;
    if let Some(h) = handle
        && let Some(DescriptorBinding::SrvTexture { resource_ptr }) = ctx.descs.get(&h).copied()
        && is_d3d12_resource(resource_ptr)
    {
        // SAFETY: `resource_ptr` validé par `is_d3d12_resource`.
        let res = unsafe { &*(resource_ptr as *const D3D12ResourceObject) };
        if res.fixed_addr & !0xFF == taddr
            || (res.fixed_addr..res.fixed_addr + res.byte_len).contains(&taddr)
        {
            found = Some((res.fixed_addr, Some(resource_ptr)));
        }
    }
    // 2. Sinon : plage de ressource dont la base (tronquée à 256 octets) est l'adresse du T#.
    if found.is_none() {
        let table = ALLOCATED_BUFFER_SIZES.lock();
        let mut hits = table
            .iter()
            .filter(|&(&b, &l)| b & !0xFF == taddr || (b..b + l).contains(&taddr))
            .map(|(&b, _)| b)
            .collect::<Vec<_>>();
        hits.sort_unstable();
        if hits.len() > 1 {
            sb_warn!(
                CHANNEL,
                "Rdna2Native : adresse de T# {taddr:#x} ambiguë ({} ressources dans le même bloc de 256 octets)",
                hits.len()
            );
        }
        found = hits.first().map(|&b| (b, None));
    }
    let Some((base, res_ptr)) = found else {
        sb_warn!(
            CHANNEL,
            "Rdna2Native : T# à l'adresse {taddr:#x} ne correspond à aucune ressource du bridge"
        );
        return NativeBinding::Unbound;
    };
    let mut data = None;
    if let Some(rp) = res_ptr {
        // SAFETY: `rp` provient de la table `descriptors` (ressource valide, voir plus haut).
        let res = unsafe { &*(rp as *const D3D12ResourceObject) };
        if res.swizzle_mode.is_some() {
            data = detile_from_resource(res).filter(|d| d.len() >= need);
        }
    }
    let data = match data.or_else(|| read_bytes(base, need)) {
        Some(d) => d[..need].to_vec(),
        None => {
            sb_warn!(
                CHANNEL,
                "Rdna2Native : T# {w}x{h} dépasse la mémoire de la ressource ({need} octets)"
            );
            return NativeBinding::Unbound;
        }
    };
    let key = ImageKey {
        addr: base,
        width: w,
        height: h,
        format: fmt.as_raw(),
    };
    let hash = fnv1a(&data);
    let mut cache = ctx.images.lock();
    let id = match cache.get(&key).copied() {
        Some(c) if c.hash == hash => c.id,
        Some(c) => {
            // Contenu modifié : les commandes en attente utilisent l'ancien contenu.
            if !ctx.pending.is_empty() {
                if let Err(e) = ctx.renderer.execute_commands(ctx.pending) {
                    sb_warn!(CHANNEL, "Rdna2Native : exécution anticipée échouée : {e}");
                }
                ctx.pending.clear();
            }
            if let Err(e) = ctx.renderer.update_native_image(c.id, &data) {
                sb_warn!(CHANNEL, "Rdna2Native : mise à jour d'image échouée : {e}");
                return NativeBinding::Unbound;
            }
            cache.insert(key, CachedImage { id: c.id, hash });
            c.id
        }
        None => match ctx.renderer.create_native_image(w, h, fmt, &data) {
            Ok(id) => {
                cache.insert(key, CachedImage { id, hash });
                id
            }
            Err(e) => {
                sb_warn!(CHANNEL, "Rdna2Native : création d'image échouée : {e}");
                return NativeBinding::Unbound;
            }
        },
    };
    NativeBinding::Image {
        image_id: id,
        swizzle: swizzle_to_vk(img.swizzle),
    }
}

/// Copie les octets d'un tampon `[addr, addr + size)` borné à sa ressource propriétaire.
fn buffer_binding(addr: usize, size: usize) -> (NativeBinding, Option<(usize, Vec<u8>)>) {
    let Some((base, total)) = owning_range(addr) else {
        sb_warn!(
            CHANNEL,
            "Rdna2Native : V# à l'adresse {addr:#x} hors de toute ressource du bridge"
        );
        return (NativeBinding::Unbound, None);
    };
    let avail = base + total - addr;
    let len = size.min(avail);
    if len < size {
        sb_warn!(
            CHANNEL,
            "Rdna2Native : V# de {size} octets borné à {len} (fin de ressource)"
        );
    }
    match read_bytes(addr, len.min(MAX_BUFFER_COPY)) {
        Some(bytes) => (NativeBinding::Buffer(bytes.clone()), Some((addr, bytes))),
        None => (NativeBinding::Unbound, None),
    }
}

/// Résout toutes les ressources d'un étage à partir des user-SGPR.
pub(crate) fn resolve_stage(
    ctx: &mut ResolveCtx<'_>,
    meta: &NativeShaderMeta,
    user: &[u32],
) -> ResolvedStage {
    let mut state = NativeStageState {
        user_data: user.to_vec(),
        bindings: Vec::with_capacity(meta.resources.len()),
    };
    let mut buffers = Vec::with_capacity(meta.resources.len());
    for res in &meta.resources {
        let dwords = res.kind.descriptor_dwords() as usize;
        // Pointeur racine : tampon désigné directement par son adresse.
        if let NativeResourceLoc::Pointer { sgpr } = res.loc {
            let va = user_ptr(user, sgpr) as usize;
            let (b, region) = match owning_range(va) {
                Some((base, total)) if va != 0 => {
                    buffer_binding(va, (base + total - va).min(MAX_POINTER_COPY))
                }
                _ => {
                    sb_warn!(CHANNEL, "Rdna2Native : pointeur racine {va:#x} non résolu");
                    (NativeBinding::Unbound, None)
                }
            };
            state.bindings.push(b);
            buffers.push(region);
            continue;
        }
        // Mots du descripteur : table (mémoire du tas) ou user-SGPR.
        let (words, handle) = match res.loc {
            NativeResourceLoc::Table {
                table_sgpr,
                byte_offset,
            } => {
                let base = user_ptr(user, table_sgpr) as usize;
                let addr = base.wrapping_add(byte_offset as usize);
                (
                    if base == 0 {
                        None
                    } else {
                        read_desc_words(addr, dwords)
                    },
                    Some(addr),
                )
            }
            NativeResourceLoc::Sgprs { first } => {
                let f = usize::from(first);
                (user.get(f..f + dwords).map(<[u32]>::to_vec), None)
            }
            NativeResourceLoc::Pointer { .. } => (None, None),
        };
        let Some(words) = words else {
            sb_warn!(
                CHANNEL,
                "Rdna2Native : descripteur illisible (table non liée ou hors des tas du bridge)"
            );
            state.bindings.push(NativeBinding::Unbound);
            buffers.push(None);
            continue;
        };
        match res.kind {
            NativeResourceKind::Buffer { .. } => {
                let (b, region) = match <[u32; 4]>::try_from(words.as_slice())
                    .ok()
                    .and_then(|w| decode_buffer(&w))
                {
                    Some(v) => {
                        let size = if v.stride == 0 {
                            v.num_records as usize
                        } else {
                            (v.stride as usize).saturating_mul(v.num_records as usize)
                        };
                        buffer_binding(v.address as usize, size)
                    }
                    None => {
                        sb_warn!(CHANNEL, "Rdna2Native : V# invalide");
                        (NativeBinding::Unbound, None)
                    }
                };
                state.bindings.push(b);
                buffers.push(region);
            }
            NativeResourceKind::Image => {
                state.bindings.push(resolve_image(ctx, &words, handle));
                buffers.push(None);
            }
            NativeResourceKind::Sampler => {
                let b = <[u32; 4]>::try_from(words.as_slice())
                    .map(|w| NativeBinding::Sampler(sampler_to_vk(&w)))
                    .unwrap_or(NativeBinding::Unbound);
                state.bindings.push(b);
                buffers.push(None);
            }
        }
    }
    ResolvedStage { state, buffers }
}

/// Construit la commande de dessin native depuis les états résolus.
pub(crate) fn draw_cmd(
    pipeline_id: usize,
    counts: (u32, u32, u32, u32),
    vs: ResolvedStage,
    ps: ResolvedStage,
) -> RenderCommand {
    RenderCommand::NativeDraw(Box::new(NativeDrawCmd {
        pipeline_id,
        vertex_count: counts.0,
        instance_count: counts.1,
        first_vertex: counts.2,
        first_instance: counts.3,
        vs: vs.state,
        ps: ps.state,
    }))
}

/// Exécute un dispatch natif et recopie en mémoire les tampons modifiés.
pub(crate) fn run_dispatch(
    renderer: &mut VulkanRenderer,
    pipeline_id: usize,
    xyz: (u32, u32, u32),
    cs: ResolvedStage,
) {
    let cmd = NativeDispatchCmd {
        pipeline_id,
        x: xyz.0,
        y: xyz.1,
        z: xyz.2,
        cs: cs.state,
    };
    match renderer.dispatch_native(&cmd) {
        Ok(out) => {
            for (orig, new) in cs.buffers.iter().zip(out) {
                if let (Some((addr, before)), Some(after)) = (orig, new)
                    && *before != after
                {
                    // SAFETY: `addr..addr+len` a été validé dans `buffer_binding` (ressource du
                    // bridge) pendant ce même `ExecuteCommandLists`, `after.len() == before.len()`.
                    unsafe {
                        ptr::copy_nonoverlapping(after.as_ptr(), *addr as *mut u8, after.len());
                    }
                }
            }
        }
        Err(e) => sb_warn!(CHANNEL, "Dispatch natif échoué : {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12_views::SLOT_BYTES;
    use sb_gpu::rdna2_native::samples;

    fn u32s(v: &[u32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    fn layout() -> Arc<RootSigLayout> {
        Arc::new(
            RootSigLayout::from_params(vec![
                (RootParamKind::Table, 0),
                (RootParamKind::Cbv, 0),
                (RootParamKind::Constants, 1),
            ])
            .expect("tient dans 32 user-SGPR"),
        )
    }

    #[test]
    fn root_args_pack_into_user_sgprs() {
        let l = layout();
        assert_eq!(l.first_sgpr, vec![0, 2, 4]);
        let mut b = RootBinding::default();
        // Sans signature : tout est ignoré, 32 zéros.
        b.set_constants(0, 0, &[1]);
        assert_eq!(b.user_data(), vec![0; USER_DWORDS]);
        b.set_signature(Some(l));
        b.set_table(0, 0x1_2345_6780);
        b.set_va(1, 0xAABB_CCDD_0000_1000);
        b.set_constants(2, 0, &[0x3F00_0000]);
        let u = b.user_data();
        assert_eq!(
            &u[..5],
            &[0x2345_6780, 1, 0x0000_1000, 0xAABB_CCDD, 0x3F00_0000]
        );
        assert_eq!(u.len(), USER_DWORDS);
        // Mauvaise nature de paramètre / hors plage : ignoré.
        b.set_va(0, 5);
        b.set_table(2, 5);
        b.set_constants(2, 1, &[7]);
        b.set_constants(9, 0, &[7]);
        assert_eq!(b.user_data(), u);
    }

    #[test]
    fn root_constants_merge_by_dest_offset() {
        let l = Arc::new(
            RootSigLayout::from_params(vec![(RootParamKind::Constants, 4)]).expect("4 dwords"),
        );
        let mut b = RootBinding::default();
        b.set_signature(Some(l));
        b.set_constants(0, 1, &[10, 11]);
        b.set_constants(0, 0, &[9]);
        assert_eq!(&b.user_data()[..4], &[9, 10, 11, 0]);
    }

    #[test]
    fn oversized_root_signature_is_refused() {
        // 17 tables = 34 user-SGPR > 32 : le spill n'est pas modélisé.
        assert!(RootSigLayout::from_params(vec![(RootParamKind::Table, 0); 17]).is_none());
        assert!(RootSigLayout::from_params(vec![(RootParamKind::Table, 0); 16]).is_some());
    }

    #[test]
    fn format_and_swizzle_conversions() {
        assert_eq!(gfx10_to_vk_format(56), Some(vk::Format::R8G8B8A8_UNORM));
        assert_eq!(gfx10_to_vk_format(130), Some(vk::Format::R8G8B8A8_SRGB));
        assert_eq!(
            gfx10_to_vk_format(1),
            None,
            "R8 : empreinte mémoire non garantie"
        );
        let sw = swizzle_to_vk([SEL_Z, SEL_Y, SEL_X, SEL_ONE]);
        assert_eq!(
            sw,
            [
                vk::ComponentSwizzle::B,
                vk::ComponentSwizzle::G,
                vk::ComponentSwizzle::R,
                vk::ComponentSwizzle::ONE
            ]
        );
    }

    #[test]
    fn sampler_words_map_to_vulkan() {
        use crate::gfx10_descriptors::{SamplerDesc, encode_sampler};
        let w = encode_sampler(&SamplerDesc {
            address_u: 0,
            address_v: 1,
            address_w: 6,
            mag_filter: 1,
            min_filter: 0,
            mip_filter: 2,
            ..Default::default()
        });
        let d = sampler_to_vk(&w);
        assert_eq!(d.address_u, vk::SamplerAddressMode::REPEAT);
        assert_eq!(d.address_v, vk::SamplerAddressMode::MIRRORED_REPEAT);
        assert_eq!(d.address_w, vk::SamplerAddressMode::CLAMP_TO_BORDER);
        assert_eq!(d.mag_filter, vk::Filter::LINEAR);
        assert_eq!(d.min_filter, vk::Filter::NEAREST);
        assert_eq!(d.mipmap_mode, vk::SamplerMipmapMode::LINEAR);
        assert_eq!(d.compare_op, None);
    }

    #[test]
    fn descriptor_and_buffer_reads_are_confined_to_bridge_memory() {
        let stack = [0u32; 8];
        // Une adresse de pile n'appartient à aucun tas de descripteurs ni ressource.
        assert!(read_desc_words(stack.as_ptr() as usize, 8).is_none());
        assert!(read_bytes(stack.as_ptr() as usize, 16).is_none());
        // Une ressource du bridge : lecture bornée à sa longueur.
        let res = super::super::alloc_d3d12_resource(
            D3D12ResourceDesc {
                dimension: 1,
                width: 128,
                height: 1,
                depth_or_array_size: 1,
                mip_levels: 1,
                ..Default::default()
            },
            false,
        );
        // SAFETY: ressource créée ci-dessus.
        let addr = unsafe { (*res).fixed_addr };
        assert_eq!(read_bytes(addr, 128).map(|v| v.len()), Some(128));
        assert!(read_bytes(addr, 129).is_none());
        assert!(read_bytes(addr + 64, 65).is_none());
        let (b, binding) = buffer_binding(addr + 100, 1000);
        assert!(matches!(&b, NativeBinding::Buffer(v) if v.len() == 28));
        assert!(binding.is_some());
    }

    // ------------------------------------------------------------ chaîne COM complète

    fn make_device() -> Option<*mut D3D12DeviceObject> {
        let mut dev: *mut c_void = ptr::null_mut();
        if D3D12CreateDevice(ptr::null_mut(), 0xC100, &IID_ID3D12DEVICE, &mut dev) != S_OK {
            return None; // pas de GPU Vulkan
        }
        Some(dev.cast())
    }

    fn make_heap(n: u32, heap_type: u32) -> usize {
        let desc = D3D12DescriptorHeapDesc {
            heap_type,
            num_descriptors: n,
            flags: 1,
            node_mask: 1,
        };
        let mut heap: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_descriptor_heap(
                ptr::null_mut(),
                &desc,
                &IID_ID3D12DESCRIPTORHEAP,
                &mut heap
            ),
            S_OK
        );
        // SAFETY: `heap` est un `D3D12DescHeapObject` créé ci-dessus.
        unsafe { (*(heap as *const D3D12DescHeapObject)).base_handle }
    }

    fn make_resource(dev: *mut D3D12DeviceObject, desc: D3D12ResourceDesc) -> *mut c_void {
        let mut r: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_committed_resource(
                dev,
                ptr::null(),
                0,
                &desc,
                0,
                ptr::null(),
                &IID_ID3D12RESOURCE,
                &mut r
            ),
            S_OK
        );
        r
    }

    fn buffer_desc(size: u64) -> D3D12ResourceDesc {
        D3D12ResourceDesc {
            dimension: 1,
            width: size,
            height: 1,
            depth_or_array_size: 1,
            mip_levels: 1,
            ..Default::default()
        }
    }

    fn write_res(res: *mut c_void, bytes: &[u8]) {
        let mut p: *mut c_void = ptr::null_mut();
        assert_eq!(res_map(res.cast(), 0, ptr::null(), &mut p), S_OK);
        // SAFETY: la ressource contient au moins `bytes.len()` octets (créée à cette taille).
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len()) };
    }

    fn read_res(res: *mut c_void, len: usize) -> Vec<u8> {
        let mut p: *mut c_void = ptr::null_mut();
        assert_eq!(res_map(res.cast(), 0, ptr::null(), &mut p), S_OK);
        // SAFETY: la ressource contient au moins `len` octets (créée à cette taille).
        unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) }.to_vec()
    }

    /// SRV de tampon brut (V#) sur `res`, `bytes` octets.
    fn raw_srv(dev: *mut D3D12DeviceObject, res: *mut c_void, bytes: u32, dest: usize) {
        let mut d = u32s(&[39, 1, 0x1688, 0, 0, 0, bytes / 4, 0, 1]);
        d.resize(48, 0);
        device_create_srv(dev, res.cast(), d.as_ptr().cast(), dest);
    }

    /// UAV de tampon brut (V#).
    fn raw_uav(dev: *mut D3D12DeviceObject, res: *mut c_void, bytes: u32, dest: usize) {
        let mut d = u32s(&[39, 1, 0, 0, bytes / 4, 0, 0, 0, 1]);
        d.resize(48, 0);
        device_create_uav(dev, res.cast(), ptr::null_mut(), d.as_ptr().cast(), dest);
    }

    /// Signature racine : paramètres `(type, num_values)` -> objet `ID3D12RootSignature`.
    fn make_root_sig(dev: *mut D3D12DeviceObject, params: &[(u32, u32)]) -> *mut c_void {
        let ps: Vec<D3D12RootParameter> = params
            .iter()
            .map(|&(t, n)| D3D12RootParameter {
                parameter_type: t,
                data0: 0,
                data2: u64::from(n),
                ..Default::default()
            })
            .collect();
        let desc = D3D12RootSignatureDesc {
            num_parameters: ps.len() as u32,
            _pad0: 0,
            parameters: ps.as_ptr(),
            num_static_samplers: 0,
            _pad1: 0,
            static_samplers: ptr::null(),
            flags: 0,
            _pad2: 0,
        };
        let mut blob: *mut c_void = ptr::null_mut();
        let mut err: *mut c_void = ptr::null_mut();
        assert_eq!(
            D3D12SerializeRootSignature(ptr::from_ref(&desc).cast(), 1, &mut blob, &mut err),
            S_OK
        );
        // SAFETY: `blob` est un `D3DBlobObject` créé par la sérialisation.
        let data = unsafe { (*(blob as *const D3DBlobObject)).data.clone() };
        let mut rs: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_root_signature(
                dev,
                0,
                data.as_ptr().cast(),
                data.len(),
                &IID_ID3D12ROOTSIGNATURE,
                &mut rs
            ),
            S_OK
        );
        rs
    }

    fn make_cmdlist(dev: *mut D3D12DeviceObject) -> *mut c_void {
        let mut cl: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_command_list(
                dev,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                &IID_ID3D12GRAPHICSCOMMANDLIST,
                &mut cl
            ),
            S_OK
        );
        cl
    }

    #[test]
    fn native_compute_through_com_with_heap_descriptors_and_root_args() {
        let _serial = crate::d3d12_xs::test_serial();
        let Some(dev) = make_device() else {
            return;
        };
        // Ressources : entrée (128 u32), sortie (512 o), CBV racine (biais 3.0).
        let input: Vec<u32> = (0..128).collect();
        let in_res = make_resource(dev, buffer_desc(512));
        write_res(in_res, &u32s(&input));
        let out_res = make_resource(dev, buffer_desc(512));
        let cbv_res = make_resource(dev, buffer_desc(256));
        write_res(cbv_res, &3.0f32.to_le_bytes());
        // Tas CBV/SRV/UAV : slot 0 = SRV entrée (V#), slot 1 = UAV sortie (V#).
        let heap = make_heap(4, 0);
        raw_srv(dev, in_res, 512, heap);
        raw_uav(dev, out_res, 512, heap + SLOT_BYTES);
        // Pipeline natif.
        let cs = samples::cs_affine().to_bytes();
        let rs = make_root_sig(dev, &[(0, 0), (2, 0), (1, 1)]);
        let desc = D3D12ComputePipelineStateDesc {
            root_signature: rs,
            cs: D3D12ShaderBytecode {
                shader_bytecode: cs.as_ptr(),
                bytecode_length: cs.len(),
            },
            node_mask: 0,
            _pad0: 0,
            cached_pso: D3D12CachedPipelineState {
                cached_blob: ptr::null(),
                cached_blob_size_in_bytes: 0,
            },
            flags: 0,
            _pad1: 0,
        };
        let mut pso: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_compute_pso(dev, &desc, &IID_ID3D12PIPELINESTATE, &mut pso),
            S_OK,
            "CreateComputePipelineState (Rdna2Native)"
        );
        // Liste de commandes : signature, arguments racine, dispatch.
        let cl = make_cmdlist(dev);
        let cl_obj = cl.cast::<D3D12CmdListObject>();
        cmdlist_set_pipeline_state(cl_obj, pso.cast());
        cmdlist_set_compute_root_sig(cl_obj, rs.cast());
        cmdlist_set_compute_root_desc_table(cl_obj, 0, heap as u64);
        cmdlist_set_compute_root_cbv(cl_obj, 1, res_get_gpu_va(cbv_res.cast()));
        let scale = [0.5f32.to_bits()];
        cmdlist_set_compute_root_32bit_constants(cl_obj, 2, 1, scale.as_ptr(), 0);
        cmdlist_dispatch(cl_obj, 2, 1, 1);
        assert_eq!(cmdlist_close(cl_obj), S_OK);

        let mut queue: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_command_queue(
                dev,
                &D3D12CommandQueueDesc::default(),
                &IID_ID3D12COMMANDQUEUE,
                &mut queue
            ),
            S_OK
        );
        queue_execute_command_lists(queue.cast(), 1, [cl_obj].as_ptr());
        let out = read_res(out_res, 512);
        for i in 0..128usize {
            let v =
                f32::from_le_bytes([out[i * 4], out[i * 4 + 1], out[i * 4 + 2], out[i * 4 + 3]]);
            assert_eq!(v, i as f32 * 0.5 + 3.0, "out[{i}]");
        }
        // L'entrée n'a pas été modifiée par la recopie sortante.
        assert_eq!(read_res(in_res, 512), u32s(&input));
    }

    #[test]
    fn native_graphics_through_com_textured_triangle_reads_descriptor_words() {
        let _serial = crate::d3d12_xs::test_serial();
        let Some(dev) = make_device() else {
            return;
        };
        let mut queue: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_command_queue(
                dev,
                &D3D12CommandQueueDesc::default(),
                &IID_ID3D12COMMANDQUEUE,
                &mut queue
            ),
            S_OK
        );
        let mut factory: *mut c_void = ptr::null_mut();
        assert_eq!(
            CreateDXGIFactory2(0, &IID_IDXGIFACTORY4, &mut factory),
            S_OK
        );
        let sc_desc = DxgiSwapChainDesc1 {
            width: 64,
            height: 64,
            format: 28,
            buffer_count: 2,
            sample_desc: DxgiSampleDesc {
                count: 1,
                quality: 0,
            },
            ..Default::default()
        };
        let mut sc: *mut c_void = ptr::null_mut();
        assert_eq!(
            factory4_create_sc_hwnd(
                factory.cast(),
                queue,
                0,
                &sc_desc,
                ptr::null(),
                ptr::null_mut(),
                &mut sc
            ),
            S_OK
        );
        let mut bb: *mut c_void = ptr::null_mut();
        assert_eq!(
            sc3_get_buffer(sc.cast(), 0, &IID_ID3D12RESOURCE, &mut bb),
            S_OK
        );

        // Sommets (V#), texture 2x2 (T#), sampler (S#).
        let verts: [[f32; 8]; 3] = [
            [-1.0, -1.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            [3.0, -1.0, 0.0, 1.0, 2.0, 0.0, 0.0, 1.0],
            [-1.0, 3.0, 0.0, 1.0, 0.0, 2.0, 0.0, 1.0],
        ];
        let vb_bytes: Vec<u8> = verts
            .iter()
            .flatten()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        let vb = make_resource(dev, buffer_desc(vb_bytes.len() as u64));
        write_res(vb, &vb_bytes);
        let tex = make_resource(
            dev,
            D3D12ResourceDesc {
                dimension: 3,
                width: 2,
                height: 2,
                depth_or_array_size: 1,
                mip_levels: 1,
                format: 28,
                sample_desc: DxgiSampleDesc {
                    count: 1,
                    quality: 0,
                },
                ..Default::default()
            },
        );
        let texels: [[u8; 4]; 4] = [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
        ];
        write_res(tex, &texels.iter().flatten().copied().collect::<Vec<u8>>());
        let heap = make_heap(4, 0);
        let sheap = make_heap(2, 3);
        raw_srv(dev, vb, vb_bytes.len() as u32, heap);
        let mut srv = u32s(&[28, 4, 0x1688, 0, 0, 1, 0]);
        srv.extend(0.0f32.to_le_bytes());
        srv.resize(48, 0);
        device_create_srv(dev, tex.cast(), srv.as_ptr().cast(), heap + SLOT_BYTES);
        let mut smp = u32s(&[0, 3, 3, 3]);
        smp.extend(0.0f32.to_le_bytes());
        smp.extend(u32s(&[1, 8]));
        for c in [0.0f32, 0.0, 0.0, 0.0, 0.0, 12.0] {
            smp.extend(c.to_le_bytes());
        }
        device_create_sampler(dev, smp.as_ptr().cast(), sheap);

        // PSO natif graphique.
        let (vs, ps) = (
            samples::vs_textured().to_bytes(),
            samples::ps_tint().to_bytes(),
        );
        let rs = make_root_sig(dev, &[(0, 0), (0, 0), (1, 4)]);
        let none = D3D12ShaderBytecode {
            shader_bytecode: ptr::null(),
            bytecode_length: 0,
        };
        let pso_desc = D3D12GraphicsPipelineStateDesc {
            root_signature: rs,
            vs: D3D12ShaderBytecode {
                shader_bytecode: vs.as_ptr(),
                bytecode_length: vs.len(),
            },
            ps: D3D12ShaderBytecode {
                shader_bytecode: ps.as_ptr(),
                bytecode_length: ps.len(),
            },
            ds: none,
            hs: none,
            gs: none,
            stream_output: D3D12StreamOutputDesc {
                so_declaration: ptr::null(),
                num_entries: 0,
                _pad0: 0,
                buffer_strides: ptr::null(),
                num_strides: 0,
                rasterized_stream: 0,
            },
            blend_state: D3D12BlendDesc::default(),
            sample_mask: u32::MAX,
            rasterizer_state: D3D12RasterizerDesc::default(),
            depth_stencil_state: D3D12DepthStencilDesc::default(),
            input_layout: D3D12InputLayoutDesc {
                input_element_descs: ptr::null(),
                num_elements: 0,
                _pad: 0,
            },
            ib_strip_cut_value: 0,
            primitive_topology_type: 3,
            num_render_targets: 1,
            rtv_formats: [28, 0, 0, 0, 0, 0, 0, 0],
            dsv_format: 0,
            sample_desc: DxgiSampleDesc {
                count: 1,
                quality: 0,
            },
            node_mask: 0,
            cached_pso: D3D12CachedPipelineState {
                cached_blob: ptr::null(),
                cached_blob_size_in_bytes: 0,
            },
            flags: 0,
        };
        let mut pso: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_graphics_pso(dev, &pso_desc, &IID_ID3D12PIPELINESTATE, &mut pso),
            S_OK,
            "CreateGraphicsPipelineState (Rdna2Native)"
        );

        let rb = make_resource(dev, buffer_desc(64 * 64 * 4));
        let cl = make_cmdlist(dev);
        let cl_obj = cl.cast::<D3D12CmdListObject>();
        let vp = D3D12Viewport {
            top_left_x: 0.0,
            top_left_y: 0.0,
            width: 64.0,
            height: 64.0,
            min_depth: 0.0,
            max_depth: 1.0,
        };
        let draw = |tint: [f32; 4]| {
            cmdlist_set_pipeline_state(cl_obj, pso.cast());
            cmdlist_set_graphics_root_sig(cl_obj, rs.cast());
            cmdlist_set_graphics_root_desc_table(cl_obj, 0, heap as u64);
            cmdlist_set_graphics_root_desc_table(cl_obj, 1, sheap as u64);
            let t = tint.map(f32::to_bits);
            cmdlist_set_graphics_root_32bit_constants(cl_obj, 2, 4, t.as_ptr(), 0);
            cmdlist_rs_set_viewports(cl_obj, 1, &vp);
            cmdlist_draw_instanced(cl_obj, 3, 1, 0, 0);
        };
        cmdlist_clear_rtv(cl_obj, 0, &[0.0, 0.0, 0.0, 1.0], 0, ptr::null());
        draw([1.0, 1.0, 1.0, 1.0]);
        cmdlist_copy_resource(cl_obj, rb.cast(), bb.cast());
        assert_eq!(cmdlist_close(cl_obj), S_OK);
        queue_execute_command_lists(queue.cast(), 1, [cl_obj].as_ptr());
        let px = |buf: &[u8], x: usize, y: usize| {
            let i = (y * 64 + x) * 4;
            [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
        };
        let buf = read_res(rb, 64 * 64 * 4);
        assert_eq!(px(&buf, 16, 48), [255, 0, 0, 255], "bas-gauche");
        assert_eq!(px(&buf, 48, 48), [0, 255, 0, 255], "bas-droite");
        assert_eq!(px(&buf, 16, 16), [0, 0, 255, 255], "haut-gauche");
        assert_eq!(px(&buf, 48, 16), [255, 255, 255, 255], "haut-droite");

        // Deuxième exécution : teinte (constantes racine) et texture modifiée en mémoire.
        write_res(tex, &[0u8, 255, 0, 255].repeat(4));
        let cl2 = make_cmdlist(dev);
        let cl2_obj = cl2.cast::<D3D12CmdListObject>();
        cmdlist_clear_rtv(cl2_obj, 0, &[0.0, 0.0, 0.0, 1.0], 0, ptr::null());
        cmdlist_set_pipeline_state(cl2_obj, pso.cast());
        cmdlist_set_graphics_root_sig(cl2_obj, rs.cast());
        cmdlist_set_graphics_root_desc_table(cl2_obj, 0, heap as u64);
        cmdlist_set_graphics_root_desc_table(cl2_obj, 1, sheap as u64);
        let t = [0.5f32, 0.5, 1.0, 1.0].map(f32::to_bits);
        cmdlist_set_graphics_root_32bit_constants(cl2_obj, 2, 4, t.as_ptr(), 0);
        cmdlist_draw_instanced(cl2_obj, 3, 1, 0, 0);
        cmdlist_copy_resource(cl2_obj, rb.cast(), bb.cast());
        assert_eq!(cmdlist_close(cl2_obj), S_OK);
        queue_execute_command_lists(queue.cast(), 1, [cl2_obj].as_ptr());
        let buf = read_res(rb, 64 * 64 * 4);
        let p = px(&buf, 30, 30);
        assert!(
            p[0] == 0 && p[1].abs_diff(128) <= 1 && p[2] == 0 && p[3] == 255,
            "texture verte x teinte (0.5, 0.5, 1) : {p:?}"
        );
    }
}
