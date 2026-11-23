//! Extension HLE CRT : chaînes, conversions, utilitaires et `errno` (`api-ms-win-crt-*`).
//!
//! Sémantique alignée sur l'UCRT Microsoft en locale "C" :
//! - `long` vaut 32 bits (LLP64), `wchar_t`/`wint_t` 16 bits ;
//! - les fonctions `_s` appliquent le comportement du handler de paramètre invalide qui
//!   retourne (sortie vidée, `errno` positionné, code `EINVAL`/`ERANGE` retourné) ;
//! - les conversions saturent avec `ERANGE` et renseignent `endptr` comme l'UCRT.

#![allow(non_snake_case, unused_variables)]

use std::cell::{Cell, UnsafeCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;

use sb_common::ffi_guard;

const CHANNEL: &str = "crt";

/// Codes `errno` MSVC usuels.
pub(crate) const EINVAL: i32 = 22;
pub(crate) const ERANGE: i32 = 34;
/// Code de retour des fonctions `_s` en mode `_TRUNCATE`.
const STRUNCATE: i32 = 80;
/// Valeur spéciale `_TRUNCATE` des fonctions `strncpy_s`/`strncat_s`.
const TRUNCATE: usize = usize::MAX;
/// Valeur d'erreur des comparaisons insensibles à la casse (`_NLSCMPERROR`).
const NLSCMPERROR: i32 = 0x7FFF_FFFF;

thread_local! {
    static ERRNO: UnsafeCell<i32> = const { UnsafeCell::new(0) };
    static STRTOK_CTX: Cell<*mut u8> = const { Cell::new(ptr::null_mut()) };
    static WCSTOK_CTX: Cell<*mut u16> = const { Cell::new(ptr::null_mut()) };
}

/// Pointeur vers l'`errno` du thread courant (stable pour la durée de vie du thread).
pub(crate) fn errno_ptr() -> *mut i32 {
    ERRNO.with(|e| e.get())
}

/// Positionne l'`errno` du thread courant.
pub(crate) fn set_errno(v: i32) {
    // SAFETY: pointeur thread-local valide, accès mono-thread.
    unsafe { *errno_ptr() = v };
}

/// Lit l'`errno` du thread courant.
pub(crate) fn get_errno() -> i32 {
    // SAFETY: pointeur thread-local valide, accès mono-thread.
    unsafe { *errno_ptr() }
}

/// Paramètre invalide : positionne `errno` et renvoie le même code (handler qui retourne).
fn invalid(code: i32) -> i32 {
    set_errno(code);
    code
}

/// Exports `nom -> adresse` fusionnés dans la famille CRT du résolveur.
pub fn exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("_errno", crt_errno);
    reg!("_get_errno", crt_get_errno);
    reg!("_set_errno", crt_set_errno);

    // ---- api-ms-win-crt-convert ----
    reg!("strtol", crt_strtol);
    reg!("strtoul", crt_strtoul);
    reg!("strtoll", crt_strtoll);
    reg!("strtoull", crt_strtoull);
    reg!("_strtoi64", crt_strtoll);
    reg!("_strtoui64", crt_strtoull);
    reg!("wcstol", crt_wcstol);
    reg!("wcstoul", crt_wcstoul);
    reg!("wcstoll", crt_wcstoll);
    reg!("wcstoull", crt_wcstoull);
    reg!("_wcstoi64", crt_wcstoll);
    reg!("_wcstoui64", crt_wcstoull);
    reg!("atol", crt_atol);
    reg!("atoll", crt_atoll);
    reg!("_atoi64", crt_atoll);
    reg!("_wtoi", crt_wtoi);
    reg!("_wtol", crt_wtoi);
    reg!("_wtoi64", crt_wtoi64);
    reg!("_wtoll", crt_wtoi64);
    reg!("strtod", crt_strtod);
    reg!("strtof", crt_strtof);
    reg!("atof", crt_atof);
    reg!("wcstod", crt_wcstod);
    reg!("wcstof", crt_wcstof);
    reg!("_wtof", crt_wtof);
    reg!("_itoa", crt_itoa);
    reg!("_ltoa", crt_itoa);
    reg!("_ultoa", crt_ultoa);
    reg!("_i64toa", crt_i64toa);
    reg!("_ui64toa", crt_ui64toa);
    reg!("_itow", crt_itow);
    reg!("_ltow", crt_itow);
    reg!("_ultow", crt_ultow);
    reg!("_i64tow", crt_i64tow);
    reg!("_ui64tow", crt_ui64tow);
    reg!("_itoa_s", crt_itoa_s);
    reg!("_ltoa_s", crt_itoa_s);
    reg!("_ultoa_s", crt_ultoa_s);
    reg!("_i64toa_s", crt_i64toa_s);
    reg!("_ui64toa_s", crt_ui64toa_s);
    reg!("_itow_s", crt_itow_s);
    reg!("_i64tow_s", crt_i64tow_s);
    reg!("_ultow_s", crt_ultow_s);
    reg!("_ltow_s", crt_itow_s);
    reg!("_ui64tow_s", crt_ui64tow_s);

    // ---- api-ms-win-crt-string : ctype ----
    reg!("isalnum", crt_isalnum);
    reg!("isalpha", crt_isalpha);
    reg!("isdigit", crt_isdigit);
    reg!("islower", crt_islower);
    reg!("isupper", crt_isupper);
    reg!("isspace", crt_isspace);
    reg!("isxdigit", crt_isxdigit);
    reg!("isprint", crt_isprint);
    reg!("ispunct", crt_ispunct);
    reg!("iscntrl", crt_iscntrl);
    reg!("isgraph", crt_isgraph);
    reg!("isblank", crt_isblank);
    reg!("tolower", crt_tolower);
    reg!("toupper", crt_toupper);
    reg!("_tolower", crt_tolower_raw);
    reg!("_toupper", crt_toupper_raw);
    reg!("towlower", crt_towlower);
    reg!("towupper", crt_towupper);
    reg!("iswspace", crt_iswspace);
    reg!("iswdigit", crt_iswdigit);
    reg!("iswalpha", crt_iswalpha);
    reg!("iswalnum", crt_iswalnum);
    reg!("iswupper", crt_iswupper);
    reg!("iswlower", crt_iswlower);
    reg!("iswxdigit", crt_iswxdigit);
    reg!("iswpunct", crt_iswpunct);
    reg!("iswprint", crt_iswprint);
    reg!("iswcntrl", crt_iswcntrl);
    reg!("iswgraph", crt_iswgraph);

    // ---- api-ms-win-crt-string : chaînes étroites ----
    reg!("strlen", crt_strlen);
    reg!("strnlen", crt_strnlen);
    reg!("strcmp", crt_strcmp);
    reg!("strcoll", crt_strcmp);
    reg!("_stricmp", crt_stricmp);
    reg!("_strcmpi", crt_stricmp);
    reg!("_stricoll", crt_stricmp);
    reg!("_strnicmp", crt_strnicmp);
    reg!("strcpy", crt_strcpy);
    reg!("strcat", crt_strcat);
    reg!("strncpy", crt_strncpy);
    reg!("strncat", crt_strncat);
    reg!("strcpy_s", crt_strcpy_s);
    reg!("strcat_s", crt_strcat_s);
    reg!("strncpy_s", crt_strncpy_s);
    reg!("strncat_s", crt_strncat_s);
    reg!("strchr", crt_strchr);
    reg!("strrchr", crt_strrchr);
    reg!("strpbrk", crt_strpbrk);
    reg!("strspn", crt_strspn);
    reg!("strcspn", crt_strcspn);
    reg!("strtok", crt_strtok);
    reg!("strtok_s", crt_strtok_s);
    reg!("_strdup", crt_strdup);
    reg!("memchr", crt_memchr);

    // ---- api-ms-win-crt-string : chaînes larges ----
    reg!("wcsnlen", crt_wcsnlen);
    reg!("wcscmp", crt_wcscmp);
    reg!("wcscoll", crt_wcscmp);
    reg!("wcsncmp", crt_wcsncmp);
    reg!("_wcsicmp", crt_wcsicmp);
    reg!("_wcsicoll", crt_wcsicmp);
    reg!("_wcsnicmp", crt_wcsnicmp);
    reg!("wcscpy", crt_wcscpy);
    reg!("wcscat", crt_wcscat);
    reg!("wcsncpy", crt_wcsncpy);
    reg!("wcsncat", crt_wcsncat);
    reg!("wcscpy_s", crt_wcscpy_s);
    reg!("wcscat_s", crt_wcscat_s);
    reg!("wcsncpy_s", crt_wcsncpy_s);
    reg!("wcsncat_s", crt_wcsncat_s);
    reg!("wcschr", crt_wcschr);
    reg!("wcsrchr", crt_wcsrchr);
    reg!("wcspbrk", crt_wcspbrk);
    reg!("wcsspn", crt_wcsspn);
    reg!("wcscspn", crt_wcscspn);
    reg!("wcstok", crt_wcstok);
    reg!("wcstok_s", crt_wcstok);
    reg!("_wcsdup", crt_wcsdup);

    // ---- api-ms-win-crt-utility ----
    reg!("bsearch", crt_bsearch);
    reg!("bsearch_s", crt_bsearch_s);
    reg!("qsort_s", crt_qsort_s);
    reg!("abs", crt_abs);
    reg!("labs", crt_abs);
    reg!("llabs", crt_llabs);
    reg!("_abs64", crt_llabs);
    reg!("div", crt_div);
    reg!("ldiv", crt_div);
    reg!("lldiv", crt_lldiv);
    m
}

// ===================================================================================
// errno
// ===================================================================================

extern "win64" fn crt_errno() -> *mut i32 {
    ffi_guard!(CHANNEL, "_errno", std::ptr::null_mut(), { errno_ptr() })
}

extern "win64" fn crt_get_errno(out: *mut i32) -> i32 {
    ffi_guard!(CHANNEL, "_get_errno", EINVAL, {
        if out.is_null() {
            return EINVAL;
        }
        // SAFETY: `out` vérifié non-nul, fourni par l'appelant invité.
        unsafe { *out = get_errno() };
        0
    })
}

extern "win64" fn crt_set_errno(v: i32) -> i32 {
    ffi_guard!(CHANNEL, "_set_errno", EINVAL, {
        set_errno(v);
        0
    })
}

// ===================================================================================
// Primitives génériques (char / wchar_t)
// ===================================================================================

/// Unité de caractère CRT : `char` (u8) ou `wchar_t` (u16).
trait Ch: Copy + Eq + 'static {
    const NUL: Self;
    const WIDE: bool;
    fn code(self) -> u32;
    fn from_code(c: u32) -> Self;
}

impl Ch for u8 {
    const NUL: Self = 0;
    const WIDE: bool = false;
    fn code(self) -> u32 {
        u32::from(self)
    }
    fn from_code(c: u32) -> Self {
        c as u8
    }
}

impl Ch for u16 {
    const NUL: Self = 0;
    const WIDE: bool = true;
    fn code(self) -> u32 {
        u32::from(self)
    }
    fn from_code(c: u32) -> Self {
        c as u16
    }
}

/// Longueur d'une chaîne terminée par NUL.
///
/// # Safety
/// `p` non-nul et terminé par `NUL`.
unsafe fn slen<C: Ch>(p: *const C) -> usize {
    let mut n = 0;
    // SAFETY: contrat de l'appelant, lecture jusqu'au NUL inclus.
    while unsafe { *p.add(n) } != C::NUL {
        n += 1;
    }
    n
}

/// Longueur bornée (`strnlen`).
///
/// # Safety
/// `p` non-nul, lisible jusqu'au premier NUL ou `max` éléments.
unsafe fn snlen<C: Ch>(p: *const C, max: usize) -> usize {
    let mut n = 0;
    // SAFETY: lecture bornée par `max` et le NUL (contrat de l'appelant).
    while n < max && unsafe { *p.add(n) } != C::NUL {
        n += 1;
    }
    n
}

/// Indique si `c` appartient à l'ensemble `set` (chaîne terminée par NUL).
///
/// # Safety
/// `set` non-nul et terminé par NUL.
unsafe fn in_set<C: Ch>(set: *const C, c: C) -> bool {
    let mut i = 0;
    loop {
        // SAFETY: `set` terminé par NUL (contrat de l'appelant).
        let s = unsafe { *set.add(i) };
        if s == C::NUL {
            return false;
        }
        if s == c {
            return true;
        }
        i += 1;
    }
}

// ===================================================================================
// ctype (locale "C", masques identiques à la table `_ctype` MSVC)
// ===================================================================================

const C_UPPER: u16 = 0x1;
const C_LOWER: u16 = 0x2;
const C_DIGIT: u16 = 0x4;
const C_SPACE: u16 = 0x8;
const C_PUNCT: u16 = 0x10;
const C_CONTROL: u16 = 0x20;
const C_BLANK: u16 = 0x40;
const C_HEX: u16 = 0x80;
const C_ALPHA: u16 = 0x100 | C_UPPER | C_LOWER;

/// Masque `_ctype` d'un code ASCII (0 hors 0..=127 en locale "C").
fn ctype_ascii(c: u32) -> u16 {
    match c {
        0x09 => C_SPACE | C_CONTROL | C_BLANK,
        0x0A..=0x0D => C_SPACE | C_CONTROL,
        0x00..=0x1F | 0x7F => C_CONTROL,
        0x20 => C_SPACE | C_BLANK,
        0x30..=0x39 => C_DIGIT | C_HEX,
        0x41..=0x46 => C_UPPER | C_HEX | 0x100,
        0x47..=0x5A => C_UPPER | 0x100,
        0x61..=0x66 => C_LOWER | C_HEX | 0x100,
        0x67..=0x7A => C_LOWER | 0x100,
        0x21..=0x7E => C_PUNCT,
        _ => 0,
    }
}

/// Masque de classification large (`_wctype` pour ASCII, propriétés Unicode au-delà).
fn ctype_wide(c: u32) -> u16 {
    if c < 0x80 {
        return ctype_ascii(c);
    }
    let Some(ch) = char::from_u32(c) else {
        return 0;
    };
    let mut m = 0;
    if ch.is_whitespace() {
        m |= C_SPACE;
        if matches!(
            c,
            0xA0 | 0x1680 | 0x2000..=0x200A | 0x202F | 0x205F | 0x3000
        ) {
            m |= C_BLANK;
        }
    }
    if ch.is_control() {
        m |= C_CONTROL;
    }
    if ch.is_alphabetic() {
        m |= 0x100;
        if ch.is_uppercase() {
            m |= C_UPPER;
        } else if ch.is_lowercase() {
            m |= C_LOWER;
        }
    } else if !ch.is_whitespace() && !ch.is_control() && !ch.is_numeric() {
        m |= C_PUNCT;
    }
    m
}

macro_rules! ctype_fns {
    ($( $f:ident, $wf:ident, $name:literal, $wname:literal, $mask:expr; )*) => {$(
        extern "win64" fn $f(c: i32) -> i32 {
            ffi_guard!(CHANNEL, $name, 0, {
                if (0..=255).contains(&c) {
                    i32::from(ctype_ascii(c as u32) & ($mask))
                } else {
                    0
                }
            })
        }
        extern "win64" fn $wf(c: u16) -> i32 {
            ffi_guard!(CHANNEL, $wname, 0, {
                i32::from(ctype_wide(u32::from(c)) & ($mask))
            })
        }
    )*};
}

ctype_fns! {
    crt_isalnum, crt_iswalnum, "isalnum", "iswalnum", C_ALPHA | C_DIGIT;
    crt_isalpha, crt_iswalpha, "isalpha", "iswalpha", C_ALPHA;
    crt_isdigit, crt_iswdigit, "isdigit", "iswdigit", C_DIGIT;
    crt_islower, crt_iswlower, "islower", "iswlower", C_LOWER;
    crt_isupper, crt_iswupper, "isupper", "iswupper", C_UPPER;
    crt_isspace, crt_iswspace, "isspace", "iswspace", C_SPACE;
    crt_isxdigit, crt_iswxdigit, "isxdigit", "iswxdigit", C_HEX;
    crt_isprint, crt_iswprint, "isprint", "iswprint", C_BLANK | C_PUNCT | C_ALPHA | C_DIGIT;
    crt_ispunct, crt_iswpunct, "ispunct", "iswpunct", C_PUNCT;
    crt_iscntrl, crt_iswcntrl, "iscntrl", "iswcntrl", C_CONTROL;
    crt_isgraph, crt_iswgraph, "isgraph", "iswgraph", C_PUNCT | C_ALPHA | C_DIGIT;
}

extern "win64" fn crt_isblank(c: i32) -> i32 {
    ffi_guard!(CHANNEL, "isblank", 0, {
        // UCRT : `isblank` est vrai pour '\t' et ' ' uniquement en locale "C".
        if c == 0x09 || c == 0x20 {
            i32::from(C_BLANK)
        } else {
            0
        }
    })
}

fn ascii_lower(c: u32) -> u32 {
    if (0x41..=0x5A).contains(&c) {
        c + 0x20
    } else {
        c
    }
}

fn ascii_upper(c: u32) -> u32 {
    if (0x61..=0x7A).contains(&c) {
        c - 0x20
    } else {
        c
    }
}

extern "win64" fn crt_tolower(c: i32) -> i32 {
    ffi_guard!(CHANNEL, "tolower", c, {
        if (0x41..=0x5A).contains(&c) {
            c + 0x20
        } else {
            c
        }
    })
}

extern "win64" fn crt_toupper(c: i32) -> i32 {
    ffi_guard!(CHANNEL, "toupper", c, {
        if (0x61..=0x7A).contains(&c) {
            c - 0x20
        } else {
            c
        }
    })
}

/// `_tolower` : macro historique sans test de classe (`c - 'A' + 'a'`).
extern "win64" fn crt_tolower_raw(c: i32) -> i32 {
    ffi_guard!(CHANNEL, "_tolower", c, {
        c.wrapping_sub(0x41).wrapping_add(0x61)
    })
}

/// `_toupper` : macro historique sans test de classe (`c - 'a' + 'A'`).
extern "win64" fn crt_toupper_raw(c: i32) -> i32 {
    ffi_guard!(CHANNEL, "_toupper", c, {
        c.wrapping_sub(0x61).wrapping_add(0x41)
    })
}

extern "win64" fn crt_towlower(c: u16) -> u16 {
    ffi_guard!(CHANNEL, "towlower", c, { ascii_lower(u32::from(c)) as u16 })
}

extern "win64" fn crt_towupper(c: u16) -> u16 {
    ffi_guard!(CHANNEL, "towupper", c, { ascii_upper(u32::from(c)) as u16 })
}

/// Espace au sens `isspace`/`iswspace` (locale "C").
fn is_space<C: Ch>(c: u32) -> bool {
    if C::WIDE {
        ctype_wide(c) & C_SPACE != 0
    } else {
        ctype_ascii(c) & C_SPACE != 0
    }
}

// ===================================================================================
// Conversions entières (strtol & co)
// ===================================================================================

/// Résultat d'analyse d'un entier : `end == None` signifie « aucune conversion ».
struct IntParse {
    neg: bool,
    mag: u64,
    overflow: bool,
    end: Option<usize>,
}

fn digit_value(c: u32) -> u32 {
    match c {
        0x30..=0x39 => c - 0x30,
        0x41..=0x5A => c - 0x41 + 10,
        0x61..=0x7A => c - 0x61 + 10,
        _ => u32::MAX,
    }
}

/// Analyse un entier selon `parse_integer` de l'UCRT. `base` doit être 0 ou 2..=36.
///
/// # Safety
/// `s` non-nul et terminé par NUL.
unsafe fn parse_int<C: Ch>(s: *const C, base: u32) -> IntParse {
    // SAFETY: chaque index lu n'est atteint qu'après des caractères non-NUL (le NUL
    // n'est ni espace, ni signe, ni chiffre), donc reste dans la chaîne.
    let at = |i: usize| -> u32 { unsafe { (*s.add(i)).code() } };
    let mut i = 0;
    while is_space::<C>(at(i)) {
        i += 1;
    }
    let mut neg = false;
    match at(i) {
        0x2D => {
            neg = true;
            i += 1;
        }
        0x2B => i += 1,
        _ => {}
    }
    let mut base = base;
    if base == 0 || base == 16 {
        if at(i) == 0x30 {
            let n = at(i + 1);
            if n == 0x78 || n == 0x58 {
                base = 16;
                i += 2;
            } else if base == 0 {
                base = 8;
            }
        } else if base == 0 {
            base = 10;
        }
    }
    let start = i;
    let mut mag: u64 = 0;
    let mut overflow = false;
    loop {
        let d = digit_value(at(i));
        if d >= base {
            break;
        }
        match mag
            .checked_mul(u64::from(base))
            .and_then(|v| v.checked_add(u64::from(d)))
        {
            Some(v) => mag = v,
            None => overflow = true,
        }
        i += 1;
    }
    if i == start {
        // UCRT : aucun chiffre lu → état initial restauré (endptr = nptr), y compris "0x".
        return IntParse {
            neg: false,
            mag: 0,
            overflow: false,
            end: None,
        };
    }
    IntParse {
        neg,
        mag,
        overflow,
        end: Some(i),
    }
}

/// Valide l'entrée, analyse et renseigne `endptr`.
///
/// # Safety
/// `s` nul ou terminé par NUL ; `end` nul ou inscriptible.
unsafe fn strto_common<C: Ch>(s: *const C, end: *mut *const C, base: i32) -> Option<IntParse> {
    if !end.is_null() {
        // SAFETY: `end` non-nul fourni par l'appelant.
        unsafe { *end = s };
    }
    if s.is_null() || !(base == 0 || (2..=36).contains(&base)) {
        set_errno(EINVAL);
        return None;
    }
    // SAFETY: `s` non-nul et terminé par NUL (contrat).
    let p = unsafe { parse_int(s, base as u32) };
    if let (Some(e), false) = (p.end, end.is_null()) {
        // SAFETY: `e` est dans la chaîne analysée ; `end` non-nul.
        unsafe { *end = s.add(e) };
    }
    Some(p)
}

/// # Safety
/// Voir [`strto_common`].
unsafe fn strto_signed<C: Ch>(s: *const C, end: *mut *const C, base: i32, max: i64) -> i64 {
    // SAFETY: contrat transmis.
    let Some(p) = (unsafe { strto_common(s, end, base) }) else {
        return 0;
    };
    let limit = if p.neg { max as u64 + 1 } else { max as u64 };
    if p.overflow || p.mag > limit {
        set_errno(ERANGE);
        return if p.neg { -max - 1 } else { max };
    }
    if p.neg {
        0i64.wrapping_sub(p.mag as i64)
    } else {
        p.mag as i64
    }
}

/// # Safety
/// Voir [`strto_common`].
unsafe fn strto_unsigned<C: Ch>(s: *const C, end: *mut *const C, base: i32, max: u64) -> u64 {
    // SAFETY: contrat transmis.
    let Some(p) = (unsafe { strto_common(s, end, base) }) else {
        return 0;
    };
    if p.overflow || p.mag > max {
        set_errno(ERANGE);
        return max;
    }
    if p.neg {
        p.mag.wrapping_neg() & max
    } else {
        p.mag
    }
}

extern "win64" fn crt_strtol(s: *const u8, end: *mut *const u8, base: i32) -> i32 {
    ffi_guard!(CHANNEL, "strtol", 0, {
        // SAFETY: chaîne C et endptr fournis par l'invité.
        unsafe { strto_signed(s, end, base, i64::from(i32::MAX)) as i32 }
    })
}

extern "win64" fn crt_strtoul(s: *const u8, end: *mut *const u8, base: i32) -> u32 {
    ffi_guard!(CHANNEL, "strtoul", 0, {
        // SAFETY: chaîne C et endptr fournis par l'invité.
        unsafe { strto_unsigned(s, end, base, u64::from(u32::MAX)) as u32 }
    })
}

extern "win64" fn crt_strtoll(s: *const u8, end: *mut *const u8, base: i32) -> i64 {
    ffi_guard!(CHANNEL, "strtoll", 0, {
        // SAFETY: chaîne C et endptr fournis par l'invité.
        unsafe { strto_signed(s, end, base, i64::MAX) }
    })
}

extern "win64" fn crt_strtoull(s: *const u8, end: *mut *const u8, base: i32) -> u64 {
    ffi_guard!(CHANNEL, "strtoull", 0, {
        // SAFETY: chaîne C et endptr fournis par l'invité.
        unsafe { strto_unsigned(s, end, base, u64::MAX) }
    })
}

extern "win64" fn crt_wcstol(s: *const u16, end: *mut *const u16, base: i32) -> i32 {
    ffi_guard!(CHANNEL, "wcstol", 0, {
        // SAFETY: chaîne large et endptr fournis par l'invité.
        unsafe { strto_signed(s, end, base, i64::from(i32::MAX)) as i32 }
    })
}

extern "win64" fn crt_wcstoul(s: *const u16, end: *mut *const u16, base: i32) -> u32 {
    ffi_guard!(CHANNEL, "wcstoul", 0, {
        // SAFETY: chaîne large et endptr fournis par l'invité.
        unsafe { strto_unsigned(s, end, base, u64::from(u32::MAX)) as u32 }
    })
}

extern "win64" fn crt_wcstoll(s: *const u16, end: *mut *const u16, base: i32) -> i64 {
    ffi_guard!(CHANNEL, "wcstoll", 0, {
        // SAFETY: chaîne large et endptr fournis par l'invité.
        unsafe { strto_signed(s, end, base, i64::MAX) }
    })
}

extern "win64" fn crt_wcstoull(s: *const u16, end: *mut *const u16, base: i32) -> u64 {
    ffi_guard!(CHANNEL, "wcstoull", 0, {
        // SAFETY: chaîne large et endptr fournis par l'invité.
        unsafe { strto_unsigned(s, end, base, u64::MAX) }
    })
}

extern "win64" fn crt_atol(s: *const u8) -> i32 {
    crt_strtol(s, ptr::null_mut(), 10)
}

extern "win64" fn crt_atoll(s: *const u8) -> i64 {
    crt_strtoll(s, ptr::null_mut(), 10)
}

extern "win64" fn crt_wtoi(s: *const u16) -> i32 {
    crt_wcstol(s, ptr::null_mut(), 10)
}

extern "win64" fn crt_wtoi64(s: *const u16) -> i64 {
    crt_wcstoll(s, ptr::null_mut(), 10)
}

// ===================================================================================
// Conversions flottantes (strtod & co)
// ===================================================================================

struct FloatParse {
    value: f64,
    end: Option<usize>,
    erange: bool,
}

/// `x * 2^e` exact tant que le résultat est représentable (mantisse déjà arrondie).
fn ldexp(mut x: f64, mut e: i64) -> f64 {
    let p1000 = f64::from_bits((1023 + 1000) << 52);
    let n1000 = f64::from_bits((1023 - 1000) << 52);
    while e > 1000 {
        x *= p1000;
        e -= 1000;
        if x.is_infinite() {
            return x;
        }
    }
    while e < -1000 {
        x *= n1000;
        e += 1000;
        if x == 0.0 {
            return x;
        }
    }
    x * f64::from_bits(((1023 + e) as u64) << 52)
}

/// Convertit `m * 2^e2` (+ bits perdus `sticky`) en flottant correctement arrondi
/// (au plus proche, pair) pour la précision cible. Retourne (valeur, ERANGE).
fn hex_to_float(m: u64, sticky: bool, e2: i64, single: bool) -> (f64, bool) {
    if m == 0 {
        return (0.0, false);
    }
    let lz = i64::from(m.leading_zeros());
    let m = m << lz;
    // valeur = (m / 2^63) * 2^exp
    let exp = e2 - lz + 63;
    let (p, emin, emax): (i64, i64, i64) = if single {
        (24, -126, 127)
    } else {
        (53, -1022, 1023)
    };
    if exp > emax {
        return (f64::INFINITY, true);
    }
    let keep = if exp >= emin { p } else { p - (emin - exp) };
    if keep < 0 {
        return (0.0, true);
    }
    let shift = (64 - keep) as u32;
    let (mut kept, rem, half) = if shift == 64 {
        (0u64, m, 1u64 << 63)
    } else {
        (m >> shift, m & ((1u64 << shift) - 1), 1u64 << (shift - 1))
    };
    if rem > half || (rem == half && (sticky || kept & 1 == 1)) {
        kept += 1;
    }
    let v = ldexp(kept as f64, exp - keep + 1);
    let v = if single { f64::from(v as f32) } else { v };
    if v.is_infinite() || v == 0.0 {
        (v, true)
    } else {
        (v, false)
    }
}

/// Compare (insensible à la casse ASCII) le texte à la position `i` avec `word`.
fn match_ci(at: &dyn Fn(usize) -> u32, i: usize, word: &str) -> bool {
    word.bytes()
        .enumerate()
        .all(|(k, b)| ascii_lower(at(i + k)) == u32::from(b))
}

fn is_digit(c: u32) -> bool {
    (0x30..=0x39).contains(&c)
}

/// Lit un exposant décimal optionnel `[+-]digits` à partir de `j`.
/// Retourne (exposant saturé, index de fin) si au moins un chiffre est présent.
fn parse_exponent(at: &dyn Fn(usize) -> u32, j: usize) -> Option<(i64, usize)> {
    let mut k = j;
    let neg = match at(k) {
        0x2D => {
            k += 1;
            true
        }
        0x2B => {
            k += 1;
            false
        }
        _ => false,
    };
    let start = k;
    let mut e: i64 = 0;
    while is_digit(at(k)) {
        e = (e * 10 + i64::from(at(k) - 0x30)).min(1_000_000_000);
        k += 1;
    }
    if k == start {
        return None;
    }
    Some((if neg { -e } else { e }, k))
}

/// Analyse un flottant selon l'UCRT : décimal, hexadécimal (`0x1.8p3`), `inf`/`infinity`,
/// `nan`/`nan(...)`.
///
/// # Safety
/// `s` non-nul et terminé par NUL.
unsafe fn parse_float<C: Ch>(s: *const C, single: bool) -> FloatParse {
    // SAFETY: chaque index lu suit des caractères non-NUL (comparaisons arrêtées au
    // premier caractère non reconnu, dont le NUL).
    let at = |i: usize| -> u32 { unsafe { (*s.add(i)).code() } };
    let none = FloatParse {
        value: 0.0,
        end: None,
        erange: false,
    };
    let mut i = 0;
    while is_space::<C>(at(i)) {
        i += 1;
    }
    let neg = match at(i) {
        0x2D => {
            i += 1;
            true
        }
        0x2B => {
            i += 1;
            false
        }
        _ => false,
    };
    let sign = |v: f64| if neg { -v } else { v };

    if match_ci(&at, i, "inf") {
        let j = if match_ci(&at, i + 3, "inity") {
            i + 8
        } else {
            i + 3
        };
        return FloatParse {
            value: sign(f64::INFINITY),
            end: Some(j),
            erange: false,
        };
    }
    if match_ci(&at, i, "nan") {
        let mut j = i + 3;
        if at(j) == 0x28 {
            let mut k = j + 1;
            while ctype_ascii(at(k)) & (C_ALPHA | C_DIGIT) != 0 || at(k) == 0x5F {
                k += 1;
            }
            if at(k) == 0x29 {
                j = k + 1;
            }
        }
        return FloatParse {
            value: sign(f64::NAN),
            end: Some(j),
            erange: false,
        };
    }

    if at(i) == 0x30 && (at(i + 1) == 0x78 || at(i + 1) == 0x58) {
        let mut j = i + 2;
        let mut m: u64 = 0;
        let mut e2: i64 = 0;
        let mut sticky = false;
        let mut any = false;
        while ctype_ascii(at(j)) & C_HEX != 0 {
            let d = u64::from(digit_value(at(j)));
            any = true;
            if m >> 60 == 0 {
                m = (m << 4) | d;
            } else {
                e2 += 4;
                sticky |= d != 0;
            }
            j += 1;
        }
        if at(j) == 0x2E {
            let mut k = j + 1;
            let mut frac_any = false;
            while ctype_ascii(at(k)) & C_HEX != 0 {
                let d = u64::from(digit_value(at(k)));
                frac_any = true;
                if m >> 60 == 0 {
                    m = (m << 4) | d;
                    e2 -= 4;
                } else {
                    sticky |= d != 0;
                }
                k += 1;
            }
            if any || frac_any {
                any = true;
                j = k;
            }
        }
        if !any {
            // "0x" sans chiffre : seul le '0' est consommé.
            return FloatParse {
                value: sign(0.0),
                end: Some(i + 1),
                erange: false,
            };
        }
        if (at(j) == 0x70 || at(j) == 0x50)
            && let Some((e, k)) = parse_exponent(&at, j + 1)
        {
            e2 = e2.saturating_add(e);
            j = k;
        }
        let (v, erange) = hex_to_float(m, sticky, e2, single);
        return FloatParse {
            value: sign(v),
            end: Some(j),
            erange,
        };
    }

    // Décimal.
    let mut j = i;
    let mut int_digits = String::new();
    while is_digit(at(j)) {
        int_digits.push(at(j) as u8 as char);
        j += 1;
    }
    let mut frac_digits = String::new();
    if at(j) == 0x2E {
        let mut k = j + 1;
        while is_digit(at(k)) {
            frac_digits.push(at(k) as u8 as char);
            k += 1;
        }
        if !int_digits.is_empty() || !frac_digits.is_empty() {
            j = k;
        }
    }
    if int_digits.is_empty() && frac_digits.is_empty() {
        return none;
    }
    let mut exp = 0i64;
    if (at(j) == 0x65 || at(j) == 0x45)
        && let Some((e, k)) = parse_exponent(&at, j + 1)
    {
        exp = e;
        j = k;
    }
    let nonzero = int_digits
        .bytes()
        .chain(frac_digits.bytes())
        .any(|b| b != b'0');
    let text = format!(
        "{}.{}e{}",
        if int_digits.is_empty() {
            "0"
        } else {
            &int_digits
        },
        if frac_digits.is_empty() {
            "0"
        } else {
            &frac_digits
        },
        exp
    );
    let v = if single {
        text.parse::<f32>().map(f64::from).unwrap_or(0.0)
    } else {
        text.parse::<f64>().unwrap_or(0.0)
    };
    FloatParse {
        value: sign(v),
        end: Some(j),
        erange: v.is_infinite() || (v == 0.0 && nonzero),
    }
}

/// # Safety
/// `s` nul ou terminé par NUL ; `end` nul ou inscriptible.
unsafe fn strtod_impl<C: Ch>(s: *const C, end: *mut *const C, single: bool) -> f64 {
    if !end.is_null() {
        // SAFETY: `end` non-nul fourni par l'appelant.
        unsafe { *end = s };
    }
    if s.is_null() {
        set_errno(EINVAL);
        return 0.0;
    }
    // SAFETY: `s` non-nul terminé par NUL.
    let p = unsafe { parse_float(s, single) };
    if let (Some(e), false) = (p.end, end.is_null()) {
        // SAFETY: `e` est dans la chaîne analysée.
        unsafe { *end = s.add(e) };
    }
    if p.erange {
        set_errno(ERANGE);
    }
    p.value
}

extern "win64" fn crt_strtod(s: *const u8, end: *mut *const u8) -> f64 {
    ffi_guard!(CHANNEL, "strtod", 0.0, {
        // SAFETY: chaîne C et endptr fournis par l'invité.
        unsafe { strtod_impl(s, end, false) }
    })
}

extern "win64" fn crt_strtof(s: *const u8, end: *mut *const u8) -> f32 {
    ffi_guard!(CHANNEL, "strtof", 0.0, {
        // SAFETY: chaîne C et endptr fournis par l'invité.
        unsafe { strtod_impl(s, end, true) as f32 }
    })
}

extern "win64" fn crt_atof(s: *const u8) -> f64 {
    crt_strtod(s, ptr::null_mut())
}

extern "win64" fn crt_wcstod(s: *const u16, end: *mut *const u16) -> f64 {
    ffi_guard!(CHANNEL, "wcstod", 0.0, {
        // SAFETY: chaîne large et endptr fournis par l'invité.
        unsafe { strtod_impl(s, end, false) }
    })
}

extern "win64" fn crt_wcstof(s: *const u16, end: *mut *const u16) -> f32 {
    ffi_guard!(CHANNEL, "wcstof", 0.0, {
        // SAFETY: chaîne large et endptr fournis par l'invité.
        unsafe { strtod_impl(s, end, true) as f32 }
    })
}

extern "win64" fn crt_wtof(s: *const u16) -> f64 {
    crt_wcstod(s, ptr::null_mut())
}

// ===================================================================================
// Entier → chaîne (_itoa & co)
// ===================================================================================

/// Chiffres (minuscules, comme l'UCRT) de `mag` en base `radix`, signe `-` éventuel.
fn radix_digits(mut mag: u64, neg: bool, radix: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(66);
    loop {
        let d = (mag % u64::from(radix)) as u8;
        out.push(if d < 10 { b'0' + d } else { b'a' + d - 10 });
        mag /= u64::from(radix);
        if mag == 0 {
            break;
        }
    }
    if neg {
        out.push(b'-');
    }
    out.reverse();
    out
}

/// Implémentation commune `_xtox_s` (taille bornée) ; `size == usize::MAX` = non borné.
///
/// # Safety
/// `buf` nul ou inscriptible sur `size` éléments (ou assez grand si non borné).
unsafe fn xtox_s<C: Ch>(mag: u64, neg: bool, buf: *mut C, size: usize, radix: i32) -> i32 {
    if buf.is_null() || size == 0 {
        return invalid(EINVAL);
    }
    // SAFETY: `buf` non-nul, `size >= 1`.
    unsafe { *buf = C::NUL };
    if !(2..=36).contains(&radix) {
        return invalid(EINVAL);
    }
    let digits = radix_digits(mag, neg, radix as u32);
    if digits.len() >= size {
        return invalid(ERANGE);
    }
    for (k, &b) in digits.iter().enumerate() {
        // SAFETY: `k < digits.len() < size`.
        unsafe { *buf.add(k) = C::from_code(u32::from(b)) };
    }
    // SAFETY: `digits.len() < size`.
    unsafe { *buf.add(digits.len()) = C::NUL };
    0
}

fn split_i32(v: i32, radix: i32) -> (u64, bool) {
    if radix == 10 && v < 0 {
        (u64::from(v.unsigned_abs()), true)
    } else {
        (u64::from(v as u32), false)
    }
}

fn split_i64(v: i64, radix: i32) -> (u64, bool) {
    if radix == 10 && v < 0 {
        (v.unsigned_abs(), true)
    } else {
        (v as u64, false)
    }
}

macro_rules! xtoa_fns {
    ($( $f:ident, $fs:ident, $name:literal, $names:literal, $ty:ty, $c:ty, $split:expr; )*) => {$(
        extern "win64" fn $f(v: $ty, buf: *mut $c, radix: i32) -> *mut $c {
            ffi_guard!(CHANNEL, $name, buf, {
                let (mag, neg) = $split(v, radix);
                // SAFETY: tampon non borné fourni par l'invité (contrat `_itoa`).
                unsafe { xtox_s(mag, neg, buf, usize::MAX, radix) };
                buf
            })
        }
        extern "win64" fn $fs(v: $ty, buf: *mut $c, size: usize, radix: i32) -> i32 {
            ffi_guard!(CHANNEL, $names, EINVAL, {
                let (mag, neg) = $split(v, radix);
                // SAFETY: tampon de `size` éléments fourni par l'invité.
                unsafe { xtox_s(mag, neg, buf, size, radix) }
            })
        }
    )*};
}

xtoa_fns! {
    crt_itoa, crt_itoa_s, "_itoa", "_itoa_s", i32, u8, split_i32;
    crt_ultoa, crt_ultoa_s, "_ultoa", "_ultoa_s", u32, u8, |v: u32, _r| (u64::from(v), false);
    crt_i64toa, crt_i64toa_s, "_i64toa", "_i64toa_s", i64, u8, split_i64;
    crt_ui64toa, crt_ui64toa_s, "_ui64toa", "_ui64toa_s", u64, u8, |v: u64, _r| (v, false);
    crt_itow, crt_itow_s, "_itow", "_itow_s", i32, u16, split_i32;
    crt_ultow, crt_ultow_s, "_ultow", "_ultow_s", u32, u16, |v: u32, _r| (u64::from(v), false);
    crt_i64tow, crt_i64tow_s, "_i64tow", "_i64tow_s", i64, u16, split_i64;
    crt_ui64tow, crt_ui64tow_s, "_ui64tow", "_ui64tow_s", u64, u16, |v: u64, _r| (v, false);
}

// ===================================================================================
// Chaînes : implémentations génériques
// ===================================================================================

/// `strcmp`/`wcscmp` : -1 / 0 / 1 comme l'UCRT.
///
/// # Safety
/// `a`, `b` non-nuls et terminés par NUL.
unsafe fn tcscmp<C: Ch>(a: *const C, b: *const C) -> i32 {
    let mut i = 0;
    loop {
        // SAFETY: lecture jusqu'au premier NUL ou différence.
        let (x, y) = unsafe { ((*a.add(i)).code(), (*b.add(i)).code()) };
        if x != y {
            return if x < y { -1 } else { 1 };
        }
        if x == 0 {
            return 0;
        }
        i += 1;
    }
}

/// Comparaison insensible à la casse ASCII ; renvoie la différence des minuscules.
///
/// # Safety
/// `a`, `b` non-nuls et terminés par NUL (ou lisibles sur `n` éléments).
unsafe fn tcsnicmp<C: Ch>(a: *const C, b: *const C, n: usize) -> i32 {
    for i in 0..n {
        // SAFETY: lecture bornée par `n` et le NUL.
        let (x, y) = unsafe { ((*a.add(i)).code(), (*b.add(i)).code()) };
        let (x, y) = (ascii_lower(x), ascii_lower(y));
        if x != y || x == 0 {
            return x as i32 - y as i32;
        }
    }
    0
}

/// # Safety
/// `dst` assez grand ; `src` terminé par NUL.
unsafe fn tcscpy<C: Ch>(dst: *mut C, src: *const C) {
    let mut i = 0;
    loop {
        // SAFETY: contrat C `strcpy`.
        let c = unsafe { *src.add(i) };
        unsafe { *dst.add(i) = c };
        if c == C::NUL {
            return;
        }
        i += 1;
    }
}

/// # Safety
/// Contrat C `strncpy` (remplissage par NUL jusqu'à `n`).
unsafe fn tcsncpy<C: Ch>(dst: *mut C, src: *const C, n: usize) {
    let mut i = 0;
    while i < n {
        // SAFETY: contrat C `strncpy`.
        let c = unsafe { *src.add(i) };
        if c == C::NUL {
            break;
        }
        unsafe { *dst.add(i) = c };
        i += 1;
    }
    while i < n {
        // SAFETY: `dst` couvre `n` éléments.
        unsafe { *dst.add(i) = C::NUL };
        i += 1;
    }
}

/// # Safety
/// Contrat C `strncat`.
unsafe fn tcsncat<C: Ch>(dst: *mut C, src: *const C, n: usize) {
    // SAFETY: `dst` terminé par NUL.
    let d = unsafe { dst.add(slen(dst)) };
    let mut i = 0;
    while i < n {
        // SAFETY: contrat C `strncat`.
        let c = unsafe { *src.add(i) };
        if c == C::NUL {
            break;
        }
        unsafe { *d.add(i) = c };
        i += 1;
    }
    // SAFETY: place réservée pour le NUL final par le contrat.
    unsafe { *d.add(i) = C::NUL };
}

/// Port de `tcscpy_s.inl` (UCRT).
///
/// # Safety
/// `dst` nul ou inscriptible sur `size` éléments ; `src` nul ou terminé par NUL.
unsafe fn tcscpy_s<C: Ch>(dst: *mut C, size: usize, src: *const C) -> i32 {
    if dst.is_null() || size == 0 {
        return invalid(EINVAL);
    }
    if src.is_null() {
        // SAFETY: `dst` non-nul, `size >= 1`.
        unsafe { *dst = C::NUL };
        return invalid(EINVAL);
    }
    let mut i = 0;
    loop {
        if i == size {
            // SAFETY: `size >= 1`.
            unsafe { *dst = C::NUL };
            return invalid(ERANGE);
        }
        // SAFETY: `i < size`, `src` terminé par NUL et non encore atteint.
        let c = unsafe { *src.add(i) };
        unsafe { *dst.add(i) = c };
        if c == C::NUL {
            return 0;
        }
        i += 1;
    }
}

/// Port de `tcscat_s.inl` (UCRT).
///
/// # Safety
/// Voir [`tcscpy_s`].
unsafe fn tcscat_s<C: Ch>(dst: *mut C, size: usize, src: *const C) -> i32 {
    if dst.is_null() || size == 0 {
        return invalid(EINVAL);
    }
    if src.is_null() {
        // SAFETY: `dst` non-nul, `size >= 1`.
        unsafe { *dst = C::NUL };
        return invalid(EINVAL);
    }
    // SAFETY: lecture bornée par `size`.
    let n = unsafe { snlen(dst, size) };
    if n == size {
        // Destination non terminée dans `size` éléments.
        unsafe { *dst = C::NUL };
        return invalid(EINVAL);
    }
    let mut i = 0;
    loop {
        if n + i == size {
            // SAFETY: `size >= 1`.
            unsafe { *dst = C::NUL };
            return invalid(ERANGE);
        }
        // SAFETY: `n + i < size` ; `src` terminé par NUL.
        let c = unsafe { *src.add(i) };
        unsafe { *dst.add(n + i) = c };
        if c == C::NUL {
            return 0;
        }
        i += 1;
    }
}

/// Port de `tcsncpy_s.inl` (UCRT), y compris `_TRUNCATE` → `STRUNCATE`.
///
/// # Safety
/// Voir [`tcscpy_s`] ; `src` lisible sur `count` éléments ou jusqu'au NUL.
unsafe fn tcsncpy_s<C: Ch>(dst: *mut C, size: usize, src: *const C, count: usize) -> i32 {
    if count == 0 && dst.is_null() && size == 0 {
        return 0;
    }
    if dst.is_null() || size == 0 {
        return invalid(EINVAL);
    }
    if count == 0 {
        // SAFETY: `dst` non-nul, `size >= 1`.
        unsafe { *dst = C::NUL };
        return 0;
    }
    if src.is_null() {
        // SAFETY: idem.
        unsafe { *dst = C::NUL };
        return invalid(EINVAL);
    }
    let mut available = size;
    let mut count = count;
    let mut i = 0;
    // SAFETY (boucles) : `i < size` car `available = size - i >= 1` à chaque écriture.
    if count == TRUNCATE {
        loop {
            let c = unsafe { *src.add(i) };
            unsafe { *dst.add(i) = c };
            i += 1;
            if c == C::NUL {
                break;
            }
            available -= 1;
            if available == 0 {
                break;
            }
        }
    } else {
        loop {
            let c = unsafe { *src.add(i) };
            unsafe { *dst.add(i) = c };
            i += 1;
            if c == C::NUL {
                break;
            }
            available -= 1;
            if available == 0 {
                break;
            }
            count -= 1;
            if count == 0 {
                break;
            }
        }
        if count == 0 {
            // SAFETY: `available >= 1` donc `i < size`.
            unsafe { *dst.add(i) = C::NUL };
        }
    }
    if available == 0 {
        if count == TRUNCATE {
            // SAFETY: `size >= 1`.
            unsafe { *dst.add(size - 1) = C::NUL };
            return STRUNCATE;
        }
        // SAFETY: `size >= 1`.
        unsafe { *dst = C::NUL };
        return invalid(ERANGE);
    }
    0
}

/// Port de `tcsncat_s.inl` (UCRT), y compris `_TRUNCATE` → `STRUNCATE`.
///
/// # Safety
/// Voir [`tcsncpy_s`].
unsafe fn tcsncat_s<C: Ch>(dst: *mut C, size: usize, src: *const C, count: usize) -> i32 {
    if count == 0 && dst.is_null() && size == 0 {
        return 0;
    }
    if dst.is_null() || size == 0 {
        return invalid(EINVAL);
    }
    if count != 0 && src.is_null() {
        // SAFETY: `dst` non-nul, `size >= 1`.
        unsafe { *dst = C::NUL };
        return invalid(EINVAL);
    }
    // SAFETY: lecture bornée par `size`.
    let mut di = unsafe { snlen(dst, size) };
    let mut available = size - di;
    if available == 0 {
        // SAFETY: `size >= 1`.
        unsafe { *dst = C::NUL };
        return invalid(EINVAL);
    }
    let mut count = count;
    let mut si = 0;
    // SAFETY (boucles) : chaque écriture se fait à `di < size` (`available >= 1`).
    if count == TRUNCATE {
        loop {
            let c = unsafe { *src.add(si) };
            unsafe { *dst.add(di) = c };
            si += 1;
            di += 1;
            if c == C::NUL {
                break;
            }
            available -= 1;
            if available == 0 {
                break;
            }
        }
    } else {
        loop {
            if count == 0 {
                break;
            }
            let c = unsafe { *src.add(si) };
            unsafe { *dst.add(di) = c };
            si += 1;
            di += 1;
            if c == C::NUL {
                break;
            }
            available -= 1;
            if available == 0 {
                break;
            }
            count -= 1;
        }
        if count == 0 {
            // SAFETY: `available >= 1` donc `di < size`.
            unsafe { *dst.add(di) = C::NUL };
        }
    }
    if available == 0 {
        if count == TRUNCATE {
            // SAFETY: `size >= 1`.
            unsafe { *dst.add(size - 1) = C::NUL };
            return STRUNCATE;
        }
        // SAFETY: `size >= 1`.
        unsafe { *dst = C::NUL };
        return invalid(ERANGE);
    }
    0
}

/// # Safety
/// `s` non-nul terminé par NUL. Le NUL final est trouvable (`c == 0`).
unsafe fn tcschr<C: Ch>(s: *const C, c: C) -> *const C {
    let mut i = 0;
    loop {
        // SAFETY: lecture jusqu'au NUL inclus.
        let x = unsafe { *s.add(i) };
        if x == c {
            return unsafe { s.add(i) };
        }
        if x == C::NUL {
            return ptr::null();
        }
        i += 1;
    }
}

/// # Safety
/// `s` non-nul terminé par NUL.
unsafe fn tcsrchr<C: Ch>(s: *const C, c: C) -> *const C {
    let mut found = ptr::null();
    let mut i = 0;
    loop {
        // SAFETY: lecture jusqu'au NUL inclus.
        let x = unsafe { *s.add(i) };
        if x == c {
            found = unsafe { s.add(i) };
        }
        if x == C::NUL {
            return found;
        }
        i += 1;
    }
}

/// Longueur du préfixe dont l'appartenance à `set` vaut `inside`.
///
/// # Safety
/// `s`, `set` non-nuls terminés par NUL.
unsafe fn tcsspn<C: Ch>(s: *const C, set: *const C, inside: bool) -> usize {
    let mut i = 0;
    loop {
        // SAFETY: lecture jusqu'au NUL.
        let x = unsafe { *s.add(i) };
        if x == C::NUL || unsafe { in_set(set, x) } != inside {
            return i;
        }
        i += 1;
    }
}

/// # Safety
/// `s`, `set` non-nuls terminés par NUL.
unsafe fn tcspbrk<C: Ch>(s: *const C, set: *const C) -> *const C {
    // SAFETY: contrat transmis.
    let n = unsafe { tcsspn(s, set, false) };
    // SAFETY: `n <= slen(s)`.
    if unsafe { *s.add(n) } == C::NUL {
        ptr::null()
    } else {
        unsafe { s.add(n) }
    }
}

/// Port de `strtok_s` sans validation (UCRT).
///
/// # Safety
/// `s` nul ou terminé par NUL inscriptible ; `delim` terminé par NUL ; `ctx` valide.
unsafe fn tcstok<C: Ch>(s: *mut C, delim: *const C, ctx: *mut *mut C) -> *mut C {
    // SAFETY: `ctx` valide (contrat).
    let mut p = if s.is_null() { unsafe { *ctx } } else { s };
    if p.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `p` pointe dans une chaîne terminée par NUL ; `delim` terminé par NUL.
    unsafe {
        while *p != C::NUL && in_set(delim, *p) {
            p = p.add(1);
        }
        if *p == C::NUL {
            *ctx = p;
            return ptr::null_mut();
        }
        let token = p;
        while *p != C::NUL && !in_set(delim, *p) {
            p = p.add(1);
        }
        if *p != C::NUL {
            *p = C::NUL;
            p = p.add(1);
        }
        *ctx = p;
        token
    }
}

/// Duplique une chaîne via `libc::malloc` (compatible avec `free` de `ucrt.rs`).
///
/// # Safety
/// `s` nul ou terminé par NUL.
unsafe fn tcsdup<C: Ch>(s: *const C) -> *mut C {
    if s.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `s` terminé par NUL.
    let n = unsafe { slen(s) } + 1;
    let Some(bytes) = n.checked_mul(size_of::<C>()) else {
        return ptr::null_mut();
    };
    // SAFETY: allocation libc, libérée par `free` (ucrt.rs → `libc::free`).
    let p = unsafe { libc::malloc(bytes) }.cast::<C>();
    if p.is_null() {
        set_errno(12); // ENOMEM
        return p;
    }
    // SAFETY: `p` couvre `n` éléments, `s` en contient `n` (NUL inclus).
    unsafe { ptr::copy_nonoverlapping(s, p, n) };
    p
}

// ===================================================================================
// Chaînes étroites : exports
// ===================================================================================

extern "win64" fn crt_strlen(s: *const u8) -> usize {
    ffi_guard!(CHANNEL, "strlen", 0, {
        if s.is_null() {
            return 0;
        }
        // SAFETY: chaîne C fournie par l'invité.
        unsafe { slen(s) }
    })
}

extern "win64" fn crt_strnlen(s: *const u8, max: usize) -> usize {
    ffi_guard!(CHANNEL, "strnlen", 0, {
        if s.is_null() {
            return 0;
        }
        // SAFETY: lecture bornée par `max`.
        unsafe { snlen(s, max) }
    })
}

extern "win64" fn crt_strcmp(a: *const u8, b: *const u8) -> i32 {
    ffi_guard!(CHANNEL, "strcmp", 0, {
        if a.is_null() || b.is_null() {
            return 0;
        }
        // SAFETY: chaînes C fournies par l'invité.
        unsafe { tcscmp(a, b) }
    })
}

extern "win64" fn crt_stricmp(a: *const u8, b: *const u8) -> i32 {
    ffi_guard!(CHANNEL, "_stricmp", NLSCMPERROR, {
        if a.is_null() || b.is_null() {
            set_errno(EINVAL);
            return NLSCMPERROR;
        }
        // SAFETY: chaînes C fournies par l'invité.
        unsafe { tcsnicmp(a, b, usize::MAX) }
    })
}

extern "win64" fn crt_strnicmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    ffi_guard!(CHANNEL, "_strnicmp", NLSCMPERROR, {
        if a.is_null() || b.is_null() {
            set_errno(EINVAL);
            return NLSCMPERROR;
        }
        if n > i32::MAX as usize {
            set_errno(EINVAL);
            return NLSCMPERROR;
        }
        // SAFETY: chaînes C fournies par l'invité.
        unsafe { tcsnicmp(a, b, n) }
    })
}

extern "win64" fn crt_strcpy(dst: *mut u8, src: *const u8) -> *mut u8 {
    ffi_guard!(CHANNEL, "strcpy", dst, {
        if !dst.is_null() && !src.is_null() {
            // SAFETY: contrat C `strcpy`.
            unsafe { tcscpy(dst, src) };
        }
        dst
    })
}

extern "win64" fn crt_strcat(dst: *mut u8, src: *const u8) -> *mut u8 {
    ffi_guard!(CHANNEL, "strcat", dst, {
        if !dst.is_null() && !src.is_null() {
            // SAFETY: contrat C `strcat`.
            unsafe { tcscpy(dst.add(slen(dst)), src) };
        }
        dst
    })
}

extern "win64" fn crt_strncpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    ffi_guard!(CHANNEL, "strncpy", dst, {
        if !dst.is_null() && !src.is_null() {
            // SAFETY: contrat C `strncpy`.
            unsafe { tcsncpy(dst, src, n) };
        }
        dst
    })
}

extern "win64" fn crt_strncat(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    ffi_guard!(CHANNEL, "strncat", dst, {
        if !dst.is_null() && !src.is_null() {
            // SAFETY: contrat C `strncat`.
            unsafe { tcsncat(dst, src, n) };
        }
        dst
    })
}

extern "win64" fn crt_strcpy_s(dst: *mut u8, size: usize, src: *const u8) -> i32 {
    ffi_guard!(CHANNEL, "strcpy_s", EINVAL, {
        // SAFETY: tampon et source fournis par l'invité.
        unsafe { tcscpy_s(dst, size, src) }
    })
}

extern "win64" fn crt_strcat_s(dst: *mut u8, size: usize, src: *const u8) -> i32 {
    ffi_guard!(CHANNEL, "strcat_s", EINVAL, {
        // SAFETY: tampon et source fournis par l'invité.
        unsafe { tcscat_s(dst, size, src) }
    })
}

extern "win64" fn crt_strncpy_s(dst: *mut u8, size: usize, src: *const u8, count: usize) -> i32 {
    ffi_guard!(CHANNEL, "strncpy_s", EINVAL, {
        // SAFETY: tampon et source fournis par l'invité.
        unsafe { tcsncpy_s(dst, size, src, count) }
    })
}

extern "win64" fn crt_strncat_s(dst: *mut u8, size: usize, src: *const u8, count: usize) -> i32 {
    ffi_guard!(CHANNEL, "strncat_s", EINVAL, {
        // SAFETY: tampon et source fournis par l'invité.
        unsafe { tcsncat_s(dst, size, src, count) }
    })
}

extern "win64" fn crt_strchr(s: *const u8, c: i32) -> *const u8 {
    ffi_guard!(CHANNEL, "strchr", ptr::null(), {
        if s.is_null() {
            return ptr::null();
        }
        // SAFETY: chaîne C fournie par l'invité.
        unsafe { tcschr(s, c as u8) }
    })
}

extern "win64" fn crt_strrchr(s: *const u8, c: i32) -> *const u8 {
    ffi_guard!(CHANNEL, "strrchr", ptr::null(), {
        if s.is_null() {
            return ptr::null();
        }
        // SAFETY: chaîne C fournie par l'invité.
        unsafe { tcsrchr(s, c as u8) }
    })
}

extern "win64" fn crt_strpbrk(s: *const u8, set: *const u8) -> *const u8 {
    ffi_guard!(CHANNEL, "strpbrk", ptr::null(), {
        if s.is_null() || set.is_null() {
            return ptr::null();
        }
        // SAFETY: chaînes C fournies par l'invité.
        unsafe { tcspbrk(s, set) }
    })
}

extern "win64" fn crt_strspn(s: *const u8, set: *const u8) -> usize {
    ffi_guard!(CHANNEL, "strspn", 0, {
        if s.is_null() || set.is_null() {
            return 0;
        }
        // SAFETY: chaînes C fournies par l'invité.
        unsafe { tcsspn(s, set, true) }
    })
}

extern "win64" fn crt_strcspn(s: *const u8, set: *const u8) -> usize {
    ffi_guard!(CHANNEL, "strcspn", 0, {
        if s.is_null() || set.is_null() {
            return 0;
        }
        // SAFETY: chaînes C fournies par l'invité.
        unsafe { tcsspn(s, set, false) }
    })
}

extern "win64" fn crt_strtok(s: *mut u8, delim: *const u8) -> *mut u8 {
    ffi_guard!(CHANNEL, "strtok", ptr::null_mut(), {
        if delim.is_null() {
            set_errno(EINVAL);
            return ptr::null_mut();
        }
        STRTOK_CTX.with(|cell| {
            let mut ctx = cell.get();
            // SAFETY: contexte par thread ; chaînes fournies par l'invité.
            let r = unsafe { tcstok(s, delim, &mut ctx) };
            cell.set(ctx);
            r
        })
    })
}

extern "win64" fn crt_strtok_s(s: *mut u8, delim: *const u8, ctx: *mut *mut u8) -> *mut u8 {
    ffi_guard!(CHANNEL, "strtok_s", ptr::null_mut(), {
        // SAFETY: `ctx` lu seulement s'il est non-nul.
        if ctx.is_null() || delim.is_null() || (s.is_null() && unsafe { (*ctx).is_null() }) {
            set_errno(EINVAL);
            return ptr::null_mut();
        }
        // SAFETY: paramètres validés ci-dessus.
        unsafe { tcstok(s, delim, ctx) }
    })
}

extern "win64" fn crt_strdup(s: *const u8) -> *mut u8 {
    ffi_guard!(CHANNEL, "_strdup", ptr::null_mut(), {
        // SAFETY: chaîne C fournie par l'invité (ou nulle).
        unsafe { tcsdup(s) }
    })
}

extern "win64" fn crt_memchr(p: *const c_void, c: i32, n: usize) -> *const c_void {
    ffi_guard!(CHANNEL, "memchr", ptr::null(), {
        if p.is_null() || n == 0 {
            return ptr::null();
        }
        // SAFETY: `p` couvre `n` octets (contrat C).
        let bytes = unsafe { std::slice::from_raw_parts(p.cast::<u8>(), n) };
        match bytes.iter().position(|&b| b == c as u8) {
            // SAFETY: `i < n`.
            Some(i) => unsafe { p.cast::<u8>().add(i).cast() },
            None => ptr::null(),
        }
    })
}

// ===================================================================================
// Chaînes larges : exports
// ===================================================================================

extern "win64" fn crt_wcsnlen(s: *const u16, max: usize) -> usize {
    ffi_guard!(CHANNEL, "wcsnlen", 0, {
        if s.is_null() {
            return 0;
        }
        // SAFETY: lecture bornée par `max`.
        unsafe { snlen(s, max) }
    })
}

extern "win64" fn crt_wcscmp(a: *const u16, b: *const u16) -> i32 {
    ffi_guard!(CHANNEL, "wcscmp", 0, {
        if a.is_null() || b.is_null() {
            return 0;
        }
        // SAFETY: chaînes larges fournies par l'invité.
        unsafe { tcscmp(a, b) }
    })
}

extern "win64" fn crt_wcsncmp(a: *const u16, b: *const u16, n: usize) -> i32 {
    ffi_guard!(CHANNEL, "wcsncmp", 0, {
        if a.is_null() || b.is_null() {
            return 0;
        }
        for i in 0..n {
            // SAFETY: lecture bornée par `n` et le NUL.
            let (x, y) = unsafe { (*a.add(i), *b.add(i)) };
            if x != y || x == 0 {
                return i32::from(x) - i32::from(y);
            }
        }
        0
    })
}

extern "win64" fn crt_wcsicmp(a: *const u16, b: *const u16) -> i32 {
    ffi_guard!(CHANNEL, "_wcsicmp", NLSCMPERROR, {
        if a.is_null() || b.is_null() {
            set_errno(EINVAL);
            return NLSCMPERROR;
        }
        // SAFETY: chaînes larges fournies par l'invité.
        unsafe { tcsnicmp(a, b, usize::MAX) }
    })
}

extern "win64" fn crt_wcsnicmp(a: *const u16, b: *const u16, n: usize) -> i32 {
    ffi_guard!(CHANNEL, "_wcsnicmp", NLSCMPERROR, {
        if a.is_null() || b.is_null() {
            set_errno(EINVAL);
            return NLSCMPERROR;
        }
        // SAFETY: chaînes larges fournies par l'invité.
        unsafe { tcsnicmp(a, b, n) }
    })
}

extern "win64" fn crt_wcscpy(dst: *mut u16, src: *const u16) -> *mut u16 {
    ffi_guard!(CHANNEL, "wcscpy", dst, {
        if !dst.is_null() && !src.is_null() {
            // SAFETY: contrat C `wcscpy`.
            unsafe { tcscpy(dst, src) };
        }
        dst
    })
}

extern "win64" fn crt_wcscat(dst: *mut u16, src: *const u16) -> *mut u16 {
    ffi_guard!(CHANNEL, "wcscat", dst, {
        if !dst.is_null() && !src.is_null() {
            // SAFETY: contrat C `wcscat`.
            unsafe { tcscpy(dst.add(slen(dst)), src) };
        }
        dst
    })
}

extern "win64" fn crt_wcsncpy(dst: *mut u16, src: *const u16, n: usize) -> *mut u16 {
    ffi_guard!(CHANNEL, "wcsncpy", dst, {
        if !dst.is_null() && !src.is_null() {
            // SAFETY: contrat C `wcsncpy`.
            unsafe { tcsncpy(dst, src, n) };
        }
        dst
    })
}

extern "win64" fn crt_wcsncat(dst: *mut u16, src: *const u16, n: usize) -> *mut u16 {
    ffi_guard!(CHANNEL, "wcsncat", dst, {
        if !dst.is_null() && !src.is_null() {
            // SAFETY: contrat C `wcsncat`.
            unsafe { tcsncat(dst, src, n) };
        }
        dst
    })
}

extern "win64" fn crt_wcscpy_s(dst: *mut u16, size: usize, src: *const u16) -> i32 {
    ffi_guard!(CHANNEL, "wcscpy_s", EINVAL, {
        // SAFETY: tampon et source fournis par l'invité.
        unsafe { tcscpy_s(dst, size, src) }
    })
}

extern "win64" fn crt_wcscat_s(dst: *mut u16, size: usize, src: *const u16) -> i32 {
    ffi_guard!(CHANNEL, "wcscat_s", EINVAL, {
        // SAFETY: tampon et source fournis par l'invité.
        unsafe { tcscat_s(dst, size, src) }
    })
}

extern "win64" fn crt_wcsncpy_s(dst: *mut u16, size: usize, src: *const u16, count: usize) -> i32 {
    ffi_guard!(CHANNEL, "wcsncpy_s", EINVAL, {
        // SAFETY: tampon et source fournis par l'invité.
        unsafe { tcsncpy_s(dst, size, src, count) }
    })
}

extern "win64" fn crt_wcsncat_s(dst: *mut u16, size: usize, src: *const u16, count: usize) -> i32 {
    ffi_guard!(CHANNEL, "wcsncat_s", EINVAL, {
        // SAFETY: tampon et source fournis par l'invité.
        unsafe { tcsncat_s(dst, size, src, count) }
    })
}

extern "win64" fn crt_wcschr(s: *const u16, c: u16) -> *const u16 {
    ffi_guard!(CHANNEL, "wcschr", ptr::null(), {
        if s.is_null() {
            return ptr::null();
        }
        // SAFETY: chaîne large fournie par l'invité.
        unsafe { tcschr(s, c) }
    })
}

extern "win64" fn crt_wcsrchr(s: *const u16, c: u16) -> *const u16 {
    ffi_guard!(CHANNEL, "wcsrchr", ptr::null(), {
        if s.is_null() {
            return ptr::null();
        }
        // SAFETY: chaîne large fournie par l'invité.
        unsafe { tcsrchr(s, c) }
    })
}

extern "win64" fn crt_wcspbrk(s: *const u16, set: *const u16) -> *const u16 {
    ffi_guard!(CHANNEL, "wcspbrk", ptr::null(), {
        if s.is_null() || set.is_null() {
            return ptr::null();
        }
        // SAFETY: chaînes larges fournies par l'invité.
        unsafe { tcspbrk(s, set) }
    })
}

extern "win64" fn crt_wcsspn(s: *const u16, set: *const u16) -> usize {
    ffi_guard!(CHANNEL, "wcsspn", 0, {
        if s.is_null() || set.is_null() {
            return 0;
        }
        // SAFETY: chaînes larges fournies par l'invité.
        unsafe { tcsspn(s, set, true) }
    })
}

extern "win64" fn crt_wcscspn(s: *const u16, set: *const u16) -> usize {
    ffi_guard!(CHANNEL, "wcscspn", 0, {
        if s.is_null() || set.is_null() {
            return 0;
        }
        // SAFETY: chaînes larges fournies par l'invité.
        unsafe { tcsspn(s, set, false) }
    })
}

/// `wcstok` UCRT (forme conforme à 3 arguments) ; contexte nul → contexte par thread.
/// Également exporté comme `wcstok_s` (validation identique quand `ctx` est fourni).
extern "win64" fn crt_wcstok(s: *mut u16, delim: *const u16, ctx: *mut *mut u16) -> *mut u16 {
    ffi_guard!(CHANNEL, "wcstok", ptr::null_mut(), {
        if delim.is_null() {
            set_errno(EINVAL);
            return ptr::null_mut();
        }
        if ctx.is_null() {
            return WCSTOK_CTX.with(|cell| {
                let mut c = cell.get();
                // SAFETY: contexte par thread ; chaînes fournies par l'invité.
                let r = unsafe { tcstok(s, delim, &mut c) };
                cell.set(c);
                r
            });
        }
        // SAFETY: `ctx` non-nul.
        if s.is_null() && unsafe { (*ctx).is_null() } {
            set_errno(EINVAL);
            return ptr::null_mut();
        }
        // SAFETY: paramètres validés.
        unsafe { tcstok(s, delim, ctx) }
    })
}

extern "win64" fn crt_wcsdup(s: *const u16) -> *mut u16 {
    ffi_guard!(CHANNEL, "_wcsdup", ptr::null_mut(), {
        // SAFETY: chaîne large fournie par l'invité (ou nulle).
        unsafe { tcsdup(s) }
    })
}

// ===================================================================================
// api-ms-win-crt-utility
// ===================================================================================

type CmpFn = unsafe extern "win64" fn(*const c_void, *const c_void) -> i32;
type CmpCtxFn = unsafe extern "win64" fn(*mut c_void, *const c_void, *const c_void) -> i32;

/// Recherche dichotomique ; `cmp(clé, élément)`.
fn bsearch_core(
    num: usize,
    width: usize,
    base: *const u8,
    mut cmp: impl FnMut(*const c_void) -> i32,
) -> *mut c_void {
    let (mut lo, mut hi) = (0usize, num);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let elem = base.wrapping_add(mid * width).cast::<c_void>();
        let r = cmp(elem);
        if r == 0 {
            return elem.cast_mut();
        }
        if r < 0 {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    ptr::null_mut()
}

extern "win64" fn crt_bsearch(
    key: *const c_void,
    base: *const c_void,
    num: usize,
    width: usize,
    compare: Option<CmpFn>,
) -> *mut c_void {
    ffi_guard!(CHANNEL, "bsearch", ptr::null_mut(), {
        let Some(cmp) = compare else {
            set_errno(EINVAL);
            return ptr::null_mut();
        };
        if (base.is_null() && num != 0) || width == 0 {
            set_errno(EINVAL);
            return ptr::null_mut();
        }
        // SAFETY: comparateur invité appelé sur des éléments du tableau fourni.
        bsearch_core(num, width, base.cast(), |e| unsafe { cmp(key, e) })
    })
}

extern "win64" fn crt_bsearch_s(
    key: *const c_void,
    base: *const c_void,
    num: usize,
    width: usize,
    compare: Option<CmpCtxFn>,
    context: *mut c_void,
) -> *mut c_void {
    ffi_guard!(CHANNEL, "bsearch_s", ptr::null_mut(), {
        let Some(cmp) = compare else {
            set_errno(EINVAL);
            return ptr::null_mut();
        };
        if (base.is_null() && num != 0) || width == 0 {
            set_errno(EINVAL);
            return ptr::null_mut();
        }
        // SAFETY: comparateur invité appelé sur des éléments du tableau fourni.
        bsearch_core(num, width, base.cast(), |e| unsafe { cmp(context, key, e) })
    })
}

/// Tri par tas en place (O(n log n), aucune hypothèse d'ordre total côté invité).
///
/// # Safety
/// `base` couvre `num * width` octets inscriptibles.
unsafe fn heap_sort(
    base: *mut u8,
    num: usize,
    width: usize,
    mut cmp: impl FnMut(*const c_void, *const c_void) -> i32,
) {
    let elem = |i: usize| base.wrapping_add(i * width);
    let swap = |i: usize, j: usize| {
        // SAFETY: `i, j < num`, éléments disjoints (`i != j`).
        unsafe { ptr::swap_nonoverlapping(elem(i), elem(j), width) };
    };
    let sift =
        |cmp: &mut dyn FnMut(*const c_void, *const c_void) -> i32, mut root: usize, end: usize| {
            loop {
                let mut child = 2 * root + 1;
                if child >= end {
                    break;
                }
                if child + 1 < end && cmp(elem(child).cast(), elem(child + 1).cast()) < 0 {
                    child += 1;
                }
                if cmp(elem(root).cast(), elem(child).cast()) >= 0 {
                    break;
                }
                swap(root, child);
                root = child;
            }
        };
    for start in (0..num / 2).rev() {
        sift(&mut cmp, start, num);
    }
    for end in (1..num).rev() {
        swap(0, end);
        sift(&mut cmp, 0, end);
    }
}

extern "win64" fn crt_qsort_s(
    base: *mut c_void,
    num: usize,
    width: usize,
    compare: Option<CmpCtxFn>,
    context: *mut c_void,
) {
    ffi_guard!(CHANNEL, "qsort_s", (), {
        let Some(cmp) = compare else {
            set_errno(EINVAL);
            return;
        };
        if (base.is_null() && num != 0) || (width == 0 && num != 0) {
            set_errno(EINVAL);
            return;
        }
        if num <= 1 {
            return;
        }
        // SAFETY: tableau de `num * width` octets fourni par l'invité ; comparateur invité.
        unsafe {
            heap_sort(base.cast(), num, width, |a, b| cmp(context, a, b));
        }
    })
}

extern "win64" fn crt_abs(v: i32) -> i32 {
    ffi_guard!(CHANNEL, "abs", v, { v.wrapping_abs() })
}

extern "win64" fn crt_llabs(v: i64) -> i64 {
    ffi_guard!(CHANNEL, "llabs", v, { v.wrapping_abs() })
}

/// `div_t` / `ldiv_t` (long = 32 bits en LLP64).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DivT {
    quot: i32,
    rem: i32,
}
const _: () = assert!(size_of::<DivT>() == 8);
const _: () = assert!(std::mem::offset_of!(DivT, rem) == 4);

/// `lldiv_t`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LLDivT {
    quot: i64,
    rem: i64,
}
const _: () = assert!(size_of::<LLDivT>() == 16);
const _: () = assert!(std::mem::offset_of!(LLDivT, rem) == 8);

/// Division par zéro : MSVC lève `EXCEPTION_INT_DIVIDE_BY_ZERO` ; ici résultat nul
/// (échec documenté, pas d'exception structurée simulée).
extern "win64" fn crt_div(n: i32, d: i32) -> DivT {
    ffi_guard!(CHANNEL, "div", DivT::default(), {
        if d == 0 {
            return DivT::default();
        }
        DivT {
            quot: n.wrapping_div(d),
            rem: n.wrapping_rem(d),
        }
    })
}

extern "win64" fn crt_lldiv(n: i64, d: i64) -> LLDivT {
    ffi_guard!(CHANNEL, "lldiv", LLDivT::default(), {
        if d == 0 {
            return LLDivT::default();
        }
        LLDivT {
            quot: n.wrapping_div(d),
            rem: n.wrapping_rem(d),
        }
    })
}

// ===================================================================================
// Tests
// ===================================================================================

#[cfg(test)]
// Les tests manipulent des `*const u8` bruts comme l'invité : littéraux `b"..\0"` voulus.
#[allow(clippy::manual_c_str_literals)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn cstr(buf: &[u8]) -> &str {
        let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        std::str::from_utf8(&buf[..n]).unwrap_or("<utf8>")
    }

    #[test]
    fn errno_is_thread_local() {
        crt_set_errno(ERANGE);
        assert_eq!(get_errno(), ERANGE);
        std::thread::spawn(|| assert_eq!(get_errno(), 0))
            .join()
            .expect("thread de test");
        let mut v = 0;
        assert_eq!(crt_get_errno(&mut v), 0);
        assert_eq!(v, ERANGE);
    }

    #[test]
    fn strtol_basics_and_endptr() {
        let s = b"  -1234xyz\0";
        let mut end = ptr::null();
        assert_eq!(crt_strtol(s.as_ptr(), &mut end, 10), -1234);
        assert_eq!(end as usize - s.as_ptr() as usize, 7);

        let s = b"0x1Fg\0";
        assert_eq!(crt_strtol(s.as_ptr(), &mut end, 16), 31);
        assert_eq!(end as usize - s.as_ptr() as usize, 4);
        assert_eq!(crt_strtol(s.as_ptr(), &mut end, 0), 31);
        assert_eq!(crt_strtol(b"0755\0".as_ptr(), ptr::null_mut(), 0), 0o755);
        assert_eq!(crt_strtol(b"z\0".as_ptr(), ptr::null_mut(), 36), 35);
        assert_eq!(crt_strtol(b"101\0".as_ptr(), ptr::null_mut(), 2), 5);
        assert_eq!(crt_strtol(b"0\0".as_ptr(), ptr::null_mut(), 0), 0);

        // Aucun chiffre après "0x" : endptr = nptr (UCRT).
        let s = b"0xg\0";
        assert_eq!(crt_strtol(s.as_ptr(), &mut end, 16), 0);
        assert_eq!(end, s.as_ptr());
        let s = b"   +\0";
        assert_eq!(crt_strtol(s.as_ptr(), &mut end, 10), 0);
        assert_eq!(end, s.as_ptr());
    }

    #[test]
    fn strtol_invalid_base_and_null() {
        set_errno(0);
        let s = b"12\0";
        let mut end = ptr::null();
        assert_eq!(crt_strtol(s.as_ptr(), &mut end, 1), 0);
        assert_eq!(get_errno(), EINVAL);
        assert_eq!(end, s.as_ptr());
        set_errno(0);
        assert_eq!(crt_strtol(ptr::null(), ptr::null_mut(), 10), 0);
        assert_eq!(get_errno(), EINVAL);
        set_errno(0);
        assert_eq!(crt_strtoll(s.as_ptr(), ptr::null_mut(), 37), 0);
        assert_eq!(get_errno(), EINVAL);
    }

    #[test]
    fn strtol_overflow_saturates() {
        set_errno(0);
        let s = b"2147483648!\0";
        let mut end = ptr::null();
        assert_eq!(crt_strtol(s.as_ptr(), &mut end, 10), i32::MAX);
        assert_eq!(get_errno(), ERANGE);
        assert_eq!(end as usize - s.as_ptr() as usize, 10);
        set_errno(0);
        assert_eq!(
            crt_strtol(b"-2147483648\0".as_ptr(), ptr::null_mut(), 10),
            i32::MIN
        );
        assert_eq!(get_errno(), 0);
        assert_eq!(
            crt_strtol(b"-2147483649\0".as_ptr(), ptr::null_mut(), 10),
            i32::MIN
        );
        assert_eq!(get_errno(), ERANGE);
        set_errno(0);
        assert_eq!(
            crt_strtoll(b"99999999999999999999999\0".as_ptr(), ptr::null_mut(), 10),
            i64::MAX
        );
        assert_eq!(get_errno(), ERANGE);
        set_errno(0);
        assert_eq!(
            crt_strtoll(b"-9223372036854775808\0".as_ptr(), ptr::null_mut(), 10),
            i64::MIN
        );
        assert_eq!(get_errno(), 0);
    }

    #[test]
    fn strtoul_negation_and_overflow() {
        set_errno(0);
        assert_eq!(crt_strtoul(b"-1\0".as_ptr(), ptr::null_mut(), 10), u32::MAX);
        assert_eq!(get_errno(), 0);
        assert_eq!(
            crt_strtoul(b"4294967296\0".as_ptr(), ptr::null_mut(), 10),
            u32::MAX
        );
        assert_eq!(get_errno(), ERANGE);
        set_errno(0);
        assert_eq!(
            crt_strtoull(b"0xFFFFFFFFFFFFFFFF\0".as_ptr(), ptr::null_mut(), 0),
            u64::MAX
        );
        assert_eq!(get_errno(), 0);
        assert_eq!(
            crt_strtoull(b"0x10000000000000000\0".as_ptr(), ptr::null_mut(), 0),
            u64::MAX
        );
        assert_eq!(get_errno(), ERANGE);
    }

    #[test]
    fn wide_integer_conversions() {
        let s = w("\u{3000} 0x7fZ");
        let mut end = ptr::null();
        assert_eq!(crt_wcstol(s.as_ptr(), &mut end, 0), 127);
        assert_eq!(end as usize - s.as_ptr() as usize, 6 * 2);
        assert_eq!(crt_wtoi(w("-42").as_ptr()), -42);
        assert_eq!(crt_wtoi64(w("9000000000").as_ptr()), 9_000_000_000);
        assert_eq!(
            crt_wcstoull(w("-1").as_ptr(), ptr::null_mut(), 10),
            u64::MAX
        );
        assert_eq!(crt_atol(b"  77\0".as_ptr()), 77);
        assert_eq!(crt_atoll(b"-9000000000\0".as_ptr()), -9_000_000_000);
    }

    #[test]
    fn strtod_decimal_hex_special() {
        let s = b"  3.25e2xyz\0";
        let mut end = ptr::null();
        assert_eq!(crt_strtod(s.as_ptr(), &mut end), 325.0);
        assert_eq!(end as usize - s.as_ptr() as usize, 8);
        assert_eq!(crt_strtod(b".5\0".as_ptr(), ptr::null_mut()), 0.5);
        let s = b"5.e\0";
        assert_eq!(crt_strtod(s.as_ptr(), &mut end), 5.0);
        assert_eq!(end as usize - s.as_ptr() as usize, 2);
        assert_eq!(crt_strtod(b"0x1.8p3\0".as_ptr(), ptr::null_mut()), 12.0);
        assert_eq!(crt_strtod(b"-0X.8\0".as_ptr(), ptr::null_mut()), -0.5);
        let s = b"0xz\0";
        assert_eq!(crt_strtod(s.as_ptr(), &mut end), 0.0);
        assert_eq!(end as usize - s.as_ptr() as usize, 1);
        let s = b"-INFINITY!\0";
        assert_eq!(crt_strtod(s.as_ptr(), &mut end), f64::NEG_INFINITY);
        assert_eq!(end as usize - s.as_ptr() as usize, 9);
        let s = b"nan(ind)x\0";
        assert!(crt_strtod(s.as_ptr(), &mut end).is_nan());
        assert_eq!(end as usize - s.as_ptr() as usize, 8);
        let s = b"abc\0";
        assert_eq!(crt_strtod(s.as_ptr(), &mut end), 0.0);
        assert_eq!(end, s.as_ptr());
        assert_eq!(crt_atof(b"1e-3\0".as_ptr()), 1e-3);
        assert_eq!(crt_strtod(b"0.1\0".as_ptr(), ptr::null_mut()), 0.1);
    }

    #[test]
    fn strtod_range_errors() {
        set_errno(0);
        assert_eq!(
            crt_strtod(b"1e400\0".as_ptr(), ptr::null_mut()),
            f64::INFINITY
        );
        assert_eq!(get_errno(), ERANGE);
        set_errno(0);
        assert_eq!(crt_strtod(b"1e-400\0".as_ptr(), ptr::null_mut()), 0.0);
        assert_eq!(get_errno(), ERANGE);
        set_errno(0);
        assert_eq!(crt_strtod(b"0e-400\0".as_ptr(), ptr::null_mut()), 0.0);
        assert_eq!(get_errno(), 0);
        assert_eq!(
            crt_strtof(b"1e39\0".as_ptr(), ptr::null_mut()),
            f32::INFINITY
        );
        assert_eq!(get_errno(), ERANGE);
        set_errno(0);
        assert_eq!(
            crt_strtof(b"0x1p-149\0".as_ptr(), ptr::null_mut()),
            f32::from_bits(1)
        );
        assert_eq!(get_errno(), 0);
        assert_eq!(
            crt_strtod(b"0x1p-1074\0".as_ptr(), ptr::null_mut()),
            f64::from_bits(1)
        );
        assert_eq!(
            crt_strtod(b"0x1.fffffffffffff8p1023\0".as_ptr(), ptr::null_mut()),
            f64::INFINITY
        );
        assert_eq!(get_errno(), ERANGE);
        assert_eq!(crt_strtof(b"0.1\0".as_ptr(), ptr::null_mut()), 0.1f32);
        assert_eq!(crt_wcstod(w(" 2.5").as_ptr(), ptr::null_mut()), 2.5);
        assert_eq!(crt_wcstof(w("0x10").as_ptr(), ptr::null_mut()), 16.0);
        assert_eq!(crt_wtof(w("-1.5").as_ptr()), -1.5);
    }

    #[test]
    fn hex_float_rounding_ties_to_even() {
        // 1 + 2^-53 : égalité exacte → pair (1.0) ; avec bit collant → au-dessus.
        assert_eq!(
            crt_strtod(b"0x1.00000000000008p0\0".as_ptr(), ptr::null_mut()),
            1.0
        );
        assert_eq!(
            crt_strtod(b"0x1.000000000000080001p0\0".as_ptr(), ptr::null_mut()),
            1.0 + f64::EPSILON
        );
        assert_eq!(
            crt_strtod(b"0x1.00000000000018p0\0".as_ptr(), ptr::null_mut()),
            1.0 + 2.0 * f64::EPSILON
        );
    }

    #[test]
    fn itoa_family() {
        let mut buf = [0u8; 40];
        crt_itoa(-255, buf.as_mut_ptr(), 10);
        assert_eq!(cstr(&buf), "-255");
        crt_itoa(-1, buf.as_mut_ptr(), 16);
        assert_eq!(cstr(&buf), "ffffffff");
        crt_i64toa(i64::MIN, buf.as_mut_ptr(), 10);
        assert_eq!(cstr(&buf), "-9223372036854775808");
        crt_ui64toa(u64::MAX, buf.as_mut_ptr(), 36);
        assert_eq!(cstr(&buf), "3w5e11264sgsf");
        crt_ultoa(5, buf.as_mut_ptr(), 2);
        assert_eq!(cstr(&buf), "101");
        let mut wb = [0u16; 20];
        crt_itow(-12, wb.as_mut_ptr(), 10);
        assert_eq!(&wb[..4], &w("-12")[..]);
        crt_i64tow(255, wb.as_mut_ptr(), 16);
        assert_eq!(&wb[..3], &w("ff")[..]);

        // _s : tampon trop petit → vidé + ERANGE ; base invalide → EINVAL.
        let mut small = [b'x'; 3];
        assert_eq!(crt_itoa_s(-12, small.as_mut_ptr(), 3, 10), ERANGE);
        assert_eq!(small[0], 0);
        assert_eq!(get_errno(), ERANGE);
        assert_eq!(crt_itoa_s(12, small.as_mut_ptr(), 3, 10), 0);
        assert_eq!(cstr(&small), "12");
        assert_eq!(crt_itoa_s(12, small.as_mut_ptr(), 3, 1), EINVAL);
        assert_eq!(small[0], 0);
        assert_eq!(crt_itoa_s(12, ptr::null_mut(), 3, 10), EINVAL);
    }

    #[test]
    fn ctype_masks_match_msvc() {
        assert_eq!(crt_isalpha(b'A' as i32), 0x101);
        assert_eq!(crt_isalpha(b'z' as i32), 0x102);
        assert_eq!(crt_isdigit(b'7' as i32), 4);
        assert_eq!(crt_isxdigit(b'f' as i32), 0x80);
        assert_eq!(crt_isspace(b'\t' as i32), 8);
        assert_eq!(crt_isspace(b'x' as i32), 0);
        assert_ne!(crt_isprint(b' ' as i32), 0);
        assert_eq!(crt_isgraph(b' ' as i32), 0);
        assert_ne!(crt_ispunct(b'!' as i32), 0);
        assert_ne!(crt_iscntrl(0x7F), 0);
        assert_eq!(crt_isalpha(0xE9), 0);
        assert_eq!(crt_isalpha(-1), 0);
        assert_ne!(crt_isalnum(b'5' as i32), 0);
        assert_ne!(crt_islower(b'q' as i32), 0);
        assert_eq!(crt_isupper(b'q' as i32), 0);
        assert_eq!(crt_tolower(b'Q' as i32), b'q' as i32);
        assert_eq!(crt_toupper(b'q' as i32), b'Q' as i32);
        assert_eq!(crt_toupper(-1), -1);
        assert_eq!(crt_towlower(b'A' as u16), b'a' as u16);
        assert_eq!(crt_towupper(0xE9), 0xE9);
        assert_ne!(crt_iswspace(0x3000), 0);
        assert_ne!(crt_iswalpha(0xE9), 0);
        assert_ne!(crt_iswdigit(b'3' as u16), 0);
        assert_eq!(crt_iswdigit(b'a' as u16), 0);
        assert_ne!(crt_isblank(b' ' as i32), 0);
    }

    #[test]
    fn compare_functions() {
        assert_eq!(crt_strcmp(b"abc\0".as_ptr(), b"abd\0".as_ptr()), -1);
        assert_eq!(crt_strcmp(b"abc\0".as_ptr(), b"abc\0".as_ptr()), 0);
        assert_eq!(crt_strcmp(b"b\0".as_ptr(), b"a\0".as_ptr()), 1);
        assert_eq!(crt_stricmp(b"HeLLo\0".as_ptr(), b"hello\0".as_ptr()), 0);
        assert!(crt_stricmp(b"A\0".as_ptr(), b"b\0".as_ptr()) < 0);
        assert_eq!(crt_strnicmp(b"ABCx\0".as_ptr(), b"abcy\0".as_ptr(), 3), 0);
        set_errno(0);
        assert_eq!(crt_stricmp(ptr::null(), b"a\0".as_ptr()), NLSCMPERROR);
        assert_eq!(get_errno(), EINVAL);
        assert_eq!(crt_wcscmp(w("a").as_ptr(), w("b").as_ptr()), -1);
        assert_eq!(crt_wcsncmp(w("abX").as_ptr(), w("abY").as_ptr(), 2), 0);
        assert!(crt_wcsncmp(w("abX").as_ptr(), w("abY").as_ptr(), 3) < 0);
        assert_eq!(crt_wcsicmp(w("ÉtÉ").as_ptr(), w("ÉtÉ").as_ptr()), 0);
        assert_eq!(crt_wcsicmp(w("ABC").as_ptr(), w("abc").as_ptr()), 0);
        assert_eq!(crt_wcsnicmp(w("ABCd").as_ptr(), w("abce").as_ptr(), 3), 0);
    }

    #[test]
    fn copy_and_concat_unsafe_variants() {
        let mut buf = [0xAAu8; 16];
        crt_strcpy(buf.as_mut_ptr(), b"foo\0".as_ptr());
        crt_strcat(buf.as_mut_ptr(), b"bar\0".as_ptr());
        assert_eq!(cstr(&buf), "foobar");
        crt_strncat(buf.as_mut_ptr(), b"bazqux\0".as_ptr(), 3);
        assert_eq!(cstr(&buf), "foobarbaz");
        let mut b2 = [0xAAu8; 6];
        crt_strncpy(b2.as_mut_ptr(), b"ab\0".as_ptr(), 5);
        assert_eq!(b2, [b'a', b'b', 0, 0, 0, 0xAA]);
        assert_eq!(crt_strlen(b"hello\0".as_ptr()), 5);
        assert_eq!(crt_strnlen(b"hello\0".as_ptr(), 3), 3);
        let mut wb = [0u16; 8];
        crt_wcsncpy(wb.as_mut_ptr(), w("xy").as_ptr(), 4);
        assert_eq!(&wb[..4], &[b'x' as u16, b'y' as u16, 0, 0]);
        assert_eq!(crt_wcsnlen(w("abc").as_ptr(), 10), 3);
    }

    #[test]
    fn secure_copy_functions() {
        let mut buf = [b'x'; 4];
        assert_eq!(crt_strcpy_s(buf.as_mut_ptr(), 4, b"abc\0".as_ptr()), 0);
        assert_eq!(cstr(&buf), "abc");
        set_errno(0);
        assert_eq!(
            crt_strcpy_s(buf.as_mut_ptr(), 4, b"abcd\0".as_ptr()),
            ERANGE
        );
        assert_eq!(buf[0], 0);
        assert_eq!(get_errno(), ERANGE);
        assert_eq!(crt_strcpy_s(buf.as_mut_ptr(), 4, ptr::null()), EINVAL);
        assert_eq!(crt_strcpy_s(ptr::null_mut(), 4, b"a\0".as_ptr()), EINVAL);
        assert_eq!(crt_strcpy_s(buf.as_mut_ptr(), 0, b"a\0".as_ptr()), EINVAL);

        let mut buf = [0u8; 6];
        assert_eq!(crt_strcpy_s(buf.as_mut_ptr(), 6, b"ab\0".as_ptr()), 0);
        assert_eq!(crt_strcat_s(buf.as_mut_ptr(), 6, b"cde\0".as_ptr()), 0);
        assert_eq!(cstr(&buf), "abcde");
        assert_eq!(crt_strcat_s(buf.as_mut_ptr(), 6, b"f\0".as_ptr()), ERANGE);
        assert_eq!(buf[0], 0);
        let mut unterminated = [b'z'; 3];
        assert_eq!(
            crt_strcat_s(unterminated.as_mut_ptr(), 3, b"a\0".as_ptr()),
            EINVAL
        );
        assert_eq!(unterminated[0], 0);

        // strncpy_s
        let mut buf = [b'x'; 5];
        assert_eq!(
            crt_strncpy_s(buf.as_mut_ptr(), 5, b"abcdef\0".as_ptr(), 3),
            0
        );
        assert_eq!(cstr(&buf), "abc");
        assert_eq!(
            crt_strncpy_s(buf.as_mut_ptr(), 5, b"abcdef\0".as_ptr(), 5),
            ERANGE
        );
        assert_eq!(buf[0], 0);
        assert_eq!(
            crt_strncpy_s(buf.as_mut_ptr(), 5, b"abcdef\0".as_ptr(), TRUNCATE),
            STRUNCATE
        );
        assert_eq!(cstr(&buf), "abcd");
        assert_eq!(crt_strncpy_s(buf.as_mut_ptr(), 5, b"ab\0".as_ptr(), 0), 0);
        assert_eq!(buf[0], 0);
        assert_eq!(crt_strncpy_s(ptr::null_mut(), 0, ptr::null(), 0), 0);

        // strncat_s
        let mut buf = [0u8; 6];
        assert_eq!(crt_strcpy_s(buf.as_mut_ptr(), 6, b"ab\0".as_ptr()), 0);
        assert_eq!(crt_strncat_s(buf.as_mut_ptr(), 6, b"cdef\0".as_ptr(), 2), 0);
        assert_eq!(cstr(&buf), "abcd");
        assert_eq!(
            crt_strncat_s(buf.as_mut_ptr(), 6, b"xyz\0".as_ptr(), TRUNCATE),
            STRUNCATE
        );
        assert_eq!(cstr(&buf), "abcdx");
        assert_eq!(
            crt_strncat_s(buf.as_mut_ptr(), 6, b"q\0".as_ptr(), 1),
            ERANGE
        );
        assert_eq!(buf[0], 0);

        // Variantes larges.
        let mut wb = [0u16; 4];
        assert_eq!(crt_wcscpy_s(wb.as_mut_ptr(), 4, w("ab").as_ptr()), 0);
        assert_eq!(crt_wcscat_s(wb.as_mut_ptr(), 4, w("c").as_ptr()), 0);
        assert_eq!(&wb, &w("abc")[..]);
        assert_eq!(crt_wcscat_s(wb.as_mut_ptr(), 4, w("d").as_ptr()), ERANGE);
        assert_eq!(wb[0], 0);
        assert_eq!(
            crt_wcsncpy_s(wb.as_mut_ptr(), 4, w("hello").as_ptr(), TRUNCATE),
            STRUNCATE
        );
        assert_eq!(&wb, &w("hel")[..]);
        assert_eq!(crt_wcsncpy_s(wb.as_mut_ptr(), 4, w("hello").as_ptr(), 2), 0);
        assert_eq!(&wb[..3], &w("he")[..]);
    }

    #[test]
    fn search_functions() {
        let s = b"hello world\0";
        assert_eq!(crt_strchr(s.as_ptr(), b'o' as i32), s[4..].as_ptr());
        assert_eq!(crt_strrchr(s.as_ptr(), b'o' as i32), s[7..].as_ptr());
        assert_eq!(crt_strchr(s.as_ptr(), 0), s[11..].as_ptr());
        assert!(crt_strchr(s.as_ptr(), b'z' as i32).is_null());
        assert_eq!(crt_strpbrk(s.as_ptr(), b"wr\0".as_ptr()), s[6..].as_ptr());
        assert!(crt_strpbrk(s.as_ptr(), b"zq\0".as_ptr()).is_null());
        assert_eq!(crt_strspn(s.as_ptr(), b"leh\0".as_ptr()), 4);
        assert_eq!(crt_strcspn(s.as_ptr(), b" \0".as_ptr()), 5);
        let p = crt_memchr(s.as_ptr().cast(), b'w' as i32, 11);
        assert_eq!(p.cast::<u8>(), s[6..].as_ptr());
        assert!(crt_memchr(s.as_ptr().cast(), b'w' as i32, 5).is_null());
        let ws = w("a/b/c");
        assert_eq!(crt_wcschr(ws.as_ptr(), b'/' as u16), ws[1..].as_ptr());
        assert_eq!(crt_wcsrchr(ws.as_ptr(), b'/' as u16), ws[3..].as_ptr());
        assert!(crt_wcschr(ws.as_ptr(), b'x' as u16).is_null());
    }

    #[test]
    fn strtok_multi_calls_and_thread_state() {
        let mut s = *b",,a,b;;c,\0";
        let d = b",;\0";
        let t1 = crt_strtok(s.as_mut_ptr(), d.as_ptr());
        assert_eq!(t1, s[2..].as_mut_ptr());
        let t2 = crt_strtok(ptr::null_mut(), d.as_ptr());
        assert_eq!(t2, s[4..].as_mut_ptr());
        let t3 = crt_strtok(ptr::null_mut(), d.as_ptr());
        assert_eq!(t3, s[7..].as_mut_ptr());
        assert!(crt_strtok(ptr::null_mut(), d.as_ptr()).is_null());
        assert!(crt_strtok(ptr::null_mut(), d.as_ptr()).is_null());
        assert_eq!(&s, b",,a\0b\0;c\0\0");

        // Un autre thread n'hérite pas du contexte.
        let mut s = *b"x y\0";
        crt_strtok(s.as_mut_ptr(), b" \0".as_ptr());
        std::thread::spawn(|| {
            assert!(crt_strtok(ptr::null_mut(), b" \0".as_ptr()).is_null());
        })
        .join()
        .expect("thread de test");

        // strtok_s
        let mut s = *b"k=v&k2=v2\0";
        let mut ctx = ptr::null_mut();
        let t = crt_strtok_s(s.as_mut_ptr(), b"&\0".as_ptr(), &mut ctx);
        assert_eq!(t, s.as_mut_ptr());
        let t = crt_strtok_s(ptr::null_mut(), b"&\0".as_ptr(), &mut ctx);
        assert_eq!(t, s[4..].as_mut_ptr());
        assert!(crt_strtok_s(ptr::null_mut(), b"&\0".as_ptr(), &mut ctx).is_null());
        set_errno(0);
        let mut nctx = ptr::null_mut();
        assert!(crt_strtok_s(ptr::null_mut(), b"&\0".as_ptr(), &mut nctx).is_null());
        assert_eq!(get_errno(), EINVAL);

        let mut ws = w("p q");
        let mut wctx = ptr::null_mut();
        let sep = w(" ");
        assert_eq!(
            crt_wcstok(ws.as_mut_ptr(), sep.as_ptr(), &mut wctx),
            ws.as_mut_ptr()
        );
        let t = crt_wcstok(ptr::null_mut(), sep.as_ptr(), &mut wctx);
        assert_eq!(t, ws[2..].as_mut_ptr());
    }

    #[test]
    fn strdup_uses_libc_heap() {
        let p = crt_strdup(b"dup\0".as_ptr());
        assert!(!p.is_null());
        // SAFETY: `p` alloué par `_strdup`, 4 octets.
        unsafe {
            assert_eq!(std::slice::from_raw_parts(p, 4), b"dup\0");
            libc::free(p.cast());
        }
        assert!(crt_strdup(ptr::null()).is_null());
        let src = w("wide");
        let q = crt_wcsdup(src.as_ptr());
        // SAFETY: `q` alloué par `_wcsdup`, 5 unités.
        unsafe {
            assert_eq!(std::slice::from_raw_parts(q, 5), &src[..]);
            libc::free(q.cast());
        }
    }

    unsafe extern "win64" fn cmp_i32(a: *const c_void, b: *const c_void) -> i32 {
        // SAFETY: le test ne passe que des pointeurs vers des i32.
        let (a, b) = unsafe { (*a.cast::<i32>(), *b.cast::<i32>()) };
        a.cmp(&b) as i32
    }

    unsafe extern "win64" fn cmp_i32_ctx(
        ctx: *mut c_void,
        a: *const c_void,
        b: *const c_void,
    ) -> i32 {
        // SAFETY: `ctx` pointe vers un compteur u32 du test.
        unsafe { *ctx.cast::<u32>() += 1 };
        // SAFETY: pointeurs vers des i32.
        unsafe { cmp_i32(a, b) }
    }

    #[test]
    fn bsearch_qsort_s_and_math() {
        let arr = [1i32, 3, 5, 7, 9, 11];
        for (i, v) in arr.iter().enumerate() {
            let p = crt_bsearch(
                ptr::from_ref(v).cast(),
                arr.as_ptr().cast(),
                arr.len(),
                4,
                Some(cmp_i32),
            );
            assert_eq!(p.cast::<i32>().cast_const(), &raw const arr[i]);
        }
        let k = 4i32;
        assert!(
            crt_bsearch(
                ptr::from_ref(&k).cast(),
                arr.as_ptr().cast(),
                6,
                4,
                Some(cmp_i32)
            )
            .is_null()
        );
        set_errno(0);
        assert!(crt_bsearch(ptr::from_ref(&k).cast(), arr.as_ptr().cast(), 6, 4, None).is_null());
        assert_eq!(get_errno(), EINVAL);

        let mut v: Vec<i32> = vec![5, -3, 9, 0, 9, 2, -7, 1, 8, 3];
        let mut calls = 0u32;
        crt_qsort_s(
            v.as_mut_ptr().cast(),
            v.len(),
            4,
            Some(cmp_i32_ctx),
            ptr::from_mut(&mut calls).cast(),
        );
        assert_eq!(v, vec![-7, -3, 0, 1, 2, 3, 5, 8, 9, 9]);
        assert!(calls > 0);
        let mut calls2 = 0u32;
        let key = 8i32;
        let p = crt_bsearch_s(
            ptr::from_ref(&key).cast(),
            v.as_ptr().cast(),
            v.len(),
            4,
            Some(cmp_i32_ctx),
            ptr::from_mut(&mut calls2).cast(),
        );
        assert_eq!(p.cast::<i32>().cast_const(), &raw const v[7]);

        assert_eq!(crt_abs(-5), 5);
        assert_eq!(crt_abs(i32::MIN), i32::MIN);
        assert_eq!(crt_llabs(-5_000_000_000), 5_000_000_000);
        assert_eq!(crt_div(-7, 2), DivT { quot: -3, rem: -1 });
        assert_eq!(
            crt_div(i32::MIN, -1),
            DivT {
                quot: i32::MIN,
                rem: 0
            }
        );
        assert_eq!(crt_lldiv(7, -2), LLDivT { quot: -3, rem: 1 });
        assert_eq!(crt_div(1, 0), DivT::default());
    }

    #[test]
    fn exports_cover_requested_symbols() {
        let e = exports();
        for name in [
            "_itoa",
            "_strtoi64",
            "_strtoui64",
            "atof",
            "atoll",
            "strtod",
            "strtof",
            "strtol",
            "strtoll",
            "strtoul",
            "strtoull",
            "wcstol",
            "wcstoul",
            "wcstoll",
            "wcstoull",
            "wcstod",
            "wcstof",
            "_wtoi",
            "_wtoi64",
            "_atoi64",
            "_i64toa",
            "_ui64toa",
            "_ultoa",
            "_ltoa",
            "_itow",
            "_i64tow",
            "_wcstoi64",
            "_wcstoui64",
            "atol",
            "_stricmp",
            "_strnicmp",
            "_wcsicoll",
            "_wcsnicmp",
            "_wcsicmp",
            "_strdup",
            "_wcsdup",
            "isalnum",
            "isalpha",
            "isdigit",
            "islower",
            "isupper",
            "isspace",
            "isxdigit",
            "isprint",
            "ispunct",
            "iscntrl",
            "isgraph",
            "tolower",
            "toupper",
            "towlower",
            "towupper",
            "iswspace",
            "iswdigit",
            "iswalpha",
            "strcat",
            "strcat_s",
            "strcmp",
            "strcpy",
            "strcpy_s",
            "strcspn",
            "strspn",
            "strlen",
            "strnlen",
            "strncat",
            "strncat_s",
            "strncpy",
            "strncpy_s",
            "strpbrk",
            "strtok",
            "strtok_s",
            "wcscat_s",
            "wcscpy_s",
            "wcsncpy_s",
            "wcscmp",
            "wcsncmp",
            "wcscoll",
            "strcoll",
            "wcsncpy",
            "wcschr",
            "wcsrchr",
            "wcsnlen",
            "strchr",
            "strrchr",
            "memchr",
            "bsearch",
            "qsort_s",
            "abs",
            "labs",
            "llabs",
            "div",
        ] {
            assert!(e.contains_key(name), "export manquant : {name}");
        }
    }
}
