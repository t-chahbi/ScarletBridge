//! Instructions ALU : flottants, entiers, binaires, comparaisons, conversions.

use super::codegen::{Gen, R, invalid, mask_comps};
use super::decode::Inst;
use super::opcodes::*;
use super::spv::{Sc, Spv, Val};
use crate::shader::ShaderStage;

const ONES: u32 = u32::MAX;

fn cv(s: &mut Spv, sc: Sc, bits: u32, n: u32) -> Val {
    s.c_vec(sc, &vec![bits; n as usize])
}

/// Booléens -> masque D3D (`0xFFFFFFFF` / `0`).
fn bool_mask(s: &mut Spv, b: Val) -> Val {
    let t = cv(s, Sc::U32, ONES, b.n);
    let f = cv(s, Sc::U32, 0, b.n);
    s.val(169, Sc::U32, b.n, &[b.id, t.id, f.id])
}

fn sel(s: &mut Spv, c: Val, a: Val, b: Val) -> Val {
    s.val(169, a.sc, a.n, &[c.id, a.id, b.id])
}

impl Gen {
    fn need(inst: &Inst, k: usize) -> R<()> {
        if inst.operands.len() < k {
            return invalid(format!(
                "{} : {} opérandes attendus",
                opcode_name(inst.opcode),
                k
            ));
        }
        Ok(())
    }

    /// Calcule `f(sources)` pour la destination `operands[0]`.
    fn alu(
        &mut self,
        inst: &Inst,
        nsrc: usize,
        sc: Sc,
        f: impl FnOnce(&mut Spv, &[Val]) -> Val,
    ) -> R<()> {
        Self::need(inst, 1 + nsrc)?;
        let dst = &inst.operands[0];
        let comps = mask_comps(dst.mask);
        if comps.is_empty() || dst.ty == OPERAND_NULL {
            return Ok(());
        }
        let mut srcs = Vec::with_capacity(nsrc);
        for k in 0..nsrc {
            srcs.push(self.read(&inst.operands[1 + k], &comps, sc)?);
        }
        let r = f(&mut self.s, &srcs);
        self.write(dst, r, inst.saturate())
    }

    fn glsl1(&mut self, inst: &Inst, op: u32) -> R<()> {
        self.alu(inst, 1, Sc::F32, |s, a| {
            s.glsl_op(Sc::F32, a[0].n, op, &[a[0].id])
        })
    }

    fn cmp(&mut self, inst: &Inst, sc: Sc, op: u16) -> R<()> {
        self.alu(inst, 2, sc, |s, a| {
            let b = s.val(op, Sc::Bool, a[0].n, &[a[0].id, a[1].id]);
            bool_mask(s, b)
        })
    }

    fn shift(&mut self, inst: &Inst, sc: Sc, op: u16) -> R<()> {
        Self::need(inst, 3)?;
        let dst = &inst.operands[0];
        let comps = mask_comps(dst.mask);
        if comps.is_empty() {
            return Ok(());
        }
        let a = self.read(&inst.operands[1], &comps, sc)?;
        let c = self.read(&inst.operands[2], &comps, Sc::U32)?;
        let m = cv(&mut self.s, Sc::U32, 31, c.n);
        let c = self.s.val(199, Sc::U32, c.n, &[c.id, m.id]);
        let r = self.s.val(op, sc, a.n, &[a.id, c.id]);
        self.write(dst, r, false)
    }

    /// `umul`/`imul` : `dst_hi, dst_lo, a, b`.
    fn mul_ext(&mut self, inst: &Inst, sc: Sc) -> R<()> {
        Self::need(inst, 4)?;
        for (k, which) in [(0usize, 1u32), (1usize, 0u32)] {
            let dst = &inst.operands[k];
            let comps = mask_comps(dst.mask);
            if dst.ty == OPERAND_NULL || comps.is_empty() {
                continue;
            }
            let a = self.read(&inst.operands[2], &comps, sc)?;
            let b = self.read(&inst.operands[3], &comps, sc)?;
            let r = if which == 0 {
                self.s.val(132, sc, a.n, &[a.id, b.id])
            } else {
                let t = self.s.ty(sc, a.n);
                let st = self.s.structure(&[t, t]);
                let op = if sc == Sc::U32 { 151 } else { 152 };
                let id = self.s.emit(op, st, &[a.id, b.id]);
                self.s.val(81, sc, a.n, &[id, 1])
            };
            self.write(dst, r, false)?;
        }
        Ok(())
    }

    fn udiv(&mut self, inst: &Inst) -> R<()> {
        Self::need(inst, 4)?;
        for k in 0..2usize {
            let dst = &inst.operands[k];
            let comps = mask_comps(dst.mask);
            if dst.ty == OPERAND_NULL || comps.is_empty() {
                continue;
            }
            let a = self.read(&inst.operands[2], &comps, Sc::U32)?;
            let b = self.read(&inst.operands[3], &comps, Sc::U32)?;
            let s = &mut self.s;
            let zero = cv(s, Sc::U32, 0, b.n);
            let one = cv(s, Sc::U32, 1, b.n);
            let is0 = s.val(170, Sc::Bool, b.n, &[b.id, zero.id]);
            let safe = sel(s, is0, one, b);
            let op = if k == 0 { 134 } else { 137 };
            let r = s.val(op, Sc::U32, a.n, &[a.id, safe.id]);
            let all = cv(s, Sc::U32, ONES, a.n);
            let r = sel(s, is0, all, r);
            self.write(dst, r, false)?;
        }
        Ok(())
    }

    fn ftoi(s: &mut Spv, x: Val, signed: bool) -> Val {
        let n = x.n;
        let (lo, hi, big, max) = if signed {
            (
                -2147483648.0f32,
                2147483520.0f32,
                2147483648.0f32,
                0x7FFF_FFFFu32,
            )
        } else {
            (0.0, 4294967040.0, 4294967296.0, ONES)
        };
        let lo = cv(s, Sc::F32, lo.to_bits(), n);
        let hi = cv(s, Sc::F32, hi.to_bits(), n);
        let cl = s.glsl_op(Sc::F32, n, 81, &[x.id, lo.id, hi.id]);
        let (conv, sc) = if signed {
            (110, Sc::I32)
        } else {
            (109, Sc::U32)
        };
        let r = s.val(conv, sc, n, &[cl.id]);
        let r = s.bitcast(r, Sc::U32);
        let bigc = cv(s, Sc::F32, big.to_bits(), n);
        let isbig = s.val(190, Sc::Bool, n, &[x.id, bigc.id]);
        let maxv = cv(s, Sc::U32, max, n);
        let r = sel(s, isbig, maxv, r);
        let nan = s.val(156, Sc::Bool, n, &[x.id]);
        let z = cv(s, Sc::U32, 0, n);
        sel(s, nan, z, r)
    }

    fn firstbit(&mut self, inst: &Inst, sc: Sc, glsl: u32) -> R<()> {
        self.alu(inst, 1, sc, |s, a| {
            let n = a[0].n;
            let r = s.glsl_op(sc, n, glsl, &[a[0].id]);
            let r = s.bitcast(r, Sc::U32);
            let all = cv(s, Sc::U32, ONES, n);
            let c31 = cv(s, Sc::U32, 31, n);
            let inv = s.val(130, Sc::U32, n, &[c31.id, r.id]);
            let none = s.val(170, Sc::Bool, n, &[r.id, all.id]);
            sel(s, none, all, inv)
        })
    }

    pub(super) fn exec_data(&mut self, inst: &Inst) -> R<()> {
        let ops = &inst.operands;
        match inst.opcode {
            OP_MOV => {
                Self::need(inst, 2)?;
                let sc = if inst.saturate() || ops[1].abs || ops[1].neg {
                    Sc::F32
                } else {
                    Sc::U32
                };
                self.alu(inst, 1, sc, |_, a| a[0])
            }
            OP_MOVC => {
                Self::need(inst, 4)?;
                let float = inst.saturate() || ops[2].abs || ops[2].neg || ops[3].abs || ops[3].neg;
                let sc = if float { Sc::F32 } else { Sc::U32 };
                let dst = &ops[0];
                let comps = mask_comps(dst.mask);
                if comps.is_empty() {
                    return Ok(());
                }
                let c = self.read(&ops[1], &comps, Sc::U32)?;
                let a = self.read(&ops[2], &comps, sc)?;
                let b = self.read(&ops[3], &comps, sc)?;
                let z = cv(&mut self.s, Sc::U32, 0, c.n);
                let t = self.s.val(171, Sc::Bool, c.n, &[c.id, z.id]);
                let r = sel(&mut self.s, t, a, b);
                self.write(dst, r, inst.saturate())
            }
            // ---- flottants
            OP_ADD => self.alu(inst, 2, Sc::F32, |s, a| {
                s.val(129, Sc::F32, a[0].n, &[a[0].id, a[1].id])
            }),
            OP_MUL => self.alu(inst, 2, Sc::F32, |s, a| {
                s.val(133, Sc::F32, a[0].n, &[a[0].id, a[1].id])
            }),
            OP_DIV => self.alu(inst, 2, Sc::F32, |s, a| {
                s.val(136, Sc::F32, a[0].n, &[a[0].id, a[1].id])
            }),
            OP_MIN => self.alu(inst, 2, Sc::F32, |s, a| {
                s.glsl_op(Sc::F32, a[0].n, 79, &[a[0].id, a[1].id])
            }),
            OP_MAX => self.alu(inst, 2, Sc::F32, |s, a| {
                s.glsl_op(Sc::F32, a[0].n, 80, &[a[0].id, a[1].id])
            }),
            OP_MAD => self.alu(inst, 3, Sc::F32, |s, a| {
                let m = s.val(133, Sc::F32, a[0].n, &[a[0].id, a[1].id]);
                s.val(129, Sc::F32, a[0].n, &[m.id, a[2].id])
            }),
            OP_RSQ => self.glsl1(inst, 32),
            OP_SQRT => self.glsl1(inst, 31),
            OP_EXP => self.glsl1(inst, 29),
            OP_LOG => self.glsl1(inst, 30),
            OP_FRC => self.glsl1(inst, 10),
            OP_ROUND_NE => self.glsl1(inst, 2),
            OP_ROUND_NI => self.glsl1(inst, 8),
            OP_ROUND_PI => self.glsl1(inst, 9),
            OP_ROUND_Z => self.glsl1(inst, 3),
            OP_RCP => self.alu(inst, 1, Sc::F32, |s, a| {
                let one = cv(s, Sc::F32, 1.0f32.to_bits(), a[0].n);
                s.val(136, Sc::F32, a[0].n, &[one.id, a[0].id])
            }),
            OP_DP2 | OP_DP3 | OP_DP4 => {
                Self::need(inst, 3)?;
                let k = (inst.opcode - OP_DP2 + 2) as usize;
                let comps: Vec<u32> = (0..k as u32).collect();
                let dst = &ops[0];
                let dcomps = mask_comps(dst.mask);
                if dcomps.is_empty() {
                    return Ok(());
                }
                let a = self.read(&ops[1], &comps, Sc::F32)?;
                let b = self.read(&ops[2], &comps, Sc::F32)?;
                let d = self.s.val(148, Sc::F32, 1, &[a.id, b.id]);
                self.write(dst, d, inst.saturate())
            }
            OP_SINCOS => {
                Self::need(inst, 3)?;
                for (k, g) in [(0usize, 13u32), (1usize, 14u32)] {
                    let dst = &ops[k];
                    let comps = mask_comps(dst.mask);
                    if dst.ty == OPERAND_NULL || comps.is_empty() {
                        continue;
                    }
                    let a = self.read(&ops[2], &comps, Sc::F32)?;
                    let r = self.s.glsl_op(Sc::F32, a.n, g, &[a.id]);
                    self.write(dst, r, inst.saturate())?;
                }
                Ok(())
            }
            OP_DERIV_RTX | OP_DERIV_RTY | OP_DERIV_RTX_COARSE | OP_DERIV_RTY_COARSE
            | OP_DERIV_RTX_FINE | OP_DERIV_RTY_FINE => {
                let (spv, cap) = match inst.opcode {
                    OP_DERIV_RTX => (207u16, false),
                    OP_DERIV_RTY => (208, false),
                    OP_DERIV_RTX_COARSE => (213, true),
                    OP_DERIV_RTY_COARSE => (214, true),
                    OP_DERIV_RTX_FINE => (210, true),
                    _ => (211, true),
                };
                if self.stage != ShaderStage::Pixel {
                    return self.alu(inst, 1, Sc::F32, |s, a| cv(s, Sc::F32, 0, a[0].n));
                }
                if cap {
                    self.s.caps.insert(51);
                }
                self.alu(inst, 1, Sc::F32, |s, a| {
                    s.val(spv, Sc::F32, a[0].n, &[a[0].id])
                })
            }
            OP_EQ => self.cmp(inst, Sc::F32, 180),
            OP_NE => self.cmp(inst, Sc::F32, 183),
            OP_LT => self.cmp(inst, Sc::F32, 184),
            OP_GE => self.cmp(inst, Sc::F32, 190),
            // ---- conversions
            OP_ITOF => self.alu(inst, 1, Sc::I32, |s, a| {
                s.val(111, Sc::F32, a[0].n, &[a[0].id])
            }),
            OP_UTOF => self.alu(inst, 1, Sc::U32, |s, a| {
                s.val(112, Sc::F32, a[0].n, &[a[0].id])
            }),
            OP_FTOI => self.alu(inst, 1, Sc::F32, |s, a| Self::ftoi(s, a[0], true)),
            OP_FTOU => self.alu(inst, 1, Sc::F32, |s, a| Self::ftoi(s, a[0], false)),
            OP_F32TOF16 => self.alu(inst, 1, Sc::F32, |s, a| {
                let z = s.c_f32(0.0);
                let mut comps = Vec::new();
                for i in 0..a[0].n {
                    let x = s.extract(a[0], i);
                    let v2 = s.construct(Sc::F32, &[x.id, z]);
                    comps.push(s.glsl_op(Sc::U32, 1, 58, &[v2.id]).id);
                }
                s.construct(Sc::U32, &comps)
            }),
            OP_F16TOF32 => self.alu(inst, 1, Sc::U32, |s, a| {
                let mut comps = Vec::new();
                for i in 0..a[0].n {
                    let x = s.extract(a[0], i);
                    let v2 = s.glsl_op(Sc::F32, 2, 62, &[x.id]);
                    comps.push(s.extract(v2, 0).id);
                }
                s.construct(Sc::F32, &comps)
            }),
            // ---- entiers
            OP_IADD => self.alu(inst, 2, Sc::U32, |s, a| {
                s.val(128, Sc::U32, a[0].n, &[a[0].id, a[1].id])
            }),
            OP_INEG => self.alu(inst, 1, Sc::I32, |s, a| {
                s.val(126, Sc::I32, a[0].n, &[a[0].id])
            }),
            OP_IMUL => self.mul_ext(inst, Sc::I32),
            OP_UMUL => self.mul_ext(inst, Sc::U32),
            OP_IMAD | OP_UMAD => self.alu(inst, 3, Sc::U32, |s, a| {
                let m = s.val(132, Sc::U32, a[0].n, &[a[0].id, a[1].id]);
                s.val(128, Sc::U32, a[0].n, &[m.id, a[2].id])
            }),
            OP_IMIN => self.alu(inst, 2, Sc::I32, |s, a| {
                s.glsl_op(Sc::I32, a[0].n, 39, &[a[0].id, a[1].id])
            }),
            OP_IMAX => self.alu(inst, 2, Sc::I32, |s, a| {
                s.glsl_op(Sc::I32, a[0].n, 42, &[a[0].id, a[1].id])
            }),
            OP_UMIN => self.alu(inst, 2, Sc::U32, |s, a| {
                s.glsl_op(Sc::U32, a[0].n, 38, &[a[0].id, a[1].id])
            }),
            OP_UMAX => self.alu(inst, 2, Sc::U32, |s, a| {
                s.glsl_op(Sc::U32, a[0].n, 41, &[a[0].id, a[1].id])
            }),
            OP_UDIV => self.udiv(inst),
            OP_ISHL => self.shift(inst, Sc::U32, 196),
            OP_ISHR => self.shift(inst, Sc::I32, 195),
            OP_USHR => self.shift(inst, Sc::U32, 194),
            OP_AND => self.alu(inst, 2, Sc::U32, |s, a| {
                s.val(199, Sc::U32, a[0].n, &[a[0].id, a[1].id])
            }),
            OP_OR => self.alu(inst, 2, Sc::U32, |s, a| {
                s.val(197, Sc::U32, a[0].n, &[a[0].id, a[1].id])
            }),
            OP_XOR => self.alu(inst, 2, Sc::U32, |s, a| {
                s.val(198, Sc::U32, a[0].n, &[a[0].id, a[1].id])
            }),
            OP_NOT => self.alu(inst, 1, Sc::U32, |s, a| {
                s.val(200, Sc::U32, a[0].n, &[a[0].id])
            }),
            OP_IEQ => self.cmp(inst, Sc::U32, 170),
            OP_INE => self.cmp(inst, Sc::U32, 171),
            OP_ILT => self.cmp(inst, Sc::I32, 177),
            OP_IGE => self.cmp(inst, Sc::I32, 175),
            OP_ULT => self.cmp(inst, Sc::U32, 176),
            OP_UGE => self.cmp(inst, Sc::U32, 174),
            OP_BFREV => self.alu(inst, 1, Sc::U32, |s, a| {
                s.val(204, Sc::U32, a[0].n, &[a[0].id])
            }),
            OP_COUNTBITS => self.alu(inst, 1, Sc::U32, |s, a| {
                s.val(205, Sc::U32, a[0].n, &[a[0].id])
            }),
            OP_FIRSTBIT_HI => self.firstbit(inst, Sc::U32, 75),
            OP_FIRSTBIT_SHI => self.firstbit(inst, Sc::I32, 74),
            OP_FIRSTBIT_LO => self.alu(inst, 1, Sc::U32, |s, a| {
                s.glsl_op(Sc::U32, a[0].n, 73, &[a[0].id])
            }),
            OP_BFI => self.alu(inst, 4, Sc::U32, |s, a| {
                let n = a[0].n;
                let c31 = cv(s, Sc::U32, 31, n);
                let one = cv(s, Sc::U32, 1, n);
                let w = s.val(199, Sc::U32, n, &[a[0].id, c31.id]);
                let o = s.val(199, Sc::U32, n, &[a[1].id, c31.id]);
                let sh = s.val(196, Sc::U32, n, &[one.id, w.id]);
                let m0 = s.val(130, Sc::U32, n, &[sh.id, one.id]);
                let m = s.val(196, Sc::U32, n, &[m0.id, o.id]);
                let ins = s.val(196, Sc::U32, n, &[a[2].id, o.id]);
                let ins = s.val(199, Sc::U32, n, &[ins.id, m.id]);
                let nm = s.val(200, Sc::U32, n, &[m.id]);
                let keep = s.val(199, Sc::U32, n, &[a[3].id, nm.id]);
                s.val(197, Sc::U32, n, &[ins.id, keep.id])
            }),
            OP_UBFE => self.alu(inst, 3, Sc::U32, |s, a| {
                let n = a[0].n;
                let c31 = cv(s, Sc::U32, 31, n);
                let one = cv(s, Sc::U32, 1, n);
                let w = s.val(199, Sc::U32, n, &[a[0].id, c31.id]);
                let o = s.val(199, Sc::U32, n, &[a[1].id, c31.id]);
                let sh = s.val(196, Sc::U32, n, &[one.id, w.id]);
                let m = s.val(130, Sc::U32, n, &[sh.id, one.id]);
                let v = s.val(194, Sc::U32, n, &[a[2].id, o.id]);
                s.val(199, Sc::U32, n, &[v.id, m.id])
            }),
            OP_IBFE => self.alu(inst, 3, Sc::U32, |s, a| {
                let n = a[0].n;
                let c31 = cv(s, Sc::U32, 31, n);
                let c32 = cv(s, Sc::U32, 32, n);
                let z = cv(s, Sc::U32, 0, n);
                let w = s.val(199, Sc::U32, n, &[a[0].id, c31.id]);
                let o = s.val(199, Sc::U32, n, &[a[1].id, c31.id]);
                let sum = s.val(128, Sc::U32, n, &[w.id, o.id]);
                let fits = s.val(176, Sc::Bool, n, &[sum.id, c32.id]);
                let t = s.val(130, Sc::U32, n, &[c32.id, sum.id]);
                let shl = sel(s, fits, t, z);
                let t2 = s.val(130, Sc::U32, n, &[c32.id, w.id]);
                let shr = sel(s, fits, t2, o);
                let v = s.val(196, Sc::U32, n, &[a[2].id, shl.id]);
                let v = s.val(195, Sc::U32, n, &[v.id, shr.id]);
                let w0 = s.val(170, Sc::Bool, n, &[w.id, z.id]);
                sel(s, w0, z, v)
            }),
            OP_UADDC | OP_USUBB => {
                Self::need(inst, 4)?;
                for k in 0..2usize {
                    let dst = &ops[k];
                    let comps = mask_comps(dst.mask);
                    if dst.ty == OPERAND_NULL || comps.is_empty() {
                        continue;
                    }
                    let a = self.read(&ops[2], &comps, Sc::U32)?;
                    let b = self.read(&ops[3], &comps, Sc::U32)?;
                    let t = self.s.ty(Sc::U32, a.n);
                    let st = self.s.structure(&[t, t]);
                    let op = if inst.opcode == OP_UADDC { 149 } else { 150 };
                    let id = self.s.emit(op, st, &[a.id, b.id]);
                    let r = self.s.val(81, Sc::U32, a.n, &[id, k as u32]);
                    self.write(dst, r, false)?;
                }
                Ok(())
            }
            _ => self.exec_tex(inst),
        }
    }
}
