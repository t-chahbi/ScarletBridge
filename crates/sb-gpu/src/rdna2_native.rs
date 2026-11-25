//! Conteneur **`SBR2`** (ScarletBridge RDNA 2, version 1) : format de shader `Rdna2Native`.
//!
//! Le conteneur est **propre à ScarletBridge** (clean-room) : il n'imite aucun format de jeu
//! commercial. Il enveloppe un blob de code RDNA 2 brut (dwords little-endian, ISA gfx10.3) et une
//! petite structure de métadonnées : étage, taille de vague, nombre de user-SGPR / VGPR d'entrée
//! et table des descripteurs lus par le programme. Voir `docs/rdna2_pipeline.md` pour le modèle de
//! liaison complet.
//!
//! # Disposition (tout en dwords little-endian)
//!
//! ```text
//!  0  magic  "SBR2"
//!  1  version (= 1)
//!  2  header_dwords (= 16 ; un lecteur ignore les dwords d'en-tête inconnus)
//!  3  stage : 0 = VS, 1 = PS, 2 = CS
//!  4  wave  : 32 | 64
//!  5..7  local_size x, y, z (CS ; 1,1,1 sinon)
//!  8  lds_dwords
//!  9  num_user_sgprs (0..=32) : s0..s(N-1) = user data
//! 10  num_input_vgprs
//! 11  sys_sgpr_flags (CS : bit0/1/2 = identifiant de groupe x/y/z, placés dans les SGPR
//!                     suivant immédiatement les user-SGPR, dans l'ordre x, y, z)
//! 12  n_resources (0..=32)
//! 13  code_dwords
//! 14  total_dwords (en-tête + ressources + code)
//! 15  réservé (0)
//! 16.. n_resources × 5 dwords : kind, loc_kind, loc_a, loc_b, stride
//! ..    code_dwords dwords de code RDNA 2
//! ```
//!
//! Enregistrement de ressource : `kind` 0 = V# (tampon, `stride` en octets pour les accès
//! `idxen`), 1 = T# (image), 2 = S# (échantillonneur) ; `loc_kind` 0 = `Table` (`loc_a` = SGPR de
//! base de la paire pointant la table, `loc_b` = décalage en octets), 1 = `Sgprs` (descripteur
//! dans les user-SGPR à partir de `loc_a`), 2 = `Pointer` (paire de SGPR `loc_a` contenant
//! l'adresse d'un tampon, cible de `s_load_*`).
//!
//! # Convention des registres d'entrée (v1)
//!
//! * VS : `v0` = `VertexIndex`, `v1` = `InstanceIndex` (si `num_input_vgprs >= 2`).
//! * PS : `v0`, `v1` = 0 (barycentriques : le traducteur utilise la valeur interpolée complète de
//!   Vulkan), `v2..v5` = `FragCoord.xyzw` (si `num_input_vgprs >= 6`).
//! * CS : `v0..v2` = identifiant local x, y, z (selon `num_input_vgprs`, 3 au plus).

use thiserror::Error;

use crate::rdna2::{
    ProgramInfo, Resource, ResourceKind, ResourceLoc, SgprInit, Stage, TranslateConfig, VgprInit,
    WaveSize, translate_program,
};

pub const SBR2_MAGIC: [u8; 4] = *b"SBR2";
pub const SBR2_VERSION: u32 = 1;
pub const HEADER_DWORDS: usize = 16;
pub const RESOURCE_DWORDS: usize = 5;
pub const MAX_RESOURCES: usize = 32;
pub const MAX_USER_SGPRS: u32 = 32;
/// Liaison Vulkan `(set 0, binding)` du tampon « user data » (mots des user-SGPR).
pub const USER_DATA_BINDING: u32 = 0;
/// Octets d'un emplacement de descripteur dans un tas (V#/S# complétés de zéros, T# sur 32).
pub const DESCRIPTOR_SLOT_BYTES: u32 = 32;

/// Étage d'un shader natif.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeStage {
    Vertex,
    Pixel,
    Compute,
}

impl NativeStage {
    fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Vertex),
            1 => Some(Self::Pixel),
            2 => Some(Self::Compute),
            _ => None,
        }
    }
    fn to_u32(self) -> u32 {
        match self {
            Self::Vertex => 0,
            Self::Pixel => 1,
            Self::Compute => 2,
        }
    }
}

/// Première liaison Vulkan des ressources d'un étage (VS 1.., PS 33.., CS 1..).
pub const fn resource_binding_base(stage: NativeStage) -> u32 {
    match stage {
        NativeStage::Vertex | NativeStage::Compute => 1,
        NativeStage::Pixel => 1 + MAX_RESOURCES as u32,
    }
}

/// Nature d'un descripteur.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeResourceKind {
    Buffer { stride: u32 },
    Image,
    Sampler,
}

impl NativeResourceKind {
    /// Taille du descripteur matériel en dwords (V# 4, T# 8, S# 4).
    pub const fn descriptor_dwords(self) -> u32 {
        match self {
            Self::Buffer { .. } | Self::Sampler => 4,
            Self::Image => 8,
        }
    }
}

/// Emplacement d'un descripteur.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeResourceLoc {
    /// Table pointée par la paire de SGPR `table_sgpr`, décalage `byte_offset`.
    Table { table_sgpr: u8, byte_offset: u32 },
    /// Descripteur dans les user-SGPR à partir de `first`.
    Sgprs { first: u8 },
    /// Paire de SGPR `sgpr` = adresse d'un tampon (racine CBV/SRV/UAV).
    Pointer { sgpr: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeResource {
    pub kind: NativeResourceKind,
    pub loc: NativeResourceLoc,
}

/// Métadonnées d'un shader natif.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeShaderMeta {
    pub stage: NativeStage,
    /// 32 ou 64.
    pub wave: u32,
    pub local_size: [u32; 3],
    pub lds_dwords: u32,
    pub num_user_sgprs: u32,
    pub num_input_vgprs: u32,
    /// CS : bits 0..2 = identifiants de groupe x/y/z.
    pub sys_sgpr_flags: u32,
    pub resources: Vec<NativeResource>,
}

impl NativeShaderMeta {
    pub fn new(stage: NativeStage, wave: u32) -> Self {
        Self {
            stage,
            wave,
            local_size: [1, 1, 1],
            lds_dwords: 0,
            num_user_sgprs: 0,
            num_input_vgprs: 0,
            sys_sgpr_flags: 0,
            resources: Vec::new(),
        }
    }
}

/// Erreurs de lecture / traduction d'un conteneur natif.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum NativeError {
    #[error("conteneur SBR2 invalide : {0}")]
    Invalid(String),
    #[error("version SBR2 non supportée : {0}")]
    Version(u32),
    #[error("traduction RDNA 2 : {0}")]
    Translate(String),
    #[error("incohérence ressources/liaisons : {0}")]
    Layout(String),
}

fn bad<T>(msg: impl Into<String>) -> Result<T, NativeError> {
    Err(NativeError::Invalid(msg.into()))
}

/// Conteneur décodé : métadonnées + code RDNA 2.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeShader {
    pub meta: NativeShaderMeta,
    pub code: Vec<u32>,
}

/// Liaison Vulkan utilisée par le module traduit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeBindingUse {
    pub binding: u32,
    /// `"storage_buffer"`, `"sampled_image"`, `"storage_image"` ou `"sampler"`.
    pub kind: &'static str,
    /// Indice dans `meta.resources`, `None` pour le tampon user data.
    pub resource: Option<usize>,
}

/// Résultat de la traduction d'un shader natif.
#[derive(Clone, Debug)]
pub struct NativeTranslation {
    pub spirv: Vec<u32>,
    pub info: ProgramInfo,
    pub bindings: Vec<NativeBindingUse>,
}

/// `true` si `bytes` commence par la magie `SBR2`.
pub fn is_native_container(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && bytes[0..4] == SBR2_MAGIC
}

fn rd(bytes: &[u8], i: usize) -> u32 {
    let o = i * 4;
    u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]])
}

impl NativeShader {
    /// Décode et valide un conteneur `SBR2`.
    pub fn parse(bytes: &[u8]) -> Result<Self, NativeError> {
        if !is_native_container(bytes) {
            return bad("magie SBR2 absente");
        }
        if bytes.len() < HEADER_DWORDS * 4 {
            return bad("en-tête tronqué");
        }
        let version = rd(bytes, 1);
        if version != SBR2_VERSION {
            return Err(NativeError::Version(version));
        }
        let header = rd(bytes, 2) as usize;
        if header < HEADER_DWORDS {
            return bad("header_dwords < 16");
        }
        let stage = NativeStage::from_u32(rd(bytes, 3))
            .ok_or_else(|| NativeError::Invalid("étage inconnu".into()))?;
        let wave = rd(bytes, 4);
        if wave != 32 && wave != 64 {
            return bad("taille de vague : 32 ou 64");
        }
        let local_size = [rd(bytes, 5), rd(bytes, 6), rd(bytes, 7)];
        let lds_dwords = rd(bytes, 8);
        let num_user_sgprs = rd(bytes, 9);
        let num_input_vgprs = rd(bytes, 10);
        let sys_sgpr_flags = rd(bytes, 11);
        let n_res = rd(bytes, 12) as usize;
        let code_dwords = rd(bytes, 13) as usize;
        let total = rd(bytes, 14) as usize;
        if num_user_sgprs > MAX_USER_SGPRS {
            return bad("plus de 32 user-SGPR");
        }
        if n_res > MAX_RESOURCES {
            return bad("plus de 32 ressources");
        }
        let need = header
            .checked_add(n_res * RESOURCE_DWORDS)
            .and_then(|v| v.checked_add(code_dwords))
            .ok_or_else(|| NativeError::Invalid("tailles incohérentes".into()))?;
        if need != total || total * 4 > bytes.len() || code_dwords == 0 {
            return bad("tailles incohérentes (total_dwords / code vide / données tronquées)");
        }
        match stage {
            NativeStage::Compute => {
                let prod = u64::from(local_size[0]) * u64::from(local_size[1]);
                let prod = prod * u64::from(local_size[2]);
                if local_size.contains(&0) || prod > 1024 {
                    return bad("local_size invalide (0 ou produit > 1024)");
                }
                if num_input_vgprs > 3 {
                    return bad("CS : au plus 3 VGPR d'entrée");
                }
                if sys_sgpr_flags & !7 != 0 {
                    return bad("sys_sgpr_flags : bits 0..2 seulement");
                }
                if num_user_sgprs + sys_sgpr_flags.count_ones() > 100 {
                    return bad("trop de SGPR d'entrée");
                }
            }
            NativeStage::Vertex => {
                if num_input_vgprs > 2 || sys_sgpr_flags != 0 {
                    return bad("VS : au plus 2 VGPR d'entrée, pas de SGPR système");
                }
            }
            NativeStage::Pixel => {
                if !(num_input_vgprs == 0 || num_input_vgprs == 2 || num_input_vgprs == 6)
                    || sys_sgpr_flags != 0
                {
                    return bad("PS : 0, 2 ou 6 VGPR d'entrée, pas de SGPR système");
                }
            }
        }
        let mut resources = Vec::with_capacity(n_res);
        for r in 0..n_res {
            let b = header + r * RESOURCE_DWORDS;
            let (kind, loc_kind, la, lb, stride) = (
                rd(bytes, b),
                rd(bytes, b + 1),
                rd(bytes, b + 2),
                rd(bytes, b + 3),
                rd(bytes, b + 4),
            );
            let kind = match kind {
                0 => NativeResourceKind::Buffer { stride },
                1 => NativeResourceKind::Image,
                2 => NativeResourceKind::Sampler,
                _ => return bad(format!("ressource {r} : kind inconnu")),
            };
            if la >= 106 {
                return bad(format!("ressource {r} : SGPR hors plage"));
            }
            let loc = match loc_kind {
                0 => NativeResourceLoc::Table {
                    table_sgpr: la as u8,
                    byte_offset: lb,
                },
                1 => NativeResourceLoc::Sgprs { first: la as u8 },
                2 => NativeResourceLoc::Pointer { sgpr: la as u8 },
                _ => return bad(format!("ressource {r} : loc_kind inconnu")),
            };
            // Les descripteurs « dans les SGPR » ou pointés doivent être des user-SGPR.
            match loc {
                NativeResourceLoc::Sgprs { first } => {
                    if u32::from(first) + kind.descriptor_dwords() > num_user_sgprs {
                        return bad(format!("ressource {r} : descripteur hors user-SGPR"));
                    }
                }
                NativeResourceLoc::Table { table_sgpr, .. }
                | NativeResourceLoc::Pointer { sgpr: table_sgpr } => {
                    if u32::from(table_sgpr) + 2 > num_user_sgprs {
                        return bad(format!("ressource {r} : paire de pointeur hors user-SGPR"));
                    }
                }
            }
            if matches!(loc, NativeResourceLoc::Pointer { .. })
                && !matches!(kind, NativeResourceKind::Buffer { .. })
            {
                return bad(format!("ressource {r} : Pointer réservé aux tampons"));
            }
            resources.push(NativeResource { kind, loc });
        }
        let code_start = header + n_res * RESOURCE_DWORDS;
        let code = (0..code_dwords)
            .map(|i| rd(bytes, code_start + i))
            .collect();
        Ok(Self {
            meta: NativeShaderMeta {
                stage,
                wave,
                local_size,
                lds_dwords,
                num_user_sgprs,
                num_input_vgprs,
                sys_sgpr_flags,
                resources,
            },
            code,
        })
    }

    /// Sérialise le conteneur (version 1).
    pub fn to_bytes(&self) -> Vec<u8> {
        let m = &self.meta;
        let total = HEADER_DWORDS + m.resources.len() * RESOURCE_DWORDS + self.code.len();
        let mut w: Vec<u32> = Vec::with_capacity(total);
        w.push(u32::from_le_bytes(SBR2_MAGIC));
        w.push(SBR2_VERSION);
        w.push(HEADER_DWORDS as u32);
        w.push(m.stage.to_u32());
        w.push(m.wave);
        w.extend_from_slice(&m.local_size);
        w.push(m.lds_dwords);
        w.push(m.num_user_sgprs);
        w.push(m.num_input_vgprs);
        w.push(m.sys_sgpr_flags);
        w.push(m.resources.len() as u32);
        w.push(self.code.len() as u32);
        w.push(total as u32);
        w.push(0);
        for r in &m.resources {
            let (kind, stride) = match r.kind {
                NativeResourceKind::Buffer { stride } => (0, stride),
                NativeResourceKind::Image => (1, 0),
                NativeResourceKind::Sampler => (2, 0),
            };
            let (lk, la, lb) = match r.loc {
                NativeResourceLoc::Table {
                    table_sgpr,
                    byte_offset,
                } => (0, u32::from(table_sgpr), byte_offset),
                NativeResourceLoc::Sgprs { first } => (1, u32::from(first), 0),
                NativeResourceLoc::Pointer { sgpr } => (2, u32::from(sgpr), 0),
            };
            w.extend_from_slice(&[kind, lk, la, lb, stride]);
        }
        w.extend_from_slice(&self.code);
        w.iter().flat_map(|d| d.to_le_bytes()).collect()
    }

    /// Configuration du traducteur RDNA 2 déduite des métadonnées (modèle de liaison v1).
    pub fn translate_config(&self) -> TranslateConfig {
        let m = &self.meta;
        let stage = match m.stage {
            NativeStage::Vertex => Stage::Vertex,
            NativeStage::Pixel => Stage::Fragment,
            NativeStage::Compute => Stage::Compute,
        };
        let wave = if m.wave == 64 {
            WaveSize::W64
        } else {
            WaveSize::W32
        };
        let mut cfg = TranslateConfig::new(stage, wave);
        cfg.local_size = m.local_size;
        cfg.lds_dwords = m.lds_dwords;
        if m.num_user_sgprs > 0 {
            cfg.user_data_binding = Some((0, USER_DATA_BINDING));
            for k in 0..m.num_user_sgprs {
                cfg.sgprs.push((k as u8, SgprInit::UserData(k)));
            }
        }
        if m.stage == NativeStage::Compute {
            let mut next = m.num_user_sgprs;
            for axis in 0..3u8 {
                if m.sys_sgpr_flags >> axis & 1 != 0 {
                    cfg.sgprs.push((next as u8, SgprInit::WorkgroupId(axis)));
                    next += 1;
                }
            }
        }
        match m.stage {
            NativeStage::Vertex => {
                if m.num_input_vgprs >= 1 {
                    cfg.vgprs.push((0, VgprInit::VertexId));
                }
                if m.num_input_vgprs >= 2 {
                    cfg.vgprs.push((1, VgprInit::InstanceId));
                }
            }
            NativeStage::Pixel => {
                if m.num_input_vgprs >= 2 {
                    cfg.vgprs.push((0, VgprInit::Constant(0)));
                    cfg.vgprs.push((1, VgprInit::Constant(0)));
                }
                if m.num_input_vgprs >= 6 {
                    for a in 0..4u8 {
                        cfg.vgprs.push((2 + a, VgprInit::FragCoord(a)));
                    }
                }
            }
            NativeStage::Compute => {
                for a in 0..m.num_input_vgprs.min(3) {
                    cfg.vgprs
                        .push((a as u8, VgprInit::LocalInvocationId(a as u8)));
                }
            }
        }
        let base = resource_binding_base(m.stage);
        for (i, r) in m.resources.iter().enumerate() {
            let kind = match r.kind {
                NativeResourceKind::Buffer { stride } => ResourceKind::Buffer { stride },
                NativeResourceKind::Image => ResourceKind::Image,
                NativeResourceKind::Sampler => ResourceKind::Sampler,
            };
            let loc = match r.loc {
                NativeResourceLoc::Table {
                    table_sgpr,
                    byte_offset,
                } => ResourceLoc::Table {
                    table_sgpr,
                    byte_offset,
                },
                NativeResourceLoc::Sgprs { first } => ResourceLoc::Sgprs(first),
                NativeResourceLoc::Pointer { sgpr } => ResourceLoc::Pointer(sgpr),
            };
            cfg.resources.push(Resource {
                kind,
                loc,
                set: 0,
                binding: base + i as u32,
            });
        }
        cfg
    }

    /// Traduit le programme en SPIR-V et recense les liaisons Vulkan réellement utilisées.
    pub fn translate(&self) -> Result<NativeTranslation, NativeError> {
        let cfg = self.translate_config();
        let t = translate_program(&self.code, &cfg)
            .map_err(|e| NativeError::Translate(e.to_string()))?;
        let base = resource_binding_base(self.meta.stage);
        let mut bindings = Vec::new();
        for b in &t.info.bindings {
            if b.set != 0 {
                return Err(NativeError::Layout(format!("set {} inattendu", b.set)));
            }
            let resource = if b.binding == USER_DATA_BINDING {
                None
            } else if b.binding >= base && ((b.binding - base) as usize) < self.meta.resources.len()
            {
                Some((b.binding - base) as usize)
            } else {
                return Err(NativeError::Layout(format!(
                    "liaison {} hors plage",
                    b.binding
                )));
            };
            // Cohérence type de ressource déclaré / usage dans le programme.
            let ok = matches!(
                (resource.map(|r| self.meta.resources[r].kind), b.kind),
                (None, "storage_buffer")
                    | (Some(NativeResourceKind::Buffer { .. }), "storage_buffer")
                    | (
                        Some(NativeResourceKind::Image),
                        "sampled_image" | "storage_image"
                    )
                    | (Some(NativeResourceKind::Sampler), "sampler")
            );
            if !ok {
                return Err(NativeError::Layout(format!(
                    "liaison {} : usage `{}` incompatible avec la ressource déclarée",
                    b.binding, b.kind
                )));
            }
            bindings.push(NativeBindingUse {
                binding: b.binding,
                kind: b.kind,
                resource,
            });
        }
        bindings.sort_by_key(|b| b.binding);
        bindings.dedup_by_key(|b| b.binding);
        Ok(NativeTranslation {
            spirv: t.words,
            info: t.info,
            bindings,
        })
    }
}

/// Programmes d'exemple écrits à la main (assemblés avec
/// `llvm-mc -triple=amdgcn -mcpu=gfx1030 -mattr=+wavefrontsize32`, octets figés ici).
///
/// Convention de la signature racine utilisée (voir `docs/rdna2_pipeline.md`) :
///
/// * VS texturé : `p0` = table CBV/SRV/UAV (`s0:s1`), slot 0 = V# du tampon de sommets.
/// * PS « teinte » : `p0` = table CBV/SRV/UAV (`s0:s1`, slot 1 = T#), `p1` = table de samplers
///   (`s2:s3`, slot 0 = S#), `p2` = 4 constantes racine (`s4..s7`, teinte RGBA `f32`).
/// * CS affine : `p0` = table (`s0:s1`, slot 0 = V# entrée, slot 1 = V# sortie), `p1` = CBV
///   racine (`s2:s3`, biais `f32` au début), `p2` = 1 constante racine (`s4` = échelle `f32`),
///   `s5` = identifiant de groupe x. `out[i] = f32(in[i]) * échelle + biais`.
pub mod samples {
    use super::*;

    /// Vertex = 32 octets : `position.xyzw` puis `uv.xyzw`.
    const VS_TEXTURED: [u32; 14] = [
        0xf4080100, 0xfa000000, 0xbf8cc07f, 0x34020085, 0xe0381000, 0x80010401, 0xe0381010,
        0x80010801, 0xbf8c3f70, 0xf800020f, 0x0b0a0908, 0xf80008cf, 0x07060504, 0xbf810000,
    ];
    const PS_TINT: [u32; 21] = [
        0xf40c0200, 0xfa000020, 0xf4080401, 0xfa000000, 0xbf8cc07f, 0xc8080000, 0xc80c0100,
        0xc8090001, 0xc80d0101, 0xf0800f08, 0x00820402, 0xbf8c3f70, 0x10080804, 0x100a0a05,
        0x100c0c06, 0x100e0e07, 0x5e080b04, 0x5e0a0f06, 0xf8001c0f, 0x00000504, 0xbf810000,
    ];
    const CS_AFFINE: [u32; 19] = [
        0xf4080200, 0xfa000000, 0xf4080300, 0xfa000020, 0xf4000181, 0xfa000000, 0x8f078605,
        0xbf8cc07f, 0x4a000007, 0x34020082, 0xe0301000, 0x80020201, 0xbf8c3f70, 0x7e060d02,
        0xd54b0003, 0x00180903, 0xe0701000, 0x80030301, 0xbf810000,
    ];

    fn table(table_sgpr: u8, byte_offset: u32, kind: NativeResourceKind) -> NativeResource {
        NativeResource {
            kind,
            loc: NativeResourceLoc::Table {
                table_sgpr,
                byte_offset,
            },
        }
    }

    pub fn vs_textured() -> NativeShader {
        let mut meta = NativeShaderMeta::new(NativeStage::Vertex, 32);
        meta.num_user_sgprs = 2;
        meta.num_input_vgprs = 1;
        meta.resources = vec![table(0, 0, NativeResourceKind::Buffer { stride: 0 })];
        NativeShader {
            meta,
            code: VS_TEXTURED.to_vec(),
        }
    }

    pub fn ps_tint() -> NativeShader {
        let mut meta = NativeShaderMeta::new(NativeStage::Pixel, 32);
        meta.num_user_sgprs = 8;
        meta.num_input_vgprs = 2;
        meta.resources = vec![
            table(0, DESCRIPTOR_SLOT_BYTES, NativeResourceKind::Image),
            table(2, 0, NativeResourceKind::Sampler),
        ];
        NativeShader {
            meta,
            code: PS_TINT.to_vec(),
        }
    }

    pub fn cs_affine() -> NativeShader {
        let mut meta = NativeShaderMeta::new(NativeStage::Compute, 32);
        meta.local_size = [64, 1, 1];
        meta.num_user_sgprs = 5;
        meta.sys_sgpr_flags = 1;
        meta.num_input_vgprs = 1;
        meta.resources = vec![
            table(0, 0, NativeResourceKind::Buffer { stride: 0 }),
            table(
                0,
                DESCRIPTOR_SLOT_BYTES,
                NativeResourceKind::Buffer { stride: 0 },
            ),
            NativeResource {
                kind: NativeResourceKind::Buffer { stride: 0 },
                loc: NativeResourceLoc::Pointer { sgpr: 2 },
            },
        ];
        NativeShader {
            meta,
            code: CS_AFFINE.to_vec(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn all_samples() -> Vec<NativeShader> {
        vec![
            samples::vs_textured(),
            samples::ps_tint(),
            samples::cs_affine(),
        ]
    }

    #[test]
    fn container_roundtrip() {
        for s in all_samples() {
            let bytes = s.to_bytes();
            assert!(is_native_container(&bytes));
            assert_eq!(NativeShader::parse(&bytes).unwrap(), s);
        }
    }

    #[test]
    fn parse_rejects_malformed() {
        let good = samples::cs_affine().to_bytes();
        // magie / version / troncature
        assert!(
            NativeShader::parse(
                b"DXBC0000000000000000000000000000000000000000000000000000000000000000"
            )
            .is_err()
        );
        let mut b = good.clone();
        b[4..8].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(NativeShader::parse(&b), Err(NativeError::Version(2)));
        for cut in [3, 20, 63, good.len() - 4] {
            assert!(NativeShader::parse(&good[..cut]).is_err(), "cut={cut}");
        }
        // vague invalide, local_size nul, user-SGPR > 32, ressource hors user-SGPR
        let patch = |idx: usize, v: u32| {
            let mut b = good.clone();
            b[idx * 4..idx * 4 + 4].copy_from_slice(&v.to_le_bytes());
            NativeShader::parse(&b)
        };
        assert!(patch(4, 16).is_err());
        assert!(patch(5, 0).is_err());
        assert!(patch(9, 33).is_err());
        assert!(
            patch(9, 1).is_err(),
            "table s0:s1 hors des user-SGPR déclarés"
        );
        assert!(patch(3, 9).is_err());
        assert!(patch(14, 1).is_err(), "total incohérent");
    }

    #[test]
    fn samples_translate_to_valid_spirv_and_expected_bindings() {
        let vs = samples::vs_textured().translate().unwrap();
        // user data (0) + V# (1)
        let b: Vec<(u32, &str)> = vs.bindings.iter().map(|b| (b.binding, b.kind)).collect();
        assert_eq!(b, vec![(0, "storage_buffer"), (1, "storage_buffer")]);
        let ps = samples::ps_tint().translate().unwrap();
        let b: Vec<(u32, &str)> = ps.bindings.iter().map(|b| (b.binding, b.kind)).collect();
        assert_eq!(
            b,
            vec![
                (0, "storage_buffer"),
                (33, "sampled_image"),
                (34, "sampler")
            ]
        );
        let cs = samples::cs_affine().translate().unwrap();
        assert_eq!(cs.bindings.len(), 4, "user data + 3 tampons");
        assert_eq!(cs.bindings[3].resource, Some(2));
        for t in [&vs, &ps, &cs] {
            assert_eq!(t.spirv[0], 0x0723_0203, "magie SPIR-V");
        }
    }

    #[test]
    fn layout_mismatch_is_reported() {
        // Le PS lit une image à la liaison 33 : la déclarer comme tampon doit échouer.
        let mut s = samples::ps_tint();
        s.meta.resources[0].kind = NativeResourceKind::Buffer { stride: 0 };
        assert!(s.translate().is_err());
    }

    #[test]
    fn samples_pass_spirv_val_when_available() {
        let Ok(path) = which_spirv_val() else {
            eprintln!("spirv-val absent : validation ignorée");
            return;
        };
        for (name, s) in [
            ("vs", samples::vs_textured()),
            ("ps", samples::ps_tint()),
            ("cs", samples::cs_affine()),
        ] {
            let t = s.translate().unwrap();
            let bytes: Vec<u8> = t.spirv.iter().flat_map(|w| w.to_le_bytes()).collect();
            let tmp = std::env::temp_dir().join(format!("sbr2_{name}_{}.spv", std::process::id()));
            std::fs::write(&tmp, bytes).unwrap();
            let out = std::process::Command::new(&path)
                .args(["--target-env", "vulkan1.1"])
                .arg(&tmp)
                .output()
                .unwrap();
            let _ = std::fs::remove_file(&tmp);
            assert!(
                out.status.success(),
                "spirv-val {name} : {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    fn which_spirv_val() -> Result<String, ()> {
        std::process::Command::new("spirv-val")
            .arg("--version")
            .output()
            .map(|_| "spirv-val".to_string())
            .map_err(|_| ())
    }
}
