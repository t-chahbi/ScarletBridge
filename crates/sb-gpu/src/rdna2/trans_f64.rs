//! f64 (capacité SPIR-V `Float64`, fonctionnalité Vulkan `shaderFloat64`) et comparaisons 64 bits.
//!
//! Les doubles sont assemblés depuis des paires de registres 32 bits (`PackDouble2x32`).
//! `omod`/`clamp` ne sont pas supportés sur f64 (erreur `Unsupported` explicite).

use super::inst::{Instruction, Operand, Src};
use super::translate::{TResult, Tr, TranslateError, unsupported};
use crate::dxbc_to_spirv::spv::{Sc, Val};

const G_RNDNE: u32 = 2;
const G_TRUNC: u32 = 3;
const G_FLOOR: u32 = 8;
const G_CEIL: u32 = 9;
const G_FRACT: u32 = 10;
const G_SQRT: u32 = 31;
const G_RSQRT: u32 = 32;
const G_FMIN: u32 = 37;
const G_FMAX: u32 = 40;
const G_FMA: u32 = 50;
const G_FREXP_STRUCT: u32 = 52;
const G_LDEXP: u32 = 53;
const G_PACK_DOUBLE: u32 = 59;
const G_UNPACK_DOUBLE: u32 = 65;

impl Tr<'_> {
    fn f64_ty(&mut self) -> u32 {
        self.spv.caps.insert(10);
        self.spv.raw_type(22, &[64])
    }

    fn pack_d(&mut self, lo: Val, hi: Val) -> u32 {
        let dt = self.f64_ty();
        let v2 = self.spv.construct(Sc::U32, &[lo.id, hi.id]);
        let g = self.spv.glsl;
        self.spv.emit(12, dt, &[g, G_PACK_DOUBLE, v2.id])
    }

    fn unpack_d(&mut self, d: u32) -> (Val, Val) {
        let t = self.spv.ty(Sc::U32, 2);
        let g = self.spv.glsl;
        let id = self.spv.emit(12, t, &[g, G_UNPACK_DOUBLE, d]);
        let v = Val {
            id,
            sc: Sc::U32,
            n: 2,
        };
        (self.spv.extract(v, 0), self.spv.extract(v, 1))
    }

    fn gd(&mut self, inst: u32, args: &[u32]) -> u32 {
        let dt = self.f64_ty();
        let g = self.spv.glsl;
        let mut ops = vec![g, inst];
        ops.extend_from_slice(args);
        self.spv.emit(12, dt, &ops)
    }

    fn bin_d(&mut self, op: u16, a: u32, b: u32) -> u32 {
        let dt = self.f64_ty();
        self.spv.emit(op, dt, &[a, b])
    }

    /// Source f64 avec `abs`/`neg`.
    fn src_d(&mut self, o: &Operand) -> TResult<u32> {
        let mut d = match o.src {
            Src::InlineFloat(c) => {
                let dt = self.f64_ty();
                let b = f64::from(Src::inline_float_value(c)).to_bits();
                self.spv.c_raw(dt, &[b as u32, (b >> 32) as u32])
            }
            _ => {
                let (lo, hi) = self.src_pair(o)?;
                self.pack_d(lo, hi)
            }
        };
        if o.abs {
            d = self.gd(4, &[d]);
        }
        if o.neg {
            let dt = self.f64_ty();
            d = self.spv.emit(127, dt, &[d]);
        }
        Ok(d)
    }

    fn write_d(&mut self, i: &Instruction, d: u32) -> TResult<()> {
        if i.mods.clamp || i.mods.omod != 0 {
            return Err(TranslateError::Unsupported(format!(
                "{} avec clamp/omod f64",
                i.name
            )));
        }
        let o = *i
            .dst()
            .ok_or_else(|| TranslateError::Unsupported(i.name.into()))?;
        let Src::Vgpr(n) = o.src else {
            return unsupported(i);
        };
        let (lo, hi) = self.unpack_d(d);
        self.write_v(usize::from(n), lo);
        self.write_v(usize::from(n) + 1, hi);
        Ok(())
    }

    pub(crate) fn valu_f64(&mut self, i: &Instruction, name: &str) -> TResult<()> {
        let s = i.srcs();
        match name {
            "v_add_f64" | "v_mul_f64" | "v_min_f64" | "v_max_f64" => {
                let (a, b) = (self.src_d(&s[0])?, self.src_d(&s[1])?);
                let r = match name {
                    "v_add_f64" => self.bin_d(129, a, b),
                    "v_mul_f64" => self.bin_d(133, a, b),
                    "v_min_f64" => self.gd(G_FMIN, &[a, b]),
                    _ => self.gd(G_FMAX, &[a, b]),
                };
                self.write_d(i, r)
            }
            "v_fma_f64" => {
                let (a, b, c) = (self.src_d(&s[0])?, self.src_d(&s[1])?, self.src_d(&s[2])?);
                let r = self.gd(G_FMA, &[a, b, c]);
                self.write_d(i, r)
            }
            "v_floor_f64" | "v_ceil_f64" | "v_trunc_f64" | "v_rndne_f64" | "v_fract_f64"
            | "v_sqrt_f64" | "v_rsq_f64" => {
                let a = self.src_d(&s[0])?;
                let g = match name {
                    "v_floor_f64" => G_FLOOR,
                    "v_ceil_f64" => G_CEIL,
                    "v_trunc_f64" => G_TRUNC,
                    "v_rndne_f64" => G_RNDNE,
                    "v_fract_f64" => G_FRACT,
                    "v_sqrt_f64" => G_SQRT,
                    _ => G_RSQRT,
                };
                let r = self.gd(g, &[a]);
                self.write_d(i, r)
            }
            "v_rcp_f64" => {
                let a = self.src_d(&s[0])?;
                let dt = self.f64_ty();
                let one = {
                    let b = 1.0f64.to_bits();
                    self.spv.c_raw(dt, &[b as u32, (b >> 32) as u32])
                };
                let r = self.bin_d(136, one, a);
                self.write_d(i, r)
            }
            "v_ldexp_f64" => {
                let a = self.src_d(&s[0])?;
                let e = self.src_u32(&s[1])?;
                let ei = self.spv.bitcast(e, Sc::I32);
                let r = self.gd(G_LDEXP, &[a, ei.id]);
                self.write_d(i, r)
            }
            "v_frexp_mant_f64" | "v_frexp_exp_i32_f64" => {
                let a = self.src_d(&s[0])?;
                let dt = self.f64_ty();
                let i32t = self.spv.ty(Sc::I32, 1);
                let st = self.spv.structure(&[dt, i32t]);
                let g = self.spv.glsl;
                let r = self.spv.emit(12, st, &[g, G_FREXP_STRUCT, a]);
                if name == "v_frexp_mant_f64" {
                    let m = self.spv.emit(81, dt, &[r, 0]);
                    self.write_d(i, m)
                } else {
                    let e = self.spv.val(81, Sc::I32, 1, &[r, 1]);
                    self.write_dst_i(i, e)
                }
            }
            "v_cvt_f64_f32" => {
                let a = self.fargs(i, 1)?;
                let dt = self.f64_ty();
                let r = self.spv.emit(115, dt, &[a[0].id]); // OpFConvert
                self.write_d(i, r)
            }
            "v_cvt_f32_f64" => {
                let a = self.src_d(&s[0])?;
                let r = self.spv.val(115, Sc::F32, 1, &[a]);
                self.write_dst_f(i, r)
            }
            "v_cvt_f64_i32" | "v_cvt_f64_u32" => {
                let a = self.src_u32(&s[0])?;
                let dt = self.f64_ty();
                let r = if name.ends_with("i32") {
                    let ai = self.spv.bitcast(a, Sc::I32);
                    self.spv.emit(111, dt, &[ai.id])
                } else {
                    self.spv.emit(112, dt, &[a.id])
                };
                self.write_d(i, r)
            }
            "v_cvt_i32_f64" | "v_cvt_u32_f64" => {
                let a = self.src_d(&s[0])?;
                if name.starts_with("v_cvt_i32") {
                    let r = self.spv.val(110, Sc::I32, 1, &[a]);
                    self.write_dst_i(i, r)
                } else {
                    let r = self.spv.val(109, Sc::U32, 1, &[a]);
                    self.write_dst_u(i, r)
                }
            }
            _ => unsupported(i),
        }
    }

    pub(crate) fn vcmp_f64_class(&mut self, i: &Instruction, _is_x: bool) -> TResult<()> {
        unsupported(i)
    }

    /// Comparaisons f64 / i64 / u64.
    pub(crate) fn vcmp_wide(
        &mut self,
        i: &Instruction,
        is_x: bool,
        rel: &str,
        ty: &str,
    ) -> TResult<()> {
        let s = i.srcs();
        let bit = if ty == "f64" {
            let (a, b) = (self.src_d(&s[0])?, self.src_d(&s[1])?);
            match self.fcmp_rel(
                rel,
                Val {
                    id: a,
                    sc: Sc::F32,
                    n: 1,
                },
                Val {
                    id: b,
                    sc: Sc::F32,
                    n: 1,
                },
            ) {
                Some(r) => r,
                None => return unsupported(i),
            }
        } else {
            let signed = ty == "i64";
            let ((al, ah), (bl, bh)) = (self.src_pair(&s[0])?, self.src_pair(&s[1])?);
            if rel == "f" || rel == "t" {
                Val {
                    id: self.spv.c_bool(rel == "t"),
                    sc: Sc::Bool,
                    n: 1,
                }
            } else {
                let eq_hi = self.op2(170, Sc::Bool, ah, bh);
                let eq_lo = self.op2(170, Sc::Bool, al, bl);
                let eq = self.bool_and(eq_hi, eq_lo);
                if rel == "eq" {
                    eq
                } else if rel == "ne" {
                    self.bool_not(eq)
                } else {
                    // lt/le/gt/ge : tête signée/non signée, queue non signée
                    let (strict_hi, strict_lo) = match rel {
                        "lt" | "le" => (Self::cmp_op(signed, "lt"), Self::cmp_op(false, "lt")),
                        _ => (Self::cmp_op(signed, "gt"), Self::cmp_op(false, "gt")),
                    };
                    let (Some(sh), Some(sl)) = (strict_hi, strict_lo) else {
                        return unsupported(i);
                    };
                    let (x, y) = if signed {
                        (self.spv.bitcast(ah, Sc::I32), self.spv.bitcast(bh, Sc::I32))
                    } else {
                        (ah, bh)
                    };
                    let hi_strict = self.op2(sh, Sc::Bool, x, y);
                    let lo_strict = self.op2(sl, Sc::Bool, al, bl);
                    let tail = self.bool_and(eq_hi, lo_strict);
                    let strict = self.bool_or(hi_strict, tail);
                    if rel == "le" || rel == "ge" {
                        self.bool_or(strict, eq)
                    } else {
                        strict
                    }
                }
            }
        };
        self.vcmp_write(i, is_x, bit)
    }
}
