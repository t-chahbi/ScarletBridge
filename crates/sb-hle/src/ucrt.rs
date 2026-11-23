//! Implémentations HLE Rust (`extern "win64"`) pour UCRT (`api-ms-win-crt-*`, `ucrtbase.dll`)
//! et `vcruntime140*.dll`.

#![allow(non_snake_case, unused_variables)]

use core::ffi::c_void;
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};

use parking_lot::Mutex;
use sb_common::{ffi_guard, sb_debug, sb_err, sb_info};

use crate::crt_str::{EINVAL, ERANGE};
use crate::process::{ReentrantCs, process_state};

const CHANNEL: &str = "ucrt";
static RAND_SEED: AtomicU32 = AtomicU32::new(1);

pub fn exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }

    // Heap & mémoire
    reg!("malloc", crt_malloc);
    reg!("calloc", crt_calloc);
    reg!("realloc", crt_realloc);
    reg!("free", crt_free);
    reg!("_set_new_mode", _set_new_mode);
    reg!("_callnewh", _callnewh);

    // Chaînes & mémoire bas niveau (vcruntime140 + crt-string)
    reg!("memcpy", crt_memcpy);
    reg!("memmove", crt_memmove);
    reg!("memset", crt_memset);
    reg!("memcmp", crt_memcmp);
    reg!("strncmp", crt_strncmp);
    reg!("strstr", crt_strstr);
    reg!("wcsstr", crt_wcsstr);
    reg!("wcslen", crt_wcslen);
    reg!("atoi", crt_atoi);
    reg!("wcstombs", crt_wcstombs);

    // Démarrage & cycle de vie CRT
    reg!("_initterm", _initterm);
    reg!("_initterm_e", _initterm_e);
    reg!("_crt_atexit", _crt_atexit);
    reg!("_initialize_onexit_table", _initialize_onexit_table);
    reg!("_register_onexit_function", _register_onexit_function);
    reg!(
        "_register_thread_local_exe_atexit_callback",
        _register_thread_local_exe_atexit_callback
    );
    reg!("_configure_narrow_argv", _configure_narrow_argv);
    reg!(
        "_initialize_narrow_environment",
        _initialize_narrow_environment
    );
    reg!(
        "_get_narrow_winmain_command_line",
        _get_narrow_winmain_command_line
    );
    reg!("__p___argc", __p___argc);
    reg!("__p___argv", __p___argv);
    reg!("__p__environ", __p__environ);
    reg!("__p__commode", __p__commode);
    reg!("__p__fmode", __p__fmode);
    reg!("_set_app_type", _set_app_type);
    reg!("_set_fmode", _set_fmode);
    reg!("_configthreadlocale", _configthreadlocale);
    reg!("__setusermatherr", __setusermatherr);
    reg!(
        "_set_invalid_parameter_handler",
        _set_invalid_parameter_handler
    );
    reg!(
        "_invalid_parameter_noinfo_noreturn",
        _invalid_parameter_noinfo_noreturn
    );
    reg!("_invalid_parameter_noinfo", _invalid_parameter_noinfo);
    reg!("_invoke_watson", _invoke_watson);
    reg!(
        "_get_invalid_parameter_handler",
        _get_invalid_parameter_handler
    );
    reg!(
        "_set_thread_local_invalid_parameter_handler",
        _set_thread_local_invalid_parameter_handler
    );
    reg!(
        "_get_thread_local_invalid_parameter_handler",
        _get_thread_local_invalid_parameter_handler
    );
    reg!("_seh_filter_exe", _seh_filter_exe);
    reg!("_c_exit", _c_exit);
    reg!("_cexit", _cexit);
    reg!("exit", crt_exit);
    reg!("_exit", crt_quick_exit);
    reg!("abort", crt_abort);
    reg!("terminate", crt_abort);
    reg!("_purecall", crt_abort);
    reg!("__std_terminate", crt_abort);
    reg!("__C_specific_handler", __C_specific_handler);
    reg!("__CxxFrameHandler3", __CxxFrameHandler3);
    reg!("__CxxFrameHandler4", __CxxFrameHandler3);
    reg!("_CxxThrowException", _CxxThrowException);
    reg!("__std_exception_copy", __std_exception_copy);
    reg!("__std_exception_destroy", __std_exception_destroy);
    reg!("_beginthreadex", crt_beginthreadex);
    reg!("_endthreadex", crt_endthreadex);

    // Stdio & fichiers
    reg!("__acrt_iob_func", __acrt_iob_func);
    reg!("fopen", crt_fopen);
    reg!("fopen_s", crt_fopen_s);
    reg!("_fsopen", crt_fsopen);
    reg!("_wfopen", crt_wfopen);
    reg!("_wfopen_s", crt_wfopen_s);
    reg!("_wfsopen", crt_wfsopen);
    reg!("fclose", crt_fclose);
    reg!("fflush", crt_fflush);
    reg!("fwrite", crt_fwrite);
    reg!("fread", crt_fread);
    reg!("fseek", crt_fseek);
    reg!("ftell", crt_ftell);
    reg!("_fseeki64", crt_fseeki64);
    reg!("_ftelli64", crt_ftelli64);
    reg!("fgetpos", crt_fgetpos);
    reg!("fsetpos", crt_fsetpos);
    reg!("rewind", crt_rewind);
    reg!("fgetc", crt_fgetc);
    reg!("getc", crt_fgetc);
    reg!("_fgetchar", crt_getchar);
    reg!("getchar", crt_getchar);
    reg!("ungetc", crt_ungetc);
    reg!("fputc", crt_fputc);
    reg!("putc", crt_fputc);
    reg!("_fputchar", crt_putchar);
    reg!("putchar", crt_putchar);
    reg!("fputs", crt_fputs);
    reg!("puts", crt_puts);
    reg!("fgets", crt_fgets);
    reg!("fgetwc", crt_fgetwc);
    reg!("getwc", crt_fgetwc);
    reg!("fputwc", crt_fputwc);
    reg!("putwc", crt_fputwc);
    reg!("ungetwc", crt_ungetwc);
    reg!("fputws", crt_fputws);
    reg!("fgetws", crt_fgetws);
    reg!("feof", crt_feof);
    reg!("ferror", crt_ferror);
    reg!("clearerr", crt_clearerr);
    reg!("_fileno", crt_fileno);
    reg!("setvbuf", crt_setvbuf);
    reg!("setbuf", crt_setbuf);
    reg!(
        "_get_stream_buffer_pointers",
        crt_get_stream_buffer_pointers
    );
    reg!("_lock_file", crt_lock_file);
    reg!("_unlock_file", crt_unlock_file);
    reg!("__stdio_common_vfprintf", __stdio_common_vfprintf);
    reg!("__stdio_common_vfprintf_s", __stdio_common_vfprintf);
    reg!("__stdio_common_vfwprintf", __stdio_common_vfwprintf);
    reg!("__stdio_common_vfwprintf_s", __stdio_common_vfwprintf);
    reg!("__stdio_common_vsprintf", __stdio_common_vsprintf);
    reg!("__stdio_common_vsprintf_s", __stdio_common_vsprintf_s);
    reg!("__stdio_common_vsnprintf_s", __stdio_common_vsnprintf_s);
    reg!("__stdio_common_vswprintf", __stdio_common_vswprintf);
    reg!("__stdio_common_vswprintf_s", __stdio_common_vswprintf_s);
    reg!("__stdio_common_vsnwprintf_s", __stdio_common_vsnwprintf_s);
    reg!("__stdio_common_vsscanf", __stdio_common_vsscanf);
    reg!("__stdio_common_vswscanf", __stdio_common_vswscanf);
    reg!("__stdio_common_vfscanf", __stdio_common_vfscanf);
    reg!("__stdio_common_vfwscanf", __stdio_common_vfwscanf);

    // Système de fichiers (via VFS)
    reg!("_stat64", crt_stat64);
    reg!("_wstat64", crt_wstat64);
    reg!("_stat64i32", crt_stat64i32);
    reg!("_wstat64i32", crt_wstat64i32);
    reg!("remove", crt_remove);
    reg!("_wremove", crt_wremove);
    reg!("_unlink", crt_remove);
    reg!("_wunlink", crt_wremove);
    reg!("rename", crt_rename);
    reg!("_wrename", crt_wrename);
    reg!("_access", crt_access);
    reg!("_waccess", crt_waccess);
    reg!("_access_s", crt_access_s);
    reg!("_waccess_s", crt_waccess_s);
    reg!("_mkdir", crt_mkdir);
    reg!("_wmkdir", crt_wmkdir);
    reg!("_rmdir", crt_rmdir);
    reg!("_wrmdir", crt_wrmdir);

    // Utilitaires
    reg!("qsort", crt_qsort);
    reg!("rand", crt_rand);
    reg!("srand", crt_srand);

    // Mathématiques, temps et locale : voir `crt_math.rs` (errno EDOM/ERANGE fidèle).

    m
}

// ---------- Heap & Mémoire ----------

extern "win64" fn crt_malloc(size: usize) -> *mut c_void {
    // SAFETY: `libc::malloc`.
    unsafe { libc::malloc(size.max(1)) }.cast::<c_void>()
}

extern "win64" fn crt_calloc(nmemb: usize, size: usize) -> *mut c_void {
    // SAFETY: `libc::calloc`.
    unsafe { libc::calloc(nmemb.max(1), size.max(1)) }.cast::<c_void>()
}

extern "win64" fn crt_realloc(ptr_val: *mut c_void, size: usize) -> *mut c_void {
    // SAFETY: `libc::realloc`.
    unsafe { libc::realloc(ptr_val.cast::<libc::c_void>(), size.max(1)) }.cast::<c_void>()
}

extern "win64" fn crt_free(ptr_val: *mut c_void) {
    if !ptr_val.is_null() {
        // SAFETY: `libc::free`.
        unsafe {
            libc::free(ptr_val.cast::<libc::c_void>());
        }
    }
}

extern "win64" fn _set_new_mode(_mode: i32) -> i32 {
    0
}

extern "win64" fn _callnewh(_size: usize) -> i32 {
    0
}

// ---------- Chaînes & Mémoire ----------

extern "win64" fn crt_memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if !dst.is_null() && !src.is_null() && n > 0 {
        // SAFETY: contrat standard C `memcpy`.
        unsafe {
            ptr::copy_nonoverlapping(src, dst, n);
        }
    }
    dst
}

extern "win64" fn crt_memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if !dst.is_null() && !src.is_null() && n > 0 {
        // SAFETY: contrat standard C `memmove`.
        unsafe {
            ptr::copy(src, dst, n);
        }
    }
    dst
}

extern "win64" fn crt_memset(dst: *mut u8, val: i32, n: usize) -> *mut u8 {
    if !dst.is_null() && n > 0 {
        // SAFETY: contrat standard C `memset`.
        unsafe {
            ptr::write_bytes(dst, val as u8, n);
        }
    }
    dst
}

extern "win64" fn crt_memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    if n == 0 || a == b {
        return 0;
    }
    // SAFETY: contrat standard C `memcmp`.
    unsafe { libc::memcmp(a.cast::<libc::c_void>(), b.cast::<libc::c_void>(), n) }
}

extern "win64" fn crt_strncmp(a: *const i8, b: *const i8, n: usize) -> i32 {
    if a.is_null() || b.is_null() {
        return 0;
    }
    // SAFETY: contrat standard C `strncmp`.
    unsafe { libc::strncmp(a, b, n) }
}

extern "win64" fn crt_strstr(haystack: *const i8, needle: *const i8) -> *const i8 {
    if haystack.is_null() || needle.is_null() {
        return ptr::null();
    }
    // SAFETY: contrat standard C `strstr`.
    unsafe { libc::strstr(haystack, needle) }
}

extern "win64" fn crt_wcsstr(haystack: *const u16, needle: *const u16) -> *const u16 {
    if haystack.is_null() || needle.is_null() {
        return ptr::null();
    }
    let n_len = crt_wcslen(needle);
    if n_len == 0 {
        return haystack;
    }
    let h_len = crt_wcslen(haystack);
    if n_len > h_len {
        return ptr::null();
    }
    // SAFETY: `haystack` et `needle` sont terminés par 0.
    let h = unsafe { std::slice::from_raw_parts(haystack, h_len) };
    let n = unsafe { std::slice::from_raw_parts(needle, n_len) };
    for i in 0..=(h_len - n_len) {
        if &h[i..i + n_len] == n {
            // SAFETY: `i <= h_len`.
            return unsafe { haystack.add(i) };
        }
    }
    ptr::null()
}

extern "win64" fn crt_wcslen(s: *const u16) -> usize {
    if s.is_null() {
        return 0;
    }
    let mut len = 0usize;
    // SAFETY: chaîne UTF-16LE terminée par 0.
    while unsafe { *s.add(len) } != 0 {
        len += 1;
    }
    len
}

extern "win64" fn crt_atoi(s: *const i8) -> i32 {
    if s.is_null() {
        return 0;
    }
    // SAFETY: chaîne C terminée par 0.
    unsafe { libc::atoi(s) }
}

extern "win64" fn crt_wcstombs(dst: *mut u8, src: *const u16, max: usize) -> usize {
    if src.is_null() {
        return 0;
    }
    let len = crt_wcslen(src);
    // SAFETY: `src` contient `len` unités UTF-16.
    let slice = unsafe { std::slice::from_raw_parts(src, len) };
    let utf8 = String::from_utf16_lossy(slice);
    let bytes = utf8.as_bytes();
    if dst.is_null() {
        return bytes.len();
    }
    let n = bytes.len().min(max);
    // SAFETY: `dst` a une capacité >= `n`.
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), dst, n);
        if n < max {
            *dst.add(n) = 0;
        }
    }
    n
}

// ---------- Initialisation & Fin CRT ----------

extern "win64" fn _initterm(start: *const usize, end: *const usize) {
    ffi_guard!(CHANNEL, "_initterm", (), {
        if start.is_null() || end.is_null() {
            return;
        }
        let mut cur = start;
        while cur < end {
            // SAFETY: table de pointeurs de constructeurs générée par le compilateur invité.
            let fn_addr = unsafe { *cur };
            if fn_addr != 0 {
                sb_debug!(CHANNEL, "_initterm appel constructeur {fn_addr:#x}");
                // SAFETY: constructeur C/C++ Win64 sans argument.
                let f: unsafe extern "win64" fn() = unsafe { core::mem::transmute(fn_addr) };
                unsafe {
                    f();
                }
            }
            // SAFETY: avance d'un pointeur dans `[start, end)`.
            cur = unsafe { cur.add(1) };
        }
    });
}

extern "win64" fn _initterm_e(start: *const usize, end: *const usize) -> i32 {
    ffi_guard!(CHANNEL, "_initterm_e", -1, {
        if start.is_null() || end.is_null() {
            return 0;
        }
        let mut cur = start;
        while cur < end {
            // SAFETY: table de pointeurs d'initialiseurs générée par le compilateur invité.
            let fn_addr = unsafe { *cur };
            if fn_addr != 0 {
                sb_debug!(CHANNEL, "_initterm_e appel initialiseur {fn_addr:#x}");
                // SAFETY: initialiseur Win64 retournant `i32` (`0` = succès).
                let f: unsafe extern "win64" fn() -> i32 = unsafe { core::mem::transmute(fn_addr) };
                let rc = unsafe { f() };
                if rc != 0 {
                    return rc;
                }
            }
            // SAFETY: avance d'un pointeur dans `[start, end)`.
            cur = unsafe { cur.add(1) };
        }
        0
    })
}

extern "win64" fn _crt_atexit(_func: usize) -> i32 {
    0
}

extern "win64" fn _initialize_onexit_table(table: *mut [usize; 3]) -> i32 {
    if !table.is_null() {
        // SAFETY: `_onexit_table_t` contient 3 pointeurs.
        unsafe {
            *table = [0, 0, 0];
        }
    }
    0
}

extern "win64" fn _register_onexit_function(_table: *mut c_void, _func: usize) -> i32 {
    0
}

extern "win64" fn _register_thread_local_exe_atexit_callback(_cb: usize) {}

extern "win64" fn _configure_narrow_argv(_mode: i32) -> i32 {
    0
}

extern "win64" fn _initialize_narrow_environment() -> i32 {
    0
}

extern "win64" fn _get_narrow_winmain_command_line() -> *const i8 {
    process_state().read().cmdline_narrow.as_ptr()
}

extern "win64" fn __p___argc() -> *mut i32 {
    let mut st = process_state().write();
    &raw mut st.argc
}

extern "win64" fn __p___argv() -> *mut *mut *mut u8 {
    static ARGV_PTR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let ptr = process_state().write().argv_ptrs.as_mut_ptr() as usize;
    ARGV_PTR.store(ptr, Ordering::Relaxed);
    (&raw const ARGV_PTR) as *mut *mut *mut u8
}

extern "win64" fn __p__environ() -> *mut *mut *mut u8 {
    static ENV_PTR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let ptr = process_state().write().env_narrow.as_mut_ptr() as usize;
    ENV_PTR.store(ptr, Ordering::Relaxed);
    (&raw const ENV_PTR) as *mut *mut *mut u8
}

extern "win64" fn __p__commode() -> *mut i32 {
    let mut st = process_state().write();
    &raw mut st.commode
}

extern "win64" fn __p__fmode() -> *mut i32 {
    let mut st = process_state().write();
    &raw mut st.fmode
}

extern "win64" fn _set_app_type(_kind: i32) {}

extern "win64" fn _set_fmode(mode: i32) -> i32 {
    process_state().write().fmode = mode;
    0
}

extern "win64" fn _configthreadlocale(_flag: i32) -> i32 {
    0
}

extern "win64" fn __setusermatherr(_handler: usize) {}

/// `_invalid_parameter_handler` global du processus (0 = gestionnaire par défaut).
static INVALID_PARAM_HANDLER: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

thread_local! {
    /// Gestionnaire par thread (`_set_thread_local_invalid_parameter_handler`), prioritaire.
    static THREAD_INVALID_PARAM_HANDLER: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Signature MSVC : `void (const wchar_t* expr, const wchar_t* func, const wchar_t* file,
/// unsigned line, uintptr_t reserved)`.
type InvalidParamHandler = unsafe extern "win64" fn(*const u16, *const u16, *const u16, u32, usize);

/// `STATUS_STACK_BUFFER_OVERRUN`, code de fin utilisé par `_invoke_watson` / `__fastfail`.
const STATUS_STACK_BUFFER_OVERRUN: u32 = 0xC000_0409;

extern "win64" fn _set_invalid_parameter_handler(handler: usize) -> usize {
    INVALID_PARAM_HANDLER.swap(handler, Ordering::AcqRel)
}

extern "win64" fn _get_invalid_parameter_handler() -> usize {
    INVALID_PARAM_HANDLER.load(Ordering::Acquire)
}

extern "win64" fn _set_thread_local_invalid_parameter_handler(handler: usize) -> usize {
    THREAD_INVALID_PARAM_HANDLER.with(|h| h.replace(handler))
}

extern "win64" fn _get_thread_local_invalid_parameter_handler() -> usize {
    THREAD_INVALID_PARAM_HANDLER.with(|h| h.get())
}

/// Appelle le gestionnaire installé (thread puis processus). Retourne `false` si aucun.
fn call_invalid_parameter_handler() -> bool {
    let handler = match THREAD_INVALID_PARAM_HANDLER.with(|h| h.get()) {
        0 => INVALID_PARAM_HANDLER.load(Ordering::Acquire),
        h => h,
    };
    if handler == 0 {
        return false;
    }
    // SAFETY: adresse fournie par l'invité via `_set_*invalid_parameter_handler`, de
    // signature `_invalid_parameter_handler` (ABI win64) ; la release CRT passe des NULL.
    unsafe {
        let f: InvalidParamHandler = core::mem::transmute(handler);
        f(ptr::null(), ptr::null(), ptr::null(), 0, 0);
    }
    true
}

/// Comportement par défaut de l'UCRT release (`_invoke_watson`) : fin immédiate du processus.
fn invoke_watson() -> ! {
    sb_err!(
        CHANNEL,
        "paramètre invalide sans gestionnaire : fin du processus (0x{STATUS_STACK_BUFFER_OVERRUN:08X})"
    );
    std::process::exit(STATUS_STACK_BUFFER_OVERRUN as i32);
}

extern "win64" fn _invalid_parameter_noinfo() {
    if !call_invalid_parameter_handler() {
        invoke_watson();
    }
}

extern "win64" fn _invalid_parameter_noinfo_noreturn() -> ! {
    call_invalid_parameter_handler();
    invoke_watson();
}

extern "win64" fn _invoke_watson(
    _expr: *const u16,
    _func: *const u16,
    _file: *const u16,
    _line: u32,
    _reserved: usize,
) -> ! {
    invoke_watson();
}

extern "win64" fn _seh_filter_exe(_code: u32, _ptrs: *mut c_void) -> i32 {
    0
}

extern "win64" fn _c_exit() {}
extern "win64" fn _cexit() {}

extern "win64" fn crt_exit(code: i32) -> ! {
    sb_info!(CHANNEL, "exit({code})");
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}

extern "win64" fn crt_quick_exit(code: i32) -> ! {
    crt_exit(code);
}

extern "win64" fn crt_abort() -> ! {
    sb_err!(
        CHANNEL,
        "abort() / terminate() appelé par le binaire invité"
    );
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(134);
}

extern "win64" fn __C_specific_handler(
    _rec: *mut c_void,
    _frame: *mut c_void,
    _ctx: *mut c_void,
    _disp: *mut c_void,
) -> i32 {
    1 // ExceptionContinueSearch
}

extern "win64" fn __CxxFrameHandler3(
    _rec: *mut c_void,
    _frame: *mut c_void,
    _ctx: *mut c_void,
    _disp: *mut c_void,
) -> i32 {
    1
}

extern "win64" fn _CxxThrowException(exc: *mut c_void, info: *mut c_void) -> ! {
    sb_err!(
        CHANNEL,
        "_CxxThrowException(exc={exc:p}, throw_info={info:p}) — unwind C++ SEH non supporté"
    );
    std::process::exit(101);
}

extern "win64" fn __std_exception_copy(_from: *const c_void, _to: *mut c_void) {}
extern "win64" fn __std_exception_destroy(_data: *mut c_void) {}

// ---------- Stdio & Fichiers ----------
//
// `FILE*` invité = pointeur vers un `CrtFile` (en-tête miroir de `__crt_stdio_stream_data` UCRT,
// suivi de l'état Rust). Les flux vivants sont enregistrés dans `FILES` : tout pointeur inconnu
// est rejeté proprement (`EINVAL`) au lieu de provoquer un accès mémoire invalide.
// Toute ouverture passe par le VFS sandboxé (jamais de chemin hôte brut).

const EOF: i32 = -1;
const WEOF: i32 = 0xFFFF;

pub(crate) const ENOENT: i32 = 2;
pub(crate) const EIO: i32 = 5;
pub(crate) const EBADF: i32 = 9;
pub(crate) const ENOMEM: i32 = 12;
pub(crate) const EACCES: i32 = 13;
pub(crate) const EEXIST: i32 = 17;
pub(crate) const ENOSPC: i32 = 28;
pub(crate) const ESPIPE: i32 = 29;
pub(crate) const ENOTEMPTY: i32 = 41;
pub(crate) const EILSEQ: i32 = 42;

const IOFBF: i32 = 0x0000;
const IOLBF: i32 = 0x0040;
const IONBF: i32 = 0x0004;
const DEFAULT_BUFSIZ: usize = 4096;
const O_BINARY: i32 = 0x8000;

// Options `_CRT_INTERNAL_PRINTF_*` / `_CRT_INTERNAL_SCANF_*` (`corecrt_stdio_config.h`).
const PF_LEGACY_VSPRINTF_NULL_TERMINATION: u64 = 1 << 0;
const PF_STANDARD_SNPRINTF_BEHAVIOR: u64 = 1 << 1;
const PF_LEGACY_WIDE_SPECIFIERS: u64 = 1 << 2;
const PF_LEGACY_THREE_DIGIT_EXPONENTS: u64 = 1 << 4;
const SF_SECURECRT: u64 = 1 << 0;
const SF_LEGACY_WIDE_SPECIFIERS: u64 = 1 << 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    /// Octets (locale « C » : 1 octet <-> 1 `wchar_t` <= 0xFF).
    Ansi,
    Utf8,
    Utf16le,
}

enum Backend {
    Host(std::fs::File),
    Stdin,
    Stdout,
    Stderr,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastOp {
    None,
    Read,
    Write,
}

pub(crate) struct FileState {
    backend: Backend,
    readable: bool,
    writable: bool,
    text: bool,
    encoding: Encoding,
    eof: bool,
    error: bool,
    rbuf: Vec<u8>,
    rpos: usize,
    wbuf: Vec<u8>,
    unget: Vec<u8>,
    wunget: Vec<u16>,
    pending_high: Option<u16>,
    buf_mode: i32,
    buf_size: usize,
    save_mount: bool,
    last: LastOp,
}

impl FileState {
    fn new(backend: Backend, readable: bool, writable: bool, text: bool) -> Self {
        Self {
            backend,
            readable,
            writable,
            text,
            encoding: Encoding::Ansi,
            eof: false,
            error: false,
            rbuf: Vec::new(),
            rpos: 0,
            wbuf: Vec::new(),
            unget: Vec::new(),
            wunget: Vec::new(),
            pending_high: None,
            buf_mode: IOFBF,
            buf_size: DEFAULT_BUFSIZ,
            save_mount: false,
            last: LastOp::None,
        }
    }

    fn is_std(&self) -> bool {
        matches!(
            self.backend,
            Backend::Stdin | Backend::Stdout | Backend::Stderr
        )
    }

    /// Traduction CRLF/Ctrl-Z du mode texte (désactivée sur la console hôte).
    fn translate(&self) -> bool {
        self.text && !self.is_std()
    }

    fn refill(&mut self) -> Result<bool, ()> {
        self.rbuf.resize(self.buf_size.max(512), 0);
        self.rpos = 0;
        let res = match &mut self.backend {
            Backend::Host(f) => f.read(&mut self.rbuf),
            Backend::Stdin => std::io::stdin().read(&mut self.rbuf),
            _ => {
                self.rbuf.clear();
                return Err(());
            }
        };
        match res {
            Ok(n) => {
                self.rbuf.truncate(n);
                Ok(n > 0)
            }
            Err(_) => {
                self.rbuf.clear();
                Err(())
            }
        }
    }

    fn next_raw(&mut self) -> Option<u8> {
        if self.rpos >= self.rbuf.len() {
            match self.refill() {
                Ok(true) => {}
                Ok(false) => {
                    self.eof = true;
                    return None;
                }
                Err(()) => {
                    self.error = true;
                    return None;
                }
            }
        }
        let b = self.rbuf[self.rpos];
        self.rpos += 1;
        Some(b)
    }

    fn peek_raw(&mut self) -> Option<u8> {
        if self.rpos >= self.rbuf.len() && self.refill() != Ok(true) {
            return None;
        }
        self.rbuf.get(self.rpos).copied()
    }

    fn begin_read(&mut self) -> bool {
        if !self.readable || matches!(self.backend, Backend::Closed) {
            self.error = true;
            crate::crt_str::set_errno(EBADF);
            return false;
        }
        if self.last == LastOp::Write && self.flush().is_err() {
            return false;
        }
        self.last = LastOp::Read;
        true
    }

    fn begin_write(&mut self) -> bool {
        if !self.writable || matches!(self.backend, Backend::Closed) {
            self.error = true;
            crate::crt_str::set_errno(EBADF);
            return false;
        }
        if self.last == LastOp::Read {
            self.drop_read_ahead();
        }
        self.last = LastOp::Write;
        true
    }

    fn unget_units(&self) -> i64 {
        let w = match self.encoding {
            Encoding::Utf16le => 2,
            _ => 1,
        };
        self.unget.len() as i64 + self.wunget.len() as i64 * w
    }

    /// Position logique (octets) : position OS - lecture anticipée - caractères repoussés.
    fn logical_pos(&mut self) -> Result<i64, i32> {
        if self.flush().is_err() {
            return Err(EIO);
        }
        let ahead = (self.rbuf.len() - self.rpos) as i64;
        let ung = self.unget_units();
        match &mut self.backend {
            Backend::Host(f) => match f.stream_position() {
                Ok(p) => Ok((p as i64 - ahead - ung).max(0)),
                Err(_) => Err(EIO),
            },
            Backend::Closed => Err(EBADF),
            _ => Err(ESPIPE),
        }
    }

    fn drop_read_ahead(&mut self) {
        if let Ok(pos) = self.logical_pos()
            && let Backend::Host(f) = &mut self.backend
        {
            let _ = f.seek(SeekFrom::Start(pos as u64));
        }
        self.rbuf.clear();
        self.rpos = 0;
        self.unget.clear();
        self.wunget.clear();
    }

    fn seek(&mut self, off: i64, whence: i32) -> Result<i64, i32> {
        let base = match whence {
            0 => 0,
            1 => self.logical_pos()?,
            2 => {
                self.flush().map_err(|()| EIO)?;
                match &mut self.backend {
                    Backend::Host(f) => f.seek(SeekFrom::End(0)).map_err(|_| EIO)? as i64,
                    Backend::Closed => return Err(EBADF),
                    _ => return Err(ESPIPE),
                }
            }
            _ => return Err(EINVAL),
        };
        let target = base.checked_add(off).ok_or(EINVAL)?;
        if target < 0 {
            return Err(EINVAL);
        }
        self.flush().map_err(|()| EIO)?;
        match &mut self.backend {
            Backend::Host(f) => {
                f.seek(SeekFrom::Start(target as u64)).map_err(|_| EIO)?;
            }
            Backend::Closed => return Err(EBADF),
            _ => return Err(ESPIPE),
        }
        self.rbuf.clear();
        self.rpos = 0;
        self.unget.clear();
        self.wunget.clear();
        self.eof = false;
        self.last = LastOp::None;
        Ok(target)
    }

    fn flush(&mut self) -> Result<(), ()> {
        if self.wbuf.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.wbuf);
        if self.save_mount {
            let st = process_state().read();
            if let Some(vfs) = st.vfs.as_ref()
                && vfs.check_save_quota(data.len() as u64).is_err()
            {
                self.error = true;
                crate::crt_str::set_errno(ENOSPC);
                return Err(());
            }
        }
        let res = match &mut self.backend {
            Backend::Host(f) => f.write_all(&data),
            Backend::Stdout => std::io::stdout().write_all(&data),
            Backend::Stderr => std::io::stderr().write_all(&data),
            _ => Err(std::io::Error::other("flux non inscriptible")),
        };
        if res.is_err() {
            self.error = true;
            crate::crt_str::set_errno(EIO);
            return Err(());
        }
        Ok(())
    }

    fn write_raw(&mut self, data: &[u8]) -> bool {
        self.wbuf.extend_from_slice(data);
        let must = self.buf_mode == IONBF
            || self.is_std()
            || self.wbuf.len() >= self.buf_size
            || (self.buf_mode == IOLBF && data.contains(&b'\n'));
        if must { self.flush().is_ok() } else { true }
    }

    /// Écrit des octets « étroits » avec traduction `\n` -> `\r\n` en mode texte.
    fn write_bytes(&mut self, data: &[u8]) -> bool {
        if !self.begin_write() {
            return false;
        }
        if self.translate() && data.contains(&b'\n') {
            let mut v = Vec::with_capacity(data.len() + 8);
            for &b in data {
                if b == b'\n' {
                    v.push(b'\r');
                }
                v.push(b);
            }
            self.write_raw(&v)
        } else {
            self.write_raw(data)
        }
    }

    fn wide_encoding(&self) -> Encoding {
        if self.is_std() {
            Encoding::Utf8
        } else {
            self.encoding
        }
    }

    fn putwc(&mut self, w: u16) -> bool {
        if !self.begin_write() {
            return false;
        }
        let tr = self.translate();
        match self.wide_encoding() {
            Encoding::Ansi => {
                if w > 0xFF {
                    self.error = true;
                    crate::crt_str::set_errno(EILSEQ);
                    return false;
                }
                if tr && w == u16::from(b'\n') {
                    self.write_raw(b"\r\n")
                } else {
                    self.write_raw(&[w as u8])
                }
            }
            Encoding::Utf16le => {
                if tr && w == u16::from(b'\n') {
                    self.write_raw(&[b'\r', 0, b'\n', 0])
                } else {
                    self.write_raw(&w.to_le_bytes())
                }
            }
            Encoding::Utf8 => {
                if (0xD800..0xDC00).contains(&w) {
                    self.pending_high = Some(w);
                    return true;
                }
                let c = if (0xDC00..0xE000).contains(&w) {
                    match self.pending_high.take() {
                        Some(h) => char::from_u32(
                            0x10000 + ((u32::from(h) - 0xD800) << 10) + (u32::from(w) - 0xDC00),
                        )
                        .unwrap_or('\u{FFFD}'),
                        None => '\u{FFFD}',
                    }
                } else {
                    self.pending_high = None;
                    char::from_u32(u32::from(w)).unwrap_or('\u{FFFD}')
                };
                if tr && c == '\n' {
                    return self.write_raw(b"\r\n");
                }
                let mut b = [0u8; 4];
                let s = c.encode_utf8(&mut b);
                self.write_raw(s.as_bytes())
            }
        }
    }

    fn getc(&mut self) -> Option<u8> {
        if !self.begin_read() {
            return None;
        }
        if let Some(c) = self.unget.pop() {
            return Some(c);
        }
        let b = self.next_raw()?;
        if self.translate() {
            if b == 0x1A {
                self.rpos -= 1;
                self.eof = true;
                return None;
            }
            if b == b'\r' && self.peek_raw() == Some(b'\n') {
                self.rpos += 1;
                return Some(b'\n');
            }
        }
        Some(b)
    }

    fn decode_wide(&mut self) -> Option<u16> {
        match self.wide_encoding() {
            Encoding::Ansi => {
                if let Some(c) = self.unget.pop() {
                    return Some(u16::from(c));
                }
                let b = self.next_raw()?;
                if self.translate() && b == 0x1A {
                    self.rpos -= 1;
                    self.eof = true;
                    return None;
                }
                Some(u16::from(b))
            }
            Encoding::Utf16le => {
                let lo = self.next_raw()?;
                let hi = self.next_raw()?;
                Some(u16::from_le_bytes([lo, hi]))
            }
            Encoding::Utf8 => {
                let b0 = self.next_raw()?;
                let (len, init) = match b0 {
                    0x00..=0x7F => return Some(u16::from(b0)),
                    0xC2..=0xDF => (1, u32::from(b0 & 0x1F)),
                    0xE0..=0xEF => (2, u32::from(b0 & 0x0F)),
                    0xF0..=0xF4 => (3, u32::from(b0 & 0x07)),
                    _ => return Some(0xFFFD),
                };
                let mut cp = init;
                for _ in 0..len {
                    match self.peek_raw() {
                        Some(b) if b & 0xC0 == 0x80 => {
                            self.rpos += 1;
                            cp = (cp << 6) | u32::from(b & 0x3F);
                        }
                        _ => return Some(0xFFFD),
                    }
                }
                let Some(c) = char::from_u32(cp) else {
                    return Some(0xFFFD);
                };
                let mut buf = [0u16; 2];
                let enc = c.encode_utf16(&mut buf);
                if enc.len() == 2 {
                    self.wunget.push(enc[1]);
                }
                Some(enc[0])
            }
        }
    }

    fn getwc(&mut self) -> Option<u16> {
        if !self.begin_read() {
            return None;
        }
        if let Some(w) = self.wunget.pop() {
            return Some(w);
        }
        let w = self.decode_wide()?;
        if self.translate() && w == u16::from(b'\r') {
            match self.decode_wide() {
                Some(n) if n == u16::from(b'\n') => return Some(n),
                Some(n) => self.wunget.push(n),
                None => self.eof = false,
            }
        }
        Some(w)
    }

    fn read_into(&mut self, dst: &mut [u8]) -> usize {
        if !self.begin_read() {
            return 0;
        }
        if self.translate() {
            let mut n = 0;
            while n < dst.len() {
                match self.getc() {
                    Some(b) => {
                        dst[n] = b;
                        n += 1;
                    }
                    None => break,
                }
            }
            return n;
        }
        let mut n = 0;
        while n < dst.len() {
            if let Some(c) = self.unget.pop() {
                dst[n] = c;
                n += 1;
                continue;
            }
            if self.rpos < self.rbuf.len() {
                let k = (self.rbuf.len() - self.rpos).min(dst.len() - n);
                dst[n..n + k].copy_from_slice(&self.rbuf[self.rpos..self.rpos + k]);
                self.rpos += k;
                n += k;
                continue;
            }
            match self.refill() {
                Ok(true) => {}
                Ok(false) => {
                    self.eof = true;
                    break;
                }
                Err(()) => {
                    self.error = true;
                    break;
                }
            }
        }
        n
    }
}

/// `FILE` invité. Les 48 premiers octets reproduisent `__crt_stdio_stream_data` (UCRT) afin que
/// `_get_stream_buffer_pointers` (utilisé par `basic_filebuf` MSVC) renvoie des champs valides.
#[repr(C)]
pub(crate) struct CrtFile {
    ptr: UnsafeCell<*mut u8>,
    base: UnsafeCell<*mut u8>,
    cnt: UnsafeCell<i32>,
    flags: UnsafeCell<i32>,
    file: i32,
    charbuf: i32,
    bufsiz: i32,
    tmpfname: *mut u8,
    lock: ReentrantCs,
    state: Mutex<FileState>,
}
const _: () = assert!(core::mem::offset_of!(CrtFile, base) == 8);
const _: () = assert!(core::mem::offset_of!(CrtFile, cnt) == 16);
const _: () = assert!(core::mem::offset_of!(CrtFile, file) == 24);
const _: () = assert!(core::mem::offset_of!(CrtFile, tmpfname) == 40);

// SAFETY: les champs bruts de l'en-tête ne sont que des cellules exposées à l'invité (jamais
// déréférencées côté Rust) ; l'état est protégé par `Mutex` + `ReentrantCs`.
unsafe impl Send for CrtFile {}
unsafe impl Sync for CrtFile {}

impl CrtFile {
    fn new(fd: i32, state: FileState) -> Arc<Self> {
        Arc::new(Self {
            ptr: UnsafeCell::new(ptr::null_mut()),
            base: UnsafeCell::new(ptr::null_mut()),
            cnt: UnsafeCell::new(0),
            flags: UnsafeCell::new(0),
            file: fd,
            charbuf: 0,
            bufsiz: DEFAULT_BUFSIZ as i32,
            tmpfname: ptr::null_mut(),
            lock: ReentrantCs::new(),
            state: Mutex::new(state),
        })
    }
}

static FILES: LazyLock<Mutex<HashMap<usize, Arc<CrtFile>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_FD: AtomicI32 = AtomicI32::new(3);

fn std_files() -> &'static [Arc<CrtFile>; 3] {
    static STD: OnceLock<[Arc<CrtFile>; 3]> = OnceLock::new();
    STD.get_or_init(|| {
        let files = [
            CrtFile::new(0, FileState::new(Backend::Stdin, true, false, true)),
            CrtFile::new(1, FileState::new(Backend::Stdout, false, true, true)),
            CrtFile::new(2, FileState::new(Backend::Stderr, false, true, true)),
        ];
        let mut reg = FILES.lock();
        for f in &files {
            reg.insert(Arc::as_ptr(f) as usize, Arc::clone(f));
        }
        files
    })
}

fn lookup_file(stream: *mut c_void) -> Option<Arc<CrtFile>> {
    if stream.is_null() {
        return None;
    }
    let _ = std_files();
    FILES.lock().get(&(stream as usize)).cloned()
}

/// Verrou `_lock_file` tenu pour la durée d'une opération.
struct FileGuard(Arc<CrtFile>);

impl FileGuard {
    fn new(f: Arc<CrtFile>) -> Self {
        f.lock.lock();
        Self(f)
    }
}

impl Drop for FileGuard {
    fn drop(&mut self) {
        self.0.lock.unlock();
    }
}

/// Exécute `op` sur l'état d'un flux valide (verrouillé), sinon `EINVAL` + `dflt`.
fn with_file<R>(stream: *mut c_void, dflt: R, op: impl FnOnce(&mut FileState) -> R) -> R {
    let Some(f) = lookup_file(stream) else {
        crate::crt_str::set_errno(EINVAL);
        return dflt;
    };
    let g = FileGuard::new(f);
    let mut st = g.0.state.lock();
    op(&mut st)
}

fn errno_from_io(e: &std::io::Error) -> i32 {
    match e.kind() {
        std::io::ErrorKind::NotFound => ENOENT,
        std::io::ErrorKind::PermissionDenied => EACCES,
        std::io::ErrorKind::AlreadyExists => EEXIST,
        std::io::ErrorKind::IsADirectory => EACCES,
        std::io::ErrorKind::DirectoryNotEmpty => ENOTEMPTY,
        std::io::ErrorKind::NotADirectory => ENOENT,
        std::io::ErrorKind::StorageFull => ENOSPC,
        _ => EIO,
    }
}

/// Chemin hôte résolu dans le sandbox VFS.
struct VfsPath {
    host: std::path::PathBuf,
    mount: sb_vfs::VfsMount,
    is_root: bool,
}

/// Résout un chemin invité via le VFS (`errno` MSVC en cas de refus).
fn vfs_path(guest: &str) -> Result<VfsPath, i32> {
    let st = process_state().read();
    let Some(vfs) = st.vfs.as_ref() else {
        sb_debug!(CHANNEL, "'{guest}' refusé : VFS non initialisé");
        return Err(ENOENT);
    };
    match vfs.resolve(guest) {
        Ok(r) => Ok(VfsPath {
            is_root: r.relative_path.as_os_str().is_empty(),
            host: r.host_path,
            mount: r.mount,
        }),
        Err(err) => {
            sb_debug!(CHANNEL, "'{guest}' bloqué par VFS : {err}");
            Err(match err {
                sb_vfs::VfsError::EscapeAttempt(_) => EACCES,
                _ => ENOENT,
            })
        }
    }
}

struct OpenMode {
    read: bool,
    write: bool,
    append: bool,
    create: bool,
    truncate: bool,
    exclusive: bool,
    text: bool,
    encoding: Encoding,
}

/// Analyse un mode `fopen` MSVC (`r|w|a`, `+`, `b|t`, `x`, `c|n|N|S|R|T|D`, `, ccs=...`).
fn parse_mode(mode: &str) -> Option<OpenMode> {
    let (flags, ccs) = match mode.split_once(',') {
        Some((f, rest)) => {
            let rest = rest.trim();
            let enc = rest
                .strip_prefix("ccs")?
                .trim_start()
                .strip_prefix('=')?
                .trim();
            let e = match enc.to_ascii_uppercase().as_str() {
                "UTF-8" => Encoding::Utf8,
                "UTF-16LE" | "UNICODE" => Encoding::Utf16le,
                _ => return None,
            };
            (f.trim_end(), Some(e))
        }
        None => (mode, None),
    };
    let mut chars = flags.trim_start().chars();
    let mut m = match chars.next()? {
        'r' => OpenMode {
            read: true,
            write: false,
            append: false,
            create: false,
            truncate: false,
            exclusive: false,
            text: true,
            encoding: Encoding::Ansi,
        },
        'w' => OpenMode {
            read: false,
            write: true,
            append: false,
            create: true,
            truncate: true,
            exclusive: false,
            text: true,
            encoding: Encoding::Ansi,
        },
        'a' => OpenMode {
            read: false,
            write: true,
            append: true,
            create: true,
            truncate: false,
            exclusive: false,
            text: true,
            encoding: Encoding::Ansi,
        },
        _ => return None,
    };
    let mut explicit_tb = false;
    let binary_default = process_state().read().fmode == O_BINARY;
    for c in chars {
        match c {
            '+' => {
                m.read = true;
                m.write = true;
            }
            'b' if !explicit_tb => {
                m.text = false;
                explicit_tb = true;
            }
            't' if !explicit_tb => {
                m.text = true;
                explicit_tb = true;
            }
            'x' if m.truncate => m.exclusive = true,
            'c' | 'n' | 'N' | 'S' | 'R' | 'T' | 'D' => {}
            ' ' => {}
            _ => return None,
        }
    }
    if !explicit_tb && binary_default {
        m.text = false;
    }
    if let Some(e) = ccs {
        m.encoding = e;
        m.text = true;
    }
    Some(m)
}

/// Ouvre un flux via le VFS ; renvoie le `FILE*` ou un `errno`.
pub(crate) fn open_stream(path: &str, mode: &str) -> Result<*mut c_void, i32> {
    let m = parse_mode(mode).ok_or(EINVAL)?;
    if path.is_empty() {
        return Err(EINVAL);
    }
    let vp = vfs_path(path)?;
    let mut oo = std::fs::OpenOptions::new();
    oo.read(m.read)
        .write(m.write && !m.append)
        .append(m.append)
        .truncate(m.truncate);
    if m.exclusive {
        oo.create_new(true);
    } else {
        oo.create(m.create);
    }
    if vp.host.is_dir() {
        return Err(EACCES);
    }
    let file = oo.open(&vp.host).map_err(|e| errno_from_io(&e))?;
    let mut st = FileState::new(Backend::Host(file), m.read, m.write, m.text);
    st.encoding = m.encoding;
    st.save_mount = vp.mount == sb_vfs::VfsMount::Save && m.write;
    if m.encoding != Encoding::Ansi {
        if m.read && !m.truncate {
            // Détection BOM : prioritaire sur `ccs=`.
            let b0 = st.peek_raw();
            if b0 == Some(0xEF) || b0 == Some(0xFF) {
                let bytes: Vec<u8> = st.rbuf[st.rpos..].iter().take(3).copied().collect();
                if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
                    st.encoding = Encoding::Utf8;
                    st.rpos += 3;
                } else if bytes.starts_with(&[0xFF, 0xFE]) {
                    st.encoding = Encoding::Utf16le;
                    st.rpos += 2;
                }
            }
        } else if m.truncate {
            let bom: &[u8] = if m.encoding == Encoding::Utf8 {
                &[0xEF, 0xBB, 0xBF]
            } else {
                &[0xFF, 0xFE]
            };
            st.write_raw(bom);
            st.last = LastOp::Write;
        }
    }
    let fd = NEXT_FD.fetch_add(1, Ordering::Relaxed);
    let f = CrtFile::new(fd, st);
    let p = Arc::as_ptr(&f) as usize;
    FILES.lock().insert(p, f);
    Ok(p as *mut c_void)
}

fn cstr_lossy(p: *const i8) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: chaîne C terminée par 0 fournie par l'invité.
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

fn wstr_vec(p: *const u16, max: Option<usize>) -> Vec<u16> {
    let mut v = Vec::new();
    if p.is_null() {
        return v;
    }
    let limit = max.unwrap_or(usize::MAX);
    while v.len() < limit {
        // SAFETY: chaîne UTF-16 terminée par 0 (ou bornée par `max`) fournie par l'invité.
        let c = unsafe { p.add(v.len()).read_unaligned() };
        if c == 0 {
            break;
        }
        v.push(c);
    }
    v
}

fn bytes_vec(p: *const u8, max: Option<usize>) -> Vec<u8> {
    let mut v = Vec::new();
    if p.is_null() {
        return v;
    }
    let limit = max.unwrap_or(usize::MAX);
    while v.len() < limit {
        // SAFETY: chaîne C terminée par 0 (ou bornée par `max`) fournie par l'invité.
        let c = unsafe { *p.add(v.len()) };
        if c == 0 {
            break;
        }
        v.push(c);
    }
    v
}

fn wstr_lossy(p: *const u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    Some(String::from_utf16_lossy(&wstr_vec(p, None)))
}

extern "win64" fn __acrt_iob_func(index: u32) -> *mut c_void {
    ffi_guard!(CHANNEL, "__acrt_iob_func", ptr::null_mut(), {
        match std_files().get(index as usize) {
            Some(f) => Arc::as_ptr(f) as *mut c_void,
            None => {
                crate::crt_str::set_errno(EINVAL);
                ptr::null_mut()
            }
        }
    })
}

extern "win64" fn crt_fopen(filename: *const i8, mode: *const i8) -> *mut c_void {
    ffi_guard!(CHANNEL, "fopen", ptr::null_mut(), {
        let (Some(p), Some(m)) = (cstr_lossy(filename), cstr_lossy(mode)) else {
            crate::crt_str::set_errno(EINVAL);
            return ptr::null_mut();
        };
        open_stream(&p, &m).unwrap_or_else(|e| {
            crate::crt_str::set_errno(e);
            ptr::null_mut()
        })
    })
}

extern "win64" fn crt_fsopen(filename: *const i8, mode: *const i8, _sh: i32) -> *mut c_void {
    crt_fopen(filename, mode)
}

extern "win64" fn crt_wfopen(filename: *const u16, mode: *const u16) -> *mut c_void {
    ffi_guard!(CHANNEL, "_wfopen", ptr::null_mut(), {
        let (Some(p), Some(m)) = (wstr_lossy(filename), wstr_lossy(mode)) else {
            crate::crt_str::set_errno(EINVAL);
            return ptr::null_mut();
        };
        open_stream(&p, &m).unwrap_or_else(|e| {
            crate::crt_str::set_errno(e);
            ptr::null_mut()
        })
    })
}

extern "win64" fn crt_wfsopen(filename: *const u16, mode: *const u16, _sh: i32) -> *mut c_void {
    crt_wfopen(filename, mode)
}

fn fopen_s_common(out: *mut *mut c_void, path: Option<String>, mode: Option<String>) -> i32 {
    if out.is_null() {
        crate::crt_str::set_errno(EINVAL);
        return EINVAL;
    }
    // SAFETY: pointeur de sortie non nul fourni par l'invité.
    unsafe { out.write_unaligned(ptr::null_mut()) };
    let (Some(p), Some(m)) = (path, mode) else {
        crate::crt_str::set_errno(EINVAL);
        return EINVAL;
    };
    match open_stream(&p, &m) {
        Ok(f) => {
            // SAFETY: idem.
            unsafe { out.write_unaligned(f) };
            0
        }
        Err(e) => {
            crate::crt_str::set_errno(e);
            e
        }
    }
}

extern "win64" fn crt_fopen_s(out: *mut *mut c_void, filename: *const i8, mode: *const i8) -> i32 {
    ffi_guard!(CHANNEL, "fopen_s", EINVAL, {
        fopen_s_common(out, cstr_lossy(filename), cstr_lossy(mode))
    })
}

extern "win64" fn crt_wfopen_s(
    out: *mut *mut c_void,
    filename: *const u16,
    mode: *const u16,
) -> i32 {
    ffi_guard!(CHANNEL, "_wfopen_s", EINVAL, {
        fopen_s_common(out, wstr_lossy(filename), wstr_lossy(mode))
    })
}

extern "win64" fn crt_fclose(stream: *mut c_void) -> i32 {
    ffi_guard!(CHANNEL, "fclose", EOF, {
        let Some(f) = lookup_file(stream) else {
            crate::crt_str::set_errno(EINVAL);
            return EOF;
        };
        let g = FileGuard::new(Arc::clone(&f));
        let mut st = g.0.state.lock();
        let ok = st.flush().is_ok();
        if st.is_std() {
            return if ok { 0 } else { EOF };
        }
        st.backend = Backend::Closed;
        drop(st);
        FILES.lock().remove(&(stream as usize));
        if ok { 0 } else { EOF }
    })
}

extern "win64" fn crt_fflush(stream: *mut c_void) -> i32 {
    ffi_guard!(CHANNEL, "fflush", EOF, {
        if stream.is_null() {
            let _ = std_files();
            let all: Vec<Arc<CrtFile>> = FILES.lock().values().cloned().collect();
            let mut rc = 0;
            for f in all {
                let g = FileGuard::new(f);
                if g.0.state.lock().flush().is_err() {
                    rc = EOF;
                }
            }
            let _ = std::io::stdout().flush();
            return rc;
        }
        with_file(stream, EOF, |st| {
            let r = st.flush();
            if matches!(st.backend, Backend::Stdout) {
                let _ = std::io::stdout().flush();
            }
            if r.is_ok() { 0 } else { EOF }
        })
    })
}

extern "win64" fn crt_fwrite(
    ptr_val: *const u8,
    size: usize,
    nmemb: usize,
    stream: *mut c_void,
) -> usize {
    ffi_guard!(CHANNEL, "fwrite", 0, {
        if size == 0 || nmemb == 0 {
            return 0;
        }
        if ptr_val.is_null() {
            crate::crt_str::set_errno(EINVAL);
            return 0;
        }
        let Some(total) = size.checked_mul(nmemb) else {
            crate::crt_str::set_errno(EINVAL);
            return 0;
        };
        // SAFETY: `ptr_val` contient `size * nmemb` octets selon le contrat `fwrite`.
        let data = unsafe { std::slice::from_raw_parts(ptr_val, total) };
        with_file(stream, 0, |st| if st.write_bytes(data) { nmemb } else { 0 })
    })
}

extern "win64" fn crt_fread(dst: *mut u8, size: usize, nmemb: usize, stream: *mut c_void) -> usize {
    ffi_guard!(CHANNEL, "fread", 0, {
        if size == 0 || nmemb == 0 {
            return 0;
        }
        if dst.is_null() {
            crate::crt_str::set_errno(EINVAL);
            return 0;
        }
        let Some(total) = size.checked_mul(nmemb) else {
            crate::crt_str::set_errno(EINVAL);
            return 0;
        };
        // SAFETY: `dst` dispose de `size * nmemb` octets selon le contrat `fread`.
        let buf = unsafe { std::slice::from_raw_parts_mut(dst, total) };
        with_file(stream, 0, |st| st.read_into(buf) / size)
    })
}

extern "win64" fn crt_fseek(stream: *mut c_void, offset: i32, whence: i32) -> i32 {
    crt_fseeki64(stream, i64::from(offset), whence)
}

extern "win64" fn crt_fseeki64(stream: *mut c_void, offset: i64, whence: i32) -> i32 {
    ffi_guard!(CHANNEL, "_fseeki64", -1, {
        with_file(stream, -1, |st| match st.seek(offset, whence) {
            Ok(_) => 0,
            Err(e) => {
                crate::crt_str::set_errno(e);
                -1
            }
        })
    })
}

extern "win64" fn crt_ftelli64(stream: *mut c_void) -> i64 {
    ffi_guard!(CHANNEL, "_ftelli64", -1, {
        with_file(stream, -1, |st| match st.logical_pos() {
            Ok(p) => p,
            Err(e) => {
                crate::crt_str::set_errno(e);
                -1
            }
        })
    })
}

extern "win64" fn crt_ftell(stream: *mut c_void) -> i32 {
    let p = crt_ftelli64(stream);
    if p > i64::from(i32::MAX) {
        crate::crt_str::set_errno(EINVAL);
        return -1;
    }
    p as i32
}

extern "win64" fn crt_fgetpos(stream: *mut c_void, pos: *mut i64) -> i32 {
    ffi_guard!(CHANNEL, "fgetpos", EINVAL, {
        if pos.is_null() || lookup_file(stream).is_none() {
            crate::crt_str::set_errno(EINVAL);
            return EINVAL;
        }
        let p = crt_ftelli64(stream);
        if p < 0 {
            return crate::crt_str::get_errno();
        }
        // SAFETY: pointeur `fpos_t*` (64 bits) non nul.
        unsafe { pos.write_unaligned(p) };
        0
    })
}

extern "win64" fn crt_fsetpos(stream: *mut c_void, pos: *const i64) -> i32 {
    ffi_guard!(CHANNEL, "fsetpos", EINVAL, {
        if pos.is_null() || lookup_file(stream).is_none() {
            crate::crt_str::set_errno(EINVAL);
            return EINVAL;
        }
        // SAFETY: pointeur `fpos_t*` non nul.
        let p = unsafe { pos.read_unaligned() };
        if crt_fseeki64(stream, p, 0) == 0 {
            0
        } else {
            crate::crt_str::get_errno()
        }
    })
}

extern "win64" fn crt_rewind(stream: *mut c_void) {
    ffi_guard!(CHANNEL, "rewind", (), {
        with_file(stream, (), |st| {
            let _ = st.seek(0, 0);
            st.error = false;
            st.eof = false;
        });
    });
}

extern "win64" fn crt_fgetc(stream: *mut c_void) -> i32 {
    ffi_guard!(CHANNEL, "fgetc", EOF, {
        with_file(stream, EOF, |st| st.getc().map_or(EOF, i32::from))
    })
}

extern "win64" fn crt_getchar() -> i32 {
    crt_fgetc(__acrt_iob_func(0))
}

extern "win64" fn crt_ungetc(c: i32, stream: *mut c_void) -> i32 {
    ffi_guard!(CHANNEL, "ungetc", EOF, {
        if c == EOF {
            return EOF;
        }
        with_file(stream, EOF, |st| {
            if !st.readable {
                return EOF;
            }
            if st.last == LastOp::Write && st.flush().is_err() {
                return EOF;
            }
            st.last = LastOp::Read;
            st.unget.push(c as u8);
            st.eof = false;
            i32::from(c as u8)
        })
    })
}

extern "win64" fn crt_fputc(c: i32, stream: *mut c_void) -> i32 {
    ffi_guard!(CHANNEL, "fputc", EOF, {
        let b = c as u8;
        with_file(stream, EOF, |st| {
            if st.write_bytes(&[b]) {
                i32::from(b)
            } else {
                EOF
            }
        })
    })
}

extern "win64" fn crt_putchar(c: i32) -> i32 {
    crt_fputc(c, __acrt_iob_func(1))
}

extern "win64" fn crt_fputs(s: *const i8, stream: *mut c_void) -> i32 {
    ffi_guard!(CHANNEL, "fputs", EOF, {
        if s.is_null() {
            crate::crt_str::set_errno(EINVAL);
            return EOF;
        }
        let data = bytes_vec(s.cast(), None);
        with_file(
            stream,
            EOF,
            |st| if st.write_bytes(&data) { 0 } else { EOF },
        )
    })
}

extern "win64" fn crt_puts(s: *const i8) -> i32 {
    ffi_guard!(CHANNEL, "puts", EOF, {
        if s.is_null() {
            crate::crt_str::set_errno(EINVAL);
            return EOF;
        }
        let mut data = bytes_vec(s.cast(), None);
        data.push(b'\n');
        with_file(__acrt_iob_func(1), EOF, |st| {
            if st.write_bytes(&data) { 0 } else { EOF }
        })
    })
}

extern "win64" fn crt_fgets(buf: *mut u8, n: i32, stream: *mut c_void) -> *mut u8 {
    ffi_guard!(CHANNEL, "fgets", ptr::null_mut(), {
        if (buf.is_null() && n != 0) || n < 0 || stream.is_null() {
            crate::crt_str::set_errno(EINVAL);
            return ptr::null_mut();
        }
        if n == 0 {
            return ptr::null_mut();
        }
        with_file(stream, ptr::null_mut(), |st| {
            let mut i = 0usize;
            let max = (n - 1) as usize;
            while i < max {
                match st.getc() {
                    Some(c) => {
                        // SAFETY: `i < n - 1`, `buf` dispose de `n` octets.
                        unsafe { *buf.add(i) = c };
                        i += 1;
                        if c == b'\n' {
                            break;
                        }
                    }
                    None => {
                        if i == 0 || st.error {
                            return ptr::null_mut();
                        }
                        break;
                    }
                }
            }
            // SAFETY: `i <= n - 1`.
            unsafe { *buf.add(i) = 0 };
            buf
        })
    })
}

extern "win64" fn crt_fgetwc(stream: *mut c_void) -> i32 {
    ffi_guard!(CHANNEL, "fgetwc", WEOF, {
        with_file(stream, WEOF, |st| st.getwc().map_or(WEOF, i32::from))
    })
}

extern "win64" fn crt_fputwc(c: u16, stream: *mut c_void) -> i32 {
    ffi_guard!(CHANNEL, "fputwc", WEOF, {
        with_file(
            stream,
            WEOF,
            |st| {
                if st.putwc(c) { i32::from(c) } else { WEOF }
            },
        )
    })
}

extern "win64" fn crt_ungetwc(c: i32, stream: *mut c_void) -> i32 {
    ffi_guard!(CHANNEL, "ungetwc", WEOF, {
        if c == WEOF {
            return WEOF;
        }
        with_file(stream, WEOF, |st| {
            if !st.readable {
                return WEOF;
            }
            st.wunget.push(c as u16);
            st.eof = false;
            i32::from(c as u16)
        })
    })
}

extern "win64" fn crt_fputws(s: *const u16, stream: *mut c_void) -> i32 {
    ffi_guard!(CHANNEL, "fputws", WEOF, {
        if s.is_null() {
            crate::crt_str::set_errno(EINVAL);
            return WEOF;
        }
        let data = wstr_vec(s, None);
        with_file(stream, WEOF, |st| {
            for &w in &data {
                if !st.putwc(w) {
                    return WEOF;
                }
            }
            0
        })
    })
}

extern "win64" fn crt_fgetws(buf: *mut u16, n: i32, stream: *mut c_void) -> *mut u16 {
    ffi_guard!(CHANNEL, "fgetws", ptr::null_mut(), {
        if (buf.is_null() && n != 0) || n < 0 || stream.is_null() {
            crate::crt_str::set_errno(EINVAL);
            return ptr::null_mut();
        }
        if n == 0 {
            return ptr::null_mut();
        }
        with_file(stream, ptr::null_mut(), |st| {
            let mut i = 0usize;
            let max = (n - 1) as usize;
            while i < max {
                match st.getwc() {
                    Some(c) => {
                        // SAFETY: `i < n - 1`, `buf` dispose de `n` `wchar_t`.
                        unsafe { buf.add(i).write_unaligned(c) };
                        i += 1;
                        if c == u16::from(b'\n') {
                            break;
                        }
                    }
                    None => {
                        if i == 0 || st.error {
                            return ptr::null_mut();
                        }
                        break;
                    }
                }
            }
            // SAFETY: `i <= n - 1`.
            unsafe { buf.add(i).write_unaligned(0) };
            buf
        })
    })
}

extern "win64" fn crt_feof(stream: *mut c_void) -> i32 {
    ffi_guard!(CHANNEL, "feof", 0, {
        with_file(stream, 0, |st| if st.eof { 0x10 } else { 0 })
    })
}

extern "win64" fn crt_ferror(stream: *mut c_void) -> i32 {
    ffi_guard!(CHANNEL, "ferror", 0, {
        with_file(stream, 0, |st| if st.error { 0x20 } else { 0 })
    })
}

extern "win64" fn crt_clearerr(stream: *mut c_void) {
    ffi_guard!(CHANNEL, "clearerr", (), {
        with_file(stream, (), |st| {
            st.error = false;
            st.eof = false;
        });
    });
}

extern "win64" fn crt_fileno(stream: *mut c_void) -> i32 {
    ffi_guard!(CHANNEL, "_fileno", -1, {
        match lookup_file(stream) {
            Some(f) => f.file,
            None => {
                crate::crt_str::set_errno(EINVAL);
                -1
            }
        }
    })
}

extern "win64" fn crt_setvbuf(stream: *mut c_void, _buf: *mut i8, mode: i32, size: usize) -> i32 {
    ffi_guard!(CHANNEL, "setvbuf", -1, {
        let mode = if mode == IOLBF { IOFBF } else { mode };
        if mode != IOFBF && mode != IONBF {
            crate::crt_str::set_errno(EINVAL);
            return -1;
        }
        if mode == IOFBF && !(2..=i32::MAX as usize).contains(&size) {
            crate::crt_str::set_errno(EINVAL);
            return -1;
        }
        with_file(stream, -1, |st| {
            if st.flush().is_err() {
                return -1;
            }
            st.buf_mode = mode;
            st.buf_size = if mode == IONBF { 1 } else { size & !1 };
            0
        })
    })
}

extern "win64" fn crt_setbuf(stream: *mut c_void, buf: *mut i8) {
    let _ = if buf.is_null() {
        crt_setvbuf(stream, buf, IONBF, 0)
    } else {
        crt_setvbuf(stream, buf, IOFBF, 512)
    };
}

extern "win64" fn crt_get_stream_buffer_pointers(
    stream: *mut c_void,
    base: *mut *mut *mut u8,
    ptr_out: *mut *mut *mut u8,
    count: *mut *mut i32,
) -> i32 {
    ffi_guard!(CHANNEL, "_get_stream_buffer_pointers", EINVAL, {
        let Some(f) = lookup_file(stream) else {
            crate::crt_str::set_errno(EINVAL);
            return EINVAL;
        };
        // SAFETY: les champs vivent aussi longtemps que le flux ; pointeurs de sortie optionnels.
        unsafe {
            if !base.is_null() {
                base.write_unaligned(f.base.get());
            }
            if !ptr_out.is_null() {
                ptr_out.write_unaligned(f.ptr.get());
            }
            if !count.is_null() {
                count.write_unaligned(f.cnt.get());
            }
        }
        0
    })
}

extern "win64" fn crt_lock_file(stream: *mut c_void) {
    ffi_guard!(CHANNEL, "_lock_file", (), {
        match lookup_file(stream) {
            Some(f) => f.lock.lock(),
            None => crate::crt_str::set_errno(EINVAL),
        }
    });
}

extern "win64" fn crt_unlock_file(stream: *mut c_void) {
    ffi_guard!(CHANNEL, "_unlock_file", (), {
        match lookup_file(stream) {
            Some(f) => f.lock.unlock(),
            None => crate::crt_str::set_errno(EINVAL),
        }
    });
}

// ---------- Moteur printf (étroit & large) ----------

/// `va_list` Windows x86-64 : tableau plat de slots 64 bits.
struct VaArgs {
    p: *const u64,
    i: usize,
}

impl VaArgs {
    fn new(p: *const c_void) -> Self {
        Self { p: p.cast(), i: 0 }
    }

    fn next(&mut self) -> u64 {
        if self.p.is_null() {
            return 0;
        }
        // SAFETY: sur ABI Win64 chaque argument variadique occupe un slot de 8 octets.
        let v = unsafe { self.p.add(self.i).read_unaligned() };
        self.i += 1;
        v
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SizeMod {
    None,
    Hh,
    H,
    L,
    Ll,
    LongDouble,
    I32,
    I64,
    Wide,
}

impl SizeMod {
    fn is_64(self) -> bool {
        matches!(self, SizeMod::Ll | SizeMod::I64 | SizeMod::LongDouble)
    }
}

fn parse_size(fmt: &[u16], i: &mut usize) -> SizeMod {
    let at = |k: usize| fmt.get(k).copied().unwrap_or(0);
    let c = at(*i);
    match u8::try_from(c).unwrap_or(0) {
        b'h' => {
            *i += 1;
            if at(*i) == u16::from(b'h') {
                *i += 1;
                SizeMod::Hh
            } else {
                SizeMod::H
            }
        }
        b'l' => {
            *i += 1;
            if at(*i) == u16::from(b'l') {
                *i += 1;
                SizeMod::Ll
            } else {
                SizeMod::L
            }
        }
        b'L' => {
            *i += 1;
            SizeMod::LongDouble
        }
        b'w' => {
            *i += 1;
            SizeMod::Wide
        }
        b'j' | b'z' | b't' => {
            *i += 1;
            SizeMod::I64
        }
        b'I' => {
            *i += 1;
            if at(*i) == u16::from(b'6') && at(*i + 1) == u16::from(b'4') {
                *i += 2;
                SizeMod::I64
            } else if at(*i) == u16::from(b'3') && at(*i + 1) == u16::from(b'2') {
                *i += 2;
                SizeMod::I32
            } else {
                SizeMod::I64
            }
        }
        _ => SizeMod::None,
    }
}

/// Le spécificateur `%s`/`%c` désigne-t-il une chaîne large ?
fn is_wide_arg(size: SizeMod, upper: bool, wide_fn: bool, legacy: bool) -> bool {
    match size {
        SizeMod::H | SizeMod::Hh => false,
        SizeMod::L | SizeMod::Wide | SizeMod::Ll => true,
        _ => {
            let natural = wide_fn && legacy;
            if upper { !natural } else { natural }
        }
    }
}

struct PfOut {
    units: Vec<u16>,
    wide: bool,
}

impl PfOut {
    fn ascii(&mut self, s: &str) {
        self.units.extend(s.bytes().map(u16::from));
    }

    fn rep(&mut self, c: u8, n: usize) {
        self.units.extend(std::iter::repeat_n(u16::from(c), n));
    }

    /// Émet `prefix` + `body` cadrés sur `width`.
    fn emit(&mut self, prefix: &str, body: &[u16], width: usize, left: bool, zero: bool) {
        let len = prefix.len() + body.len();
        let pad = width.saturating_sub(len);
        if left {
            self.ascii(prefix);
            self.units.extend_from_slice(body);
            self.rep(b' ', pad);
        } else if zero {
            self.ascii(prefix);
            self.rep(b'0', pad);
            self.units.extend_from_slice(body);
        } else {
            self.rep(b' ', pad);
            self.ascii(prefix);
            self.units.extend_from_slice(body);
        }
    }
}

fn ascii_units(s: &str) -> Vec<u16> {
    s.bytes().map(u16::from).collect()
}

/// Convertit des unités larges vers la cible (étroite : locale « C », > 0xFF = `EILSEQ`).
fn wide_to_target(src: &[u16], wide_target: bool) -> Result<Vec<u16>, i32> {
    if !wide_target && src.iter().any(|&w| w > 0xFF) {
        return Err(EILSEQ);
    }
    Ok(src.to_vec())
}

fn nan_text(v: f64, upper: bool) -> &'static str {
    let bits = v.to_bits();
    let quiet = bits & (1 << 51) != 0;
    let payload = bits & ((1 << 51) - 1);
    match (quiet, payload == 0 && v.is_sign_negative(), upper) {
        (false, _, false) => "nan(snan)",
        (false, _, true) => "NAN(SNAN)",
        (true, true, false) => "nan(ind)",
        (true, true, true) => "NAN(IND)",
        (true, false, false) => "nan",
        (true, false, true) => "NAN",
    }
}

fn exp_form(a: f64, p: usize, upper: bool, alt: bool, three: bool) -> String {
    let s = format!("{a:.p$e}");
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let mut m = mant.to_string();
    if alt && p == 0 {
        m.push('.');
    }
    let ed = if three { 3 } else { 2 };
    format!(
        "{m}{}{}{:0ed$}",
        if upper { 'E' } else { 'e' },
        if exp < 0 { '-' } else { '+' },
        exp.unsigned_abs()
    )
}

fn hex_float(a: f64, precision: Option<usize>, upper: bool, alt: bool) -> String {
    let bits = a.to_bits();
    let raw_exp = ((bits >> 52) & 0x7FF) as i32;
    let mut mant = bits & ((1u64 << 52) - 1);
    let (mut lead, exp) = if raw_exp == 0 {
        (0u64, if mant == 0 { 0 } else { -1022 })
    } else {
        (1u64, raw_exp - 1023)
    };
    let p = precision.unwrap_or(13);
    let digits = if p < 13 {
        let shift = (13 - p) * 4;
        let rem = mant & ((1u64 << shift) - 1);
        let half = 1u64 << (shift - 1);
        mant >>= shift;
        if rem > half || (rem == half && mant & 1 == 1) {
            mant += 1;
            if p == 0 || mant >> (p * 4) != 0 {
                mant &= if p == 0 { 0 } else { (1u64 << (p * 4)) - 1 };
                lead += 1;
            }
        }
        if p == 0 {
            String::new()
        } else {
            format!("{mant:0p$x}")
        }
    } else {
        format!("{mant:013x}{}", "0".repeat(p - 13))
    };
    let dot = if p > 0 || alt { "." } else { "" };
    let s = format!(
        "0x{lead}{dot}{digits}p{}{}",
        if exp < 0 { '-' } else { '+' },
        exp.unsigned_abs()
    );
    if upper { s.to_ascii_uppercase() } else { s }
}

fn strip_g(s: String, alt: bool) -> String {
    if alt {
        return if s.contains('.') { s } else { format!("{s}.") };
    }
    let (mant, exp) = match s.find(['e', 'E']) {
        Some(k) => (s[..k].to_string(), s[k..].to_string()),
        None => (s.clone(), String::new()),
    };
    let mant = if mant.contains('.') {
        mant.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        mant
    };
    format!("{mant}{exp}")
}

/// Formate un flottant ; renvoie (corps, fini ?).
fn format_float(
    v: f64,
    conv: u8,
    precision: Option<usize>,
    alt: bool,
    options: u64,
) -> (String, bool) {
    let upper = conv.is_ascii_uppercase();
    let a = v.abs();
    if v.is_nan() {
        return (nan_text(v, upper).to_string(), false);
    }
    if v.is_infinite() {
        return ((if upper { "INF" } else { "inf" }).to_string(), false);
    }
    let three = options & PF_LEGACY_THREE_DIGIT_EXPONENTS != 0;
    let s = match conv.to_ascii_lowercase() {
        b'f' => {
            let p = precision.unwrap_or(6);
            let mut s = format!("{a:.p$}");
            if alt && p == 0 {
                s.push('.');
            }
            s
        }
        b'e' => exp_form(a, precision.unwrap_or(6), upper, alt, three),
        b'g' => {
            let p = match precision.unwrap_or(6) {
                0 => 1,
                p => p,
            };
            let x: i64 = if a == 0.0 {
                0
            } else {
                let e = format!("{a:.prec$e}", prec = p - 1);
                e.split_once('e')
                    .and_then(|(_, x)| x.parse().ok())
                    .unwrap_or(0)
            };
            let s = if x < p as i64 && x >= -4 {
                format!("{a:.prec$}", prec = (p as i64 - 1 - x) as usize)
            } else {
                exp_form(a, p - 1, upper, false, three)
            };
            strip_g(s, alt)
        }
        _ => hex_float(a, precision, upper, alt),
    };
    (s, true)
}

/// Moteur `printf` commun. `fmt` : unités (octets pour l'étroit). Sortie en unités de la cible.
pub(crate) fn printf_engine(
    fmt: &[u16],
    wide_fn: bool,
    options: u64,
    arglist: *const c_void,
) -> Result<Vec<u16>, i32> {
    let mut out = PfOut {
        units: Vec::with_capacity(fmt.len() + 16),
        wide: wide_fn,
    };
    let mut va = VaArgs::new(arglist);
    let legacy = options & PF_LEGACY_WIDE_SPECIFIERS != 0;
    let at = |k: usize| fmt.get(k).copied().unwrap_or(0);
    let mut i = 0usize;
    while i < fmt.len() {
        let c = fmt[i];
        i += 1;
        if c != u16::from(b'%') {
            out.units.push(c);
            continue;
        }
        let (mut left, mut plus, mut space, mut alt, mut zero) =
            (false, false, false, false, false);
        loop {
            match u8::try_from(at(i)).unwrap_or(0) {
                b'-' => left = true,
                b'+' => plus = true,
                b' ' => space = true,
                b'#' => alt = true,
                b'0' => zero = true,
                _ => break,
            }
            i += 1;
        }
        let mut width = 0usize;
        if at(i) == u16::from(b'*') {
            i += 1;
            let w = va.next() as i32;
            if w < 0 {
                left = true;
            }
            width = w.unsigned_abs() as usize;
        } else {
            while (u16::from(b'0')..=u16::from(b'9')).contains(&at(i)) {
                width = width.saturating_mul(10) + usize::from(at(i) - u16::from(b'0'));
                i += 1;
            }
        }
        let mut precision: Option<usize> = None;
        if at(i) == u16::from(b'.') {
            i += 1;
            if at(i) == u16::from(b'*') {
                i += 1;
                let p = va.next() as i32;
                precision = (p >= 0).then_some(p as usize);
            } else {
                let mut p = 0usize;
                while (u16::from(b'0')..=u16::from(b'9')).contains(&at(i)) {
                    p = p.saturating_mul(10) + usize::from(at(i) - u16::from(b'0'));
                    i += 1;
                }
                precision = Some(p);
            }
        }
        let size = parse_size(fmt, &mut i);
        if i >= fmt.len() {
            return Err(EINVAL);
        }
        let conv = u8::try_from(fmt[i]).map_err(|_| EINVAL)?;
        i += 1;
        if left {
            zero = false;
        }
        match conv {
            b'%' => out.units.push(u16::from(b'%')),
            b'd' | b'i' => {
                let raw = va.next();
                let v: i64 = match size {
                    SizeMod::Hh => i64::from(raw as i8),
                    SizeMod::H => i64::from(raw as i16),
                    s if s.is_64() => raw as i64,
                    _ => i64::from(raw as i32),
                };
                let mut digits = if precision == Some(0) && v == 0 {
                    String::new()
                } else {
                    v.unsigned_abs().to_string()
                };
                if let Some(p) = precision {
                    digits = format!("{digits:0>p$}");
                }
                let sign = if v < 0 {
                    "-"
                } else if plus {
                    "+"
                } else if space {
                    " "
                } else {
                    ""
                };
                out.emit(
                    sign,
                    &ascii_units(&digits),
                    width,
                    left,
                    zero && precision.is_none(),
                );
            }
            b'u' | b'o' | b'x' | b'X' => {
                let raw = va.next();
                let v: u64 = match size {
                    SizeMod::Hh => u64::from(raw as u8),
                    SizeMod::H => u64::from(raw as u16),
                    s if s.is_64() => raw,
                    _ => u64::from(raw as u32),
                };
                let mut digits = if precision == Some(0) && v == 0 {
                    String::new()
                } else {
                    match conv {
                        b'u' => v.to_string(),
                        b'o' => format!("{v:o}"),
                        b'x' => format!("{v:x}"),
                        _ => format!("{v:X}"),
                    }
                };
                if let Some(p) = precision {
                    digits = format!("{digits:0>p$}");
                }
                if conv == b'o' && alt && !digits.starts_with('0') {
                    digits.insert(0, '0');
                }
                let prefix = match conv {
                    b'x' if alt && v != 0 => "0x",
                    b'X' if alt && v != 0 => "0X",
                    _ => "",
                };
                out.emit(
                    prefix,
                    &ascii_units(&digits),
                    width,
                    left,
                    zero && precision.is_none(),
                );
            }
            b'p' => {
                let raw = va.next();
                let digits = format!("{raw:016X}");
                out.emit("", &ascii_units(&digits), width, left, false);
            }
            b'c' | b'C' => {
                let raw = va.next();
                let body = if is_wide_arg(size, conv == b'C', wide_fn, legacy) {
                    wide_to_target(&[raw as u16], wide_fn)?
                } else {
                    vec![u16::from(raw as u8)]
                };
                out.emit("", &body, width, left, zero);
            }
            b's' | b'S' => {
                let p = va.next() as usize;
                let wide_arg = is_wide_arg(size, conv == b'S', wide_fn, legacy);
                let body = if p == 0 {
                    let s = ascii_units("(null)");
                    match precision {
                        Some(n) => s[..n.min(s.len())].to_vec(),
                        None => s,
                    }
                } else if wide_arg {
                    wide_to_target(&wstr_vec(p as *const u16, precision), wide_fn)?
                } else {
                    bytes_vec(p as *const u8, precision)
                        .into_iter()
                        .map(u16::from)
                        .collect()
                };
                out.emit("", &body, width, left, zero);
            }
            b'Z' => {
                // ANSI_STRING / UNICODE_STRING : { USHORT Length; USHORT Max; PVOID Buffer; }
                let p = va.next() as usize;
                let wide_arg = matches!(size, SizeMod::L | SizeMod::Wide)
                    || (wide_fn && legacy && !matches!(size, SizeMod::H));
                let body = if p == 0 {
                    ascii_units("(null)")
                } else {
                    // SAFETY: structure `*_STRING` fournie par l'invité.
                    let (len, buf) = unsafe {
                        (
                            (p as *const u16).read_unaligned() as usize,
                            ((p + 8) as *const usize).read_unaligned(),
                        )
                    };
                    if buf == 0 {
                        ascii_units("(null)")
                    } else if wide_arg {
                        let n = (len / 2).min(precision.unwrap_or(usize::MAX));
                        // SAFETY: tampon de `Length` octets.
                        let s = unsafe { std::slice::from_raw_parts(buf as *const u16, n) };
                        wide_to_target(s, wide_fn)?
                    } else {
                        let n = len.min(precision.unwrap_or(usize::MAX));
                        // SAFETY: tampon de `Length` octets.
                        let s = unsafe { std::slice::from_raw_parts(buf as *const u8, n) };
                        s.iter().map(|&b| u16::from(b)).collect()
                    }
                };
                out.emit("", &body, width, left, zero);
            }
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'a' | b'A' => {
                let v = f64::from_bits(va.next());
                let (body, finite) = format_float(v, conv, precision, alt, options);
                let sign = if v.is_sign_negative() {
                    "-"
                } else if plus {
                    "+"
                } else if space {
                    " "
                } else {
                    ""
                };
                out.emit(sign, &ascii_units(&body), width, left, zero && finite);
            }
            // `%n` désactivé par défaut dans l'UCRT (`_set_printf_count_output`).
            _ => return Err(EINVAL),
        }
    }
    let _ = out.wide;
    Ok(out.units)
}

fn narrow_fmt(p: *const i8) -> Vec<u16> {
    bytes_vec(p.cast(), None)
        .into_iter()
        .map(u16::from)
        .collect()
}

/// Écrit des unités formatées dans un tampon invité (`u8` ou `u16`).
trait CharUnit: Copy {
    const NUL: Self;
    fn from_unit(u: u16) -> Self;
}

impl CharUnit for u8 {
    const NUL: Self = 0;
    fn from_unit(u: u16) -> Self {
        u as u8
    }
}

impl CharUnit for u16 {
    const NUL: Self = 0;
    fn from_unit(u: u16) -> Self {
        u
    }
}

fn put_units<T: CharUnit>(buf: *mut T, units: &[u16], nul: bool) {
    for (k, &u) in units.iter().enumerate() {
        // SAFETY: l'appelant garantit `units.len() (+1)` éléments disponibles dans `buf`.
        unsafe { buf.add(k).write_unaligned(T::from_unit(u)) };
    }
    if nul {
        // SAFETY: idem.
        unsafe { buf.add(units.len()).write_unaligned(T::NUL) };
    }
}

fn put_nul<T: CharUnit>(buf: *mut T) {
    // SAFETY: `buf` non nul avec au moins un élément (vérifié par l'appelant).
    unsafe { buf.write_unaligned(T::NUL) };
}

/// Sémantique `__stdio_common_vs(w)printf` (snprintf C99 / `_vsnprintf` legacy).
fn sprintf_common<T: CharUnit>(
    options: u64,
    buf: *mut T,
    count: usize,
    res: Result<Vec<u16>, i32>,
) -> i32 {
    if buf.is_null() && count != 0 {
        crate::crt_str::set_errno(EINVAL);
        return -1;
    }
    let units = match res {
        Ok(u) => u,
        Err(e) => {
            crate::crt_str::set_errno(e);
            if !buf.is_null() && count > 0 {
                put_nul(buf);
            }
            return -1;
        }
    };
    let len = units.len();
    if buf.is_null() {
        return len as i32;
    }
    if options & PF_STANDARD_SNPRINTF_BEHAVIOR != 0 {
        let n = len.min(count - 1);
        put_units(buf, &units[..n], true);
        return len as i32;
    }
    if len < count {
        put_units(buf, &units, true);
        return len as i32;
    }
    let legacy_nul = options & PF_LEGACY_VSPRINTF_NULL_TERMINATION != 0;
    if legacy_nul {
        put_units(buf, &units[..count], false);
        if len == count {
            return len as i32;
        }
    } else {
        put_units(buf, &units[..count - 1], true);
    }
    -1
}

/// Sémantique `__stdio_common_vs(w)printf_s` (`sprintf_s`).
fn sprintf_s_common<T: CharUnit>(buf: *mut T, count: usize, res: Result<Vec<u16>, i32>) -> i32 {
    if buf.is_null() || count == 0 {
        crate::crt_str::set_errno(EINVAL);
        return -1;
    }
    match res {
        Ok(units) if units.len() < count => {
            put_units(buf, &units, true);
            units.len() as i32
        }
        Ok(_) => {
            put_nul(buf);
            crate::crt_str::set_errno(ERANGE);
            -1
        }
        Err(e) => {
            put_nul(buf);
            crate::crt_str::set_errno(e);
            -1
        }
    }
}

/// Sémantique `__stdio_common_vsn(w)printf_s` (`_snprintf_s`, `_TRUNCATE`).
fn snprintf_s_common<T: CharUnit>(
    buf: *mut T,
    count: usize,
    max_count: usize,
    res: Result<Vec<u16>, i32>,
) -> i32 {
    if buf.is_null() && count == 0 && max_count == 0 {
        return 0;
    }
    if buf.is_null() || count == 0 {
        crate::crt_str::set_errno(EINVAL);
        return -1;
    }
    let units = match res {
        Ok(u) => u,
        Err(e) => {
            put_nul(buf);
            crate::crt_str::set_errno(e);
            return -1;
        }
    };
    let truncate = max_count == usize::MAX;
    let limit = if truncate {
        count - 1
    } else {
        max_count.min(count - 1)
    };
    if units.len() <= limit {
        put_units(buf, &units, true);
        return units.len() as i32;
    }
    if truncate || max_count < count {
        put_units(buf, &units[..limit], true);
        return -1;
    }
    put_nul(buf);
    crate::crt_str::set_errno(ERANGE);
    -1
}

fn fprintf_common(stream: *mut c_void, res: Result<Vec<u16>, i32>, wide: bool) -> i32 {
    let units = match res {
        Ok(u) => u,
        Err(e) => {
            crate::crt_str::set_errno(e);
            return -1;
        }
    };
    with_file(stream, -1, |st| {
        let ok = if wide {
            units.iter().all(|&w| st.putwc(w))
        } else {
            let bytes: Vec<u8> = units.iter().map(|&u| u as u8).collect();
            st.write_bytes(&bytes)
        };
        if ok { units.len() as i32 } else { -1 }
    })
}

extern "win64" fn __stdio_common_vfprintf(
    options: u64,
    stream: *mut c_void,
    format: *const i8,
    _locale: *mut c_void,
    arglist: *mut c_void,
) -> i32 {
    ffi_guard!(CHANNEL, "__stdio_common_vfprintf", -1, {
        if format.is_null() || stream.is_null() {
            crate::crt_str::set_errno(EINVAL);
            return -1;
        }
        let res = printf_engine(&narrow_fmt(format), false, options, arglist);
        fprintf_common(stream, res, false)
    })
}

extern "win64" fn __stdio_common_vfwprintf(
    options: u64,
    stream: *mut c_void,
    format: *const u16,
    _locale: *mut c_void,
    arglist: *mut c_void,
) -> i32 {
    ffi_guard!(CHANNEL, "__stdio_common_vfwprintf", -1, {
        if format.is_null() || stream.is_null() {
            crate::crt_str::set_errno(EINVAL);
            return -1;
        }
        let res = printf_engine(&wstr_vec(format, None), true, options, arglist);
        fprintf_common(stream, res, true)
    })
}

extern "win64" fn __stdio_common_vsprintf(
    options: u64,
    buf: *mut u8,
    buf_count: usize,
    format: *const i8,
    _locale: *mut c_void,
    arglist: *mut c_void,
) -> i32 {
    ffi_guard!(CHANNEL, "__stdio_common_vsprintf", -1, {
        if format.is_null() {
            crate::crt_str::set_errno(EINVAL);
            return -1;
        }
        let res = printf_engine(&narrow_fmt(format), false, options, arglist);
        sprintf_common(options, buf, buf_count, res)
    })
}

extern "win64" fn __stdio_common_vsprintf_s(
    options: u64,
    buf: *mut u8,
    buf_count: usize,
    format: *const i8,
    _locale: *mut c_void,
    arglist: *mut c_void,
) -> i32 {
    ffi_guard!(CHANNEL, "__stdio_common_vsprintf_s", -1, {
        if format.is_null() {
            if !buf.is_null() && buf_count > 0 {
                put_nul(buf);
            }
            crate::crt_str::set_errno(EINVAL);
            return -1;
        }
        let res = printf_engine(&narrow_fmt(format), false, options, arglist);
        sprintf_s_common(buf, buf_count, res)
    })
}

extern "win64" fn __stdio_common_vsnprintf_s(
    options: u64,
    buf: *mut u8,
    buf_count: usize,
    max_count: usize,
    format: *const i8,
    _locale: *mut c_void,
    arglist: *mut c_void,
) -> i32 {
    ffi_guard!(CHANNEL, "__stdio_common_vsnprintf_s", -1, {
        if format.is_null() {
            crate::crt_str::set_errno(EINVAL);
            return -1;
        }
        let res = printf_engine(&narrow_fmt(format), false, options, arglist);
        snprintf_s_common(buf, buf_count, max_count, res)
    })
}

extern "win64" fn __stdio_common_vswprintf(
    options: u64,
    buf: *mut u16,
    buf_count: usize,
    format: *const u16,
    _locale: *mut c_void,
    arglist: *mut c_void,
) -> i32 {
    ffi_guard!(CHANNEL, "__stdio_common_vswprintf", -1, {
        if format.is_null() {
            crate::crt_str::set_errno(EINVAL);
            return -1;
        }
        let res = printf_engine(&wstr_vec(format, None), true, options, arglist);
        sprintf_common(options, buf, buf_count, res)
    })
}

extern "win64" fn __stdio_common_vswprintf_s(
    options: u64,
    buf: *mut u16,
    buf_count: usize,
    format: *const u16,
    _locale: *mut c_void,
    arglist: *mut c_void,
) -> i32 {
    ffi_guard!(CHANNEL, "__stdio_common_vswprintf_s", -1, {
        if format.is_null() {
            if !buf.is_null() && buf_count > 0 {
                put_nul(buf);
            }
            crate::crt_str::set_errno(EINVAL);
            return -1;
        }
        let res = printf_engine(&wstr_vec(format, None), true, options, arglist);
        sprintf_s_common(buf, buf_count, res)
    })
}

extern "win64" fn __stdio_common_vsnwprintf_s(
    options: u64,
    buf: *mut u16,
    buf_count: usize,
    max_count: usize,
    format: *const u16,
    _locale: *mut c_void,
    arglist: *mut c_void,
) -> i32 {
    ffi_guard!(CHANNEL, "__stdio_common_vsnwprintf_s", -1, {
        if format.is_null() {
            crate::crt_str::set_errno(EINVAL);
            return -1;
        }
        let res = printf_engine(&wstr_vec(format, None), true, options, arglist);
        snprintf_s_common(buf, buf_count, max_count, res)
    })
}

// ---------- Moteur scanf (étroit & large) ----------

trait ScanSrc {
    fn get(&mut self) -> Option<u16>;
    fn unget(&mut self, c: u16);
}

struct StrSrc<'a> {
    data: &'a [u16],
    pos: usize,
}

impl ScanSrc for StrSrc<'_> {
    fn get(&mut self) -> Option<u16> {
        let c = self.data.get(self.pos).copied();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }
    fn unget(&mut self, _c: u16) {
        self.pos = self.pos.saturating_sub(1);
    }
}

struct FileSrc<'a> {
    st: &'a mut FileState,
    wide: bool,
}

impl ScanSrc for FileSrc<'_> {
    fn get(&mut self) -> Option<u16> {
        if self.wide {
            self.st.getwc()
        } else {
            self.st.getc().map(u16::from)
        }
    }
    fn unget(&mut self, c: u16) {
        if self.wide {
            self.st.wunget.push(c);
        } else {
            self.st.unget.push(c as u8);
        }
        self.st.eof = false;
    }
}

struct Rd<'a> {
    src: &'a mut dyn ScanSrc,
    n: usize,
    hit_eof: bool,
}

impl Rd<'_> {
    fn get(&mut self) -> Option<u16> {
        let c = self.src.get();
        match c {
            Some(_) => self.n += 1,
            None => self.hit_eof = true,
        }
        c
    }
    /// Lit un caractère dans la limite `left` (largeur de champ).
    fn get_w(&mut self, left: &mut usize) -> Option<u16> {
        if *left == 0 {
            return None;
        }
        let c = self.get()?;
        *left -= 1;
        Some(c)
    }
    fn unget(&mut self, c: u16) {
        self.src.unget(c);
        self.n -= 1;
    }
    fn skip_ws(&mut self) -> bool {
        loop {
            match self.get() {
                Some(c) if is_space(c) => {}
                Some(c) => {
                    self.unget(c);
                    return true;
                }
                None => return false,
            }
        }
    }
}

fn is_space(c: u16) -> bool {
    matches!(c, 0x20 | 0x09..=0x0D)
}

fn digit_val(c: u16, base: u32) -> Option<u32> {
    char::from_u32(u32::from(c)).and_then(|ch| ch.to_digit(base))
}

/// Lit un entier (base 0 = auto). `None` = échec de correspondance.
fn scan_int(rd: &mut Rd<'_>, width: usize, mut base: u32) -> Option<u64> {
    let mut left = width;
    let mut neg = false;
    let mut c = rd.get_w(&mut left)?;
    if c == u16::from(b'+') || c == u16::from(b'-') {
        neg = c == u16::from(b'-');
        c = rd.get_w(&mut left)?;
    }
    let mut digits = 0usize;
    let mut val: u64 = 0;
    if (base == 0 || base == 16) && c == u16::from(b'0') {
        digits = 1;
        match rd.get_w(&mut left) {
            Some(x) if x == u16::from(b'x') || x == u16::from(b'X') => {
                base = 16;
                digits = 0;
                match rd.get_w(&mut left) {
                    Some(n) if digit_val(n, 16).is_some() => c = n,
                    other => {
                        if let Some(n) = other {
                            rd.unget(n);
                        }
                        return Some(0);
                    }
                }
            }
            Some(x) => {
                if base == 0 {
                    base = 8;
                }
                c = x;
            }
            None => return Some(0),
        }
    } else if base == 0 {
        base = 10;
    }
    loop {
        match digit_val(c, base) {
            Some(d) => {
                val = val.wrapping_mul(u64::from(base)).wrapping_add(u64::from(d));
                digits += 1;
            }
            None => {
                rd.unget(c);
                break;
            }
        }
        match rd.get_w(&mut left) {
            Some(n) => c = n,
            None => break,
        }
    }
    if digits == 0 {
        return None;
    }
    Some(if neg { val.wrapping_neg() } else { val })
}

fn lower(c: u16) -> u16 {
    if (u16::from(b'A')..=u16::from(b'Z')).contains(&c) {
        c + 32
    } else {
        c
    }
}

/// Analyse un flottant hexadécimal (`0x1.8p3`) déjà collecté (sans signe ni préfixe).
fn parse_hex_float(s: &str) -> Option<f64> {
    let (mant, exp) = match s.find(['p', 'P']) {
        Some(k) => (&s[..k], s[k + 1..].parse::<i32>().ok()?),
        None => (s, 0),
    };
    let mut m: u64 = 0;
    let mut e: i32 = exp;
    let mut seen_dot = false;
    let mut any = false;
    for ch in mant.chars() {
        if ch == '.' {
            seen_dot = true;
            continue;
        }
        let d = ch.to_digit(16)?;
        any = true;
        if m >> 60 == 0 {
            m = (m << 4) | u64::from(d);
            if seen_dot {
                e -= 4;
            }
        } else if !seen_dot {
            e += 4;
        }
    }
    if !any {
        return None;
    }
    Some(crate::crt_math::scalbn64(m as f64, e))
}

/// Lit un flottant (`%e %f %g %a`). `None` = échec de correspondance.
fn scan_float(rd: &mut Rd<'_>, width: usize) -> Option<f64> {
    let mut left = width;
    let mut s = String::new();
    let mut c = rd.get_w(&mut left)?;
    let mut neg = false;
    if c == u16::from(b'+') || c == u16::from(b'-') {
        neg = c == u16::from(b'-');
        c = rd.get_w(&mut left)?;
    }
    let sign = |v: f64| if neg { -v } else { v };
    let lc = lower(c);
    if lc == u16::from(b'i') || lc == u16::from(b'n') {
        let mut word: Vec<u16> = vec![c];
        let target: &[u8] = if lc == u16::from(b'i') {
            b"infinity"
        } else {
            b"nan"
        };
        while word.len() < target.len() {
            match rd.get_w(&mut left) {
                Some(n) if lower(n) == u16::from(target[word.len()]) => word.push(n),
                Some(n) => {
                    rd.unget(n);
                    break;
                }
                None => break,
            }
        }
        if lc == u16::from(b'i') {
            if word.len() == 8 {
                return Some(sign(f64::INFINITY));
            }
            if word.len() >= 3 {
                for &w in word[3..].iter().rev() {
                    rd.unget(w);
                }
                return Some(sign(f64::INFINITY));
            }
        } else if word.len() == 3 {
            // `nan(...)` optionnel.
            if let Some(p) = rd.get_w(&mut left) {
                if p == u16::from(b'(') {
                    loop {
                        match rd.get_w(&mut left) {
                            Some(x) if x == u16::from(b')') => break,
                            Some(x)
                                if char::from_u32(u32::from(x))
                                    .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_') => {}
                            Some(x) => {
                                rd.unget(x);
                                break;
                            }
                            None => break,
                        }
                    }
                } else {
                    rd.unget(p);
                }
            }
            return Some(sign(f64::NAN));
        }
        for &w in word[1..].iter().rev() {
            rd.unget(w);
        }
        rd.unget(c);
        return None;
    }
    // Hexadécimal ?
    if c == u16::from(b'0') {
        match rd.get_w(&mut left) {
            Some(x) if lower(x) == u16::from(b'x') => {
                let mut hs = String::new();
                let mut seen_p = false;
                while let Some(n) = rd.get_w(&mut left) {
                    let ch = char::from_u32(u32::from(n)).unwrap_or('\0');
                    let ok = if seen_p {
                        ch.is_ascii_digit()
                            || ((ch == '+' || ch == '-') && hs.ends_with(['p', 'P']))
                    } else if ch == 'p' || ch == 'P' {
                        seen_p = true;
                        true
                    } else {
                        ch.is_ascii_hexdigit() || (ch == '.' && !hs.contains('.'))
                    };
                    if !ok {
                        rd.unget(n);
                        break;
                    }
                    hs.push(ch);
                }
                return Some(sign(parse_hex_float(&hs).unwrap_or(0.0)));
            }
            Some(x) => {
                s.push('0');
                c = x;
            }
            None => return Some(sign(0.0)),
        }
    }
    // Décimal : chiffres [. chiffres] [e [signe] chiffres]
    let mut digits = s.len();
    let mut seen_dot = false;
    let mut cur = Some(c);
    while let Some(n) = cur {
        let ch = char::from_u32(u32::from(n)).unwrap_or('\0');
        if ch.is_ascii_digit() {
            digits += 1;
            s.push(ch);
        } else if ch == '.' && !seen_dot {
            seen_dot = true;
            s.push(ch);
        } else {
            break;
        }
        cur = rd.get_w(&mut left);
    }
    if digits == 0 {
        if let Some(n) = cur {
            rd.unget(n);
        }
        return None;
    }
    if let Some(n) = cur
        && lower(n) == u16::from(b'e')
    {
        let mut tail: Vec<u16> = vec![n];
        let mut exp = String::from("e");
        let mut nx = rd.get_w(&mut left);
        if let Some(sg) = nx
            && (sg == u16::from(b'+') || sg == u16::from(b'-'))
        {
            tail.push(sg);
            exp.push(sg as u8 as char);
            nx = rd.get_w(&mut left);
        }
        let mut exp_digits = 0;
        while let Some(d) = nx {
            if (u16::from(b'0')..=u16::from(b'9')).contains(&d) {
                exp.push(d as u8 as char);
                exp_digits += 1;
                nx = rd.get_w(&mut left);
            } else {
                break;
            }
        }
        if let Some(d) = nx {
            rd.unget(d);
        }
        if exp_digits > 0 {
            s.push_str(&exp);
        } else {
            for &t in tail.iter().rev() {
                rd.unget(t);
            }
        }
    } else if let Some(n) = cur {
        rd.unget(n);
    }
    s.parse::<f64>().ok().map(sign)
}

fn store_int(p: usize, size: SizeMod, v: u64) {
    // SAFETY: pointeur de destination fourni par l'invité pour la conversion correspondante.
    unsafe {
        match size {
            SizeMod::Hh => (p as *mut u8).write_unaligned(v as u8),
            SizeMod::H => (p as *mut u16).write_unaligned(v as u16),
            s if s.is_64() => (p as *mut u64).write_unaligned(v),
            _ => (p as *mut u32).write_unaligned(v as u32),
        }
    }
}

/// Moteur `scanf` commun. Renvoie le nombre de champs affectés ou `EOF`.
fn scanf_engine(
    fmt: &[u16],
    wide_fn: bool,
    options: u64,
    src: &mut dyn ScanSrc,
    arglist: *const c_void,
) -> i32 {
    let secure = options & SF_SECURECRT != 0;
    let legacy = options & SF_LEGACY_WIDE_SPECIFIERS != 0;
    let mut va = VaArgs::new(arglist);
    let mut rd = Rd {
        src,
        n: 0,
        hit_eof: false,
    };
    let mut assigned = 0i32;
    let mut conversions = 0usize;
    let at = |k: usize| fmt.get(k).copied().unwrap_or(0);
    let input_failure = |assigned: i32, conversions: usize| {
        if conversions == 0 { EOF } else { assigned }
    };
    let mut i = 0usize;
    while i < fmt.len() {
        let c = fmt[i];
        i += 1;
        if is_space(c) {
            rd.skip_ws();
            continue;
        }
        if c != u16::from(b'%') {
            match rd.get() {
                Some(x) if x == c => continue,
                Some(x) => {
                    rd.unget(x);
                    return assigned;
                }
                None => return input_failure(assigned, conversions),
            }
        }
        let suppress = at(i) == u16::from(b'*');
        if suppress {
            i += 1;
        }
        let mut width = 0usize;
        while (u16::from(b'0')..=u16::from(b'9')).contains(&at(i)) {
            width = width.saturating_mul(10) + usize::from(at(i) - u16::from(b'0'));
            i += 1;
        }
        let size = parse_size(fmt, &mut i);
        let Some(conv) = fmt.get(i).and_then(|&u| u8::try_from(u).ok()) else {
            return assigned;
        };
        i += 1;
        if !matches!(conv, b'[' | b'c' | b'C' | b'n') && !rd.skip_ws() {
            return input_failure(assigned, conversions);
        }
        let w = if width == 0 { usize::MAX } else { width };
        match conv {
            b'%' => match rd.get() {
                Some(x) if x == u16::from(b'%') => {}
                Some(x) => {
                    rd.unget(x);
                    return assigned;
                }
                None => return input_failure(assigned, conversions),
            },
            b'n' => {
                if !suppress {
                    store_int(va.next() as usize, size, rd.n as u64);
                }
            }
            b'd' | b'i' | b'u' | b'o' | b'x' | b'X' | b'p' => {
                let base = match conv {
                    b'd' | b'u' => 10,
                    b'i' => 0,
                    b'o' => 8,
                    _ => 16,
                };
                let Some(v) = scan_int(&mut rd, w, base) else {
                    return if rd.hit_eof && rd.n == 0 {
                        input_failure(assigned, conversions)
                    } else {
                        assigned
                    };
                };
                conversions += 1;
                if !suppress {
                    let sz = if conv == b'p' { SizeMod::I64 } else { size };
                    store_int(va.next() as usize, sz, v);
                    assigned += 1;
                }
            }
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'a' | b'A' => {
                let Some(v) = scan_float(&mut rd, w) else {
                    return assigned;
                };
                conversions += 1;
                if !suppress {
                    let p = va.next() as usize;
                    // SAFETY: pointeur `float*`/`double*` fourni par l'invité.
                    unsafe {
                        if matches!(size, SizeMod::L | SizeMod::LongDouble | SizeMod::Ll) {
                            (p as *mut f64).write_unaligned(v);
                        } else {
                            (p as *mut f32).write_unaligned(v as f32);
                        }
                    }
                    assigned += 1;
                }
            }
            b's' | b'S' | b'c' | b'C' | b'[' => {
                let upper = conv == b'S' || conv == b'C';
                let wide_dest = is_wide_arg(size, upper, wide_fn, legacy);
                let mut collected: Vec<u16> = Vec::new();
                let mut left = if matches!(conv, b'c' | b'C') && width == 0 {
                    1
                } else {
                    w
                };
                if conv == b'[' {
                    // Analyse du scanset.
                    let mut negate = false;
                    if at(i) == u16::from(b'^') {
                        negate = true;
                        i += 1;
                    }
                    let mut set: Vec<(u16, u16)> = Vec::new();
                    if at(i) == u16::from(b']') {
                        set.push((at(i), at(i)));
                        i += 1;
                    }
                    while i < fmt.len() && fmt[i] != u16::from(b']') {
                        let a = fmt[i];
                        if at(i + 1) == u16::from(b'-')
                            && i + 2 < fmt.len()
                            && fmt[i + 2] != u16::from(b']')
                        {
                            let b = fmt[i + 2];
                            set.push((a.min(b), a.max(b)));
                            i += 3;
                        } else {
                            set.push((a, a));
                            i += 1;
                        }
                    }
                    if i >= fmt.len() {
                        return assigned;
                    }
                    i += 1; // `]`
                    while let Some(x) = rd.get_w(&mut left) {
                        let inside = set.iter().any(|&(lo, hi)| (lo..=hi).contains(&x));
                        if inside != negate {
                            collected.push(x);
                        } else {
                            rd.unget(x);
                            break;
                        }
                    }
                    if collected.is_empty() {
                        return if rd.hit_eof {
                            input_failure(assigned, conversions)
                        } else {
                            assigned
                        };
                    }
                } else if conv == b's' || conv == b'S' {
                    while let Some(x) = rd.get_w(&mut left) {
                        if is_space(x) {
                            rd.unget(x);
                            break;
                        }
                        collected.push(x);
                    }
                } else {
                    let want = left;
                    while let Some(x) = rd.get_w(&mut left) {
                        collected.push(x);
                    }
                    if collected.is_empty() {
                        return input_failure(assigned, conversions);
                    }
                    if collected.len() < want {
                        return input_failure(assigned, conversions);
                    }
                }
                conversions += 1;
                if suppress {
                    continue;
                }
                let p = va.next() as usize;
                let is_c = matches!(conv, b'c' | b'C');
                let need = collected.len() + usize::from(!is_c);
                if secure {
                    let cap = va.next() as u32 as usize;
                    if need > cap {
                        if cap > 0 {
                            // SAFETY: tampon invité d'au moins un élément.
                            unsafe {
                                if wide_dest {
                                    (p as *mut u16).write_unaligned(0);
                                } else {
                                    (p as *mut u8).write_unaligned(0);
                                }
                            }
                        }
                        crate::crt_str::set_errno(ENOMEM);
                        return assigned;
                    }
                }
                if wide_dest {
                    put_units(p as *mut u16, &collected, !is_c);
                } else {
                    if collected.iter().any(|&u| u > 0xFF) {
                        crate::crt_str::set_errno(EILSEQ);
                        return assigned;
                    }
                    put_units(p as *mut u8, &collected, !is_c);
                }
                assigned += 1;
            }
            _ => return assigned,
        }
    }
    assigned
}

extern "win64" fn __stdio_common_vsscanf(
    options: u64,
    buffer: *const u8,
    buffer_count: usize,
    format: *const i8,
    _locale: *mut c_void,
    arglist: *mut c_void,
) -> i32 {
    ffi_guard!(CHANNEL, "__stdio_common_vsscanf", EOF, {
        if buffer.is_null() || format.is_null() {
            crate::crt_str::set_errno(EINVAL);
            return EOF;
        }
        let input: Vec<u16> = bytes_vec(buffer, Some(buffer_count))
            .into_iter()
            .map(u16::from)
            .collect();
        let mut src = StrSrc {
            data: &input,
            pos: 0,
        };
        scanf_engine(&narrow_fmt(format), false, options, &mut src, arglist)
    })
}

extern "win64" fn __stdio_common_vswscanf(
    options: u64,
    buffer: *const u16,
    buffer_count: usize,
    format: *const u16,
    _locale: *mut c_void,
    arglist: *mut c_void,
) -> i32 {
    ffi_guard!(CHANNEL, "__stdio_common_vswscanf", EOF, {
        if buffer.is_null() || format.is_null() {
            crate::crt_str::set_errno(EINVAL);
            return EOF;
        }
        let input = wstr_vec(buffer, Some(buffer_count));
        let mut src = StrSrc {
            data: &input,
            pos: 0,
        };
        scanf_engine(&wstr_vec(format, None), true, options, &mut src, arglist)
    })
}

extern "win64" fn __stdio_common_vfscanf(
    options: u64,
    stream: *mut c_void,
    format: *const i8,
    _locale: *mut c_void,
    arglist: *mut c_void,
) -> i32 {
    ffi_guard!(CHANNEL, "__stdio_common_vfscanf", EOF, {
        if format.is_null() {
            crate::crt_str::set_errno(EINVAL);
            return EOF;
        }
        let fmt = narrow_fmt(format);
        with_file(stream, EOF, |st| {
            let mut src = FileSrc { st, wide: false };
            scanf_engine(&fmt, false, options, &mut src, arglist)
        })
    })
}

extern "win64" fn __stdio_common_vfwscanf(
    options: u64,
    stream: *mut c_void,
    format: *const u16,
    _locale: *mut c_void,
    arglist: *mut c_void,
) -> i32 {
    ffi_guard!(CHANNEL, "__stdio_common_vfwscanf", EOF, {
        if format.is_null() {
            crate::crt_str::set_errno(EINVAL);
            return EOF;
        }
        let fmt = wstr_vec(format, None);
        with_file(stream, EOF, |st| {
            let mut src = FileSrc { st, wide: true };
            scanf_engine(&fmt, true, options, &mut src, arglist)
        })
    })
}

// ---------- Système de fichiers (`api-ms-win-crt-filesystem`) via VFS ----------

/// `struct _stat64` MSVC.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Stat64 {
    pub st_dev: u32,
    pub st_ino: u16,
    pub st_mode: u16,
    pub st_nlink: i16,
    pub st_uid: i16,
    pub st_gid: i16,
    pub st_rdev: u32,
    pub st_size: i64,
    pub st_atime: i64,
    pub st_mtime: i64,
    pub st_ctime: i64,
}
const _: () = assert!(size_of::<Stat64>() == 56);
const _: () = assert!(core::mem::offset_of!(Stat64, st_rdev) == 16);
const _: () = assert!(core::mem::offset_of!(Stat64, st_size) == 24);

/// `struct _stat64i32` MSVC.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Stat64i32 {
    pub st_dev: u32,
    pub st_ino: u16,
    pub st_mode: u16,
    pub st_nlink: i16,
    pub st_uid: i16,
    pub st_gid: i16,
    pub st_rdev: u32,
    pub st_size: i32,
    pub st_atime: i64,
    pub st_mtime: i64,
    pub st_ctime: i64,
}
const _: () = assert!(size_of::<Stat64i32>() == 48);
const _: () = assert!(core::mem::offset_of!(Stat64i32, st_size) == 20);
const _: () = assert!(core::mem::offset_of!(Stat64i32, st_atime) == 24);

const S_IFDIR: u16 = 0x4000;
const S_IFREG: u16 = 0x8000;
const S_IREAD: u16 = 0x0100;
const S_IWRITE: u16 = 0x0080;
const S_IEXEC: u16 = 0x0040;

fn systime_secs(t: std::io::Result<std::time::SystemTime>) -> Option<i64> {
    let t = t.ok()?;
    Some(match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    })
}

fn stat_path(guest: Option<String>) -> Result<Stat64, i32> {
    let guest = guest.ok_or(EINVAL)?;
    let vp = vfs_path(&guest)?;
    let md = std::fs::metadata(&vp.host).map_err(|e| errno_from_io(&e))?;
    if !md.is_dir() && (guest.ends_with('/') || guest.ends_with('\\')) {
        return Err(ENOENT);
    }
    let mut mode: u16 = S_IREAD;
    if !md.permissions().readonly() {
        mode |= S_IWRITE;
    }
    if md.is_dir() {
        mode |= S_IFDIR | S_IEXEC;
    } else {
        mode |= S_IFREG;
        let ext = vp
            .host
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if matches!(ext.as_str(), "exe" | "com" | "bat" | "cmd") {
            mode |= S_IEXEC;
        }
    }
    mode |= (mode & 0o700) >> 3;
    mode |= (mode & 0o700) >> 6;
    let drive = match vp.mount {
        sb_vfs::VfsMount::Game => 6,
        sb_vfs::VfsMount::Save => 18,
        sb_vfs::VfsMount::Temp => 19,
    };
    let mtime = systime_secs(md.modified()).unwrap_or(0);
    Ok(Stat64 {
        st_dev: drive,
        st_ino: 0,
        st_mode: mode,
        st_nlink: 1,
        st_uid: 0,
        st_gid: 0,
        st_rdev: drive,
        st_size: if md.is_dir() { 0 } else { md.len() as i64 },
        st_atime: systime_secs(md.accessed()).unwrap_or(mtime),
        st_mtime: mtime,
        st_ctime: systime_secs(md.created()).unwrap_or(mtime),
    })
}

fn stat64_out(path: Option<String>, out: *mut Stat64) -> i32 {
    if out.is_null() {
        crate::crt_str::set_errno(EINVAL);
        return -1;
    }
    match stat_path(path) {
        Ok(s) => {
            // SAFETY: pointeur de sortie non nul vers une `struct _stat64`.
            unsafe { out.write_unaligned(s) };
            0
        }
        Err(e) => {
            // SAFETY: idem ; l'UCRT remet la structure à zéro en cas d'échec.
            unsafe { out.write_unaligned(Stat64::default()) };
            crate::crt_str::set_errno(e);
            -1
        }
    }
}

fn stat64i32_out(path: Option<String>, out: *mut Stat64i32) -> i32 {
    if out.is_null() {
        crate::crt_str::set_errno(EINVAL);
        return -1;
    }
    match stat_path(path) {
        Ok(s) => {
            let v = Stat64i32 {
                st_dev: s.st_dev,
                st_ino: s.st_ino,
                st_mode: s.st_mode,
                st_nlink: s.st_nlink,
                st_uid: s.st_uid,
                st_gid: s.st_gid,
                st_rdev: s.st_rdev,
                st_size: s.st_size as i32,
                st_atime: s.st_atime,
                st_mtime: s.st_mtime,
                st_ctime: s.st_ctime,
            };
            // SAFETY: pointeur de sortie non nul vers une `struct _stat64i32`.
            unsafe { out.write_unaligned(v) };
            0
        }
        Err(e) => {
            // SAFETY: idem.
            unsafe { out.write_unaligned(Stat64i32::default()) };
            crate::crt_str::set_errno(e);
            -1
        }
    }
}

extern "win64" fn crt_stat64(path: *const i8, out: *mut Stat64) -> i32 {
    ffi_guard!(CHANNEL, "_stat64", -1, {
        stat64_out(cstr_lossy(path), out)
    })
}

extern "win64" fn crt_wstat64(path: *const u16, out: *mut Stat64) -> i32 {
    ffi_guard!(CHANNEL, "_wstat64", -1, {
        stat64_out(wstr_lossy(path), out)
    })
}

extern "win64" fn crt_stat64i32(path: *const i8, out: *mut Stat64i32) -> i32 {
    ffi_guard!(CHANNEL, "_stat64i32", -1, {
        stat64i32_out(cstr_lossy(path), out)
    })
}

extern "win64" fn crt_wstat64i32(path: *const u16, out: *mut Stat64i32) -> i32 {
    ffi_guard!(CHANNEL, "_wstat64i32", -1, {
        stat64i32_out(wstr_lossy(path), out)
    })
}

fn errno_result(r: Result<(), i32>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => {
            crate::crt_str::set_errno(e);
            -1
        }
    }
}

fn remove_impl(path: Option<String>) -> Result<(), i32> {
    let vp = vfs_path(&path.ok_or(EINVAL)?)?;
    if vp.is_root {
        return Err(EACCES);
    }
    let md = std::fs::symlink_metadata(&vp.host).map_err(|e| errno_from_io(&e))?;
    if md.is_dir() || md.permissions().readonly() {
        return Err(EACCES);
    }
    std::fs::remove_file(&vp.host).map_err(|e| errno_from_io(&e))
}

fn rename_impl(old: Option<String>, new: Option<String>) -> Result<(), i32> {
    let (old, new) = (old.ok_or(EINVAL)?, new.ok_or(EINVAL)?);
    let src = vfs_path(&old)?;
    let dst = vfs_path(&new).map_err(|e| if e == ENOENT { EACCES } else { e })?;
    if src.is_root || dst.is_root {
        return Err(EACCES);
    }
    let md = std::fs::symlink_metadata(&src.host).map_err(|e| errno_from_io(&e))?;
    if std::fs::symlink_metadata(&dst.host).is_ok() {
        return Err(EACCES);
    }
    if md.is_dir() && src.mount != dst.mount {
        return Err(EACCES);
    }
    match std::fs::rename(&src.host, &dst.host) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices && md.is_file() => {
            std::fs::copy(&src.host, &dst.host).map_err(|e| errno_from_io(&e))?;
            std::fs::remove_file(&src.host).map_err(|e| errno_from_io(&e))
        }
        Err(e) => Err(match e.kind() {
            std::io::ErrorKind::NotFound => ENOENT,
            _ => EACCES,
        }),
    }
}

fn access_impl(path: Option<String>, mode: i32) -> Result<(), i32> {
    if mode & !6 != 0 {
        return Err(EINVAL);
    }
    let vp = vfs_path(&path.ok_or(EINVAL)?)?;
    let md = std::fs::metadata(&vp.host).map_err(|e| errno_from_io(&e))?;
    if mode & 2 != 0 && md.permissions().readonly() {
        return Err(EACCES);
    }
    Ok(())
}

fn mkdir_impl(path: Option<String>) -> Result<(), i32> {
    let vp = vfs_path(&path.ok_or(EINVAL)?)?;
    if vp.is_root || vp.host.exists() {
        return Err(EEXIST);
    }
    std::fs::create_dir(&vp.host).map_err(|e| errno_from_io(&e))
}

fn rmdir_impl(path: Option<String>) -> Result<(), i32> {
    let vp = vfs_path(&path.ok_or(EINVAL)?)?;
    if vp.is_root {
        return Err(ENOTEMPTY);
    }
    let md = std::fs::symlink_metadata(&vp.host).map_err(|e| errno_from_io(&e))?;
    if !md.is_dir() {
        return Err(ENOTEMPTY);
    }
    std::fs::remove_dir(&vp.host).map_err(|e| match errno_from_io(&e) {
        EIO => ENOTEMPTY,
        x => x,
    })
}

extern "win64" fn crt_remove(path: *const i8) -> i32 {
    ffi_guard!(CHANNEL, "remove", -1, {
        errno_result(remove_impl(cstr_lossy(path)))
    })
}

extern "win64" fn crt_wremove(path: *const u16) -> i32 {
    ffi_guard!(CHANNEL, "_wremove", -1, {
        errno_result(remove_impl(wstr_lossy(path)))
    })
}

extern "win64" fn crt_rename(old: *const i8, new: *const i8) -> i32 {
    ffi_guard!(CHANNEL, "rename", -1, {
        errno_result(rename_impl(cstr_lossy(old), cstr_lossy(new)))
    })
}

extern "win64" fn crt_wrename(old: *const u16, new: *const u16) -> i32 {
    ffi_guard!(CHANNEL, "_wrename", -1, {
        errno_result(rename_impl(wstr_lossy(old), wstr_lossy(new)))
    })
}

extern "win64" fn crt_access(path: *const i8, mode: i32) -> i32 {
    ffi_guard!(CHANNEL, "_access", -1, {
        errno_result(access_impl(cstr_lossy(path), mode))
    })
}

extern "win64" fn crt_waccess(path: *const u16, mode: i32) -> i32 {
    ffi_guard!(CHANNEL, "_waccess", -1, {
        errno_result(access_impl(wstr_lossy(path), mode))
    })
}

extern "win64" fn crt_access_s(path: *const i8, mode: i32) -> i32 {
    ffi_guard!(CHANNEL, "_access_s", EINVAL, {
        match access_impl(cstr_lossy(path), mode) {
            Ok(()) => 0,
            Err(e) => {
                crate::crt_str::set_errno(e);
                e
            }
        }
    })
}

extern "win64" fn crt_waccess_s(path: *const u16, mode: i32) -> i32 {
    ffi_guard!(CHANNEL, "_waccess_s", EINVAL, {
        match access_impl(wstr_lossy(path), mode) {
            Ok(()) => 0,
            Err(e) => {
                crate::crt_str::set_errno(e);
                e
            }
        }
    })
}

extern "win64" fn crt_mkdir(path: *const i8) -> i32 {
    ffi_guard!(CHANNEL, "_mkdir", -1, {
        errno_result(mkdir_impl(cstr_lossy(path)))
    })
}

extern "win64" fn crt_wmkdir(path: *const u16) -> i32 {
    ffi_guard!(CHANNEL, "_wmkdir", -1, {
        errno_result(mkdir_impl(wstr_lossy(path)))
    })
}

extern "win64" fn crt_rmdir(path: *const i8) -> i32 {
    ffi_guard!(CHANNEL, "_rmdir", -1, {
        errno_result(rmdir_impl(cstr_lossy(path)))
    })
}

extern "win64" fn crt_wrmdir(path: *const u16) -> i32 {
    ffi_guard!(CHANNEL, "_wrmdir", -1, {
        errno_result(rmdir_impl(wstr_lossy(path)))
    })
}

// ---------- Utilitaires ----------

extern "win64" fn crt_beginthreadex(
    sec: *mut c_void,
    stack_size: u32,
    start_addr: usize,
    arg: *mut c_void,
    init_flag: u32,
    out_tid: *mut u32,
) -> usize {
    crate::sync::CreateThread(
        sec,
        stack_size as usize,
        start_addr,
        arg,
        init_flag,
        out_tid,
    )
}

extern "win64" fn crt_endthreadex(retval: u32) -> ! {
    crate::sync::ExitThread(retval)
}

extern "win64" fn crt_qsort(
    base: *mut u8,
    nmemb: usize,
    size: usize,
    compar: Option<unsafe extern "win64" fn(*const u8, *const u8) -> i32>,
) {
    ffi_guard!(CHANNEL, "qsort", (), {
        let Some(cmp) = compar else {
            return;
        };
        if base.is_null() || nmemb <= 1 || size == 0 {
            return;
        }
        // Tri simple par insertion/sélection compatible comparateur `extern "win64"`.
        let mut tmp = vec![0u8; size];
        for i in 1..nmemb {
            let mut j = i;
            while j > 0 {
                // SAFETY: `j < nmemb`, indices valides dans `base[0..nmemb*size]`.
                let prev = unsafe { base.add((j - 1) * size) };
                let curr = unsafe { base.add(j * size) };
                let ord = unsafe { cmp(prev, curr) };
                if ord <= 0 {
                    break;
                }
                unsafe {
                    ptr::copy_nonoverlapping(curr, tmp.as_mut_ptr(), size);
                    ptr::copy_nonoverlapping(prev, curr, size);
                    ptr::copy_nonoverlapping(tmp.as_ptr(), prev, size);
                }
                j -= 1;
            }
        }
    });
}

extern "win64" fn crt_srand(seed: u32) {
    RAND_SEED.store(seed, Ordering::Relaxed);
}

extern "win64" fn crt_rand() -> i32 {
    let prev = RAND_SEED.load(Ordering::Relaxed);
    let next = prev.wrapping_mul(214_013).wrapping_add(2_531_011);
    RAND_SEED.store(next, Ordering::Relaxed);
    ((next >> 16) & 0x7FFF) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crt_str::get_errno;

    const LEGACY: u64 = PF_LEGACY_WIDE_SPECIFIERS;

    static IPH_CALLS: AtomicU32 = AtomicU32::new(0);

    unsafe extern "win64" fn counting_iph(
        _e: *const u16,
        _f: *const u16,
        _file: *const u16,
        _l: u32,
        _r: usize,
    ) {
        IPH_CALLS.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn invalid_parameter_handler_is_stored_and_called() {
        let h = counting_iph as *const () as usize;
        // Gestionnaire par thread : n'interfère pas avec les autres tests.
        assert_eq!(_set_thread_local_invalid_parameter_handler(h), 0);
        assert_eq!(_get_thread_local_invalid_parameter_handler(), h);
        _invalid_parameter_noinfo();
        assert_eq!(IPH_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(_set_thread_local_invalid_parameter_handler(0), h);
    }

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn c(s: &str) -> Vec<u8> {
        s.bytes().chain(std::iter::once(0)).collect()
    }

    fn fmt_narrow(options: u64, f: &str, args: &[u64]) -> (i32, String) {
        let mut buf = [0u8; 256];
        let f = c(f);
        let n = __stdio_common_vsprintf(
            options,
            buf.as_mut_ptr(),
            buf.len(),
            f.as_ptr().cast(),
            ptr::null_mut(),
            args.as_ptr() as *mut c_void,
        );
        let end = buf.iter().position(|&b| b == 0).unwrap_or(0);
        (n, String::from_utf8_lossy(&buf[..end]).into_owned())
    }

    /// Installe un VFS de test (racines dans le répertoire temporaire) si aucun n'est actif.
    fn ensure_vfs() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            let mut st = process_state().write();
            if st.vfs.is_none() {
                let base =
                    std::env::temp_dir().join(format!("sb-ucrt-test-{}", std::process::id()));
                let game = base.join("game");
                std::fs::create_dir_all(&game).expect("racine game de test");
                st.vfs = Some(
                    sb_vfs::VirtualFs::new(&game, &base.join("save"), &base.join("temp"), None)
                        .expect("VFS de test"),
                );
            }
        });
    }

    fn unique_dir(tag: &str) -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        ensure_vfs();
        let d = format!(
            "temp:/ucrt_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        );
        let cd = c(&d);
        assert_eq!(crt_mkdir(cd.as_ptr().cast()), 0);
        d
    }

    #[test]
    fn printf_engine_conversions() {
        let s = c("abc");
        let ws = w("wide");
        let args = [
            (-42i64) as u64,
            3.25f64.to_bits(),
            s.as_ptr() as u64,
            0xBEEF,
            8,
            12345.678f64.to_bits(),
            0.0001f64.to_bits(),
            u64::from(b'Z'),
            (-5i64) as u64,
            0x1234,
            ws.as_ptr() as u64,
        ];
        let (n, out) = fmt_narrow(0, "%d|%5.2f|%-4s|%x|%#o|%+e|%g|%c|%%|%lld|%p|%ls", &args);
        assert_eq!(
            out,
            "-42| 3.25|abc |beef|010|+1.234568e+04|0.0001|Z|%|-5|0000000000001234|wide"
        );
        assert_eq!(n as usize, out.len());

        let args = [
            7u64,
            0x1_0000_0001,
            255,
            100_000_000f64.to_bits(),
            1.5f64.to_bits(),
            f64::INFINITY.to_bits(),
            0xFFF8_0000_0000_0000,
            0.0f64.to_bits(),
            6,
            42,
        ];
        let (_, out) = fmt_narrow(0, "%03d|%ld|%#X|%g|%a|%f|%f|%.0e|%*d", &args);
        assert_eq!(
            out,
            "007|1|0XFF|1e+08|0x1.8000000000000p+0|inf|-nan(ind)|0e+00|    42"
        );
        // `%n` désactivé par défaut : paramètre invalide.
        set_errno(0);
        let mut cnt = 0i32;
        let (n, _) = fmt_narrow(0, "ab%n", &[(&raw mut cnt) as u64]);
        assert_eq!(n, -1);
        assert_eq!(get_errno(), EINVAL);
        // Exposants à 3 chiffres (legacy).
        let (_, out) = fmt_narrow(PF_LEGACY_THREE_DIGIT_EXPONENTS, "%e", &[1.0f64.to_bits()]);
        assert_eq!(out, "1.000000e+000");
    }

    fn set_errno(v: i32) {
        crate::crt_str::set_errno(v);
    }

    #[test]
    fn sprintf_truncation_semantics() {
        let f = c("%s");
        let s = c("hello");
        let args = [s.as_ptr() as u64];
        let mut buf = [0x7Fu8; 8];
        // snprintf C99 : longueur totale, troncature + NUL.
        let n = __stdio_common_vsprintf(
            PF_STANDARD_SNPRINTF_BEHAVIOR,
            buf.as_mut_ptr(),
            4,
            f.as_ptr().cast(),
            ptr::null_mut(),
            args.as_ptr() as *mut c_void,
        );
        assert_eq!(n, 5);
        assert_eq!(&buf[..4], b"hel\0");
        // Taille requise (NULL, 0).
        let n = __stdio_common_vsprintf(
            PF_STANDARD_SNPRINTF_BEHAVIOR,
            ptr::null_mut(),
            0,
            f.as_ptr().cast(),
            ptr::null_mut(),
            args.as_ptr() as *mut c_void,
        );
        assert_eq!(n, 5);
        // `_vsnprintf` legacy : -1 sans NUL, ou longueur exacte sans NUL.
        buf = [0x7F; 8];
        let n = __stdio_common_vsprintf(
            PF_LEGACY_VSPRINTF_NULL_TERMINATION,
            buf.as_mut_ptr(),
            4,
            f.as_ptr().cast(),
            ptr::null_mut(),
            args.as_ptr() as *mut c_void,
        );
        assert_eq!(n, -1);
        assert_eq!(&buf[..5], b"hell\x7F");
        let n = __stdio_common_vsprintf(
            PF_LEGACY_VSPRINTF_NULL_TERMINATION,
            buf.as_mut_ptr(),
            5,
            f.as_ptr().cast(),
            ptr::null_mut(),
            args.as_ptr() as *mut c_void,
        );
        assert_eq!(n, 5);
        // sprintf_s : ERANGE + chaîne vide.
        set_errno(0);
        let n = __stdio_common_vsprintf_s(
            0,
            buf.as_mut_ptr(),
            5,
            f.as_ptr().cast(),
            ptr::null_mut(),
            args.as_ptr() as *mut c_void,
        );
        assert_eq!(n, -1);
        assert_eq!(buf[0], 0);
        assert_eq!(get_errno(), ERANGE);
        let n = __stdio_common_vsprintf_s(
            0,
            buf.as_mut_ptr(),
            6,
            f.as_ptr().cast(),
            ptr::null_mut(),
            args.as_ptr() as *mut c_void,
        );
        assert_eq!(n, 5);
        // _snprintf_s(_TRUNCATE).
        let n = __stdio_common_vsnprintf_s(
            0,
            buf.as_mut_ptr(),
            4,
            usize::MAX,
            f.as_ptr().cast(),
            ptr::null_mut(),
            args.as_ptr() as *mut c_void,
        );
        assert_eq!(n, -1);
        assert_eq!(&buf[..4], b"hel\0");
        let n = __stdio_common_vsnprintf_s(
            0,
            buf.as_mut_ptr(),
            8,
            2,
            f.as_ptr().cast(),
            ptr::null_mut(),
            args.as_ptr() as *mut c_void,
        );
        assert_eq!(n, -1);
        assert_eq!(&buf[..3], b"he\0");
        set_errno(0);
        let n = __stdio_common_vsnprintf_s(
            0,
            buf.as_mut_ptr(),
            4,
            10,
            f.as_ptr().cast(),
            ptr::null_mut(),
            args.as_ptr() as *mut c_void,
        );
        assert_eq!(n, -1);
        assert_eq!(get_errno(), ERANGE);
    }

    #[test]
    fn swprintf_wide_specifiers() {
        let mut buf = [0u16; 64];
        let wide = w("wé");
        let narrow = c("na");
        let f = w("%s|%hs|%S|%c|%d");
        let args = [
            wide.as_ptr() as u64,
            narrow.as_ptr() as u64,
            narrow.as_ptr() as u64,
            0x263A,
            12,
        ];
        let n = __stdio_common_vswprintf(
            LEGACY,
            buf.as_mut_ptr(),
            buf.len(),
            f.as_ptr(),
            ptr::null_mut(),
            args.as_ptr() as *mut c_void,
        );
        assert_eq!(
            String::from_utf16_lossy(&buf[..n as usize]),
            "wé|na|na|\u{263A}|12"
        );
        // Mode ISO : `%s` = char*, `%S` = wchar_t*.
        let f = w("%s/%S");
        let args = [narrow.as_ptr() as u64, wide.as_ptr() as u64];
        let n = __stdio_common_vswprintf(
            0,
            buf.as_mut_ptr(),
            buf.len(),
            f.as_ptr(),
            ptr::null_mut(),
            args.as_ptr() as *mut c_void,
        );
        assert_eq!(String::from_utf16_lossy(&buf[..n as usize]), "na/wé");
        // Narrow printf d'un wchar > 0xFF : EILSEQ (locale « C »).
        set_errno(0);
        let (n, _) = fmt_narrow(0, "%lc", &[0x263A]);
        assert_eq!(n, -1);
        assert_eq!(get_errno(), EILSEQ);
        // _s : troncature interdite.
        let n = __stdio_common_vswprintf_s(
            LEGACY,
            buf.as_mut_ptr(),
            3,
            w("%d").as_ptr(),
            ptr::null_mut(),
            [12345u64].as_ptr() as *mut c_void,
        );
        assert_eq!(n, -1);
        assert_eq!(buf[0], 0);
        let n = __stdio_common_vsnwprintf_s(
            LEGACY,
            buf.as_mut_ptr(),
            4,
            usize::MAX,
            w("%d").as_ptr(),
            ptr::null_mut(),
            [12345u64].as_ptr() as *mut c_void,
        );
        assert_eq!(n, -1);
        assert_eq!(String::from_utf16_lossy(&buf[..3]), "123");
    }

    fn sscanf(options: u64, input: &str, f: &str, args: &[u64]) -> i32 {
        let i = c(input);
        let f = c(f);
        __stdio_common_vsscanf(
            options,
            i.as_ptr(),
            usize::MAX,
            f.as_ptr().cast(),
            ptr::null_mut(),
            args.as_ptr() as *mut c_void,
        )
    }

    #[test]
    fn sscanf_conversions() {
        let (mut d, mut i, mut u, mut x, mut o) = (0i32, 0i32, 0u32, 0u32, 0u32);
        let (mut f, mut e, mut g) = (0f32, 0f64, 0f32);
        let mut s = [0u8; 16];
        let mut ch = [0u8; 2];
        let mut set = [0u8; 16];
        let mut n = 0i32;
        let mut p = 0usize;
        let args = [
            (&raw mut d) as u64,
            (&raw mut i) as u64,
            (&raw mut u) as u64,
            (&raw mut x) as u64,
            (&raw mut o) as u64,
            (&raw mut f) as u64,
            (&raw mut e) as u64,
            (&raw mut g) as u64,
            s.as_mut_ptr() as u64,
            ch.as_mut_ptr() as u64,
            set.as_mut_ptr() as u64,
            (&raw mut n) as u64,
            (&raw mut p) as u64,
        ];
        let r = sscanf(
            0,
            "-12 0x1F 42 ff 17 1.5 -2.5e3 inf word XY abcdef deadBEEF",
            "%d %i %u %x %o %f %le %g %s %2c %[a-f]%n %p",
            &args,
        );
        assert_eq!(r, 12);
        assert_eq!((d, i, u, x, o), (-12, 31, 42, 255, 15));
        assert_eq!((f, e), (1.5, -2500.0));
        assert!(g.is_infinite());
        assert_eq!(&s[..5], b"word\0");
        assert_eq!(&ch, b"XY");
        assert_eq!(&set[..7], b"abcdef\0");
        assert_eq!(n, 47);
        assert_eq!(p, 0xDEAD_BEEF);

        // Largeurs, tailles, suppression.
        let (mut a, mut b) = (0i16, 0i64);
        let mut c8 = 0u8;
        let r = sscanf(
            0,
            "12345 9876543210123 77 7",
            "%3hd%*d %I64d %hhu",
            &[
                (&raw mut a) as u64,
                (&raw mut b) as u64,
                (&raw mut c8) as u64,
            ],
        );
        assert_eq!(r, 3);
        assert_eq!((a, b, c8), (123, 9_876_543_210_123, 77));
        let mut ll = 0i64;
        assert_eq!(sscanf(0, "-0x10", "%lli", &[(&raw mut ll) as u64]), 1);
        assert_eq!(ll, -16);
        let mut neg = [0u8; 8];
        assert_eq!(sscanf(0, "abc]x", "%[^]x]", &[neg.as_mut_ptr() as u64]), 1);
        assert_eq!(&neg[..4], b"abc\0");

        // EOF vs échec de correspondance.
        let mut v = 0i32;
        assert_eq!(sscanf(0, "", "%d", &[(&raw mut v) as u64]), EOF);
        assert_eq!(sscanf(0, "   ", "%d", &[(&raw mut v) as u64]), EOF);
        assert_eq!(sscanf(0, "abc", "%d", &[(&raw mut v) as u64]), 0);
        assert_eq!(sscanf(0, "5 x", "%d %d", &[(&raw mut v) as u64, 0]), 1);
        assert_eq!(sscanf(0, "5", "%d %d", &[(&raw mut v) as u64, 0]), 1);
        // Flottant hexadécimal.
        let mut hf = 0f64;
        assert_eq!(sscanf(0, "0x1.8p3", "%lf", &[(&raw mut hf) as u64]), 1);
        assert_eq!(hf, 12.0);

        // scanf_s : taille de tampon obligatoire.
        let mut small = [0x55u8; 4];
        set_errno(0);
        let r = sscanf(
            SF_SECURECRT,
            "toolong",
            "%s",
            &[small.as_mut_ptr() as u64, 4],
        );
        assert_eq!(r, 0);
        assert_eq!(small[0], 0);
        assert_eq!(get_errno(), ENOMEM);
        let r = sscanf(SF_SECURECRT, "ok", "%s", &[small.as_mut_ptr() as u64, 4]);
        assert_eq!(r, 1);
        assert_eq!(&small[..3], b"ok\0");
    }

    #[test]
    fn swscanf_wide() {
        let input = w("42 héllo 2.5");
        let mut v = 0i32;
        let mut ws = [0u16; 16];
        let mut d = 0f64;
        let f = w("%d %s %lf");
        let args = [
            (&raw mut v) as u64,
            ws.as_mut_ptr() as u64,
            (&raw mut d) as u64,
        ];
        let r = __stdio_common_vswscanf(
            SF_LEGACY_WIDE_SPECIFIERS,
            input.as_ptr(),
            usize::MAX,
            f.as_ptr(),
            ptr::null_mut(),
            args.as_ptr() as *mut c_void,
        );
        assert_eq!(r, 3);
        assert_eq!(v, 42);
        assert_eq!(String::from_utf16_lossy(&ws[..5]), "héllo");
        assert_eq!(d, 2.5);
        // `%hs` vers char* depuis une entrée large.
        let mut nb = [0u8; 8];
        let f = w("%*d %hs");
        let r = __stdio_common_vswscanf(
            SF_LEGACY_WIDE_SPECIFIERS,
            input.as_ptr(),
            usize::MAX,
            f.as_ptr(),
            ptr::null_mut(),
            [nb.as_mut_ptr() as u64].as_ptr() as *mut c_void,
        );
        assert_eq!(r, 1);
        assert_eq!(&nb[..6], b"h\xE9llo\0");
    }

    #[test]
    fn file_stdio_through_vfs() {
        let dir = unique_dir("stdio");
        let path = c(&format!("{dir}/a.txt"));
        let wmode = c("w");
        let f = crt_fopen(path.as_ptr().cast(), wmode.as_ptr().cast());
        assert!(!f.is_null());
        assert!(crt_fileno(f) >= 3);
        assert_eq!(crt_fputs(c("line1\nline2\n").as_ptr().cast(), f), 0);
        assert_eq!(crt_fputc(i32::from(b'Z'), f), i32::from(b'Z'));
        let fmt = c("%d-%s\n");
        let s = c("x");
        assert_eq!(
            __stdio_common_vfprintf(
                0,
                f,
                fmt.as_ptr().cast(),
                ptr::null_mut(),
                [7u64, s.as_ptr() as u64].as_ptr() as *mut c_void
            ),
            4
        );
        assert_eq!(crt_fclose(f), 0);
        set_errno(0);
        assert_eq!(crt_fclose(f), EOF);
        assert_eq!(get_errno(), EINVAL);

        // Binaire : CRLF visible.
        let mut out: *mut c_void = ptr::null_mut();
        assert_eq!(
            crt_fopen_s(&mut out, path.as_ptr().cast(), c("rb").as_ptr().cast()),
            0
        );
        let mut raw = [0u8; 64];
        let n = crt_fread(raw.as_mut_ptr(), 1, raw.len(), out);
        assert_eq!(&raw[..n], b"line1\r\nline2\r\nZ7-x\r\n");
        assert_ne!(crt_feof(out), 0);
        crt_clearerr(out);
        assert_eq!(crt_feof(out), 0);
        assert_eq!(crt_ftelli64(out), n as i64);
        crt_rewind(out);
        assert_eq!(crt_fgetc(out), i32::from(b'l'));
        assert_eq!(crt_fclose(out), 0);

        // Texte : CRLF -> LF, fgets, ungetc, positions.
        let f = crt_fopen(path.as_ptr().cast(), c("r").as_ptr().cast());
        let mut line = [0u8; 32];
        assert!(!crt_fgets(line.as_mut_ptr(), 32, f).is_null());
        assert_eq!(&line[..7], b"line1\n\0");
        assert_eq!(crt_ftell(f), 7);
        let mut pos = 0i64;
        assert_eq!(crt_fgetpos(f, &mut pos), 0);
        assert_eq!(crt_fgetc(f), i32::from(b'l'));
        assert_eq!(crt_ungetc(i32::from(b'L'), f), i32::from(b'L'));
        assert_eq!(crt_fgetc(f), i32::from(b'L'));
        assert_eq!(crt_fsetpos(f, &pos), 0);
        assert!(!crt_fgets(line.as_mut_ptr(), 4, f).is_null());
        assert_eq!(&line[..4], b"lin\0");
        assert_eq!(crt_fseek(f, -5, 2), 0);
        let (mut num, mut word) = (0i32, [0u8; 8]);
        let r = __stdio_common_vfscanf(
            0,
            f,
            c("%d-%s").as_ptr().cast(),
            ptr::null_mut(),
            [(&raw mut num) as u64, word.as_mut_ptr() as u64].as_ptr() as *mut c_void,
        );
        assert_eq!(r, 2);
        assert_eq!((num, &word[..2]), (7, &b"x\0"[..]));
        assert_eq!(crt_fseeki64(f, -1, 0), -1);
        assert_eq!(crt_fseeki64(f, 0, 7), -1);
        // Écriture interdite sur flux en lecture seule.
        assert_eq!(crt_fputc(i32::from(b'q'), f), EOF);
        assert_ne!(crt_ferror(f), 0);
        crt_clearerr(f);
        assert_eq!(crt_ferror(f), 0);
        // Verrou réentrant + pointeurs de tampon.
        crt_lock_file(f);
        crt_lock_file(f);
        assert_eq!(crt_fgetc(f), i32::from(b'\n'));
        assert_eq!(crt_fgetc(f), EOF);
        crt_unlock_file(f);
        crt_unlock_file(f);
        let (mut b, mut p, mut cnt): (*mut *mut u8, *mut *mut u8, *mut i32) =
            (ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
        assert_eq!(
            crt_get_stream_buffer_pointers(f, &mut b, &mut p, &mut cnt),
            0
        );
        assert_eq!(b as usize, f as usize + 8);
        assert_eq!(p as usize, f as usize);
        assert_eq!(cnt as usize, f as usize + 16);
        // SAFETY: champ `_cnt` du flux vivant.
        assert_eq!(unsafe { *cnt }, 0);
        assert_eq!(crt_setvbuf(f, ptr::null_mut(), 99, 0), -1);
        assert_eq!(crt_setvbuf(f, ptr::null_mut(), IONBF, 0), 0);
        assert_eq!(crt_fclose(f), 0);

        // Mode invalide / fichier absent / évasion du sandbox.
        set_errno(0);
        assert!(crt_fopen(path.as_ptr().cast(), c("z").as_ptr().cast()).is_null());
        assert_eq!(get_errno(), EINVAL);
        let missing = c(&format!("{dir}/nope.txt"));
        assert!(crt_fopen(missing.as_ptr().cast(), c("r").as_ptr().cast()).is_null());
        assert_eq!(get_errno(), ENOENT);
        let mut o2: *mut c_void = ptr::dangling_mut::<c_void>();
        assert_eq!(
            crt_fopen_s(
                &mut o2,
                c("temp:/../../etc/passwd").as_ptr().cast(),
                c("r").as_ptr().cast()
            ),
            EACCES
        );
        assert!(o2.is_null());
        assert!(crt_fopen(c("/home/x").as_ptr().cast(), c("r").as_ptr().cast()).is_null());
        // Flux inconnu.
        set_errno(0);
        assert_eq!(crt_fgetc(0x1234 as *mut c_void), EOF);
        assert_eq!(get_errno(), EINVAL);
    }

    #[test]
    fn wide_stdio_utf8_and_fwprintf() {
        let dir = unique_dir("wide");
        let path = w(&format!("{dir}/u.txt"));
        let f = crt_wfopen(path.as_ptr(), w("w, ccs=UTF-8").as_ptr());
        assert!(!f.is_null());
        assert_eq!(crt_fputws(w("héllo\n").as_ptr(), f), 0);
        let fmt = w("%s=%d\n");
        let arg = w("π");
        assert_eq!(
            __stdio_common_vfwprintf(
                LEGACY,
                f,
                fmt.as_ptr(),
                ptr::null_mut(),
                [arg.as_ptr() as u64, 3].as_ptr() as *mut c_void
            ),
            4
        );
        assert_eq!(crt_fclose(f), 0);

        let mut f2: *mut c_void = ptr::null_mut();
        assert_eq!(crt_wfopen_s(&mut f2, path.as_ptr(), w("rb").as_ptr()), 0);
        let mut raw = [0u8; 32];
        let n = crt_fread(raw.as_mut_ptr(), 1, 32, f2);
        assert_eq!(&raw[..n], "\u{FEFF}héllo\r\nπ=3\r\n".as_bytes());
        assert_eq!(crt_fclose(f2), 0);

        let f3 = crt_wfopen(path.as_ptr(), w("r, ccs=UTF-8").as_ptr());
        let mut line = [0u16; 16];
        assert!(!crt_fgetws(line.as_mut_ptr(), 16, f3).is_null());
        assert_eq!(String::from_utf16_lossy(&line[..6]), "héllo\n");
        assert_eq!(crt_fgetwc(f3), 0x3C0);
        assert_eq!(crt_fclose(f3), 0);

        // ANSI (locale « C ») : caractère > 0xFF -> EILSEQ.
        let p2 = w(&format!("{dir}/a.txt"));
        let f4 = crt_wfopen(p2.as_ptr(), w("w").as_ptr());
        set_errno(0);
        assert_eq!(crt_fputwc(0x3C0, f4), WEOF);
        assert_eq!(get_errno(), EILSEQ);
        assert_eq!(crt_fputwc(0xE9, f4), 0xE9);
        assert_eq!(crt_fclose(f4), 0);
    }

    #[test]
    fn filesystem_through_vfs() {
        let dir = unique_dir("fs");
        let sub = c(&format!("{dir}/sub"));
        assert_eq!(crt_mkdir(sub.as_ptr().cast()), 0);
        set_errno(0);
        assert_eq!(crt_mkdir(sub.as_ptr().cast()), -1);
        assert_eq!(get_errno(), EEXIST);

        let file = c(&format!("{dir}/sub/f.bin"));
        let f = crt_fopen(file.as_ptr().cast(), c("wb").as_ptr().cast());
        assert_eq!(crt_fwrite(b"12345".as_ptr(), 1, 5, f), 5);
        assert_eq!(crt_fclose(f), 0);

        let mut st = Stat64::default();
        assert_eq!(crt_stat64(file.as_ptr().cast(), &mut st), 0);
        assert_eq!(st.st_size, 5);
        assert_eq!(st.st_mode & S_IFREG, S_IFREG);
        assert_eq!(
            st.st_mode & 0o777,
            0o666 & 0o600 | 0o066 & (0o600 >> 3 | 0o600 >> 6)
        );
        assert_eq!(st.st_dev, 19);
        assert!(st.st_mtime > 0);
        let mut sd = Stat64i32::default();
        let wsub = w(&format!("{dir}/sub"));
        assert_eq!(crt_wstat64i32(wsub.as_ptr(), &mut sd), 0);
        assert_eq!(sd.st_mode & S_IFDIR, S_IFDIR);
        let mut ws = Stat64::default();
        assert_eq!(crt_wstat64(wsub.as_ptr(), &mut ws), 0);
        set_errno(0);
        assert_eq!(
            crt_stat64(c(&format!("{dir}/none")).as_ptr().cast(), &mut st),
            -1
        );
        assert_eq!(get_errno(), ENOENT);
        assert_eq!(crt_stat64i32(c("/home").as_ptr().cast(), &mut sd), -1);

        assert_eq!(crt_access(file.as_ptr().cast(), 6), 0);
        assert_eq!(crt_access(file.as_ptr().cast(), 1), -1);
        assert_eq!(get_errno(), EINVAL);
        assert_eq!(crt_waccess(w(&format!("{dir}/x")).as_ptr(), 0), -1);
        assert_eq!(get_errno(), ENOENT);

        // _rmdir sur répertoire non vide / fichier.
        assert_eq!(crt_rmdir(sub.as_ptr().cast()), -1);
        assert_eq!(get_errno(), ENOTEMPTY);
        assert_eq!(crt_rmdir(file.as_ptr().cast()), -1);
        assert_eq!(get_errno(), ENOTEMPTY);

        // rename : destination existante = EACCES.
        let other = c(&format!("{dir}/sub/g.bin"));
        let f = crt_fopen(other.as_ptr().cast(), c("w").as_ptr().cast());
        assert_eq!(crt_fclose(f), 0);
        assert_eq!(crt_rename(file.as_ptr().cast(), other.as_ptr().cast()), -1);
        assert_eq!(get_errno(), EACCES);
        let moved = w(&format!("{dir}/moved.bin"));
        assert_eq!(
            crt_wrename(w(&format!("{dir}/sub/f.bin")).as_ptr(), moved.as_ptr()),
            0
        );
        assert_eq!(
            crt_rename(
                c(&format!("{dir}/nope")).as_ptr().cast(),
                file.as_ptr().cast()
            ),
            -1
        );
        assert_eq!(get_errno(), ENOENT);

        // remove / _wremove / _rmdir / _wrmdir.
        assert_eq!(crt_remove(other.as_ptr().cast()), 0);
        assert_eq!(crt_wremove(moved.as_ptr()), 0);
        assert_eq!(crt_remove(other.as_ptr().cast()), -1);
        assert_eq!(get_errno(), ENOENT);
        assert_eq!(crt_remove(sub.as_ptr().cast()), -1);
        assert_eq!(get_errno(), EACCES);
        assert_eq!(crt_wrmdir(wsub.as_ptr()), 0);
        let wd = w(&format!("{dir}/wd"));
        assert_eq!(crt_wmkdir(wd.as_ptr()), 0);
        assert_eq!(crt_wrmdir(wd.as_ptr()), 0);
        assert_eq!(crt_rmdir(c(&dir).as_ptr().cast()), 0);
        // Racine de montage et évasion.
        assert_eq!(crt_rmdir(c("temp:/").as_ptr().cast()), -1);
        assert_eq!(crt_mkdir(c("temp:/../evil").as_ptr().cast()), -1);
        assert_eq!(get_errno(), EACCES);
    }

    #[test]
    fn std_streams_exist() {
        let out = __acrt_iob_func(1);
        let err = __acrt_iob_func(2);
        assert!(!out.is_null() && out != err);
        assert_eq!(crt_fileno(__acrt_iob_func(0)), 0);
        assert_eq!(crt_fileno(err), 2);
        assert!(__acrt_iob_func(3).is_null());
        assert_eq!(crt_fflush(ptr::null_mut()), 0);
    }
}
