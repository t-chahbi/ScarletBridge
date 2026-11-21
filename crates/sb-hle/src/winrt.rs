//! Implémentations HLE Rust (`extern "win64"`) pour :
//! - WinRT (`HSTRING`, `RoInitialize`, `RoGetActivationFactory`, `CoTaskMem*`),
//! - `vccorlib140_app.dll` (runtime C++/CX Xbox/UWP connecté à `winrt_objects`),
//! - `msvcp140_app.dll` (mutex, condvar, `std::call_once`).

#![allow(non_snake_case, unused_variables)]

use core::ffi::c_void;
use std::collections::HashMap;
use std::ptr;
use std::sync::{Arc, OnceLock};

use parking_lot::{Condvar, Mutex};
use sb_common::abi::{E_NOTIMPL, E_POINTER, E_UNEXPECTED, HResult, S_OK};
use sb_common::handle::{HandleTable, OpaqueHandle};
use sb_common::wide::{from_pcwstr, to_wide_null};
use sb_common::{ffi_guard, sb_debug, sb_err, sb_info, sb_stub};

use crate::com::Guid;
use crate::process::{ReentrantCs, process_state};
use crate::winrt_objects;

const CHANNEL: &str = "winrt";

/// Représentation interne d'un `HSTRING` WinRT (UTF-16LE immuable + nul terminal).
struct HStringData {
    units: Vec<u16>,
}

static HSTRINGS: OnceLock<HandleTable<HStringData>> = OnceLock::new();

fn hstrings() -> &'static HandleTable<HStringData> {
    HSTRINGS.get_or_init(HandleTable::new)
}

/// Crée un `HSTRING` WinRT depuis une `&str` Rust UTF-8.
pub fn create_hstring(s: &str) -> u64 {
    if s.is_empty() {
        return 0;
    }
    let units = to_wide_null(s);
    hstrings().insert(HStringData { units }).as_raw()
}

pub fn winrt_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }

    reg!("RoInitialize", RoInitialize);
    reg!("RoUninitialize", RoUninitialize);
    reg!("RoGetActivationFactory", RoGetActivationFactory);
    reg!("RoFailFastWithErrorContext", RoFailFastWithErrorContext);
    reg!("RoReportUnhandledError", RoReportUnhandledError);
    reg!("SetRestrictedErrorInfo", SetRestrictedErrorInfo);
    reg!("WindowsCreateString", WindowsCreateString);
    reg!("WindowsDeleteString", WindowsDeleteString);
    reg!("WindowsDuplicateString", WindowsDuplicateString);
    reg!("WindowsGetStringRawBuffer", WindowsGetStringRawBuffer);
    reg!("CoTaskMemAlloc", CoTaskMemAlloc);
    reg!("CoTaskMemFree", CoTaskMemFree);
    reg!(
        "CoCreateFreeThreadedMarshaler",
        CoCreateFreeThreadedMarshaler
    );
    m
}

pub fn vccorlib_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }

    reg!(
        "?InitializeData@Details@Platform@@YAJH@Z",
        vccorlib_init_data
    );
    reg!(
        "?UninitializeData@Details@Platform@@YAXH@Z",
        vccorlib_uninit_data
    );
    reg!(
        "?Allocate@Heap@Details@Platform@@SAPEAX_K0@Z",
        vccorlib_heap_allocate
    );
    reg!(
        "?AllocateException@Heap@Details@Platform@@SAPEAX_K0@Z",
        vccorlib_heap_allocate
    );
    reg!("?Free@Heap@Details@Platform@@SAXPEAX@Z", vccorlib_heap_free);
    reg!(
        "?AlignedFree@Heap@Details@Platform@@SAXPEAX@Z",
        vccorlib_heap_free
    );
    reg!(
        "?GetCmdArguments@Details@Platform@@YAPEAPEA_WPEAH@Z",
        vccorlib_get_cmd_arguments
    );
    reg!(
        "?GetActivationFactoryByPCWSTR@@YAJPEAXAEAVGuid@Platform@@PEAPEAX@Z",
        vccorlib_get_activation_factory_by_pcwstr
    );
    reg!(
        "?GetIBoxArrayVtable@Details@Platform@@YAPEAXPEAX@Z",
        vccorlib_get_ibox_array_vtable
    );
    reg!(
        "?GetIidsFn@@YAJHPEAKPEBU__s_GUID@@PEAPEAVGuid@Platform@@@Z",
        vccorlib_get_iids_fn
    );
    reg!("??0Object@Platform@@QE$AAA@XZ", vccorlib_object_ctor);
    reg!("??0Delegate@Platform@@QE$AAA@XZ", vccorlib_object_ctor);
    reg!(
        "?ReleaseTarget@ControlBlock@Details@Platform@@AEAAXXZ",
        vccorlib_release_target
    );
    reg!(
        "?__abi_WinRTraiseCOMException@@YAXJ@Z",
        vccorlib_raise_com_exception
    );
    reg!(
        "?__abi_WinRTraiseNotImplementedException@@YAXXZ",
        vccorlib_raise_not_impl
    );
    reg!(
        "?__abi_WinRTraiseClassNotRegisteredException@@YAXXZ",
        vccorlib_raise_class_not_reg
    );
    reg!(
        "?__abi_WinRTraiseFailureException@@YAXXZ",
        vccorlib_raise_failure
    );
    reg!(
        "?__abi_WinRTraiseNullReferenceException@@YAXXZ",
        vccorlib_raise_null_ref
    );
    reg!(
        "?__abi_WinRTraiseInvalidArgumentException@@YAXXZ",
        vccorlib_raise_invalid_arg
    );
    reg!(
        "?__abi_WinRTraiseInvalidCastException@@YAXXZ",
        vccorlib_raise_invalid_cast
    );
    m
}

pub fn msvcp_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }

    reg!("_Mtx_init_in_situ", msvcp_mtx_init);
    reg!("_Mtx_destroy_in_situ", msvcp_mtx_destroy);
    reg!("_Mtx_lock", msvcp_mtx_lock);
    reg!("_Mtx_unlock", msvcp_mtx_unlock);
    reg!("_Cnd_init_in_situ", msvcp_cnd_init);
    reg!("_Cnd_destroy_in_situ", msvcp_cnd_destroy);
    reg!("_Cnd_wait", msvcp_cnd_wait);
    reg!("_Cnd_broadcast", msvcp_cnd_broadcast);
    reg!(
        "?_Execute_once@std@@YAHAEAUonce_flag@1@P6AHPEAX1PEAPEAX@Z1@Z",
        msvcp_execute_once
    );
    reg!(
        "?GetCurrentThreadId@platform@details@Concurrency@@YAJXZ",
        msvcp_get_current_tid
    );
    m
}

// ---------- WinRT Core & HSTRING ----------

extern "win64" fn RoInitialize(init_type: u32) -> HResult {
    ffi_guard!(CHANNEL, "RoInitialize", E_UNEXPECTED, {
        sb_info!(CHANNEL, "RoInitialize(type={init_type}) -> S_OK");
        S_OK
    })
}

extern "win64" fn RoUninitialize() {
    sb_debug!(CHANNEL, "RoUninitialize()");
}

extern "win64" fn RoGetActivationFactory(
    activatable_class_id: u64,
    iid: *const Guid,
    out_factory: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "RoGetActivationFactory", E_UNEXPECTED, {
        let class_name = read_hstring(activatable_class_id).unwrap_or_else(|| "<null>".to_string());
        winrt_objects::resolve_activation_factory(&class_name, iid, out_factory)
    })
}

extern "win64" fn RoFailFastWithErrorContext(hr: HResult) -> ! {
    sb_err!(
        CHANNEL,
        "RoFailFastWithErrorContext(hr={hr:#010x} / {})",
        sb_common::abi::hresult_name(hr)
    );
    std::process::exit(1);
}

extern "win64" fn RoReportUnhandledError(_info: *mut c_void) -> HResult {
    sb_stub!(CHANNEL, "RoReportUnhandledError", "info={_info:p}");
    S_OK
}

extern "win64" fn SetRestrictedErrorInfo(_info: *mut c_void) -> HResult {
    S_OK
}

extern "win64" fn WindowsCreateString(src: *const u16, len: u32, out_string: *mut u64) -> HResult {
    ffi_guard!(CHANNEL, "WindowsCreateString", E_UNEXPECTED, {
        if out_string.is_null() {
            return E_POINTER;
        }
        if len == 0 || src.is_null() {
            // SAFETY: `out_string` vérifié non-nul.
            unsafe {
                *out_string = 0;
            }
            return S_OK;
        }
        // SAFETY: `src` contient `len` unités UTF-16.
        let slice = unsafe { std::slice::from_raw_parts(src, len as usize) };
        let mut units = Vec::with_capacity(slice.len() + 1);
        units.extend_from_slice(slice);
        units.push(0);
        let handle = hstrings().insert(HStringData { units });
        // SAFETY: `out_string` vérifié non-nul.
        unsafe {
            *out_string = handle.as_raw();
        }
        S_OK
    })
}

extern "win64" fn WindowsDeleteString(hstr: u64) -> HResult {
    ffi_guard!(CHANNEL, "WindowsDeleteString", E_UNEXPECTED, {
        if hstr != 0 {
            let _ = hstrings().remove(OpaqueHandle::from_raw(hstr));
        }
        S_OK
    })
}

extern "win64" fn WindowsDuplicateString(hstr: u64, out_new: *mut u64) -> HResult {
    ffi_guard!(CHANNEL, "WindowsDuplicateString", E_UNEXPECTED, {
        if out_new.is_null() {
            return E_POINTER;
        }
        if hstr == 0 {
            // SAFETY: `out_new` vérifié non-nul.
            unsafe {
                *out_new = 0;
            }
            return S_OK;
        }
        let cloned = hstrings().with(OpaqueHandle::from_raw(hstr), |d| d.units.clone());
        if let Some(units) = cloned {
            let h = hstrings().insert(HStringData { units });
            // SAFETY: `out_new` vérifié non-nul.
            unsafe {
                *out_new = h.as_raw();
            }
        } else {
            unsafe {
                *out_new = 0;
            }
        }
        S_OK
    })
}

extern "win64" fn WindowsGetStringRawBuffer(hstr: u64, out_len: *mut u32) -> *const u16 {
    static EMPTY: u16 = 0;
    ffi_guard!(CHANNEL, "WindowsGetStringRawBuffer", &raw const EMPTY, {
        if hstr == 0 {
            if !out_len.is_null() {
                // SAFETY: `out_len` vérifié non-nul.
                unsafe {
                    *out_len = 0;
                }
            }
            return &raw const EMPTY;
        }
        let res = hstrings().with(OpaqueHandle::from_raw(hstr), |d| {
            let char_len = d.units.len().saturating_sub(1) as u32;
            (d.units.as_ptr(), char_len)
        });
        match res {
            Some((ptr_val, len)) => {
                if !out_len.is_null() {
                    // SAFETY: `out_len` vérifié non-nul.
                    unsafe {
                        *out_len = len;
                    }
                }
                ptr_val
            }
            None => &raw const EMPTY,
        }
    })
}

fn read_hstring(hstr: u64) -> Option<String> {
    if hstr == 0 {
        return Some(String::new());
    }
    hstrings().with(OpaqueHandle::from_raw(hstr), |d| {
        let content = &d.units[..d.units.len().saturating_sub(1)];
        String::from_utf16_lossy(content)
    })
}

extern "win64" fn CoTaskMemAlloc(bytes: usize) -> *mut c_void {
    // SAFETY: allocation heap standard compatible avec `CoTaskMemFree`.
    unsafe { libc::malloc(bytes.max(1)) }.cast::<c_void>()
}

extern "win64" fn CoTaskMemFree(ptr_val: *mut c_void) {
    if !ptr_val.is_null() {
        // SAFETY: libération d'un pointeur issu de `CoTaskMemAlloc`.
        unsafe {
            libc::free(ptr_val.cast::<libc::c_void>());
        }
    }
}

extern "win64" fn CoCreateFreeThreadedMarshaler(
    _outer: *mut c_void,
    out_marshaler: *mut *mut c_void,
) -> HResult {
    if !out_marshaler.is_null() {
        // SAFETY: `out_marshaler` vérifié non-nul.
        unsafe {
            *out_marshaler = ptr::null_mut();
        }
    }
    S_OK
}

// ---------- vccorlib140_app (C++/CX) ----------

extern "win64" fn vccorlib_init_data(flags: i32) -> HResult {
    ffi_guard!("vccorlib", "InitializeData", E_UNEXPECTED, {
        sb_info!(
            "vccorlib",
            "Platform::Details::InitializeData(flags={flags}) -> S_OK"
        );
        S_OK
    })
}

extern "win64" fn vccorlib_uninit_data(flags: i32) {
    sb_info!(
        "vccorlib",
        "Platform::Details::UninitializeData(flags={flags})"
    );
}

/// `Platform::Details::Heap::Allocate(size_t alignment, size_t size)` — MSVC C++/CX passe
/// l'alignement en 1er argument (`rcx`) et la taille en octets en 2e argument (`rdx`) !
extern "win64" fn vccorlib_heap_allocate(_alignment: usize, size: usize) -> *mut c_void {
    // SAFETY: allocation zero-initialized pour objets C++/CX (`size` en 2e argument).
    unsafe { libc::calloc(1, size.max(16)) }.cast::<c_void>()
}

extern "win64" fn vccorlib_heap_free(ptr_val: *mut c_void) {
    if !ptr_val.is_null() {
        // SAFETY: libération d'un pointeur issu de `vccorlib_heap_allocate`.
        unsafe {
            libc::free(ptr_val.cast::<libc::c_void>());
        }
    }
}

extern "win64" fn vccorlib_get_cmd_arguments(out_argc: *mut i32) -> *mut *mut u16 {
    ffi_guard!("vccorlib", "GetCmdArguments", ptr::null_mut(), {
        let mut st = process_state().write();
        if !out_argc.is_null() {
            // SAFETY: `out_argc` vérifié non-nul.
            unsafe {
                *out_argc = st.argc;
            }
        }
        st.wargv_ptrs.as_mut_ptr()
    })
}

extern "win64" fn vccorlib_get_activation_factory_by_pcwstr(
    class_name_ptr: *const u16,
    iid: *const Guid,
    out_factory: *mut *mut c_void,
) -> HResult {
    ffi_guard!("vccorlib", "GetActivationFactoryByPCWSTR", E_UNEXPECTED, {
        // SAFETY: `class_name_ptr` est un `PCWSTR` UTF-16LE.
        let class_name = unsafe { from_pcwstr(class_name_ptr) }.unwrap_or_else(|| "<null>".into());
        winrt_objects::resolve_activation_factory(&class_name, iid, out_factory)
    })
}

static IBOX_ARRAY_VTBL: [usize; 8] = [0; 8];

extern "win64" fn vccorlib_get_ibox_array_vtable(_arg: *mut c_void) -> *const c_void {
    (&raw const IBOX_ARRAY_VTBL).cast()
}

extern "win64" fn vccorlib_get_iids_fn(
    count: i32,
    out_count: *mut u32,
    src_iids: *const Guid,
    out_iids: *mut *mut Guid,
) -> HResult {
    ffi_guard!("vccorlib", "GetIidsFn", E_UNEXPECTED, {
        let n = count.max(0) as usize;
        if !out_count.is_null() {
            // SAFETY: `out_count` non-nul.
            unsafe {
                *out_count = n as u32;
            }
        }
        if !out_iids.is_null() {
            if n > 0 && !src_iids.is_null() {
                let bytes = n * std::mem::size_of::<Guid>();
                // SAFETY: allocation COM compatible `CoTaskMemFree`.
                let dst = unsafe { libc::malloc(bytes) }.cast::<Guid>();
                if !dst.is_null() {
                    unsafe {
                        ptr::copy_nonoverlapping(src_iids, dst, n);
                        *out_iids = dst;
                    }
                }
            } else {
                // SAFETY: `out_iids` non-nul.
                unsafe {
                    *out_iids = ptr::null_mut();
                }
            }
        }
        S_OK
    })
}

extern "win64" fn vccorlib_object_ctor(this: *mut c_void) -> *mut c_void {
    this
}

extern "win64" fn vccorlib_release_target(_this: *mut c_void) {}

extern "win64" fn vccorlib_raise_com_exception(hr: HResult) -> ! {
    sb_err!(
        "vccorlib",
        "__abi_WinRTraiseCOMException(hr={hr:#010x} / {})",
        sb_common::abi::hresult_name(hr)
    );
    std::process::exit(2);
}

extern "win64" fn vccorlib_raise_not_impl() -> ! {
    vccorlib_raise_com_exception(E_NOTIMPL);
}
extern "win64" fn vccorlib_raise_class_not_reg() -> ! {
    vccorlib_raise_com_exception(0x8004_0154_u32 as i32);
}
extern "win64" fn vccorlib_raise_failure() -> ! {
    vccorlib_raise_com_exception(sb_common::abi::E_FAIL);
}
extern "win64" fn vccorlib_raise_null_ref() -> ! {
    vccorlib_raise_com_exception(E_POINTER);
}
extern "win64" fn vccorlib_raise_invalid_arg() -> ! {
    vccorlib_raise_com_exception(sb_common::abi::E_INVALIDARG);
}
extern "win64" fn vccorlib_raise_invalid_cast() -> ! {
    vccorlib_raise_com_exception(sb_common::abi::E_NOINTERFACE);
}

// ---------- msvcp140_app (STL sync) ----------

fn msvcp_mtx(ptr_val: usize) -> Arc<ReentrantCs> {
    let mut st = process_state().write();
    Arc::clone(
        st.crit_sections
            .entry(ptr_val)
            .or_insert_with(|| Arc::new(ReentrantCs::new())),
    )
}

extern "win64" fn msvcp_mtx_init(mtx: *mut c_void, _kind: i32) {
    if !mtx.is_null() {
        let _ = msvcp_mtx(mtx as usize);
    }
}

extern "win64" fn msvcp_mtx_destroy(mtx: *mut c_void) {
    if !mtx.is_null() {
        process_state()
            .write()
            .crit_sections
            .remove(&(mtx as usize));
    }
}

extern "win64" fn msvcp_mtx_lock(mtx: *mut c_void) -> i32 {
    if !mtx.is_null() {
        msvcp_mtx(mtx as usize).lock();
    }
    0
}

extern "win64" fn msvcp_mtx_unlock(mtx: *mut c_void) -> i32 {
    if !mtx.is_null() {
        msvcp_mtx(mtx as usize).unlock();
    }
    0
}

extern "win64" fn msvcp_cnd_init(cnd: *mut c_void) {
    if !cnd.is_null() {
        process_state()
            .write()
            .condvars
            .entry(cnd as usize)
            .or_insert_with(|| Arc::new(Condvar::new()));
    }
}

extern "win64" fn msvcp_cnd_destroy(cnd: *mut c_void) {
    if !cnd.is_null() {
        process_state().write().condvars.remove(&(cnd as usize));
    }
}

extern "win64" fn msvcp_cnd_wait(cnd: *mut c_void, mtx: *mut c_void) -> i32 {
    let cond = {
        let mut st = process_state().write();
        Arc::clone(
            st.condvars
                .entry(cnd as usize)
                .or_insert_with(|| Arc::new(Condvar::new())),
        )
    };
    let m = msvcp_mtx(mtx as usize);
    m.unlock();
    let dummy = Mutex::new(());
    let mut g = dummy.lock();
    cond.wait(&mut g);
    drop(g);
    m.lock();
    0
}

extern "win64" fn msvcp_cnd_broadcast(cnd: *mut c_void) -> i32 {
    if let Some(cond) = process_state()
        .read()
        .condvars
        .get(&(cnd as usize))
        .cloned()
    {
        cond.notify_all();
    }
    0
}

extern "win64" fn msvcp_execute_once(
    flag: *mut usize,
    callback: Option<unsafe extern "win64" fn(*mut usize, *mut c_void, *mut *mut c_void) -> i32>,
    param: *mut c_void,
) -> i32 {
    ffi_guard!("msvcp", "_Execute_once", 0, {
        if flag.is_null() {
            return 0;
        }
        // SAFETY: `std::once_flag` stocke un `usize`.
        if unsafe { *flag } == 0 {
            if let Some(cb) = callback {
                let mut ctx = ptr::null_mut();
                // SAFETY: callback `std::call_once` Win64.
                let ok = unsafe { cb(flag, param, &mut ctx) };
                if ok != 0 {
                    unsafe {
                        *flag = 1;
                    }
                }
                return ok;
            }
            unsafe {
                *flag = 1;
            }
        }
        1
    })
}

extern "win64" fn msvcp_get_current_tid() -> i32 {
    // SAFETY: syscall `gettid` Linux.
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hstring_create_read_duplicate_delete() {
        let wide = to_wide_null("Windows.Gaming.Input.Gamepad");
        let mut h: u64 = 0;
        let hr = WindowsCreateString(wide.as_ptr(), (wide.len() - 1) as u32, &mut h);
        assert_eq!(hr, S_OK);
        assert_ne!(h, 0);

        let mut out_len = 0u32;
        let raw = WindowsGetStringRawBuffer(h, &mut out_len);
        assert_eq!(out_len as usize, "Windows.Gaming.Input.Gamepad".len());
        // SAFETY: `raw` est une chaîne UTF-16LE terminée par nul.
        let decoded = unsafe { from_pcwstr(raw) };
        assert_eq!(decoded.as_deref(), Some("Windows.Gaming.Input.Gamepad"));

        let mut dup = 0u64;
        assert_eq!(WindowsDuplicateString(h, &mut dup), S_OK);
        assert_ne!(dup, 0);
        assert_ne!(dup, h);

        assert_eq!(WindowsDeleteString(h), S_OK);
        assert_eq!(WindowsDeleteString(dup), S_OK);
    }
}
