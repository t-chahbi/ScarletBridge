//! Extension HLE CRT : mathématiques (`api-ms-win-crt-math`), temps (`api-ms-win-crt-time`)
//! et locale minimale « C » (`api-ms-win-crt-locale`).
//!
//! Comportement aligné sur l'UCRT Microsoft : classes FP MSVC (`FP_NORMAL = -1`, …), `errno`
//! `EDOM`/`ERANGE` positionné sur erreurs de domaine / pôle / dépassement, `struct tm` MSVC de
//! 9 `i32`, plages `__time64_t` UCRT (1970-01-01 → 3000-12-31 23:59:59 UTC).

#![allow(non_snake_case, unused_variables)]

use core::ffi::c_char;
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::num::FpCategory;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use sb_common::ffi_guard;

use crate::crt_str::{EINVAL, ERANGE, set_errno};

const CHANNEL: &str = "crt-math";

/// `errno` MSVC : erreur de domaine.
pub(crate) const EDOM: i32 = 33;

/// NaN « indéfini » x86 (`-nan(ind)`) renvoyé par l'UCRT sur erreur de domaine.
const IND64: f64 = f64::from_bits(0xFFF8_0000_0000_0000);
const IND32: f32 = f32::from_bits(0xFFC0_0000);

// Classes `FP_*` MSVC (`<math.h>` UCRT).
const FP_INFINITE: i16 = 1;
const FP_NAN: i16 = 2;
const FP_NORMAL: i16 = -1;
const FP_SUBNORMAL: i16 = -2;
const FP_ZERO: i16 = 0;

// Masques `_FPCLASS_*` (`<float.h>`).
const FPCLASS_SNAN: i32 = 0x0001;
const FPCLASS_QNAN: i32 = 0x0002;
const FPCLASS_NINF: i32 = 0x0004;
const FPCLASS_NN: i32 = 0x0008;
const FPCLASS_ND: i32 = 0x0010;
const FPCLASS_NZ: i32 = 0x0020;
const FPCLASS_PZ: i32 = 0x0040;
const FPCLASS_PD: i32 = 0x0080;
const FPCLASS_PN: i32 = 0x0100;
const FPCLASS_PINF: i32 = 0x0200;

/// `FP_ILOGB0` / `FP_ILOGBNAN` MSVC.
const FP_ILOGB0: i32 = i32::MIN;
const FP_ILOGBNAN: i32 = i32::MAX;

// FFI vers la libm hôte pour les fonctions spéciales absentes de `std` (précision libm).
unsafe extern "C" {
    fn erf(x: f64) -> f64;
    fn erfc(x: f64) -> f64;
    fn erff(x: f32) -> f32;
    fn erfcf(x: f32) -> f32;
    fn tzset();
}

/// Exports `nom -> adresse` fusionnés dans la famille CRT du résolveur.
pub fn exports() -> HashMap<&'static str, usize> {
    let _ = process_start();
    let mut m = HashMap::new();
    register_math(&mut m);
    register_time(&mut m);
    register_locale(&mut m);
    // Alias MSVC.
    m.insert("_hypot", m_hypot as *const () as usize);
    m.insert("_hypotf", m_hypotf as *const () as usize);
    m.insert("_copysign", m_copysign as *const () as usize);
    m.insert("_copysignf", m_copysignf as *const () as usize);
    m.insert("_chgsign", m_chgsign as *const () as usize);
    m.insert("time", t_time64 as *const () as usize);
    m.insert("_ldclass", m_dclass as *const () as usize);
    m.insert("_ldsign", m_dsign as *const () as usize);
    m.insert("nearbyint", m_rint as *const () as usize);
    m.insert("nearbyintf", m_rintf as *const () as usize);
    m.insert("nexttoward", m_nextafter as *const () as usize);
    m.insert("ldexp", m_scalbn as *const () as usize);
    m.insert("ldexpf", m_scalbnf as *const () as usize);
    m
}

// =====================================================================================
// Helpers mathématiques (purs, testables)
// =====================================================================================

fn domain64() -> f64 {
    set_errno(EDOM);
    IND64
}

fn domain32() -> f32 {
    set_errno(EDOM);
    IND32
}

/// `ERANGE` si un résultat infini provient d'entrées finies (dépassement).
fn ovf64(finite_in: bool, r: f64) -> f64 {
    if finite_in && r.is_infinite() {
        set_errno(ERANGE);
    }
    r
}

fn ovf32(finite_in: bool, r: f32) -> f32 {
    if finite_in && r.is_infinite() {
        set_errno(ERANGE);
    }
    r
}

/// `ERANGE` sur dépassement ou sous-dépassement total vers 0 d'un résultat non nul.
fn range64(finite_in: bool, r: f64) -> f64 {
    if finite_in && (r.is_infinite() || r == 0.0) {
        set_errno(ERANGE);
    }
    r
}

fn range32(finite_in: bool, r: f32) -> f32 {
    if finite_in && (r.is_infinite() || r == 0.0) {
        set_errno(ERANGE);
    }
    r
}

fn fp_short(c: FpCategory) -> i16 {
    match c {
        FpCategory::Nan => FP_NAN,
        FpCategory::Infinite => FP_INFINITE,
        FpCategory::Zero => FP_ZERO,
        FpCategory::Subnormal => FP_SUBNORMAL,
        FpCategory::Normal => FP_NORMAL,
    }
}

fn fpclass_mask(c: FpCategory, negative: bool, signaling: bool) -> i32 {
    match (c, negative) {
        (FpCategory::Nan, _) => {
            if signaling {
                FPCLASS_SNAN
            } else {
                FPCLASS_QNAN
            }
        }
        (FpCategory::Infinite, true) => FPCLASS_NINF,
        (FpCategory::Infinite, false) => FPCLASS_PINF,
        (FpCategory::Normal, true) => FPCLASS_NN,
        (FpCategory::Normal, false) => FPCLASS_PN,
        (FpCategory::Subnormal, true) => FPCLASS_ND,
        (FpCategory::Subnormal, false) => FPCLASS_PD,
        (FpCategory::Zero, true) => FPCLASS_NZ,
        (FpCategory::Zero, false) => FPCLASS_PZ,
    }
}

fn is_snan64(x: f64) -> bool {
    x.is_nan() && x.to_bits() & (1 << 51) == 0
}

fn is_snan32(x: f32) -> bool {
    x.is_nan() && x.to_bits() & (1 << 22) == 0
}

/// `scalbn` exact (algorithme musl).
pub(crate) fn scalbn64(mut x: f64, mut n: i32) -> f64 {
    let p1023 = f64::from_bits(0x7FE0_0000_0000_0000);
    let small = f64::from_bits(0x0010_0000_0000_0000) * f64::from_bits(0x4340_0000_0000_0000);
    if n > 1023 {
        x *= p1023;
        n -= 1023;
        if n > 1023 {
            x *= p1023;
            n -= 1023;
            n = n.min(1023);
        }
    } else if n < -1022 {
        x *= small;
        n += 1022 - 53;
        if n < -1022 {
            x *= small;
            n += 1022 - 53;
            n = n.max(-1022);
        }
    }
    x * f64::from_bits(((0x3FF + n) as u64) << 52)
}

fn scalbn32(mut x: f32, mut n: i32) -> f32 {
    let p127 = f32::from_bits(0x7F00_0000);
    let small = f32::from_bits(0x0080_0000) * f32::from_bits(0x4B80_0000);
    if n > 127 {
        x *= p127;
        n -= 127;
        if n > 127 {
            x *= p127;
            n -= 127;
            n = n.min(127);
        }
    } else if n < -126 {
        x *= small;
        n += 126 - 24;
        if n < -126 {
            x *= small;
            n += 126 - 24;
            n = n.max(-126);
        }
    }
    x * f32::from_bits(((0x7F + n) as u32) << 23)
}

/// `frexp` : mantisse dans `[0.5, 1)` et exposant binaire.
pub(crate) fn frexp64(x: f64) -> (f64, i32) {
    if x == 0.0 || !x.is_finite() {
        return (x, 0);
    }
    let (x, adj) = if x.is_subnormal() {
        (x * f64::from_bits(0x4350_0000_0000_0000), -54) // 2^54
    } else {
        (x, 0)
    };
    let bits = x.to_bits();
    let e = ((bits >> 52) & 0x7FF) as i32;
    let m = f64::from_bits((bits & !(0x7FF << 52)) | (1022 << 52));
    (m, e - 1022 + adj)
}

fn frexp32(x: f32) -> (f32, i32) {
    let (m, e) = frexp64(f64::from(x));
    (m as f32, e)
}

fn ilogb64(x: f64) -> i32 {
    if x.is_nan() || x.is_infinite() {
        return if x.is_nan() { FP_ILOGBNAN } else { i32::MAX };
    }
    if x == 0.0 {
        return FP_ILOGB0;
    }
    frexp64(x).1 - 1
}

fn next_after64(x: f64, y: f64) -> f64 {
    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    if x == y {
        return y;
    }
    let r = if x == 0.0 {
        f64::from_bits(1).copysign(y)
    } else if (y > x) == (x > 0.0) {
        f64::from_bits(x.to_bits() + 1)
    } else {
        f64::from_bits(x.to_bits() - 1)
    };
    if r.is_infinite() || r.is_subnormal() || r == 0.0 {
        set_errno(ERANGE);
    }
    r
}

fn next_after32(x: f32, y: f64) -> f32 {
    if x.is_nan() || y.is_nan() {
        return (f64::from(x) + y) as f32;
    }
    if f64::from(x) == y {
        return y as f32;
    }
    let up = y > f64::from(x);
    let r = if x == 0.0 {
        f32::from_bits(1).copysign(if up { 1.0 } else { -1.0 })
    } else if up == (x > 0.0) {
        f32::from_bits(x.to_bits() + 1)
    } else {
        f32::from_bits(x.to_bits() - 1)
    };
    if r.is_infinite() || r.is_subnormal() || r == 0.0 {
        set_errno(ERANGE);
    }
    r
}

/// `remainder` IEEE 754 (algorithme fdlibm `__ieee754_remainder`).
fn remainder64(x: f64, p: f64) -> f64 {
    if x.is_nan() || p.is_nan() {
        return x + p;
    }
    if x.is_infinite() || p == 0.0 {
        return domain64();
    }
    let sx = x.is_sign_negative();
    let p = p.abs();
    let mut x = x;
    if (p * 2.0).is_finite() {
        x %= p * 2.0;
    }
    if x.abs() == p {
        return 0.0 * x;
    }
    x = x.abs();
    if p < f64::from_bits(0x0020_0000_0000_0000) {
        if x + x > p {
            x -= p;
            if x + x >= p {
                x -= p;
            }
        }
    } else {
        let half = 0.5 * p;
        if x > half {
            x -= p;
            if x >= half {
                x -= p;
            }
        }
    }
    if sx { -x } else { x }
}

fn fmod64(x: f64, y: f64) -> f64 {
    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    if x.is_infinite() || y == 0.0 {
        return domain64();
    }
    x % y
}

fn fmod32(x: f32, y: f32) -> f32 {
    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    if x.is_infinite() || y == 0.0 {
        return domain32();
    }
    x % y
}

fn log_like64(x: f64, f: fn(f64) -> f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x < 0.0 {
        return domain64();
    }
    if x == 0.0 {
        set_errno(ERANGE);
        return f64::NEG_INFINITY;
    }
    f(x)
}

fn log_like32(x: f32, f: fn(f32) -> f32) -> f32 {
    if x.is_nan() {
        return x;
    }
    if x < 0.0 {
        return domain32();
    }
    if x == 0.0 {
        set_errno(ERANGE);
        return f32::NEG_INFINITY;
    }
    f(x)
}

fn round_to_i32(r: f64) -> i32 {
    if r.is_nan() || !(-2_147_483_648.0..=2_147_483_647.0).contains(&r) {
        set_errno(EDOM);
        return i32::MIN;
    }
    r as i32
}

fn round_to_i64(r: f64) -> i64 {
    if r.is_nan() || !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&r) {
        set_errno(EDOM);
        return i64::MIN;
    }
    r as i64
}

fn pow64(x: f64, y: f64) -> f64 {
    let r = x.powf(y);
    if x == 0.0 && y < 0.0 {
        set_errno(ERANGE);
    } else if x.is_finite() && x < 0.0 && y.is_finite() && y.fract() != 0.0 {
        return domain64();
    } else if x.is_finite() && y.is_finite() && x != 0.0 && (r.is_infinite() || r == 0.0) {
        set_errno(ERANGE);
    }
    r
}

fn pow32(x: f32, y: f32) -> f32 {
    let r = x.powf(y);
    if x == 0.0 && y < 0.0 {
        set_errno(ERANGE);
    } else if x.is_finite() && x < 0.0 && y.is_finite() && y.fract() != 0.0 {
        return domain32();
    } else if x.is_finite() && y.is_finite() && x != 0.0 && (r.is_infinite() || r == 0.0) {
        set_errno(ERANGE);
    }
    r
}

fn modf64(x: f64) -> (f64, f64) {
    if x.is_nan() {
        return (x, x);
    }
    if x.is_infinite() {
        return (0.0f64.copysign(x), x);
    }
    let i = x.trunc();
    ((x - i).copysign(x), i)
}

fn modf32(x: f32) -> (f32, f32) {
    if x.is_nan() {
        return (x, x);
    }
    if x.is_infinite() {
        return (0.0f32.copysign(x), x);
    }
    let i = x.trunc();
    ((x - i).copysign(x), i)
}

// =====================================================================================
// Points d'entrée mathématiques
// =====================================================================================

macro_rules! exports_table {
    ($register:ident; $( $sym:literal => fn $name:ident($($a:ident : $t:ty),*) -> $r:ty [$dflt:expr] $body:block )*) => {
        $(
            extern "win64" fn $name($($a: $t),*) -> $r {
                ffi_guard!(CHANNEL, $sym, $dflt, $body)
            }
        )*
        fn $register(m: &mut HashMap<&'static str, usize>) {
            $( m.insert($sym, $name as *const () as usize); )*
        }
    };
}

exports_table! { register_math;
    // --- Classification ---
    "_dtest" => fn m_dtest(px: *const f64) -> i16 [FP_NAN] {
        if px.is_null() { set_errno(EINVAL); return FP_NAN; }
        // SAFETY: pointeur non nul fourni par l'invité vers un `double`.
        fp_short(unsafe { px.read_unaligned() }.classify())
    }
    "_ldtest" => fn m_ldtest(px: *const f64) -> i16 [FP_NAN] {
        if px.is_null() { set_errno(EINVAL); return FP_NAN; }
        // SAFETY: `long double` MSVC = `double` ; pointeur non nul.
        fp_short(unsafe { px.read_unaligned() }.classify())
    }
    "_fdtest" => fn m_fdtest(px: *const f32) -> i16 [FP_NAN] {
        if px.is_null() { set_errno(EINVAL); return FP_NAN; }
        // SAFETY: pointeur non nul fourni par l'invité vers un `float`.
        fp_short(unsafe { px.read_unaligned() }.classify())
    }
    "_dclass" => fn m_dclass(x: f64) -> i16 [FP_NAN] { fp_short(x.classify()) }
    "_fdclass" => fn m_fdclass(x: f32) -> i16 [FP_NAN] { fp_short(x.classify()) }
    "_dsign" => fn m_dsign(x: f64) -> i32 [0] { if x.is_sign_negative() { 0x8000 } else { 0 } }
    "_fdsign" => fn m_fdsign(x: f32) -> i32 [0] { if x.is_sign_negative() { 0x8000 } else { 0 } }
    "_finite" => fn m_finite(x: f64) -> i32 [0] { i32::from(x.is_finite()) }
    "_finitef" => fn m_finitef(x: f32) -> i32 [0] { i32::from(x.is_finite()) }
    "_isnan" => fn m_isnan(x: f64) -> i32 [0] { i32::from(x.is_nan()) }
    "_isnanf" => fn m_isnanf(x: f32) -> i32 [0] { i32::from(x.is_nan()) }
    "_fpclass" => fn m_fpclass(x: f64) -> i32 [FPCLASS_QNAN] {
        fpclass_mask(x.classify(), x.is_sign_negative(), is_snan64(x))
    }
    "_fpclassf" => fn m_fpclassf(x: f32) -> i32 [FPCLASS_QNAN] {
        fpclass_mask(x.classify(), x.is_sign_negative(), is_snan32(x))
    }

    // --- Trigonométrie ---
    "sin" => fn m_sin(x: f64) -> f64 [IND64] { if x.is_infinite() { domain64() } else { x.sin() } }
    "sinf" => fn m_sinf(x: f32) -> f32 [IND32] { if x.is_infinite() { domain32() } else { x.sin() } }
    "cos" => fn m_cos(x: f64) -> f64 [IND64] { if x.is_infinite() { domain64() } else { x.cos() } }
    "cosf" => fn m_cosf(x: f32) -> f32 [IND32] { if x.is_infinite() { domain32() } else { x.cos() } }
    "tan" => fn m_tan(x: f64) -> f64 [IND64] { if x.is_infinite() { domain64() } else { x.tan() } }
    "tanf" => fn m_tanf(x: f32) -> f32 [IND32] { if x.is_infinite() { domain32() } else { x.tan() } }
    "acos" => fn m_acos(x: f64) -> f64 [IND64] { if x.abs() > 1.0 { domain64() } else { x.acos() } }
    "acosf" => fn m_acosf(x: f32) -> f32 [IND32] { if x.abs() > 1.0 { domain32() } else { x.acos() } }
    "asin" => fn m_asin(x: f64) -> f64 [IND64] { if x.abs() > 1.0 { domain64() } else { x.asin() } }
    "asinf" => fn m_asinf(x: f32) -> f32 [IND32] { if x.abs() > 1.0 { domain32() } else { x.asin() } }
    "atan" => fn m_atan(x: f64) -> f64 [IND64] { x.atan() }
    "atanf" => fn m_atanf(x: f32) -> f32 [IND32] { x.atan() }
    "atan2" => fn m_atan2(y: f64, x: f64) -> f64 [IND64] { y.atan2(x) }
    "atan2f" => fn m_atan2f(y: f32, x: f32) -> f32 [IND32] { y.atan2(x) }
    "sinh" => fn m_sinh(x: f64) -> f64 [IND64] { ovf64(x.is_finite(), x.sinh()) }
    "sinhf" => fn m_sinhf(x: f32) -> f32 [IND32] { ovf32(x.is_finite(), x.sinh()) }
    "cosh" => fn m_cosh(x: f64) -> f64 [IND64] { ovf64(x.is_finite(), x.cosh()) }
    "coshf" => fn m_coshf(x: f32) -> f32 [IND32] { ovf32(x.is_finite(), x.cosh()) }
    "tanh" => fn m_tanh(x: f64) -> f64 [IND64] { x.tanh() }
    "tanhf" => fn m_tanhf(x: f32) -> f32 [IND32] { x.tanh() }
    "hypot" => fn m_hypot(x: f64, y: f64) -> f64 [IND64] { ovf64(x.is_finite() && y.is_finite(), x.hypot(y)) }
    "hypotf" => fn m_hypotf(x: f32, y: f32) -> f32 [IND32] { ovf32(x.is_finite() && y.is_finite(), x.hypot(y)) }

    // --- Exponentielles / logarithmes ---
    "exp" => fn m_exp(x: f64) -> f64 [IND64] { range64(x.is_finite(), x.exp()) }
    "expf" => fn m_expf(x: f32) -> f32 [IND32] { range32(x.is_finite(), x.exp()) }
    "exp2" => fn m_exp2(x: f64) -> f64 [IND64] { range64(x.is_finite(), x.exp2()) }
    "exp2f" => fn m_exp2f(x: f32) -> f32 [IND32] { range32(x.is_finite(), x.exp2()) }
    "expm1" => fn m_expm1(x: f64) -> f64 [IND64] { ovf64(x.is_finite(), x.exp_m1()) }
    "expm1f" => fn m_expm1f(x: f32) -> f32 [IND32] { ovf32(x.is_finite(), x.exp_m1()) }
    "log" => fn m_log(x: f64) -> f64 [IND64] { log_like64(x, f64::ln) }
    "logf" => fn m_logf(x: f32) -> f32 [IND32] { log_like32(x, f32::ln) }
    "log10" => fn m_log10(x: f64) -> f64 [IND64] { log_like64(x, f64::log10) }
    "log10f" => fn m_log10f(x: f32) -> f32 [IND32] { log_like32(x, f32::log10) }
    "log2" => fn m_log2(x: f64) -> f64 [IND64] { log_like64(x, f64::log2) }
    "log2f" => fn m_log2f(x: f32) -> f32 [IND32] { log_like32(x, f32::log2) }
    "log1p" => fn m_log1p(x: f64) -> f64 [IND64] {
        if x.is_nan() || x > -1.0 { x.ln_1p() } else if x == -1.0 { set_errno(ERANGE); f64::NEG_INFINITY } else { domain64() }
    }
    "log1pf" => fn m_log1pf(x: f32) -> f32 [IND32] {
        if x.is_nan() || x > -1.0 { x.ln_1p() } else if x == -1.0 { set_errno(ERANGE); f32::NEG_INFINITY } else { domain32() }
    }
    "logb" => fn m_logb(x: f64) -> f64 [IND64] {
        if x == 0.0 { set_errno(ERANGE); f64::NEG_INFINITY }
        else if x.is_nan() { x } else if x.is_infinite() { f64::INFINITY } else { f64::from(ilogb64(x)) }
    }
    "logbf" => fn m_logbf(x: f32) -> f32 [IND32] {
        if x == 0.0 { set_errno(ERANGE); f32::NEG_INFINITY }
        else if x.is_nan() { x } else if x.is_infinite() { f32::INFINITY } else { ilogb64(f64::from(x)) as f32 }
    }
    "ilogb" => fn m_ilogb(x: f64) -> i32 [FP_ILOGBNAN] { ilogb64(x) }
    "ilogbf" => fn m_ilogbf(x: f32) -> i32 [FP_ILOGBNAN] { ilogb64(f64::from(x)) }
    "pow" => fn m_pow(x: f64, y: f64) -> f64 [IND64] { pow64(x, y) }
    "powf" => fn m_powf(x: f32, y: f32) -> f32 [IND32] { pow32(x, y) }
    "sqrt" => fn m_sqrt(x: f64) -> f64 [IND64] { if x < 0.0 { domain64() } else { x.sqrt() } }
    "sqrtf" => fn m_sqrtf(x: f32) -> f32 [IND32] { if x < 0.0 { domain32() } else { x.sqrt() } }
    "cbrt" => fn m_cbrt(x: f64) -> f64 [IND64] { x.cbrt() }
    "cbrtf" => fn m_cbrtf(x: f32) -> f32 [IND32] { x.cbrt() }

    // --- Fonctions spéciales ---
    "erf" => fn m_erf(x: f64) -> f64 [IND64] {
        // SAFETY: fonction pure de la libm hôte.
        unsafe { erf(x) }
    }
    "erff" => fn m_erff(x: f32) -> f32 [IND32] {
        // SAFETY: fonction pure de la libm hôte.
        unsafe { erff(x) }
    }
    "erfc" => fn m_erfc(x: f64) -> f64 [IND64] {
        // SAFETY: fonction pure de la libm hôte.
        let r = unsafe { erfc(x) };
        if x.is_finite() && r == 0.0 { set_errno(ERANGE); }
        r
    }
    "erfcf" => fn m_erfcf(x: f32) -> f32 [IND32] {
        // SAFETY: fonction pure de la libm hôte.
        let r = unsafe { erfcf(x) };
        if x.is_finite() && r == 0.0 { set_errno(ERANGE); }
        r
    }

    // --- Arrondis ---
    "ceil" => fn m_ceil(x: f64) -> f64 [IND64] { x.ceil() }
    "ceilf" => fn m_ceilf(x: f32) -> f32 [IND32] { x.ceil() }
    "floor" => fn m_floor(x: f64) -> f64 [IND64] { x.floor() }
    "floorf" => fn m_floorf(x: f32) -> f32 [IND32] { x.floor() }
    "round" => fn m_round(x: f64) -> f64 [IND64] { x.round() }
    "roundf" => fn m_roundf(x: f32) -> f32 [IND32] { x.round() }
    "trunc" => fn m_trunc(x: f64) -> f64 [IND64] { x.trunc() }
    "truncf" => fn m_truncf(x: f32) -> f32 [IND32] { x.trunc() }
    "rint" => fn m_rint(x: f64) -> f64 [IND64] { x.round_ties_even() }
    "rintf" => fn m_rintf(x: f32) -> f32 [IND32] { x.round_ties_even() }
    "lround" => fn m_lround(x: f64) -> i32 [i32::MIN] { round_to_i32(x.round()) }
    "lroundf" => fn m_lroundf(x: f32) -> i32 [i32::MIN] { round_to_i32(f64::from(x.round())) }
    "llround" => fn m_llround(x: f64) -> i64 [i64::MIN] { round_to_i64(x.round()) }
    "llroundf" => fn m_llroundf(x: f32) -> i64 [i64::MIN] { round_to_i64(f64::from(x.round())) }
    "lrint" => fn m_lrint(x: f64) -> i32 [i32::MIN] { round_to_i32(x.round_ties_even()) }
    "lrintf" => fn m_lrintf(x: f32) -> i32 [i32::MIN] { round_to_i32(f64::from(x.round_ties_even())) }
    "llrint" => fn m_llrint(x: f64) -> i64 [i64::MIN] { round_to_i64(x.round_ties_even()) }
    "llrintf" => fn m_llrintf(x: f32) -> i64 [i64::MIN] { round_to_i64(f64::from(x.round_ties_even())) }
    "modf" => fn m_modf(x: f64, ip: *mut f64) -> f64 [IND64] {
        let (frac, int) = modf64(x);
        if !ip.is_null() {
            // SAFETY: pointeur de sortie non nul fourni par l'invité.
            unsafe { ip.write_unaligned(int) };
        }
        frac
    }
    "modff" => fn m_modff(x: f32, ip: *mut f32) -> f32 [IND32] {
        let (frac, int) = modf32(x);
        if !ip.is_null() {
            // SAFETY: pointeur de sortie non nul fourni par l'invité.
            unsafe { ip.write_unaligned(int) };
        }
        frac
    }

    // --- Restes / manipulation ---
    "fmod" => fn m_fmod(x: f64, y: f64) -> f64 [IND64] { fmod64(x, y) }
    "fmodf" => fn m_fmodf(x: f32, y: f32) -> f32 [IND32] { fmod32(x, y) }
    "remainder" => fn m_remainder(x: f64, y: f64) -> f64 [IND64] { remainder64(x, y) }
    "remainderf" => fn m_remainderf(x: f32, y: f32) -> f32 [IND32] {
        if x.is_infinite() || y == 0.0 { domain32() } else { remainder64(f64::from(x), f64::from(y)) as f32 }
    }
    "nextafter" => fn m_nextafter(x: f64, y: f64) -> f64 [IND64] { next_after64(x, y) }
    "nextafterf" => fn m_nextafterf(x: f32, y: f32) -> f32 [IND32] { next_after32(x, f64::from(y)) }
    "nexttowardf" => fn m_nexttowardf(x: f32, y: f64) -> f32 [IND32] { next_after32(x, y) }
    "copysign" => fn m_copysign(x: f64, y: f64) -> f64 [IND64] { x.copysign(y) }
    "copysignf" => fn m_copysignf(x: f32, y: f32) -> f32 [IND32] { x.copysign(y) }
    "_chgsignf" => fn m_chgsignf(x: f32) -> f32 [IND32] { -x }
    "fabs" => fn m_fabs(x: f64) -> f64 [IND64] { x.abs() }
    "fabsf" => fn m_fabsf(x: f32) -> f32 [IND32] { x.abs() }
    "fmin" => fn m_fmin(x: f64, y: f64) -> f64 [IND64] { x.min(y) }
    "fminf" => fn m_fminf(x: f32, y: f32) -> f32 [IND32] { x.min(y) }
    "fmax" => fn m_fmax(x: f64, y: f64) -> f64 [IND64] { x.max(y) }
    "fmaxf" => fn m_fmaxf(x: f32, y: f32) -> f32 [IND32] { x.max(y) }
    "fdim" => fn m_fdim(x: f64, y: f64) -> f64 [IND64] {
        if x.is_nan() || y.is_nan() { x + y } else if x > y { ovf64(x.is_finite() && y.is_finite(), x - y) } else { 0.0 }
    }
    "fdimf" => fn m_fdimf(x: f32, y: f32) -> f32 [IND32] {
        if x.is_nan() || y.is_nan() { x + y } else if x > y { ovf32(x.is_finite() && y.is_finite(), x - y) } else { 0.0 }
    }
    "fma" => fn m_fma(x: f64, y: f64, z: f64) -> f64 [IND64] {
        if (x.is_infinite() && y == 0.0) || (x == 0.0 && y.is_infinite()) { return domain64(); }
        ovf64(x.is_finite() && y.is_finite() && z.is_finite(), x.mul_add(y, z))
    }
    "fmaf" => fn m_fmaf(x: f32, y: f32, z: f32) -> f32 [IND32] {
        if (x.is_infinite() && y == 0.0) || (x == 0.0 && y.is_infinite()) { return domain32(); }
        ovf32(x.is_finite() && y.is_finite() && z.is_finite(), x.mul_add(y, z))
    }
    "frexp" => fn m_frexp(x: f64, e: *mut i32) -> f64 [IND64] {
        let (m, ex) = frexp64(x);
        if !e.is_null() {
            // SAFETY: pointeur de sortie non nul fourni par l'invité.
            unsafe { e.write_unaligned(ex) };
        }
        m
    }
    "frexpf" => fn m_frexpf(x: f32, e: *mut i32) -> f32 [IND32] {
        let (m, ex) = frexp32(x);
        if !e.is_null() {
            // SAFETY: pointeur de sortie non nul fourni par l'invité.
            unsafe { e.write_unaligned(ex) };
        }
        m
    }
    "scalbn" => fn m_scalbn(x: f64, n: i32) -> f64 [IND64] { range64(x.is_finite() && x != 0.0, scalbn64(x, n)) }
    "scalbnf" => fn m_scalbnf(x: f32, n: i32) -> f32 [IND32] { range32(x.is_finite() && x != 0.0, scalbn32(x, n)) }
    "scalbln" => fn m_scalbln(x: f64, n: i32) -> f64 [IND64] { range64(x.is_finite() && x != 0.0, scalbn64(x, n)) }
    "scalblnf" => fn m_scalblnf(x: f32, n: i32) -> f32 [IND32] { range32(x.is_finite() && x != 0.0, scalbn32(x, n)) }
}

extern "win64" fn m_chgsign(x: f64) -> f64 {
    ffi_guard!(CHANNEL, "_chgsign", IND64, { -x })
}

// =====================================================================================
// Temps (`api-ms-win-crt-time`)
// =====================================================================================

/// `struct tm` MSVC (9 × `int`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Tm {
    pub tm_sec: i32,
    pub tm_min: i32,
    pub tm_hour: i32,
    pub tm_mday: i32,
    pub tm_mon: i32,
    pub tm_year: i32,
    pub tm_wday: i32,
    pub tm_yday: i32,
    pub tm_isdst: i32,
}
const _: () = assert!(size_of::<Tm>() == 36);
const _: () = assert!(core::mem::offset_of!(Tm, tm_isdst) == 32);

/// `struct __timeb64` MSVC.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Timeb64 {
    pub time: i64,
    pub millitm: u16,
    pub timezone: i16,
    pub dstflag: i16,
}
const _: () = assert!(size_of::<Timeb64>() == 16);
const _: () = assert!(core::mem::offset_of!(Timeb64, dstflag) == 12);

/// `_MAX__TIME64_T` : 3000-12-31 23:59:59 UTC.
const MAX_TIME64: i64 = 0x7_9340_6FFF;
const MIN_LOCAL_TIME: i64 = -12 * 3600;
const MAX_LOCAL_TIME: i64 = 13 * 3600;
const CLOCKS_PER_SEC: u128 = 1000;

thread_local! {
    static TM_BUF: UnsafeCell<Tm> = const { UnsafeCell::new(Tm {
        tm_sec: 0, tm_min: 0, tm_hour: 0, tm_mday: 0, tm_mon: 0,
        tm_year: 0, tm_wday: 0, tm_yday: 0, tm_isdst: 0,
    }) };
}

fn process_start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Décompose un temps UTC (secondes Unix) en `struct tm`.
pub(crate) fn tm_from_utc(t: i64) -> Tm {
    let days = t.div_euclid(86_400);
    let rem = t.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    Tm {
        tm_sec: (rem % 60) as i32,
        tm_min: ((rem / 60) % 60) as i32,
        tm_hour: (rem / 3600) as i32,
        tm_mday: d as i32,
        tm_mon: (m - 1) as i32,
        tm_year: (y - 1900) as i32,
        tm_wday: (days + 4).rem_euclid(7) as i32,
        tm_yday: (days - days_from_civil(y, 1, 1)) as i32,
        tm_isdst: 0,
    }
}

/// Convertit une `struct tm` (champs éventuellement hors bornes) en secondes UTC.
pub(crate) fn utc_from_tm(tm: &Tm) -> i64 {
    let mon = i64::from(tm.tm_mon);
    let year = i64::from(tm.tm_year) + 1900 + mon.div_euclid(12);
    let mon = mon.rem_euclid(12);
    let days = days_from_civil(year, mon + 1, 1) + i64::from(tm.tm_mday) - 1;
    days * 86_400 + i64::from(tm.tm_hour) * 3600 + i64::from(tm.tm_min) * 60 + i64::from(tm.tm_sec)
}

fn now_unix() -> (i64, u32) {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_millis()),
        Err(_) => (0, 0),
    }
}

fn host_localtime(t: i64) -> Option<(Tm, i64, String)> {
    // SAFETY: `libc::tm` est un POD ; zéro est une valeur valide.
    let mut out: libc::tm = unsafe { core::mem::zeroed() };
    let tt: libc::time_t = t as libc::time_t;
    // SAFETY: pointeurs valides vers des variables locales ; `localtime_r` est réentrant.
    let r = unsafe { libc::localtime_r(&tt, &mut out) };
    if r.is_null() {
        return None;
    }
    let zone = if out.tm_zone.is_null() {
        String::new()
    } else {
        // SAFETY: `tm_zone` pointe vers une chaîne statique de la libc.
        unsafe { std::ffi::CStr::from_ptr(out.tm_zone) }
            .to_string_lossy()
            .into_owned()
    };
    Some((
        Tm {
            tm_sec: out.tm_sec,
            tm_min: out.tm_min,
            tm_hour: out.tm_hour,
            tm_mday: out.tm_mday,
            tm_mon: out.tm_mon,
            tm_year: out.tm_year,
            tm_wday: out.tm_wday,
            tm_yday: out.tm_yday,
            tm_isdst: i32::from(out.tm_isdst > 0),
        },
        out.tm_gmtoff as i64,
        sanitize_zone(&zone),
    ))
}

/// Ne conserve qu'une abréviation de fuseau (jamais de chemin hôte).
fn sanitize_zone(z: &str) -> String {
    let s: String = z
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '-')
        .take(63)
        .collect();
    if s.is_empty() { "UTC".to_string() } else { s }
}

/// Informations de fuseau à la MSVC (`_timezone`, `_daylight`, `_dstbias`, `_tzname`).
#[derive(Debug, Clone)]
pub(crate) struct TzInfo {
    pub timezone: i32,
    pub daylight: i32,
    pub dstbias: i32,
    pub names: [String; 2],
}

pub(crate) fn tz_info() -> TzInfo {
    // SAFETY: `tzset` initialise l'état de fuseau de la libc hôte.
    unsafe { tzset() };
    let (now, _) = now_unix();
    let year = i64::from(tm_from_utc(now).tm_year) + 1900;
    let jan = days_from_civil(year, 1, 1) * 86_400;
    let jul = days_from_civil(year, 7, 1) * 86_400;
    let a = host_localtime(jan);
    let b = host_localtime(jul);
    match (a, b) {
        (Some((_, off_a, name_a)), Some((_, off_b, name_b))) => {
            let ((std_off, std_name), (dst_off, dst_name)) = if off_a <= off_b {
                ((off_a, name_a), (off_b, name_b))
            } else {
                ((off_b, name_b), (off_a, name_a))
            };
            let daylight = std_off != dst_off;
            TzInfo {
                timezone: -(std_off as i32),
                daylight: i32::from(daylight),
                dstbias: if daylight {
                    -((dst_off - std_off) as i32)
                } else {
                    -3600
                },
                names: [std_name.clone(), if daylight { dst_name } else { std_name }],
            }
        }
        _ => TzInfo {
            timezone: 0,
            daylight: 0,
            dstbias: -3600,
            names: ["UTC".to_string(), "UTC".to_string()],
        },
    }
}

fn tm_buf() -> *mut Tm {
    TM_BUF.with(|b| b.get())
}

const TM_INVALID: Tm = Tm {
    tm_sec: -1,
    tm_min: -1,
    tm_hour: -1,
    tm_mday: -1,
    tm_mon: -1,
    tm_year: -1,
    tm_wday: -1,
    tm_yday: -1,
    tm_isdst: -1,
};

fn gmtime_checked(t: i64) -> Result<Tm, i32> {
    if !(MIN_LOCAL_TIME..=MAX_TIME64 + MAX_LOCAL_TIME).contains(&t) {
        return Err(EINVAL);
    }
    Ok(tm_from_utc(t))
}

fn localtime_checked(t: i64) -> Result<Tm, i32> {
    if !(0..=MAX_TIME64).contains(&t) {
        return Err(EINVAL);
    }
    host_localtime(t).map(|(tm, _, _)| tm).ok_or(EINVAL)
}

/// Normalise `tm` en heure locale et renvoie le `__time64_t` correspondant.
fn mktime_local(tm: &mut Tm) -> i64 {
    // SAFETY: `libc::tm` est un POD ; zéro est une valeur valide.
    let mut lt: libc::tm = unsafe { core::mem::zeroed() };
    lt.tm_sec = tm.tm_sec;
    lt.tm_min = tm.tm_min;
    lt.tm_hour = tm.tm_hour;
    lt.tm_mday = tm.tm_mday;
    lt.tm_mon = tm.tm_mon;
    lt.tm_year = tm.tm_year;
    lt.tm_isdst = tm.tm_isdst;
    // SAFETY: `lt` est une `struct tm` locale valide.
    let t = unsafe { libc::mktime(&mut lt) } as i64;
    if t == -1 && lt.tm_year == tm.tm_year && lt.tm_wday == 0 && lt.tm_yday == 0 {
        set_errno(EINVAL);
        return -1;
    }
    if !(0..=MAX_TIME64).contains(&t) {
        set_errno(EINVAL);
        return -1;
    }
    *tm = Tm {
        tm_sec: lt.tm_sec,
        tm_min: lt.tm_min,
        tm_hour: lt.tm_hour,
        tm_mday: lt.tm_mday,
        tm_mon: lt.tm_mon,
        tm_year: lt.tm_year,
        tm_wday: lt.tm_wday,
        tm_yday: lt.tm_yday,
        tm_isdst: i32::from(lt.tm_isdst > 0),
    };
    t
}

// --- strftime ---

const DAYS_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const DAYS_LONG: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTHS_SHORT: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTHS_LONG: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Année et semaine ISO 8601 (`%G`, `%V`).
fn iso_week(tm: &Tm) -> (i64, i32) {
    fn weeks_in(y: i64) -> i32 {
        let p =
            |y: i64| (y + y.div_euclid(4) - y.div_euclid(100) + y.div_euclid(400)).rem_euclid(7);
        if p(y) == 4 || p(y - 1) == 3 { 53 } else { 52 }
    }
    let year = i64::from(tm.tm_year) + 1900;
    let wday = (tm.tm_wday + 6) % 7; // lundi = 0
    let week = (tm.tm_yday - wday + 10) / 7;
    if week < 1 {
        (year - 1, weeks_in(year - 1))
    } else if week > weeks_in(year) {
        (year + 1, 1)
    } else {
        (year, week)
    }
}

fn check_range(v: i32, lo: i32, hi: i32) -> Result<(), i32> {
    if (lo..=hi).contains(&v) {
        Ok(())
    } else {
        Err(EINVAL)
    }
}

/// Moteur `strftime` commun (unités UTF-16 ; le format étroit est traité octet par octet).
pub(crate) fn strftime_units(fmt: &[u16], tm: &Tm) -> Result<Vec<u16>, i32> {
    let mut out: Vec<u16> = Vec::with_capacity(fmt.len() * 2);
    let mut i = 0usize;
    let push = |out: &mut Vec<u16>, s: &str| out.extend(s.encode_utf16());
    let num = |out: &mut Vec<u16>, v: i64, width: usize, alt: bool| {
        let s = if alt {
            format!("{v}")
        } else {
            format!("{v:0width$}")
        };
        out.extend(s.encode_utf16());
    };
    while i < fmt.len() {
        let c = fmt[i];
        i += 1;
        if c != u16::from(b'%') {
            out.push(c);
            continue;
        }
        let mut alt = false;
        if i < fmt.len() && fmt[i] == u16::from(b'#') {
            alt = true;
            i += 1;
        }
        if i < fmt.len() && (fmt[i] == u16::from(b'E') || fmt[i] == u16::from(b'O')) {
            i += 1;
        }
        let Some(&spec) = fmt.get(i) else {
            return Err(EINVAL);
        };
        i += 1;
        let spec = u8::try_from(spec).map_err(|_| EINVAL)?;
        let year = i64::from(tm.tm_year) + 1900;
        match spec {
            b'a' | b'A' => {
                check_range(tm.tm_wday, 0, 6)?;
                let t = if spec == b'a' { DAYS_SHORT } else { DAYS_LONG };
                push(&mut out, t[tm.tm_wday as usize]);
            }
            b'b' | b'h' | b'B' => {
                check_range(tm.tm_mon, 0, 11)?;
                let t = if spec == b'B' {
                    MONTHS_LONG
                } else {
                    MONTHS_SHORT
                };
                push(&mut out, t[tm.tm_mon as usize]);
            }
            b'c' => {
                let sub = if alt {
                    "%A, %B %#d, %Y %H:%M:%S"
                } else {
                    "%m/%d/%y %H:%M:%S"
                };
                out.extend(strftime_units(&sub.encode_utf16().collect::<Vec<_>>(), tm)?);
            }
            b'x' => {
                let sub = if alt { "%A, %B %#d, %Y" } else { "%m/%d/%y" };
                out.extend(strftime_units(&sub.encode_utf16().collect::<Vec<_>>(), tm)?);
            }
            b'X' | b'T' => {
                out.extend(strftime_units(
                    &"%H:%M:%S".encode_utf16().collect::<Vec<_>>(),
                    tm,
                )?);
            }
            b'D' => {
                out.extend(strftime_units(
                    &"%m/%d/%y".encode_utf16().collect::<Vec<_>>(),
                    tm,
                )?);
            }
            b'F' => {
                out.extend(strftime_units(
                    &"%Y-%m-%d".encode_utf16().collect::<Vec<_>>(),
                    tm,
                )?);
            }
            b'r' => {
                out.extend(strftime_units(
                    &"%I:%M:%S %p".encode_utf16().collect::<Vec<_>>(),
                    tm,
                )?);
            }
            b'R' => {
                out.extend(strftime_units(
                    &"%H:%M".encode_utf16().collect::<Vec<_>>(),
                    tm,
                )?);
            }
            b'C' => {
                check_range(tm.tm_year, -1900, 8099)?;
                num(&mut out, year / 100, 2, alt);
            }
            b'd' => {
                check_range(tm.tm_mday, 1, 31)?;
                num(&mut out, i64::from(tm.tm_mday), 2, alt);
            }
            b'e' => {
                check_range(tm.tm_mday, 1, 31)?;
                if alt {
                    num(&mut out, i64::from(tm.tm_mday), 0, true);
                } else {
                    push(&mut out, &format!("{:>2}", tm.tm_mday));
                }
            }
            b'g' | b'G' | b'V' => {
                check_range(tm.tm_year, -1900, 8099)?;
                check_range(tm.tm_wday, 0, 6)?;
                check_range(tm.tm_yday, 0, 365)?;
                let (iy, w) = iso_week(tm);
                match spec {
                    b'g' => num(&mut out, iy.rem_euclid(100), 2, alt),
                    b'G' => num(&mut out, iy, 4, alt),
                    _ => num(&mut out, i64::from(w), 2, alt),
                }
            }
            b'H' => {
                check_range(tm.tm_hour, 0, 23)?;
                num(&mut out, i64::from(tm.tm_hour), 2, alt);
            }
            b'I' => {
                check_range(tm.tm_hour, 0, 23)?;
                let h = match tm.tm_hour % 12 {
                    0 => 12,
                    h => h,
                };
                num(&mut out, i64::from(h), 2, alt);
            }
            b'j' => {
                check_range(tm.tm_yday, 0, 365)?;
                num(&mut out, i64::from(tm.tm_yday) + 1, 3, alt);
            }
            b'm' => {
                check_range(tm.tm_mon, 0, 11)?;
                num(&mut out, i64::from(tm.tm_mon) + 1, 2, alt);
            }
            b'M' => {
                check_range(tm.tm_min, 0, 59)?;
                num(&mut out, i64::from(tm.tm_min), 2, alt);
            }
            b'n' => out.push(u16::from(b'\n')),
            b't' => out.push(u16::from(b'\t')),
            b'p' => {
                check_range(tm.tm_hour, 0, 23)?;
                push(&mut out, if tm.tm_hour < 12 { "AM" } else { "PM" });
            }
            b'S' => {
                check_range(tm.tm_sec, 0, 60)?;
                num(&mut out, i64::from(tm.tm_sec), 2, alt);
            }
            b'u' => {
                check_range(tm.tm_wday, 0, 6)?;
                let u = if tm.tm_wday == 0 { 7 } else { tm.tm_wday };
                num(&mut out, i64::from(u), 1, alt);
            }
            b'w' => {
                check_range(tm.tm_wday, 0, 6)?;
                num(&mut out, i64::from(tm.tm_wday), 1, alt);
            }
            b'U' | b'W' => {
                check_range(tm.tm_wday, 0, 6)?;
                check_range(tm.tm_yday, 0, 365)?;
                let wd = if spec == b'U' {
                    tm.tm_wday
                } else {
                    (tm.tm_wday + 6) % 7
                };
                num(&mut out, i64::from((tm.tm_yday + 7 - wd) / 7), 2, alt);
            }
            b'y' => {
                check_range(tm.tm_year, -1900, 8099)?;
                num(&mut out, year.rem_euclid(100), 2, alt);
            }
            b'Y' => {
                check_range(tm.tm_year, -1900, 8099)?;
                num(&mut out, year, 4, alt);
            }
            b'z' | b'Z' => {
                let tz = tz_info();
                if spec == b'Z' {
                    push(&mut out, &tz.names[usize::from(tm.tm_isdst > 0)]);
                } else {
                    let west = tz.timezone + if tm.tm_isdst > 0 { tz.dstbias } else { 0 };
                    let east = -west;
                    let sign = if east < 0 { '-' } else { '+' };
                    let a = east.abs();
                    push(
                        &mut out,
                        &format!("{sign}{:02}{:02}", a / 3600, (a / 60) % 60),
                    );
                }
            }
            b'%' => out.push(u16::from(b'%')),
            _ => return Err(EINVAL),
        }
    }
    Ok(out)
}

fn cstr_units(p: *const u8) -> Vec<u16> {
    let mut v = Vec::new();
    let mut i = 0usize;
    loop {
        // SAFETY: chaîne C terminée par 0 fournie par l'invité.
        let b = unsafe { *p.add(i) };
        if b == 0 {
            break;
        }
        v.push(u16::from(b));
        i += 1;
    }
    v
}

fn wstr_units(p: *const u16) -> Vec<u16> {
    let mut v = Vec::new();
    let mut i = 0usize;
    loop {
        // SAFETY: chaîne UTF-16 terminée par 0 fournie par l'invité.
        let c = unsafe { *p.add(i) };
        if c == 0 {
            break;
        }
        v.push(c);
        i += 1;
    }
    v
}

fn write_strz<T: Copy + Default>(buf: *mut T, max: usize, data: &[T]) -> usize {
    if data.len() >= max {
        // SAFETY: `buf` non nul et `max > 0` vérifiés par l'appelant.
        unsafe { *buf = T::default() };
        set_errno(ERANGE);
        return 0;
    }
    // SAFETY: `buf` dispose d'au moins `max > data.len()` éléments.
    unsafe {
        core::ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len());
        *buf.add(data.len()) = T::default();
    }
    data.len()
}

fn read_tm(p: *const Tm) -> Option<Tm> {
    if p.is_null() {
        None
    } else {
        // SAFETY: pointeur non nul vers une `struct tm` invitée.
        Some(unsafe { p.read_unaligned() })
    }
}

fn write_tm(p: *mut Tm, tm: Tm) {
    // SAFETY: pointeur non nul (vérifié par l'appelant) vers une `struct tm` invitée.
    unsafe { p.write_unaligned(tm) };
}

exports_table! { register_time;
    "_time64" => fn t_time64(out: *mut i64) -> i64 [-1] {
        let (t, _) = now_unix();
        if !out.is_null() {
            // SAFETY: pointeur de sortie non nul fourni par l'invité.
            unsafe { out.write_unaligned(t) };
        }
        t
    }
    "_difftime64" => fn t_difftime64(a: i64, b: i64) -> f64 [0.0] {
        if a < 0 || b < 0 {
            set_errno(EINVAL);
            return 0.0;
        }
        (a - b) as f64
    }
    "difftime" => fn t_difftime(a: i64, b: i64) -> f64 [0.0] {
        if a < 0 || b < 0 {
            set_errno(EINVAL);
            return 0.0;
        }
        (a - b) as f64
    }
    "clock" => fn t_clock() -> i32 [-1] {
        let ms = process_start().elapsed().as_millis() * CLOCKS_PER_SEC / 1000;
        i32::try_from(ms).unwrap_or(-1)
    }
    "_ftime64" => fn t_ftime64(tb: *mut Timeb64) -> () [()] {
        if tb.is_null() {
            set_errno(EINVAL);
            return;
        }
        fill_timeb(tb);
    }
    "_ftime64_s" => fn t_ftime64_s(tb: *mut Timeb64) -> i32 [EINVAL] {
        if tb.is_null() {
            set_errno(EINVAL);
            return EINVAL;
        }
        fill_timeb(tb);
        0
    }
    "_gmtime64" => fn t_gmtime64(t: *const i64) -> *mut Tm [core::ptr::null_mut()] {
        if t.is_null() {
            set_errno(EINVAL);
            return core::ptr::null_mut();
        }
        // SAFETY: pointeur non nul vers un `__time64_t`.
        match gmtime_checked(unsafe { t.read_unaligned() }) {
            Ok(tm) => {
                let b = tm_buf();
                write_tm(b, tm);
                b
            }
            Err(e) => {
                set_errno(e);
                core::ptr::null_mut()
            }
        }
    }
    "_gmtime64_s" => fn t_gmtime64_s(out: *mut Tm, t: *const i64) -> i32 [EINVAL] {
        if out.is_null() {
            set_errno(EINVAL);
            return EINVAL;
        }
        if t.is_null() {
            write_tm(out, TM_INVALID);
            set_errno(EINVAL);
            return EINVAL;
        }
        // SAFETY: pointeur non nul vers un `__time64_t`.
        match gmtime_checked(unsafe { t.read_unaligned() }) {
            Ok(tm) => {
                write_tm(out, tm);
                0
            }
            Err(e) => {
                write_tm(out, TM_INVALID);
                set_errno(e);
                e
            }
        }
    }
    "_localtime64" => fn t_localtime64(t: *const i64) -> *mut Tm [core::ptr::null_mut()] {
        if t.is_null() {
            set_errno(EINVAL);
            return core::ptr::null_mut();
        }
        // SAFETY: pointeur non nul vers un `__time64_t`.
        match localtime_checked(unsafe { t.read_unaligned() }) {
            Ok(tm) => {
                let b = tm_buf();
                write_tm(b, tm);
                b
            }
            Err(e) => {
                set_errno(e);
                core::ptr::null_mut()
            }
        }
    }
    "_localtime64_s" => fn t_localtime64_s(out: *mut Tm, t: *const i64) -> i32 [EINVAL] {
        if out.is_null() {
            set_errno(EINVAL);
            return EINVAL;
        }
        if t.is_null() {
            write_tm(out, TM_INVALID);
            set_errno(EINVAL);
            return EINVAL;
        }
        // SAFETY: pointeur non nul vers un `__time64_t`.
        match localtime_checked(unsafe { t.read_unaligned() }) {
            Ok(tm) => {
                write_tm(out, tm);
                0
            }
            Err(e) => {
                write_tm(out, TM_INVALID);
                set_errno(e);
                e
            }
        }
    }
    "_mktime64" => fn t_mktime64(p: *mut Tm) -> i64 [-1] {
        let Some(mut tm) = read_tm(p) else {
            set_errno(EINVAL);
            return -1;
        };
        let t = mktime_local(&mut tm);
        if t != -1 {
            write_tm(p, tm);
        }
        t
    }
    "_mkgmtime64" => fn t_mkgmtime64(p: *mut Tm) -> i64 [-1] {
        let Some(tm) = read_tm(p) else {
            set_errno(EINVAL);
            return -1;
        };
        let t = utc_from_tm(&tm);
        if !(0..=MAX_TIME64).contains(&t) {
            set_errno(EINVAL);
            return -1;
        }
        write_tm(p, tm_from_utc(t));
        t
    }
    "_tzset" => fn t_tzset() -> () [()] {
        let _ = tz_info();
    }
    "_get_timezone" => fn t_get_timezone(out: *mut i32) -> i32 [EINVAL] {
        if out.is_null() {
            set_errno(EINVAL);
            return EINVAL;
        }
        // SAFETY: pointeur de sortie non nul (`long` MSVC = 32 bits).
        unsafe { out.write_unaligned(tz_info().timezone) };
        0
    }
    "_get_daylight" => fn t_get_daylight(out: *mut i32) -> i32 [EINVAL] {
        if out.is_null() {
            set_errno(EINVAL);
            return EINVAL;
        }
        // SAFETY: pointeur de sortie non nul.
        unsafe { out.write_unaligned(tz_info().daylight) };
        0
    }
    "_get_dstbias" => fn t_get_dstbias(out: *mut i32) -> i32 [EINVAL] {
        if out.is_null() {
            set_errno(EINVAL);
            return EINVAL;
        }
        // SAFETY: pointeur de sortie non nul.
        unsafe { out.write_unaligned(tz_info().dstbias) };
        0
    }
    "_get_tzname" => fn t_get_tzname(ret: *mut usize, buf: *mut u8, size: usize, index: i32) -> i32 [EINVAL] {
        if buf.is_null() != (size == 0) {
            set_errno(EINVAL);
            return EINVAL;
        }
        if !buf.is_null() {
            // SAFETY: `buf` non nul avec `size > 0`.
            unsafe { *buf = 0 };
        }
        if ret.is_null() || !(0..=1).contains(&index) {
            set_errno(EINVAL);
            return EINVAL;
        }
        let name = tz_info().names[index as usize].clone();
        let need = name.len() + 1;
        // SAFETY: pointeur de sortie non nul.
        unsafe { ret.write_unaligned(need) };
        if buf.is_null() {
            return 0;
        }
        if need > size {
            set_errno(ERANGE);
            return ERANGE;
        }
        // SAFETY: `buf` dispose de `size >= need` octets.
        unsafe {
            core::ptr::copy_nonoverlapping(name.as_ptr(), buf, name.len());
            *buf.add(name.len()) = 0;
        }
        0
    }
    "strftime" => fn t_strftime(buf: *mut u8, max: usize, fmt: *const u8, tm: *const Tm) -> usize [0] {
        if buf.is_null() || max == 0 || fmt.is_null() {
            set_errno(EINVAL);
            return 0;
        }
        // SAFETY: `buf` non nul, `max > 0`.
        unsafe { *buf = 0 };
        let f = cstr_units(fmt);
        let Some(t) = read_tm(tm) else {
            set_errno(EINVAL);
            return 0;
        };
        match strftime_units(&f, &t) {
            Ok(units) => {
                let bytes: Vec<u8> = units.iter().map(|&u| u8::try_from(u).unwrap_or(b'?')).collect();
                write_strz(buf, max, &bytes)
            }
            Err(e) => {
                set_errno(e);
                0
            }
        }
    }
    "wcsftime" => fn t_wcsftime(buf: *mut u16, max: usize, fmt: *const u16, tm: *const Tm) -> usize [0] {
        if buf.is_null() || max == 0 || fmt.is_null() {
            set_errno(EINVAL);
            return 0;
        }
        // SAFETY: `buf` non nul, `max > 0`.
        unsafe { *buf = 0 };
        let f = wstr_units(fmt);
        let Some(t) = read_tm(tm) else {
            set_errno(EINVAL);
            return 0;
        };
        match strftime_units(&f, &t) {
            Ok(units) => write_strz(buf, max, &units),
            Err(e) => {
                set_errno(e);
                0
            }
        }
    }
}

fn fill_timeb(tb: *mut Timeb64) {
    let (t, ms) = now_unix();
    let tz = tz_info();
    let dst = host_localtime(t).is_some_and(|(tm, _, _)| tm.tm_isdst > 0);
    let v = Timeb64 {
        time: t,
        millitm: ms as u16,
        timezone: (tz.timezone / 60) as i16,
        dstflag: i16::from(dst),
    };
    // SAFETY: pointeur non nul (vérifié par l'appelant) vers une `__timeb64`.
    unsafe { tb.write_unaligned(v) };
}

// =====================================================================================
// Locale « C » (`api-ms-win-crt-locale`)
// =====================================================================================

/// `struct lconv` MSVC.
#[repr(C)]
pub(crate) struct Lconv {
    pub decimal_point: *const c_char,
    pub thousands_sep: *const c_char,
    pub grouping: *const c_char,
    pub int_curr_symbol: *const c_char,
    pub currency_symbol: *const c_char,
    pub mon_decimal_point: *const c_char,
    pub mon_thousands_sep: *const c_char,
    pub mon_grouping: *const c_char,
    pub positive_sign: *const c_char,
    pub negative_sign: *const c_char,
    pub int_frac_digits: c_char,
    pub frac_digits: c_char,
    pub p_cs_precedes: c_char,
    pub p_sep_by_space: c_char,
    pub n_cs_precedes: c_char,
    pub n_sep_by_space: c_char,
    pub p_sign_posn: c_char,
    pub n_sign_posn: c_char,
    pub w_decimal_point: *const u16,
    pub w_thousands_sep: *const u16,
    pub w_int_curr_symbol: *const u16,
    pub w_currency_symbol: *const u16,
    pub w_mon_decimal_point: *const u16,
    pub w_mon_thousands_sep: *const u16,
    pub w_positive_sign: *const u16,
    pub w_negative_sign: *const u16,
}
const _: () = assert!(size_of::<Lconv>() == 152);
const _: () = assert!(core::mem::offset_of!(Lconv, int_frac_digits) == 80);
const _: () = assert!(core::mem::offset_of!(Lconv, w_decimal_point) == 88);

struct SyncLconv(Lconv);
// SAFETY: la structure ne référence que des littéraux statiques immuables.
unsafe impl Sync for SyncLconv {}

const C_DOT: &[u8; 2] = b".\0";
const C_EMPTY: &[u8; 1] = b"\0";
const W_DOT: &[u16; 2] = &[b'.' as u16, 0];
const W_EMPTY: &[u16; 1] = &[0];
static C_LOCALE_NAME: &[u8; 2] = b"C\0";
static W_LOCALE_NAME: [u16; 2] = [b'C' as u16, 0];

static C_LCONV: SyncLconv = SyncLconv(Lconv {
    decimal_point: C_DOT.as_ptr().cast(),
    thousands_sep: C_EMPTY.as_ptr().cast(),
    grouping: C_EMPTY.as_ptr().cast(),
    int_curr_symbol: C_EMPTY.as_ptr().cast(),
    currency_symbol: C_EMPTY.as_ptr().cast(),
    mon_decimal_point: C_EMPTY.as_ptr().cast(),
    mon_thousands_sep: C_EMPTY.as_ptr().cast(),
    mon_grouping: C_EMPTY.as_ptr().cast(),
    positive_sign: C_EMPTY.as_ptr().cast(),
    negative_sign: C_EMPTY.as_ptr().cast(),
    int_frac_digits: 127,
    frac_digits: 127,
    p_cs_precedes: 127,
    p_sep_by_space: 127,
    n_cs_precedes: 127,
    n_sep_by_space: 127,
    p_sign_posn: 127,
    n_sign_posn: 127,
    w_decimal_point: W_DOT.as_ptr(),
    w_thousands_sep: W_EMPTY.as_ptr(),
    w_int_curr_symbol: W_EMPTY.as_ptr(),
    w_currency_symbol: W_EMPTY.as_ptr(),
    w_mon_decimal_point: W_EMPTY.as_ptr(),
    w_mon_thousands_sep: W_EMPTY.as_ptr(),
    w_positive_sign: W_EMPTY.as_ptr(),
    w_negative_sign: W_EMPTY.as_ptr(),
});

const LC_MAX: i32 = 5;

fn locale_name_ok(units: &[u16]) -> bool {
    units.is_empty() || units == [u16::from(b'C')]
}

exports_table! { register_locale;
    "localeconv" => fn l_localeconv() -> *const Lconv [core::ptr::null()] {
        &raw const C_LCONV.0
    }
    "setlocale" => fn l_setlocale(category: i32, locale: *const u8) -> *const u8 [core::ptr::null()] {
        if !(0..=LC_MAX).contains(&category) {
            set_errno(EINVAL);
            return core::ptr::null();
        }
        if locale.is_null() || locale_name_ok(&cstr_units(locale)) {
            C_LOCALE_NAME.as_ptr()
        } else {
            core::ptr::null()
        }
    }
    "_wsetlocale" => fn l_wsetlocale(category: i32, locale: *const u16) -> *const u16 [core::ptr::null()] {
        if !(0..=LC_MAX).contains(&category) {
            set_errno(EINVAL);
            return core::ptr::null();
        }
        if locale.is_null() || locale_name_ok(&wstr_units(locale)) {
            W_LOCALE_NAME.as_ptr()
        } else {
            core::ptr::null()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crt_str::get_errno;

    #[test]
    fn fp_classes_match_msvc() {
        let v = [1.0f64, 0.0, f64::INFINITY, f64::NAN, f64::from_bits(1)];
        let expect = [FP_NORMAL, FP_ZERO, FP_INFINITE, FP_NAN, FP_SUBNORMAL];
        for (x, e) in v.iter().zip(expect) {
            assert_eq!(m_dtest(x), e);
            assert_eq!(m_ldtest(x), e);
            assert_eq!(m_dclass(*x), e);
        }
        assert_eq!(FP_NORMAL, -1);
        let f = f32::from_bits(1);
        assert_eq!(m_fdtest(&f), FP_SUBNORMAL);
        assert_eq!(m_fdclass(-0.0), FP_ZERO);
        assert_eq!(m_dsign(-0.0), 0x8000);
        assert_eq!(m_fdsign(1.0), 0);
        assert_eq!(m_fpclass(-0.0), FPCLASS_NZ);
        assert_eq!(m_fpclass(f64::NEG_INFINITY), FPCLASS_NINF);
        assert_eq!(
            m_fpclass(f64::from_bits(0x7FF0_0000_0000_0001)),
            FPCLASS_SNAN
        );
        assert_eq!(m_fpclassf(f32::NAN), FPCLASS_QNAN);
        assert_eq!(m_fpclassf(-1e-40), FPCLASS_ND);
        assert_eq!(m_finite(1.0), 1);
        assert_eq!(m_isnan(f64::NAN), 1);
    }

    #[test]
    fn errno_domain_and_range() {
        set_errno(0);
        assert!(m_acos(2.0).is_nan());
        assert_eq!(get_errno(), EDOM);
        set_errno(0);
        assert_eq!(m_log(0.0), f64::NEG_INFINITY);
        assert_eq!(get_errno(), ERANGE);
        set_errno(0);
        assert!(m_log10f(-1.0).is_nan());
        assert_eq!(get_errno(), EDOM);
        set_errno(0);
        assert_eq!(m_exp(1000.0), f64::INFINITY);
        assert_eq!(get_errno(), ERANGE);
        set_errno(0);
        assert!(m_fmod(1.0, 0.0).is_nan());
        assert_eq!(get_errno(), EDOM);
        set_errno(0);
        assert_eq!(m_log1p(-1.0), f64::NEG_INFINITY);
        assert_eq!(get_errno(), ERANGE);
        set_errno(0);
        assert!(m_pow(-8.0, 0.5).is_nan());
        assert_eq!(get_errno(), EDOM);
        set_errno(0);
        assert_eq!(m_coshf(1000.0), f32::INFINITY);
        assert_eq!(get_errno(), ERANGE);
        set_errno(0);
        assert_eq!(m_sqrt(4.0), 2.0);
        assert_eq!(get_errno(), 0);
    }

    #[test]
    fn rounding_and_manipulation() {
        assert_eq!(m_lround(2.5), 3);
        assert_eq!(m_lround(-2.5), -3);
        assert_eq!(m_lrint(2.5), 2);
        assert_eq!(m_llroundf(1e10), 10_000_000_000);
        set_errno(0);
        assert_eq!(m_lround(1e20), i32::MIN);
        assert_eq!(get_errno(), EDOM);
        assert_eq!(m_rint(3.5), 4.0);
        assert_eq!(m_rintf(2.5), 2.0);
        let mut ip = 0.0;
        assert_eq!(m_modf(-3.25, &mut ip), -0.25);
        assert_eq!(ip, -3.0);
        let mut e = 0;
        assert_eq!(m_frexp(8.0, &mut e), 0.5);
        assert_eq!(e, 4);
        assert_eq!(m_frexp(f64::from_bits(1), &mut e), 0.5);
        assert_eq!(e, -1073);
        assert_eq!(m_scalbn(1.0, 10), 1024.0);
        assert_eq!(m_scalbn(1.0, -1074), f64::from_bits(1));
        set_errno(0);
        assert_eq!(m_scalbn(1.0, 5000), f64::INFINITY);
        assert_eq!(get_errno(), ERANGE);
        assert_eq!(m_ilogb(0.0), FP_ILOGB0);
        assert_eq!(m_ilogb(8.0), 3);
        assert_eq!(m_logb(0.75), -1.0);
        assert_eq!(m_remainder(5.0, 2.0), 1.0);
        assert_eq!(m_remainder(7.0, 2.0), -1.0);
        assert_eq!(m_remainderf(-5.5, 2.0), 0.5);
        assert_eq!(m_nextafter(1.0, 2.0), 1.0 + f64::EPSILON);
        assert_eq!(m_nextafterf(0.0, -1.0), -f32::from_bits(1));
        assert_eq!(m_nexttowardf(1.0, 0.0), 1.0 - f32::EPSILON / 2.0);
        assert_eq!(m_fdim(1.0, 3.0), 0.0);
        assert_eq!(m_fmax(f64::NAN, 1.0), 1.0);
        assert_eq!(m_fma(2.0, 3.0, 1.0), 7.0);
        assert_eq!(m_copysign(2.0, -0.0), -2.0);
        assert!((m_erf(1.0) - 0.842_700_792_949_714_9).abs() < 1e-15);
        assert!((m_erfcf(1.0) - 0.157_299_2).abs() < 1e-6);
        assert!((m_cbrt(27.0) - 3.0).abs() < 1e-15);
        assert!((m_hypot(3.0, 4.0) - 5.0).abs() < 1e-15);
    }

    #[test]
    fn gmtime_mkgmtime_roundtrip() {
        let t: i64 = 1_700_000_000; // 2023-11-14 22:13:20 UTC (mardi)
        let p = t_gmtime64(&t);
        assert!(!p.is_null());
        // SAFETY: tampon thread-local valide.
        let tm = unsafe { *p };
        assert_eq!(
            (
                tm.tm_year, tm.tm_mon, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec
            ),
            (123, 10, 14, 22, 13, 20)
        );
        assert_eq!(tm.tm_wday, 2);
        assert_eq!(tm.tm_yday, 317);
        let mut tm2 = tm;
        assert_eq!(t_mkgmtime64(&mut tm2), t);
        // Normalisation : 32 janvier -> 1er février.
        let mut n = Tm {
            tm_mday: 32,
            tm_year: 100,
            ..Tm::default()
        };
        let tn = t_mkgmtime64(&mut n);
        assert_eq!((n.tm_mon, n.tm_mday, n.tm_wday), (1, 1, 2));
        assert_eq!(tn, 949_363_200);
        // Bornes.
        let neg: i64 = -100_000;
        set_errno(0);
        assert!(t_gmtime64(&neg).is_null());
        assert_eq!(get_errno(), EINVAL);
        let mut out = Tm::default();
        assert_eq!(t_gmtime64_s(&mut out, &neg), EINVAL);
        assert_eq!(out, TM_INVALID);
        assert_eq!(t_gmtime64_s(&mut out, &t), 0);
        assert_eq!(out, tm);
    }

    #[test]
    fn mktime_localtime_roundtrip() {
        let t: i64 = 1_600_000_000;
        let mut lt = Tm::default();
        assert_eq!(t_localtime64_s(&mut lt, &t), 0);
        let mut copy = lt;
        assert_eq!(t_mktime64(&mut copy), t);
        assert_eq!(copy, lt);
        assert!(!t_localtime64(&t).is_null());
        let mut now = 0i64;
        let r = t_time64(&mut now);
        assert_eq!(r, now);
        assert!(r > 1_600_000_000);
        assert_eq!(t_difftime64(10, 4), 6.0);
        assert!(t_clock() >= 0);
        let mut tb = Timeb64::default();
        t_ftime64(&mut tb);
        assert!(tb.time > 0 && tb.millitm < 1000);
        let mut tz = 0;
        assert_eq!(t_get_timezone(&mut tz), 0);
        let mut dl = -1;
        assert_eq!(t_get_daylight(&mut dl), 0);
        assert!(dl == 0 || dl == 1);
        let mut need = 0usize;
        assert_eq!(t_get_tzname(&mut need, core::ptr::null_mut(), 0, 0), 0);
        assert!(need >= 2);
        let mut buf = [0u8; 64];
        assert_eq!(t_get_tzname(&mut need, buf.as_mut_ptr(), 64, 1), 0);
        assert!(!buf.contains(&b'/'));
        assert_eq!(t_get_tzname(&mut need, buf.as_mut_ptr(), 1, 0), ERANGE);
        assert_eq!(t_get_tzname(&mut need, buf.as_mut_ptr(), 64, 2), EINVAL);
    }

    #[test]
    fn strftime_formats() {
        let t: i64 = 1_700_000_000;
        let tm = tm_from_utc(t);
        let mut buf = [0u8; 128];
        let fmt = b"%Y-%m-%d %H:%M:%S %a %B %j %p %I %y %%\0";
        let n = t_strftime(buf.as_mut_ptr(), buf.len(), fmt.as_ptr(), &tm);
        assert_eq!(
            &buf[..n],
            b"2023-11-14 22:13:20 Tue November 318 PM 10 23 %"
        );
        let n = t_strftime(
            buf.as_mut_ptr(),
            buf.len(),
            c"%c|%x|%#d|%e|%G-W%V-%u|%U|%W".as_ptr().cast(),
            &tm,
        );
        assert_eq!(
            &buf[..n],
            b"11/14/23 22:13:20|11/14/23|14|14|2023-W46-2|46|46"
        );
        // Tampon trop petit.
        set_errno(0);
        assert_eq!(
            t_strftime(buf.as_mut_ptr(), 4, c"%Y-%m".as_ptr().cast(), &tm),
            0
        );
        assert_eq!(buf[0], 0);
        assert_eq!(get_errno(), ERANGE);
        // Spécificateur invalide / champ hors bornes.
        set_errno(0);
        assert_eq!(
            t_strftime(buf.as_mut_ptr(), 64, c"%Q".as_ptr().cast(), &tm),
            0
        );
        assert_eq!(get_errno(), EINVAL);
        let bad = Tm { tm_mon: 12, ..tm };
        assert_eq!(
            t_strftime(buf.as_mut_ptr(), 64, c"%b".as_ptr().cast(), &bad),
            0
        );
        // Large.
        let wfmt: Vec<u16> = "%A %d\0".encode_utf16().collect();
        let mut wbuf = [0u16; 32];
        let n = t_wcsftime(wbuf.as_mut_ptr(), 32, wfmt.as_ptr(), &tm);
        assert_eq!(String::from_utf16_lossy(&wbuf[..n]), "Tuesday 14");
        // Semaine ISO en début d'année : 2021-01-01 (vendredi) = 2020-W53.
        let jan = tm_from_utc(1_609_459_200);
        let n = t_strftime(buf.as_mut_ptr(), 64, c"%G-%V".as_ptr().cast(), &jan);
        assert_eq!(&buf[..n], b"2020-53");
        // 2024-12-30 (lundi) = 2025-W01.
        let dec = tm_from_utc(1_735_516_800);
        let n = t_strftime(buf.as_mut_ptr(), 64, c"%G-%V".as_ptr().cast(), &dec);
        assert_eq!(&buf[..n], b"2025-01");
    }

    #[test]
    fn locale_c_only() {
        let l = l_localeconv();
        // SAFETY: pointeur statique valide.
        let lc = unsafe { &*l };
        // SAFETY: chaînes statiques terminées par 0.
        assert_eq!(unsafe { *lc.decimal_point }, b'.' as c_char);
        assert_eq!(lc.frac_digits, 127);
        // SAFETY: idem.
        assert_eq!(unsafe { *lc.w_decimal_point }, u16::from(b'.'));
        let r = l_setlocale(0, core::ptr::null());
        // SAFETY: chaîne statique.
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(r.cast()) }.to_bytes(),
            b"C"
        );
        assert!(!l_setlocale(0, c"C".as_ptr().cast()).is_null());
        assert!(!l_setlocale(2, c"".as_ptr().cast()).is_null());
        assert!(l_setlocale(0, c"fr-FR".as_ptr().cast()).is_null());
        set_errno(0);
        assert!(l_setlocale(9, core::ptr::null()).is_null());
        assert_eq!(get_errno(), EINVAL);
        let w: Vec<u16> = "C\0".encode_utf16().collect();
        assert!(!l_wsetlocale(0, w.as_ptr()).is_null());
    }
}
