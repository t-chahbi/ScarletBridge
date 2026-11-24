//! Générateur SPIR-V : déclarations, interface, opérandes, flux de contrôle structuré.
//!
//! Modèle : les registres DXBC (`r#`, `o#`, `x#[]`) sont des variables `Private` de type `uvec4`
//! (les bits sont conservés tels quels, les opérations typées font des `OpBitcast`). Le corps du
//! shader est une fonction `void` (plus une fonction par sous-routine `label`) appelée par
//! `main`, qui recopie ensuite les registres de sortie vers les variables `Output` SPIR-V.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::decode::{Index, Inst, Operand, decode_program};
use super::opcodes::*;
use super::spv::{Sc, Spv, Val};
use super::{
    CBUFFER_VEC4_COUNT, MAX_BINDING_SLOTS, SAMPLER_BINDING_BASE, SRV_BUFFER_BINDING_BASE,
    STORAGE_BINDING_BASE, ShaderTranslateError, SignatureElement, TEXTURE_BINDING_BASE,
};
use crate::shader::ShaderStage;

pub type R<T> = Result<T, ShaderTranslateError>;

pub fn invalid<T>(msg: impl Into<String>) -> R<T> {
    Err(ShaderTranslateError::InvalidOperand(msg.into()))
}
pub fn unsupported<T>(msg: impl Into<String>) -> R<T> {
    Err(ShaderTranslateError::Unsupported(msg.into()))
}

pub fn mask_comps(mask: u8) -> Vec<u32> {
    (0..4).filter(|c| mask & (1 << c) != 0).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResDim {
    Buffer,
    T1D,
    T1DArray,
    T2D,
    T2DArray,
    T2DMS,
    T2DMSArray,
    T3D,
    Cube,
    CubeArray,
}

impl ResDim {
    pub fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            1 => Self::Buffer,
            2 => Self::T1D,
            3 => Self::T2D,
            4 => Self::T2DMS,
            5 => Self::T3D,
            6 => Self::Cube,
            7 => Self::T1DArray,
            8 => Self::T2DArray,
            9 => Self::T2DMSArray,
            10 => Self::CubeArray,
            _ => return None,
        })
    }
    pub fn spv_dim(self) -> u32 {
        match self {
            Self::T1D | Self::T1DArray => 0,
            Self::T2D | Self::T2DArray | Self::T2DMS | Self::T2DMSArray => 1,
            Self::T3D => 2,
            Self::Cube | Self::CubeArray => 3,
            Self::Buffer => 5,
        }
    }
    pub fn arrayed(self) -> bool {
        matches!(
            self,
            Self::T1DArray | Self::T2DArray | Self::T2DMSArray | Self::CubeArray
        )
    }
    pub fn ms(self) -> bool {
        matches!(self, Self::T2DMS | Self::T2DMSArray)
    }
    /// Composantes de coordonnée flottante pour `sample*` / entières pour `ld` (sans mip).
    pub fn coord_comps(self) -> u32 {
        match self {
            Self::Buffer | Self::T1D => 1,
            Self::T1DArray | Self::T2D | Self::T2DMS => 2,
            Self::T2DArray | Self::T2DMSArray | Self::T3D | Self::Cube => 3,
            Self::CubeArray => 4,
        }
    }
    /// Composantes retournées par `OpImageQuerySize*` (dimensions + couches).
    pub fn size_comps(self) -> u32 {
        match self {
            Self::Buffer | Self::T1D => 1,
            Self::T1DArray | Self::T2D | Self::T2DMS | Self::Cube => 2,
            Self::T2DArray | Self::T2DMSArray | Self::T3D | Self::CubeArray => 3,
        }
    }
    /// Composantes d'offset de texel (`aoffimmi`).
    pub fn offset_comps(self) -> u32 {
        match self {
            Self::Buffer | Self::T1D | Self::T1DArray => 1,
            Self::T2D | Self::T2DArray | Self::T2DMS | Self::T2DMSArray => 2,
            Self::T3D => 3,
            Self::Cube | Self::CubeArray => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufKind {
    Typed,
    Raw,
    Structured(u32),
}

#[derive(Debug, Clone, Copy)]
pub struct ResDecl {
    pub dim: ResDim,
    pub sc: Sc,
    pub kind: BufKind,
}

#[derive(Debug, Clone, Copy)]
struct InDecl {
    reg: u32,
    name: u32,
    interp: u32,
}

#[derive(Default)]
pub struct Decls {
    pub temps: u32,
    pub indexable: HashMap<u32, u32>,
    pub tgroup: [u32; 3],
    pub cb_size: HashMap<u32, u32>,
    pub res: HashMap<u32, ResDecl>,
    pub uav: HashMap<u32, ResDecl>,
    pub samp_cmp: HashMap<u32, bool>,
    pub tgsm: HashMap<u32, (u32, u32)>, // (octets, stride)
    pub icb: Vec<u32>,
    pub cmp_tex: BTreeSet<u32>,
    pub atomic_uav: BTreeSet<u32>,
    pub depth_ops: BTreeSet<u32>,
    pub uses_coverage_out: bool,
    pub early_z: bool,
    ins: Vec<InDecl>,
}

fn ret_sc(code: u32) -> Sc {
    match code {
        3 => Sc::I32,
        4 => Sc::U32,
        _ => Sc::F32,
    }
}

impl Decls {
    fn scan(insts: &[Inst], stage: ShaderStage) -> R<Self> {
        let mut d = Decls {
            tgroup: [1, 1, 1],
            ..Default::default()
        };
        for i in insts {
            let slot = |k: usize| i.operands.get(k).map(|o| o.idx[0].imm).unwrap_or(0);
            match i.opcode {
                OP_DCL_TEMPS => d.temps = d.temps.max(i.extra.first().copied().unwrap_or(0)),
                OP_DCL_INDEXABLE_TEMP if i.extra.len() >= 2 => {
                    d.indexable.insert(i.extra[0], i.extra[1]);
                }
                OP_DCL_THREAD_GROUP if i.extra.len() >= 3 => {
                    d.tgroup = [i.extra[0].max(1), i.extra[1].max(1), i.extra[2].max(1)];
                }
                OP_DCL_CONSTANT_BUFFER => {
                    if let Some(o) = i.operands.first() {
                        // SM5.0 : cb#[taille] ; SM5.1 : cb#[id][lower][upper] (taille en mot extra).
                        let size = if o.dims >= 3 {
                            i.extra.first().copied().unwrap_or(0)
                        } else {
                            o.idx[1].imm
                        };
                        d.cb_size.insert(o.idx[0].imm, size);
                    }
                }
                OP_DCL_RESOURCE => {
                    let dim = ResDim::from_code((i.token >> 11) & 0x1F).ok_or_else(|| {
                        ShaderTranslateError::Unsupported("dcl_resource: dimension inconnue".into())
                    })?;
                    let sc = ret_sc(i.extra.first().copied().unwrap_or(5) & 0xF);
                    d.res.insert(
                        slot(0),
                        ResDecl {
                            dim,
                            sc,
                            kind: BufKind::Typed,
                        },
                    );
                }
                OP_DCL_RESOURCE_RAW => {
                    d.res.insert(
                        slot(0),
                        ResDecl {
                            dim: ResDim::Buffer,
                            sc: Sc::U32,
                            kind: BufKind::Raw,
                        },
                    );
                }
                OP_DCL_RESOURCE_STRUCTURED => {
                    let stride = i.extra.first().copied().unwrap_or(4);
                    d.res.insert(
                        slot(0),
                        ResDecl {
                            dim: ResDim::Buffer,
                            sc: Sc::U32,
                            kind: BufKind::Structured(stride),
                        },
                    );
                }
                OP_DCL_UAV_TYPED => {
                    let dim = ResDim::from_code((i.token >> 11) & 0x1F).ok_or_else(|| {
                        ShaderTranslateError::Unsupported(
                            "dcl_uav_typed: dimension inconnue".into(),
                        )
                    })?;
                    let sc = ret_sc(i.extra.first().copied().unwrap_or(5) & 0xF);
                    d.uav.insert(
                        slot(0),
                        ResDecl {
                            dim,
                            sc,
                            kind: BufKind::Typed,
                        },
                    );
                }
                OP_DCL_UAV_RAW => {
                    d.uav.insert(
                        slot(0),
                        ResDecl {
                            dim: ResDim::Buffer,
                            sc: Sc::U32,
                            kind: BufKind::Raw,
                        },
                    );
                }
                OP_DCL_UAV_STRUCTURED => {
                    let stride = i.extra.first().copied().unwrap_or(4);
                    d.uav.insert(
                        slot(0),
                        ResDecl {
                            dim: ResDim::Buffer,
                            sc: Sc::U32,
                            kind: BufKind::Structured(stride),
                        },
                    );
                }
                OP_DCL_SAMPLER => {
                    d.samp_cmp.insert(slot(0), (i.token >> 11) & 0xF == 1);
                }
                OP_DCL_TGSM_RAW => {
                    d.tgsm
                        .insert(slot(0), (i.extra.first().copied().unwrap_or(0), 0));
                }
                OP_DCL_TGSM_STRUCTURED if i.extra.len() >= 2 => {
                    d.tgsm
                        .insert(slot(0), (i.extra[0].saturating_mul(i.extra[1]), i.extra[0]));
                }
                OP_DCL_GLOBAL_FLAGS => d.early_z = i.token & 0x2000 != 0,
                OP_CUSTOMDATA => {
                    if i.extra.first().copied() == Some(3) {
                        d.icb = i.custom.get(1..).map(<[u32]>::to_vec).unwrap_or_default();
                    }
                }
                OP_DCL_INPUT | OP_DCL_INPUT_SGV | OP_DCL_INPUT_SIV | OP_DCL_INPUT_PS
                | OP_DCL_INPUT_PS_SGV | OP_DCL_INPUT_PS_SIV => {
                    if let Some(o) = i.operands.first()
                        && o.ty == OPERAND_INPUT
                    {
                        let interp = if matches!(
                            i.opcode,
                            OP_DCL_INPUT_PS | OP_DCL_INPUT_PS_SIV | OP_DCL_INPUT_PS_SGV
                        ) {
                            (i.token >> 11) & 0xF
                        } else {
                            0
                        };
                        let name = if matches!(
                            i.opcode,
                            OP_DCL_INPUT_SGV
                                | OP_DCL_INPUT_SIV
                                | OP_DCL_INPUT_PS_SGV
                                | OP_DCL_INPUT_PS_SIV
                        ) {
                            i.extra.first().copied().unwrap_or(0) & 0xFFFF
                        } else {
                            0
                        };
                        d.ins.push(InDecl {
                            reg: o.idx[0].imm,
                            name,
                            interp,
                        });
                    }
                }
                _ => {}
            }
            // Utilisation des textures avec comparaison, des UAV avec atomiques, des sorties spéciales.
            match i.opcode {
                OP_SAMPLE_C | OP_SAMPLE_C_LZ | OP_GATHER4_C | OP_GATHER4_PO_C => {
                    let r = if i.opcode == OP_GATHER4_PO_C { 3 } else { 2 };
                    if let Some(o) = i.operands.get(r) {
                        d.cmp_tex.insert(o.idx[0].imm);
                    }
                }
                OP_ATOMIC_AND..=OP_ATOMIC_UMIN | OP_IMM_ATOMIC_IADD..=OP_IMM_ATOMIC_UMIN => {
                    let k = if i.opcode >= OP_IMM_ATOMIC_IADD { 1 } else { 0 };
                    if let Some(o) = i.operands.get(k)
                        && o.ty == OPERAND_UNORDERED_ACCESS_VIEW
                    {
                        d.atomic_uav.insert(o.idx[0].imm);
                    }
                }
                _ => {}
            }
            if stage == ShaderStage::Pixel {
                for o in i.operands.iter().take(1) {
                    if matches!(
                        o.ty,
                        OPERAND_OUTPUT_DEPTH
                            | OPERAND_OUTPUT_DEPTH_GREATER_EQUAL
                            | OPERAND_OUTPUT_DEPTH_LESS_EQUAL
                    ) {
                        d.depth_ops.insert(o.ty);
                    }
                    if o.ty == OPERAND_OUTPUT_COVERAGE_MASK {
                        d.uses_coverage_out = true;
                    }
                }
            }
        }
        Ok(d)
    }
}

#[derive(Clone, Copy)]
enum InKind {
    Loc,
    FragCoord,
    VertexIndex,
    InstanceIndex,
    FrontFacing,
    PrimitiveId,
    SampleId,
}

#[derive(Clone, Copy)]
struct InVar {
    var: u32,
    kind: InKind,
    sc: Sc,
}

#[derive(Clone, Copy)]
enum OutTarget {
    Loc {
        var: u32,
        sc: Sc,
    },
    Position {
        var: u32,
    },
    Layer {
        var: u32,
    },
    ViewportIndex {
        var: u32,
    },
    /// Géré par la table clip/cull.
    ClipCull,
}

struct OutVar {
    private: u32,
    target: OutTarget,
}

#[derive(Clone, Copy)]
enum Cf {
    If {
        merge: u32,
        else_lbl: u32,
        has_else: bool,
    },
    Loop {
        merge: u32,
        cont: u32,
    },
    Switch {
        merge: u32,
        default_lbl: u32,
    },
}

#[derive(Clone, Copy)]
pub struct TexVar {
    pub var: u32,
    pub img_ty: u32,
    pub dim: ResDim,
    pub sc: Sc,
}

#[derive(Clone, Copy)]
pub struct BufVar {
    pub var: u32,
    pub kind: BufKind,
}

pub struct Gen {
    pub s: Spv,
    pub stage: ShaderStage,
    pub d: Decls,
    pub temps: Vec<u32>,
    pub indexables: HashMap<u32, (u32, u32)>,
    icb: Option<(u32, u32)>,
    inputs: HashMap<u32, InVar>,
    outs: BTreeMap<u32, OutVar>,
    special_outs: BTreeMap<u32, u32>,
    clip: Vec<(u32, u32)>,
    cull: Vec<(u32, u32)>,
    pv_var: Option<u32>,
    depth_var: Option<u32>,
    mask_out_var: Option<u32>,
    sample_mask_in: Option<u32>,
    cs_vars: HashMap<u32, u32>,
    cf: Vec<Cf>,
    fn_labels: HashMap<u32, u32>,
    pub tex_vars: BTreeMap<u32, TexVar>,
    pub samp_vars: BTreeMap<u32, u32>,
    pub uav_img: BTreeMap<u32, TexVar>,
    pub uav_buf: BTreeMap<u32, BufVar>,
    pub srv_buf: BTreeMap<u32, BufVar>,
    pub tgsm_vars: BTreeMap<u32, (u32, u32)>,
    cbs: BTreeMap<u32, (u32, u32)>,
    pub body_fn: u32,
    loop_headers: Vec<u32>,
    case_labels: Vec<Vec<u32>>,
    case_pos: Vec<usize>,
    rta_done: bool,
}

pub struct Emitted {
    pub spirv: Vec<u32>,
    pub used_cbuffers: Vec<u32>,
    pub used_textures: Vec<u32>,
    pub used_samplers: Vec<u32>,
    pub used_storage_buffers: Vec<u32>,
    pub used_srv_buffers: Vec<u32>,
    pub thread_group_size: [u32; 3],
}

fn sc_of_comp(t: u32) -> Sc {
    match t {
        1 => Sc::U32,
        2 => Sc::I32,
        _ => Sc::F32,
    }
}

pub fn emit_spirv(
    stage: ShaderStage,
    inputs: &[SignatureElement],
    outputs: &[SignatureElement],
    words: &[u32],
) -> R<Emitted> {
    if !matches!(
        stage,
        ShaderStage::Vertex | ShaderStage::Pixel | ShaderStage::Compute
    ) {
        return Err(ShaderTranslateError::UnsupportedStage(stage));
    }
    let insts = decode_program(words)?;
    let d = Decls::scan(&insts, stage)?;
    let mut g = Gen {
        s: Spv::new(),
        stage,
        d,
        temps: Vec::new(),
        indexables: HashMap::new(),
        icb: None,
        inputs: HashMap::new(),
        outs: BTreeMap::new(),
        special_outs: BTreeMap::new(),
        clip: Vec::new(),
        cull: Vec::new(),
        pv_var: None,
        depth_var: None,
        mask_out_var: None,
        sample_mask_in: None,
        cs_vars: HashMap::new(),
        cf: Vec::new(),
        fn_labels: HashMap::new(),
        tex_vars: BTreeMap::new(),
        samp_vars: BTreeMap::new(),
        uav_img: BTreeMap::new(),
        uav_buf: BTreeMap::new(),
        srv_buf: BTreeMap::new(),
        tgsm_vars: BTreeMap::new(),
        cbs: BTreeMap::new(),
        body_fn: 0,
        loop_headers: Vec::new(),
        case_labels: Vec::new(),
        case_pos: Vec::new(),
        rta_done: false,
    };
    g.setup_registers()?;
    g.setup_inputs(inputs)?;
    g.setup_outputs(outputs)?;
    g.run(&insts)?;
    g.finish()
}

impl Gen {
    // ---------------------------------------------------------------- types utilitaires
    pub fn pty(&mut self, class: u32, sc: Sc, n: u32) -> u32 {
        let t = self.s.ty(sc, n);
        self.s.ptr(class, t)
    }

    fn private_uvec4(&mut self, init_zero: bool) -> u32 {
        let t = self.s.ty(Sc::U32, 4);
        let p = self.s.ptr(6, t);
        let init = if init_zero {
            Some(self.s.c_null(t))
        } else {
            None
        };
        self.s.global_var(p, 6, init)
    }

    fn setup_registers(&mut self) -> R<()> {
        if self.d.temps > 4096 {
            return unsupported(format!("dcl_temps {} > 4096", self.d.temps));
        }
        for _ in 0..self.d.temps {
            let v = self.private_uvec4(true);
            self.temps.push(v);
        }
        let ids: Vec<(u32, u32)> = self.d.indexable.iter().map(|(&k, &v)| (k, v)).collect();
        for (reg, count) in ids {
            let v = self.private_array(count.max(1), None);
            self.indexables.insert(reg, (v, count.max(1)));
        }
        if !self.d.icb.is_empty() {
            let count = (self.d.icb.len() / 4) as u32;
            if count > 0 {
                let u4 = self.s.ty(Sc::U32, 4);
                let icb = std::mem::take(&mut self.d.icb);
                let elems: Vec<u32> = icb
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| self.s.c_vec(Sc::U32, c).id)
                    .collect();
                self.d.icb = icb;
                let len = self.s.c_u32(count);
                let arr = self.s.array(u4, len);
                let init = self.s.c_composite(arr, &elems);
                let ptr = self.s.ptr(6, arr);
                let v = self.s.global_var(ptr, 6, Some(init));
                self.icb = Some((v, count));
            }
        }
        Ok(())
    }

    fn private_array(&mut self, count: u32, _init: Option<u32>) -> u32 {
        let u4 = self.s.ty(Sc::U32, 4);
        let len = self.s.c_u32(count);
        let arr = self.s.array(u4, len);
        let ptr = self.s.ptr(6, arr);
        let init = self.s.c_null(arr);
        self.s.global_var(ptr, 6, Some(init))
    }

    // ---------------------------------------------------------------- interface
    fn in_kind(&self, sv: u32, name: &str) -> InKind {
        let n = name.to_ascii_uppercase();
        match self.stage {
            ShaderStage::Vertex => {
                if sv == SV_VERTEX_ID || n == "SV_VERTEXID" {
                    InKind::VertexIndex
                } else if sv == SV_INSTANCE_ID || n == "SV_INSTANCEID" {
                    InKind::InstanceIndex
                } else {
                    InKind::Loc
                }
            }
            ShaderStage::Pixel => {
                if sv == SV_POSITION || n == "SV_POSITION" {
                    InKind::FragCoord
                } else if sv == SV_IS_FRONT_FACE || n == "SV_ISFRONTFACE" {
                    InKind::FrontFacing
                } else if sv == SV_PRIMITIVE_ID || n == "SV_PRIMITIVEID" {
                    InKind::PrimitiveId
                } else if sv == SV_SAMPLE_INDEX || n == "SV_SAMPLEINDEX" {
                    InKind::SampleId
                } else {
                    InKind::Loc
                }
            }
            _ => InKind::Loc,
        }
    }

    fn setup_inputs(&mut self, sig: &[SignatureElement]) -> R<()> {
        if self.stage == ShaderStage::Compute {
            return Ok(());
        }
        // Registres dans l'ordre de première apparition (signature, puis déclarations).
        let mut order: Vec<u32> = Vec::new();
        let mut info: HashMap<u32, (InKind, Sc, u32)> = HashMap::new(); // kind, type, interp
        for e in sig {
            if e.register == u32::MAX || info.contains_key(&e.register) {
                continue;
            }
            let kind = self.in_kind(e.system_value_type, &e.semantic_name);
            order.push(e.register);
            info.insert(e.register, (kind, sc_of_comp(e.component_type), 2));
        }
        let decl_ins: Vec<InDecl> = self.d.ins.clone();
        for dcl in decl_ins {
            if dcl.name != 0 {
                let kind = self.in_kind(dcl.name, "");
                if let Some(x) = info.get_mut(&dcl.reg) {
                    if !matches!(kind, InKind::Loc) {
                        x.0 = kind;
                    }
                } else {
                    order.push(dcl.reg);
                    info.insert(dcl.reg, (kind, Sc::F32, dcl.interp));
                }
            } else if let std::collections::hash_map::Entry::Vacant(e) = info.entry(dcl.reg) {
                order.push(dcl.reg);
                e.insert((InKind::Loc, Sc::F32, dcl.interp));
            }
            if let Some(x) = info.get_mut(&dcl.reg)
                && dcl.interp != 0
            {
                x.2 = dcl.interp;
            }
        }
        let mut loc = 0u32;
        for reg in order {
            let (kind, sc, interp) = info[&reg];
            let (var, vsc) = match kind {
                InKind::Loc | InKind::FragCoord => {
                    let vsc = if matches!(kind, InKind::FragCoord) {
                        Sc::F32
                    } else {
                        sc
                    };
                    let p = self.pty(1, vsc, 4);
                    let v = self.s.global_var(p, 1, None);
                    if matches!(kind, InKind::FragCoord) {
                        self.s.decorate(v, 11, &[15]);
                    } else {
                        self.s.decorate(v, 30, &[loc]);
                        loc += 1;
                        if self.stage == ShaderStage::Pixel {
                            if vsc != Sc::F32 || interp == 1 {
                                self.s.decorate(v, 14, &[]);
                            }
                            if matches!(interp, 4 | 5 | 7 | 9) {
                                self.s.decorate(v, 13, &[]);
                            }
                            if matches!(interp, 3 | 5) {
                                self.s.decorate(v, 16, &[]);
                            }
                            if matches!(interp, 6..=9) {
                                self.s.decorate(v, 17, &[]);
                                self.s.caps.insert(35);
                            }
                        }
                    }
                    (v, vsc)
                }
                InKind::VertexIndex
                | InKind::InstanceIndex
                | InKind::PrimitiveId
                | InKind::SampleId => {
                    let p = self.pty(1, Sc::U32, 1);
                    let v = self.s.global_var(p, 1, None);
                    let b = match kind {
                        InKind::VertexIndex => 42,
                        InKind::InstanceIndex => 43,
                        InKind::PrimitiveId => {
                            self.s.caps.insert(2);
                            7
                        }
                        _ => {
                            self.s.caps.insert(35);
                            18
                        }
                    };
                    self.s.decorate(v, 11, &[b]);
                    if matches!(kind, InKind::PrimitiveId | InKind::SampleId) {
                        self.s.decorate(v, 14, &[]);
                    }
                    (v, Sc::U32)
                }
                InKind::FrontFacing => {
                    let p = self.pty(1, Sc::Bool, 1);
                    let v = self.s.global_var(p, 1, None);
                    self.s.decorate(v, 11, &[17]);
                    (v, Sc::Bool)
                }
            };
            self.s.interface.push(var);
            self.inputs.insert(reg, InVar { var, kind, sc: vsc });
        }
        Ok(())
    }

    fn setup_outputs(&mut self, sig: &[SignatureElement]) -> R<()> {
        if self.stage == ShaderStage::Compute {
            return Ok(());
        }
        // Tables clip/cull (ordre : index sémantique, puis registre).
        if self.stage == ShaderStage::Vertex {
            let mut cl: Vec<&SignatureElement> = sig
                .iter()
                .filter(|e| e.system_value_type == SV_CLIP_DISTANCE)
                .collect();
            cl.sort_by_key(|e| (e.semantic_index, e.register));
            for e in cl {
                for c in mask_comps(e.mask) {
                    self.clip.push((e.register, c));
                }
            }
            let mut cu: Vec<&SignatureElement> = sig
                .iter()
                .filter(|e| e.system_value_type == SV_CULL_DISTANCE)
                .collect();
            cu.sort_by_key(|e| (e.semantic_index, e.register));
            for e in cu {
                for c in mask_comps(e.mask) {
                    self.cull.push((e.register, c));
                }
            }
            let f4 = self.s.ty(Sc::F32, 4);
            let mut members = vec![f4];
            if !self.clip.is_empty() {
                self.s.caps.insert(32);
                let f = self.s.ty(Sc::F32, 1);
                let len = self.s.c_u32(self.clip.len() as u32);
                members.push(self.s.array(f, len));
            }
            if !self.cull.is_empty() {
                self.s.caps.insert(33);
                let f = self.s.ty(Sc::F32, 1);
                let len = self.s.c_u32(self.cull.len() as u32);
                members.push(self.s.array(f, len));
            }
            let st = self.s.structure(&members);
            let ptr = self.s.ptr(3, st);
            let var = self.s.global_var(ptr, 3, None);
            self.s.decorate(st, 2, &[]);
            self.s.member_decorate(st, 0, 11, &[0]);
            let mut mi = 1;
            if !self.clip.is_empty() {
                self.s.member_decorate(st, mi, 11, &[3]);
                mi += 1;
            }
            if !self.cull.is_empty() {
                self.s.member_decorate(st, mi, 11, &[4]);
            }
            self.s.interface.push(var);
            self.pv_var = Some(var);
        }
        let mut loc = 0u32;
        for e in sig {
            if e.register == u32::MAX || self.outs.contains_key(&e.register) {
                continue;
            }
            let private = self.private_uvec4(true);
            let sv = e.system_value_type;
            let nm = e.semantic_name.to_ascii_uppercase();
            let target = match self.stage {
                ShaderStage::Vertex if sv == SV_POSITION || nm == "SV_POSITION" => {
                    OutTarget::Position {
                        var: self.pv_var.unwrap_or(0),
                    }
                }
                ShaderStage::Vertex if sv == SV_CLIP_DISTANCE || sv == SV_CULL_DISTANCE => {
                    OutTarget::ClipCull
                }
                ShaderStage::Vertex
                    if sv == SV_RENDER_TARGET_ARRAY_INDEX || sv == SV_VIEWPORT_ARRAY_INDEX =>
                {
                    self.s.caps.insert(5254);
                    self.s.exts.insert("SPV_EXT_shader_viewport_index_layer");
                    let p = self.pty(3, Sc::I32, 1);
                    let v = self.s.global_var(p, 3, None);
                    let layer = sv == SV_RENDER_TARGET_ARRAY_INDEX;
                    self.s.decorate(v, 11, &[if layer { 9 } else { 10 }]);
                    self.s.decorate(v, 14, &[]);
                    self.s.interface.push(v);
                    if layer {
                        OutTarget::Layer { var: v }
                    } else {
                        OutTarget::ViewportIndex { var: v }
                    }
                }
                ShaderStage::Pixel => {
                    let sc = sc_of_comp(e.component_type);
                    let p = self.pty(3, sc, 4);
                    let v = self.s.global_var(p, 3, None);
                    self.s.decorate(v, 30, &[e.register]);
                    self.s.interface.push(v);
                    OutTarget::Loc { var: v, sc }
                }
                _ => {
                    let sc = sc_of_comp(e.component_type);
                    let p = self.pty(3, sc, 4);
                    let v = self.s.global_var(p, 3, None);
                    self.s.decorate(v, 30, &[loc]);
                    loc += 1;
                    if sc != Sc::F32 {
                        self.s.decorate(v, 14, &[]);
                    }
                    self.s.interface.push(v);
                    OutTarget::Loc { var: v, sc }
                }
            };
            self.outs.insert(e.register, OutVar { private, target });
        }
        if self.stage == ShaderStage::Pixel {
            let ops: Vec<u32> = self.d.depth_ops.iter().copied().collect();
            if !ops.is_empty() {
                let p = self.pty(3, Sc::F32, 1);
                let v = self.s.global_var(p, 3, None);
                self.s.decorate(v, 11, &[22]);
                self.s.interface.push(v);
                self.depth_var = Some(v);
            }
            for o in ops {
                let pv = self.private_uvec4(true);
                self.special_outs.insert(o, pv);
            }
            if self.d.uses_coverage_out {
                let len = self.s.c_u32(1);
                let u = self.s.ty(Sc::U32, 1);
                let arr = self.s.array(u, len);
                let ptr = self.s.ptr(3, arr);
                let v = self.s.global_var(ptr, 3, None);
                self.s.decorate(v, 11, &[20]);
                self.s.interface.push(v);
                self.mask_out_var = Some(v);
                let pv = self.private_uvec4(true);
                self.special_outs.insert(OPERAND_OUTPUT_COVERAGE_MASK, pv);
            }
        }
        Ok(())
    }

    pub(super) fn input_ptr(&mut self, reg: u32) -> R<u32> {
        match self.inputs.get(&reg) {
            Some(i) if matches!(i.kind, InKind::Loc) => Ok(i.var),
            _ => invalid(format!("entrée v{reg} non interpolable")),
        }
    }

    // ---------------------------------------------------------------- opérandes
    pub fn idx_val(&mut self, ix: &Index) -> R<Val> {
        match &ix.rel {
            None => Ok(Val {
                id: self.s.c_u32(ix.imm),
                sc: Sc::U32,
                n: 1,
            }),
            Some(rel) => {
                let r = self.read(rel, &[0], Sc::U32)?;
                if ix.imm == 0 {
                    Ok(r)
                } else {
                    let c = self.s.c_u32(ix.imm);
                    Ok(self.s.val(128, Sc::U32, 1, &[r.id, c]))
                }
            }
        }
    }

    fn cs_builtin(&mut self, kind: u32) -> R<u32> {
        if let Some(&v) = self.cs_vars.get(&kind) {
            return Ok(v);
        }
        let (sc3, builtin) = match kind {
            OPERAND_INPUT_THREAD_ID => (3, 28),
            OPERAND_INPUT_THREAD_GROUP_ID => (3, 26),
            OPERAND_INPUT_THREAD_ID_IN_GROUP => (3, 27),
            OPERAND_INPUT_THREAD_ID_IN_GROUP_FLATTENED => (1, 29),
            _ => return invalid("builtin de calcul inconnu"),
        };
        let p = self.pty(1, Sc::U32, sc3);
        let v = self.s.global_var(p, 1, None);
        self.s.decorate(v, 11, &[builtin]);
        self.s.interface.push(v);
        self.cs_vars.insert(kind, v);
        Ok(v)
    }

    fn load(&mut self, ptr: u32, sc: Sc, n: u32) -> Val {
        self.s.val(61, sc, n, &[ptr])
    }

    fn widen_u4(&mut self, v: Val) -> Val {
        // Étend un scalaire/vecteur de bits en uvec4 (composantes manquantes à zéro).
        let v = self.s.bitcast(v, Sc::U32);
        if v.n == 4 {
            return v;
        }
        let z = self.s.c_u32(0);
        let mut comps = Vec::new();
        for i in 0..4 {
            if i < v.n {
                comps.push(self.s.extract(v, i).id);
            } else {
                comps.push(z);
            }
        }
        self.s.construct(Sc::U32, &comps)
    }

    /// Valeur brute (4 composantes) d'un opérande source non immédiat.
    fn raw(&mut self, op: &Operand) -> R<Val> {
        match op.ty {
            OPERAND_TEMP => {
                let i = op.idx[0].imm as usize;
                let Some(&v) = self.temps.get(i) else {
                    return invalid(format!("registre temporaire r{i} non déclaré"));
                };
                Ok(self.load(v, Sc::U32, 4))
            }
            OPERAND_OUTPUT => {
                let reg = op.idx[0].imm;
                let Some(o) = self.outs.get(&reg) else {
                    return invalid(format!("lecture de la sortie o{reg} non déclarée"));
                };
                let p = o.private;
                Ok(self.load(p, Sc::U32, 4))
            }
            OPERAND_OUTPUT_DEPTH
            | OPERAND_OUTPUT_DEPTH_GREATER_EQUAL
            | OPERAND_OUTPUT_DEPTH_LESS_EQUAL
            | OPERAND_OUTPUT_COVERAGE_MASK => {
                let Some(&p) = self.special_outs.get(&op.ty) else {
                    return invalid("sortie spéciale non déclarée");
                };
                Ok(self.load(p, Sc::U32, 4))
            }
            OPERAND_INDEXABLE_TEMP => {
                let reg = op.idx[0].imm;
                let Some(&(var, _)) = self.indexables.get(&reg) else {
                    return invalid(format!("x{reg} non déclaré"));
                };
                let i = self.idx_val(&op.idx[1])?;
                let p = self.pty(6, Sc::U32, 4);
                let ptr = self.s.emit(65, p, &[var, i.id]);
                Ok(self.load(ptr, Sc::U32, 4))
            }
            OPERAND_IMMEDIATE_CONSTANT_BUFFER => {
                let Some((var, count)) = self.icb else {
                    return invalid("icb[] sans dcl_immediateConstantBuffer");
                };
                let i = self.idx_val(&op.idx[0])?;
                let last = self.s.c_u32(count - 1);
                let i = self.s.glsl_op(Sc::U32, 1, 38, &[i.id, last]);
                let p = self.pty(6, Sc::U32, 4);
                let ptr = self.s.emit(65, p, &[var, i.id]);
                Ok(self.load(ptr, Sc::U32, 4))
            }
            OPERAND_CONSTANT_BUFFER => {
                if op.dims < 2 {
                    return invalid("cb# sans index de registre");
                }
                if op.idx[0].rel.is_some() {
                    return unsupported("cbuffer à indice dynamique (SM5.1)");
                }
                let (var, size) = self.cbuffer(op.idx[0].imm)?;
                let i = self.idx_val(&op.idx[1])?;
                let i = if op.idx[1].rel.is_some() {
                    let last = self.s.c_u32(size - 1);
                    self.s.glsl_op(Sc::U32, 1, 38, &[i.id, last])
                } else {
                    i
                };
                let z = self.s.c_u32(0);
                let p = self.pty(2, Sc::U32, 4);
                let ptr = self.s.emit(65, p, &[var, z, i.id]);
                Ok(self.load(ptr, Sc::U32, 4))
            }
            OPERAND_INPUT => {
                if op.dims > 1 {
                    return unsupported("entrée indexée (GS/HS/DS)");
                }
                if op.idx[0].rel.is_some() {
                    return unsupported("indexation relative des entrées");
                }
                let reg = op.idx[0].imm;
                let Some(iv) = self.inputs.get(&reg).copied() else {
                    return invalid(format!("entrée v{reg} absente de la signature"));
                };
                match iv.kind {
                    InKind::Loc | InKind::FragCoord => Ok(self.load(iv.var, iv.sc, 4)),
                    InKind::FrontFacing => {
                        let b = self.load(iv.var, Sc::Bool, 1);
                        let t = self.s.c_u32(u32::MAX);
                        let f = self.s.c_u32(0);
                        let v = self.s.val(169, Sc::U32, 1, &[b.id, t, f]);
                        Ok(self.widen_u4(v))
                    }
                    _ => {
                        let v = self.load(iv.var, Sc::U32, 1);
                        Ok(self.widen_u4(v))
                    }
                }
            }
            OPERAND_INPUT_THREAD_ID
            | OPERAND_INPUT_THREAD_GROUP_ID
            | OPERAND_INPUT_THREAD_ID_IN_GROUP => {
                let v = self.cs_builtin(op.ty)?;
                let u = self.load(v, Sc::U32, 3);
                Ok(self.widen_u4(u))
            }
            OPERAND_INPUT_THREAD_ID_IN_GROUP_FLATTENED => {
                let v = self.cs_builtin(op.ty)?;
                let u = self.load(v, Sc::U32, 1);
                Ok(self.widen_u4(u))
            }
            OPERAND_INPUT_PRIMITIVE_ID => {
                let v = self.prim_id_var();
                let u = self.load(v, Sc::U32, 1);
                Ok(self.widen_u4(u))
            }
            OPERAND_INPUT_COVERAGE_MASK => {
                let v = self.sample_mask_in_var();
                let z = self.s.c_u32(0);
                let p = self.pty(1, Sc::U32, 1);
                let ptr = self.s.emit(65, p, &[v, z]);
                let u = self.load(ptr, Sc::U32, 1);
                Ok(self.widen_u4(u))
            }
            t => invalid(format!("type d'opérande source 0x{t:X} non supporté")),
        }
    }

    fn prim_id_var(&mut self) -> u32 {
        if let Some(&v) = self.cs_vars.get(&OPERAND_INPUT_PRIMITIVE_ID) {
            return v;
        }
        self.s.caps.insert(2);
        let p = self.pty(1, Sc::U32, 1);
        let v = self.s.global_var(p, 1, None);
        self.s.decorate(v, 11, &[7]);
        self.s.decorate(v, 14, &[]);
        self.s.interface.push(v);
        self.cs_vars.insert(OPERAND_INPUT_PRIMITIVE_ID, v);
        v
    }

    fn sample_mask_in_var(&mut self) -> u32 {
        if let Some(v) = self.sample_mask_in {
            return v;
        }
        let len = self.s.c_u32(1);
        let u = self.s.ty(Sc::U32, 1);
        let arr = self.s.array(u, len);
        let ptr = self.s.ptr(1, arr);
        let v = self.s.global_var(ptr, 1, None);
        self.s.decorate(v, 11, &[20]);
        self.s.decorate(v, 14, &[]);
        self.s.interface.push(v);
        self.sample_mask_in = Some(v);
        v
    }

    /// Lit `op` pour les composantes de destination `comps`, converti en `want`, modificateurs appliqués.
    pub fn read(&mut self, op: &Operand, comps: &[u32], want: Sc) -> R<Val> {
        let v = if op.ty == OPERAND_IMMEDIATE32 {
            let bits: Vec<u32> = comps.iter().map(|&c| op.imm[(c & 3) as usize]).collect();
            self.s.c_vec(want, &bits)
        } else {
            let raw = self.raw(op)?;
            let swz: Vec<u32> = comps
                .iter()
                .map(|&c| op.swizzle[(c & 3) as usize])
                .collect();
            let p = self.s.pick(raw, &swz);
            self.s.bitcast(p, want)
        };
        Ok(self.modifiers(v, op.abs, op.neg))
    }

    pub fn modifiers(&mut self, mut v: Val, abs: bool, neg: bool) -> Val {
        if abs {
            v = match v.sc {
                Sc::F32 => self.s.glsl_op(Sc::F32, v.n, 4, &[v.id]),
                _ => self.s.glsl_op(v.sc, v.n, 5, &[v.id]),
            };
        }
        if neg {
            v = match v.sc {
                Sc::F32 => self.s.val(127, Sc::F32, v.n, &[v.id]),
                _ => self.s.val(126, v.sc, v.n, &[v.id]),
            };
        }
        v
    }

    /// Booléen scalaire « condition vraie » (`_nz` : valeur != 0, `_z` : valeur == 0).
    pub fn test(&mut self, op: &Operand, nonzero: bool) -> R<Val> {
        let v = self.read(op, &[0], Sc::U32)?;
        let z = self.s.c_u32(0);
        let code = if nonzero { 171 } else { 170 };
        Ok(self.s.val(code, Sc::Bool, 1, &[v.id, z]))
    }

    fn dst_ptr(&mut self, op: &Operand) -> R<Option<u32>> {
        match op.ty {
            OPERAND_NULL => Ok(None),
            OPERAND_TEMP => {
                let i = op.idx[0].imm as usize;
                match self.temps.get(i) {
                    Some(&v) => Ok(Some(v)),
                    None => invalid(format!("registre temporaire r{i} non déclaré")),
                }
            }
            OPERAND_OUTPUT => {
                let reg = op.idx[0].imm;
                match self.outs.get(&reg) {
                    Some(o) => Ok(Some(o.private)),
                    None => invalid(format!(
                        "écriture de la sortie o{reg} absente de la signature"
                    )),
                }
            }
            OPERAND_OUTPUT_DEPTH
            | OPERAND_OUTPUT_DEPTH_GREATER_EQUAL
            | OPERAND_OUTPUT_DEPTH_LESS_EQUAL
            | OPERAND_OUTPUT_COVERAGE_MASK => match self.special_outs.get(&op.ty) {
                Some(&p) => Ok(Some(p)),
                None => invalid("sortie spéciale non déclarée"),
            },
            OPERAND_INDEXABLE_TEMP => {
                let reg = op.idx[0].imm;
                let Some(&(var, _)) = self.indexables.get(&reg) else {
                    return invalid(format!("x{reg} non déclaré"));
                };
                let i = self.idx_val(&op.idx[1])?;
                let p = self.pty(6, Sc::U32, 4);
                Ok(Some(self.s.emit(65, p, &[var, i.id])))
            }
            t => invalid(format!("type d'opérande destination 0x{t:X} non supporté")),
        }
    }

    /// Écrit `v` (une composante par bit du masque, dans l'ordre) dans la destination.
    pub fn write(&mut self, dst: &Operand, v: Val, sat: bool) -> R<()> {
        let Some(ptr) = self.dst_ptr(dst)? else {
            return Ok(());
        };
        let comps = mask_comps(dst.mask);
        let n = comps.len() as u32;
        if n == 0 {
            return Ok(());
        }
        let mut v = if v.n == 1 && n > 1 {
            self.s.splat(v, n)
        } else {
            v
        };
        if sat && v.sc == Sc::F32 {
            let z = self.s.c_vec(Sc::F32, &vec![0.0f32.to_bits(); n as usize]);
            let o = self.s.c_vec(Sc::F32, &vec![1.0f32.to_bits(); n as usize]);
            v = self.s.glsl_op(Sc::F32, n, 81, &[v.id, z.id, o.id]);
        }
        let v = self.s.bitcast(v, Sc::U32);
        let u4 = self.s.ty(Sc::U32, 4);
        let merged = if n == 4 {
            v
        } else {
            let old = self.s.val(61, Sc::U32, 4, &[ptr]);
            let mut cur = old;
            for (k, &c) in comps.iter().enumerate() {
                let e = self.s.extract(v, k as u32);
                let id = self.s.emit(82, u4, &[e.id, cur.id, c]);
                cur = Val {
                    id,
                    sc: Sc::U32,
                    n: 4,
                };
            }
            cur
        };
        self.s.emit_void(62, &[ptr, merged.id]);
        Ok(())
    }

    // ---------------------------------------------------------------- ressources
    fn binding_slot(slot: u32, what: &str) -> R<u32> {
        if slot >= MAX_BINDING_SLOTS {
            return unsupported(format!("{what} slot {slot} >= {MAX_BINDING_SLOTS}"));
        }
        Ok(slot)
    }

    pub fn cbuffer(&mut self, slot: u32) -> R<(u32, u32)> {
        if let Some(&c) = self.cbs.get(&slot) {
            return Ok(c);
        }
        let b = Self::binding_slot(slot, "cbuffer")?;
        let size = match self.d.cb_size.get(&slot) {
            Some(&s) if s > 0 => s.min(4096),
            _ => CBUFFER_VEC4_COUNT,
        };
        let u4 = self.s.ty(Sc::U32, 4);
        let len = self.s.c_u32(size);
        let arr = self.s.array(u4, len);
        self.s.decorate(arr, 6, &[16]);
        let st = self.s.structure(&[arr]);
        self.s.decorate(st, 2, &[]);
        self.s.member_decorate(st, 0, 35, &[0]);
        let ptr = self.s.ptr(2, st);
        let var = self.s.global_var(ptr, 2, None);
        self.s.decorate(var, 34, &[0]);
        self.s.decorate(var, 33, &[b]);
        self.cbs.insert(slot, (var, size));
        Ok((var, size))
    }

    fn image_type(&mut self, sc: Sc, dim: ResDim, depth: u32, sampled: u32, format: u32) -> u32 {
        match (dim, sampled) {
            (ResDim::T1D | ResDim::T1DArray, 1) => {
                self.s.caps.insert(43);
            }
            (ResDim::T1D | ResDim::T1DArray, _) => {
                self.s.caps.insert(44);
            }
            (ResDim::Buffer, 1) => {
                self.s.caps.insert(46);
            }
            (ResDim::Buffer, _) => {
                self.s.caps.insert(47);
            }
            (ResDim::CubeArray, _) => {
                self.s.caps.insert(34);
            }
            (ResDim::T2DMSArray, _) => {
                self.s.caps.insert(48);
            }
            _ => {}
        }
        if dim.ms() && sampled == 2 {
            self.s.caps.insert(27);
        }
        let st = self.s.ty(sc, 1);
        self.s.raw_type(
            25,
            &[
                st,
                dim.spv_dim(),
                depth,
                u32::from(dim.arrayed()),
                u32::from(dim.ms()),
                sampled,
                format,
            ],
        )
    }

    pub fn texture(&mut self, slot: u32) -> R<TexVar> {
        if let Some(&t) = self.tex_vars.get(&slot) {
            return Ok(t);
        }
        let b = Self::binding_slot(slot, "texture")?;
        let decl = self.d.res.get(&slot).copied().unwrap_or(ResDecl {
            dim: ResDim::T2D,
            sc: Sc::F32,
            kind: BufKind::Typed,
        });
        if decl.kind != BufKind::Typed {
            return invalid(format!(
                "t{slot} est un buffer brut/structuré utilisé comme texture"
            ));
        }
        let depth = u32::from(self.d.cmp_tex.contains(&slot));
        let img = self.image_type(decl.sc, decl.dim, depth, 1, 0);
        let ptr = self.s.ptr(0, img);
        let var = self.s.global_var(ptr, 0, None);
        self.s.decorate(var, 34, &[0]);
        self.s.decorate(var, 33, &[TEXTURE_BINDING_BASE + b]);
        let t = TexVar {
            var,
            img_ty: img,
            dim: decl.dim,
            sc: decl.sc,
        };
        self.tex_vars.insert(slot, t);
        Ok(t)
    }

    pub fn sampler(&mut self, slot: u32) -> R<u32> {
        if let Some(&v) = self.samp_vars.get(&slot) {
            return Ok(v);
        }
        let b = Self::binding_slot(slot, "sampler")?;
        let st = self.s.raw_type(26, &[]);
        let ptr = self.s.ptr(0, st);
        let var = self.s.global_var(ptr, 0, None);
        self.s.decorate(var, 34, &[0]);
        self.s.decorate(var, 33, &[SAMPLER_BINDING_BASE + b]);
        self.samp_vars.insert(slot, var);
        Ok(var)
    }

    /// Image de stockage (UAV typé `RWTexture*`/`RWBuffer`).
    pub fn uav_image(&mut self, slot: u32) -> R<TexVar> {
        if let Some(&t) = self.uav_img.get(&slot) {
            return Ok(t);
        }
        let b = Self::binding_slot(slot, "uav")?;
        let decl = self.d.uav.get(&slot).copied().unwrap_or(ResDecl {
            dim: ResDim::T2D,
            sc: Sc::F32,
            kind: BufKind::Typed,
        });
        if decl.kind != BufKind::Typed {
            return invalid(format!("u{slot} est un buffer utilisé comme image"));
        }
        let format = if self.d.atomic_uav.contains(&slot) {
            match decl.sc {
                Sc::I32 => 24,
                Sc::U32 => 33,
                _ => 3,
            }
        } else {
            self.s.caps.insert(55);
            self.s.caps.insert(56);
            0
        };
        let img = self.image_type(decl.sc, decl.dim, 0, 2, format);
        let ptr = self.s.ptr(0, img);
        let var = self.s.global_var(ptr, 0, None);
        self.s.decorate(var, 34, &[0]);
        self.s.decorate(var, 33, &[STORAGE_BINDING_BASE + b]);
        let t = TexVar {
            var,
            img_ty: img,
            dim: decl.dim,
            sc: decl.sc,
        };
        self.uav_img.insert(slot, t);
        Ok(t)
    }

    fn storage_buffer(&mut self, binding: u32, writable: bool) -> u32 {
        let u = self.s.ty(Sc::U32, 1);
        let rta = self.s.runtime_array(u);
        if !self.rta_done {
            self.s.decorate(rta, 6, &[4]);
            self.rta_done = true;
        }
        let st = self.s.struct_new(&[rta]);
        self.s.decorate(st, 2, &[]);
        self.s.member_decorate(st, 0, 35, &[0]);
        if !writable {
            self.s.member_decorate(st, 0, 24, &[]);
        }
        let ptr = self.s.ptr(12, st);
        let var = self.s.global_var(ptr, 12, None);
        self.s.decorate(var, 34, &[0]);
        self.s.decorate(var, 33, &[binding]);
        var
    }

    /// Buffer d'UAV brut/structuré (`StorageBuffer`, binding `24 + slot`).
    pub fn uav_buffer(&mut self, slot: u32) -> R<BufVar> {
        if let Some(&b) = self.uav_buf.get(&slot) {
            return Ok(b);
        }
        let b = Self::binding_slot(slot, "uav")?;
        // Sans déclaration : tableau de `vec4` (stride 16), comme les anciens shaders de test.
        let kind = self
            .d
            .uav
            .get(&slot)
            .map(|d| d.kind)
            .unwrap_or(BufKind::Structured(16));
        if kind == BufKind::Typed {
            return invalid(format!("u{slot} typé utilisé comme buffer brut"));
        }
        let var = self.storage_buffer(STORAGE_BINDING_BASE + b, true);
        let bv = BufVar { var, kind };
        self.uav_buf.insert(slot, bv);
        Ok(bv)
    }

    /// Buffer SRV brut/structuré (`StorageBuffer` en lecture seule, binding `32 + slot`).
    pub fn srv_buffer(&mut self, slot: u32) -> R<BufVar> {
        if let Some(&b) = self.srv_buf.get(&slot) {
            return Ok(b);
        }
        let b = Self::binding_slot(slot, "srv")?;
        let kind = self
            .d
            .res
            .get(&slot)
            .map(|d| d.kind)
            .unwrap_or(BufKind::Structured(16));
        if kind == BufKind::Typed {
            return invalid(format!("t{slot} typé utilisé comme buffer brut"));
        }
        let var = self.storage_buffer(SRV_BUFFER_BINDING_BASE + b, false);
        let bv = BufVar { var, kind };
        self.srv_buf.insert(slot, bv);
        Ok(bv)
    }

    /// Mémoire partagée de groupe `g#` : tableau de `uint` en classe `Workgroup`.
    pub fn tgsm(&mut self, idx: u32) -> R<(u32, u32)> {
        if let Some(&t) = self.tgsm_vars.get(&idx) {
            return Ok(t);
        }
        let Some(&(bytes, stride)) = self.d.tgsm.get(&idx) else {
            return invalid(format!("g{idx} non déclaré"));
        };
        let u = self.s.ty(Sc::U32, 1);
        let len = self.s.c_u32((bytes / 4).max(1));
        let arr = self.s.array(u, len);
        let ptr = self.s.ptr(4, arr);
        let var = self.s.global_var(ptr, 4, None);
        self.tgsm_vars.insert(idx, (var, stride));
        Ok((var, stride))
    }

    // ---------------------------------------------------------------- exécution
    fn label_fn(&mut self, n: u32) -> u32 {
        if let Some(&f) = self.fn_labels.get(&n) {
            return f;
        }
        let f = self.s.id();
        self.fn_labels.insert(n, f);
        f
    }

    fn run(&mut self, insts: &[Inst]) -> R<()> {
        self.body_fn = self.s.id();
        self.s.begin_function(self.body_fn);
        let mut i = 0usize;
        while i < insts.len() {
            let inst = &insts[i];
            self.exec(inst, insts, i)?;
            i += 1;
        }
        if !self.cf.is_empty() {
            return unsupported("flux de contrôle non fermé (endif/endloop/endswitch manquant)");
        }
        self.s.end_function();
        Ok(())
    }

    /// Branche conditionnelle `if (cond) { <body> }` ; `body` termine le bloc `then`.
    fn cond_block(&mut self, cond: Val, body: impl FnOnce(&mut Self)) {
        let t = self.s.id();
        let m = self.s.id();
        self.s.ensure_block();
        self.s.emit_raw(247, &[m, 0]);
        self.s.terminate(250, &[cond.id, t, m]);
        self.s.begin_block(t);
        body(self);
        self.s.begin_block(m);
    }

    fn break_target(&self) -> R<u32> {
        for c in self.cf.iter().rev() {
            match c {
                Cf::Loop { merge, .. } | Cf::Switch { merge, .. } => return Ok(*merge),
                _ => {}
            }
        }
        invalid("break hors d'une boucle/switch")
    }

    fn continue_target(&self) -> R<u32> {
        for c in self.cf.iter().rev() {
            if let Cf::Loop { cont, .. } = c {
                return Ok(*cont);
            }
        }
        invalid("continue hors d'une boucle")
    }

    fn exec(&mut self, inst: &Inst, all: &[Inst], idx: usize) -> R<()> {
        let nz = inst.test_nonzero();
        match inst.opcode {
            // ---- Déclarations et no-ops
            OP_NOP
            | OP_CUSTOMDATA
            | OP_DCL_TEMPS
            | OP_DCL_INDEXABLE_TEMP
            | OP_DCL_GLOBAL_FLAGS
            | OP_DCL_THREAD_GROUP
            | OP_DCL_RESOURCE
            | OP_DCL_CONSTANT_BUFFER
            | OP_DCL_SAMPLER
            | OP_DCL_INPUT
            | OP_DCL_INPUT_SGV
            | OP_DCL_INPUT_SIV
            | OP_DCL_INPUT_PS
            | OP_DCL_INPUT_PS_SGV
            | OP_DCL_INPUT_PS_SIV
            | OP_DCL_OUTPUT
            | OP_DCL_OUTPUT_SGV
            | OP_DCL_OUTPUT_SIV
            | OP_DCL_INDEX_RANGE
            | OP_DCL_UAV_TYPED
            | OP_DCL_UAV_RAW
            | OP_DCL_UAV_STRUCTURED
            | OP_DCL_TGSM_RAW
            | OP_DCL_TGSM_STRUCTURED
            | OP_DCL_RESOURCE_RAW
            | OP_DCL_RESOURCE_STRUCTURED
            | OP_DEBUG_BREAK => Ok(()),
            // ---- Flux de contrôle
            OP_IF => {
                let c = self.test(&inst.operands[0], nz)?;
                let (then_l, else_l, merge) = (self.s.id(), self.s.id(), self.s.id());
                self.s.ensure_block();
                self.s.emit_raw(247, &[merge, 0]);
                self.s.terminate(250, &[c.id, then_l, else_l]);
                self.s.begin_block(then_l);
                self.cf.push(Cf::If {
                    merge,
                    else_lbl: else_l,
                    has_else: false,
                });
                Ok(())
            }
            OP_ELSE => {
                let Some(Cf::If {
                    merge, else_lbl, ..
                }) = self.cf.pop()
                else {
                    return invalid("else sans if");
                };
                self.s.begin_block_from(merge, else_lbl);
                self.cf.push(Cf::If {
                    merge,
                    else_lbl,
                    has_else: true,
                });
                Ok(())
            }
            OP_ENDIF => {
                let Some(Cf::If {
                    merge,
                    else_lbl,
                    has_else,
                }) = self.cf.pop()
                else {
                    return invalid("endif sans if");
                };
                if !has_else {
                    self.s.begin_block_from(merge, else_lbl);
                }
                self.s.begin_block(merge);
                Ok(())
            }
            OP_LOOP => {
                let (header, body, cont, merge) =
                    (self.s.id(), self.s.id(), self.s.id(), self.s.id());
                self.s.begin_block(header);
                self.s.emit_raw(246, &[merge, cont, 0]);
                self.s.terminate(249, &[body]);
                self.s.begin_block(body);
                // Le bloc d'en-tête est retrouvé via `header` stocké dans `cont` → on le garde à part.
                self.cf.push(Cf::Loop { merge, cont });
                self.loop_headers.push(header);
                Ok(())
            }
            OP_ENDLOOP => {
                let Some(Cf::Loop { merge, cont }) = self.cf.pop() else {
                    return invalid("endloop sans loop");
                };
                let header = self.loop_headers.pop().unwrap_or(0);
                self.s.begin_block(cont);
                self.s.branch(header);
                self.s.begin_block(merge);
                Ok(())
            }
            OP_BREAK => {
                let t = self.break_target()?;
                self.s.branch(t);
                Ok(())
            }
            OP_BREAKC => {
                let t = self.break_target()?;
                let c = self.test(&inst.operands[0], nz)?;
                self.cond_block(c, |g| g.s.branch(t));
                Ok(())
            }
            OP_CONTINUE => {
                let t = self.continue_target()?;
                self.s.branch(t);
                Ok(())
            }
            OP_CONTINUEC => {
                let t = self.continue_target()?;
                let c = self.test(&inst.operands[0], nz)?;
                self.cond_block(c, |g| g.s.branch(t));
                Ok(())
            }
            OP_RET => {
                self.s.terminate(253, &[]);
                Ok(())
            }
            OP_RETC => {
                let c = self.test(&inst.operands[0], nz)?;
                self.cond_block(c, |g| g.s.terminate(253, &[]));
                Ok(())
            }
            OP_DISCARD => {
                if self.stage != ShaderStage::Pixel {
                    return unsupported("discard hors pixel shader");
                }
                let c = self.test(&inst.operands[0], nz)?;
                self.cond_block(c, |g| g.s.terminate(252, &[]));
                Ok(())
            }
            OP_SWITCH => self.exec_switch(inst, all, idx),
            OP_CASE => {
                if !matches!(self.cf.last(), Some(Cf::Switch { .. })) {
                    return invalid("case hors switch");
                }
                let lbl = *self
                    .case_labels
                    .last()
                    .and_then(|v| v.get(self.case_pos.last().copied().unwrap_or(0)))
                    .ok_or_else(|| ShaderTranslateError::InvalidOperand("case inattendu".into()))?;
                if let Some(p) = self.case_pos.last_mut() {
                    *p += 1;
                }
                self.s.begin_block(lbl);
                Ok(())
            }
            OP_DEFAULT => {
                let Some(Cf::Switch { default_lbl, .. }) = self.cf.last().copied() else {
                    return invalid("default hors switch");
                };
                self.s.begin_block(default_lbl);
                Ok(())
            }
            OP_ENDSWITCH => {
                let Some(Cf::Switch { merge, .. }) = self.cf.pop() else {
                    return invalid("endswitch sans switch");
                };
                self.case_labels.pop();
                self.case_pos.pop();
                self.s.begin_block(merge);
                Ok(())
            }
            OP_LABEL => {
                if !self.cf.is_empty() {
                    return invalid("label dans un bloc de contrôle ouvert");
                }
                let n = inst.operands.first().map(|o| o.idx[0].imm).unwrap_or(0);
                let f = self.label_fn(n);
                self.s.end_function();
                self.s.begin_function(f);
                Ok(())
            }
            OP_CALL => {
                let n = inst.operands.first().map(|o| o.idx[0].imm).unwrap_or(0);
                let f = self.label_fn(n);
                let v = self.s.t_void();
                self.s.emit(57, v, &[f]);
                Ok(())
            }
            OP_CALLC => {
                let n = inst.operands.get(1).map(|o| o.idx[0].imm).unwrap_or(0);
                let f = self.label_fn(n);
                let c = self.test(&inst.operands[0], nz)?;
                let v = self.s.t_void();
                self.cond_block(c, |g| {
                    g.s.emit(57, v, &[f]);
                });
                Ok(())
            }
            _ => self.exec_data(inst),
        }
    }

    fn exec_switch(&mut self, inst: &Inst, all: &[Inst], idx: usize) -> R<()> {
        // Pré-balayage : valeurs des `case` et présence de `default` de ce niveau.
        let mut depth = 0i32;
        let mut cases: Vec<u32> = Vec::new();
        let mut has_default = false;
        for j in &all[idx + 1..] {
            match j.opcode {
                OP_SWITCH => depth += 1,
                OP_ENDSWITCH if depth == 0 => break,
                OP_ENDSWITCH => depth -= 1,
                OP_CASE if depth == 0 => {
                    let Some(o) = j.operands.first() else {
                        return invalid("case sans valeur");
                    };
                    cases.push(o.imm[0]);
                }
                OP_DEFAULT if depth == 0 => has_default = true,
                _ => {}
            }
        }
        let sel = self.read(&inst.operands[0], &[0], Sc::U32)?;
        let merge = self.s.id();
        let default_lbl = if has_default { self.s.id() } else { merge };
        let labels: Vec<u32> = cases.iter().map(|_| self.s.id()).collect();
        self.s.ensure_block();
        self.s.emit_raw(247, &[merge, 0]);
        let mut ops = vec![sel.id, default_lbl];
        for (v, l) in cases.iter().zip(&labels) {
            ops.push(*v);
            ops.push(*l);
        }
        self.s.terminate(251, &ops);
        self.case_labels.push(labels);
        self.case_pos.push(0);
        self.cf.push(Cf::Switch { merge, default_lbl });
        Ok(())
    }

    // ---------------------------------------------------------------- finalisation
    fn finish(mut self) -> R<Emitted> {
        let main_fn = self.s.id();
        self.s.begin_function(main_fn);
        let v = self.s.t_void();
        let body = self.body_fn;
        self.s.emit(57, v, &[body]);
        self.copy_outputs()?;
        self.s.end_function();

        let model = match self.stage {
            ShaderStage::Vertex => 0,
            ShaderStage::Pixel => 4,
            _ => 5,
        };
        let mut modes: Vec<Vec<u32>> = Vec::new();
        match self.stage {
            ShaderStage::Pixel => {
                modes.push(vec![7]);
                if self
                    .d
                    .depth_ops
                    .contains(&OPERAND_OUTPUT_DEPTH_GREATER_EQUAL)
                {
                    modes.push(vec![12]);
                    modes.push(vec![14]);
                } else if self.d.depth_ops.contains(&OPERAND_OUTPUT_DEPTH_LESS_EQUAL) {
                    modes.push(vec![12]);
                    modes.push(vec![15]);
                } else if !self.d.depth_ops.is_empty() {
                    modes.push(vec![12]);
                }
                if self.d.early_z {
                    modes.push(vec![9]);
                }
            }
            ShaderStage::Compute => {
                let t = self.d.tgroup;
                modes.push(vec![17, t[0], t[1], t[2]]);
            }
            _ => {}
        }
        let spirv = self.s.assemble(model, main_fn, &modes);
        Ok(Emitted {
            spirv,
            used_cbuffers: self.cbs.keys().copied().collect(),
            used_textures: self.tex_vars.keys().copied().collect(),
            used_samplers: self.samp_vars.keys().copied().collect(),
            used_storage_buffers: self
                .uav_buf
                .keys()
                .chain(self.uav_img.keys())
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            used_srv_buffers: self.srv_buf.keys().copied().collect(),
            thread_group_size: self.d.tgroup,
        })
    }

    fn copy_outputs(&mut self) -> R<()> {
        let regs: Vec<u32> = self.outs.keys().copied().collect();
        for reg in regs {
            let (private, target) = {
                let o = &self.outs[&reg];
                (o.private, o.target)
            };
            let val = self.load(private, Sc::U32, 4);
            match target {
                OutTarget::Loc { var, sc } => {
                    let v = self.s.bitcast(val, sc);
                    self.s.emit_void(62, &[var, v.id]);
                }
                OutTarget::Position { var } => {
                    let v = self.s.bitcast(val, Sc::F32);
                    let z = self.s.c_u32(0);
                    let p = self.pty(3, Sc::F32, 4);
                    let ptr = self.s.emit(65, p, &[var, z]);
                    self.s.emit_void(62, &[ptr, v.id]);
                }
                OutTarget::Layer { var } | OutTarget::ViewportIndex { var } => {
                    let c = self.s.extract(val, 0);
                    let i = self.s.bitcast(c, Sc::I32);
                    self.s.emit_void(62, &[var, i.id]);
                }
                OutTarget::ClipCull => {
                    let f = self.s.bitcast(val, Sc::F32);
                    let pv = self.pv_var.unwrap_or(0);
                    let table: Vec<(u32, Vec<(u32, u32)>)> = {
                        let mut t = Vec::new();
                        t.push((1u32, self.clip.clone()));
                        t.push((if self.clip.is_empty() { 1 } else { 2 }, self.cull.clone()));
                        t
                    };
                    for (member, entries) in table {
                        for (k, (r, c)) in entries.iter().enumerate() {
                            if *r != reg {
                                continue;
                            }
                            let e = self.s.extract(f, *c);
                            let m = self.s.c_u32(member);
                            let kk = self.s.c_u32(k as u32);
                            let p = self.pty(3, Sc::F32, 1);
                            let ptr = self.s.emit(65, p, &[pv, m, kk]);
                            self.s.emit_void(62, &[ptr, e.id]);
                        }
                    }
                }
            }
        }
        if let (Some(var), Some(&p)) = (
            self.depth_var,
            self.special_outs
                .iter()
                .find(|(k, _)| **k != OPERAND_OUTPUT_COVERAGE_MASK)
                .map(|(_, v)| v),
        ) {
            let v = self.load(p, Sc::U32, 4);
            let c = self.s.extract(v, 0);
            let f = self.s.bitcast(c, Sc::F32);
            self.s.emit_void(62, &[var, f.id]);
        }
        if let (Some(var), Some(&p)) = (
            self.mask_out_var,
            self.special_outs.get(&OPERAND_OUTPUT_COVERAGE_MASK),
        ) {
            let v = self.load(p, Sc::U32, 4);
            let c = self.s.extract(v, 0);
            let z = self.s.c_u32(0);
            let pt = self.pty(3, Sc::U32, 1);
            let ptr = self.s.emit(65, pt, &[var, z]);
            self.s.emit_void(62, &[ptr, c.id]);
        }
        Ok(())
    }
}
