//! DS : LDS (`Workgroup`), atomiques, `read2/write2`, sous-mots, `swizzle`/`permute`/`bpermute`,
//! `append`/`consume`. Hors périmètre : GWS, `ordered_count`, GDS, atomiques 64 bits.

use super::inst::{Instruction, Operand, Src};
use super::translate::{TResult, Tr, TranslateError, unsupported};
use crate::dxbc_to_spirv::spv::{Sc, Val};

const OP_IADD: u16 = 128;
const OP_IMUL: u16 = 132;
const OP_AND: u16 = 199;
const OP_OR: u16 = 197;
const OP_XOR: u16 = 198;
const OP_NOT: u16 = 200;
const OP_SHR: u16 = 194;
const OP_SHL: u16 = 196;
const OP_BFSEXT: u16 = 202;
const OP_BFUEXT: u16 = 203;

fn vg(o: &Operand, i: &Instruction) -> TResult<usize> {
    match o.src {
        Src::Vgpr(n) => Ok(usize::from(n)),
        _ => unsupported(i),
    }
}

impl Tr<'_> {
    fn lds_ptr(&mut self, widx: Val) -> u32 {
        let lds = self.lds_var();
        let u = self.spv.ty(Sc::U32, 1);
        let pt = self.spv.ptr(4, u);
        self.spv.emit(65, pt, &[lds, widx.id])
    }

    fn vload(&mut self, n: usize) -> Val {
        let v = self.vvar(n);
        self.load_u32(v)
    }

    /// Mot LDS d'adresse en octets `byte` : (pointeur, décalage en bits dans le mot).
    fn lds_word(&mut self, byte: Val) -> (u32, Val) {
        let (c2, c3, c8) = (self.cu(2), self.cu(3), self.cu(8));
        let widx = self.op2(OP_SHR, Sc::U32, byte, c2);
        let low = self.op2(OP_AND, Sc::U32, byte, c3);
        let sh = self.op2(OP_IMUL, Sc::U32, low, c8);
        (self.lds_ptr(widx), sh)
    }

    fn atomic_lds(&mut self, op: u16, ptr: u32, v: Val) -> Val {
        let (scope, sem) = (self.spv.c_u32(2), self.spv.c_u32(0));
        let u = self.spv.ty(Sc::U32, 1);
        let id = self.spv.emit(op, u, &[ptr, scope, sem, v.id]);
        Val {
            id,
            sc: Sc::U32,
            n: 1,
        }
    }

    pub(crate) fn ds(&mut self, i: &Instruction) -> TResult<()> {
        if self.cfg.lds_dwords == 0 {
            return Err(TranslateError::Config(
                "instruction ds_* sans lds_dwords".into(),
            ));
        }
        let name = i.name.strip_prefix("ds_").unwrap_or(i.name);
        let ops = &i.operands;
        if name == "nop" {
            return Ok(());
        }
        if ops.len() <= usize::from(i.n_dst) && !matches!(name, "append" | "consume") {
            return unsupported(i);
        }
        let mut k = 0;
        let dst = if i.n_dst == 1 {
            k = 1;
            Some(ops[0])
        } else {
            None
        };
        let m = &i.mods;
        // ---- sous-groupes
        if name == "bpermute_b32" || name == "permute_b32" || name == "swizzle_b32" {
            return self.ds_lane_exchange(i, name, dst, &ops[k..]);
        }
        if name == "append" || name == "consume" {
            let d = vg(
                &dst.ok_or_else(|| TranslateError::Unsupported(i.name.into()))?,
                i,
            )?;
            let m0 = self.sload(124);
            let off = self.cu(m.offset as u32);
            let base = self.op2(OP_IADD, Sc::U32, m0, off);
            let c2 = self.cu(2);
            let widx = self.op2(OP_SHR, Sc::U32, base, c2);
            let ptr = self.lds_ptr(widx);
            let one = self.cu(1);
            return self.guarded(|t| {
                if name == "append" {
                    let old = t.atomic_lds(234, ptr, one);
                    t.write_v(d, old);
                } else {
                    let old = t.atomic_lds(235, ptr, one);
                    let nv = t.op2(130, Sc::U32, old, one);
                    t.write_v(d, nv);
                }
                Ok(())
            });
        }
        let addr_reg = vg(&ops[k], i)?;
        let a = self.vload(addr_reg);
        let off = self.cu(m.offset as u32);
        let byte = self.op2(OP_IADD, Sc::U32, a, off);
        // ---- lectures
        if let Some(t) = name.strip_prefix("read_") {
            let d = vg(
                &dst.ok_or_else(|| TranslateError::Unsupported(i.name.into()))?,
                i,
            )?;
            let nwords = match t {
                "b32" => 1,
                "b64" => 2,
                "b96" => 3,
                "b128" => 4,
                _ => 0,
            };
            if nwords > 0 {
                let (c2, _) = (self.cu(2), 0);
                let w0 = self.op2(OP_SHR, Sc::U32, byte, c2);
                return self.guarded(|t| {
                    for j in 0..nwords {
                        let kc = t.cu(j as u32);
                        let idx = t.op2(OP_IADD, Sc::U32, w0, kc);
                        let p = t.lds_ptr(idx);
                        let v = t.load_ptr_u32(p);
                        t.write_v(d + j, v);
                    }
                    Ok(())
                });
            }
            // u8/i8/u16/i16 (+ _d16, _d16_hi)
            let (core, d16) = match t.strip_suffix("_d16_hi") {
                Some(c) => (c, 2),
                None => match t.strip_suffix("_d16") {
                    Some(c) => (c, 1),
                    None => (t, 0),
                },
            };
            let (bits, signed) = match core {
                "u8" => (8, false),
                "i8" => (8, true),
                "u16" => (16, false),
                "i16" => (16, true),
                _ => return unsupported(i),
            };
            let signed = signed && d16 == 0;
            let old = self.vload(d);
            return self.guarded(|t| {
                let (p, sh) = t.lds_word(byte);
                let w = t.load_ptr_u32(p);
                let cb = t.cu(bits);
                let r = if signed {
                    let wi = t.spv.bitcast(w, Sc::I32);
                    let r = t.spv.val(OP_BFSEXT, Sc::I32, 1, &[wi.id, sh.id, cb.id]);
                    t.spv.bitcast(r, Sc::U32)
                } else {
                    t.spv.val(OP_BFUEXT, Sc::U32, 1, &[w.id, sh.id, cb.id])
                };
                let (lo, c16) = (t.cu(0xFFFF), t.cu(16));
                let out = match d16 {
                    0 => r,
                    1 => {
                        let hi = t.cu(0xFFFF_0000);
                        let keep = t.op2(OP_AND, Sc::U32, old, hi);
                        let nv = t.op2(OP_AND, Sc::U32, r, lo);
                        t.op2(OP_OR, Sc::U32, keep, nv)
                    }
                    _ => {
                        let keep = t.op2(OP_AND, Sc::U32, old, lo);
                        let nv = t.op2(OP_SHL, Sc::U32, r, c16);
                        t.op2(OP_OR, Sc::U32, keep, nv)
                    }
                };
                t.write_v(d, out);
                Ok(())
            });
        }
        if let Some(t) = name.strip_prefix("read2") {
            let st64 = t.starts_with("st64");
            let b64 = t.ends_with("b64");
            let d = vg(
                &dst.ok_or_else(|| TranslateError::Unsupported(i.name.into()))?,
                i,
            )?;
            let (scale, words) = (
                (if b64 { 8 } else { 4 }) * if st64 { 64 } else { 1 },
                if b64 { 2 } else { 1 },
            );
            return self.guarded(|tt| {
                for (e, o) in [m.offset0, m.offset1].iter().enumerate() {
                    let c = tt.cu(u32::from(*o) * scale);
                    let bb = tt.op2(OP_IADD, Sc::U32, a, c);
                    let c2 = tt.cu(2);
                    let w0 = tt.op2(OP_SHR, Sc::U32, bb, c2);
                    for j in 0..words {
                        let kc = tt.cu(j as u32);
                        let idx = tt.op2(OP_IADD, Sc::U32, w0, kc);
                        let p = tt.lds_ptr(idx);
                        let v = tt.load_ptr_u32(p);
                        tt.write_v(d + e * words + j, v);
                    }
                }
                Ok(())
            });
        }
        // ---- écritures
        let data0 = ops.get(k + 1).copied();
        if let Some(t) = name.strip_prefix("write_") {
            let d0 = vg(
                &data0.ok_or_else(|| TranslateError::Unsupported(i.name.into()))?,
                i,
            )?;
            let nwords = match t {
                "b32" => 1,
                "b64" => 2,
                "b96" => 3,
                "b128" => 4,
                _ => 0,
            };
            if nwords > 0 {
                let vals: Vec<Val> = (0..nwords).map(|j| self.vload(d0 + j)).collect();
                let c2 = self.cu(2);
                let w0 = self.op2(OP_SHR, Sc::U32, byte, c2);
                return self.guarded(|tt| {
                    for (j, v) in vals.iter().enumerate() {
                        let kc = tt.cu(j as u32);
                        let idx = tt.op2(OP_IADD, Sc::U32, w0, kc);
                        let p = tt.lds_ptr(idx);
                        tt.spv.emit_void(62, &[p, v.id]);
                    }
                    Ok(())
                });
            }
            let (core, hi) = match t.strip_suffix("_d16_hi") {
                Some(c) => (c, true),
                None => (t, false),
            };
            let bits = match core {
                "b8" => 8,
                "b16" => 16,
                _ => return unsupported(i),
            };
            let mut v = self.vload(d0);
            if hi {
                let c16 = self.cu(16);
                v = self.op2(OP_SHR, Sc::U32, v, c16);
            }
            return self.guarded(|tt| {
                let (p, sh) = tt.lds_word(byte);
                let mask = tt.cu(if bits == 8 { 0xFF } else { 0xFFFF });
                let mv = tt.op2(OP_AND, Sc::U32, v, mask);
                let vs = tt.op2(OP_SHL, Sc::U32, mv, sh);
                let ms = tt.op2(OP_SHL, Sc::U32, mask, sh);
                let inv = tt.op1(OP_NOT, Sc::U32, ms);
                tt.atomic_lds(240, p, inv);
                tt.atomic_lds(241, p, vs);
                Ok(())
            });
        }
        if let Some(t) = name.strip_prefix("write2") {
            let st64 = t.starts_with("st64");
            let b64 = t.ends_with("b64");
            let (d0, d1) = (
                vg(
                    &data0.ok_or_else(|| TranslateError::Unsupported(i.name.into()))?,
                    i,
                )?,
                vg(
                    &ops.get(k + 2)
                        .copied()
                        .ok_or_else(|| TranslateError::Unsupported(i.name.into()))?,
                    i,
                )?,
            );
            let (scale, words) = (
                (if b64 { 8 } else { 4 }) * if st64 { 64 } else { 1 },
                if b64 { 2 } else { 1 },
            );
            let mut vals: Vec<Val> = Vec::new();
            for j in 0..words {
                vals.push(self.vload(d0 + j));
            }
            for j in 0..words {
                vals.push(self.vload(d1 + j));
            }
            return self.guarded(|tt| {
                for (e, o) in [m.offset0, m.offset1].iter().enumerate() {
                    let c = tt.cu(u32::from(*o) * scale);
                    let bb = tt.op2(OP_IADD, Sc::U32, a, c);
                    let c2 = tt.cu(2);
                    let w0 = tt.op2(OP_SHR, Sc::U32, bb, c2);
                    for j in 0..words {
                        let kc = tt.cu(j as u32);
                        let idx = tt.op2(OP_IADD, Sc::U32, w0, kc);
                        let p = tt.lds_ptr(idx);
                        tt.spv.emit_void(62, &[p, vals[e * words + j].id]);
                    }
                }
                Ok(())
            });
        }
        // ---- atomiques 32 bits
        let stripped = name.replacen("_rtn", "", 1);
        let t = stripped.as_str();
        let code: Option<u16> = match t {
            "add_u32" => Some(234),
            "sub_u32" => Some(235),
            "min_i32" => Some(236),
            "min_u32" => Some(237),
            "max_i32" => Some(238),
            "max_u32" => Some(239),
            "and_b32" => Some(240),
            "or_b32" => Some(241),
            "xor_b32" => Some(242),
            "wrxchg_b32" => Some(229),
            "cmpst_b32" => Some(230),
            _ => None,
        };
        let Some(code) = code else {
            return unsupported(i);
        };
        let d0 = vg(
            &data0.ok_or_else(|| TranslateError::Unsupported(i.name.into()))?,
            i,
        )?;
        let val0 = self.vload(d0);
        let val1 = if code == 230 {
            let d1 = vg(
                &ops.get(k + 2)
                    .copied()
                    .ok_or_else(|| TranslateError::Unsupported(i.name.into()))?,
                i,
            )?;
            Some(self.vload(d1))
        } else {
            None
        };
        let c2 = self.cu(2);
        let widx = self.op2(OP_SHR, Sc::U32, byte, c2);
        let ret = dst.map(|d| vg(&d, i)).transpose()?;
        self.guarded(|tt| {
            let p = tt.lds_ptr(widx);
            let (scope, sem) = (tt.spv.c_u32(2), tt.spv.c_u32(0));
            let u = tt.spv.ty(Sc::U32, 1);
            let old = if let Some(new) = val1 {
                // ds_cmpst : data0 = comparateur, data1 = nouvelle valeur
                tt.spv.emit(230, u, &[p, scope, sem, sem, new.id, val0.id])
            } else {
                tt.spv.emit(code, u, &[p, scope, sem, val0.id])
            };
            if let Some(r) = ret {
                tt.write_v(
                    r,
                    Val {
                        id: old,
                        sc: Sc::U32,
                        n: 1,
                    },
                );
            }
            Ok(())
        })
    }

    /// `ds_bpermute_b32`, `ds_permute_b32`, `ds_swizzle_b32` via `OpGroupNonUniformShuffle`.
    fn ds_lane_exchange(
        &mut self,
        i: &Instruction,
        name: &str,
        dst: Option<Operand>,
        ops: &[Operand],
    ) -> TResult<()> {
        let d = vg(
            &dst.ok_or_else(|| TranslateError::Unsupported(i.name.into()))?,
            i,
        )?;
        self.spv.caps.insert(65);
        self.spv.caps.insert(64);
        self.spv.caps.insert(61);
        let wave = self.wave.lanes();
        let addr = self.vload(vg(&ops[0], i)?);
        let scope = self.cu(3);
        let shuffle = |t: &mut Self, v: Val, lane: Val| {
            t.spv.val(345, Sc::U32, 1, &[scope.id, v.id, lane.id])
        };
        let r = match name {
            "bpermute_b32" => {
                let data = self.vload(vg(&ops[1], i)?);
                let (c2, cm) = (self.cu(2), self.cu(wave - 1));
                let sh = self.op2(OP_SHR, Sc::U32, addr, c2);
                let lane = self.op2(OP_AND, Sc::U32, sh, cm);
                shuffle(self, data, lane)
            }
            "permute_b32" => {
                // dst[addr/4] = data[lane] : balayage des lanes sources
                let data = self.vload(vg(&ops[1], i)?);
                let me = self.lane_id();
                let (c2, cm) = (self.cu(2), self.cu(wave - 1));
                let mut r = self.cu(0);
                for j in 0..wave {
                    let cj = self.cu(j);
                    let aj = shuffle(self, addr, cj);
                    let dj = shuffle(self, data, cj);
                    let t = self.op2(OP_SHR, Sc::U32, aj, c2);
                    let tl = self.op2(OP_AND, Sc::U32, t, cm);
                    let hit = self.op2(170, Sc::Bool, tl, me);
                    r = self.select(hit, dj, r);
                }
                r
            }
            _ => {
                let off = (i.mods.offset as u32) & 0xFFFF;
                let me = self.lane_id();
                let lane = if off & 0x8000 != 0 {
                    // quad_perm : 4 sélecteurs de 2 bits
                    let (c3, c2) = (self.cu(3), self.cu(2));
                    let q = self.op2(OP_AND, Sc::U32, me, c3);
                    let q2 = self.op2(OP_IMUL, Sc::U32, q, c2);
                    let perm = self.cu(off & 0xFF);
                    let sel0 = self.op2(OP_SHR, Sc::U32, perm, q2);
                    let sel = self.op2(OP_AND, Sc::U32, sel0, c3);
                    let nc3 = self.cu(!3u32);
                    let base = self.op2(OP_AND, Sc::U32, me, nc3);
                    self.op2(OP_OR, Sc::U32, base, sel)
                } else {
                    // mode bitmask : ((lane & and) | or) ^ xor sur les 5 bits bas
                    let (and, or, xor) = (off & 0x1F, (off >> 5) & 0x1F, (off >> 10) & 0x1F);
                    let hi_mask = self.cu(!0x1Fu32);
                    let hi = self.op2(OP_AND, Sc::U32, me, hi_mask);
                    let (cand, cor, cxor, c1f) =
                        (self.cu(and), self.cu(or), self.cu(xor), self.cu(0x1F));
                    let l5 = self.op2(OP_AND, Sc::U32, me, c1f);
                    let a1 = self.op2(OP_AND, Sc::U32, l5, cand);
                    let o1 = self.op2(OP_OR, Sc::U32, a1, cor);
                    let x1 = self.op2(OP_XOR, Sc::U32, o1, cxor);
                    self.op2(OP_OR, Sc::U32, hi, x1)
                };
                shuffle(self, addr, lane)
            }
        };
        self.write_v(d, r);
        Ok(())
    }
}
