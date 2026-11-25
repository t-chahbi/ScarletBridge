//! Interprétation des métadonnées DXIL (`dx.shaderModel`, `dx.resources`, `dx.entryPoints`).
#![allow(dead_code)] // métadonnées/IR décodées en entier (GS/HS/DS à venir) : champs non encore consommés

use super::DxilParseError;
use super::module::{Md, Module, ValId, ValueKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResClass {
    Srv,
    Uav,
    Cbv,
    Sampler,
}

/// `DXIL::ResourceKind`.
pub mod rk {
    pub const TEXTURE1D: u32 = 1;
    pub const TEXTURE2D: u32 = 2;
    pub const TEXTURE2DMS: u32 = 3;
    pub const TEXTURE3D: u32 = 4;
    pub const TEXTURECUBE: u32 = 5;
    pub const TEXTURE1DARRAY: u32 = 6;
    pub const TEXTURE2DARRAY: u32 = 7;
    pub const TEXTURE2DMSARRAY: u32 = 8;
    pub const TEXTURECUBEARRAY: u32 = 9;
    pub const TYPEDBUFFER: u32 = 10;
    pub const RAWBUFFER: u32 = 11;
    pub const STRUCTUREDBUFFER: u32 = 12;
    pub const CBUFFER: u32 = 13;
    pub const SAMPLER: u32 = 14;
    pub const TBUFFER: u32 = 15;
    pub const RTACCEL: u32 = 16;
    pub const STRUCTUREDBUFFERCOUNTER: u32 = 19;
    pub const SAMPLERCOMPARISON: u32 = 20;
}

#[derive(Debug, Clone)]
pub struct ResRecord {
    pub class: ResClass,
    pub id: u32,
    pub name: String,
    pub space: u32,
    pub lower: u32,
    /// Nombre de slots du range (`u32::MAX` si non borné).
    pub size: u32,
    pub kind: u32,
    /// `ComponentType` DXIL de l'élément (SRV/UAV typés), 0 si inconnu.
    pub comp_type: u32,
    pub stride: u32,
    /// Taille en octets d'un cbuffer.
    pub byte_size: u32,
    pub has_counter: bool,
    pub rov: bool,
}

#[derive(Debug, Clone)]
pub struct SigElem {
    pub id: u32,
    pub name: String,
    pub comp_type: u32,
    pub sys_kind: u32,
    pub sem_index: Vec<u32>,
    pub interp: u32,
    pub rows: u32,
    pub cols: u32,
    pub start_row: i32,
    pub start_col: u32,
    pub used_mask: u32,
}

#[derive(Debug, Clone, Default)]
pub struct GsState {
    pub input_prim: u32,
    pub max_vertices: u32,
    pub instance_count: u32,
    pub stream_topology: u32,
}

#[derive(Debug, Clone, Default)]
pub struct HsState {
    pub patch_const_fn: Option<usize>,
    pub input_cps: u32,
    pub output_cps: u32,
    pub domain: u32,
    pub partitioning: u32,
    pub out_prim: u32,
}

#[derive(Debug, Clone)]
pub struct EntryInfo {
    pub func: usize,
    pub name: String,
    pub kind: u32,
    pub sm_major: u32,
    pub sm_minor: u32,
    pub inputs: Vec<SigElem>,
    pub outputs: Vec<SigElem>,
    pub patch: Vec<SigElem>,
    pub srvs: Vec<ResRecord>,
    pub uavs: Vec<ResRecord>,
    pub cbvs: Vec<ResRecord>,
    pub samplers: Vec<ResRecord>,
    pub flags: u64,
    pub numthreads: [u32; 3],
    pub gs: GsState,
    pub hs: HsState,
    pub ds_domain: u32,
    pub ds_input_cps: u32,
}

fn bad<T>(m: impl Into<String>) -> Result<T, DxilParseError> {
    Err(DxilParseError::Module(m.into()))
}

struct Mdx<'a> {
    m: &'a Module,
}

impl Mdx<'_> {
    fn int(&self, id: Option<u32>) -> Option<u64> {
        match self.m.md(id?)? {
            Md::Value(_, v) => self.m.const_int(*v),
            _ => None,
        }
    }
    fn s(&self, id: Option<u32>) -> Option<String> {
        match self.m.md(id?)? {
            Md::Str(s) => Some(s.clone()),
            _ => None,
        }
    }
    fn node(&self, id: Option<u32>) -> Option<&[Option<u32>]> {
        match self.m.md(id?)? {
            Md::Node(v) => Some(v),
            _ => None,
        }
    }
    fn func(&self, id: Option<u32>) -> Option<usize> {
        match self.m.md(id?)? {
            Md::Value(_, v) => match self.m.values.get(*v as usize)?.kind {
                ValueKind::Function(f) => Some(f),
                _ => None,
            },
            _ => None,
        }
    }
    fn val_id(&self, id: Option<u32>) -> Option<ValId> {
        match self.m.md(id?)? {
            Md::Value(_, v) => Some(*v),
            _ => None,
        }
    }
}

fn tag_pairs(x: &Mdx, id: Option<u32>) -> Vec<(u64, Option<u32>)> {
    let mut out = Vec::new();
    if let Some(n) = x.node(id) {
        let mut i = 0;
        while i + 1 < n.len() {
            if let Some(t) = x.int(n[i]) {
                out.push((t, n[i + 1]));
            }
            i += 2;
        }
    }
    out
}

fn parse_res_list(x: &Mdx, list: Option<u32>, class: ResClass) -> Vec<ResRecord> {
    let mut out = Vec::new();
    let Some(items) = x.node(list) else {
        return out;
    };
    for &it in items {
        let Some(n) = x.node(it) else { continue };
        let g = |i: usize| x.int(n.get(i).copied().flatten()).unwrap_or(0);
        let mut r = ResRecord {
            class,
            id: g(0) as u32,
            name: x.s(n.get(2).copied().flatten()).unwrap_or_default(),
            space: g(3) as u32,
            lower: g(4) as u32,
            size: if g(5) == 0 || g(5) > u64::from(u32::MAX) {
                u32::MAX
            } else {
                g(5) as u32
            },
            kind: 0,
            comp_type: 0,
            stride: 0,
            byte_size: 0,
            has_counter: false,
            rov: false,
        };
        #[allow(clippy::needless_late_init)]
        let tags_idx;
        match class {
            ResClass::Srv => {
                r.kind = g(6) as u32;
                tags_idx = 8;
            }
            ResClass::Uav => {
                r.kind = g(6) as u32;
                r.has_counter = g(8) != 0;
                r.rov = g(9) != 0;
                tags_idx = 10;
            }
            ResClass::Cbv => {
                r.kind = rk::CBUFFER;
                r.byte_size = g(6) as u32;
                tags_idx = 7;
            }
            ResClass::Sampler => {
                r.kind = if g(6) == 1 {
                    rk::SAMPLERCOMPARISON
                } else {
                    rk::SAMPLER
                };
                tags_idx = 99;
            }
        }
        for (t, v) in tag_pairs(x, n.get(tags_idx).copied().flatten()) {
            let val = x.int(v).unwrap_or(0) as u32;
            match t {
                0 => r.comp_type = val,
                1 => r.stride = val,
                _ => {}
            }
        }
        out.push(r);
    }
    out
}

fn parse_sig(x: &Mdx, list: Option<u32>) -> Vec<SigElem> {
    let mut out = Vec::new();
    let Some(items) = x.node(list) else {
        return out;
    };
    for &it in items {
        let Some(n) = x.node(it) else { continue };
        let g = |i: usize| x.int(n.get(i).copied().flatten()).unwrap_or(0);
        let sem_index = x
            .node(n.get(4).copied().flatten())
            .map(|v| v.iter().map(|&e| x.int(e).unwrap_or(0) as u32).collect())
            .unwrap_or_default();
        let mut e = SigElem {
            id: g(0) as u32,
            name: x.s(n.get(1).copied().flatten()).unwrap_or_default(),
            comp_type: g(2) as u32,
            sys_kind: g(3) as u32,
            sem_index,
            interp: g(5) as u32,
            rows: g(6) as u32,
            cols: g(7) as u32,
            start_row: g(8) as u32 as i32,
            start_col: g(9) as u32,
            used_mask: 0xF,
        };
        for (t, v) in tag_pairs(x, n.get(10).copied().flatten()) {
            if t == 3 {
                e.used_mask = x.int(v).unwrap_or(0xF) as u32;
            }
        }
        out.push(e);
    }
    out
}

pub fn parse_entry(m: &Module) -> Result<EntryInfo, DxilParseError> {
    let x = Mdx { m };
    let Some(sm) = m.named_md("dx.shaderModel") else {
        return bad("dx.shaderModel absent");
    };
    let sm_node = x.node(sm.first().map(|&v| Some(v)).unwrap_or(None));
    let Some(smn) = sm_node else {
        return bad("dx.shaderModel invalide");
    };
    let sm_kind_str = x.s(smn.first().copied().flatten()).unwrap_or_default();
    let sm_major = x.int(smn.get(1).copied().flatten()).unwrap_or(6) as u32;
    let sm_minor = x.int(smn.get(2).copied().flatten()).unwrap_or(0) as u32;
    let kind = match sm_kind_str.as_str() {
        "ps" => 0,
        "vs" => 1,
        "gs" => 2,
        "hs" => 3,
        "ds" => 4,
        "cs" => 5,
        "lib" => 6,
        "ms" => 13,
        "as" => 14,
        _ => 255,
    };
    let Some(eps) = m.named_md("dx.entryPoints") else {
        return bad("dx.entryPoints absent");
    };
    if eps.is_empty() {
        return bad("aucun point d'entrée");
    }
    if kind == 6 {
        // Bibliothèque : reconnaissance seulement.
        return Ok(EntryInfo {
            func: 0,
            name: String::new(),
            kind,
            sm_major,
            sm_minor,
            inputs: vec![],
            outputs: vec![],
            patch: vec![],
            srvs: vec![],
            uavs: vec![],
            cbvs: vec![],
            samplers: vec![],
            flags: 0,
            numthreads: [1, 1, 1],
            gs: GsState::default(),
            hs: HsState::default(),
            ds_domain: 0,
            ds_input_cps: 0,
        });
    }
    let Some(ep) = x.node(Some(eps[0])) else {
        return bad("dx.entryPoints invalide");
    };
    let Some(func) = x.func(ep.first().copied().flatten()) else {
        return bad("point d'entrée sans fonction");
    };
    let name = x.s(ep.get(1).copied().flatten()).unwrap_or_default();
    let sigs = x.node(ep.get(2).copied().flatten()).unwrap_or(&[]);
    let inputs = parse_sig(&x, sigs.first().copied().flatten());
    let outputs = parse_sig(&x, sigs.get(1).copied().flatten());
    let patch = parse_sig(&x, sigs.get(2).copied().flatten());
    let res = x
        .node(ep.get(3).copied().flatten())
        .or_else(|| {
            m.named_md("dx.resources")
                .and_then(|v| x.node(v.first().map(|&e| Some(e)).unwrap_or(None)))
        })
        .unwrap_or(&[]);
    let srvs = parse_res_list(&x, res.first().copied().flatten(), ResClass::Srv);
    let uavs = parse_res_list(&x, res.get(1).copied().flatten(), ResClass::Uav);
    let cbvs = parse_res_list(&x, res.get(2).copied().flatten(), ResClass::Cbv);
    let samplers = parse_res_list(&x, res.get(3).copied().flatten(), ResClass::Sampler);
    let mut info = EntryInfo {
        func,
        name,
        kind,
        sm_major,
        sm_minor,
        inputs,
        outputs,
        patch,
        srvs,
        uavs,
        cbvs,
        samplers,
        flags: 0,
        numthreads: [1, 1, 1],
        gs: GsState::default(),
        hs: HsState::default(),
        ds_domain: 0,
        ds_input_cps: 0,
    };
    for (tag, val) in tag_pairs(&x, ep.get(4).copied().flatten()) {
        match tag {
            0 => info.flags = x.int(val).unwrap_or(0),
            4 => {
                if let Some(n) = x.node(val) {
                    for (i, t) in info.numthreads.iter_mut().enumerate() {
                        *t = (x.int(n.get(i).copied().flatten()).unwrap_or(1) as u32).max(1);
                    }
                }
            }
            1 => {
                if let Some(n) = x.node(val) {
                    let g = |i: usize| x.int(n.get(i).copied().flatten()).unwrap_or(0) as u32;
                    info.gs = GsState {
                        input_prim: g(0),
                        max_vertices: g(1),
                        stream_topology: g(3),
                        instance_count: g(4).max(1),
                    };
                }
            }
            2 => {
                if let Some(n) = x.node(val) {
                    let g = |i: usize| x.int(n.get(i).copied().flatten()).unwrap_or(0) as u32;
                    info.ds_domain = g(0);
                    info.ds_input_cps = g(1);
                }
            }
            3 => {
                if let Some(n) = x.node(val) {
                    let g = |i: usize| x.int(n.get(i).copied().flatten()).unwrap_or(0) as u32;
                    info.hs = HsState {
                        patch_const_fn: x.func(n.first().copied().flatten()),
                        input_cps: g(1),
                        output_cps: g(2),
                        domain: g(3),
                        partitioning: g(4),
                        out_prim: g(5),
                    };
                }
            }
            _ => {}
        }
    }
    let _ = x.val_id(None);
    Ok(info)
}
