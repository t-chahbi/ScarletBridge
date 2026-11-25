//! f16 / i16 / u16, VOP3P (math packée, dot, fma_mix), classification, frexp, perm, mad 64 bits.
//!
//! L'arithmétique f16 est **émulée en f32** (conversion `UnpackHalf2x16`, calcul f32, arrondi
//! `PackHalf2x16`) : aucune capacité `Float16`/`Int16` n'est requise. Pour add/sub/mul/div/sqrt le
//! double arrondi est exact (24 ≥ 2·11+2 bits) ; pour fma/mad il peut différer d'1 ulp f16.

use super::inst::{Instruction, Operand, Src};
use super::translate::{TResult, Tr, TranslateError, unsupported};
use crate::dxbc_to_spirv::spv::{Sc, Val};

const OP_IADD: u16 = 128;
const OP_FADD: u16 = 129;
const OP_ISUB: u16 = 130;
const OP_FSUB: u16 = 131;
const OP_IMUL: u16 = 132;
const OP_FMUL: u16 = 133;
const OP_FDIV: u16 = 136;
const OP_IEQ: u16 = 170;
const OP_INE: u16 = 171;
const OP_OR: u16 = 197;
const OP_AND: u16 = 199;
const OP_NOT: u16 = 200;
const OP_SHR: u16 = 194;
const OP_SAR: u16 = 195;
const OP_SHL: u16 = 196;
const OP_BFSEXT: u16 = 202;
const OP_BFUEXT: u16 = 203;
const G_RNDNE: u32 = 2;
const G_TRUNC: u32 = 3;
const G_FLOOR: u32 = 8;
const G_CEIL: u32 = 9;
const G_FRACT: u32 = 10;
const G_SIN: u32 = 13;
const G_COS: u32 = 14;
const G_EXP2: u32 = 29;
const G_LOG2: u32 = 30;
const G_SQRT: u32 = 31;
const G_RSQRT: u32 = 32;
const G_FMIN: u32 = 37;
const G_UMIN: u32 = 38;
const G_SMIN: u32 = 39;
const G_FMAX: u32 = 40;
const G_UMAX: u32 = 41;
const G_SMAX: u32 = 42;
const G_FCLAMP: u32 = 43;
const G_FMA: u32 = 50;
const G_FREXP_STRUCT: u32 = 52;
const G_LDEXP: u32 = 53;
const G_PACK_HALF: u32 = 58;
const G_PACK_UNORM2: u32 = 57;
const G_PACK_SNORM2: u32 = 56;
const G_UNPACK_HALF: u32 = 62;
const G_FIND_ILSB: u32 = 73;
const G_FIND_SMSB: u32 = 74;

fn need<T>(o: Option<T>, i: &Instruction) -> TResult<T> {
    o.ok_or_else(|| TranslateError::Unsupported(i.name.into()))
}

impl Tr<'_> {
    // ------------------------------------------------------------ primitives f16
    /// Demi-mot (0 = bas, 1 = haut) d'un mot de bits, converti en f32.
    pub(crate) fn half_of(&mut self, bits: Val, hi: bool) -> Val {
        let v2 = self.spv.glsl_op(Sc::F32, 2, G_UNPACK_HALF, &[bits.id]);
        self.spv.extract(v2, u32::from(hi))
    }

    /// f32 -> bits f16 dans les 16 bits bas.
    pub(crate) fn f2h(&mut self, f: Val) -> Val {
        let z = self.cf(0.0);
        let v2 = self.spv.construct(Sc::F32, &[f.id, z.id]);
        self.spv.glsl_op(Sc::U32, 1, G_PACK_HALF, &[v2.id])
    }

    pub(crate) fn pack2h(&mut self, lo: Val, hi: Val) -> Val {
        let v2 = self.spv.construct(Sc::F32, &[lo.id, hi.id]);
        self.spv.glsl_op(Sc::U32, 1, G_PACK_HALF, &[v2.id])
    }

    /// Source f16 (en f32) avec `abs`/`neg`.
    pub(crate) fn src_half(&mut self, o: &Operand, hi: bool) -> TResult<Val> {
        let mut f = match o.src {
            Src::InlineFloat(c) => self.cf(Src::inline_float_value(c)),
            Src::Literal(v) => {
                let b = self.cu(v & 0xFFFF);
                self.half_of(b, false)
            }
            _ => {
                let b = self.src_u32(o)?;
                self.half_of(b, hi)
            }
        };
        if o.abs {
            f = self.glsl(Sc::F32, 4, &[f]);
        }
        if o.neg {
            f = self.op1(127, Sc::F32, f);
        }
        Ok(f)
    }

    /// Source entière 16 bits étendue (signe ou zéro) vers 32 bits (bits dans un `u32`).
    pub(crate) fn src_i16(&mut self, o: &Operand, hi: bool, signed: bool) -> TResult<Val> {
        let b = self.src_u32(o)?;
        Ok(self.ext16(b, hi, signed))
    }

    pub(crate) fn ext16(&mut self, b: Val, hi: bool, signed: bool) -> Val {
        let (off, w) = (self.cu(if hi { 16 } else { 0 }), self.cu(16));
        if signed {
            let bi = self.spv.bitcast(b, Sc::I32);
            let r = self.spv.val(OP_BFSEXT, Sc::I32, 1, &[bi.id, off.id, w.id]);
            self.spv.bitcast(r, Sc::U32)
        } else {
            self.spv.val(OP_BFUEXT, Sc::U32, 1, &[b.id, off.id, w.id])
        }
    }

    fn omod_clamp(&mut self, i: &Instruction, mut r: Val) -> Val {
        match i.mods.omod {
            1 => r = self.fmul_c(r, 2.0),
            2 => r = self.fmul_c(r, 4.0),
            3 => r = self.fmul_c(r, 0.5),
            _ => {}
        }
        if i.mods.clamp {
            let (z, o) = (self.cf(0.0), self.cf(1.0));
            r = self.glsl(Sc::F32, G_FCLAMP, &[r, z, o]);
        }
        r
    }

    fn fmul_c(&mut self, r: Val, k: f32) -> Val {
        let c = self.cf(k);
        self.op2(OP_FMUL, Sc::F32, r, c)
    }

    /// Écrit un résultat 16 bits (bits dans les 16 bas de `bits16`) : bas (zéro haut) ou haut
    /// (bas préservé) selon `op_sel[3]` de VOP3.
    pub(crate) fn write16(&mut self, i: &Instruction, bits16: Val) -> TResult<()> {
        let d = *need(i.dst(), i)?;
        let Src::Vgpr(n) = d.src else {
            return unsupported(i);
        };
        let m = self.cu(0xFFFF);
        let low = self.op2(OP_AND, Sc::U32, bits16, m);
        let hi = i.mods.op_sel & 8 != 0;
        let v = if hi {
            let var = self.vvar(usize::from(n));
            let old = self.load_u32(var);
            let c16 = self.cu(16);
            let sh = self.op2(OP_SHL, Sc::U32, low, c16);
            let keep = self.op2(OP_AND, Sc::U32, old, m);
            self.op2(OP_OR, Sc::U32, keep, sh)
        } else {
            low
        };
        self.write_v(usize::from(n), v);
        Ok(())
    }

    fn finish_half(&mut self, i: &Instruction, r: Val) -> TResult<()> {
        let r = self.omod_clamp(i, r);
        let h = self.f2h(r);
        self.write16(i, h)
    }

    fn sel(i: &Instruction, s: usize) -> bool {
        i.mods.op_sel >> s & 1 != 0
    }

    // ------------------------------------------------------------ entrée VALU étendue
    pub(crate) fn valu_ext(&mut self, i: &Instruction, name: &str) -> TResult<()> {
        let s = i.srcs();
        let fh = |t: &mut Self, k: usize| -> TResult<Val> {
            let o = need(s.get(k), i)?;
            t.src_half(o, Self::sel(i, k))
        };
        match name {
            "v_add_f16" | "v_sub_f16" | "v_subrev_f16" | "v_mul_f16" | "v_min_f16"
            | "v_max_f16" => {
                let (a, b) = (fh(self, 0)?, fh(self, 1)?);
                let r = match name {
                    "v_add_f16" => self.op2(OP_FADD, Sc::F32, a, b),
                    "v_sub_f16" => self.op2(OP_FSUB, Sc::F32, a, b),
                    "v_subrev_f16" => self.op2(OP_FSUB, Sc::F32, b, a),
                    "v_mul_f16" => self.op2(OP_FMUL, Sc::F32, a, b),
                    "v_min_f16" => self.glsl(Sc::F32, G_FMIN, &[a, b]),
                    _ => self.glsl(Sc::F32, G_FMAX, &[a, b]),
                };
                self.finish_half(i, r)
            }
            "v_fma_f16" | "v_mad_f16" => {
                let (a, b, c) = (fh(self, 0)?, fh(self, 1)?, fh(self, 2)?);
                let r = self.glsl(Sc::F32, G_FMA, &[a, b, c]);
                self.finish_half(i, r)
            }
            "v_fmac_f16" => {
                let (a, b) = (fh(self, 0)?, fh(self, 1)?);
                let d = *need(i.dst(), i)?;
                let acc = self.src_half(&Operand::new(d.src, 1), false)?;
                let r = self.glsl(Sc::F32, G_FMA, &[a, b, acc]);
                self.finish_half(i, r)
            }
            "v_fmamk_f16" | "v_fmaak_f16" => {
                let (a, k, b) = if name == "v_fmamk_f16" {
                    (fh(self, 0)?, fh(self, 1)?, fh(self, 2)?)
                } else {
                    (fh(self, 0)?, fh(self, 2)?, fh(self, 1)?)
                };
                let r = if name == "v_fmamk_f16" {
                    self.glsl(Sc::F32, G_FMA, &[a, k, b])
                } else {
                    self.glsl(Sc::F32, G_FMA, &[a, b, k])
                };
                self.finish_half(i, r)
            }
            "v_med3_f16" | "v_min3_f16" | "v_max3_f16" => {
                let (a, b, c) = (fh(self, 0)?, fh(self, 1)?, fh(self, 2)?);
                let r = if name == "v_med3_f16" {
                    let (lo, hi) = (
                        self.glsl(Sc::F32, G_FMIN, &[a, b]),
                        self.glsl(Sc::F32, G_FMAX, &[a, b]),
                    );
                    let m = self.glsl(Sc::F32, G_FMIN, &[hi, c]);
                    self.glsl(Sc::F32, G_FMAX, &[lo, m])
                } else {
                    let g = if name == "v_min3_f16" { G_FMIN } else { G_FMAX };
                    let t = self.glsl(Sc::F32, g, &[a, b]);
                    self.glsl(Sc::F32, g, &[t, c])
                };
                self.finish_half(i, r)
            }
            "v_ldexp_f16" => {
                let a = fh(self, 0)?;
                let e = self.src_i16(&s[1], false, true)?;
                let ei = self.spv.bitcast(e, Sc::I32);
                let r = self.spv.glsl_op(Sc::F32, 1, G_LDEXP, &[a.id, ei.id]);
                self.finish_half(i, r)
            }
            "v_rcp_f16" | "v_sqrt_f16" | "v_rsq_f16" | "v_log_f16" | "v_exp_f16"
            | "v_floor_f16" | "v_ceil_f16" | "v_trunc_f16" | "v_rndne_f16" | "v_fract_f16"
            | "v_sin_f16" | "v_cos_f16" => {
                let a = fh(self, 0)?;
                let r = match name {
                    "v_rcp_f16" => {
                        let one = self.cf(1.0);
                        self.op2(OP_FDIV, Sc::F32, one, a)
                    }
                    "v_sin_f16" | "v_cos_f16" => {
                        let tau = self.cf(std::f32::consts::TAU);
                        let x = self.op2(OP_FMUL, Sc::F32, a, tau);
                        self.glsl(
                            Sc::F32,
                            if name == "v_sin_f16" { G_SIN } else { G_COS },
                            &[x],
                        )
                    }
                    _ => {
                        let g = match name {
                            "v_sqrt_f16" => G_SQRT,
                            "v_rsq_f16" => G_RSQRT,
                            "v_log_f16" => G_LOG2,
                            "v_exp_f16" => G_EXP2,
                            "v_floor_f16" => G_FLOOR,
                            "v_ceil_f16" => G_CEIL,
                            "v_trunc_f16" => G_TRUNC,
                            "v_rndne_f16" => G_RNDNE,
                            _ => G_FRACT,
                        };
                        self.glsl(Sc::F32, g, &[a])
                    }
                };
                self.finish_half(i, r)
            }
            "v_cvt_f16_u16" | "v_cvt_f16_i16" => {
                let signed = name.ends_with("i16");
                let x = self.src_i16(&s[0], Self::sel(i, 0), signed)?;
                let r = if signed {
                    let xi = self.spv.bitcast(x, Sc::I32);
                    self.spv.val(111, Sc::F32, 1, &[xi.id])
                } else {
                    self.spv.val(112, Sc::F32, 1, &[x.id])
                };
                self.finish_half(i, r)
            }
            "v_cvt_u16_f16" | "v_cvt_i16_f16" => {
                let a = fh(self, 0)?;
                let r = if name == "v_cvt_i16_f16" {
                    let c = self.spv.val(110, Sc::I32, 1, &[a.id]);
                    self.spv.bitcast(c, Sc::U32)
                } else {
                    self.spv.val(109, Sc::U32, 1, &[a.id])
                };
                self.write16(i, r)
            }
            "v_pack_b32_f16" => {
                let (a, b) = (fh(self, 0)?, fh(self, 1)?);
                let r = self.pack2h(a, b);
                let d = *need(i.dst(), i)?;
                let Src::Vgpr(n) = d.src else {
                    return unsupported(i);
                };
                self.write_v(usize::from(n), r);
                Ok(())
            }
            // --- entiers 16 bits (VOP2 e64 / VOP3)
            "v_add_nc_u16" | "v_sub_nc_u16" | "v_mul_lo_u16" | "v_min_u16" | "v_max_u16"
            | "v_min_i16" | "v_max_i16" | "v_lshlrev_b16" | "v_lshrrev_b16" | "v_ashrrev_i16"
            | "v_add_nc_i16" | "v_sub_nc_i16" => {
                let signed = name.contains("i16");
                let (a, b) = (
                    self.src_i16(&s[0], Self::sel(i, 0), signed)?,
                    self.src_i16(&s[1], Self::sel(i, 1), signed)?,
                );
                let r = self.int16_binop(name, a, b, signed);
                self.write16(i, r)
            }
            "v_mad_u16" | "v_mad_i16" => {
                let signed = name.ends_with("i16");
                let a = self.src_i16(&s[0], Self::sel(i, 0), signed)?;
                let b = self.src_i16(&s[1], Self::sel(i, 1), signed)?;
                let c = self.src_i16(&s[2], Self::sel(i, 2), signed)?;
                let m = self.op2(OP_IMUL, Sc::U32, a, b);
                let r = self.op2(OP_IADD, Sc::U32, m, c);
                self.write16(i, r)
            }
            // --- conversions / bits divers
            "v_cvt_rpi_i32_f32" | "v_cvt_flr_i32_f32" => {
                let a = self.fargs(i, 1)?;
                let x = if name == "v_cvt_rpi_i32_f32" {
                    let h = self.cf(0.5);
                    let t = self.op2(OP_FADD, Sc::F32, a[0], h);
                    self.glsl(Sc::F32, G_FLOOR, &[t])
                } else {
                    self.glsl(Sc::F32, G_FLOOR, &[a[0]])
                };
                let r = self.spv.val(110, Sc::I32, 1, &[x.id]);
                self.write_dst_i(i, r)
            }
            "v_ffbl_b32" => {
                let a = self.src_u32(&s[0])?;
                let r = self.glsl(Sc::I32, G_FIND_ILSB, &[a]);
                self.write_dst_i(i, r)
            }
            "v_ffbh_i32" => {
                let a = self.src_u32(&s[0])?;
                let ai = self.spv.bitcast(a, Sc::I32);
                let msb = self.glsl(Sc::I32, G_FIND_SMSB, &[ai]);
                let c31 = self.spv.c_i32(31);
                let c31 = Val {
                    id: c31,
                    sc: Sc::I32,
                    n: 1,
                };
                let lz = self.spv.val(OP_ISUB, Sc::I32, 1, &[c31.id, msb.id]);
                let m1 = Val {
                    id: self.spv.c_i32(-1),
                    sc: Sc::I32,
                    n: 1,
                };
                let neg1 = self.op2(OP_IEQ, Sc::Bool, msb, m1);
                let r = self.select(neg1, m1, lz);
                self.write_dst_i(i, r)
            }
            "v_mul_hi_u32_u24" | "v_mul_hi_i32_i24" => {
                let signed = name.contains("i32_i24");
                let a = self.uargs(i, 2)?;
                let (z, w) = (self.cu(0), self.cu(24));
                let ex = |t: &mut Self, v: Val| {
                    if signed {
                        let vi = t.spv.bitcast(v, Sc::I32);
                        let r = t.spv.val(OP_BFSEXT, Sc::I32, 1, &[vi.id, z.id, w.id]);
                        t.spv.bitcast(r, Sc::U32)
                    } else {
                        t.spv.val(OP_BFUEXT, Sc::U32, 1, &[v.id, z.id, w.id])
                    }
                };
                let (x, y) = (ex(self, a[0]), ex(self, a[1]));
                let r = self.mul_hi(x, y, signed);
                self.write_dst_u(i, r)
            }
            "v_frexp_mant_f32" | "v_frexp_exp_i32_f32" => {
                let a = self.fargs(i, 1)?;
                let f32t = self.spv.ty(Sc::F32, 1);
                let i32t = self.spv.ty(Sc::I32, 1);
                let st = self.spv.structure(&[f32t, i32t]);
                let glsl = self.spv.glsl;
                let r = self.spv.emit(12, st, &[glsl, G_FREXP_STRUCT, a[0].id]);
                if name == "v_frexp_mant_f32" {
                    let m = self.spv.val(81, Sc::F32, 1, &[r, 0]);
                    self.write_dst_f(i, m)
                } else {
                    let e = self.spv.val(81, Sc::I32, 1, &[r, 1]);
                    self.write_dst_i(i, e)
                }
            }
            "v_cvt_pknorm_i16_f32" | "v_cvt_pknorm_u16_f32" => {
                let a = self.fargs(i, 2)?;
                let v2 = self.spv.construct(Sc::F32, &[a[0].id, a[1].id]);
                let g = if name.contains("_i16") {
                    G_PACK_SNORM2
                } else {
                    G_PACK_UNORM2
                };
                let r = self.spv.glsl_op(Sc::U32, 1, g, &[v2.id]);
                self.write_dst_u(i, r)
            }
            "v_cvt_pk_u8_f32" => {
                // dst = (dst & ~(0xff << 8*sel)) | (clamp_u8(round(src0)) << 8*sel)
                let a = self.fargs(i, 1)?;
                let u = self.uargs(i, 3)?;
                let (z, m255) = (self.cf(0.0), self.cf(255.0));
                let c = self.glsl(Sc::F32, G_FCLAMP, &[a[0], z, m255]);
                let r = self.glsl(Sc::F32, G_RNDNE, &[c]);
                let b = self.spv.val(109, Sc::U32, 1, &[r.id]);
                let c3 = self.cu(3);
                let sel = self.op2(OP_AND, Sc::U32, u[1], c3);
                let c8 = self.cu(8);
                let sh = self.op2(OP_IMUL, Sc::U32, sel, c8);
                let ff = self.cu(0xFF);
                let mask = self.op2(OP_SHL, Sc::U32, ff, sh);
                let nm = self.op1(OP_NOT, Sc::U32, mask);
                let kept = self.op2(OP_AND, Sc::U32, u[2], nm);
                let ins = self.op2(OP_SHL, Sc::U32, b, sh);
                let out = self.op2(OP_OR, Sc::U32, kept, ins);
                self.write_dst_u(i, out)
            }
            "v_perm_b32" => {
                let u = self.uargs(i, 3)?;
                let mut out = self.cu(0);
                for k in 0..4u32 {
                    let (c8, cff) = (self.cu(8 * k), self.cu(0xFF));
                    let t = self.op2(OP_SHR, Sc::U32, u[2], c8);
                    let sel = self.op2(OP_AND, Sc::U32, t, cff);
                    let r = self.perm_byte(u[0], u[1], sel);
                    let placed = self.op2(OP_SHL, Sc::U32, r, c8);
                    out = self.op2(OP_OR, Sc::U32, out, placed);
                }
                self.write_dst_u(i, out)
            }
            "v_mad_u64_u32" | "v_mad_i64_i32" => self.mad64(i, name == "v_mad_i64_i32"),
            _ => self.valu_misc(i, name),
        }
    }

    /// Octet sélectionné par `sel` dans {s0 (4..7), s1 (0..3)} ; 8..11 : signes ; 12 : 0 ; >12 : 0xff.
    fn perm_byte(&mut self, s0: Val, s1: Val, sel: Val) -> Val {
        let (c3, c4, c8, c12, cff, z) = (
            self.cu(3),
            self.cu(4),
            self.cu(8),
            self.cu(12),
            self.cu(0xFF),
            self.cu(0),
        );
        let idx = self.op2(OP_AND, Sc::U32, sel, c3);
        let c8b = self.cu(8);
        let sh = self.op2(OP_IMUL, Sc::U32, idx, c8b);
        let lt4 = self.op2(176, Sc::Bool, sel, c4);
        let w = self.select(lt4, s1, s0);
        let shifted = self.op2(OP_SHR, Sc::U32, w, sh);
        let mut r = self.op2(OP_AND, Sc::U32, shifted, cff);
        let srcs = [(s1, 15u32), (s1, 31), (s0, 15), (s0, 31)];
        let mut sign = z;
        for (j, (src, bit)) in srcs.iter().enumerate() {
            let (cb, one) = (self.cu(*bit), self.cu(1));
            let t = self.op2(OP_SHR, Sc::U32, *src, cb);
            let b1 = self.op2(OP_AND, Sc::U32, t, one);
            let nz = self.op2(OP_INE, Sc::Bool, b1, z);
            let v = self.select(nz, cff, z);
            let cj = self.cu(8 + j as u32);
            let is_j = self.op2(OP_IEQ, Sc::Bool, sel, cj);
            sign = self.select(is_j, v, sign);
        }
        let ge8 = self.op2(174, Sc::Bool, sel, c8);
        let lt12 = self.op2(176, Sc::Bool, sel, c12);
        let in_sign = self.bool_and(ge8, lt12);
        r = self.select(in_sign, sign, r);
        let is12 = self.op2(OP_IEQ, Sc::Bool, sel, c12);
        r = self.select(is12, z, r);
        let c13 = self.cu(13);
        let ge13 = self.op2(174, Sc::Bool, sel, c13);
        self.select(ge13, cff, r)
    }

    fn int16_binop(&mut self, name: &str, a: Val, b: Val, signed: bool) -> Val {
        let sc = if signed { Sc::I32 } else { Sc::U32 };
        let (ai, bi) = (self.spv.bitcast(a, sc), self.spv.bitcast(b, sc));
        if name.contains("lshlrev") || name.contains("lshrrev") || name.contains("ashrrev") {
            // opérandes inversés : dst = src1 <op> (src0 & 15) ; `a` = décalage, `b` = valeur
            let m15 = self.cu(15);
            let sh = self.op2(OP_AND, Sc::U32, a, m15);
            return if name.contains("lshl") {
                self.op2(OP_SHL, Sc::U32, b, sh)
            } else if name.contains("lshr") {
                let zb = self.ext16(b, false, false);
                self.op2(OP_SHR, Sc::U32, zb, sh)
            } else {
                let sb = self.ext16(b, false, true);
                let x = self.spv.bitcast(sb, Sc::I32);
                let r = self.spv.val(OP_SAR, Sc::I32, 1, &[x.id, sh.id]);
                self.spv.bitcast(r, Sc::U32)
            };
        }
        let r = if name.contains("add") {
            self.op2(OP_IADD, sc, ai, bi)
        } else if name.contains("sub") {
            self.op2(OP_ISUB, sc, ai, bi)
        } else if name.contains("mul") {
            self.op2(OP_IMUL, sc, ai, bi)
        } else if name.contains("min") {
            self.glsl(sc, if signed { G_SMIN } else { G_UMIN }, &[ai, bi])
        } else {
            self.glsl(sc, if signed { G_SMAX } else { G_UMAX }, &[ai, bi])
        };
        self.spv.bitcast(r, Sc::U32)
    }

    fn mad64(&mut self, i: &Instruction, signed: bool) -> TResult<()> {
        let ops = &i.operands;
        if ops.len() < 5 {
            return unsupported(i);
        }
        let (d, carry) = (ops[0], ops[1]);
        let (a, b) = (self.src_u32(&ops[2])?, self.src_u32(&ops[3])?);
        let (cl, ch) = self.src_pair(&ops[4])?;
        let (lo, hi) = self.mul_wide(a, b, signed);
        let l2 = self.op2(OP_IADD, Sc::U32, lo, cl);
        let c1 = self.op2(176, Sc::Bool, l2, lo);
        let (one, zero) = (self.cu(1), self.cu(0));
        let cin = self.select(c1, one, zero);
        let h1 = self.op2(OP_IADD, Sc::U32, hi, ch);
        let h2 = self.op2(OP_IADD, Sc::U32, h1, cin);
        let Src::Vgpr(n) = d.src else {
            return unsupported(i);
        };
        self.write_v(usize::from(n), l2);
        self.write_v(usize::from(n) + 1, h2);
        let c2 = self.op2(176, Sc::Bool, h1, hi);
        let c3 = self.op2(176, Sc::Bool, h2, h1);
        let co = self.bool_or(c2, c3);
        self.write_mask(&carry, co)
    }

    /// Produit 32x32 -> (bas, haut) 64 bits.
    pub(crate) fn mul_wide(&mut self, a: Val, b: Val, signed: bool) -> (Val, Val) {
        let (sc, op) = if signed {
            (Sc::I32, 152)
        } else {
            (Sc::U32, 151)
        };
        let t = self.spv.ty(sc, 1);
        let st = self.spv.structure(&[t, t]);
        let (x, y) = (self.spv.bitcast(a, sc), self.spv.bitcast(b, sc));
        let r = self.spv.emit(op, st, &[x.id, y.id]);
        let lo = self.spv.val(81, sc, 1, &[r, 0]);
        let hi = self.spv.val(81, sc, 1, &[r, 1]);
        (self.spv.bitcast(lo, Sc::U32), self.spv.bitcast(hi, Sc::U32))
    }

    // ------------------------------------------------------------ comparaisons étendues
    pub(crate) fn vcmp_ext(
        &mut self,
        i: &Instruction,
        is_x: bool,
        rel: &str,
        ty: &str,
    ) -> TResult<()> {
        let s = i.srcs();
        if s.len() < 2 {
            return unsupported(i);
        }
        if rel == "class" {
            let (bits, ebits, mbits) = match ty {
                "f32" => (self.src_u32(&s[0])?, 8u32, 23u32),
                "f16" => {
                    let b = self.src_u32(&s[0])?;
                    (self.ext16(b, false, false), 5, 10)
                }
                _ => return self.vcmp_f64_class(i, is_x),
            };
            // neg/abs agissent sur le motif de bits (bit de signe)
            let mut bits = bits;
            let sign_bit = self.cu(1 << (ebits + mbits));
            if s[0].abs {
                let nm = self.op1(OP_NOT, Sc::U32, sign_bit);
                bits = self.op2(OP_AND, Sc::U32, bits, nm);
            }
            if s[0].neg {
                bits = self.op2(198, Sc::U32, bits, sign_bit);
            }
            let mask = self.class_mask(bits, ebits, mbits);
            let m = self.src_u32(&s[1])?;
            let t = self.op2(OP_AND, Sc::U32, mask, m);
            let z = self.cu(0);
            let r = self.op2(OP_INE, Sc::Bool, t, z);
            return self.vcmp_write(i, is_x, r);
        }
        let bit = match ty {
            "f16" => {
                let (a, b) = (self.src_half(&s[0], false)?, self.src_half(&s[1], false)?);
                match self.fcmp_rel(rel, a, b) {
                    Some(r) => r,
                    None => return unsupported(i),
                }
            }
            "i16" | "u16" => {
                let signed = ty == "i16";
                let (a, b) = (
                    self.src_i16(&s[0], false, signed)?,
                    self.src_i16(&s[1], false, signed)?,
                );
                if rel == "f" || rel == "t" {
                    Val {
                        id: self.spv.c_bool(rel == "t"),
                        sc: Sc::Bool,
                        n: 1,
                    }
                } else {
                    let Some(op) = Self::cmp_op(signed, rel) else {
                        return unsupported(i);
                    };
                    let (a, b) = if signed {
                        (self.spv.bitcast(a, Sc::I32), self.spv.bitcast(b, Sc::I32))
                    } else {
                        (a, b)
                    };
                    self.op2(op, Sc::Bool, a, b)
                }
            }
            _ => return self.vcmp_wide(i, is_x, rel, ty),
        };
        self.vcmp_write(i, is_x, bit)
    }

    /// Relation flottante ordonnée/non ordonnée sur f32.
    pub(crate) fn fcmp_rel(&mut self, rel: &str, a: Val, b: Val) -> Option<Val> {
        let op = match rel {
            "lt" => 184,
            "eq" => 180,
            "le" => 188,
            "gt" => 186,
            "lg" => 182,
            "ge" => 190,
            "nge" => 185,
            "nlg" => 181,
            "ngt" => 189,
            "nle" => 187,
            "neq" => 183,
            "nlt" => 191,
            "o" | "u" => {
                let (na, nb) = (self.op1(156, Sc::Bool, a), self.op1(156, Sc::Bool, b));
                let any = self.bool_or(na, nb);
                return Some(if rel == "u" { any } else { self.bool_not(any) });
            }
            "f" | "tru" => {
                return Some(Val {
                    id: self.spv.c_bool(rel == "tru"),
                    sc: Sc::Bool,
                    n: 1,
                });
            }
            _ => return None,
        };
        Some(self.op2(op, Sc::Bool, a, b))
    }

    /// Masque des 10 classes IEEE de `v_cmp_class` pour un flottant de bits `bits`.
    pub(crate) fn class_mask(&mut self, bits: Val, ebits: u32, mbits: u32) -> Val {
        let emax = (1u32 << ebits) - 1;
        let (cm, ce, cmax) = (self.cu((1 << mbits) - 1), self.cu(emax), self.cu(emax));
        let (csh, cs31, c1) = (self.cu(mbits), self.cu(ebits + mbits), self.cu(1));
        let zero = self.cu(0);
        let e0 = self.op2(OP_SHR, Sc::U32, bits, csh);
        let e = self.op2(OP_AND, Sc::U32, e0, ce);
        let m = self.op2(OP_AND, Sc::U32, bits, cm);
        let s0 = self.op2(OP_SHR, Sc::U32, bits, cs31);
        let sbit = self.op2(OP_AND, Sc::U32, s0, c1);
        let neg = self.op2(OP_INE, Sc::Bool, sbit, zero);
        let e_max = self.op2(OP_IEQ, Sc::Bool, e, cmax);
        let e_zero = self.op2(OP_IEQ, Sc::Bool, e, zero);
        let m_zero = self.op2(OP_IEQ, Sc::Bool, m, zero);
        let m_nz = self.bool_not(m_zero);
        let nan = self.bool_and(e_max, m_nz);
        let qb = self.cu(1 << (mbits - 1));
        let qm = self.op2(OP_AND, Sc::U32, m, qb);
        let quiet = self.op2(OP_INE, Sc::Bool, qm, zero);
        let nquiet = self.bool_not(quiet);
        let snan = self.bool_and(nan, nquiet);
        let qnan = self.bool_and(nan, quiet);
        let inf = self.bool_and(e_max, m_zero);
        let zr = self.bool_and(e_zero, m_zero);
        let den = self.bool_and(e_zero, m_nz);
        let ne = self.bool_not(e_zero);
        let nmx = self.bool_not(e_max);
        let nrm = self.bool_and(ne, nmx);
        let pos = self.bool_not(neg);
        let conds = [
            snan,
            qnan,
            self.bool_and(inf, neg),
            self.bool_and(nrm, neg),
            self.bool_and(den, neg),
            self.bool_and(zr, neg),
            self.bool_and(zr, pos),
            self.bool_and(den, pos),
            self.bool_and(nrm, pos),
            self.bool_and(inf, pos),
        ];
        let mut acc = zero;
        for (k, c) in conds.iter().enumerate() {
            let bit = self.cu(1 << k);
            let v = self.select(*c, bit, zero);
            acc = self.op2(OP_OR, Sc::U32, acc, v);
        }
        acc
    }

    // ------------------------------------------------------------ VOP3P
    /// Demi-mot `half` (0 bas, 1 haut) de la source `k` d'une instruction VOP3P, en f32.
    fn pk_f(&mut self, i: &Instruction, k: usize, half: usize) -> TResult<Val> {
        let o = *need(i.srcs().get(k), i)?;
        let sel = if half == 0 {
            i.mods.op_sel >> k & 1
        } else {
            i.mods.op_sel_hi >> k & 1
        } != 0;
        let negbit = if half == 0 {
            i.mods.neg_lo >> k & 1
        } else {
            i.mods.neg_hi >> k & 1
        } != 0;
        let o2 = Operand {
            neg: negbit,
            abs: false,
            ..o
        };
        self.src_half(&o2, sel)
    }

    fn pk_i(&mut self, i: &Instruction, k: usize, half: usize, signed: bool) -> TResult<Val> {
        let o = *need(i.srcs().get(k), i)?;
        let sel = if half == 0 {
            i.mods.op_sel >> k & 1
        } else {
            i.mods.op_sel_hi >> k & 1
        } != 0;
        self.src_i16(&o, sel, signed)
    }

    pub(crate) fn vop3p(&mut self, i: &Instruction) -> TResult<()> {
        let name = i.base_name();
        let d = *need(i.dst(), i)?;
        let Src::Vgpr(dn) = d.src else {
            return unsupported(i);
        };
        let dn = usize::from(dn);
        match name {
            "v_pk_add_f16" | "v_pk_mul_f16" | "v_pk_min_f16" | "v_pk_max_f16" | "v_pk_fma_f16" => {
                let mut res = [self.cf(0.0); 2];
                for (h, slot) in res.iter_mut().enumerate() {
                    let (a, b) = (self.pk_f(i, 0, h)?, self.pk_f(i, 1, h)?);
                    let mut r = match name {
                        "v_pk_add_f16" => self.op2(OP_FADD, Sc::F32, a, b),
                        "v_pk_mul_f16" => self.op2(OP_FMUL, Sc::F32, a, b),
                        "v_pk_min_f16" => self.glsl(Sc::F32, G_FMIN, &[a, b]),
                        "v_pk_max_f16" => self.glsl(Sc::F32, G_FMAX, &[a, b]),
                        _ => {
                            let c = self.pk_f(i, 2, h)?;
                            self.glsl(Sc::F32, G_FMA, &[a, b, c])
                        }
                    };
                    if i.mods.clamp {
                        let (z, o) = (self.cf(0.0), self.cf(1.0));
                        r = self.glsl(Sc::F32, G_FCLAMP, &[r, z, o]);
                    }
                    *slot = r;
                }
                let r = self.pack2h(res[0], res[1]);
                self.write_v(dn, r);
                Ok(())
            }
            "v_pk_add_i16" | "v_pk_add_u16" | "v_pk_sub_i16" | "v_pk_sub_u16"
            | "v_pk_mul_lo_u16" | "v_pk_min_i16" | "v_pk_max_i16" | "v_pk_min_u16"
            | "v_pk_max_u16" | "v_pk_lshlrev_b16" | "v_pk_lshrrev_b16" | "v_pk_ashrrev_i16"
            | "v_pk_mad_i16" | "v_pk_mad_u16" => {
                let signed = name.contains("i16");
                let mut out = self.cu(0);
                for h in 0..2 {
                    let (a, b) = (self.pk_i(i, 0, h, signed)?, self.pk_i(i, 1, h, signed)?);
                    let r = if name.contains("mad") {
                        let c = self.pk_i(i, 2, h, signed)?;
                        let m = self.op2(OP_IMUL, Sc::U32, a, b);
                        self.op2(OP_IADD, Sc::U32, m, c)
                    } else {
                        self.int16_binop(&name.replace("_pk", ""), a, b, signed)
                    };
                    let m = self.cu(0xFFFF);
                    let lo = self.op2(OP_AND, Sc::U32, r, m);
                    let c = self.cu(16 * h as u32);
                    let placed = self.op2(OP_SHL, Sc::U32, lo, c);
                    out = self.op2(OP_OR, Sc::U32, out, placed);
                }
                self.write_v(dn, out);
                Ok(())
            }
            "v_dot2_f32_f16" => {
                let mut acc = self.src_f32(&i.srcs()[2])?;
                for h in 0..2 {
                    let (a, b) = (self.pk_f(i, 0, h)?, self.pk_f(i, 1, h)?);
                    acc = self.glsl(Sc::F32, G_FMA, &[a, b, acc]);
                }
                if i.mods.clamp {
                    let (z, o) = (self.cf(0.0), self.cf(1.0));
                    acc = self.glsl(Sc::F32, G_FCLAMP, &[acc, z, o]);
                }
                self.write_v(dn, acc);
                Ok(())
            }
            "v_dot2_i32_i16" | "v_dot2_u32_u16" => {
                let signed = name.contains("i32");
                let mut acc = self.src_u32(&i.srcs()[2])?;
                for h in 0..2 {
                    let (a, b) = (self.pk_i(i, 0, h, signed)?, self.pk_i(i, 1, h, signed)?);
                    let m = self.op2(OP_IMUL, Sc::U32, a, b);
                    acc = self.op2(OP_IADD, Sc::U32, acc, m);
                }
                self.write_v(dn, acc);
                Ok(())
            }
            "v_dot4_i32_i8" | "v_dot4_u32_u8" | "v_dot8_i32_i4" | "v_dot8_u32_u4" => {
                let signed = name.contains("i32");
                let (n, w) = if name.contains("dot4") {
                    (4u32, 8u32)
                } else {
                    (8, 4)
                };
                let s = i.srcs();
                let (a, b) = (self.src_u32(&s[0])?, self.src_u32(&s[1])?);
                let mut acc = self.src_u32(&s[2])?;
                let cw = self.cu(w);
                for k in 0..n {
                    let off = self.cu(k * w);
                    let (x, y) = if signed {
                        let (ai, bi) = (self.spv.bitcast(a, Sc::I32), self.spv.bitcast(b, Sc::I32));
                        let x = self.spv.val(OP_BFSEXT, Sc::I32, 1, &[ai.id, off.id, cw.id]);
                        let y = self.spv.val(OP_BFSEXT, Sc::I32, 1, &[bi.id, off.id, cw.id]);
                        (self.spv.bitcast(x, Sc::U32), self.spv.bitcast(y, Sc::U32))
                    } else {
                        (
                            self.spv.val(OP_BFUEXT, Sc::U32, 1, &[a.id, off.id, cw.id]),
                            self.spv.val(OP_BFUEXT, Sc::U32, 1, &[b.id, off.id, cw.id]),
                        )
                    };
                    let m = self.op2(OP_IMUL, Sc::U32, x, y);
                    acc = self.op2(OP_IADD, Sc::U32, acc, m);
                }
                self.write_v(dn, acc);
                Ok(())
            }
            "v_fma_mix_f32" | "v_fma_mixlo_f16" | "v_fma_mixhi_f16" => {
                let mut v = Vec::new();
                for k in 0..3 {
                    let o = i.srcs()[k];
                    let o2 = Operand {
                        neg: false,
                        abs: false,
                        ..o
                    };
                    let mut x = if i.mods.op_sel_hi >> k & 1 != 0 {
                        self.src_half(&o2, i.mods.op_sel >> k & 1 != 0)?
                    } else {
                        let b = self.src_u32(&o2)?;
                        self.spv.bitcast(b, Sc::F32)
                    };
                    if i.mods.neg_hi >> k & 1 != 0 {
                        x = self.glsl(Sc::F32, 4, &[x]);
                    }
                    if i.mods.neg_lo >> k & 1 != 0 {
                        x = self.op1(127, Sc::F32, x);
                    }
                    v.push(x);
                }
                let mut r = self.glsl(Sc::F32, G_FMA, &[v[0], v[1], v[2]]);
                if i.mods.clamp {
                    let (z, o) = (self.cf(0.0), self.cf(1.0));
                    r = self.glsl(Sc::F32, G_FCLAMP, &[r, z, o]);
                }
                if name == "v_fma_mix_f32" {
                    self.write_v(dn, r);
                } else {
                    let h = self.f2h(r);
                    let old = {
                        let var = self.vvar(dn);
                        self.load_u32(var)
                    };
                    let (m, c16) = (self.cu(0xFFFF), self.cu(16));
                    let out = if name == "v_fma_mixlo_f16" {
                        let keep = self.op1(OP_NOT, Sc::U32, m);
                        let k = self.op2(OP_AND, Sc::U32, old, keep);
                        self.op2(OP_OR, Sc::U32, k, h)
                    } else {
                        let k = self.op2(OP_AND, Sc::U32, old, m);
                        let sh = self.op2(OP_SHL, Sc::U32, h, c16);
                        self.op2(OP_OR, Sc::U32, k, sh)
                    };
                    self.write_v(dn, out);
                }
                Ok(())
            }
            _ => unsupported(i),
        }
    }
}
