//! Traduction d'un programme RDNA 2 (VS/PS/CS) vers SPIR-V 1.3 (Vulkan 1.1).
//!
//! # Modèle
//! * Chaque invocation SPIR-V = une lane de la vague. Les registres sont des variables `Function`
//!   32 bits (`uint`) : 128 codes scalaires (SGPR/VCC/TTMP/M0/EXEC) et 256 VGPR, créés à la
//!   demande. Les SGPR sont **uniformes** : toutes les lanes calculent les mêmes valeurs.
//! * `EXEC`, `VCC` et tout masque de lanes sont de *vrais* registres entiers (1 dword en wave32,
//!   2 en wave64). Un `v_cmp` écrit `ballot(cmp && exec)` ; `v_cndmask` et les écritures VALU
//!   prédiquées par `EXEC` extraient le bit de la lane courante (`SubgroupLocalInvocationId`).
//!   Si le programme n'écrit jamais EXEC, la prédication est supprimée (code linéaire sans
//!   capacité de sous-groupe).
//! * Le flot de contrôle est une machine à `pc` : une boucle SPIR-V structurée contenant un
//!   `OpSwitch` sur le bloc courant (toujours valide, y compris pour les CFG irréductibles).
//!   Les branches RDNA étant uniformes sur la vague, le `pc` reste identique pour toutes les lanes.
//! * Les descripteurs V#/T#/S# ne sont pas interprétés : la configuration indique quels SGPR
//!   (ou quelle entrée d'une table pointée par un SGPR) correspondent à quelle liaison Vulkan.

use std::collections::BTreeMap;

use thiserror::Error;

use super::cfg::{BranchCond, Cfg, CfgError, Terminator, build_cfg, writes_exec};
use super::decode::{Decoded, decode_stream};
use super::inst::{DecodeError, Instruction, Operand, Src, WaveSize};
use crate::dxbc_to_spirv::spv::{Sc, Spv, Val};

// ---------------------------------------------------------------- configuration

/// Étage du pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Vertex,
    Fragment,
    Compute,
}

/// Nature d'une ressource décrite par un descripteur.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceKind {
    /// Buffer (V#) lu/écrit comme tableau de dwords ; `stride` en octets pour les accès `idxen`.
    Buffer { stride: u32 },
    /// Image échantillonnable (T#).
    Image,
    /// Échantillonneur (S#).
    Sampler,
}

/// Emplacement d'un descripteur dans les registres du shader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceLoc {
    /// Le descripteur occupe directement des SGPR utilisateur à partir de celui-ci.
    Sgprs(u8),
    /// Le descripteur est lu par `s_load_*` dans une table pointée par la paire de SGPR
    /// `table_sgpr` (offset en octets depuis la base de la table).
    Table { table_sgpr: u8, byte_offset: u32 },
    /// La paire de SGPR contient un pointeur vers les données d'un buffer (cible de `s_load_*`).
    Pointer(u8),
}

/// Liaison Vulkan associée à une ressource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resource {
    pub kind: ResourceKind,
    pub loc: ResourceLoc,
    pub set: u32,
    pub binding: u32,
}

/// Valeur initiale d'un SGPR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SgprInit {
    /// Mot `i` du buffer « user data » (voir [`TranslateConfig::user_data_binding`]).
    UserData(u32),
    /// Identifiant de groupe de travail (axe 0..=2), CS uniquement.
    WorkgroupId(u8),
    Constant(u32),
}

/// Valeur initiale d'un VGPR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VgprInit {
    VertexId,
    InstanceId,
    /// Identifiant local d'invocation (axe 0..=2), CS uniquement.
    LocalInvocationId(u8),
    /// Composante de `FragCoord` (0..=3), PS uniquement.
    FragCoord(u8),
    Constant(u32),
}

/// Paramètres de traduction.
#[derive(Clone, Debug)]
pub struct TranslateConfig {
    pub stage: Stage,
    pub wave: WaveSize,
    /// Taille de groupe de travail (CS).
    pub local_size: [u32; 3],
    pub sgprs: Vec<(u8, SgprInit)>,
    pub vgprs: Vec<(u8, VgprInit)>,
    pub resources: Vec<Resource>,
    /// `(set, binding)` du buffer de données utilisateur (requis si `SgprInit::UserData`).
    pub user_data_binding: Option<(u32, u32)>,
    /// Taille de la LDS en dwords (requis pour les instructions `ds_*`).
    pub lds_dwords: u32,
}

impl TranslateConfig {
    pub fn new(stage: Stage, wave: WaveSize) -> Self {
        TranslateConfig {
            stage,
            wave,
            local_size: [1, 1, 1],
            sgprs: Vec::new(),
            vgprs: Vec::new(),
            resources: Vec::new(),
            user_data_binding: None,
            lds_dwords: 0,
        }
    }
}

/// Erreurs de traduction.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TranslateError {
    #[error("décodage : {0}")]
    Decode(#[from] DecodeError),
    #[error("CFG : {0}")]
    Cfg(#[from] CfgError),
    #[error("instruction non supportée : {0}")]
    Unsupported(String),
    #[error("ressource non résolue : {0}")]
    Unresolved(String),
    #[error("configuration invalide : {0}")]
    Config(String),
}

pub(crate) type TResult<T> = Result<T, TranslateError>;

pub(crate) fn unsupported<T>(inst: &Instruction) -> TResult<T> {
    Err(TranslateError::Unsupported(inst.name.to_string()))
}

/// Liaison utilisée par le module généré.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingUse {
    pub set: u32,
    pub binding: u32,
    pub kind: &'static str,
}

/// Informations sur le module produit.
#[derive(Clone, Debug, Default)]
pub struct ProgramInfo {
    pub instructions: usize,
    pub blocks: usize,
    /// Le programme modifie EXEC : prédication par lane + capacités de sous-groupe.
    pub uses_exec_mask: bool,
    pub bindings: Vec<BindingUse>,
    /// Emplacements (`param N` / `Location N`) écrits par un VS.
    pub vs_params: Vec<u32>,
    /// Attributs lus par un PS (`v_interp_*`).
    pub ps_attrs: Vec<u32>,
    /// Cibles couleur écrites par un PS.
    pub mrts: Vec<u32>,
    pub writes_depth: bool,
}

/// Résultat : module SPIR-V + informations.
#[derive(Clone, Debug)]
pub struct TranslatedProgram {
    pub words: Vec<u32>,
    pub info: ProgramInfo,
}

// ---------------------------------------------------------------- traducteur

/// Descripteur résolu à la compilation pour une plage de SGPR.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ResRef {
    pub kind: ResourceKind,
    pub set: u32,
    pub binding: u32,
    pub size: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum BuiltinIn {
    VertexIndex,
    InstanceIndex,
    LocalId,
    WorkgroupId,
    FragCoord,
    SubgroupLane,
}

pub(crate) struct Tr<'a> {
    pub spv: Spv,
    pub cfg: &'a TranslateConfig,
    pub wave: WaveSize,
    entry_pos: usize,
    var_words: usize,
    svars: [Option<u32>; 128],
    vvars: [Option<u32>; 256],
    scc_var: Option<u32>,
    pub exec_const: bool,
    pub res_map: BTreeMap<u8, ResRef>,
    pub interface: Vec<u32>,
    builtins: BTreeMap<BuiltinIn, u32>,
    pub position: Option<u32>,
    pub params: BTreeMap<u32, u32>,
    pub mrts: BTreeMap<u32, u32>,
    pub depth: Option<u32>,
    pub attrs: BTreeMap<u32, u32>,
    pub buffers: BTreeMap<(u32, u32), u32>,
    pub images: BTreeMap<(u32, u32), (u32, u32, u8)>,
    pub samplers: BTreeMap<(u32, u32), u32>,
    pub lds: Option<u32>,
    user_data: Option<u32>,
    ra_decorated: bool,
    pub info: ProgramInfo,
}

impl<'a> Tr<'a> {
    fn new(cfg: &'a TranslateConfig) -> Self {
        let mut spv = Spv::new();
        spv.caps.insert(1);
        Tr {
            spv,
            cfg,
            wave: cfg.wave,
            entry_pos: 0,
            var_words: 0,
            svars: [None; 128],
            vvars: [None; 256],
            scc_var: None,
            exec_const: true,
            res_map: BTreeMap::new(),
            interface: Vec::new(),
            builtins: BTreeMap::new(),
            position: None,
            params: BTreeMap::new(),
            mrts: BTreeMap::new(),
            depth: None,
            attrs: BTreeMap::new(),
            buffers: BTreeMap::new(),
            images: BTreeMap::new(),
            samplers: BTreeMap::new(),
            lds: None,
            user_data: None,
            ra_decorated: false,
            info: ProgramInfo::default(),
        }
    }

    // ------------------------------------------------ variables de registres
    fn new_function_var(&mut self, sc: Sc) -> u32 {
        let t = self.spv.ty(sc, 1);
        let pt = self.spv.ptr(7, t);
        let init = match sc {
            Sc::Bool => self.spv.c_bool(false),
            _ => self.spv.c_u32(0),
        };
        let id = self.spv.id();
        self.spv
            .insert_raw(self.entry_pos + self.var_words, 59, &[pt, id, 7, init]);
        self.var_words += 5;
        id
    }

    pub fn svar(&mut self, code: usize) -> u32 {
        if let Some(v) = self.svars[code] {
            return v;
        }
        let v = self.new_function_var(Sc::U32);
        self.svars[code] = Some(v);
        v
    }

    pub fn vvar(&mut self, n: usize) -> u32 {
        if let Some(v) = self.vvars[n] {
            return v;
        }
        let v = self.new_function_var(Sc::U32);
        self.vvars[n] = Some(v);
        v
    }

    fn scc(&mut self) -> u32 {
        if let Some(v) = self.scc_var {
            return v;
        }
        let v = self.new_function_var(Sc::Bool);
        self.scc_var = Some(v);
        v
    }

    pub fn load_u32(&mut self, var: u32) -> Val {
        let t = self.spv.ty(Sc::U32, 1);
        let id = self.spv.emit(61, t, &[var]);
        Val {
            id,
            sc: Sc::U32,
            n: 1,
        }
    }

    pub fn store_u32(&mut self, var: u32, v: Val) {
        let v = self.spv.bitcast(v, Sc::U32);
        self.spv.emit_void(62, &[var, v.id]);
    }

    pub fn scc_get(&mut self) -> Val {
        let var = self.scc();
        let t = self.spv.ty(Sc::Bool, 1);
        let id = self.spv.emit(61, t, &[var]);
        Val {
            id,
            sc: Sc::Bool,
            n: 1,
        }
    }

    pub fn scc_set(&mut self, b: Val) {
        let var = self.scc();
        self.spv.emit_void(62, &[var, b.id]);
    }

    // ------------------------------------------------ constantes / opérations courantes
    pub fn cu(&mut self, v: u32) -> Val {
        Val {
            id: self.spv.c_u32(v),
            sc: Sc::U32,
            n: 1,
        }
    }

    pub fn cf(&mut self, v: f32) -> Val {
        Val {
            id: self.spv.c_f32(v),
            sc: Sc::F32,
            n: 1,
        }
    }

    pub fn op2(&mut self, op: u16, sc: Sc, a: Val, b: Val) -> Val {
        self.spv.val(op, sc, 1, &[a.id, b.id])
    }

    pub fn op1(&mut self, op: u16, sc: Sc, a: Val) -> Val {
        self.spv.val(op, sc, 1, &[a.id])
    }

    pub fn glsl(&mut self, sc: Sc, inst: u32, args: &[Val]) -> Val {
        let ids: Vec<u32> = args.iter().map(|a| a.id).collect();
        self.spv.glsl_op(sc, 1, inst, &ids)
    }

    pub fn select(&mut self, c: Val, a: Val, b: Val) -> Val {
        self.spv.val(169, a.sc, 1, &[c.id, a.id, b.id])
    }

    pub fn bool_not(&mut self, a: Val) -> Val {
        self.spv.val(168, Sc::Bool, 1, &[a.id])
    }

    pub fn bool_and(&mut self, a: Val, b: Val) -> Val {
        self.spv.val(167, Sc::Bool, 1, &[a.id, b.id])
    }

    pub fn bool_or(&mut self, a: Val, b: Val) -> Val {
        self.spv.val(166, Sc::Bool, 1, &[a.id, b.id])
    }

    pub fn ne0(&mut self, a: Val) -> Val {
        let z = self.cu(0);
        let a = self.spv.bitcast(a, Sc::U32);
        self.op2(171, Sc::Bool, a, z)
    }

    // ------------------------------------------------ entrées intégrées
    fn builtin(&mut self, which: BuiltinIn) -> u32 {
        if let Some(&v) = self.builtins.get(&which) {
            return v;
        }
        let (sc, n, bi) = match which {
            BuiltinIn::VertexIndex => (Sc::U32, 1, 42),
            BuiltinIn::InstanceIndex => (Sc::U32, 1, 43),
            BuiltinIn::LocalId => (Sc::U32, 3, 27),
            BuiltinIn::WorkgroupId => (Sc::U32, 3, 26),
            BuiltinIn::FragCoord => (Sc::F32, 4, 15),
            BuiltinIn::SubgroupLane => (Sc::U32, 1, 41),
        };
        let t = self.spv.ty(sc, n);
        let pt = self.spv.ptr(1, t);
        let var = self.spv.global_var(pt, 1, None);
        self.spv.decorate(var, 11, &[bi]);
        if self.cfg.stage == Stage::Fragment && sc == Sc::U32 {
            self.spv.decorate(var, 14, &[]); // Flat : entrées entières d'un PS
        }
        if which == BuiltinIn::SubgroupLane {
            self.spv.caps.insert(61);
        }
        self.interface.push(var);
        self.builtins.insert(which, var);
        var
    }

    /// Charge une composante d'une entrée intégrée.
    fn load_builtin_comp(&mut self, which: BuiltinIn, comp: Option<u32>) -> Val {
        let var = self.builtin(which);
        let (sc, n) = match which {
            BuiltinIn::LocalId | BuiltinIn::WorkgroupId => (Sc::U32, 3),
            BuiltinIn::FragCoord => (Sc::F32, 4),
            _ => (Sc::U32, 1),
        };
        let t = self.spv.ty(sc, n);
        let id = self.spv.emit(61, t, &[var]);
        let v = Val { id, sc, n };
        match comp {
            Some(c) if n > 1 => self.spv.extract(v, c),
            _ => v,
        }
    }

    // ------------------------------------------------ masques de lanes
    /// Identifiant de lane dans le sous-groupe.
    pub fn lane_id(&mut self) -> Val {
        self.load_builtin_comp(BuiltinIn::SubgroupLane, None)
    }

    /// Bit de la lane courante dans le masque `(lo, hi)` (hi ignoré en wave32).
    pub fn lane_bit(&mut self, lo: Val, hi: Option<Val>) -> Val {
        let lane = self.lane_id();
        let m31 = self.cu(31);
        let sh = self.op2(199, Sc::U32, lane, m31);
        let word = match (self.wave, hi) {
            (WaveSize::W64, Some(hi)) => {
                let c32 = self.cu(32);
                let ge = self.op2(174, Sc::Bool, lane, c32);
                self.select(ge, hi, lo)
            }
            _ => lo,
        };
        let shifted = self.op2(194, Sc::U32, word, sh);
        let one = self.cu(1);
        let masked = self.op2(199, Sc::U32, shifted, one);
        let zero = self.cu(0);
        self.op2(171, Sc::Bool, masked, zero)
    }

    /// Bit de lane de EXEC (`None` si EXEC est constant : toutes les lanes sont actives).
    pub fn exec_bit(&mut self) -> Option<Val> {
        if self.exec_const {
            return None;
        }
        let lo_v = self.svar(126);
        let lo = self.load_u32(lo_v);
        let hi = (self.wave == WaveSize::W64).then(|| {
            let h = self.svar(127);
            self.load_u32(h)
        });
        Some(self.lane_bit(lo, hi))
    }

    /// `ballot(pred)` : retourne `(lo, hi)`.
    pub fn ballot(&mut self, pred: Val) -> (Val, Val) {
        self.spv.caps.insert(61);
        self.spv.caps.insert(64);
        let scope = self.cu(3);
        let t = self.spv.ty(Sc::U32, 4);
        let id = self.spv.emit(339, t, &[scope.id, pred.id]);
        let v = Val {
            id,
            sc: Sc::U32,
            n: 4,
        };
        let lo = self.spv.extract(v, 0);
        let hi = self.spv.extract(v, 1);
        (lo, hi)
    }

    /// Exécute `body` dans une région gardée par EXEC (sans garde si EXEC est constant).
    pub fn guarded<F: FnOnce(&mut Self) -> TResult<()>>(&mut self, body: F) -> TResult<()> {
        let Some(cond) = self.exec_bit() else {
            return body(self);
        };
        let then_l = self.spv.id();
        let merge_l = self.spv.id();
        self.spv.emit_void(247, &[merge_l, 0]);
        self.spv.terminate(250, &[cond.id, then_l, merge_l]);
        self.spv.begin_block(then_l);
        body(self)?;
        self.spv.branch(merge_l);
        self.spv.begin_block(merge_l);
        Ok(())
    }

    // ------------------------------------------------ lecture/écriture d'opérandes
    /// Valeur 32 bits (bits bruts) d'un opérande source, largeur 1.
    pub fn src_u32(&mut self, o: &Operand) -> TResult<Val> {
        Ok(match o.src {
            Src::Sgpr(n) => {
                let v = self.svar(usize::from(n));
                self.load_u32(v)
            }
            Src::Ttmp(n) => {
                let v = self.svar(108 + usize::from(n));
                self.load_u32(v)
            }
            Src::VccLo => self.sload(106),
            Src::VccHi => self.sload(107),
            Src::M0 => self.sload(124),
            Src::ExecLo => self.sload(126),
            Src::ExecHi => self.sload(127),
            Src::Null => self.cu(0),
            Src::InlineInt(v) => self.cu(v as u32),
            Src::InlineFloat(c) => self.cu(Src::inline_float_value(c).to_bits()),
            Src::Literal(v) => self.cu(v),
            Src::Scc => {
                let b = self.scc_get();
                let (one, zero) = (self.cu(1), self.cu(0));
                self.select(b, one, zero)
            }
            Src::Vccz | Src::Execz => {
                let (lo, hi) = if matches!(o.src, Src::Vccz) {
                    (106, 107)
                } else {
                    (126, 127)
                };
                let b = self.mask_is_zero(lo, hi);
                let (one, zero) = (self.cu(1), self.cu(0));
                self.select(b, one, zero)
            }
            Src::Vgpr(n) => {
                let v = self.vvar(usize::from(n));
                self.load_u32(v)
            }
            Src::LdsDirect | Src::Special(_) | Src::Off => {
                return Err(TranslateError::Unsupported(format!(
                    "opérande source {:?}",
                    o.src
                )));
            }
        })
    }

    pub fn sload(&mut self, code: usize) -> Val {
        let v = self.svar(code);
        self.load_u32(v)
    }

    /// `(lo, hi)` d'un opérande 64 bits.
    pub fn src_pair(&mut self, o: &Operand) -> TResult<(Val, Val)> {
        let (a, b) = match o.src {
            Src::Sgpr(n) => (usize::from(n), usize::from(n) + 1),
            Src::Ttmp(n) => (108 + usize::from(n), 109 + usize::from(n)),
            Src::VccLo | Src::VccHi => (106, 107),
            Src::ExecLo | Src::ExecHi => (126, 127),
            Src::Vgpr(n) => {
                let lo = self.vvar(usize::from(n));
                let hi = self.vvar(usize::from(n) + 1);
                return Ok((self.load_u32(lo), self.load_u32(hi)));
            }
            Src::InlineInt(v) => {
                let hi = if v < 0 { u32::MAX } else { 0 };
                return Ok((self.cu(v as u32), self.cu(hi)));
            }
            Src::Literal(v) => return Ok((self.cu(v), self.cu(0))),
            Src::Null => return Ok((self.cu(0), self.cu(0))),
            other => {
                return Err(TranslateError::Unsupported(format!(
                    "opérande 64 bits {other:?}"
                )));
            }
        };
        Ok((self.sload(a), self.sload(b)))
    }

    /// Source flottante 32 bits avec modificateurs `abs`/`neg`.
    pub fn src_f32(&mut self, o: &Operand) -> TResult<Val> {
        let u = self.src_u32(o)?;
        let mut f = self.spv.bitcast(u, Sc::F32);
        if o.abs {
            f = self.glsl(Sc::F32, 4, &[f]);
        }
        if o.neg {
            f = self.op1(127, Sc::F32, f);
        }
        Ok(f)
    }

    /// Code scalaire (0..128) d'un opérande destination, ou `None` pour `null`.
    pub fn scalar_code(o: &Operand) -> TResult<Option<usize>> {
        Ok(Some(match o.src {
            Src::Sgpr(n) => usize::from(n),
            Src::Ttmp(n) => 108 + usize::from(n),
            Src::VccLo => 106,
            Src::VccHi => 107,
            Src::M0 => 124,
            Src::ExecLo => 126,
            Src::ExecHi => 127,
            Src::Null => return Ok(None),
            other => {
                return Err(TranslateError::Unsupported(format!(
                    "destination scalaire {other:?}"
                )));
            }
        }))
    }

    /// Invalide les descripteurs résolus recouvrant `[first, first+width)`.
    pub fn clobber(&mut self, first: usize, width: usize) {
        let dead: Vec<u8> = self
            .res_map
            .iter()
            .filter(|(k, r)| {
                let k = **k;
                let (a, b) = (usize::from(k), usize::from(k) + usize::from(r.size));
                a < first + width && first < b
            })
            .map(|(&k, _)| k)
            .collect();
        for k in dead {
            self.res_map.remove(&k);
        }
    }

    /// Écrit un résultat 32 bits dans un opérande scalaire.
    pub fn write_s(&mut self, dst: &Operand, v: Val) -> TResult<()> {
        if let Some(code) = Self::scalar_code(dst)? {
            self.clobber(code, 1);
            let var = self.svar(code);
            self.store_u32(var, v);
        }
        Ok(())
    }

    /// Écrit une paire 64 bits dans un opérande scalaire.
    pub fn write_s_pair(&mut self, dst: &Operand, lo: Val, hi: Val) -> TResult<()> {
        if let Some(code) = Self::scalar_code(dst)? {
            self.clobber(code, 2);
            let (a, b) = (self.svar(code), self.svar(code + 1));
            self.store_u32(a, lo);
            self.store_u32(b, hi);
        }
        Ok(())
    }

    /// Écriture VALU 32 bits prédiquée par EXEC.
    pub fn write_v(&mut self, n: usize, v: Val) {
        let var = self.vvar(n);
        let new = self.spv.bitcast(v, Sc::U32);
        match self.exec_bit() {
            None => self.store_u32(var, new),
            Some(e) => {
                let old = self.load_u32(var);
                let sel = self.select(e, new, old);
                self.store_u32(var, sel);
            }
        }
    }

    /// Écrit le résultat d'un opérande destination vectoriel (VGPR ou, pour `v_readlane`, SGPR).
    pub fn write_dst(&mut self, dst: &Operand, v: Val) -> TResult<()> {
        match dst.src {
            Src::Vgpr(n) => {
                self.write_v(usize::from(n), v);
                Ok(())
            }
            _ => self.write_s(dst, v),
        }
    }

    /// `lo == 0 && hi == 0` pour le masque stocké en `(lo_code, hi_code)`.
    pub fn mask_is_zero(&mut self, lo_code: usize, hi_code: usize) -> Val {
        let lo = self.sload(lo_code);
        let all = if self.wave == WaveSize::W64 {
            let hi = self.sload(hi_code);
            self.op2(197, Sc::U32, lo, hi)
        } else {
            lo
        };
        let z = self.cu(0);
        self.op2(170, Sc::Bool, all, z)
    }

    /// Bit de lane d'un opérande masque (VCC, EXEC, paire de SGPR, constante 0/-1).
    pub fn mask_lane(&mut self, o: &Operand) -> TResult<Val> {
        match o.src {
            Src::InlineInt(0) => Ok(Val {
                id: self.spv.c_bool(false),
                sc: Sc::Bool,
                n: 1,
            }),
            Src::InlineInt(-1) => Ok(Val {
                id: self.spv.c_bool(true),
                sc: Sc::Bool,
                n: 1,
            }),
            _ => {
                let code = match o.src {
                    Src::Sgpr(n) => usize::from(n),
                    Src::Ttmp(n) => 108 + usize::from(n),
                    Src::VccLo => 106,
                    Src::ExecLo => 126,
                    other => {
                        return Err(TranslateError::Unsupported(format!(
                            "masque de lanes {other:?}"
                        )));
                    }
                };
                let lo = self.sload(code);
                let hi = (self.wave == WaveSize::W64).then(|| self.sload(code + 1));
                Ok(self.lane_bit(lo, hi))
            }
        }
    }

    /// Écrit `ballot(bit && exec)` dans un opérande masque.
    pub fn write_mask(&mut self, dst: &Operand, bit: Val) -> TResult<()> {
        let bit = match self.exec_bit() {
            Some(e) => self.bool_and(bit, e),
            None => bit,
        };
        let (lo, hi) = self.ballot(bit);
        let Some(code) = Self::scalar_code(dst)? else {
            return Ok(());
        };
        self.clobber(code, usize::from(self.wave.mask_dwords()));
        let a = self.svar(code);
        self.store_u32(a, lo);
        if self.wave == WaveSize::W64 {
            let b = self.svar(code + 1);
            self.store_u32(b, hi);
        }
        Ok(())
    }

    // ------------------------------------------------ ressources
    pub fn storage_buffer(&mut self, set: u32, binding: u32) -> u32 {
        if let Some(&v) = self.buffers.get(&(set, binding)) {
            return v;
        }
        let u = self.spv.ty(Sc::U32, 1);
        let ra = self.spv.runtime_array(u);
        let st = self.spv.structure(&[ra]);
        if !self.ra_decorated {
            self.spv.decorate(ra, 6, &[4]);
            self.spv.member_decorate(st, 0, 35, &[0]);
            self.spv.decorate(st, 2, &[]);
            self.ra_decorated = true;
        }
        let pt = self.spv.ptr(12, st);
        let var = self.spv.global_var(pt, 12, None);
        self.spv.decorate(var, 34, &[set]);
        self.spv.decorate(var, 33, &[binding]);
        self.buffers.insert((set, binding), var);
        self.info.bindings.push(BindingUse {
            set,
            binding,
            kind: "storage_buffer",
        });
        var
    }

    /// Pointeur vers le dword `idx` d'un buffer.
    pub fn buffer_word_ptr(&mut self, set: u32, binding: u32, idx: Val) -> u32 {
        let var = self.storage_buffer(set, binding);
        let zero = self.spv.c_u32(0);
        let u = self.spv.ty(Sc::U32, 1);
        let pt = self.spv.ptr(12, u);
        self.spv.emit(65, pt, &[var, zero, idx.id])
    }

    fn user_data_word(&mut self, i: u32) -> TResult<Val> {
        let Some((set, binding)) = self.cfg.user_data_binding else {
            return Err(TranslateError::Config(
                "SgprInit::UserData sans user_data_binding".into(),
            ));
        };
        self.user_data.get_or_insert(0);
        let idx = self.cu(i);
        let ptr = self.buffer_word_ptr(set, binding, idx);
        Ok(self.load_ptr_u32(ptr))
    }

    pub fn load_ptr_u32(&mut self, ptr: u32) -> Val {
        let t = self.spv.ty(Sc::U32, 1);
        let id = self.spv.emit(61, t, &[ptr]);
        Val {
            id,
            sc: Sc::U32,
            n: 1,
        }
    }

    // ------------------------------------------------ prologue
    fn prologue(&mut self) -> TResult<()> {
        let cfg = self.cfg;
        for &(n, init) in &cfg.sgprs {
            if usize::from(n) >= 106 {
                return Err(TranslateError::Config(format!(
                    "SGPR initial s{n} hors plage"
                )));
            }
            let v = match init {
                SgprInit::UserData(i) => self.user_data_word(i)?,
                SgprInit::WorkgroupId(a) => {
                    if cfg.stage != Stage::Compute || a > 2 {
                        return Err(TranslateError::Config(
                            "WorkgroupId : CS et axe 0..=2".into(),
                        ));
                    }
                    self.load_builtin_comp(BuiltinIn::WorkgroupId, Some(u32::from(a)))
                }
                SgprInit::Constant(c) => self.cu(c),
            };
            let var = self.svar(usize::from(n));
            self.store_u32(var, v);
        }
        for &(n, init) in &cfg.vgprs {
            let v = match init {
                VgprInit::VertexId => self.load_builtin_comp(BuiltinIn::VertexIndex, None),
                VgprInit::InstanceId => self.load_builtin_comp(BuiltinIn::InstanceIndex, None),
                VgprInit::LocalInvocationId(a) => {
                    self.load_builtin_comp(BuiltinIn::LocalId, Some(u32::from(a.min(2))))
                }
                VgprInit::FragCoord(a) => {
                    self.load_builtin_comp(BuiltinIn::FragCoord, Some(u32::from(a & 3)))
                }
                VgprInit::Constant(c) => self.cu(c),
            };
            let var = self.vvar(usize::from(n));
            self.store_u32(var, v);
        }
        for r in &cfg.resources {
            if let ResourceLoc::Sgprs(first) = r.loc {
                self.res_map.insert(first, res_ref(r));
            }
        }
        if !self.exec_const {
            // EXEC initial = lanes présentes dans le sous-groupe.
            let t = Val {
                id: self.spv.c_bool(true),
                sc: Sc::Bool,
                n: 1,
            };
            let (lo, hi) = self.ballot(t);
            let (a, b) = (self.svar(126), self.svar(127));
            self.store_u32(a, lo);
            if self.wave == WaveSize::W64 {
                self.store_u32(b, hi);
            }
        }
        Ok(())
    }

    // ------------------------------------------------ flot de contrôle
    fn cond_value(&mut self, c: BranchCond) -> Val {
        match c {
            BranchCond::Scc0 => {
                let s = self.scc_get();
                self.bool_not(s)
            }
            BranchCond::Scc1 => self.scc_get(),
            BranchCond::Vccz => self.mask_is_zero(106, 107),
            BranchCond::Execz => self.mask_is_zero(126, 127),
            BranchCond::Vccnz => {
                let z = self.mask_is_zero(106, 107);
                self.bool_not(z)
            }
            BranchCond::Execnz => {
                let z = self.mask_is_zero(126, 127);
                self.bool_not(z)
            }
        }
    }

    fn emit_body(&mut self, insts: &[Decoded]) -> TResult<()> {
        for d in insts {
            self.translate_inst(&d.inst)?;
        }
        Ok(())
    }

    fn emit_program(&mut self, cfg: &Cfg) -> TResult<()> {
        if cfg.is_straight_line() {
            return self.emit_body(cfg.body(0));
        }
        let n = cfg.blocks.len();
        let end_id = n as u32;
        let pc = self.new_function_var(Sc::U32);
        let labels: Vec<u32> = (0..n).map(|_| self.spv.id()).collect();
        let (header, dispatch, dmerge, cont, lmerge) = (
            self.spv.id(),
            self.spv.id(),
            self.spv.id(),
            self.spv.id(),
            self.spv.id(),
        );
        self.spv.begin_block(header);
        self.spv.emit_void(246, &[lmerge, cont, 0]);
        self.spv.branch(dispatch);
        self.spv.begin_block(dispatch);
        let cur = self.load_u32(pc);
        self.spv.emit_void(247, &[dmerge, 0]);
        let mut sw = vec![cur.id, dmerge];
        for (i, l) in labels.iter().enumerate() {
            sw.push(i as u32);
            sw.push(*l);
        }
        self.spv.terminate(251, &sw);
        for b in 0..n {
            self.spv.begin_block(labels[b]);
            self.emit_body(cfg.body(b))?;
            let next = match cfg.blocks[b].term {
                Terminator::Fallthrough(t) | Terminator::Branch(t) => self.cu(t as u32),
                Terminator::End => self.cu(end_id),
                Terminator::Cond { cond, taken, fall } => {
                    let c = self.cond_value(cond);
                    let (t, f) = (self.cu(taken as u32), self.cu(fall as u32));
                    self.select(c, t, f)
                }
            };
            self.store_u32(pc, next);
            self.spv.branch(dmerge);
        }
        self.spv.begin_block(dmerge);
        self.spv.branch(cont);
        self.spv.begin_block(cont);
        let p = self.load_u32(pc);
        let endc = self.cu(end_id);
        let done = self.op2(170, Sc::Bool, p, endc);
        self.spv.terminate(250, &[done.id, lmerge, header]);
        self.spv.begin_block(lmerge);
        Ok(())
    }

    /// `if (!exec) OpKill` en fin de PS, puis `OpReturn`.
    fn epilogue(&mut self) {
        if self.cfg.stage == Stage::Fragment
            && !self.exec_const
            && let Some(e) = self.exec_bit()
        {
            let dead = self.bool_not(e);
            let (kill, after) = (self.spv.id(), self.spv.id());
            self.spv.emit_void(247, &[after, 0]);
            self.spv.terminate(250, &[dead.id, kill, after]);
            self.spv.begin_block(kill);
            self.spv.terminate(252, &[]);
            self.spv.begin_block(after);
        }
    }
}

pub(crate) fn res_ref(r: &Resource) -> ResRef {
    ResRef {
        kind: r.kind,
        set: r.set,
        binding: r.binding,
        size: match r.kind {
            ResourceKind::Buffer { .. } | ResourceKind::Sampler => 4,
            ResourceKind::Image => 8,
        },
    }
}

/// Traduit un programme RDNA 2 en module SPIR-V.
pub fn translate_program(code: &[u32], cfg: &TranslateConfig) -> TResult<TranslatedProgram> {
    let decoded = decode_stream(code, cfg.wave)?;
    let count = decoded.len();
    let graph = build_cfg(decoded)?;
    let mut tr = Tr::new(cfg);
    tr.exec_const = !graph.insts.iter().any(|d| writes_exec(&d.inst));
    tr.info.instructions = count;
    tr.info.blocks = graph.blocks.len();
    tr.info.uses_exec_mask = !tr.exec_const;

    let main = tr.spv.id();
    tr.spv.begin_function(main);
    tr.entry_pos = tr.spv.cur_len();
    tr.prologue()?;
    tr.emit_program(&graph)?;
    tr.epilogue();
    tr.spv.terminate(253, &[]);
    tr.spv.end_function();

    let (model, modes): (u32, Vec<Vec<u32>>) = match cfg.stage {
        Stage::Vertex => (0, vec![]),
        Stage::Fragment => {
            let mut m = vec![vec![7]];
            if tr.depth.is_some() {
                m.push(vec![12]);
            }
            (4, m)
        }
        Stage::Compute => (
            5,
            vec![vec![
                17,
                cfg.local_size[0],
                cfg.local_size[1],
                cfg.local_size[2],
            ]],
        ),
    };
    if cfg.stage == Stage::Vertex && tr.position.is_none() {
        return Err(TranslateError::Config("VS sans export `pos0`".into()));
    }
    let iface = tr.interface.clone();
    tr.spv.set_interface(&iface);
    tr.info.vs_params = tr.params.keys().copied().collect();
    tr.info.ps_attrs = tr.attrs.keys().copied().collect();
    tr.info.mrts = tr.mrts.keys().copied().collect();
    tr.info.writes_depth = tr.depth.is_some();
    let words = tr.spv.assemble(model, main, &modes);
    Ok(TranslatedProgram {
        words,
        info: tr.info,
    })
}
