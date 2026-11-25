//! Traduction des appels `dx.op.*` (opcode = premier argument constant).

use super::dxmeta::rk;
use super::module::{TyId, ValId};
use super::opnames::op_name;
use super::res::{ImgInfo, dim_of_kind};
use super::sp::*;
use super::tr::{Handle, R, RVal, Slot, Tr, invalid, st_of_ty, unsup};
use crate::dxbc_to_spirv::ShaderTranslateError;
use crate::dxbc_to_spirv::spv::Sc;
use crate::shader::ShaderStage;

const MEM_ACQ_REL: u32 = 8;
const MEM_UNIFORM: u32 = 0x40;
const MEM_WORKGROUP: u32 = 0x100;
const MEM_IMAGE: u32 = 0x800;

impl Tr<'_> {
    fn ia(&self, args: &[ValId], i: usize) -> Option<u64> {
        args.get(i).and_then(|&a| self.const_u(a))
    }

    fn ga(&self, args: &[ValId], i: usize) -> R<ValId> {
        match args.get(i) {
            Some(&a) => Ok(a),
            None => invalid("argument d'opération DXIL manquant"),
        }
    }

    fn arg(&mut self, args: &[ValId], i: usize) -> R<(u32, St)> {
        match args.get(i) {
            Some(&a) => self.scalar(a),
            None => invalid("argument d'opération DXIL manquant"),
        }
    }

    fn glsl(&mut self, st: St, inst: u32, a: &[u32]) -> RVal {
        let t = self.sty(st);
        let mut ops = vec![self.s.glsl, inst];
        ops.extend_from_slice(a);
        RVal::S(self.s.emit(OP_EXT_INST, t, &ops), st)
    }

    fn un(&mut self, op: u16, st: St, a: &[u32]) -> RVal {
        let t = self.sty(st);
        RVal::S(self.s.emit(op, t, a), st)
    }

    fn fconst(&mut self, st: St, v: f64) -> u32 {
        let bits = match st {
            St::F32 => u64::from((v as f32).to_bits()),
            St::F64 => v.to_bits(),
            _ => f16_bits(v as f32),
        };
        self.cst(st, bits)
    }

    fn vec_of(&mut self, st: St, ids: &[u32]) -> u32 {
        if ids.len() == 1 {
            return ids[0];
        }
        let t = self.vty(st, ids.len() as u32);
        self.s.emit(OP_COMPOSITE_CONSTRUCT, t, ids)
    }

    /// `ResRet` : composants + statut (0xFFFFFFFF = entièrement mappé).
    fn resret(&mut self, comps: &[RVal], rt: Option<TyId>) -> R<RVal> {
        let n = match rt.map(|t| self.m.ty(t).clone()) {
            Some(super::module::Ty::Struct(f)) => f.len(),
            _ => 5,
        };
        let est = self.st_from_resret(rt).unwrap_or(St::F32);
        let mut out: Vec<RVal> = comps.to_vec();
        while out.len() < n.saturating_sub(1).max(comps.len()) {
            let z = self.cst(est, 0);
            out.push(RVal::S(z, est));
        }
        if n > comps.len() {
            let st = self.c32(u32::MAX);
            out.push(RVal::S(st, St::I32));
        }
        Ok(RVal::Agg(out))
    }

    pub fn dx_op(&mut self, name: &str, rt: Option<TyId>, args: &[ValId]) -> R<Option<RVal>> {
        let _ = name;
        let Some(op) = self.ia(args, 0) else {
            return invalid("opcode dx.op non constant");
        };
        let op = op as u32;
        let rst = rt.and_then(|t| st_of_ty(self.m, t).ok());
        let r = match op {
            4 => {
                let st = rst.unwrap_or(St::F32);
                let ids: Vec<u32> = args.to_vec();
                Some(self.load_input(&ids, st)?)
            }
            5 => {
                self.store_output(args)?;
                None
            }
            // ---- unaires flottants
            6 | 12..=29 => {
                let (a, st) = self.arg(args, 1)?;
                let g = match op {
                    6 => G_FABS,
                    12 => G_COS,
                    13 => G_SIN,
                    14 => G_TAN,
                    15 => G_ACOS,
                    16 => G_ASIN,
                    17 => G_ATAN,
                    18 => G_COSH,
                    19 => G_SINH,
                    20 => G_TANH,
                    21 => G_EXP2,
                    22 => G_FRACT,
                    23 => G_LOG2,
                    24 => G_SQRT,
                    25 => G_INVERSE_SQRT,
                    26 => G_ROUND_EVEN,
                    27 => G_FLOOR,
                    28 => G_CEIL,
                    _ => G_TRUNC,
                };
                Some(self.glsl(st, g, &[a]))
            }
            7 => {
                let (a, st) = self.arg(args, 1)?;
                let z = self.fconst(st, 0.0);
                let o = self.fconst(st, 1.0);
                Some(self.glsl(st, G_FCLAMP, &[a, z, o]))
            }
            8 | 9 => {
                let (a, _) = self.arg(args, 1)?;
                Some(self.un(if op == 8 { OP_IS_NAN } else { OP_IS_INF }, St::I1, &[a]))
            }
            10 => {
                let (a, _) = self.arg(args, 1)?;
                let n = self.un(OP_IS_NAN, St::I1, &[a]);
                let i = self.un(OP_IS_INF, St::I1, &[a]);
                let (RVal::S(n, _), RVal::S(i, _)) = (n, i) else {
                    return invalid("isfinite");
                };
                let o = self.un(OP_LOGICAL_OR, St::I1, &[n, i]);
                let RVal::S(o, _) = o else {
                    return invalid("isfinite");
                };
                Some(self.un(OP_LOGICAL_NOT, St::I1, &[o]))
            }
            30 => {
                let (a, st) = self.arg(args, 1)?;
                Some(self.un(OP_BIT_REVERSE, st, &[a]))
            }
            31 => {
                let (a, _) = self.arg(args, 1)?;
                Some(self.un(OP_BIT_COUNT, St::I32, &[a]))
            }
            32 => {
                let (a, st) = self.arg(args, 1)?;
                if st != St::I32 {
                    return unsup("firstbitlow sur un entier non 32 bits");
                }
                Some(self.glsl(St::I32, G_FIND_I_LSB, &[a]))
            }
            33 | 34 => {
                let (a, st) = self.arg(args, 1)?;
                if st != St::I32 {
                    return unsup("firstbithigh sur un entier non 32 bits");
                }
                let msb = self.glsl(
                    St::I32,
                    if op == 33 { G_FIND_U_MSB } else { G_FIND_S_MSB },
                    &[a],
                );
                let RVal::S(msb, _) = msb else {
                    return invalid("firstbit");
                };
                let c31 = self.c32(31);
                let neg1 = self.c32(u32::MAX);
                let from_top = self.un(OP_I_SUB, St::I32, &[c31, msb]);
                let RVal::S(ft, _) = from_top else {
                    return invalid("firstbit");
                };
                let none = self.un(OP_I_EQUAL, St::I1, &[msb, neg1]);
                let RVal::S(none, _) = none else {
                    return invalid("firstbit");
                };
                Some(self.un(OP_SELECT, St::I32, &[none, neg1, ft]))
            }
            35 | 36 => {
                let (a, st) = self.arg(args, 1)?;
                let (b, _) = self.arg(args, 2)?;
                Some(self.glsl(st, if op == 35 { G_NMAX } else { G_NMIN }, &[a, b]))
            }
            37..=40 => {
                let (a, st) = self.arg(args, 1)?;
                let (b, _) = self.arg(args, 2)?;
                let g = match op {
                    37 => G_SMAX,
                    38 => G_SMIN,
                    39 => G_UMAX,
                    _ => G_UMIN,
                };
                Some(self.glsl(st, g, &[a, b]))
            }
            41 | 42 => {
                let (a, st) = self.arg(args, 1)?;
                let (b, _) = self.arg(args, 2)?;
                let t = self.sty(st);
                let sty = self.s.structure(&[t, t]);
                let r = self.s.emit(
                    if op == 41 {
                        OP_S_MUL_EXTENDED
                    } else {
                        OP_U_MUL_EXTENDED
                    },
                    sty,
                    &[a, b],
                );
                let lo = self.s.emit(OP_COMPOSITE_EXTRACT, t, &[r, 0]);
                let hi = self.s.emit(OP_COMPOSITE_EXTRACT, t, &[r, 1]);
                Some(RVal::Agg(vec![RVal::S(hi, st), RVal::S(lo, st)]))
            }
            43 => {
                let (a, st) = self.arg(args, 1)?;
                let (b, _) = self.arg(args, 2)?;
                let q = self.un(OP_U_DIV, st, &[a, b]);
                let r = self.un(OP_U_MOD, st, &[a, b]);
                Some(RVal::Agg(vec![q, r]))
            }
            44 | 45 => {
                let (a, st) = self.arg(args, 1)?;
                let (b, _) = self.arg(args, 2)?;
                let t = self.sty(st);
                let sty = self.s.structure(&[t, t]);
                let r = self.s.emit(
                    if op == 44 {
                        OP_I_ADD_CARRY
                    } else {
                        OP_I_SUB_BORROW
                    },
                    sty,
                    &[a, b],
                );
                let v = self.s.emit(OP_COMPOSITE_EXTRACT, t, &[r, 0]);
                let c = self.s.emit(OP_COMPOSITE_EXTRACT, t, &[r, 1]);
                let z = self.cst(st, 0);
                let bt = self.sty(St::I1);
                let cb = self.s.emit(OP_I_NOT_EQUAL, bt, &[c, z]);
                Some(RVal::Agg(vec![RVal::S(v, st), RVal::S(cb, St::I1)]))
            }
            46 | 48 | 49 => {
                let (a, st) = self.arg(args, 1)?;
                let (b, _) = self.arg(args, 2)?;
                let (c, _) = self.arg(args, 3)?;
                let (m, add) = if op == 46 {
                    (OP_F_MUL, OP_F_ADD)
                } else {
                    (OP_I_MUL, OP_I_ADD)
                };
                let RVal::S(p, _) = self.un(m, st, &[a, b]) else {
                    return invalid("mad");
                };
                Some(self.un(add, st, &[p, c]))
            }
            47 => {
                let (a, st) = self.arg(args, 1)?;
                let (b, _) = self.arg(args, 2)?;
                let (c, _) = self.arg(args, 3)?;
                Some(self.glsl(st, G_FMA, &[a, b, c]))
            }
            51 | 52 => {
                let (w, st) = self.arg(args, 1)?;
                let (o, _) = self.arg(args, 2)?;
                let (v, _) = self.arg(args, 3)?;
                let m = self.c32(31);
                let w = self.un(OP_BITWISE_AND, st, &[w, m]);
                let o = self.un(OP_BITWISE_AND, st, &[o, m]);
                let (RVal::S(w, _), RVal::S(o, _)) = (w, o) else {
                    return invalid("bfe");
                };
                Some(self.un(
                    if op == 51 {
                        OP_BIT_FIELD_S_EXTRACT
                    } else {
                        OP_BIT_FIELD_U_EXTRACT
                    },
                    st,
                    &[v, o, w],
                ))
            }
            53 => {
                let (w, st) = self.arg(args, 1)?;
                let (o, _) = self.arg(args, 2)?;
                let (v, _) = self.arg(args, 3)?;
                let (base, _) = self.arg(args, 4)?;
                let m = self.c32(31);
                let w = self.un(OP_BITWISE_AND, st, &[w, m]);
                let o = self.un(OP_BITWISE_AND, st, &[o, m]);
                let (RVal::S(w, _), RVal::S(o, _)) = (w, o) else {
                    return invalid("bfi");
                };
                Some(self.un(OP_BIT_FIELD_INSERT, st, &[base, v, o, w]))
            }
            54..=56 => {
                let n = (op - 52) as usize;
                let mut a = Vec::new();
                let mut b = Vec::new();
                let mut st = St::F32;
                for i in 0..n {
                    let (x, s) = self.arg(args, 1 + i)?;
                    st = s;
                    a.push(x);
                    let (y, _) = self.arg(args, 1 + n + i)?;
                    b.push(y);
                }
                let va = self.vec_of(st, &a);
                let vb = self.vec_of(st, &b);
                Some(self.un(OP_DOT, st, &[va, vb]))
            }
            // ---- ressources
            57 => Some(self.op_create_handle(args)?),
            216 => Some(self.op_annotate_handle(args)?),
            217 => Some(self.op_create_handle_binding(args)?),
            218 => Some(self.op_create_handle_heap(args)?),
            58 | 59 => Some(self.op_cbuffer_load(op, rt, args)?),
            60..=65 => Some(self.op_sample(op, args, rt)?),
            66 => Some(self.op_texture_load(args, rt)?),
            67 => {
                self.op_texture_store(args)?;
                None
            }
            68 | 139 => Some(self.op_buffer_load(op, args, rt)?),
            69 | 140 => {
                self.op_buffer_store(op, args)?;
                None
            }
            71 => {
                let (a, _) = self.arg(args, 1)?;
                let z = self.c32(0);
                Some(self.un(OP_I_NOT_EQUAL, St::I1, &[a, z]))
            }
            72 => Some(self.op_get_dimensions(args)?),
            73 | 74 => Some(self.op_gather(op, args, rt)?),
            78 | 79 => Some(self.op_atomic(op, args)?),
            80 => {
                self.op_barrier(args)?;
                None
            }
            81 => Some(self.op_calc_lod(args)?),
            82 => {
                self.op_discard(args)?;
                None
            }
            83..=86 => {
                let (a, st) = self.arg(args, 1)?;
                self.s.caps.insert(CAP_DERIVATIVE_CONTROL);
                let o = match op {
                    83 => OP_DPDX_COARSE,
                    84 => OP_DPDY_COARSE,
                    85 => OP_DPDX_FINE,
                    _ => OP_DPDY_FINE,
                };
                Some(self.un(o, st, &[a]))
            }
            87..=89 => Some(self.op_eval(op, args, rst.unwrap_or(St::F32))?),
            90 => {
                self.s.caps.insert(CAP_SAMPLE_RATE_SHADING);
                let v = self.load_builtin_comp(BI_SAMPLE_ID, None);
                Some(RVal::S(v, St::I32))
            }
            91 => {
                let v = self.sample_mask_in();
                Some(RVal::S(v, St::I32))
            }
            93..=95 => {
                let comp = self.ia(args, 1).unwrap_or(0) as u32;
                let b = match op {
                    93 => BI_GLOBAL_INVOCATION_ID,
                    94 => BI_WORKGROUP_ID,
                    _ => BI_LOCAL_INVOCATION_ID,
                };
                Some(RVal::S(self.load_builtin_comp(b, Some(comp)), St::I32))
            }
            96 => Some(RVal::S(
                self.load_builtin_comp(BI_LOCAL_INVOCATION_INDEX, None),
                St::I32,
            )),
            101 => {
                let (lo, _) = self.arg(args, 1)?;
                let (hi, _) = self.arg(args, 2)?;
                let v2 = self.vec_of(St::I32, &[lo, hi]);
                Some(self.un(OP_BITCAST, St::F64, &[v2]))
            }
            102 => {
                let (d, _) = self.arg(args, 1)?;
                let t = self.vty(St::I32, 2);
                let v2 = self.s.emit(OP_BITCAST, t, &[d]);
                let u = self.sty(St::I32);
                let lo = self.s.emit(OP_COMPOSITE_EXTRACT, u, &[v2, 0]);
                let hi = self.s.emit(OP_COMPOSITE_EXTRACT, u, &[v2, 1]);
                Some(RVal::Agg(vec![RVal::S(lo, St::I32), RVal::S(hi, St::I32)]))
            }
            108 => {
                self.s.caps.insert(CAP_GEOMETRY);
                Some(RVal::S(
                    self.load_builtin_comp(BI_PRIMITIVE_ID, None),
                    St::I32,
                ))
            }
            110..=123 | 135 | 136 => self.op_wave(op, args, rt)?,
            124..=129 => {
                let (a, _) = self.arg(args, 1)?;
                let to = rst.unwrap_or(St::I32);
                Some(self.un(OP_BITCAST, to, &[a]))
            }
            130 => {
                let (a, _) = self.arg(args, 1)?;
                let z = self.fconst(St::F32, 0.0);
                let v2 = self.vec_of(St::F32, &[a, z]);
                Some(self.glsl(St::I32, G_PACK_HALF_2X16, &[v2]))
            }
            131 => {
                let (a, _) = self.arg(args, 1)?;
                let v2 = self.glsl_vec(St::F32, 2, G_UNPACK_HALF_2X16, &[a]);
                let f = self.sty(St::F32);
                Some(RVal::S(
                    self.s.emit(OP_COMPOSITE_EXTRACT, f, &[v2, 0]),
                    St::F32,
                ))
            }
            132 => {
                let (a, _) = self.arg(args, 1)?;
                Some(self.un(OP_F_CONVERT, St::F32, &[a]))
            }
            133 | 134 => {
                let (a, _) = self.arg(args, 1)?;
                Some(self.un(
                    if op == 133 {
                        OP_CONVERT_F_TO_S
                    } else {
                        OP_CONVERT_F_TO_U
                    },
                    St::I32,
                    &[a],
                ))
            }
            137 => Some(self.op_attribute_at_vertex(args, rst.unwrap_or(St::F32))?),
            _ => {
                return Err(ShaderTranslateError::UnsupportedDxilOp {
                    opcode: op,
                    name: op_name(op),
                });
            }
        };
        Ok(r)
    }

    fn glsl_vec(&mut self, st: St, n: u32, inst: u32, a: &[u32]) -> u32 {
        let t = self.vty(st, n);
        let mut ops = vec![self.s.glsl, inst];
        ops.extend_from_slice(a);
        self.s.emit(OP_EXT_INST, t, &ops)
    }

    fn sample_mask_in(&mut self) -> u32 {
        // Entrée `SampleMask[1]` (flat).
        if let Some(&v) = self.io.builtins.get(&BI_SAMPLE_MASK) {
            let u = self.sty(St::I32);
            let pt = self.s.ptr(SC_INPUT, u);
            let z = self.c32(0);
            let p = self.s.emit(OP_ACCESS_CHAIN, pt, &[v, z]);
            return self.s.emit(OP_LOAD, u, &[p]);
        }
        let u = self.sty(St::I32);
        let len = self.c32(1);
        let arr = self.s.array(u, len);
        let ptr = self.s.ptr(SC_INPUT, arr);
        let var = self.s.global_var(ptr, SC_INPUT, None);
        self.s.decorate(var, DEC_BUILTIN, &[BI_SAMPLE_MASK]);
        self.s.decorate(var, DEC_FLAT, &[]);
        self.io.interface.push(var);
        self.io.builtins.insert(BI_SAMPLE_MASK, var);
        self.sample_mask_in()
    }

    // ------------------------------------------------------------------ cbuffer

    fn op_cbuffer_load(&mut self, op: u32, rt: Option<TyId>, args: &[ValId]) -> R<RVal> {
        let h = self.handle(*args.get(1).unwrap_or(&0))?;
        if op == 59 {
            let (row, _) = self.arg(args, 2)?;
            let row = self.to_u32(row, St::I32)?;
            let v = self.cbuffer_row(&h, row)?;
            // Type d'élément : champs de CBufRet.
            let est = match rt.map(|t| self.m.ty(t).clone()) {
                Some(super::module::Ty::Struct(f)) if !f.is_empty() => st_of_ty(self.m, f[0])?,
                _ => St::F32,
            };
            let n = match rt.map(|t| self.m.ty(t).clone()) {
                Some(super::module::Ty::Struct(f)) => f.len() as u32,
                _ => 4,
            };
            let u = self.sty(St::I32);
            let mut comps = Vec::new();
            match est.bits() {
                32 => {
                    for i in 0..n {
                        let w = self.s.emit(OP_COMPOSITE_EXTRACT, u, &[v, i]);
                        comps.push(self.word_to(w, est)?);
                    }
                }
                64 => {
                    for i in 0..n {
                        let w = self.vec_of_words(v, &[2 * i, 2 * i + 1]);
                        comps.push(self.un(OP_BITCAST, est, &[w]));
                    }
                }
                _ => {
                    for i in 0..n {
                        let w = self.s.emit(OP_COMPOSITE_EXTRACT, u, &[v, i / 2]);
                        comps.push(self.half_of_word(w, i % 2, est)?);
                    }
                }
            }
            return Ok(RVal::Agg(comps));
        }
        // CBufferLoad (non legacy) : (handle, byteOffset, alignment)
        let (off, _) = self.arg(args, 2)?;
        let est = match rt {
            Some(t) => st_of_ty(self.m, t)?,
            None => St::I32,
        };
        let off = self.to_u32(off, St::I32)?;
        let u = self.sty(St::I32);
        let c4 = self.c32(4);
        let c2 = self.c32(2);
        let c3 = self.c32(3);
        let word = self.s.emit(OP_SHIFT_RIGHT_LOGICAL, u, &[off, c2]);
        let row = self.s.emit(OP_SHIFT_RIGHT_LOGICAL, u, &[word, c2]);
        let comp = self.s.emit(OP_BITWISE_AND, u, &[word, c3]);
        let _ = c4;
        let v = self.cbuffer_row(&h, row)?;
        let w = self.s.emit(OP_VECTOR_EXTRACT_DYN, u, &[v, comp]);
        match est.bits() {
            32 => self.word_to(w, est),
            16 => {
                let one = self.c32(1);
                let hi = self.s.emit(OP_BITWISE_AND, u, &[off, c2]);
                let _ = (one, hi);
                unsup("cbufferLoad 16 bits non legacy")
            }
            _ => unsup("cbufferLoad 64 bits non legacy"),
        }
    }

    fn vec_of_words(&mut self, v: u32, idx: &[u32]) -> u32 {
        let u = self.sty(St::I32);
        let ids: Vec<u32> = idx
            .iter()
            .map(|&i| self.s.emit(OP_COMPOSITE_EXTRACT, u, &[v, i]))
            .collect();
        self.vec_of(St::I32, &ids)
    }

    /// Mot 32 bits -> valeur 32 bits typée (float/int).
    fn word_to(&mut self, w: u32, st: St) -> R<RVal> {
        Ok(match st {
            St::F32 => self.un(OP_BITCAST, St::F32, &[w]),
            St::I1 => {
                let z = self.c32(0);
                self.un(OP_I_NOT_EQUAL, St::I1, &[w, z])
            }
            _ => RVal::S(w, if st == St::I8 { St::I8 } else { St::I32 }),
        })
    }

    fn half_of_word(&mut self, w: u32, which: u32, st: St) -> R<RVal> {
        let u = self.sty(St::I32);
        let sh = self.c32(16 * which);
        let shifted = self.s.emit(OP_SHIFT_RIGHT_LOGICAL, u, &[w, sh]);
        let m = self.c32(0xFFFF);
        let masked = self.s.emit(OP_BITWISE_AND, u, &[shifted, m]);
        let i16t = self.sty(St::I16);
        let h = self.s.emit(OP_U_CONVERT, i16t, &[masked]);
        Ok(if st == St::F16 {
            self.un(OP_BITCAST, St::F16, &[h])
        } else {
            RVal::S(h, St::I16)
        })
    }

    // ------------------------------------------------------------------ échantillonnage

    fn coords(&mut self, args: &[ValId], first: usize, n: u32, st: St) -> R<u32> {
        let mut ids = Vec::new();
        for i in 0..n as usize {
            let (v, vst) = self.arg(args, first + i)?;
            let _ = (vst, st);
            ids.push(v);
        }
        Ok(self.vec_of(st, &ids))
    }

    fn offsets(&mut self, args: &[ValId], first: usize, n: u32) -> R<Option<(bool, u32)>> {
        if n == 0 {
            return Ok(None);
        }
        let mut consts = Vec::new();
        let mut all_const = true;
        for i in 0..n as usize {
            match args.get(first + i) {
                Some(&a) if self.is_undef(a) => consts.push(Some(0u64)),
                Some(&a) => {
                    let c = self.const_u(a);
                    if c.is_none() {
                        all_const = false;
                    }
                    consts.push(c);
                }
                None => consts.push(Some(0)),
            }
        }
        if all_const {
            if consts.iter().all(|c| *c == Some(0)) {
                return Ok(None);
            }
            let bits: Vec<u32> = consts.iter().map(|c| c.unwrap_or(0) as u32).collect();
            let v = self.s.c_vec(Sc::I32, &bits);
            return Ok(Some((true, v.id)));
        }
        self.s.caps.insert(CAP_IMAGE_GATHER_EXTENDED);
        let mut ids = Vec::new();
        for i in 0..n as usize {
            ids.push(self.scalar_u32(self.ga(args, first + i)?)?);
        }
        Ok(Some((false, self.vec_of(St::I32, &ids))))
    }

    /// Convertit un vec4 chargé (type d'échantillon `sc`) en composants du type demandé.
    fn unpack4(&mut self, v: u32, sc: Sc, est: St, n: u32) -> R<Vec<RVal>> {
        let mut out = Vec::new();
        for i in 0..n {
            let f = match sc {
                Sc::F32 => {
                    let t = self.sty(St::F32);
                    RVal::S(self.s.emit(OP_COMPOSITE_EXTRACT, t, &[v, i]), St::F32)
                }
                Sc::I32 => {
                    let ts = self.s.ty(Sc::I32, 1);
                    let e = self.s.emit(OP_COMPOSITE_EXTRACT, ts, &[v, i]);
                    let u = self.sty(St::I32);
                    RVal::S(self.s.emit(OP_BITCAST, u, &[e]), St::I32)
                }
                _ => {
                    let t = self.sty(St::I32);
                    RVal::S(self.s.emit(OP_COMPOSITE_EXTRACT, t, &[v, i]), St::I32)
                }
            };
            let RVal::S(id, from) = f else {
                return invalid("unpack4");
            };
            out.push(self.convert_io(id, from, est)?);
        }
        Ok(out)
    }

    fn sampled_image(&mut self, h: &Handle, hs: &Handle) -> R<(u32, ImgInfo)> {
        let (img, info) = self.image_value(h, false)?;
        let smp = self.sampler_value(hs)?;
        let si_ty = self.s.raw_type(27, &[info.ty]);
        let si = self.s.emit(OP_SAMPLED_IMAGE, si_ty, &[img, smp]);
        Ok((si, info))
    }

    fn op_sample(&mut self, opc: u32, args: &[ValId], rt: Option<TyId>) -> R<RVal> {
        let h = self.handle(*args.get(1).unwrap_or(&0))?;
        let hs = self.handle(*args.get(2).unwrap_or(&0))?;
        let (si, info) = self.sampled_image(&h, &hs)?;
        let dim = info.dim;
        let coord = self.coords(args, 3, dim.coords, St::F32)?;
        let pixel = self.stage == ShaderStage::Pixel;
        let implicit = pixel && matches!(opc, 60 | 61 | 64);
        let est = self.st_from_resret(rt)?;
        let mut mask = 0u32;
        let mut words: Vec<u32> = Vec::new();
        let mut dref: Option<u32> = None;
        let off = self.offsets(args, 7, dim.offs)?;
        match opc {
            61 if pixel => {
                let (b, _) = self.arg(args, 10)?;
                mask |= 1;
                words.push(b);
            }
            62 => {
                let (l, _) = self.arg(args, 10)?;
                mask |= 2;
                words.push(l);
            }
            63 => {
                let g = dim.coords - u32::from(dim.arrayed);
                let dx = self.coords(args, 10, g, St::F32)?;
                let dy = self.coords(args, 13, g, St::F32)?;
                mask |= 4;
                words.push(dx);
                words.push(dy);
            }
            64 | 65 => {
                let (d, _) = self.arg(args, 10)?;
                dref = Some(d);
                if opc == 65 {
                    mask |= 2;
                    words.push(self.fconst(St::F32, 0.0));
                }
            }
            _ => {}
        }
        if !implicit && mask == 0 {
            mask |= 2;
            words.push(self.fconst(St::F32, 0.0));
        }
        if let Some((is_const, o)) = off {
            mask |= if is_const { 8 } else { 0x10 };
            words.push(o);
        }
        let mut ops = vec![si, coord];
        let (spop, rty, rn) = if let Some(d) = dref {
            ops.push(d);
            (
                if implicit {
                    OP_IMAGE_SAMPLE_DREF_IMPLICIT_LOD
                } else {
                    OP_IMAGE_SAMPLE_DREF_EXPLICIT_LOD
                },
                St::F32,
                1,
            )
        } else {
            (
                if implicit {
                    OP_IMAGE_SAMPLE_IMPLICIT_LOD
                } else {
                    OP_IMAGE_SAMPLE_EXPLICIT_LOD
                },
                St::F32,
                4,
            )
        };
        if mask != 0 {
            ops.push(mask);
            ops.extend_from_slice(&words);
        }
        let comps = if dref.is_some() {
            let t = self.sty(rty);
            let r = self.s.emit(spop, t, &ops);
            let c = self.convert_io(r, St::F32, est)?;
            vec![c.clone(), c.clone(), c.clone(), c]
        } else {
            let t = self.sampled_vec_ty(info.sc);
            let r = self.s.emit(spop, t, &ops);
            let _ = rn;
            self.unpack4(r, info.sc, est, 4)?
        };
        self.resret(&comps, rt)
    }

    fn sampled_vec_ty(&mut self, sc: Sc) -> u32 {
        self.s.ty(sc, 4)
    }

    fn op_gather(&mut self, opc: u32, args: &[ValId], rt: Option<TyId>) -> R<RVal> {
        let h = self.handle(*args.get(1).unwrap_or(&0))?;
        let hs = self.handle(*args.get(2).unwrap_or(&0))?;
        let (si, info) = self.sampled_image(&h, &hs)?;
        let dim = info.dim;
        let coord = self.coords(args, 3, dim.coords, St::F32)?;
        let est = self.st_from_resret(rt)?;
        let off = if dim.offs > 0 {
            self.offsets(args, 7, 2)?
        } else {
            None
        };
        let channel = self.ia(args, 9).unwrap_or(0) as u32;
        let mut ops = vec![si, coord];
        let spop = if opc == 74 {
            let (d, _) = self.arg(args, 10)?;
            ops.push(d);
            OP_IMAGE_DREF_GATHER
        } else {
            let c = self.c32(channel);
            ops.push(c);
            OP_IMAGE_GATHER
        };
        if let Some((is_const, o)) = off {
            ops.push(if is_const { 8 } else { 0x10 });
            ops.push(o);
        }
        let t = self.sampled_vec_ty(info.sc);
        let r = self.s.emit(spop, t, &ops);
        let comps = self.unpack4(r, info.sc, est, 4)?;
        self.resret(&comps, rt)
    }

    fn op_calc_lod(&mut self, args: &[ValId]) -> R<RVal> {
        let h = self.handle(*args.get(1).unwrap_or(&0))?;
        let hs = self.handle(*args.get(2).unwrap_or(&0))?;
        let (si, info) = self.sampled_image(&h, &hs)?;
        self.s.caps.insert(CAP_IMAGE_QUERY);
        let n = info.dim.coords - u32::from(info.dim.arrayed);
        let coord = self.coords(args, 3, n, St::F32)?;
        let t = self.vty(St::F32, 2);
        let r = self.s.emit(OP_IMAGE_QUERY_LOD, t, &[si, coord]);
        let clamped = self.ia(args, 6).unwrap_or(1) != 0;
        let f = self.sty(St::F32);
        let v = self
            .s
            .emit(OP_COMPOSITE_EXTRACT, f, &[r, if clamped { 0 } else { 1 }]);
        Ok(RVal::S(v, St::F32))
    }

    // ------------------------------------------------------------------ chargements / stockages

    fn op_texture_load(&mut self, args: &[ValId], rt: Option<TyId>) -> R<RVal> {
        let h = self.handle(*args.get(1).unwrap_or(&0))?;
        let est = self.st_from_resret(rt)?;
        let uav = h.class == Some(super::dxmeta::ResClass::Uav);
        let (img, info) = self.image_value(&h, uav)?;
        let dim = info.dim;
        let coord = self.coords(args, 3, dim.coords, St::I32)?;
        let t = self.sampled_vec_ty(info.sc);
        let r = if uav {
            self.s.emit(OP_IMAGE_READ, t, &[img, coord])
        } else {
            let mut ops = vec![img, coord];
            let mut mask = 0u32;
            let mut words = Vec::new();
            let off = self.offsets(args, 6, dim.offs)?;
            if dim.ms {
                let (smp, _) = self.arg(args, 2)?;
                mask |= 0x40;
                words.push(smp);
            } else if !dim.buffer {
                let lod = match args.get(2) {
                    Some(&a) if !self.is_undef(a) => self.arg(args, 2)?.0,
                    _ => self.c32(0),
                };
                mask |= 2;
                words.push(lod);
            }
            if let Some((true, o)) = off {
                mask |= 8;
                // Ordre : ConstOffset (8) avant Sample (0x40).
                words.insert(if mask & 2 != 0 { 1 } else { 0 }, o);
            }
            if mask != 0 {
                ops.push(mask);
                ops.extend_from_slice(&words);
            }
            self.s.emit(OP_IMAGE_FETCH, t, &ops)
        };
        let comps = self.unpack4(r, info.sc, est, 4)?;
        self.resret(&comps, rt)
    }

    fn pack_texel(&mut self, args: &[ValId], first: usize, sc: Sc) -> R<u32> {
        let mut ids = Vec::new();
        for i in 0..4 {
            let (v, st) = self.arg(args, first + i)?;
            let id = match (sc, st) {
                (Sc::F32, St::F32) => v,
                (Sc::F32, St::F16) => self.un_id(OP_F_CONVERT, St::F32, v),
                (Sc::F32, St::I32) => self.un_id(OP_BITCAST, St::F32, v),
                (Sc::I32, St::I32) => {
                    let t = self.s.ty(Sc::I32, 1);
                    self.s.emit(OP_BITCAST, t, &[v])
                }
                (Sc::U32, St::I32) => v,
                (Sc::U32, St::F32) => self.un_id(OP_BITCAST, St::I32, v),
                (Sc::I32, St::F32) => {
                    let u = self.un_id(OP_BITCAST, St::I32, v);
                    let t = self.s.ty(Sc::I32, 1);
                    self.s.emit(OP_BITCAST, t, &[u])
                }
                _ => return unsup("texel de stockage de type inattendu"),
            };
            ids.push(id);
        }
        let t = self.s.ty(sc, 4);
        Ok(self.s.emit(OP_COMPOSITE_CONSTRUCT, t, &ids))
    }

    fn un_id(&mut self, op: u16, st: St, a: u32) -> u32 {
        let t = self.sty(st);
        self.s.emit(op, t, &[a])
    }

    fn op_texture_store(&mut self, args: &[ValId]) -> R<()> {
        let h = self.handle(*args.get(1).unwrap_or(&0))?;
        let (img, info) = self.image_value(&h, true)?;
        let coord = self.coords(args, 2, info.dim.coords, St::I32)?;
        let texel = self.pack_texel(args, 5, info.sc)?;
        self.s.emit_void(OP_IMAGE_WRITE, &[img, coord, texel]);
        Ok(())
    }

    fn is_typed_buffer(h: &Handle) -> bool {
        h.kind == rk::TYPEDBUFFER
    }

    fn op_buffer_load(&mut self, op: u32, args: &[ValId], rt: Option<TyId>) -> R<RVal> {
        let h = self.handle(*args.get(1).unwrap_or(&0))?;
        let uav = h.class == Some(super::dxmeta::ResClass::Uav);
        let est = self.st_from_resret(rt)?;
        if Self::is_typed_buffer(&h) {
            let (img, info) = self.image_value(&h, uav)?;
            let (i, _) = self.arg(args, 2)?;
            let t = self.sampled_vec_ty(info.sc);
            let r = if uav {
                self.s.emit(OP_IMAGE_READ, t, &[img, i])
            } else {
                self.s.emit(OP_IMAGE_FETCH, t, &[img, i])
            };
            let comps = self.unpack4(r, info.sc, est, 4)?;
            return self.resret(&comps, rt);
        }
        // Nombre de composantes : masque (rawBufferLoad) ou usage des extractvalue.
        let n = if op == 139 {
            let m = self.ia(args, 4).unwrap_or(15) as u32;
            (32 - m.leading_zeros()).clamp(1, 4)
        } else {
            0
        };
        let n = if n == 0 { 4 } else { n };
        let byte = self.byte_addr(&h, self.ga(args, 2)?, args.get(3).copied())?;
        let mut comps = Vec::new();
        let u = self.sty(St::I32);
        let words_per = (est.bits() / 32).max(1);
        let mut live = n;
        if op == 68 {
            // BufferLoad : on ne lit que les composantes réellement utilisées.
            live = 4;
        }
        for k in 0..live {
            if est.bits() == 16 {
                let wi = {
                    let sh = self.c32(k * 2);
                    let b = self.s.emit(OP_I_ADD, u, &[byte, sh]);
                    self.word_index(b, 0)
                };
                let p = self.buffer_word_ptr(&h, uav, wi)?;
                let w = self.s.emit(OP_LOAD, u, &[p]);
                let sh = self.c32(k * 2);
                let b = self.s.emit(OP_I_ADD, u, &[byte, sh]);
                let two = self.c32(2);
                let hi = self.s.emit(OP_BITWISE_AND, u, &[b, two]);
                let z = self.c32(0);
                let bt = self.sty(St::I1);
                let is_hi = self.s.emit(OP_I_NOT_EQUAL, bt, &[hi, z]);
                let s16 = self.c32(16);
                let sel = self.s.emit(OP_SELECT, u, &[is_hi, s16, z]);
                let shifted = self.s.emit(OP_SHIFT_RIGHT_LOGICAL, u, &[w, sel]);
                let m = self.c32(0xFFFF);
                let masked = self.s.emit(OP_BITWISE_AND, u, &[shifted, m]);
                let i16t = self.sty(St::I16);
                let h16 = self.s.emit(OP_U_CONVERT, i16t, &[masked]);
                comps.push(if est == St::F16 {
                    self.un(OP_BITCAST, St::F16, &[h16])
                } else {
                    RVal::S(h16, St::I16)
                });
            } else if words_per == 2 {
                let w0 = self.word_index(byte, 2 * k);
                let w1 = self.word_index(byte, 2 * k + 1);
                let p0 = self.buffer_word_ptr(&h, uav, w0)?;
                let p1 = self.buffer_word_ptr(&h, uav, w1)?;
                let a = self.s.emit(OP_LOAD, u, &[p0]);
                let b = self.s.emit(OP_LOAD, u, &[p1]);
                let v2 = self.vec_of(St::I32, &[a, b]);
                comps.push(self.un(OP_BITCAST, est, &[v2]));
            } else {
                let w = self.word_index(byte, k);
                let p = self.buffer_word_ptr(&h, uav, w)?;
                let v = self.s.emit(OP_LOAD, u, &[p]);
                comps.push(self.word_to(v, est)?);
            }
        }
        self.resret(&comps, rt)
    }

    fn op_buffer_store(&mut self, op: u32, args: &[ValId]) -> R<()> {
        let h = self.handle(*args.get(1).unwrap_or(&0))?;
        if Self::is_typed_buffer(&h) {
            let (img, info) = self.image_value(&h, true)?;
            let (i, _) = self.arg(args, 2)?;
            let texel = self.pack_texel(args, 4, info.sc)?;
            self.s.emit_void(OP_IMAGE_WRITE, &[img, i, texel]);
            return Ok(());
        }
        let (first, mask_idx) = (4usize, 8usize);
        let _ = op;
        let mask = self.ia(args, mask_idx).unwrap_or(15) as u32;
        let byte = self.byte_addr(&h, self.ga(args, 2)?, args.get(3).copied())?;
        let u = self.sty(St::I32);
        for k in 0..4u32 {
            if mask & (1 << k) == 0 {
                continue;
            }
            let (v, st) = self.arg(args, first + k as usize)?;
            match st.bits() {
                16 => {
                    let w16 = if st == St::F16 {
                        self.un_id(OP_BITCAST, St::I16, v)
                    } else {
                        v
                    };
                    let wv = self.un_id(OP_U_CONVERT, St::I32, w16);
                    let sh = self.c32(k * 2);
                    let b = self.s.emit(OP_I_ADD, u, &[byte, sh]);
                    let wi = self.word_index(b, 0);
                    let p = self.buffer_word_ptr(&h, true, wi)?;
                    let two = self.c32(2);
                    let hi = self.s.emit(OP_BITWISE_AND, u, &[b, two]);
                    let z = self.c32(0);
                    let bt = self.sty(St::I1);
                    let is_hi = self.s.emit(OP_I_NOT_EQUAL, bt, &[hi, z]);
                    let s16 = self.c32(16);
                    let shamt = self.s.emit(OP_SELECT, u, &[is_hi, s16, z]);
                    let fff = self.c32(0xFFFF);
                    let m = self.s.emit(OP_SHIFT_LEFT_LOGICAL, u, &[fff, shamt]);
                    let nm = self.s.emit(OP_NOT, u, &[m]);
                    let val = self.s.emit(OP_SHIFT_LEFT_LOGICAL, u, &[wv, shamt]);
                    let scope = self.c32(1);
                    let sem = self.c32(0);
                    self.s.emit(OP_ATOMIC_AND, u, &[p, scope, sem, nm]);
                    self.s.emit(OP_ATOMIC_OR, u, &[p, scope, sem, val]);
                }
                64 => {
                    let t = self.vty(St::I32, 2);
                    let v2 = self.s.emit(OP_BITCAST, t, &[v]);
                    for j in 0..2u32 {
                        let wj = self.s.emit(OP_COMPOSITE_EXTRACT, u, &[v2, j]);
                        let wi = self.word_index(byte, 2 * k + j);
                        let p = self.buffer_word_ptr(&h, true, wi)?;
                        self.s.emit_void(OP_STORE, &[p, wj]);
                    }
                }
                _ => {
                    let w = match st {
                        St::F32 => self.un_id(OP_BITCAST, St::I32, v),
                        St::I1 => self.to_u32(v, st)?,
                        _ => v,
                    };
                    let wi = self.word_index(byte, k);
                    let p = self.buffer_word_ptr(&h, true, wi)?;
                    self.s.emit_void(OP_STORE, &[p, w]);
                }
            }
        }
        Ok(())
    }

    fn op_get_dimensions(&mut self, args: &[ValId]) -> R<RVal> {
        let h = self.handle(*args.get(1).unwrap_or(&0))?;
        let uav = h.class == Some(super::dxmeta::ResClass::Uav);
        let u = self.sty(St::I32);
        self.s.caps.insert(CAP_IMAGE_QUERY);
        let zero = self.c32(0);
        let mut comps = vec![zero; 4];
        if let Some(dim) = dim_of_kind(h.kind) {
            let (img, _info) = self.image_value(&h, uav)?;
            let size_ty = self.vty(St::I32, dim.size);
            let storage = uav || dim.ms || dim.buffer;
            let size = if storage {
                self.s.emit(OP_IMAGE_QUERY_SIZE, size_ty, &[img])
            } else {
                let lod = match args.get(2) {
                    Some(&a) if !self.is_undef(a) => self.arg(args, 2)?.0,
                    _ => zero,
                };
                self.s.emit(OP_IMAGE_QUERY_SIZE_LOD, size_ty, &[img, lod])
            };
            for i in 0..dim.size {
                comps[i as usize] = if dim.size == 1 {
                    size
                } else {
                    self.s.emit(OP_COMPOSITE_EXTRACT, u, &[size, i])
                };
            }
            if dim.ms {
                comps[3] = self.s.emit(OP_IMAGE_QUERY_SAMPLES, u, &[img]);
            } else if !dim.buffer && !uav {
                comps[3] = self.s.emit(OP_IMAGE_QUERY_LEVELS, u, &[img]);
            }
        } else if matches!(
            h.kind,
            rk::RAWBUFFER | rk::STRUCTUREDBUFFER | rk::STRUCTUREDBUFFERCOUNTER
        ) {
            let words = self.buffer_words(&h, uav)?;
            let two = self.c32(2);
            let bytes = self.s.emit(OP_SHIFT_LEFT_LOGICAL, u, &[words, two]);
            comps[0] = if h.kind == rk::RAWBUFFER {
                bytes
            } else {
                let st = self.c32(h.stride.max(1));
                self.s.emit(OP_U_DIV, u, &[bytes, st])
            };
        } else {
            return unsup("GetDimensions sur ce type de ressource");
        }
        Ok(RVal::Agg(
            comps.into_iter().map(|c| RVal::S(c, St::I32)).collect(),
        ))
    }

    // ------------------------------------------------------------------ atomiques

    fn op_atomic(&mut self, op: u32, args: &[ValId]) -> R<RVal> {
        let h = self.handle(*args.get(1).unwrap_or(&0))?;
        let (cmp_new, atomic_op, off0, value_idx) = if op == 78 {
            (None, self.ia(args, 2).unwrap_or(0), 3usize, 6usize)
        } else {
            (Some((5usize, 6usize)), 0, 2usize, 6usize)
        };
        let u = self.sty(St::I32);
        let ptr = if let Some(dim) = dim_of_kind(h.kind) {
            let (var, info) = self.image_ptr(&h)?;
            let coord = self.coords(args, off0, dim.coords, St::I32)?;
            let pt = self.s.ptr(11, u);
            let z = self.c32(0);
            let _ = info;
            self.s.emit(OP_IMAGE_TEXEL_POINTER, pt, &[var, coord, z])
        } else {
            let byte = self.byte_addr(
                &h,
                self.ga(args, off0)?,
                if h.kind == rk::RAWBUFFER {
                    None
                } else {
                    args.get(off0 + 1).copied()
                },
            )?;
            let w = self.word_index(byte, 0);
            self.buffer_word_ptr(&h, true, w)?
        };
        let scope = self.c32(1);
        let sem = self.c32(0);
        if let Some((ci, ni)) = cmp_new {
            let (c, _) = self.arg(args, ci)?;
            let (n, _) = self.arg(args, ni)?;
            let old = self
                .s
                .emit(OP_ATOMIC_COMPARE_EXCHANGE, u, &[ptr, scope, sem, sem, n, c]);
            return Ok(RVal::S(old, St::I32));
        }
        let (v, _) = self.arg(args, value_idx)?;
        let spop = match atomic_op {
            0 => OP_ATOMIC_I_ADD,
            1 => OP_ATOMIC_AND,
            2 => OP_ATOMIC_OR,
            3 => OP_ATOMIC_XOR,
            4 => OP_ATOMIC_S_MIN,
            5 => OP_ATOMIC_S_MAX,
            6 => OP_ATOMIC_U_MIN,
            7 => OP_ATOMIC_U_MAX,
            8 => OP_ATOMIC_EXCHANGE,
            o => return unsup(format!("opération atomique {o}")),
        };
        let r = self.s.emit(spop, u, &[ptr, scope, sem, v]);
        Ok(RVal::S(r, St::I32))
    }

    fn op_barrier(&mut self, args: &[ValId]) -> R<()> {
        let mode = self.ia(args, 1).unwrap_or(1) as u32;
        let sync = mode & 1 != 0;
        let device = mode & 2 != 0;
        let mut sem = 0;
        if mode & 8 != 0 {
            sem |= MEM_WORKGROUP;
        }
        if mode & 4 != 0 {
            sem |= MEM_UNIFORM | MEM_IMAGE;
        }
        if device {
            sem |= MEM_UNIFORM | MEM_IMAGE;
        }
        if sem != 0 {
            sem |= MEM_ACQ_REL;
        }
        let mem_scope = self.c32(if device { 1 } else { 2 });
        let sem_c = self.c32(sem);
        if sync {
            let exec = self.c32(2);
            self.s
                .emit_void(OP_CONTROL_BARRIER, &[exec, mem_scope, sem_c]);
        } else if sem != 0 {
            self.s.emit_void(OP_MEMORY_BARRIER, &[mem_scope, sem_c]);
        }
        Ok(())
    }

    fn op_discard(&mut self, args: &[ValId]) -> R<()> {
        let c = match args.get(1) {
            Some(&a) => self.bool_val(a)?,
            None => return invalid("discard sans condition"),
        };
        let k = self.s.id();
        let m = self.s.id();
        self.s.ensure_block();
        self.s.emit_raw(OP_SELECTION_MERGE, &[m, 0]);
        self.s.terminate(OP_BRANCH_CONDITIONAL, &[c, k, m]);
        self.s.begin_block(k);
        self.s.terminate(OP_KILL, &[]);
        self.s.begin_block(m);
        Ok(())
    }

    // ------------------------------------------------------------------ interpolation

    fn input_ptr_for_eval(&mut self, args: &[ValId]) -> R<(u32, u32, u32, u32)> {
        let Some(id) = self.ia(args, 1) else {
            return invalid("eval : identifiant de signature non constant");
        };
        let Some(&iv) = self.io.inputs.get(&(id as u32)) else {
            return invalid("eval : entrée inconnue");
        };
        let col = self.ia(args, 3).unwrap_or(0) as u32;
        let start = self
            .info
            .inputs
            .get(id as usize)
            .map(|e| e.start_col)
            .unwrap_or(0);
        let mut ptr = iv.var;
        let vt = self.vty(iv.comp, iv.cols);
        if iv.rows > 1 {
            let r = match args.get(2) {
                Some(&r) => self.scalar_u32(r)?,
                None => self.c32(0),
            };
            let pt = self.s.ptr(SC_INPUT, vt);
            ptr = self.s.emit(OP_ACCESS_CHAIN, pt, &[iv.var, r]);
        }
        Ok((ptr, col.saturating_sub(start), iv.cols, vt))
    }

    fn op_eval(&mut self, op: u32, args: &[ValId], est: St) -> R<RVal> {
        self.s.caps.insert(CAP_INTERPOLATION_FUNCTION);
        let (ptr, comp, cols, vt) = self.input_ptr_for_eval(args)?;
        let r = match op {
            89 => {
                let ops = [self.s.glsl, G_INTERPOLATE_AT_CENTROID, ptr];
                self.s.emit(OP_EXT_INST, vt, &ops)
            }
            88 => {
                let (s, _) = self.arg(args, 4)?;
                let ops = [self.s.glsl, G_INTERPOLATE_AT_SAMPLE, ptr, s];
                self.s.emit(OP_EXT_INST, vt, &ops)
            }
            _ => {
                let (x, _) = self.arg(args, 4)?;
                let (y, _) = self.arg(args, 5)?;
                // Décalages signés sur 4 bits, en 1/16 de pixel.
                let f = self.sty(St::F32);
                let xf = self.s.emit(OP_CONVERT_S_TO_F, f, &[x]);
                let yf = self.s.emit(OP_CONVERT_S_TO_F, f, &[y]);
                let k = self.fconst(St::F32, 1.0 / 16.0);
                let xs = self.s.emit(OP_F_MUL, f, &[xf, k]);
                let ys = self.s.emit(OP_F_MUL, f, &[yf, k]);
                let v2 = self.vec_of(St::F32, &[xs, ys]);
                let ops = [self.s.glsl, G_INTERPOLATE_AT_OFFSET, ptr, v2];
                self.s.emit(OP_EXT_INST, vt, &ops)
            }
        };
        let f = self.sty(St::F32);
        let v = if cols > 1 {
            self.s
                .emit(OP_COMPOSITE_EXTRACT, f, &[r, comp.min(cols - 1)])
        } else {
            r
        };
        self.convert_io(v, St::F32, est)
    }

    fn op_attribute_at_vertex(&mut self, args: &[ValId], est: St) -> R<RVal> {
        let Some(id) = self.ia(args, 1) else {
            return invalid("attributeAtVertex : identifiant non constant");
        };
        let Some(&iv) = self.io.inputs.get(&(id as u32)) else {
            return invalid("attributeAtVertex : entrée inconnue");
        };
        let col = self.ia(args, 3).unwrap_or(0) as u32;
        let start = self
            .info
            .inputs
            .get(id as usize)
            .map(|e| e.start_col)
            .unwrap_or(0);
        let (vtx, _) = self.arg(args, 4)?;
        let vtx = self.to_u32(vtx, St::I32)?;
        let f = self.sty(iv.comp);
        let pt = self.s.ptr(SC_INPUT, f);
        let c = self.c32(col.saturating_sub(start).min(3));
        let p = if iv.cols > 1 {
            self.s.emit(OP_ACCESS_CHAIN, pt, &[iv.var, vtx, c])
        } else {
            self.s.emit(OP_ACCESS_CHAIN, pt, &[iv.var, vtx])
        };
        let v = self.s.emit(OP_LOAD, f, &[p]);
        self.convert_io(v, iv.comp, est)
    }

    // ------------------------------------------------------------------ ondes

    fn op_wave(&mut self, op: u32, args: &[ValId], rt: Option<TyId>) -> R<Option<RVal>> {
        let scope = self.c32(3);
        self.s.caps.insert(CAP_GROUP_NON_UNIFORM);
        let bt = self.sty(St::I1);
        Ok(Some(match op {
            110 => {
                let id = self.s.emit(OP_GROUP_NU_ELECT, bt, &[scope]);
                RVal::S(id, St::I1)
            }
            111 => RVal::S(
                self.load_builtin_comp(BI_SUBGROUP_LOCAL_INVOCATION_ID, None),
                St::I32,
            ),
            112 => RVal::S(self.load_builtin_comp(BI_SUBGROUP_SIZE, None), St::I32),
            113 | 114 => {
                self.s.caps.insert(CAP_GROUP_NU_VOTE);
                let (a, _) = self.arg(args, 1)?;
                let id = self.s.emit(
                    if op == 113 {
                        OP_GROUP_NU_ANY
                    } else {
                        OP_GROUP_NU_ALL
                    },
                    bt,
                    &[scope, a],
                );
                RVal::S(id, St::I1)
            }
            115 => {
                self.s.caps.insert(CAP_GROUP_NU_VOTE);
                let (a, _) = self.arg(args, 1)?;
                let id = self.s.emit(OP_GROUP_NU_ALL_EQUAL, bt, &[scope, a]);
                RVal::S(id, St::I1)
            }
            116 => {
                self.s.caps.insert(CAP_GROUP_NU_BALLOT);
                let (a, _) = self.arg(args, 1)?;
                let t = self.vty(St::I32, 4);
                let b = self.s.emit(OP_GROUP_NU_BALLOT, t, &[scope, a]);
                let u = self.sty(St::I32);
                let comps = (0..4)
                    .map(|i| RVal::S(self.s.emit(OP_COMPOSITE_EXTRACT, u, &[b, i]), St::I32))
                    .collect();
                RVal::Agg(comps)
            }
            117 => {
                self.s.caps.insert(CAP_GROUP_NU_SHUFFLE);
                let (a, st) = self.arg(args, 1)?;
                let (l, _) = self.arg(args, 2)?;
                let t = self.sty(st);
                RVal::S(self.s.emit(OP_GROUP_NU_SHUFFLE, t, &[scope, a, l]), st)
            }
            118 => {
                self.s.caps.insert(CAP_GROUP_NU_BALLOT);
                let (a, st) = self.arg(args, 1)?;
                let t = self.sty(st);
                RVal::S(self.s.emit(OP_GROUP_NU_BROADCAST_FIRST, t, &[scope, a]), st)
            }
            119 | 121 => {
                self.s.caps.insert(CAP_GROUP_NU_ARITHMETIC);
                let (a, st) = self.arg(args, 1)?;
                let kind = self.ia(args, 2).unwrap_or(0);
                let unsigned = self.ia(args, 3).unwrap_or(0) != 0;
                let f = st.is_float();
                let spop = match (kind, f, unsigned) {
                    (0, false, _) => OP_GROUP_NU_IADD,
                    (0, true, _) => OP_GROUP_NU_FADD,
                    (1, false, _) => OP_GROUP_NU_IMUL,
                    (1, true, _) => OP_GROUP_NU_FMUL,
                    (2, true, _) => OP_GROUP_NU_FMIN,
                    (2, false, false) => OP_GROUP_NU_SMIN,
                    (2, false, true) => OP_GROUP_NU_UMIN,
                    (3, true, _) => OP_GROUP_NU_FMAX,
                    (3, false, false) => OP_GROUP_NU_SMAX,
                    (3, false, true) => OP_GROUP_NU_UMAX,
                    _ => return unsup("opération d'onde inconnue"),
                };
                let t = self.sty(st);
                let go = if op == 119 { 0 } else { 2 };
                RVal::S(self.s.emit(spop, t, &[scope, go, a]), st)
            }
            120 => {
                self.s.caps.insert(CAP_GROUP_NU_ARITHMETIC);
                let (a, st) = self.arg(args, 1)?;
                let kind = self.ia(args, 2).unwrap_or(0);
                let spop = match kind {
                    0 => OP_GROUP_NU_BITWISE_AND,
                    1 => OP_GROUP_NU_BITWISE_OR,
                    _ => OP_GROUP_NU_BITWISE_XOR,
                };
                let t = self.sty(st);
                let go = 0;
                RVal::S(self.s.emit(spop, t, &[scope, go, a]), st)
            }
            135 | 136 => {
                self.s.caps.insert(CAP_GROUP_NU_BALLOT);
                let (a, _) = self.arg(args, 1)?;
                let t = self.vty(St::I32, 4);
                let b = self.s.emit(OP_GROUP_NU_BALLOT, t, &[scope, a]);
                let u = self.sty(St::I32);
                let go = if op == 135 { 0 } else { 2 };
                RVal::S(
                    self.s
                        .emit(OP_GROUP_NU_BALLOT_BIT_COUNT, u, &[scope, go, b]),
                    St::I32,
                )
            }
            122 => {
                self.s.caps.insert(CAP_GROUP_NU_SHUFFLE);
                let (a, st) = self.arg(args, 1)?;
                let (l, _) = self.arg(args, 2)?;
                let lane = self.load_builtin_comp(BI_SUBGROUP_LOCAL_INVOCATION_ID, None);
                let u = self.sty(St::I32);
                let m = self.c32(!3u32);
                let base = self.s.emit(OP_BITWISE_AND, u, &[lane, m]);
                let target = self.s.emit(OP_BITWISE_OR, u, &[base, l]);
                let t = self.sty(st);
                RVal::S(self.s.emit(OP_GROUP_NU_SHUFFLE, t, &[scope, a, target]), st)
            }
            123 => {
                self.s.caps.insert(CAP_GROUP_NU_QUAD);
                let (a, st) = self.arg(args, 1)?;
                let dir = self.ia(args, 2).unwrap_or(0) as u32;
                let d = self.c32(dir);
                let t = self.sty(st);
                RVal::S(self.s.emit(OP_GROUP_NU_QUAD_SWAP, t, &[scope, a, d]), st)
            }
            _ => {
                let _ = rt;
                return Err(ShaderTranslateError::UnsupportedDxilOp {
                    opcode: op,
                    name: op_name(op),
                });
            }
        }))
    }
}

fn f16_bits(v: f32) -> u64 {
    // Conversion f32 -> binary16 (arrondi vers zéro suffisant pour 0/1).
    let b = v.to_bits();
    let sign = (b >> 16) & 0x8000;
    let exp = ((b >> 23) & 0xFF) as i32 - 127 + 15;
    let man = (b >> 13) & 0x3FF;
    if v == 0.0 {
        return u64::from(sign);
    }
    if exp <= 0 {
        return u64::from(sign);
    }
    if exp >= 31 {
        return u64::from(sign | 0x7C00);
    }
    u64::from(sign | ((exp as u32) << 10) | man)
}

#[allow(dead_code)]
fn _unused(_: Slot) {}
