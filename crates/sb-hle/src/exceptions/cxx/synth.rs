//! Exceptions `std::exception` synthétiques levées par le runtime HLE (`std::_Xbad_alloc`,
//! `_Xlength_error`, `_Xout_of_range`, `_Xruntime_error`, `_Throw_C_error`, ...).
//!
//! Une région exécutable unique (`Region`) contient les `TypeDescriptor`, vtables
//! (`[destructeur scalaire, what]`), `CatchableType[Array]`, `ThrowInfo` et deux petits thunks
//! (les RVA de `ThrowInfo` / `CatchableType` doivent tenir sur 32 bits, les fonctions Rust
//! hôtes sont donc jointes via `mov rax, imm64 ; jmp rax` placés dans la région). L'ImageBase
//! publié dans l'enregistrement d'exception est la base de la région.
//!
//! Disposition de l'objet (MSVC `std::exception`, 24 octets utiles, 64 alloués) :
//! `+0 vptr`, `+8 _What (const char *)`, `+16 _DoFree (bool)`. La hiérarchie des types
//! (`runtime_error` → `exception`, `system_error` → `runtime_error`, ...) est exposée via les
//! `CatchableType` : `catch (const std::exception &)` et `catch (const std::runtime_error &)`
//! fonctionnent ; les champs additionnels (`error_code`) restent à zéro.

use core::ffi::{CStr, c_void};
use std::collections::HashMap;
use std::sync::OnceLock;

use sb_common::ffi_guard;
use sb_loader::unwind::Context;

use super::{CHANNEL, CXX_EXCEPTION_CODE, CXX_MAGIC_BASE};
use crate::exceptions::{EXCEPTION_NONCONTINUABLE, ExceptionRecord, capture_thunk, raise_internal};

const OBJECT_ALLOC: usize = 64;
const OBJECT_SIZE: i32 = 24;
const REGION_SIZE: usize = 0x4000;

/// `(nom décoré RTTI, parent)` ; l'indice est l'identifiant de type.
const KINDS: &[(&[u8], Option<usize>)] = &[
    (b".?AVexception@std@@\0", None),            // 0
    (b".?AVbad_alloc@std@@\0", Some(0)),         // 1
    (b".?AVbad_function_call@std@@\0", Some(0)), // 2
    (b".?AVlogic_error@std@@\0", Some(0)),       // 3
    (b".?AVinvalid_argument@std@@\0", Some(3)),  // 4
    (b".?AVlength_error@std@@\0", Some(3)),      // 5
    (b".?AVout_of_range@std@@\0", Some(3)),      // 6
    (b".?AVruntime_error@std@@\0", Some(0)),     // 7
    (b".?AVsystem_error@std@@\0", Some(7)),      // 8
    (b".?AVbad_cast@std@@\0", Some(0)),          // 9
    (b".?AVfailure@ios_base@std@@\0", Some(8)),  // 10
];

pub(crate) const K_BAD_ALLOC: usize = 1;
pub(crate) const K_BAD_FUNCTION_CALL: usize = 2;
pub(crate) const K_INVALID_ARGUMENT: usize = 4;
pub(crate) const K_LENGTH_ERROR: usize = 5;
pub(crate) const K_OUT_OF_RANGE: usize = 6;
pub(crate) const K_RUNTIME_ERROR: usize = 7;
pub(crate) const K_SYSTEM_ERROR: usize = 8;

struct Region {
    base: usize,
    vtables: Vec<usize>,
    throw_infos: Vec<usize>,
}

static REGION: OnceLock<Region> = OnceLock::new();

/// Base de la région si `addr` s'y trouve.
pub(super) fn base_containing(addr: usize) -> Option<usize> {
    REGION
        .get()
        .filter(|r| addr >= r.base && addr < r.base + REGION_SIZE)
        .map(|r| r.base)
}

// ----- fonctions de l'objet -----

extern "win64" fn synth_what(this: *const u8) -> *const i8 {
    // SAFETY: `this` est un objet synthétique (ou un `std::exception` MSVC de même disposition).
    let p = unsafe { *(this.add(8) as *const *const i8) };
    if p.is_null() {
        c"Unknown exception".as_ptr()
    } else {
        p
    }
}

/// Libère le message si nécessaire (sans libérer l'objet).
///
/// # Safety
/// `this` désigne un objet synthétique valide.
unsafe fn free_what(this: *mut u8) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        let what = *(this.add(8) as *const *mut c_void);
        if *this.add(16) != 0 && !what.is_null() {
            libc::free(what);
        }
        *(this.add(8) as *mut usize) = 0;
        *this.add(16) = 0;
    }
}

/// Destructeur scalaire virtuel (`flags & 1` : libère l'objet).
extern "win64" fn synth_dtor(this: *mut u8, flags: u32) -> *mut u8 {
    // SAFETY: objet synthétique alloué par `throw_kind`.
    unsafe {
        free_what(this);
        if flags & 1 != 0 {
            libc::free(this.cast());
        }
    }
    this
}

/// `ThrowInfo.pmfnUnwind` : détruit puis libère l'objet synthétique.
extern "win64" fn synth_destroy(obj: *mut u8) {
    // SAFETY: objet synthétique propriétaire de sa mémoire.
    unsafe {
        free_what(obj);
        libc::free(obj.cast());
    }
}

/// `CatchableType.copyFunction(dest, src, has_virtual_base)`.
extern "win64" fn synth_copy(dest: *mut u8, src: *const u8, _vb: u32) {
    // SAFETY: `dest` dispose de `OBJECT_SIZE` octets ; `src` est un objet synthétique.
    unsafe {
        core::ptr::copy_nonoverlapping(src, dest, OBJECT_SIZE as usize);
        let what = *(src.add(8) as *const *const i8);
        if *src.add(16) != 0 && !what.is_null() {
            *(dest.add(8) as *mut *mut i8) = libc::strdup(what);
        }
    }
}

fn region() -> &'static Region {
    REGION.get_or_init(build_region)
}

fn build_region() -> Region {
    // SAFETY: mapping anonyme RWX privé, dédié à ce module.
    let p = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            REGION_SIZE,
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(p, libc::MAP_FAILED, "mmap de la région RTTI synthétique");
    let base = p as usize;
    let mut off = 0usize;
    let put = |off: usize, bytes: &[u8]| {
        // SAFETY: écriture bornée par `REGION_SIZE` (vérifiée par l'assertion finale).
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), (base + off) as *mut u8, bytes.len())
        };
    };
    let thunk = |target: usize| {
        let mut t = vec![0x48, 0xB8];
        t.extend_from_slice(&(target as u64).to_le_bytes());
        t.extend_from_slice(&[0xFF, 0xE0]);
        t
    };
    let destroy_thunk = off;
    put(off, &thunk(synth_destroy as *const () as usize));
    off += 16;
    let copy_thunk = off;
    put(off, &thunk(synth_copy as *const () as usize));
    off += 16;

    // TypeDescriptors
    let mut td = Vec::new();
    for (name, _) in KINDS {
        td.push(off);
        put(off + 16, name);
        off += (16 + name.len() + 7) & !7;
    }
    // vtables
    let mut vtables = Vec::new();
    for _ in KINDS {
        vtables.push(base + off);
        put(
            off,
            &(synth_dtor as *const () as usize as u64).to_le_bytes(),
        );
        put(
            off + 8,
            &(synth_what as *const () as usize as u64).to_le_bytes(),
        );
        off += 16;
    }
    // CatchableTypes + arrays + ThrowInfo
    let mut throw_infos = Vec::new();
    for k in 0..KINDS.len() {
        let mut chain = Vec::new();
        let mut cur = Some(k);
        while let Some(c) = cur {
            chain.push(c);
            cur = KINDS[c].1;
        }
        let mut cts = Vec::new();
        for &c in &chain {
            let mut ct = [0u8; 28];
            ct[4..8].copy_from_slice(&(td[c] as i32).to_le_bytes());
            ct[12..16].copy_from_slice(&(-1i32).to_le_bytes()); // PMD.pdisp = -1 (pas de vbase)
            ct[20..24].copy_from_slice(&OBJECT_SIZE.to_le_bytes());
            ct[24..28].copy_from_slice(&(copy_thunk as i32).to_le_bytes());
            put(off, &ct);
            cts.push(off as i32);
            off += 32;
        }
        let array = off;
        put(off, &(cts.len() as i32).to_le_bytes());
        for (i, c) in cts.iter().enumerate() {
            put(off + 4 + i * 4, &c.to_le_bytes());
        }
        off += (4 + cts.len() * 4 + 7) & !7;
        let mut ti = [0u8; 16];
        ti[4..8].copy_from_slice(&(destroy_thunk as i32).to_le_bytes());
        ti[12..16].copy_from_slice(&(array as i32).to_le_bytes());
        put(off, &ti);
        throw_infos.push(base + off);
        off += 16;
    }
    assert!(off <= REGION_SIZE, "région RTTI synthétique trop petite");
    Region {
        base,
        vtables,
        throw_infos,
    }
}

/// Lève une exception `std::` de type `kind` avec le message `msg` (copié ; `NULL` accepté) depuis
/// le contexte invité `ctx`.
///
/// # Safety
/// `ctx` est le contexte capturé de l'appelant invité ; `msg` est nul ou une chaîne C valide.
pub(crate) unsafe fn throw_kind(kind: usize, msg: *const i8, ctx: &mut Context) -> ! {
    let r = region();
    // SAFETY: allocation et initialisation d'un objet de `OBJECT_ALLOC` octets.
    let obj = unsafe { libc::calloc(1, OBJECT_ALLOC) }.cast::<u8>();
    // SAFETY: `obj` vient d'être alloué (non nul sauf OOM, géré ci-dessous).
    unsafe {
        if !obj.is_null() {
            *(obj as *mut usize) = r.vtables[kind];
            if !msg.is_null() {
                *(obj.add(8) as *mut *mut i8) = libc::strdup(msg);
                *obj.add(16) = 1;
            }
        }
    }
    let mut rec = ExceptionRecord::new(CXX_EXCEPTION_CODE, EXCEPTION_NONCONTINUABLE);
    rec.number_parameters = 4;
    rec.exception_information[..4].copy_from_slice(&[
        CXX_MAGIC_BASE,
        obj as usize,
        r.throw_infos[kind],
        r.base,
    ]);
    rec.exception_address = ctx.rip as usize;
    // SAFETY: contrat de l'appelant.
    unsafe { raise_internal(&raw mut rec, ctx) }
}

// ----- points d'entrée (thunks de capture de contexte) -----

macro_rules! thunk_throw {
    ($(#[$m:meta])* $name:ident, $imp:ident, $kind:expr, with_msg) => {
        capture_thunk! { $(#[$m])* pub(crate) fn $name(msg: *const i8) => $imp }
        extern "win64" fn $imp(msg: *const i8, _b: u64, _c: u64, _d: u64, ctx: *mut Context) {
            ffi_guard!(CHANNEL, stringify!($name), (), {
                // SAFETY: contexte capturé par le thunk.
                unsafe { throw_kind($kind, msg, &mut *ctx) }
            });
        }
    };
    ($(#[$m:meta])* $name:ident, $imp:ident, $kind:expr, $text:expr) => {
        capture_thunk! { $(#[$m])* pub(crate) fn $name() => $imp }
        extern "win64" fn $imp(_a: u64, _b: u64, _c: u64, _d: u64, ctx: *mut Context) {
            ffi_guard!(CHANNEL, stringify!($name), (), {
                // SAFETY: contexte capturé par le thunk.
                unsafe { throw_kind($kind, $text.as_ptr(), &mut *ctx) }
            });
        }
    };
}

thunk_throw!(
    /// `std::_Xbad_alloc()`.
    x_bad_alloc,
    x_bad_alloc_impl,
    K_BAD_ALLOC,
    c"bad allocation"
);
thunk_throw!(
    /// `std::_Xbad_function_call()`.
    x_bad_function_call,
    x_bad_function_call_impl,
    K_BAD_FUNCTION_CALL,
    c"bad function call"
);
thunk_throw!(
    /// `std::_Xinvalid_argument(const char *)`.
    x_invalid_argument,
    x_invalid_argument_impl,
    K_INVALID_ARGUMENT,
    with_msg
);
thunk_throw!(
    /// `std::_Xlength_error(const char *)`.
    x_length_error,
    x_length_error_impl,
    K_LENGTH_ERROR,
    with_msg
);
thunk_throw!(
    /// `std::_Xout_of_range(const char *)`.
    x_out_of_range,
    x_out_of_range_impl,
    K_OUT_OF_RANGE,
    with_msg
);
thunk_throw!(
    /// `std::_Xruntime_error(const char *)`.
    x_runtime_error,
    x_runtime_error_impl,
    K_RUNTIME_ERROR,
    with_msg
);

capture_thunk! {
    /// `std::_Throw_C_error(int)` : codes `_Thrd_result` → `std::system_error`.
    pub(crate) fn throw_c_error(code: i32) => throw_c_error_impl
}

extern "win64" fn throw_c_error_impl(code: i32, _b: u64, _c: u64, _d: u64, ctx: *mut Context) {
    ffi_guard!(CHANNEL, "_Throw_C_error", (), {
        let what: &CStr = match code {
            1 | 2 => c"resource unavailable try again",
            3 => c"device or resource busy",
            4 => c"invalid argument",
            _ => c"unknown error",
        };
        // SAFETY: contexte capturé par le thunk.
        unsafe { throw_kind(K_SYSTEM_ERROR, what.as_ptr(), &mut *ctx) }
    });
}

// ----- __std_exception_copy / __std_exception_destroy -----

/// `__std_exception_copy(const __std_exception_data *from, __std_exception_data *to)`.
extern "win64" fn std_exception_copy(from: *const u8, to: *mut u8) {
    ffi_guard!(CHANNEL, "__std_exception_copy", (), {
        if from.is_null() || to.is_null() {
            return;
        }
        // SAFETY: `__std_exception_data` = `{ const char *_What; bool _DoFree; }`.
        unsafe {
            let what = *(from as *const *const i8);
            let do_free = *from.add(8) != 0;
            if do_free && !what.is_null() {
                *(to as *mut *mut i8) = libc::strdup(what);
                *to.add(8) = 1;
            } else {
                *(to as *mut *const i8) = what;
                *to.add(8) = 0;
            }
        }
    })
}

/// `__std_exception_destroy(__std_exception_data *)`.
extern "win64" fn std_exception_destroy(data: *mut u8) {
    ffi_guard!(CHANNEL, "__std_exception_destroy", (), {
        if data.is_null() {
            return;
        }
        // SAFETY: idem.
        unsafe {
            let what = *(data as *const *mut c_void);
            if *data.add(8) != 0 && !what.is_null() {
                libc::free(what);
            }
            *(data as *mut usize) = 0;
            *data.add(8) = 0;
        }
    })
}

pub(crate) fn register_crt(m: &mut HashMap<&'static str, usize>) {
    m.insert(
        "__std_exception_copy",
        std_exception_copy as *const () as usize,
    );
    m.insert(
        "__std_exception_destroy",
        std_exception_destroy as *const () as usize,
    );
}

pub(crate) fn register_msvcp(m: &mut HashMap<&'static str, usize>) {
    macro_rules! reg {
        ($name:expr, $f:expr) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("?_Xbad_alloc@std@@YAXXZ", x_bad_alloc);
    reg!("?_Xbad_function_call@std@@YAXXZ", x_bad_function_call);
    reg!("?_Xinvalid_argument@std@@YAXPEBD@Z", x_invalid_argument);
    reg!("?_Xlength_error@std@@YAXPEBD@Z", x_length_error);
    reg!("?_Xout_of_range@std@@YAXPEBD@Z", x_out_of_range);
    reg!("?_Xruntime_error@std@@YAXPEBD@Z", x_runtime_error);
    reg!("?_Throw_C_error@std@@YAXH@Z", throw_c_error);
}

/// Construit un objet synthétique et retourne son `what()` (tests).
#[cfg(test)]
pub(crate) fn what_of_new(kind: usize, msg: &CStr) -> String {
    let r = region();
    // SAFETY: objet de test alloué puis libéré ici.
    unsafe {
        let obj = libc::calloc(1, OBJECT_ALLOC).cast::<u8>();
        *(obj as *mut usize) = r.vtables[kind];
        *(obj.add(8) as *mut *mut i8) = libc::strdup(msg.as_ptr());
        *obj.add(16) = 1;
        // `what()` via la vtable, comme le ferait l'invité.
        let vt = *(obj as *const *const usize);
        let what: extern "win64" fn(*const u8) -> *const i8 = core::mem::transmute(*vt.add(1));
        let s = CStr::from_ptr(what(obj)).to_string_lossy().into_owned();
        synth_destroy(obj);
        s
    }
}
