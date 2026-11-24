//! `std::exception_ptr` (`msvcp140.dll` : `__ExceptionPtr*`).
//!
//! `exception_ptr` fait 16 octets (`shared_ptr`) : mot 0 = pointeur vers un [`ExcPtrData`]
//! comptant ses références (0 : vide), mot 1 = copie du mot 0 (non interprété). Les données
//! contiennent une copie de l'`EXCEPTION_RECORD` et de l'objet lancé (copié avec la
//! `copyFunction` du premier `CatchableType`, sinon `memcpy`). `rethrow` relance une **nouvelle
//! copie** de l'objet : l'exception relancée est détruite à la fin de son catch sans toucher
//! l'exemplaire partagé (la mémoire de la copie, sans libération par `pmfnUnwind`, est perdue :
//! fuite bornée à la taille de l'objet).

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use sb_common::ffi_guard;
use sb_loader::unwind::Context;

use super::desc::rd_i32;
use super::{CHANNEL, CxxParams, cxx_params, read_ct, terminate_now};
use crate::exceptions::{EXCEPTION_NONCONTINUABLE, ExceptionRecord, capture_thunk, raise_internal};

struct ExcPtrData {
    refs: AtomicUsize,
    rec: ExceptionRecord,
    object: usize,
    size: usize,
}

/// Copie l'objet décrit par `p` dans `dest` (au moins `size` octets).
///
/// # Safety
/// `p` décrit une exception C++ dont l'objet est vivant ; `dest` est inscriptible.
unsafe fn copy_object(p: &CxxParams, dest: usize) -> usize {
    // SAFETY: tables `ThrowInfo` valides (contrat de l'appelant).
    unsafe {
        let array = p.base + rd_i32(p.throw_info + 12) as usize;
        if rd_i32(array) < 1 {
            return 0;
        }
        let ct = read_ct(p.base + rd_i32(array + 4) as usize);
        let size = ct.size.max(0) as usize;
        if dest != 0 {
            if ct.copy_fn != 0 {
                let f: unsafe extern "win64" fn(usize, usize, u32) =
                    core::mem::transmute::<usize, _>(p.base + ct.copy_fn as usize);
                f(dest, p.object, u32::from(ct.props & 4 != 0));
            } else {
                core::ptr::copy_nonoverlapping(p.object as *const u8, dest as *mut u8, size);
            }
        }
        size
    }
}

/// Crée les données partagées pour l'exception `rec` (0 si ce n'est pas une exception C++).
///
/// # Safety
/// `rec` est un `EXCEPTION_RECORD` valide dont l'objet est vivant.
unsafe fn make_data(rec: &ExceptionRecord) -> usize {
    let Some(p) = cxx_params(rec) else {
        // Exception SEH non C++ : enregistrement seul.
        let d = Box::new(ExcPtrData {
            refs: AtomicUsize::new(1),
            rec: *rec,
            object: 0,
            size: 0,
        });
        return Box::into_raw(d) as usize;
    };
    // SAFETY: contrat de l'appelant.
    unsafe {
        let size = copy_object(&p, 0);
        let obj = libc::calloc(1, size.max(1)) as usize;
        copy_object(&p, obj);
        let mut r = *rec;
        r.exception_information[1] = obj;
        let d = Box::new(ExcPtrData {
            refs: AtomicUsize::new(1),
            rec: r,
            object: obj,
            size,
        });
        Box::into_raw(d) as usize
    }
}

/// # Safety
/// `data` est nul ou un pointeur issu de `make_data`.
unsafe fn addref(data: usize) {
    if data != 0 {
        // SAFETY: contrat de l'appelant.
        unsafe { &*(data as *const ExcPtrData) }
            .refs
            .fetch_add(1, Ordering::AcqRel);
    }
}

/// # Safety
/// `data` est nul ou un pointeur issu de `make_data` détenant une référence.
unsafe fn release(data: usize) {
    if data == 0 {
        return;
    }
    // SAFETY: contrat de l'appelant.
    unsafe {
        let d = &*(data as *const ExcPtrData);
        if d.refs.fetch_sub(1, Ordering::AcqRel) == 1 {
            let b = Box::from_raw(data as *mut ExcPtrData);
            if let Some(p) = cxx_params(&b.rec) {
                super::destroy_thrown_object(&p);
            }
            if b.object != 0 {
                libc::free(b.object as *mut c_void);
            }
        }
    }
}

fn slot(ep: *mut usize) -> usize {
    // SAFETY: `ep` désigne un `exception_ptr` invité (16 octets).
    unsafe { *ep }
}

fn set_slot(ep: *mut usize, data: usize) {
    // SAFETY: idem.
    unsafe {
        *ep = data;
        *ep.add(1) = data;
    }
}

extern "win64" fn ptr_create(ep: *mut usize) {
    ffi_guard!(CHANNEL, "__ExceptionPtrCreate", (), {
        if !ep.is_null() {
            set_slot(ep, 0);
        }
    })
}

extern "win64" fn ptr_destroy(ep: *mut usize) {
    ffi_guard!(CHANNEL, "__ExceptionPtrDestroy", (), {
        if !ep.is_null() {
            // SAFETY: donnée détenue par cet `exception_ptr`.
            unsafe { release(slot(ep)) };
            set_slot(ep, 0);
        }
    })
}

extern "win64" fn ptr_copy(dst: *mut usize, src: *const usize) {
    ffi_guard!(CHANNEL, "__ExceptionPtrCopy", (), {
        if dst.is_null() || src.is_null() {
            return;
        }
        // SAFETY: `src` désigne un `exception_ptr` valide.
        let d = unsafe { *src };
        // SAFETY: référence prise pour la nouvelle copie.
        unsafe { addref(d) };
        set_slot(dst, d);
    })
}

extern "win64" fn ptr_assign(dst: *mut usize, src: *const usize) {
    ffi_guard!(CHANNEL, "__ExceptionPtrAssign", (), {
        if dst.is_null() || src.is_null() {
            return;
        }
        // SAFETY: `src` valide.
        let d = unsafe { *src };
        // SAFETY: nouvelle référence puis libération de l'ancienne.
        unsafe {
            addref(d);
            release(slot(dst));
        }
        set_slot(dst, d);
    })
}

extern "win64" fn ptr_swap(a: *mut usize, b: *mut usize) {
    ffi_guard!(CHANNEL, "__ExceptionPtrSwap", (), {
        if a.is_null() || b.is_null() {
            return;
        }
        let (x, y) = (slot(a), slot(b));
        set_slot(a, y);
        set_slot(b, x);
    })
}

extern "win64" fn ptr_to_bool(ep: *const usize) -> u8 {
    ffi_guard!(CHANNEL, "__ExceptionPtrToBool", 0, {
        u8::from(!ep.is_null() && slot(ep as *mut usize) != 0)
    })
}

extern "win64" fn ptr_compare(a: *const usize, b: *const usize) -> u8 {
    ffi_guard!(CHANNEL, "__ExceptionPtrCompare", 0, {
        if a.is_null() || b.is_null() {
            return 0;
        }
        u8::from(slot(a as *mut usize) == slot(b as *mut usize))
    })
}

extern "win64" fn ptr_copy_exception(
    ep: *mut usize,
    rec: *const ExceptionRecord,
    _ctx: *const c_void,
) {
    ffi_guard!(CHANNEL, "__ExceptionPtrCopyException", (), {
        if ep.is_null() {
            return;
        }
        // SAFETY: détruit l'éventuelle valeur précédente (objet déjà construit par l'appelant).
        unsafe { release(slot(ep)) };
        let data = if rec.is_null() {
            0
        } else {
            // SAFETY: `rec` valide, objet vivant pendant l'appel.
            unsafe { make_data(&*rec) }
        };
        set_slot(ep, data);
    })
}

extern "win64" fn ptr_current_exception(ep: *mut usize) {
    ffi_guard!(CHANNEL, "__ExceptionPtrCurrentException", (), {
        if ep.is_null() {
            return;
        }
        let cur = super::CUR_EXC.with(core::cell::Cell::get) as *const ExceptionRecord;
        let data = if cur.is_null() {
            0
        } else {
            // SAFETY: l'enregistrement courant vit pendant le bloc catch.
            unsafe { make_data(&*cur) }
        };
        set_slot(ep, data);
    })
}

capture_thunk! {
    /// `std::rethrow_exception` : `__ExceptionPtrRethrow(const exception_ptr *)`.
    fn ptr_rethrow(ep: *const usize) => ptr_rethrow_impl
}

extern "win64" fn ptr_rethrow_impl(ep: *const usize, _b: u64, _c: u64, _d: u64, ctx: *mut Context) {
    ffi_guard!(CHANNEL, "__ExceptionPtrRethrow", (), {
        let data = if ep.is_null() {
            0
        } else {
            slot(ep as *mut usize)
        };
        if data == 0 {
            terminate_now();
        }
        // SAFETY: `data` pointe sur des `ExcPtrData` détenues par `ep`.
        unsafe {
            let d = &*(data as *const ExcPtrData);
            let mut rec = d.rec;
            if let Some(p) = cxx_params(&rec) {
                let size = copy_object(&p, 0);
                let obj = libc::calloc(1, size.max(1)) as usize;
                let shared = CxxParams {
                    object: d.object,
                    ..p
                };
                copy_object(&shared, obj);
                rec.exception_information[1] = obj;
            }
            rec.exception_flags = EXCEPTION_NONCONTINUABLE;
            rec.exception_address = (*ctx).rip as usize;
            let _ = d.size;
            raise_internal(&raw mut rec, ctx)
        }
    });
}

pub(super) fn register(m: &mut HashMap<&'static str, usize>) {
    macro_rules! reg {
        ($name:expr, $f:expr) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("?__ExceptionPtrCreate@@YAXPEAX@Z", ptr_create);
    reg!("?__ExceptionPtrDestroy@@YAXPEAX@Z", ptr_destroy);
    reg!("?__ExceptionPtrCopy@@YAXPEAXPEBX@Z", ptr_copy);
    reg!("?__ExceptionPtrAssign@@YAXPEAXPEBX@Z", ptr_assign);
    reg!("?__ExceptionPtrSwap@@YAXPEAX0@Z", ptr_swap);
    reg!("?__ExceptionPtrToBool@@YA_NPEBX@Z", ptr_to_bool);
    reg!("?__ExceptionPtrCompare@@YA_NPEBX0@Z", ptr_compare);
    reg!(
        "?__ExceptionPtrCurrentException@@YAXPEAX@Z",
        ptr_current_exception
    );
    reg!(
        "?__ExceptionPtrCopyException@@YAXPEAXPEBX1@Z",
        ptr_copy_exception
    );
    reg!("?__ExceptionPtrRethrow@@YAXPEBX@Z", ptr_rethrow);
}
