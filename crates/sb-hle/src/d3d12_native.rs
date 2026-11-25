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
use sb_gpu::rdna2::{Decoded, Encoding, Src, WaveSize, decode, decode_stream};
use sb_gpu::rdna2_native::{
    MAX_RESOURCES, NativeResource, NativeResourceKind, NativeResourceLoc, NativeShader,
    NativeShaderMeta, NativeStage, is_native_container,
};
use sb_gpu::renderer::{
    NativeBinding, NativeDispatchCmd, NativeDrawCmd, NativeImageDim, NativeSamplerDesc,
    NativeStageState, native_format_texel_bytes,
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

    /// Premier user-SGPR (paire 64 bits) de chaque paramètre de type table de descripteurs.
    pub fn table_sgprs(&self) -> Vec<u8> {
        self.params
            .iter()
            .zip(&self.first_sgpr)
            .filter(|((kind, _), _)| *kind == RootParamKind::Table)
            .filter_map(|(_, &first)| u8::try_from(first).ok())
            .collect()
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
    /// Plan user-SGPR propre au PSO courant : `(indice de paramètre racine, premier user-SGPR)`
    /// qui remplace `RootSigLayout::first_sgpr` (plan déduit de l'usage, voir
    /// [`discover_root_tables`]). Conservé lors d'un changement de signature.
    pub plan_override: Vec<(usize, u32)>,
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
        let overridden = |i: usize| self.plan_override.iter().find(|(p, _)| *p == i);
        // Paramètres au plan réel d'abord, puis ceux déplacés par le plan déduit (qui gagnent en
        // cas de recouvrement avec un paramètre inutilisé par le shader).
        let order = (0..self.args.len())
            .filter(|&i| overridden(i).is_none())
            .chain((0..self.args.len()).filter(|&i| overridden(i).is_some()));
        for i in order {
            let arg = &self.args[i];
            let first = match overridden(i) {
                Some(&(_, sgpr)) => sgpr as usize,
                None => layout.first_sgpr[i] as usize,
            };
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
    /// Plan user-SGPR déduit de l'usage (vide = plan réel de la signature racine).
    pub user_plan: Vec<(usize, u32)>,
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
        let plan = native
            .as_ref()
            .map(|n| n.user_plan.clone())
            .unwrap_or_default();
        if is_compute {
            self.cs_pso = v;
            self.cs.plan_override = plan;
        } else {
            self.gfx_pso = v;
            self.gfx.plan_override = plan;
        }
    }
}

fn new_pso_object(pipeline_id: usize, is_compute: bool, native: Arc<NativePso>) -> *mut c_void {
    Box::into_raw(Box::new(D3D12PsoObject {
        header: ComHeader::new(traced_vtbl(
            (&raw const D3D12_PSO_VTBL).cast(),
            "ID3D12PipelineState",
        )),
        console_inline_pad: [0u8; 32],
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
                user_plan: Vec::new(),
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
                user_plan: Vec::new(),
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

// ==================== Compilation FSPX RDNA 2 -> SPIR-V ====================

/// Opcode `s_endpgm` en RDNA 2 (SOPP 0x01).
const RDNA2_S_ENDPGM: u32 = 0xbf81_0000;
/// Opcode `s_code_end` utilisé comme remplissage de ligne de cache après `s_endpgm`.
const RDNA2_S_CODE_END: u32 = 0xbf9f_0000;
/// Opcode `s_nop 0` utilisé comme remplissage.
const RDNA2_S_NOP_0: u32 = 0xbf80_0000;

fn is_isa_trailer_pad(w: u32) -> bool {
    w == 0 || w == RDNA2_S_CODE_END || w == RDNA2_S_NOP_0
}

fn bytes_prefix_u32(bytes: &[u8], max_words: usize) -> Vec<u32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .take(max_words)
        .copied()
        .map(u32::from_le_bytes)
        .collect()
}

/// Extrait un flux d'instructions RDNA 2 valide depuis un bloc binaire `kind6` FSPX en
/// supprimant l'éventuel remplissage de fin de ligne de cache (`0x0`, `s_code_end`, `s_nop`)
/// après le dernier `s_endpgm`.
pub(crate) fn extract_rdna2_code_words(raw: &[u8]) -> Result<Vec<u32>, String> {
    if raw.len() < 4 || !raw.len().is_multiple_of(4) {
        return Err(format!("taille ISA invalide ({} octets)", raw.len()));
    }
    let words: Vec<u32> = raw
        .as_chunks::<4>()
        .0
        .iter()
        .copied()
        .map(u32::from_le_bytes)
        .collect();
    let mut pc = 0usize;
    let mut last_endpgm_end: Option<usize> = None;
    while pc < words.len() {
        match decode(&words[pc..], WaveSize::W32) {
            Ok(inst) => {
                let len = usize::from(inst.words);
                let next = pc + len;
                if words[pc] == RDNA2_S_ENDPGM || inst.name == "s_endpgm" {
                    last_endpgm_end = Some(next);
                }
                pc = next;
            }
            Err(err) => {
                if let Some(end) = last_endpgm_end {
                    return Ok(words[..end].to_vec());
                }
                return Err(format!("décodage RDNA 2 échoué au mot #{pc} : {err}"));
            }
        }
    }
    if let Some(end) = last_endpgm_end
        && words[end..].iter().all(|&w| is_isa_trailer_pad(w))
    {
        return Ok(words[..end].to_vec());
    }
    Ok(words)
}

/// Déduit `(num_user_sgprs, resources)` pour un shader compute RDNA 2 depuis la `RootSigLayout`
/// (lorsqu'elle est disponible) et les accès `s_load_*` + `MUBUF`/`MIMG` décodés, y compris les
/// pointeurs 32→64 bits élargis par `s_mov_b32 s<dst>, s<user> ; s_mov_b32 s<dst+1>, <const>`.
pub(crate) fn infer_compute_resources(
    insts: &[Decoded],
    layout: Option<&RootSigLayout>,
) -> (u32, Vec<NativeResource>) {
    let mut resources: Vec<NativeResource> = Vec::new();
    let mut num_user_sgprs: u32 = 0;

    if let Some(l) = layout {
        for (i, &(kind, n_vals)) in l.params.iter().enumerate() {
            let Some(&first_u32) = l.first_sgpr.get(i) else {
                continue;
            };
            let width = match kind {
                RootParamKind::Constants => n_vals.max(1),
                RootParamKind::Table
                | RootParamKind::Cbv
                | RootParamKind::Srv
                | RootParamKind::Uav => 2,
            };
            num_user_sgprs = num_user_sgprs.max(first_u32.saturating_add(width));
            let first = first_u32 as u8;
            if matches!(
                kind,
                RootParamKind::Cbv | RootParamKind::Srv | RootParamKind::Uav
            ) {
                let loc = NativeResourceLoc::Pointer { sgpr: first };
                if resources.len() < MAX_RESOURCES && !resources.iter().any(|r| r.loc == loc) {
                    resources.push(NativeResource {
                        kind: NativeResourceKind::Buffer { stride: 4 },
                        loc,
                    });
                }
            }
        }
    }

    // Repère les SGPR consommés comme descripteurs `V#`/`T#`/`S#` par MUBUF/MTBUF/MIMG/s_buffer_load.
    let mut sampler_sgprs: Vec<u8> = Vec::new();
    let mut desc_sgprs: Vec<u8> = Vec::new();
    let mut note_desc = |s: u8| {
        if !desc_sgprs.contains(&s) {
            desc_sgprs.push(s);
        }
    };
    for d in insts {
        match d.inst.encoding {
            Encoding::Mimg => {
                if let Some(Src::Sgpr(s)) = d.inst.operands.get(2).map(|o| o.src) {
                    note_desc(s);
                }
                if let Some(Src::Sgpr(s)) = d.inst.operands.get(3).map(|o| o.src) {
                    note_desc(s);
                    if !sampler_sgprs.contains(&s) {
                        sampler_sgprs.push(s);
                    }
                }
            }
            Encoding::Mubuf | Encoding::Mtbuf => {
                if let Some(Src::Sgpr(s)) = d.inst.operands.get(2).map(|o| o.src) {
                    note_desc(s);
                }
            }
            Encoding::Smem if d.inst.name.starts_with("s_buffer_load") => {
                if let Some(Src::Sgpr(s)) = d.inst.operands.get(1).map(|o| o.src) {
                    note_desc(s);
                }
            }
            _ => {}
        }
    }

    // Suit les alias `s_mov_b32` / `s_mov_b64` et résout chaque `s_load_*`.
    let mut alias: HashMap<u8, u8> = HashMap::new();
    for d in insts {
        if d.inst.name == "s_mov_b32"
            && let (Some(Src::Sgpr(dst)), Some(Src::Sgpr(src))) = (
                d.inst.operands.first().map(|o| o.src),
                d.inst.operands.get(1).map(|o| o.src),
            )
        {
            let orig = alias.get(&src).copied().unwrap_or(src);
            alias.insert(dst, orig);
            continue;
        }
        if d.inst.name == "s_mov_b64"
            && let (Some(Src::Sgpr(dst)), Some(Src::Sgpr(src))) = (
                d.inst.operands.first().map(|o| o.src),
                d.inst.operands.get(1).map(|o| o.src),
            )
        {
            let orig_lo = alias.get(&src).copied().unwrap_or(src);
            let orig_hi = alias
                .get(&src.saturating_add(1))
                .copied()
                .unwrap_or(src.saturating_add(1));
            alias.insert(dst, orig_lo);
            alias.insert(dst.saturating_add(1), orig_hi);
            continue;
        }
        if d.inst.encoding != Encoding::Smem || !d.inst.name.starts_with("s_load_") {
            // Toute autre écriture scalaire invalide l'alias du SGPR destination.
            if d.inst.n_dst > 0
                && let Some(op) = d.inst.operands.first()
                && let Src::Sgpr(dst) = op.src
            {
                for k in dst..dst.saturating_add(op.width.max(1)) {
                    alias.remove(&k);
                }
            }
            continue;
        }
        let Some(Src::Sgpr(dst_sgpr)) = d.inst.operands.first().map(|o| o.src) else {
            continue;
        };
        let Some(Src::Sgpr(base_sgpr)) = d.inst.operands.get(1).map(|o| o.src) else {
            continue;
        };
        let origin = alias.get(&base_sgpr).copied().unwrap_or(base_sgpr);
        if usize::from(origin) + 2 > USER_DWORDS {
            continue;
        }
        let is_x4 = d.inst.name == "s_load_dwordx4";
        let is_x8 = d.inst.name == "s_load_dwordx8";
        let is_desc_load = (is_x4 || is_x8)
            && (desc_sgprs.contains(&dst_sgpr)
                || (is_x8 && desc_sgprs.contains(&dst_sgpr.saturating_add(4))));

        if is_desc_load {
            let dyn_off = d.inst.operands.get(2).map(|o| o.src);
            let const_off = match dyn_off {
                None | Some(Src::Null | Src::Off | Src::InlineInt(0)) => {
                    if d.inst.mods.offset >= 0 {
                        d.inst.mods.offset as u32
                    } else {
                        continue;
                    }
                }
                Some(Src::InlineInt(v)) if v >= 0 && d.inst.mods.offset == 0 => v as u32,
                _ => continue,
            };
            num_user_sgprs = num_user_sgprs.max(u32::from(origin) + 2);
            let kind = if is_x8 {
                NativeResourceKind::Image
            } else if sampler_sgprs.contains(&dst_sgpr) {
                NativeResourceKind::Sampler
            } else {
                NativeResourceKind::Buffer { stride: 4 }
            };
            let loc = NativeResourceLoc::Table {
                table_sgpr: origin,
                byte_offset: const_off,
            };
            // Un même SGPR ne peut pas être simultanément `Pointer` et `Table` : si le shader
            // charge des descripteurs depuis `origin`, supprimer un éventuel `Pointer(origin)`
            // générique ajouté depuis `RootSigLayout`.
            resources.retain(|r| r.loc != NativeResourceLoc::Pointer { sgpr: origin });
            if resources.len() < MAX_RESOURCES && !resources.iter().any(|r| r.loc == loc) {
                resources.push(NativeResource { kind, loc });
            }
        } else {
            // Lecture scalaire directe (`s_load_dword`, `s_load_dwordx2`, ou `x4` non-descripteur) :
            // `origin` est un pointeur de tampon racine (`Pointer`).
            let has_table = resources.iter().any(|r| {
                matches!(
                    r.loc,
                    NativeResourceLoc::Table { table_sgpr, .. } if table_sgpr == origin
                )
            });
            if !has_table {
                num_user_sgprs = num_user_sgprs.max(u32::from(origin) + 2);
                let loc = NativeResourceLoc::Pointer { sgpr: origin };
                if resources.len() < MAX_RESOURCES && !resources.iter().any(|r| r.loc == loc) {
                    resources.push(NativeResource {
                        kind: NativeResourceKind::Buffer { stride: 4 },
                        loc,
                    });
                }
            }
        }
    }

    (num_user_sgprs.clamp(2, USER_DWORDS as u32), resources)
}

/// Tables de descripteurs d'un shader compute.
///
/// Les entrées V#/T#/S# réellement utilisées sont découvertes par le traducteur (suivi symbolique
/// « base de table + offset constant » à travers `s_add_u32`/`s_addc_u32`/`s_mov_b32`/`s_mov_b64`,
/// `s_load_dwordx{1..16}`, plusieurs T#/S# par table, entrées indexées par (offset, nature)) et
/// remplacent les ressources `Table`/`Pointer` issues de l'heuristique.
///
/// Les tables sont d'abord celles du plan user-SGPR réel de la signature racine
/// (`pack_user_sgprs`). Toute autre paire de user-SGPR utilisée comme base de `s_load_*` (hors
/// pointeurs CBV/SRV/UAV du plan) est une table **inférée par l'usage** : elle est associée, dans
/// l'ordre de première utilisation, aux paramètres table de la signature que le shader n'utilise
/// pas à leur emplacement du plan. L'association est journalisée (`sb_warn`) et renvoyée sous
/// forme de plan `(paramètre racine, user-SGPR)`. Aucune donnée n'est inventée : trop de tables
/// utilisées, ou une paire qui recouvre des constantes racine, est une erreur précise.
///
/// Sans disposition de signature racine, rien n'est modifié.
pub(crate) fn discover_root_tables(
    meta: &mut NativeShaderMeta,
    code: &[u32],
    layout: Option<&RootSigLayout>,
) -> Result<Vec<(usize, u32)>, String> {
    let Some(l) = layout else {
        return Ok(Vec::new());
    };
    let declared = l.table_sgprs();
    let ptr_slots: Vec<u8> = l
        .params
        .iter()
        .zip(&l.first_sgpr)
        .filter(|((k, _), _)| {
            matches!(
                k,
                RootParamKind::Cbv | RootParamKind::Srv | RootParamKind::Uav
            )
        })
        .filter_map(|(_, &f)| u8::try_from(f).ok())
        .collect();
    meta.resources.retain(|r| match r.loc {
        NativeResourceLoc::Table { .. } => false,
        NativeResourceLoc::Pointer { sgpr } => ptr_slots.contains(&sgpr),
        _ => true,
    });
    let found = NativeShader {
        meta: meta.clone(),
        code: code.to_vec(),
    }
    .discover_tables(&declared, true)
    .map_err(|e| format!("tables de descripteurs (user-SGPR du plan {declared:?}) : {e}"))?;
    if meta.resources.len() + found.resources.len() > MAX_RESOURCES {
        return Err(format!(
            "{} entrées de table + {} autres ressources > {MAX_RESOURCES} (limite SBR2)",
            found.resources.len(),
            meta.resources.len()
        ));
    }
    let used_slots: Vec<u8> = found
        .resources
        .iter()
        .filter_map(|r| match r.loc {
            NativeResourceLoc::Table { table_sgpr, .. } => Some(table_sgpr),
            _ => None,
        })
        .collect();
    let inferred: Vec<u8> = found.tables[declared.len()..].to_vec();
    let mut plan: Vec<(usize, u32)> = Vec::new();
    if !inferred.is_empty() {
        // paramètres table du plan réel non utilisés à leur emplacement
        let free: Vec<usize> = l
            .params
            .iter()
            .enumerate()
            .filter(|(i, (k, _))| {
                *k == RootParamKind::Table
                    && u8::try_from(l.first_sgpr[*i]).is_ok_and(|s| !used_slots.contains(&s))
            })
            .map(|(i, _)| i)
            .collect();
        if inferred.len() > free.len() {
            return Err(format!(
                "plan user-SGPR : le shader utilise {} table(s) de descripteurs aux user-SGPR {inferred:?} (hors plan de la signature racine, tables du plan : {declared:?}) mais la signature n'a que {} paramètre(s) table libre(s) ({} au total) : association impossible",
                inferred.len(),
                free.len(),
                declared.len()
            ));
        }
        for (k, &sgpr) in inferred.iter().enumerate() {
            plan.push((free[k], u32::from(sgpr)));
        }
        // une paire déduite ne doit pas recouvrir des constantes racine (données lues par le
        // shader) ni une autre paire du plan
        for &(param, sgpr) in &plan {
            for (i, &(kind, n)) in l.params.iter().enumerate() {
                if i == param {
                    continue;
                }
                let first = plan
                    .iter()
                    .find(|(p, _)| *p == i)
                    .map_or(l.first_sgpr[i], |&(_, s)| s);
                let width = if kind == RootParamKind::Constants {
                    n.max(1)
                } else {
                    2
                };
                let overlaps = first < sgpr + 2 && sgpr < first + width;
                if overlaps && (kind == RootParamKind::Constants || plan.iter().any(|p| p.0 == i)) {
                    return Err(format!(
                        "plan user-SGPR : la table déduite s[{sgpr}:{}] (paramètre racine #{param}) recouvre le paramètre racine #{i} ({kind:?}) aux user-SGPR [{first}..{})",
                        sgpr + 1,
                        first + width
                    ));
                }
            }
            if sgpr + 2 > USER_DWORDS as u32 {
                return Err(format!(
                    "plan user-SGPR : la table déduite s[{sgpr}:{}] dépasse les {USER_DWORDS} user-SGPR",
                    sgpr + 1
                ));
            }
            meta.num_user_sgprs = meta.num_user_sgprs.max(sgpr + 2);
        }
        let desc: Vec<String> = plan
            .iter()
            .map(|&(p, s)| {
                format!(
                    "paramètre racine #{p} (plan réel s{}) -> s[{s}:{}]",
                    l.first_sgpr[p],
                    s + 1
                )
            })
            .collect();
        sb_warn!(
            CHANNEL,
            "FSPX Compute : plan user-SGPR déduit de l'usage (le shader charge ses descripteurs depuis des paires hors plan de la signature racine, tables du plan {declared:?}) : {}",
            desc.join(", ")
        );
    }
    meta.resources.extend(found.resources);
    Ok(plan)
}

/// Registres matériels compute AMD RDNA 2 extraits du bloc `kind0` FSPX (28 octets = 7 dwords) :
/// - mot 3 (`+0x0c`) : `COMPUTE_NUM_THREAD_X` (bits 0..15) | `COMPUTE_NUM_THREAD_Y` (bits 16..31)
/// - mot 4 (`+0x10`) : `COMPUTE_NUM_THREAD_Z` (bits 0..15)
/// - mot 6 (`+0x18`) : `COMPUTE_PGM_RSRC2` (`USER_SGPR` `[5:1]`, `TGID_X/Y/Z_EN` `[9:7]`,
///   `TIDIG_COMP_CNT` `[12:11]`, `LDS_SIZE` `[23:15]` en blocs de 128 dwords)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FspxComputeRsrc {
    pub local_size: [u32; 3],
    pub user_sgprs: u32,
    pub sys_sgpr_flags: u32,
    pub num_input_vgprs: u32,
    pub lds_dwords: u32,
}

pub(crate) fn parse_fspx_compute_rsrc(kind0: &[u8]) -> Option<FspxComputeRsrc> {
    if kind0.len() < 28 {
        return None;
    }
    let w = |i: usize| {
        let o = i * 4;
        u32::from_le_bytes([kind0[o], kind0[o + 1], kind0[o + 2], kind0[o + 3]])
    };
    let xy = w(3);
    let tx = (xy & 0xFFFF).clamp(1, 1024);
    let ty = ((xy >> 16) & 0xFFFF).clamp(1, 1024);
    let tz = (w(4) & 0xFFFF).clamp(1, 1024);
    if tx * ty * tz > 1024 {
        return None;
    }
    let rsrc2 = w(6);
    let user_sgprs = ((rsrc2 >> 1) & 0x1F).min(USER_DWORDS as u32);
    let sys_sgpr_flags = (rsrc2 >> 7) & 0x7;
    let tidig_comp_cnt = (rsrc2 >> 11) & 0x3;
    let num_input_vgprs = (tidig_comp_cnt + 1).min(3);
    // RDNA 2 : `LDS_SIZE` dans `COMPUTE_PGM_RSRC2[23:15]` en granulats de 128 dwords (512 octets).
    let lds_dwords = ((rsrc2 >> 15) & 0x1FF) * 128;
    Some(FspxComputeRsrc {
        local_size: [tx, ty, tz],
        user_sgprs,
        sys_sgpr_flags,
        num_input_vgprs,
        lds_dwords,
    })
}

/// Tente de compiler un descripteur FSPX Compute (`DeserializeComputePipelineStateX`, slot 67)
/// vers un vrai `VkPipeline` Vulkan natif RDNA 2.
///
/// En cas d'échec (en-tête binaire inconnu ou opcode RDNA 2 non encore couvert), journalise un
/// diagnostic compact sur les premiers appels et renvoie `None` afin que l'appelant bascule sans
/// régression sur le PSO standard.
pub(crate) fn try_compile_fspx_compute(
    dev: &D3D12DeviceObject,
    desc_addr: usize,
) -> Option<(usize, Arc<NativePso>)> {
    if desc_addr < 0x1_0000 {
        return None;
    }
    let hdr = sb_loader::trampoline::peek_memory(desc_addr, 0x48);
    if hdr.len() != 0x48 {
        return None;
    }
    let rd64 = |o: usize| {
        u64::from_le_bytes([
            hdr[o],
            hdr[o + 1],
            hdr[o + 2],
            hdr[o + 3],
            hdr[o + 4],
            hdr[o + 5],
            hdr[o + 6],
            hdr[o + 7],
        ]) as usize
    };
    let rs_ptr = rd64(0x00);
    let k0_ptr = rd64(0x08);
    let k0_sz = rd64(0x10);
    let k1_ptr = rd64(0x18);
    let k1_sz = rd64(0x20);
    let k6_ptr = rd64(0x28);
    let k6_sz = rd64(0x30);
    let k7_sz = rd64(0x40);

    if k6_ptr < 0x1_0000 || !(4..=2 * 1024 * 1024).contains(&k6_sz) {
        return None;
    }
    let k6 = sb_loader::trampoline::peek_memory(k6_ptr, k6_sz);
    if k6.len() != k6_sz {
        return None;
    }
    let k0 = if k0_ptr >= 0x1_0000 && k0_sz > 0 {
        sb_loader::trampoline::peek_memory(k0_ptr, k0_sz.min(64))
    } else {
        Vec::new()
    };
    let k1 = if k1_ptr >= 0x1_0000 && k1_sz > 0 {
        sb_loader::trampoline::peek_memory(k1_ptr, k1_sz.min(64))
    } else {
        Vec::new()
    };

    let layout = if d3d_known_object_name(rs_ptr) == Some("ID3D12RootSignature") {
        // SAFETY: `rs_ptr` validé comme `D3D12RootSigObject` créé par le bridge.
        unsafe { (*(rs_ptr as *const D3D12RootSigObject)).layout.clone() }
    } else {
        None
    };

    let rsrc = parse_fspx_compute_rsrc(&k0);
    let result: Result<(usize, Arc<NativePso>, usize, usize), String> = (|| {
        let code = extract_rdna2_code_words(&k6)?;
        let insts =
            decode_stream(&code, WaveSize::W32).map_err(|e| format!("decode_stream : {e}"))?;
        let (inferred_sgprs, resources) = infer_compute_resources(&insts, layout.as_deref());
        let mut meta = NativeShaderMeta::new(NativeStage::Compute, 32);
        if let Some(r) = rsrc {
            meta.local_size = r.local_size;
            meta.num_user_sgprs = r.user_sgprs.max(inferred_sgprs).min(USER_DWORDS as u32);
            meta.num_input_vgprs = r.num_input_vgprs;
            meta.sys_sgpr_flags = r.sys_sgpr_flags;
            meta.lds_dwords = r.lds_dwords;
        } else {
            meta.local_size = [64, 1, 1];
            meta.num_user_sgprs = inferred_sgprs;
            meta.num_input_vgprs = 3;
            meta.sys_sgpr_flags = 0x7;
        }
        meta.resources = resources;
        let user_plan = discover_root_tables(&mut meta, &code, layout.as_deref())?;
        let n_res = meta.resources.len();
        let n_words = code.len();
        let sbr2 = NativeShader {
            meta: meta.clone(),
            code,
        }
        .to_bytes();
        let mut renderer = dev.shared.renderer.lock();
        let id = renderer
            .create_native_compute_pipeline(&sbr2)
            .map_err(|e| format!("Vulkan/SPIR-V : {e}"))?;
        Ok((
            id,
            Arc::new(NativePso {
                stages: vec![meta],
                user_plan,
            }),
            n_words,
            n_res,
        ))
    })();

    static LOGGED_OK: AtomicU32 = AtomicU32::new(0);
    static LOGGED_ERR: AtomicU32 = AtomicU32::new(0);
    match result {
        Ok((id, native, n_words, n_res)) => {
            if LOGGED_OK.fetch_add(1, Ordering::Relaxed) < 5 {
                sb_info!(
                    CHANNEL,
                    "FSPX Compute -> VkPipeline #{id} compilé nativement ({n_words} mots RDNA 2, {n_res} ressources, kind0={k0_sz}o, kind1={k1_sz}o, kind6={k6_sz}o, kind7={k7_sz}o)"
                );
            } else {
                sb_debug!(
                    CHANNEL,
                    "FSPX Compute -> VkPipeline #{id} ({n_words} mots RDNA 2, {n_res} ressources)"
                );
            }
            Some((id, native))
        }
        Err(reason) => {
            if LOGGED_ERR.fetch_add(1, Ordering::Relaxed) < 4 {
                let k0_w = bytes_prefix_u32(&k0, 8);
                let k1_w = bytes_prefix_u32(&k1, 6);
                let k6_w = bytes_prefix_u32(&k6, 12);
                sb_info!(
                    CHANNEL,
                    "FSPX Compute repli non-natif (rs={rs_ptr:#x}, layout={}, kind0={k0_sz}o:{k0_w:x?}, kind1={k1_sz}o:{k1_w:x?}, kind6={k6_sz}o:{k6_w:x?}, kind7={k7_sz}o) : {reason}",
                    layout.is_some()
                );
            } else {
                sb_debug!(
                    CHANNEL,
                    "FSPX Compute repli non-natif (kind6={k6_sz}o) : {reason}"
                );
            }
            None
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
        21 => vk::Format::R32_SINT,
        57 => vk::Format::R8G8B8A8_SNORM,
        60 => vk::Format::R8G8B8A8_UINT,
        61 => vk::Format::R8G8B8A8_SINT,
        64 => vk::Format::R32G32_SFLOAT,
        13 => vk::Format::R16_SFLOAT,
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
    /// Couches (2D array) ou profondeur (3D) ; 1 pour une image 2D.
    depth: u32,
    dim: NativeImageDim,
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
    /// Par ressource : région mémoire des images T# (pour la recopie sortante des images de
    /// stockage).
    pub images: Vec<Option<ImageRegion>>,
}

/// Région mémoire d'une image T# résolue.
#[derive(Debug, Clone)]
pub(crate) struct ImageRegion {
    /// Adresse de base de la ressource propriétaire (`need` octets lus à cette adresse).
    pub base: usize,
    /// Octets linéaires serrés téléversés (comparaison avant/après un dispatch).
    pub before: Vec<u8>,
    /// Ressource linéaire : la recopie sortante est possible (sinon aucun re-tiling connu).
    pub linear: bool,
}

fn user_ptr(user: &[u32], sgpr: u8) -> u64 {
    let i = usize::from(sgpr);
    match (user.get(i), user.get(i + 1)) {
        (Some(&lo), Some(&hi)) => u64::from(lo) | (u64::from(hi) << 32),
        _ => 0,
    }
}

/// Résout l'image d'un T# : adresse (mots matériels) -> ressource du bridge -> image Vulkan.
fn resolve_image(
    ctx: &mut ResolveCtx<'_>,
    words: &[u32],
    handle: Option<usize>,
) -> (NativeBinding, Option<ImageRegion>) {
    let unbound = (NativeBinding::Unbound, None);
    let Ok(w8) = <[u32; 8]>::try_from(words) else {
        return unbound;
    };
    let Some(img) = decode_image(&w8) else {
        sb_warn!(CHANNEL, "Rdna2Native : T# invalide, image de repli liée");
        return unbound;
    };
    let Some(fmt) = gfx10_to_vk_format(img.format) else {
        sb_warn!(
            CHANNEL,
            "Rdna2Native : format GFX10 {} non supporté pour un T#",
            img.format
        );
        return unbound;
    };
    // 2D, 2D array et 3D (mip 0, première couche 0) ; contenu supposé linéaire serré
    // (couche/tranche après couche/tranche) ou détuilé par `detile_from_resource`.
    let (dim, depth) = match img.image_type {
        ImageType::Image2D if img.layer_count == 1 => (NativeImageDim::D2, 1),
        ImageType::Image2DArray if img.layer_count >= 1 => {
            (NativeImageDim::D2Array, img.layer_count)
        }
        ImageType::Image3D if img.depth >= 1 => (NativeImageDim::D3, img.depth),
        _ => {
            sb_warn!(
                CHANNEL,
                "Rdna2Native : T# {:?} ({} couche(s), profondeur {}) non supporté (2D, 2D array, 3D seulement)",
                img.image_type,
                img.layer_count,
                img.depth
            );
            return unbound;
        }
    };
    if img.first_mip != 0 || (dim == NativeImageDim::D2Array && img.first_layer != 0) {
        sb_warn!(
            CHANNEL,
            "Rdna2Native : T# avec mip de base {} / couche de base {} non supporté (mip 0, couche 0)",
            img.first_mip,
            img.first_layer
        );
        return unbound;
    }
    let Some(bpp) = native_format_texel_bytes(fmt) else {
        return unbound;
    };
    let (w, h) = (img.width, img.height);
    let need = (w as usize) * (h as usize) * (depth as usize) * bpp as usize;
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
        return unbound;
    };
    let mut data = None;
    let mut linear = true;
    if let Some(rp) = res_ptr {
        // SAFETY: `rp` provient de la table `descriptors` (ressource valide, voir plus haut).
        let res = unsafe { &*(rp as *const D3D12ResourceObject) };
        if res.swizzle_mode.is_some() {
            linear = false;
            data = detile_from_resource(res).filter(|d| d.len() >= need);
        }
    }
    let data = match data.or_else(|| read_bytes(base, need)) {
        Some(d) => d[..need].to_vec(),
        None => {
            sb_warn!(
                CHANNEL,
                "Rdna2Native : T# {w}x{h}x{depth} dépasse la mémoire de la ressource ({need} octets)"
            );
            return unbound;
        }
    };
    let key = ImageKey {
        addr: base,
        width: w,
        height: h,
        depth,
        dim,
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
                return unbound;
            }
            cache.insert(key, CachedImage { id: c.id, hash });
            c.id
        }
        None => match ctx
            .renderer
            .create_native_image_dim(w, h, depth, dim, fmt, &data)
        {
            Ok(id) => {
                cache.insert(key, CachedImage { id, hash });
                id
            }
            Err(e) => {
                sb_warn!(CHANNEL, "Rdna2Native : création d'image échouée : {e}");
                return unbound;
            }
        },
    };
    (
        NativeBinding::Image {
            image_id: id,
            swizzle: swizzle_to_vk(img.swizzle),
        },
        Some(ImageRegion {
            base,
            before: data,
            linear,
        }),
    )
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
    let mut images: Vec<Option<ImageRegion>> = Vec::with_capacity(meta.resources.len());
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
            images.push(None);
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
            images.push(None);
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
                images.push(None);
            }
            NativeResourceKind::Image => {
                let (b, region) = resolve_image(ctx, &words, handle);
                state.bindings.push(b);
                buffers.push(None);
                images.push(region);
            }
            NativeResourceKind::Sampler => {
                let b = <[u32; 4]>::try_from(words.as_slice())
                    .map(|w| NativeBinding::Sampler(sampler_to_vk(&w)))
                    .unwrap_or(NativeBinding::Unbound);
                state.bindings.push(b);
                buffers.push(None);
                images.push(None);
            }
        }
    }
    ResolvedStage {
        state,
        buffers,
        images,
    }
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
    match renderer.dispatch_native_ex(&cmd) {
        Ok(out) => {
            for (region, after) in cs.images.iter().zip(out.images) {
                let (Some(region), Some(after)) = (region, after) else {
                    continue;
                };
                if after == region.before {
                    continue;
                }
                if !region.linear {
                    sb_warn!(
                        CHANNEL,
                        "Rdna2Native : image de stockage modifiée par le dispatch mais ressource tuilée (aucun re-tiling connu) : le contenu reste côté GPU, la mémoire invitée n'est pas mise à jour"
                    );
                    continue;
                }
                let Some((base, total)) = owning_range(region.base) else {
                    continue;
                };
                if after.len() != region.before.len() || region.base + after.len() > base + total {
                    sb_warn!(
                        CHANNEL,
                        "Rdna2Native : recopie d'image de stockage hors de la ressource, ignorée"
                    );
                    continue;
                }
                // SAFETY: `region.base..+len` appartient à une ressource du bridge (vérifié
                // ci-dessus avec `owning_range`) et a été lu avec succès pendant ce même
                // `ExecuteCommandLists` ; `after.len() == before.len()`.
                unsafe {
                    ptr::copy_nonoverlapping(after.as_ptr(), region.base as *mut u8, after.len());
                }
            }
            for (orig, new) in cs.buffers.iter().zip(out.buffers) {
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

    /// `cs_store_2d` (llvm-mc gfx1030) : écrit `(x, y, 0.5, 1.0)` dans le texel `(x, y)` d'une
    /// image 8x8 (T# en entrée 0 de la table s[0:1], groupe 8x8 = 64 threads).
    const CS_STORE_2D: &[u32] = &[
        0xf40c0200, 0xfa000000, 0xbf8cc07f, 0x36020087, 0x2c040083, 0x7e060202, 0x7e080d01,
        0x7e0a0d02, 0x7e0c02f0, 0x7e0e02f2, 0xf0201f08, 0x00020401, 0xbf810000,
    ];
    const CS_STORE_3D: &[u32] = &[
        0xf40c0200, 0xfa000000, 0xbf8cc07f, 0x36020087, 0x2c040083, 0x7e060202, 0x7e080d01,
        0x7e0a0d02, 0x7e0c0d03, 0x7e0e02f2, 0xf0201f10, 0x00020401, 0xbf810000,
    ];

    fn storage_cs_bytes(code: &[u32]) -> Vec<u8> {
        let mut meta = NativeShaderMeta::new(NativeStage::Compute, 32);
        meta.local_size = [64, 1, 1];
        meta.num_user_sgprs = 2;
        meta.num_input_vgprs = 1;
        meta.sys_sgpr_flags = 1;
        meta.resources = vec![NativeResource {
            kind: NativeResourceKind::Image,
            loc: NativeResourceLoc::Table {
                table_sgpr: 0,
                byte_offset: 0,
            },
        }];
        NativeShader {
            meta,
            code: code.to_vec(),
        }
        .to_bytes()
    }

    /// Dispatch d'un CS d'écriture d'image de stockage sur une texture `dimension` (3 = 2D,
    /// 4 = 3D) `8x8xdepth` RGBA32F ; renvoie les octets de la ressource après recopie sortante.
    fn run_storage_texture(
        code: &[u32],
        dimension: u32,
        uav_dim: u32,
        depth: u32,
    ) -> Option<Vec<u8>> {
        let dev = make_device()?;
        let tex = make_resource(
            dev,
            D3D12ResourceDesc {
                dimension,
                width: 8,
                height: 8,
                depth_or_array_size: depth as u16,
                mip_levels: 1,
                format: 2,
                sample_desc: DxgiSampleDesc {
                    count: 1,
                    quality: 0,
                },
                ..Default::default()
            },
        );
        // la mémoire d'une texture committed est zéro-initialisée ; sa taille ne couvre que la
        // première tranche (`compute_resource_byte_len` ignore la profondeur / les couches)
        let heap = make_heap(2, 0);
        let mut uav = u32s(&[2, uav_dim, 0, 0, depth]);
        uav.resize(48, 0);
        device_create_uav(dev, tex.cast(), ptr::null_mut(), uav.as_ptr().cast(), heap);
        let cs = storage_cs_bytes(code);
        let rs = make_root_sig(dev, &[(0, 0)]);
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
            S_OK
        );
        let cl = make_cmdlist(dev);
        let cl_obj = cl.cast::<D3D12CmdListObject>();
        cmdlist_set_pipeline_state(cl_obj, pso.cast());
        cmdlist_set_compute_root_sig(cl_obj, rs.cast());
        cmdlist_set_compute_root_desc_table(cl_obj, 0, heap as u64);
        cmdlist_dispatch(cl_obj, depth, 1, 1);
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
        Some(read_res(tex, 8 * 8 * 16))
    }

    fn f32_of(b: &[u8], i: usize) -> f32 {
        f32::from_le_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]])
    }

    #[test]
    fn native_storage_image_through_com_writes_back_linear_2d_texture() {
        let _serial = crate::d3d12_xs::test_serial();
        let Some(out) = run_storage_texture(CS_STORE_2D, 3, 4, 1) else {
            return;
        };
        for y in 0..8usize {
            for x in 0..8usize {
                let o = (y * 8 + x) * 4;
                let got = [
                    f32_of(&out, o),
                    f32_of(&out, o + 1),
                    f32_of(&out, o + 2),
                    f32_of(&out, o + 3),
                ];
                assert_eq!(got, [x as f32, y as f32, 0.5, 1.0], "({x},{y})");
            }
        }
    }

    #[test]
    fn native_storage_image_through_com_refuses_3d_texture_truncated_by_bridge_sizing() {
        // `compute_resource_byte_len` (d3d12_xs) ne compte que la première tranche d'une texture
        // committed : un T# 3D 8x8x4 dépasse la ressource -> liaison non résolue, dispatch refusé
        // proprement, mémoire invitée intacte (aucun débordement).
        let _serial = crate::d3d12_xs::test_serial();
        let Some(out) = run_storage_texture(CS_STORE_3D, 4, 8, 4) else {
            return;
        };
        assert!(out.iter().all(|&b| b == 0));
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

    #[test]
    fn fspx_extract_trims_cacheline_padding() {
        let padded: [u32; 7] = [
            0xf408_0100,
            0xfa00_0000,
            0xbf81_0000, // s_endpgm
            0xbf9f_0000, // s_code_end
            0x0000_0000,
            0xbf80_0000, // s_nop 0
            0x0000_0000,
        ];
        let raw: Vec<u8> = padded.iter().flat_map(|w| w.to_le_bytes()).collect();
        let code = extract_rdna2_code_words(&raw).expect("ISA RDNA 2 valide");
        assert_eq!(code, vec![0xf408_0100, 0xfa00_0000, 0xbf81_0000]);
    }

    #[test]
    fn root_table_discovery_resolves_x16_loads_with_constant_pointer_offsets() {
        // `cs_table_x16` de docs/rdna2.md (llvm-mc gfx1030, wave32), écrit à la main.
        let code: Vec<u32> = vec![
            0xf4100200, 0xfa000000, 0x8018c000, 0x82198001, 0xf410080c, 0xfa000010, 0xf4080c0c,
            0xfa000040, 0xbf8cc07f, 0x34020082, 0xe0301000, 0x80030201, 0xe0301000, 0x80040301,
            0xe0301000, 0x80050401, 0xe0301000, 0x80080501, 0xe0301000, 0x80090601, 0xe0301000,
            0x800a0701, 0xe0301000, 0x800b0801, 0xe0301000, 0x800c0901, 0xbf8c3f70, 0xd543000a,
            0x04091503, 0xd543000a, 0x0429ff04, 0x00000064, 0xd543000a, 0x0429ff05, 0x000003e8,
            0xd543000a, 0x0429ff06, 0x00002710, 0xd543000a, 0x0429ff07, 0x000186a0, 0xd543000a,
            0x0429ff08, 0x000f4240, 0x4a141509, 0xe0701000, 0x80020a01, 0xbf810000,
        ];
        let layout =
            RootSigLayout::from_params(vec![(RootParamKind::Table, 0), (RootParamKind::Cbv, 0)])
                .expect("2 paramètres");
        assert_eq!(layout.table_sgprs(), vec![0]);
        let mut meta = NativeShaderMeta::new(NativeStage::Compute, 32);
        meta.num_user_sgprs = 4;
        meta.num_input_vgprs = 3;
        meta.local_size = [64, 1, 1];
        discover_root_tables(&mut meta, &code, Some(&layout)).expect("découverte");
        let offs: Vec<u32> = meta
            .resources
            .iter()
            .map(|r| match r.loc {
                NativeResourceLoc::Table {
                    table_sgpr: 0,
                    byte_offset,
                } => byte_offset,
                other => panic!("loc inattendue {other:?}"),
            })
            .collect();
        assert_eq!(offs, vec![0x00, 0x10, 0x20, 0x30, 0x50, 0x60, 0x70, 0x80]);
        assert!(
            meta.resources
                .iter()
                .all(|r| matches!(r.kind, NativeResourceKind::Buffer { .. }))
        );
        // la configuration SBR2 résultante se traduit et a 8 liaisons distinctes
        let t = NativeShader { meta, code }.translate().expect("traduction");
        assert_eq!(
            t.bindings
                .iter()
                .filter(|b| b.kind == "storage_buffer" && b.resource.is_some())
                .count(),
            8
        );
    }

    #[test]
    fn root_table_discovery_names_sgpr_and_instruction_on_dynamic_base() {
        let code: Vec<u32> = vec![
            0x80080200, 0x82098001, 0xf4080404, 0xfa000000, 0xbf8cc07f, 0xe0301000, 0x80040100,
            0xbf810000,
        ];
        let layout = RootSigLayout::from_params(vec![
            (RootParamKind::Table, 0),
            (RootParamKind::Constants, 2),
        ])
        .expect("layout");
        let mut meta = NativeShaderMeta::new(NativeStage::Compute, 32);
        meta.num_user_sgprs = 4;
        meta.num_input_vgprs = 3;
        let e = discover_root_tables(&mut meta, &code, Some(&layout)).expect_err("base dynamique");
        assert!(
            e.contains("s_load_dwordx4") && e.contains("s_add_u32") && e.contains("s[8:9]"),
            "{e}"
        );
        assert!(e.contains("user-SGPR du plan [0]"), "{e}");
    }

    /// `cs_infer_tables` de docs/rdna2.md (llvm-mc gfx1030) : tables aux user-SGPR s[4:5] (via
    /// `s_mov_b64`) et s[6:7], absentes du plan réel de la signature racine.
    const CS_INFER_TABLES: &[u32] = &[
        0xbe8a0404, 0xf4100405, 0xfa000000, 0xf4080803, 0xfa000010, 0xbf8cc07f, 0x34020082,
        0xe0301000, 0x80050201, 0xe0301000, 0x80070301, 0xbf8c3f70, 0x4a080702, 0xe0701000,
        0x80080401, 0xbf810000,
    ];

    fn infer_meta() -> NativeShaderMeta {
        let mut meta = NativeShaderMeta::new(NativeStage::Compute, 32);
        meta.num_user_sgprs = 8;
        meta.num_input_vgprs = 3;
        meta.local_size = [64, 1, 1];
        meta
    }

    #[test]
    fn root_table_inference_maps_usage_order_to_table_params_and_remaps_user_data() {
        let layout =
            RootSigLayout::from_params(vec![(RootParamKind::Table, 0), (RootParamKind::Table, 0)])
                .expect("layout");
        assert_eq!(layout.table_sgprs(), vec![0, 2]);
        let mut meta = infer_meta();
        let plan = discover_root_tables(&mut meta, CS_INFER_TABLES, Some(&layout)).expect("plan");
        // ordre d'usage : s4 puis s6 <-> paramètres table #0 puis #1
        assert_eq!(plan, vec![(0, 4), (1, 6)]);
        let again = {
            let mut m = infer_meta();
            discover_root_tables(&mut m, CS_INFER_TABLES, Some(&layout)).expect("plan")
        };
        assert_eq!(plan, again, "déterministe");
        let offs: Vec<(u8, u32)> = meta
            .resources
            .iter()
            .map(|r| match r.loc {
                NativeResourceLoc::Table {
                    table_sgpr,
                    byte_offset,
                } => (table_sgpr, byte_offset),
                other => panic!("loc inattendue {other:?}"),
            })
            .collect();
        assert_eq!(offs, vec![(4, 0x10), (4, 0x30), (6, 0x10)]);
        // les adresses de table arrivent aux user-SGPR utilisés par le shader
        let mut b = RootBinding::default();
        b.set_signature(Some(Arc::new(layout)));
        b.plan_override = plan;
        b.set_table(0, 0x1_0000_1000);
        b.set_table(1, 0x2_0000_2000);
        let u = b.user_data();
        assert_eq!(&u[..8], &[0, 0, 0, 0, 0x1000, 1, 0x2000, 2]);
        // la traduction finale (ressources explicites) réussit
        NativeShader {
            meta,
            code: CS_INFER_TABLES.to_vec(),
        }
        .translate()
        .expect("traduction");
    }

    /// `err_ptr_used_as_table` (table aux user-SGPR s[2:3], `s_load_dwordx16` puis `image_load`).
    const CS_PTR_AS_TABLE: &[u32] = &[
        0xf4100201, 0xfa000000, 0xbf8cc07f, 0xf0001f08, 0x00020400, 0xbf8c3f70, 0xbf810000,
    ];
    /// `cs_reuse_regs` (table s[4:5] ; s[12:15] = S# puis V# à des offsets disjoints).
    const CS_REUSE_REGS: &[u32] = &[
        0xf40c0402, 0xfa000000, 0xf4080302, 0xfa000020, 0xbf8cc07f, 0xf09c0f08, 0x00640400,
        0xf4080302, 0xfa000030, 0xbf8cc07f, 0xf4240606, 0xfa000000, 0xbf8c0070, 0x7e100218,
        0xe0701000, 0x80030800, 0xbf810000,
    ];

    /// Avant (heuristique historique `infer_compute_resources`, plan réel de la signature) /
    /// après (`discover_root_tables`) sur les motifs des journaux : cas 1 (x16 depuis une paire
    /// hors plan), cas 2 (cascade `image_load` après un `s_load` classé pointeur), cas 3 (SGPR
    /// réutilisés avec deux natures).
    #[test]
    fn log_patterns_old_heuristic_fails_discovery_succeeds() {
        let layout =
            RootSigLayout::from_params(vec![(RootParamKind::Table, 0), (RootParamKind::Table, 0)])
                .expect("layout");
        let before = |code: &[u32]| -> String {
            let insts = decode_stream(code, WaveSize::W32).expect("décodage");
            let (_, resources) = infer_compute_resources(&insts, Some(&layout));
            let mut meta = infer_meta();
            meta.resources = resources;
            // déclare les tables du plan comme le faisait l'ancienne glue
            match (NativeShader {
                meta,
                code: code.to_vec(),
            })
            .translate()
            {
                Ok(_) => "OK".into(),
                Err(e) => e.to_string(),
            }
        };
        let after = |code: &[u32]| -> Result<Vec<(usize, u32)>, String> {
            let mut meta = infer_meta();
            let insts = decode_stream(code, WaveSize::W32).expect("décodage");
            meta.resources = infer_compute_resources(&insts, Some(&layout)).1;
            let plan = discover_root_tables(&mut meta, code, Some(&layout))?;
            NativeShader {
                meta,
                code: code.to_vec(),
            }
            .translate()
            .map_err(|e| e.to_string())?;
            Ok(plan)
        };
        // cas 1
        let b = before(CS_INFER_TABLES);
        assert!(b != "OK" && b.contains("s_load_dwordx16"), "avant : {b}");
        assert_eq!(after(CS_INFER_TABLES).expect("après"), vec![(0, 4), (1, 6)]);
        // cas 2 (cascade)
        let b = before(CS_PTR_AS_TABLE);
        assert!(b != "OK" && b.contains("image_load"), "avant : {b}");
        assert_eq!(after(CS_PTR_AS_TABLE).expect("après"), Vec::new());
        // cas 3
        let b = before(CS_REUSE_REGS);
        assert!(b != "OK", "avant : {b}");
        assert_eq!(after(CS_REUSE_REGS).expect("après"), vec![(0, 4)]);
        eprintln!("avant cas 1 : {}", before(CS_INFER_TABLES));
        eprintln!("avant cas 2 : {}", before(CS_PTR_AS_TABLE));
        eprintln!("avant cas 3 : {}", before(CS_REUSE_REGS));
    }

    #[test]
    fn root_table_inference_fails_when_more_tables_than_table_params() {
        let layout = RootSigLayout::from_params(vec![(RootParamKind::Table, 0)]).expect("1 table");
        let mut meta = infer_meta();
        let e = discover_root_tables(&mut meta, CS_INFER_TABLES, Some(&layout))
            .expect_err("2 tables utilisées, 1 paramètre");
        assert!(
            e.contains("2 table(s)") && e.contains("1 paramètre(s) table libre(s)"),
            "{e}"
        );
    }

    #[test]
    fn root_table_inference_refuses_pairs_overlapping_root_constants() {
        let layout = RootSigLayout::from_params(vec![
            (RootParamKind::Table, 0),
            (RootParamKind::Table, 0),
            (RootParamKind::Constants, 4),
        ])
        .expect("layout");
        // constantes racine aux user-SGPR 4..8 : recouvrent s[4:5] et s[6:7]
        let mut meta = infer_meta();
        let e = discover_root_tables(&mut meta, CS_INFER_TABLES, Some(&layout))
            .expect_err("recouvrement");
        assert!(e.contains("recouvre") && e.contains("Constants"), "{e}");
    }

    #[test]
    fn root_table_plan_in_use_is_kept_as_is() {
        // shader qui utilise la table du plan réel s[0:1] : aucun plan déduit
        let code: Vec<u32> = vec![
            0xf4080200, 0xfa000000, 0xbf8cc07f, 0xe0301000, 0x80020100, 0xbf810000,
        ];
        let layout =
            RootSigLayout::from_params(vec![(RootParamKind::Table, 0), (RootParamKind::Table, 0)])
                .expect("layout");
        let mut meta = infer_meta();
        let plan = discover_root_tables(&mut meta, &code, Some(&layout)).expect("découverte");
        assert!(plan.is_empty(), "{plan:?}");
        assert_eq!(meta.resources.len(), 1);
    }

    #[test]
    fn root_table_discovery_without_layout_invents_nothing() {
        let mut meta = NativeShaderMeta::new(NativeStage::Compute, 32);
        discover_root_tables(&mut meta, &[0xbf810000], None).expect("sans signature : no-op");
        assert!(meta.resources.is_empty());
    }

    #[test]
    fn fspx_infer_compute_resources_root_and_table() {
        let words: [u32; 11] = [
            0xf40c_0200,
            0xfa00_0000,
            0xf408_0400,
            0xfa00_0020,
            0xf408_0500,
            0xfa00_0040,
            0xf080_0f08,
            0x0082_0402,
            0xe030_1000,
            0x8005_0000, // buffer_load_dword v0, v0, s[20:23], 0 offen
            0xbf81_0000,
        ];
        let insts = decode_stream(&words, WaveSize::W32).expect("decode");
        let layout =
            RootSigLayout::from_params(vec![(RootParamKind::Table, 1), (RootParamKind::Cbv, 0)])
                .expect("layout");
        let (n_sgpr, res) = infer_compute_resources(&insts, Some(&layout));
        assert!(n_sgpr >= 4);
        assert!(res.iter().any(|r| matches!(
            r,
            NativeResource {
                kind: NativeResourceKind::Buffer { .. },
                loc: NativeResourceLoc::Pointer { sgpr: 2 }
            }
        )));
        assert!(res.iter().any(|r| matches!(
            r,
            NativeResource {
                kind: NativeResourceKind::Image,
                loc: NativeResourceLoc::Table {
                    table_sgpr: 0,
                    byte_offset: 0
                }
            }
        )));
        assert!(res.iter().any(|r| matches!(
            r,
            NativeResource {
                kind: NativeResourceKind::Sampler,
                loc: NativeResourceLoc::Table {
                    table_sgpr: 0,
                    byte_offset: 0x20
                }
            }
        )));
        assert!(res.iter().any(|r| matches!(
            r,
            NativeResource {
                kind: NativeResourceKind::Buffer { .. },
                loc: NativeResourceLoc::Table {
                    table_sgpr: 0,
                    byte_offset: 0x40
                }
            }
        )));
    }

    #[test]
    fn fspx_compute_rsrc_decodes_threads_and_pgm_rsrc2() {
        // Registres matériels RDNA 2 : 8x8x1 threads, COMPUTE_PGM_RSRC2 = 0x0b8a
        // (USER_SGPR=5, TGID_X/Y/Z_EN=0b111, TIDIG_COMP_CNT=1 -> 2 VGPR d'entrée, LDS=0).
        let words: [u32; 7] = [
            0x4005_cf00,
            0x6c09_0102,
            0,
            0x0008_0008,
            1,
            0x402c_0343,
            0x0b8a,
        ];
        let raw: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let r = parse_fspx_compute_rsrc(&raw).expect("kind0 valide");
        assert_eq!(r.local_size, [8, 8, 1]);
        assert_eq!(r.user_sgprs, 5);
        assert_eq!(r.sys_sgpr_flags, 0x7);
        assert_eq!(r.num_input_vgprs, 2);
        assert_eq!(r.lds_dwords, 0);
    }

    #[test]
    fn fspx_compute_compiles_end_to_end_to_vk_pipeline() {
        let _serial = crate::d3d12_xs::test_serial();
        let Some(dev) = make_device() else {
            return;
        };
        let code_words: [u32; 7] = [
            0xf408_0100,
            0xfa00_0000,
            0xbf8c_c07f,
            0xe070_1000,
            0x8001_0000,
            0xbf81_0000,
            0xbf9f_0000,
        ];
        let k6: Vec<u8> = code_words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let k0 = [0u8; 28];
        let k1 = [0u8; 20];
        let rs = make_root_sig(dev, &[(0, 1)]);
        #[repr(C)]
        struct FspxDesc {
            rs: usize,
            k0_ptr: usize,
            k0_sz: u64,
            k1_ptr: usize,
            k1_sz: u64,
            k6_ptr: usize,
            k6_sz: u64,
            k7_ptr: usize,
            k7_sz: u64,
        }
        let desc = FspxDesc {
            rs: rs as usize,
            k0_ptr: k0.as_ptr() as usize,
            k0_sz: k0.len() as u64,
            k1_ptr: k1.as_ptr() as usize,
            k1_sz: k1.len() as u64,
            k6_ptr: k6.as_ptr() as usize,
            k6_sz: k6.len() as u64,
            k7_ptr: 0,
            k7_sz: 0,
        };
        let iid = IID_ID3D12PIPELINESTATE;
        let mut pso_raw: *mut c_void = ptr::null_mut();
        let hr = device_deserialize_compute_pso_x(
            dev,
            &raw const desc as usize,
            (&raw const iid) as usize,
            &raw mut pso_raw as usize,
            0,
            0,
            0,
        );
        assert_eq!(hr, S_OK);
        assert!(!pso_raw.is_null());
        // SAFETY: `pso_raw` est un `D3D12PsoObject` créé par `device_deserialize_compute_pso_x`.
        let pso = unsafe { &*(pso_raw as *const D3D12PsoObject) };
        assert!(pso.is_compute);
        assert!(
            pso.native.is_some(),
            "le PSO FSPX compute doit être compilé en pipeline Vulkan natif RDNA 2"
        );
    }

    #[test]
    fn fspx_compute_widened_s_mov_b32_compiles_to_vk_pipeline() {
        let _serial = crate::d3d12_xs::test_serial();
        let Some(dev) = make_device() else {
            return;
        };
        // Motif exact des shaders FSPX console :
        //   v_nop_e64 (3 dwords avec littéral 32 bits)
        //   s_inst_prefetch 3
        //   s_mov_b32 s8, s4
        //   s_mov_b32 s9, 0
        //   s_load_dwordx4 s[12:15], s[8:9], 0x0
        //   s_waitcnt lgkmcnt(0)
        //   buffer_store_dword v0, v0, s[12:15], 0 offen
        //   s_endpgm
        let code_words: [u32; 12] = [
            0xd580_0000,
            0x03fe_0100,
            0x0000_0ac0,
            0xbfa0_0003,
            0xbe88_0304, // s_mov_b32 s8, s4
            0xbe89_0380, // s_mov_b32 s9, 0
            0xf408_0304, // s_load_dwordx4 s[12:15], s[8:9], 0x0 (sbase=8/2=4, sdst=12)
            0xfa00_0000,
            0xbf8c_c07f, // s_waitcnt lgkmcnt(0)
            0xe070_1000, // buffer_store_dword v0, v0, s[12:15], 0 offen (srsrc=12/4=3)
            0x8003_0000,
            0xbf81_0000, // s_endpgm
        ];
        let k6: Vec<u8> = code_words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let k0_words: [u32; 7] = [
            0x4005_cf00,
            0x6c09_0102,
            0,
            0x0008_0008,
            1,
            0x402c_0343,
            0x0b8a,
        ];
        let k0: Vec<u8> = k0_words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let k1 = [0u8; 20];
        let rs = make_root_sig(dev, &[(0, 1), (0, 1), (0, 1)]);
        #[repr(C)]
        struct FspxDesc {
            rs: usize,
            k0_ptr: usize,
            k0_sz: u64,
            k1_ptr: usize,
            k1_sz: u64,
            k6_ptr: usize,
            k6_sz: u64,
            k7_ptr: usize,
            k7_sz: u64,
        }
        let desc = FspxDesc {
            rs: rs as usize,
            k0_ptr: k0.as_ptr() as usize,
            k0_sz: k0.len() as u64,
            k1_ptr: k1.as_ptr() as usize,
            k1_sz: k1.len() as u64,
            k6_ptr: k6.as_ptr() as usize,
            k6_sz: k6.len() as u64,
            k7_ptr: 0,
            k7_sz: 0,
        };
        let iid = IID_ID3D12PIPELINESTATE;
        let mut pso_raw: *mut c_void = ptr::null_mut();
        let hr = device_deserialize_compute_pso_x(
            dev,
            &raw const desc as usize,
            (&raw const iid) as usize,
            &raw mut pso_raw as usize,
            0,
            0,
            0,
        );
        assert_eq!(hr, S_OK);
        assert!(!pso_raw.is_null());
        // SAFETY: `pso_raw` est un `D3D12PsoObject` créé par `device_deserialize_compute_pso_x`.
        let pso = unsafe { &*(pso_raw as *const D3D12PsoObject) };
        assert!(
            pso.native.is_some(),
            "le shader FSPX avec v_nop_e64 + s_mov_b32 s8, s4 doit compiler en VkPipeline natif"
        );
    }
}
