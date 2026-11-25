//! Interface du shader : signatures d'entrée/sortie -> variables `Input`/`Output`, built-ins,
//! identifiants de threads (compute).
//!
//! Locations : les éléments de signature non built-in reçoivent des Locations consécutives par
//! rang de registre (même convention que le traducteur DXBC : `SV_Position` & co ne consomment
//! pas de Location) ; les éléments empaquetés sur un même registre partagent la Location avec
//! `Component = colonne de départ`. Sorties pixel : `Location = index de cible`.

use std::collections::{BTreeMap, HashMap};

use super::dxmeta::SigElem;
use super::module::{Op, ValueKind};
use super::sp::*;
use super::tr::{R, RVal, Tr, invalid, unsup};
use crate::shader::ShaderStage;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InKind {
    Loc,
    FragCoord,
    FrontFace,
    ScalarBuiltin,
    SampleMask,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct InVar {
    pub var: u32,
    pub kind: InKind,
    pub rows: u32,
    pub cols: u32,
    pub comp: St,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OutKind {
    Loc,
    Position,
    Clip,
    Cull,
    Scalar,
    Coverage,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct OutVar {
    pub var: u32,
    pub kind: OutKind,
    pub rows: u32,
    pub cols: u32,
    pub comp: St,
    pub base: u32,
}

#[derive(Default)]
pub(super) struct IoState {
    pub inputs: HashMap<u32, InVar>,
    pub outputs: HashMap<u32, OutVar>,
    pub interface: Vec<u32>,
    pub per_vertex: Option<u32>,
    pub clip_n: u32,
    pub cull_n: u32,
    pub builtins: HashMap<u32, u32>,
    pub used_in: Vec<u32>,
    pub pervertex: Vec<u32>,
    pub depth_modes: Vec<u32>,
}

fn comp_st(comp_type: u32) -> St {
    match comp_type {
        1..=7 => St::I32,
        _ => St::F32,
    }
}

fn rank_map(elems: &[&SigElem]) -> BTreeMap<i32, u32> {
    let mut rows: Vec<i32> = Vec::new();
    for e in elems {
        for r in 0..e.rows.max(1) {
            rows.push(e.start_row + r as i32);
        }
    }
    rows.sort_unstable();
    rows.dedup();
    rows.into_iter()
        .enumerate()
        .map(|(i, r)| (r, i as u32))
        .collect()
}

impl Tr<'_> {
    fn input_is_builtin(&self, e: &SigElem) -> bool {
        match self.stage {
            ShaderStage::Vertex => matches!(e.sys_kind, 1 | 2),
            ShaderStage::Pixel => matches!(e.sys_kind, 3 | 4 | 5 | 10 | 12 | 13 | 14),
            _ => e.sys_kind != 0,
        }
    }

    fn output_is_builtin(&self, e: &SigElem) -> bool {
        match self.stage {
            ShaderStage::Vertex => matches!(e.sys_kind, 3..=7),
            ShaderStage::Pixel => matches!(e.sys_kind, 14 | 16 | 17 | 18 | 19 | 20),
            _ => e.sys_kind != 0,
        }
    }

    /// Repère les éléments d'entrée réellement lus par le shader.
    fn scan_used_inputs(&mut self) {
        let f = self.f;
        for b in &f.blocks {
            for i in &b.insts {
                let Op::Call { callee, args } = &i.op else {
                    continue;
                };
                let Some(ValueKind::Function(fi)) =
                    f.values.get(*callee as usize).map(|v| v.kind.clone())
                else {
                    continue;
                };
                let name = &self.m.functions[fi].name;
                let op = args.first().and_then(|&a| self.const_u(a)).unwrap_or(0);
                let is_in = name.starts_with("dx.op.loadInput")
                    || name.starts_with("dx.op.evalSnapped")
                    || name.starts_with("dx.op.evalSampleIndex")
                    || name.starts_with("dx.op.evalCentroid")
                    || name.starts_with("dx.op.attributeAtVertex")
                    || op == 4;
                if is_in && let Some(id) = args.get(1).and_then(|&a| self.const_u(a)) {
                    self.io.used_in.push(id as u32);
                    if name.starts_with("dx.op.attributeAtVertex") || op == 137 {
                        self.io.pervertex.push(id as u32);
                    }
                }
            }
        }
    }

    pub fn setup_io(&mut self) -> R<()> {
        let info = self.info;
        if self.stage == ShaderStage::Compute {
            return Ok(());
        }
        self.scan_used_inputs();
        // ---- entrées
        let in_loc: Vec<&SigElem> = info
            .inputs
            .iter()
            .filter(|e| !self.input_is_builtin(e))
            .collect();
        let in_rank = rank_map(&in_loc);
        for (idx, e) in info.inputs.iter().enumerate() {
            if !self.io.used_in.contains(&(idx as u32)) {
                continue;
            }
            self.make_input(idx as u32, e, &in_rank)?;
        }
        // ---- sorties
        let out_loc: Vec<&SigElem> = info
            .outputs
            .iter()
            .filter(|e| !self.output_is_builtin(e))
            .collect();
        let out_rank = rank_map(&out_loc);
        let mut clip_base = 0u32;
        let mut cull_base = 0u32;
        let mut clips: Vec<(usize, &SigElem)> = Vec::new();
        for (i, e) in info.outputs.iter().enumerate() {
            if self.stage == ShaderStage::Vertex && matches!(e.sys_kind, 6 | 7) {
                clips.push((i, e));
            }
        }
        clips.sort_by_key(|(i, e)| (e.sys_kind, e.sem_index.first().copied().unwrap_or(0), *i));
        let mut bases: HashMap<usize, u32> = HashMap::new();
        for (i, e) in &clips {
            let n = e.rows.max(1) * e.cols.max(1);
            if e.sys_kind == 6 {
                bases.insert(*i, clip_base);
                clip_base += n;
            } else {
                bases.insert(*i, cull_base);
                cull_base += n;
            }
        }
        if clip_base > 8 || cull_base > 8 || clip_base + cull_base > 8 {
            return unsup("plus de 8 distances clip/cull");
        }
        self.io.clip_n = clip_base;
        self.io.cull_n = cull_base;
        if self.stage == ShaderStage::Vertex {
            self.make_per_vertex()?;
        }
        for (idx, e) in info.outputs.iter().enumerate() {
            let base = bases.get(&idx).copied().unwrap_or(0);
            self.make_output(idx as u32, e, &out_rank, base)?;
        }
        Ok(())
    }

    fn decorate_loc(&mut self, var: u32, loc: u32, comp: u32) {
        self.s.decorate(var, DEC_LOCATION, &[loc]);
        if comp != 0 {
            self.s.decorate(var, DEC_COMPONENT, &[comp]);
        }
    }

    fn make_input(&mut self, idx: u32, e: &SigElem, rank: &BTreeMap<i32, u32>) -> R<()> {
        let (kind, ty_st, cols, rows, builtin): (InKind, St, u32, u32, Option<u32>) =
            match (self.stage, e.sys_kind) {
                (ShaderStage::Vertex, 1) => {
                    (InKind::ScalarBuiltin, St::I32, 1, 1, Some(BI_VERTEX_INDEX))
                }
                (ShaderStage::Vertex, 2) => (
                    InKind::ScalarBuiltin,
                    St::I32,
                    1,
                    1,
                    Some(BI_INSTANCE_INDEX),
                ),
                (ShaderStage::Pixel, 3) => (InKind::FragCoord, St::F32, 4, 1, Some(BI_FRAG_COORD)),
                (ShaderStage::Pixel, 13) => {
                    (InKind::FrontFace, St::I1, 1, 1, Some(BI_FRONT_FACING))
                }
                (ShaderStage::Pixel, 10) => {
                    self.s.caps.insert(CAP_GEOMETRY);
                    (InKind::ScalarBuiltin, St::I32, 1, 1, Some(BI_PRIMITIVE_ID))
                }
                (ShaderStage::Pixel, 12) => {
                    self.s.caps.insert(CAP_SAMPLE_RATE_SHADING);
                    (InKind::ScalarBuiltin, St::I32, 1, 1, Some(BI_SAMPLE_ID))
                }
                (ShaderStage::Pixel, 14) => {
                    (InKind::SampleMask, St::I32, 1, 1, Some(BI_SAMPLE_MASK))
                }
                (ShaderStage::Pixel, 4) => {
                    self.s.caps.insert(CAP_GEOMETRY);
                    (InKind::ScalarBuiltin, St::I32, 1, 1, Some(BI_LAYER))
                }
                (ShaderStage::Pixel, 5) => {
                    self.s.caps.insert(57);
                    (
                        InKind::ScalarBuiltin,
                        St::I32,
                        1,
                        1,
                        Some(BI_VIEWPORT_INDEX),
                    )
                }
                (_, k) if k != 0 && self.input_is_builtin(e) => {
                    return unsup(format!("entrée système SV (kind {k}) non supportée"));
                }
                (_, 0) => (
                    InKind::Loc,
                    comp_st(e.comp_type),
                    e.cols.clamp(1, 4),
                    e.rows.max(1),
                    None,
                ),
                (_, k) => return unsup(format!("entrée système SV (kind {k}) non supportée")),
            };
        let st_for_ty = if kind == InKind::SampleMask {
            St::I32
        } else {
            ty_st
        };
        let elem = self.vty(st_for_ty, cols);
        let pervertex = builtin.is_none() && self.io.pervertex.contains(&idx);
        let var_ty = if pervertex {
            self.s.caps.insert(CAP_FRAGMENT_BARYCENTRIC);
            self.s.exts.insert("SPV_KHR_fragment_shader_barycentric");
            let len = self.c32(3);
            self.s.array(elem, len)
        } else if kind == InKind::SampleMask {
            let len = self.c32(1);
            self.s.array(elem, len)
        } else if rows > 1 {
            let len = self.c32(rows);
            self.s.array(elem, len)
        } else {
            elem
        };
        let ptr = self.s.ptr(SC_INPUT, var_ty);
        let var = self.s.global_var(ptr, SC_INPUT, None);
        if let Some(b) = builtin {
            self.s.decorate(var, DEC_BUILTIN, &[b]);
            if self.stage == ShaderStage::Pixel
                && !matches!(kind, InKind::FragCoord | InKind::FrontFace)
            {
                self.s.decorate(var, DEC_FLAT, &[]);
            }
        } else {
            let row = rank.get(&e.start_row).copied().unwrap_or(0);
            self.decorate_loc(var, row, e.start_col);
            if pervertex {
                self.s.decorate(var, DEC_PER_VERTEX_KHR, &[]);
            } else if self.stage == ShaderStage::Pixel {
                if ty_st != St::F32 || e.interp == 1 {
                    self.s.decorate(var, DEC_FLAT, &[]);
                }
                if matches!(e.interp, 4 | 5 | 7) {
                    self.s.decorate(var, DEC_NO_PERSPECTIVE, &[]);
                }
                if matches!(e.interp, 3 | 5) {
                    self.s.decorate(var, DEC_CENTROID, &[]);
                }
                if matches!(e.interp, 6 | 7) {
                    self.s.decorate(var, DEC_SAMPLE, &[]);
                    self.s.caps.insert(CAP_SAMPLE_RATE_SHADING);
                }
            }
        }
        self.io.interface.push(var);
        self.io.inputs.insert(
            idx,
            InVar {
                var,
                kind,
                rows,
                cols,
                comp: ty_st,
            },
        );
        Ok(())
    }

    fn make_per_vertex(&mut self) -> R<()> {
        let f4 = self.vty(St::F32, 4);
        let mut members = vec![f4];
        let f1 = self.sty(St::F32);
        if self.io.clip_n > 0 {
            self.s.caps.insert(32);
            let len = self.c32(self.io.clip_n);
            members.push(self.s.array(f1, len));
        }
        if self.io.cull_n > 0 {
            self.s.caps.insert(33);
            let len = self.c32(self.io.cull_n);
            members.push(self.s.array(f1, len));
        }
        let st = self.s.struct_new(&members);
        self.s.decorate(st, DEC_BLOCK, &[]);
        self.s.member_decorate(st, 0, DEC_BUILTIN, &[0]);
        let mut mi = 1;
        if self.io.clip_n > 0 {
            self.s.member_decorate(st, mi, DEC_BUILTIN, &[3]);
            mi += 1;
        }
        if self.io.cull_n > 0 {
            self.s.member_decorate(st, mi, DEC_BUILTIN, &[4]);
        }
        let ptr = self.s.ptr(SC_OUTPUT, st);
        let var = self.s.global_var(ptr, SC_OUTPUT, None);
        self.io.interface.push(var);
        self.io.per_vertex = Some(var);
        Ok(())
    }

    fn make_output(
        &mut self,
        idx: u32,
        e: &SigElem,
        rank: &BTreeMap<i32, u32>,
        base: u32,
    ) -> R<()> {
        let stage = self.stage;
        let (kind, comp, cols, rows) = match (stage, e.sys_kind) {
            (ShaderStage::Vertex, 3) => (OutKind::Position, St::F32, 4, 1),
            (ShaderStage::Vertex, 6) => (OutKind::Clip, St::F32, e.cols.max(1), e.rows.max(1)),
            (ShaderStage::Vertex, 7) => (OutKind::Cull, St::F32, e.cols.max(1), e.rows.max(1)),
            (ShaderStage::Vertex, 4 | 5) => (OutKind::Scalar, St::I32, 1, 1),
            (ShaderStage::Pixel, 16) => (OutKind::Loc, comp_st(e.comp_type), 4, 1),
            (ShaderStage::Pixel, 17..=19) => (OutKind::Scalar, St::F32, 1, 1),
            (ShaderStage::Pixel, 14) => (OutKind::Coverage, St::I32, 1, 1),
            (ShaderStage::Pixel, 20) => return unsup("SV_StencilRef (export de stencil)"),
            (_, 0) => (
                OutKind::Loc,
                comp_st(e.comp_type),
                e.cols.clamp(1, 4),
                e.rows.max(1),
            ),
            (_, k) => return unsup(format!("sortie système SV (kind {k}) non supportée")),
        };
        match kind {
            OutKind::Position | OutKind::Clip | OutKind::Cull => {
                self.io.outputs.insert(
                    idx,
                    OutVar {
                        var: self.io.per_vertex.unwrap_or(0),
                        kind,
                        rows,
                        cols,
                        comp,
                        base,
                    },
                );
                return Ok(());
            }
            _ => {}
        }
        let elem = self.vty(comp, cols);
        let var_ty = if kind == OutKind::Coverage {
            let len = self.c32(1);
            self.s.array(elem, len)
        } else if rows > 1 {
            let len = self.c32(rows);
            self.s.array(elem, len)
        } else {
            elem
        };
        let ptr = self.s.ptr(SC_OUTPUT, var_ty);
        let var = self.s.global_var(ptr, SC_OUTPUT, None);
        match (stage, e.sys_kind, kind) {
            (ShaderStage::Vertex, 4, _) => {
                self.s.caps.insert(5254);
                self.s.exts.insert("SPV_EXT_shader_viewport_index_layer");
                self.s.decorate(var, DEC_BUILTIN, &[BI_LAYER]);
                self.s.decorate(var, DEC_FLAT, &[]);
            }
            (ShaderStage::Vertex, 5, _) => {
                self.s.caps.insert(5254);
                self.s.exts.insert("SPV_EXT_shader_viewport_index_layer");
                self.s.decorate(var, DEC_BUILTIN, &[BI_VIEWPORT_INDEX]);
                self.s.decorate(var, DEC_FLAT, &[]);
            }
            (ShaderStage::Pixel, 17..=19, _) => {
                self.s.decorate(var, DEC_BUILTIN, &[BI_FRAG_DEPTH]);
                self.io.depth_modes.push(match e.sys_kind {
                    19 => 14,
                    18 => 15,
                    _ => 0,
                });
            }
            (ShaderStage::Pixel, 14, _) => {
                self.s.decorate(var, DEC_BUILTIN, &[BI_SAMPLE_MASK]);
            }
            (ShaderStage::Pixel, 16, _) => {
                let t = if e.start_row >= 0 {
                    e.start_row as u32
                } else {
                    e.sem_index.first().copied().unwrap_or(0)
                };
                self.s.decorate(var, DEC_LOCATION, &[t]);
            }
            _ => {
                let row = rank.get(&e.start_row).copied().unwrap_or(0);
                self.decorate_loc(var, row, e.start_col);
                if comp != St::F32 {
                    self.s.decorate(var, DEC_FLAT, &[]);
                }
            }
        }
        self.io.interface.push(var);
        self.io.outputs.insert(
            idx,
            OutVar {
                var,
                kind,
                rows,
                cols,
                comp,
                base,
            },
        );
        Ok(())
    }

    /// Initialise les sorties à zéro (valeurs non écrites déterministes).
    pub fn init_outputs(&mut self) {
        let mut keys: Vec<u32> = self.io.outputs.keys().copied().collect();
        keys.sort_unstable();
        for k in keys {
            let o = self.io.outputs[&k];
            match o.kind {
                OutKind::Loc => {
                    let elem = self.vty(o.comp, o.cols);
                    let t = if o.rows > 1 {
                        let len = self.c32(o.rows);
                        self.s.array(elem, len)
                    } else {
                        elem
                    };
                    let z = self.s.c_null(t);
                    self.s.emit_void(OP_STORE, &[o.var, z]);
                }
                OutKind::Position => {
                    let t = self.vty(St::F32, 4);
                    let z = self.s.c_null(t);
                    let pt = self.s.ptr(SC_OUTPUT, t);
                    let c0 = self.c32(0);
                    let p = self.s.emit(OP_ACCESS_CHAIN, pt, &[o.var, c0]);
                    self.s.emit_void(OP_STORE, &[p, z]);
                }
                _ => {}
            }
        }
    }

    // ------------------------------------------------------------------ lecture / écriture

    /// `loadInput(sigId, row, col, vertex)`.
    pub fn load_input(&mut self, args: &[u32], rt: St) -> R<RVal> {
        let Some(&sig) = args.get(1) else {
            return invalid("loadInput : arguments manquants");
        };
        let Some(id) = self.const_u(sig) else {
            return invalid("loadInput : identifiant de signature non constant");
        };
        let Some(&iv) = self.io.inputs.get(&(id as u32)) else {
            return invalid("loadInput : élément d'entrée inconnu");
        };
        let col = args.get(3).and_then(|&c| self.const_u(c)).unwrap_or(0) as u32;
        let cur = self.input_chain(iv, args.get(2).copied(), col)?;
        let sty = match iv.kind {
            InKind::FragCoord => St::F32,
            InKind::FrontFace => St::I1,
            InKind::SampleMask | InKind::ScalarBuiltin => St::I32,
            InKind::Loc => iv.comp,
        };
        let ty = self.sty(sty);
        let raw = self.s.emit(OP_LOAD, ty, &[cur]);
        let (v, vst) = if sty == St::I1 {
            // IsFrontFace : 0xFFFFFFFF si vrai (comme DXBC).
            let t = self.sty(St::I32);
            let ones = self.c32(u32::MAX);
            let z = self.c32(0);
            (self.s.emit(OP_SELECT, t, &[raw, ones, z]), St::I32)
        } else {
            (raw, sty)
        };
        self.convert_io(v, vst, rt)
    }

    fn input_chain(&mut self, iv: InVar, row: Option<u32>, col: u32) -> R<u32> {
        let sty = match iv.kind {
            InKind::FragCoord => St::F32,
            InKind::FrontFace => St::I1,
            InKind::SampleMask | InKind::ScalarBuiltin => St::I32,
            InKind::Loc => iv.comp,
        };
        let mut ops = vec![iv.var];
        match iv.kind {
            InKind::SampleMask => ops.push(self.c32(0)),
            InKind::Loc if iv.rows > 1 => {
                let r = match row {
                    Some(r) => self.scalar_u32(r)?,
                    None => self.c32(0),
                };
                ops.push(r);
            }
            _ => {}
        }
        let vector =
            matches!(iv.kind, InKind::FragCoord) || (iv.kind == InKind::Loc && iv.cols > 1);
        if vector {
            let c = if iv.kind == InKind::Loc {
                col.saturating_sub(self.start_col_of(iv))
            } else {
                col
            };
            ops.push(self.c32(c.min(3)));
        }
        if ops.len() == 1 {
            return Ok(iv.var);
        }
        let t = self.sty(sty);
        let pt = self.s.ptr(SC_INPUT, t);
        Ok(self.s.emit(OP_ACCESS_CHAIN, pt, &ops))
    }

    fn start_col_of(&self, iv: InVar) -> u32 {
        // Les variables empaquetées portent leur colonne de départ : retrouve l'élément.
        let _ = iv;
        self.io_start_col(iv.var)
    }

    fn io_start_col(&self, var: u32) -> u32 {
        for (idx, v) in &self.io.inputs {
            if v.var == var {
                return self
                    .info
                    .inputs
                    .get(*idx as usize)
                    .map(|e| e.start_col)
                    .unwrap_or(0);
            }
        }
        0
    }

    pub fn convert_io(&mut self, v: u32, from: St, to: St) -> R<RVal> {
        if from == to {
            return Ok(RVal::S(v, to));
        }
        let tt = self.sty(to);
        let id = match (from, to) {
            (St::F32, St::F16) | (St::F32, St::F64) => self.s.emit(OP_F_CONVERT, tt, &[v]),
            (St::I32, St::I16) | (St::I32, St::I64) => self.s.emit(OP_U_CONVERT, tt, &[v]),
            (St::F32, St::I32) | (St::I32, St::F32) => self.s.emit(OP_BITCAST, tt, &[v]),
            (St::I32, St::I1) => {
                let z = self.c32(0);
                self.s.emit(OP_I_NOT_EQUAL, tt, &[v, z])
            }
            (St::I32, St::I8) => v,
            (St::F32, St::I16) => {
                let t32 = self.sty(St::I32);
                let b = self.s.emit(OP_BITCAST, t32, &[v]);
                self.s.emit(OP_U_CONVERT, tt, &[b])
            }
            (St::I32, St::F16) => {
                let t32 = self.sty(St::F32);
                let b = self.s.emit(OP_BITCAST, t32, &[v]);
                self.s.emit(OP_F_CONVERT, tt, &[b])
            }
            _ => return unsup(format!("conversion d'interface {from:?} -> {to:?}")),
        };
        Ok(RVal::S(id, to))
    }

    /// `storeOutput(sigId, row, col, value)`.
    pub fn store_output(&mut self, args: &[u32]) -> R<()> {
        if args.len() < 5 {
            return invalid("storeOutput : arguments manquants");
        }
        let Some(id) = self.const_u(args[1]) else {
            return invalid("storeOutput : identifiant de signature non constant");
        };
        let Some(&ov) = self.io.outputs.get(&(id as u32)) else {
            return invalid("storeOutput : élément de sortie inconnu");
        };
        let col = self.const_u(args[3]).unwrap_or(0) as u32;
        let (vid, vst) = self.scalar(args[4])?;
        let target_st = match ov.kind {
            OutKind::Scalar | OutKind::Coverage => ov.comp,
            _ => ov.comp,
        };
        // Conversion vers le type de composante de la variable (32 bits).
        let conv = match (vst, target_st) {
            (a, b) if a == b => vid,
            (St::F16, St::F32) | (St::F64, St::F32) => {
                let t = self.sty(St::F32);
                self.s.emit(OP_F_CONVERT, t, &[vid])
            }
            (St::I16, St::I32) | (St::I64, St::I32) => {
                let t = self.sty(St::I32);
                self.s.emit(OP_U_CONVERT, t, &[vid])
            }
            (St::F32, St::I32) | (St::I32, St::F32) => {
                let t = self.sty(target_st);
                self.s.emit(OP_BITCAST, t, &[vid])
            }
            (St::I1, St::I32) => {
                let t = self.sty(St::I32);
                let one = self.c32(1);
                let z = self.c32(0);
                self.s.emit(OP_SELECT, t, &[vid, one, z])
            }
            (St::F16, St::I32) => {
                let f = self.sty(St::F32);
                let c = self.s.emit(OP_F_CONVERT, f, &[vid]);
                let t = self.sty(St::I32);
                self.s.emit(OP_BITCAST, t, &[c])
            }
            (St::I16, St::F32) => {
                let u = self.sty(St::I32);
                let c = self.s.emit(OP_U_CONVERT, u, &[vid]);
                let t = self.sty(St::F32);
                self.s.emit(OP_BITCAST, t, &[c])
            }
            _ => return unsup(format!("conversion de sortie {vst:?} -> {target_st:?}")),
        };
        let ct = self.sty(target_st);
        match ov.kind {
            OutKind::Position => {
                let pt = self.s.ptr(SC_OUTPUT, ct);
                let m0 = self.c32(0);
                let c = self.c32(col.min(3));
                let p = self.s.emit(OP_ACCESS_CHAIN, pt, &[ov.var, m0, c]);
                self.s.emit_void(OP_STORE, &[p, conv]);
            }
            OutKind::Clip | OutKind::Cull => {
                let row = self.const_u(args[2]).unwrap_or(0) as u32;
                let member = if ov.kind == OutKind::Clip {
                    1
                } else if self.io.clip_n > 0 {
                    2
                } else {
                    1
                };
                let pt = self.s.ptr(SC_OUTPUT, ct);
                let m = self.c32(member);
                let k = self.c32(ov.base + row * ov.cols + col);
                let p = self.s.emit(OP_ACCESS_CHAIN, pt, &[ov.var, m, k]);
                self.s.emit_void(OP_STORE, &[p, conv]);
            }
            OutKind::Scalar => {
                self.s.emit_void(OP_STORE, &[ov.var, conv]);
            }
            OutKind::Coverage => {
                let pt = self.s.ptr(SC_OUTPUT, ct);
                let z = self.c32(0);
                let p = self.s.emit(OP_ACCESS_CHAIN, pt, &[ov.var, z]);
                self.s.emit_void(OP_STORE, &[p, conv]);
            }
            OutKind::Loc => {
                let mut ops = vec![ov.var];
                if ov.rows > 1 {
                    let r = self.scalar_u32(args[2])?;
                    ops.push(r);
                }
                let start = self
                    .info
                    .outputs
                    .get(id as usize)
                    .map(|e| e.start_col)
                    .unwrap_or(0);
                if ov.cols > 1 {
                    let c = if self.stage == ShaderStage::Pixel {
                        col
                    } else {
                        col.saturating_sub(start)
                    };
                    ops.push(self.c32(c.min(3)));
                }
                if ops.len() == 1 {
                    self.s.emit_void(OP_STORE, &[ov.var, conv]);
                } else {
                    let pt = self.s.ptr(SC_OUTPUT, ct);
                    let p = self.s.emit(OP_ACCESS_CHAIN, pt, &ops);
                    self.s.emit_void(OP_STORE, &[p, conv]);
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ built-ins compute / wave

    /// Variable d'entrée built-in (créée à la demande, ajoutée à l'interface).
    pub fn builtin_input(&mut self, builtin: u32, vec3: bool) -> u32 {
        if let Some(&v) = self.io.builtins.get(&builtin) {
            return v;
        }
        let t = self.vty(St::I32, if vec3 { 3 } else { 1 });
        let ptr = self.s.ptr(SC_INPUT, t);
        let var = self.s.global_var(ptr, SC_INPUT, None);
        self.s.decorate(var, DEC_BUILTIN, &[builtin]);
        if self.stage == ShaderStage::Pixel && builtin != BI_FRAG_COORD {
            self.s.decorate(var, DEC_FLAT, &[]);
        }
        if matches!(builtin, BI_SUBGROUP_SIZE | BI_SUBGROUP_LOCAL_INVOCATION_ID) {
            self.s.caps.insert(CAP_GROUP_NON_UNIFORM);
        }
        self.io.interface.push(var);
        self.io.builtins.insert(builtin, var);
        var
    }

    pub fn load_builtin_comp(&mut self, builtin: u32, comp: Option<u32>) -> u32 {
        let var = self.builtin_input(builtin, comp.is_some());
        let u = self.sty(St::I32);
        match comp {
            Some(c) => {
                let pt = self.s.ptr(SC_INPUT, u);
                let k = self.c32(c.min(2));
                let p = self.s.emit(OP_ACCESS_CHAIN, pt, &[var, k]);
                self.s.emit(OP_LOAD, u, &[p])
            }
            None => self.s.emit(OP_LOAD, u, &[var]),
        }
    }
}
