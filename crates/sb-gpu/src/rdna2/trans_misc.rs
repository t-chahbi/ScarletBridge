//! Instructions diverses : sous-groupes (`v_writelane`, `v_mbcnt`), décalages 64 bits, `s_wqm`,
//! bits scalaires supplémentaires, `v_bfm/bcnt/xor3/add_lshl`, min3/max3/med3 16 bits.

use super::inst::{Instruction, Operand, Src};
use super::translate::{TResult, Tr, TranslateError, unsupported};
use crate::dxbc_to_spirv::spv::{Sc, Val};

const OP_IADD: u16 = 128;
const OP_ISUB: u16 = 130;
const OP_IMUL: u16 = 132;
const OP_IEQ: u16 = 170;
const OP_UGE: u16 = 174;
const OP_ULT: u16 = 176;
const OP_OR: u16 = 197;
const OP_XOR: u16 = 198;
const OP_AND: u16 = 199;
const OP_NOT: u16 = 200;
const OP_SHR: u16 = 194;
const OP_SAR: u16 = 195;
const OP_SHL: u16 = 196;
const OP_BITCNT: u16 = 205;
const G_UMIN: u32 = 38;
const G_SMIN: u32 = 39;
const G_UMAX: u32 = 41;
const G_SMAX: u32 = 42;
const G_FIND_ILSB: u32 = 73;
const G_FIND_UMSB: u32 = 75;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Sh64 {
    Left,
    LogicalRight,
    ArithRight,
}

impl Tr<'_> {
    /// Décalage 64 bits de `(lo, hi)` par `n & 63`, sans décalage ≥ 32 indéfini.
    pub(crate) fn shift64(&mut self, lo: Val, hi: Val, n: Val, kind: Sh64) -> (Val, Val) {
        let (c63, c31, c32, c1, z) = (
            self.cu(63),
            self.cu(31),
            self.cu(32),
            self.cu(1),
            self.cu(0),
        );
        let n = self.op2(OP_AND, Sc::U32, n, c63);
        let big = self.op2(OP_UGE, Sc::Bool, n, c32);
        let s = self.op2(OP_AND, Sc::U32, n, c31);
        let inv = self.op2(OP_ISUB, Sc::U32, c31, s); // 31 - s
        match kind {
            Sh64::Left => {
                let ls = self.op2(OP_SHL, Sc::U32, lo, s);
                let hs = self.op2(OP_SHL, Sc::U32, hi, s);
                let t = self.op2(OP_SHR, Sc::U32, lo, c1);
                let carry = self.op2(OP_SHR, Sc::U32, t, inv);
                let hi_small = self.op2(OP_OR, Sc::U32, hs, carry);
                (self.select(big, z, ls), self.select(big, ls, hi_small))
            }
            Sh64::LogicalRight => {
                let hs = self.op2(OP_SHR, Sc::U32, hi, s);
                let ls = self.op2(OP_SHR, Sc::U32, lo, s);
                let t = self.op2(OP_SHL, Sc::U32, hi, c1);
                let carry = self.op2(OP_SHL, Sc::U32, t, inv);
                let lo_small = self.op2(OP_OR, Sc::U32, ls, carry);
                (self.select(big, hs, lo_small), self.select(big, z, hs))
            }
            Sh64::ArithRight => {
                let hi_i = self.spv.bitcast(hi, Sc::I32);
                let hs = self.spv.val(OP_SAR, Sc::I32, 1, &[hi_i.id, s.id]);
                let hs = self.spv.bitcast(hs, Sc::U32);
                let sign = self.spv.val(OP_SAR, Sc::I32, 1, &[hi_i.id, c31.id]);
                let sign = self.spv.bitcast(sign, Sc::U32);
                let ls = self.op2(OP_SHR, Sc::U32, lo, s);
                let t = self.op2(OP_SHL, Sc::U32, hi, c1);
                let carry = self.op2(OP_SHL, Sc::U32, t, inv);
                let lo_small = self.op2(OP_OR, Sc::U32, ls, carry);
                (self.select(big, hs, lo_small), self.select(big, sign, hs))
            }
        }
    }

    /// Octets 0..3 de `x` : nibbles-quad -> masque « whole quad mode » (`s_wqm`).
    fn wqm32(&mut self, x: Val) -> Val {
        let (c1, c2, c3) = (self.cu(1), self.cu(2), self.cu(3));
        let a = self.op2(OP_SHR, Sc::U32, x, c1);
        let b = self.op2(OP_SHR, Sc::U32, x, c2);
        let c = self.op2(OP_SHR, Sc::U32, x, c3);
        let t = self.op2(OP_OR, Sc::U32, x, a);
        let t = self.op2(OP_OR, Sc::U32, t, b);
        let t = self.op2(OP_OR, Sc::U32, t, c);
        let m = self.cu(0x1111_1111);
        let q = self.op2(OP_AND, Sc::U32, t, m);
        let c15 = self.cu(15);
        self.op2(OP_IMUL, Sc::U32, q, c15)
    }

    fn quadmask32(&mut self, x: Val) -> Val {
        // un bit par quad (16 bits de sortie)
        let w = self.wqm32(x);
        let m = self.cu(0x1111_1111);
        let q = self.op2(OP_AND, Sc::U32, w, m);
        let mut out = self.cu(0);
        for k in 0..8u32 {
            let ck = self.cu(4 * k);
            let b = self.op2(OP_SHR, Sc::U32, q, ck);
            let c1 = self.cu(1);
            let b = self.op2(OP_AND, Sc::U32, b, c1);
            let cs = self.cu(k);
            let placed = self.op2(OP_SHL, Sc::U32, b, cs);
            out = self.op2(OP_OR, Sc::U32, out, placed);
        }
        out
    }

    /// SOP2/SOP1/SOPC 32 bits supplémentaires. Retourne `Ok(())` si géré.
    pub(crate) fn salu_ext32(
        &mut self,
        i: &Instruction,
        name: &str,
        dst: &Operand,
        srcs: &[Operand],
    ) -> TResult<()> {
        let a = if srcs.is_empty() {
            None
        } else {
            Some(self.src_u32(&srcs[0])?)
        };
        let b = if srcs.len() > 1 {
            Some(self.src_u32(&srcs[1])?)
        } else {
            None
        };
        let r = match (name, a, b) {
            ("s_wqm_b32", Some(a), _) => {
                let r = self.wqm32(a);
                let nz = self.ne0(r);
                self.scc_set(nz);
                r
            }
            ("s_quadmask_b32", Some(a), _) => {
                let r = self.quadmask32(a);
                let nz = self.ne0(r);
                self.scc_set(nz);
                r
            }
            ("s_bcnt0_i32_b32", Some(a), _) => {
                let na = self.op1(OP_NOT, Sc::U32, a);
                let r = self.op1(OP_BITCNT, Sc::U32, na);
                let nz = self.ne0(r);
                self.scc_set(nz);
                r
            }
            ("s_ff0_i32_b32", Some(a), _) => {
                let na = self.op1(OP_NOT, Sc::U32, a);
                let r = self.glsl(Sc::I32, G_FIND_ILSB, &[na]);
                self.spv.bitcast(r, Sc::U32)
            }
            ("s_flbit_i32", Some(a), _) => {
                // nombre de bits de signe de tête ; -1 si 0 ou -1
                let ai = self.spv.bitcast(a, Sc::I32);
                let msb = self.glsl(Sc::I32, 74, &[ai]);
                let c31 = self.cu(31);
                let m = self.spv.bitcast(msb, Sc::U32);
                let lz = self.op2(OP_ISUB, Sc::U32, c31, m);
                let m1 = self.cu(u32::MAX);
                let neg1 = self.op2(OP_IEQ, Sc::Bool, m, m1);
                self.select(neg1, m1, lz)
            }
            ("s_bitset0_b32" | "s_bitset1_b32", Some(a), _) => {
                // dst |= 1<<(src0&31) ou dst &= ~(1<<...)
                let old = self.src_u32(dst)?;
                let (c31, one) = (self.cu(31), self.cu(1));
                let sh = self.op2(OP_AND, Sc::U32, a, c31);
                let bit = self.op2(OP_SHL, Sc::U32, one, sh);
                if name == "s_bitset1_b32" {
                    self.op2(OP_OR, Sc::U32, old, bit)
                } else {
                    let nb = self.op1(OP_NOT, Sc::U32, bit);
                    self.op2(OP_AND, Sc::U32, old, nb)
                }
            }
            (
                "s_lshl1_add_u32" | "s_lshl2_add_u32" | "s_lshl3_add_u32" | "s_lshl4_add_u32",
                Some(a),
                Some(b),
            ) => {
                let k = u32::from(name.as_bytes()[7] - b'0');
                let ck = self.cu(k);
                let sh = self.op2(OP_SHL, Sc::U32, a, ck);
                let r = self.op2(OP_IADD, Sc::U32, sh, b);
                let c = self.op2(OP_ULT, Sc::Bool, r, sh);
                self.scc_set(c);
                r
            }
            ("s_bfe_u32" | "s_bfe_i32", _, _) => return unsupported(i),
            _ => return unsupported(i),
        };
        self.write_s(dst, r)
    }

    /// SOP1/SOP2/SOPC 64 bits supplémentaires.
    pub(crate) fn salu64_ext(
        &mut self,
        i: &Instruction,
        name: &str,
        dst: &Operand,
        srcs: &[Operand],
    ) -> TResult<()> {
        match name {
            "s_lshl_b64" | "s_lshr_b64" | "s_ashr_i64" if srcs.len() == 2 => {
                let (lo, hi) = self.src_pair(&srcs[0])?;
                let n = self.src_u32(&srcs[1])?;
                let kind = match name {
                    "s_lshl_b64" => Sh64::Left,
                    "s_lshr_b64" => Sh64::LogicalRight,
                    _ => Sh64::ArithRight,
                };
                let (l, h) = self.shift64(lo, hi, n, kind);
                let any = self.op2(OP_OR, Sc::U32, l, h);
                let nz = self.ne0(any);
                self.scc_set(nz);
                self.write_s_pair(dst, l, h)
            }
            "s_wqm_b64" => {
                let (lo, hi) = self.src_pair(&srcs[0])?;
                let (l, h) = (self.wqm32(lo), self.wqm32(hi));
                let any = self.op2(OP_OR, Sc::U32, l, h);
                let nz = self.ne0(any);
                self.scc_set(nz);
                self.write_s_pair(dst, l, h)
            }
            "s_brev_b64" => {
                let (lo, hi) = self.src_pair(&srcs[0])?;
                let (l, h) = (self.op1(204, Sc::U32, hi), self.op1(204, Sc::U32, lo));
                self.write_s_pair(dst, l, h)
            }
            "s_bcnt0_i32_b64" => {
                let (lo, hi) = self.src_pair(&srcs[0])?;
                let (nl, nh) = (self.op1(OP_NOT, Sc::U32, lo), self.op1(OP_NOT, Sc::U32, hi));
                let (cl, ch) = (
                    self.op1(OP_BITCNT, Sc::U32, nl),
                    self.op1(OP_BITCNT, Sc::U32, nh),
                );
                let r = self.op2(OP_IADD, Sc::U32, cl, ch);
                let nz = self.ne0(r);
                self.scc_set(nz);
                self.write_s(dst, r)
            }
            "s_ff1_i32_b64" | "s_flbit_i32_b64" => {
                let (lo, hi) = self.src_pair(&srcs[0])?;
                let (c32, m1, z) = (self.cu(32), self.cu(u32::MAX), self.cu(0));
                let lo_zero = self.op2(OP_IEQ, Sc::Bool, lo, z);
                let hi_zero = self.op2(OP_IEQ, Sc::Bool, hi, z);
                let r = if name == "s_ff1_i32_b64" {
                    let a = self.glsl(Sc::I32, G_FIND_ILSB, &[lo]);
                    let b = self.glsl(Sc::I32, G_FIND_ILSB, &[hi]);
                    let (a, b) = (self.spv.bitcast(a, Sc::U32), self.spv.bitcast(b, Sc::U32));
                    let b32 = self.op2(OP_IADD, Sc::U32, b, c32);
                    let both = self.bool_and(lo_zero, hi_zero);
                    let t = self.select(lo_zero, b32, a);
                    self.select(both, m1, t)
                } else {
                    let a = self.glsl(Sc::I32, G_FIND_UMSB, &[lo]);
                    let b = self.glsl(Sc::I32, G_FIND_UMSB, &[hi]);
                    let (a, b) = (self.spv.bitcast(a, Sc::U32), self.spv.bitcast(b, Sc::U32));
                    let (c31, c63) = (self.cu(31), self.cu(63));
                    let lz_hi = self.op2(OP_ISUB, Sc::U32, c31, b);
                    let lz_lo = self.op2(OP_ISUB, Sc::U32, c63, a);
                    let both = self.bool_and(lo_zero, hi_zero);
                    let t = self.select(hi_zero, lz_lo, lz_hi);
                    self.select(both, m1, t)
                };
                self.write_s(dst, r)
            }
            "s_bitset0_b64" | "s_bitset1_b64" => {
                let (lo, hi) = self.src_pair(&Operand::new(dst.src, 2))?;
                let n = self.src_u32(&srcs[0])?;
                let (c63, one, z) = (self.cu(63), self.cu(1), self.cu(0));
                let (olo, ohi) = (one, z);
                let nn = self.and_val(n, c63);
                let (bl, bh) = self.shift64(olo, ohi, nn, Sh64::Left);
                let (l, h) = if name == "s_bitset1_b64" {
                    (
                        self.op2(OP_OR, Sc::U32, lo, bl),
                        self.op2(OP_OR, Sc::U32, hi, bh),
                    )
                } else {
                    let (nl, nh) = (self.op1(OP_NOT, Sc::U32, bl), self.op1(OP_NOT, Sc::U32, bh));
                    (
                        self.op2(OP_AND, Sc::U32, lo, nl),
                        self.op2(OP_AND, Sc::U32, hi, nh),
                    )
                };
                self.write_s_pair(dst, l, h)
            }
            _ => unsupported(i),
        }
    }

    fn and_val(&mut self, a: Val, b: Val) -> Val {
        self.op2(OP_AND, Sc::U32, a, b)
    }

    pub(crate) fn valu_misc(&mut self, i: &Instruction, name: &str) -> TResult<()> {
        let s = i.srcs();
        let dst = |t: &Self| -> TResult<Operand> {
            let _ = t;
            i.dst()
                .copied()
                .ok_or_else(|| TranslateError::Unsupported(i.name.into()))
        };
        match name {
            "v_writelane_b32" => {
                // vdst[lane == src1] = src0 ; non masqué par EXEC
                let d = dst(self)?;
                let Src::Vgpr(n) = d.src else {
                    return unsupported(i);
                };
                let (val, lane) = (self.src_u32(&s[0])?, self.src_u32(&s[1])?);
                let me = self.lane_id();
                let c = if self.wave.lanes() == 64 { 63 } else { 31 };
                let cm = self.cu(c);
                let l = self.op2(OP_AND, Sc::U32, lane, cm);
                let hit = self.op2(OP_IEQ, Sc::Bool, me, l);
                let var = self.vvar(usize::from(n));
                let old = self.load_u32(var);
                let r = self.select(hit, val, old);
                self.store_u32(var, r);
                Ok(())
            }
            "v_mbcnt_lo_u32_b32" | "v_mbcnt_hi_u32_b32" => {
                let (mask, acc) = (self.src_u32(&s[0])?, self.src_u32(&s[1])?);
                let me = self.lane_id();
                let (c31, c32, one, z, ffff) = (
                    self.cu(31),
                    self.cu(32),
                    self.cu(1),
                    self.cu(0),
                    self.cu(u32::MAX),
                );
                let lo = name.contains("_lo_");
                // bits strictement sous la lane dans le demi-mot concerné
                let sh = self.op2(OP_AND, Sc::U32, me, c31);
                let below = {
                    let b = self.op2(OP_SHL, Sc::U32, one, sh);
                    self.op2(OP_ISUB, Sc::U32, b, one)
                };
                let in_lo_half = self.op2(OP_ULT, Sc::Bool, me, c32);
                let m = if lo {
                    self.select(in_lo_half, below, ffff)
                } else {
                    self.select(in_lo_half, z, below)
                };
                let masked = self.op2(OP_AND, Sc::U32, mask, m);
                let c = self.op1(OP_BITCNT, Sc::U32, masked);
                let r = self.op2(OP_IADD, Sc::U32, c, acc);
                self.write_dst_u(i, r)
            }
            "v_bfm_b32" => {
                let (a, b) = (self.src_u32(&s[0])?, self.src_u32(&s[1])?);
                let (c31, one) = (self.cu(31), self.cu(1));
                let (w, o) = (
                    self.op2(OP_AND, Sc::U32, a, c31),
                    self.op2(OP_AND, Sc::U32, b, c31),
                );
                let t = self.op2(OP_SHL, Sc::U32, one, w);
                let m = self.op2(OP_ISUB, Sc::U32, t, one);
                let r = self.op2(OP_SHL, Sc::U32, m, o);
                self.write_dst_u(i, r)
            }
            "v_bcnt_u32_b32" => {
                let (a, b) = (self.src_u32(&s[0])?, self.src_u32(&s[1])?);
                let c = self.op1(OP_BITCNT, Sc::U32, a);
                let r = self.op2(OP_IADD, Sc::U32, c, b);
                self.write_dst_u(i, r)
            }
            "v_xor3_b32" | "v_add_lshl_u32" => {
                let (a, b, c) = (
                    self.src_u32(&s[0])?,
                    self.src_u32(&s[1])?,
                    self.src_u32(&s[2])?,
                );
                let r = if name == "v_xor3_b32" {
                    let t = self.op2(OP_XOR, Sc::U32, a, b);
                    self.op2(OP_XOR, Sc::U32, t, c)
                } else {
                    let t = self.op2(OP_IADD, Sc::U32, a, b);
                    let c31 = self.cu(31);
                    let sh = self.op2(OP_AND, Sc::U32, c, c31);
                    self.op2(OP_SHL, Sc::U32, t, sh)
                };
                self.write_dst_u(i, r)
            }
            "v_add_nc_i32" | "v_sub_nc_i32" => {
                let (a, b) = (self.src_u32(&s[0])?, self.src_u32(&s[1])?);
                let r = self.op2(
                    if name == "v_add_nc_i32" {
                        OP_IADD
                    } else {
                        OP_ISUB
                    },
                    Sc::U32,
                    a,
                    b,
                );
                self.write_dst_u(i, r)
            }
            "v_min3_i16" | "v_min3_u16" | "v_max3_i16" | "v_max3_u16" | "v_med3_i16"
            | "v_med3_u16" => {
                let signed = name.contains("_i16");
                let sc = if signed { Sc::I32 } else { Sc::U32 };
                let mut v = Vec::new();
                for (k, o) in s.iter().enumerate().take(3) {
                    let x = self.src_i16(o, i.mods.op_sel >> k & 1 != 0, signed)?;
                    v.push(self.spv.bitcast(x, sc));
                }
                let (mn, mx) = if signed {
                    (G_SMIN, G_SMAX)
                } else {
                    (G_UMIN, G_UMAX)
                };
                let r = if name.starts_with("v_med3") {
                    let (lo, hi) = (
                        self.glsl(sc, mn, &[v[0], v[1]]),
                        self.glsl(sc, mx, &[v[0], v[1]]),
                    );
                    let m = self.glsl(sc, mn, &[hi, v[2]]);
                    self.glsl(sc, mx, &[lo, m])
                } else {
                    let g = if name.starts_with("v_min3") { mn } else { mx };
                    let t = self.glsl(sc, g, &[v[0], v[1]]);
                    self.glsl(sc, g, &[t, v[2]])
                };
                let r = self.spv.bitcast(r, Sc::U32);
                self.write16(i, r)
            }
            "v_mad_u32_u16" | "v_mad_i32_i16" => {
                let signed = name.contains("i32_i16");
                let a = self.src_i16(&s[0], i.mods.op_sel & 1 != 0, signed)?;
                let b = self.src_i16(&s[1], i.mods.op_sel >> 1 & 1 != 0, signed)?;
                let c = self.src_u32(&s[2])?;
                let m = self.op2(OP_IMUL, Sc::U32, a, b);
                let r = self.op2(OP_IADD, Sc::U32, m, c);
                self.write_dst_u(i, r)
            }
            "v_cvt_pk_u16_u32" | "v_cvt_pk_i16_i32" => {
                let (a, b) = (self.src_u32(&s[0])?, self.src_u32(&s[1])?);
                let signed = name.contains("i16_i32");
                let (sc, lo, hi) = if signed {
                    (Sc::I32, -32768i32 as u32, 32767u32)
                } else {
                    (Sc::U32, 0, 65535)
                };
                let clamp = |t: &mut Self, v: Val| {
                    let x = t.spv.bitcast(v, sc);
                    let (l, h) = (t.cu(lo), t.cu(hi));
                    let (l, h) = (t.spv.bitcast(l, sc), t.spv.bitcast(h, sc));
                    let m = t.glsl(sc, if signed { G_SMAX } else { G_UMAX }, &[x, l]);
                    let m = t.glsl(sc, if signed { G_SMIN } else { G_UMIN }, &[m, h]);
                    t.spv.bitcast(m, Sc::U32)
                };
                let (x, y) = (clamp(self, a), clamp(self, b));
                let (m, c16) = (self.cu(0xFFFF), self.cu(16));
                let xl = self.op2(OP_AND, Sc::U32, x, m);
                let yh = self.op2(OP_SHL, Sc::U32, y, c16);
                let r = self.op2(OP_OR, Sc::U32, xl, yh);
                self.write_dst_u(i, r)
            }
            "v_lshlrev_b64" | "v_lshrrev_b64" | "v_ashrrev_i64" => {
                let d = dst(self)?;
                let Src::Vgpr(n) = d.src else {
                    return unsupported(i);
                };
                let amt = self.src_u32(&s[0])?;
                let (lo, hi) = self.src_pair(&s[1])?;
                let kind = match name {
                    "v_lshlrev_b64" => Sh64::Left,
                    "v_lshrrev_b64" => Sh64::LogicalRight,
                    _ => Sh64::ArithRight,
                };
                let (l, h) = self.shift64(lo, hi, amt, kind);
                self.write_v(usize::from(n), l);
                self.write_v(usize::from(n) + 1, h);
                Ok(())
            }
            _ => self.valu_f64(i, name),
        }
    }
}
