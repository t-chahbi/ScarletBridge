//! Implémentations HLE Rust (`extern "win64"`) pour `kernel32.dll`, `kernelbase.dll`,
//! `api-ms-win-core-*`, `ntdll.dll` et `bcryptprimitives.dll`.

#![allow(non_snake_case, unused_variables)]

use core::ffi::c_void;
use std::collections::HashMap;
use std::ffi::CStr;
use std::io::Write;
use std::ptr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sb_common::abi::{WIN_FALSE, WIN_TRUE, WinBool};
use sb_common::wide::{from_pcwstr, write_wide_buf};
use sb_common::{ffi_guard, sb_debug, sb_info, sb_trace, sb_warn};
use sb_loader::{ImportSymbol, last_error, module_manager, set_last_error};

use crate::file_io;
use crate::process::{
    HANDLE_STDERR, HANDLE_STDIN, HANDLE_STDOUT, PSEUDO_CURRENT_PROCESS, PSEUDO_CURRENT_THREAD,
    process_heap_handle, process_state,
};
use crate::sync;

const CHANNEL: &str = "kernel32";
const ERROR_MOD_NOT_FOUND: u32 = 126;
const ERROR_PROC_NOT_FOUND: u32 = 127;
const ERROR_ENVVAR_NOT_FOUND: u32 = 203;
const HEAP_ZERO_MEMORY: u32 = 0x0000_0008;

static PERF_EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

pub fn exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }

    // Modules & processus
    reg!("LoadLibraryA", LoadLibraryA);
    reg!("LoadLibraryW", LoadLibraryW);
    reg!("GetProcAddress", GetProcAddress);
    reg!("FreeLibrary", FreeLibrary);
    reg!("GetModuleHandleA", GetModuleHandleA);
    reg!("GetModuleHandleW", GetModuleHandleW);
    reg!("GetModuleHandleExW", GetModuleHandleExW);
    reg!("GetModuleFileNameW", GetModuleFileNameW);
    reg!("ExitProcess", ExitProcess);
    reg!("TerminateProcess", TerminateProcess);
    reg!("GetCurrentProcess", GetCurrentProcess);
    reg!("GetCurrentProcessId", GetCurrentProcessId);
    reg!("GetCurrentThread", GetCurrentThread);
    reg!("GetCurrentThreadId", GetCurrentThreadId);
    reg!("GetProcessId", GetProcessId);
    reg!("GetCommandLineA", GetCommandLineA);
    reg!("GetCommandLineW", GetCommandLineW);
    reg!("GetEnvironmentVariableW", GetEnvironmentVariableW);
    reg!("GetEnvironmentStringsW", GetEnvironmentStringsW);
    reg!("FreeEnvironmentStringsW", FreeEnvironmentStringsW);
    reg!("GetSystemInfo", GetSystemInfo);
    reg!("GetLastError", GetLastError);
    reg!("SetLastError", SetLastError);
    reg!("EncodePointer", EncodePointer);
    reg!("DecodePointer", DecodePointer);
    reg!("SetUnhandledExceptionFilter", SetUnhandledExceptionFilter);
    reg!("SetThreadStackGuarantee", SetThreadStackGuarantee);
    reg!("IsThreadAFiber", IsThreadAFiber);
    reg!("InitializeSListHead", InitializeSListHead);

    // Mémoire & tas
    reg!("GetProcessHeap", GetProcessHeap);
    reg!("HeapAlloc", HeapAlloc);
    reg!("HeapReAlloc", HeapReAlloc);
    reg!("HeapFree", HeapFree);
    reg!("VirtualAlloc", VirtualAlloc);
    reg!("VirtualFree", VirtualFree);
    reg!("VirtualProtect", VirtualProtect);
    reg!("VirtualQuery", VirtualQuery);

    // Console & E/S fichiers 64 bits (VFS sandboxé) + Mapping + Find + IOCP
    reg!("GetStdHandle", GetStdHandle);
    reg!("SetStdHandle", SetStdHandle);
    reg!("GetFileType", GetFileType);
    reg!("GetConsoleMode", GetConsoleMode);
    reg!("GetConsoleOutputCP", GetConsoleOutputCP);
    reg!("WriteConsoleW", WriteConsoleW);
    m.insert("CreateFileA", file_io::CreateFileA as *const () as usize);
    m.insert("CreateFileW", file_io::CreateFileW as *const () as usize);
    m.insert("CreateFile2", file_io::CreateFile2 as *const () as usize);
    m.insert("ReadFile", file_io::ReadFile as *const () as usize);
    m.insert("ReadFileEx", file_io::ReadFileEx as *const () as usize);
    m.insert("WriteFile", file_io::WriteFile as *const () as usize);
    m.insert(
        "GetOverlappedResult",
        file_io::GetOverlappedResult as *const () as usize,
    );
    m.insert(
        "GetOverlappedResultEx",
        file_io::GetOverlappedResultEx as *const () as usize,
    );
    m.insert("GetFileSize", file_io::GetFileSize as *const () as usize);
    m.insert(
        "GetFileSizeEx",
        file_io::GetFileSizeEx as *const () as usize,
    );
    m.insert(
        "SetFilePointer",
        file_io::SetFilePointer as *const () as usize,
    );
    m.insert(
        "SetFilePointerEx",
        file_io::SetFilePointerEx as *const () as usize,
    );
    m.insert("SetEndOfFile", file_io::SetEndOfFile as *const () as usize);
    m.insert(
        "GetFileInformationByHandle",
        file_io::GetFileInformationByHandle as *const () as usize,
    );
    m.insert(
        "GetFileAttributesA",
        file_io::GetFileAttributesA as *const () as usize,
    );
    m.insert(
        "GetFileAttributesW",
        file_io::GetFileAttributesW as *const () as usize,
    );
    m.insert(
        "GetFileAttributesExW",
        file_io::GetFileAttributesExW as *const () as usize,
    );
    m.insert(
        "CreateDirectoryW",
        file_io::CreateDirectoryW as *const () as usize,
    );
    m.insert("DeleteFileW", file_io::DeleteFileW as *const () as usize);
    m.insert("MoveFileExW", file_io::MoveFileExW as *const () as usize);
    m.insert(
        "GetCurrentDirectoryW",
        file_io::GetCurrentDirectoryW as *const () as usize,
    );
    m.insert(
        "GetFullPathNameW",
        file_io::GetFullPathNameW as *const () as usize,
    );
    m.insert(
        "CreateFileMappingA",
        file_io::CreateFileMappingA as *const () as usize,
    );
    m.insert(
        "CreateFileMappingW",
        file_io::CreateFileMappingW as *const () as usize,
    );
    m.insert(
        "MapViewOfFile",
        file_io::MapViewOfFile as *const () as usize,
    );
    m.insert(
        "MapViewOfFileEx",
        file_io::MapViewOfFileEx as *const () as usize,
    );
    m.insert(
        "UnmapViewOfFile",
        file_io::UnmapViewOfFile as *const () as usize,
    );
    m.insert(
        "FindFirstFileA",
        file_io::FindFirstFileA as *const () as usize,
    );
    m.insert(
        "FindNextFileA",
        file_io::FindNextFileA as *const () as usize,
    );
    m.insert(
        "FindFirstFileW",
        file_io::FindFirstFileW as *const () as usize,
    );
    m.insert(
        "FindFirstFileExW",
        file_io::FindFirstFileExW as *const () as usize,
    );
    m.insert(
        "FindNextFileW",
        file_io::FindNextFileW as *const () as usize,
    );
    m.insert("FindClose", file_io::FindClose as *const () as usize);
    m.insert(
        "CreateIoCompletionPort",
        file_io::CreateIoCompletionPort as *const () as usize,
    );
    m.insert(
        "PostQueuedCompletionStatus",
        file_io::PostQueuedCompletionStatus as *const () as usize,
    );
    m.insert(
        "GetQueuedCompletionStatus",
        file_io::GetQueuedCompletionStatus as *const () as usize,
    );
    reg!("NtWriteFile", NtWriteFile);
    reg!("FlushFileBuffers", FlushFileBuffers);
    reg!("CloseHandle", CloseHandle);
    reg!("MultiByteToWideChar", MultiByteToWideChar);
    reg!("WideCharToMultiByte", WideCharToMultiByte);
    reg!("lstrlenW", lstrlenW);

    // Synchronisation multi-thread réelle, TLS/FLS par thread, Threads & Temps
    macro_rules! reg_sync {
        ($name:expr, $f:ident) => {
            m.insert($name, sync::$f as *const () as usize);
        };
    }
    reg_sync!("TlsAlloc", TlsAlloc);
    reg_sync!("TlsGetValue", TlsGetValue);
    reg_sync!("TlsSetValue", TlsSetValue);
    reg_sync!("TlsFree", TlsFree);
    reg_sync!("FlsAlloc", FlsAlloc);
    reg_sync!("FlsGetValue", FlsGetValue);
    reg_sync!("FlsSetValue", FlsSetValue);
    reg_sync!("FlsFree", FlsFree);
    reg_sync!("InitializeCriticalSection", InitializeCriticalSection);
    reg_sync!(
        "InitializeCriticalSectionAndSpinCount",
        InitializeCriticalSectionAndSpinCount
    );
    reg_sync!("InitializeCriticalSectionEx", InitializeCriticalSectionEx);
    reg_sync!("DeleteCriticalSection", DeleteCriticalSection);
    reg_sync!("EnterCriticalSection", EnterCriticalSection);
    reg_sync!("TryEnterCriticalSection", TryEnterCriticalSection);
    reg_sync!("LeaveCriticalSection", LeaveCriticalSection);
    reg_sync!("InitializeSRWLock", InitializeSRWLock);
    reg_sync!("AcquireSRWLockExclusive", AcquireSRWLockExclusive);
    reg_sync!("TryAcquireSRWLockExclusive", TryAcquireSRWLockExclusive);
    reg_sync!("ReleaseSRWLockExclusive", ReleaseSRWLockExclusive);
    reg_sync!("AcquireSRWLockShared", AcquireSRWLockShared);
    reg_sync!("TryAcquireSRWLockShared", TryAcquireSRWLockShared);
    reg_sync!("ReleaseSRWLockShared", ReleaseSRWLockShared);
    reg_sync!("InitializeConditionVariable", InitializeConditionVariable);
    reg_sync!("SleepConditionVariableCS", SleepConditionVariableCS);
    reg_sync!("SleepConditionVariableSRW", SleepConditionVariableSRW);
    reg_sync!("WakeConditionVariable", WakeConditionVariable);
    reg_sync!("WakeAllConditionVariable", WakeAllConditionVariable);
    reg_sync!("InitOnceInitialize", InitOnceInitialize);
    reg_sync!("InitOnceBeginInitialize", InitOnceBeginInitialize);
    reg_sync!("InitOnceComplete", InitOnceComplete);
    reg_sync!("InitOnceExecuteOnce", InitOnceExecuteOnce);
    reg_sync!("WaitOnAddress", WaitOnAddress);
    reg_sync!("WakeByAddressSingle", WakeByAddressSingle);
    reg_sync!("WakeByAddressAll", WakeByAddressAll);
    reg_sync!("CreateEventA", CreateEventA);
    reg_sync!("CreateEventW", CreateEventW);
    reg_sync!("CreateEventExA", CreateEventExA);
    reg_sync!("CreateEventExW", CreateEventExW);
    reg_sync!("SetEvent", SetEvent);
    reg_sync!("ResetEvent", ResetEvent);
    reg_sync!("PulseEvent", PulseEvent);
    reg_sync!("CreateSemaphoreA", CreateSemaphoreA);
    reg_sync!("CreateSemaphoreW", CreateSemaphoreW);
    reg_sync!("CreateSemaphoreExW", CreateSemaphoreExW);
    reg_sync!("ReleaseSemaphore", ReleaseSemaphore);
    reg_sync!("CreateMutexA", CreateMutexA);
    reg_sync!("CreateMutexW", CreateMutexW);
    reg_sync!("CreateMutexExW", CreateMutexExW);
    reg_sync!("ReleaseMutex", ReleaseMutex);
    reg_sync!("CreateWaitableTimerW", CreateWaitableTimerW);
    reg_sync!("CreateWaitableTimerExW", CreateWaitableTimerExW);
    reg_sync!("SetWaitableTimer", SetWaitableTimer);
    reg_sync!("SetWaitableTimerEx", SetWaitableTimerEx);
    reg_sync!("CancelWaitableTimer", CancelWaitableTimer);
    reg_sync!("WaitForSingleObject", WaitForSingleObject);
    reg_sync!("WaitForSingleObjectEx", WaitForSingleObjectEx);
    reg_sync!("WaitForMultipleObjects", WaitForMultipleObjects);
    reg_sync!("WaitForMultipleObjectsEx", WaitForMultipleObjectsEx);
    reg_sync!("CreateThread", CreateThread);
    reg_sync!("ResumeThread", ResumeThread);
    reg_sync!("SuspendThread", SuspendThread);
    reg_sync!("ExitThread", ExitThread);
    reg_sync!("GetExitCodeThread", GetExitCodeThread);
    reg_sync!("GetThreadId", GetThreadId);
    reg_sync!("SetThreadPriority", SetThreadPriority);
    reg_sync!("GetThreadPriority", GetThreadPriority);
    reg_sync!("SetThreadAffinityMask", SetThreadAffinityMask);
    reg_sync!("SetThreadIdealProcessor", SetThreadIdealProcessor);
    reg_sync!("SetThreadDescription", SetThreadDescription);
    reg_sync!("GetActiveProcessorCount", GetActiveProcessorCount);
    reg_sync!("GetCurrentProcessorNumber", GetCurrentProcessorNumber);
    reg_sync!("GetProcessAffinityMask", GetProcessAffinityMask);
    reg_sync!("DuplicateHandle", DuplicateHandle);
    reg!("Sleep", Sleep);
    reg!("SleepEx", SleepEx);
    reg!("SwitchToThread", SwitchToThread);
    reg!("QueryPerformanceCounter", QueryPerformanceCounter);
    reg!("QueryPerformanceFrequency", QueryPerformanceFrequency);
    reg!("GetSystemTimeAsFileTime", GetSystemTimeAsFileTime);
    reg!(
        "GetSystemTimePreciseAsFileTime",
        GetSystemTimePreciseAsFileTime
    );

    // bcryptprimitives.dll
    reg!("ProcessPrng", ProcessPrng);

    // advapi32.dll : registre Windows inexistant sous ScarletBridge → toujours « introuvable ».
    reg!("RegOpenKeyW", RegOpenKeyW);
    reg!("RegOpenKeyA", RegOpenKeyA);
    reg!("RegOpenKeyExW", RegOpenKeyExW);
    reg!("RegOpenKeyExA", RegOpenKeyExA);
    reg!("RegQueryValueExW", RegQueryValueExW);
    reg!("RegQueryValueExA", RegQueryValueExA);
    reg!("RegCloseKey", RegCloseKey);

    m
}

// ---------- advapi32 : registre (stub honnête) ----------

const ERROR_SUCCESS: u32 = 0;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_INVALID_HANDLE: u32 = 6;

/// Aucune ruche n'existe : met `*out` à NULL et retourne `ERROR_FILE_NOT_FOUND`.
fn reg_not_found(out: *mut usize) -> u32 {
    if !out.is_null() {
        // SAFETY: `out` est un `PHKEY` fourni par l'appelant invité, vérifié non-nul.
        unsafe { *out = 0 };
    }
    ERROR_FILE_NOT_FOUND
}

extern "win64" fn RegOpenKeyW(_hkey: usize, _sub: *const u16, out: *mut usize) -> u32 {
    ffi_guard!(CHANNEL, "RegOpenKeyW", ERROR_FILE_NOT_FOUND, {
        reg_not_found(out)
    })
}
extern "win64" fn RegOpenKeyA(_hkey: usize, _sub: *const i8, out: *mut usize) -> u32 {
    ffi_guard!(CHANNEL, "RegOpenKeyA", ERROR_FILE_NOT_FOUND, {
        reg_not_found(out)
    })
}
extern "win64" fn RegOpenKeyExW(
    _hkey: usize,
    _sub: *const u16,
    _opts: u32,
    _sam: u32,
    out: *mut usize,
) -> u32 {
    ffi_guard!(CHANNEL, "RegOpenKeyExW", ERROR_FILE_NOT_FOUND, {
        reg_not_found(out)
    })
}
extern "win64" fn RegOpenKeyExA(
    _hkey: usize,
    _sub: *const i8,
    _opts: u32,
    _sam: u32,
    out: *mut usize,
) -> u32 {
    ffi_guard!(CHANNEL, "RegOpenKeyExA", ERROR_FILE_NOT_FOUND, {
        reg_not_found(out)
    })
}
extern "win64" fn RegQueryValueExW(
    _hkey: usize,
    _name: *const u16,
    _reserved: *mut u32,
    _ty: *mut u32,
    _data: *mut u8,
    _len: *mut u32,
) -> u32 {
    ffi_guard!(CHANNEL, "RegQueryValueExW", ERROR_FILE_NOT_FOUND, {
        // Handle NULL (clé jamais ouverte) → invalide, sinon valeur absente.
        if _hkey == 0 {
            ERROR_INVALID_HANDLE
        } else {
            ERROR_FILE_NOT_FOUND
        }
    })
}
extern "win64" fn RegQueryValueExA(
    _hkey: usize,
    _name: *const i8,
    _reserved: *mut u32,
    _ty: *mut u32,
    _data: *mut u8,
    _len: *mut u32,
) -> u32 {
    ffi_guard!(CHANNEL, "RegQueryValueExA", ERROR_FILE_NOT_FOUND, {
        if _hkey == 0 {
            ERROR_INVALID_HANDLE
        } else {
            ERROR_FILE_NOT_FOUND
        }
    })
}
extern "win64" fn RegCloseKey(_hkey: usize) -> u32 {
    ffi_guard!(CHANNEL, "RegCloseKey", ERROR_SUCCESS, { ERROR_SUCCESS })
}

// ---------- Modules & Processus ----------

extern "win64" fn LoadLibraryA(name: *const i8) -> usize {
    ffi_guard!(CHANNEL, "LoadLibraryA", 0, {
        if name.is_null() {
            set_last_error(ERROR_MOD_NOT_FOUND);
            return 0;
        }
        // SAFETY: `name` est une chaîne C non-nulle.
        let s = unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned();
        load_library_impl(&s)
    })
}

extern "win64" fn LoadLibraryW(name: *const u16) -> usize {
    ffi_guard!(CHANNEL, "LoadLibraryW", 0, {
        // SAFETY: `name` est un `PCWSTR` valide ou nul.
        let Some(s) = (unsafe { from_pcwstr(name) }) else {
            set_last_error(ERROR_MOD_NOT_FOUND);
            return 0;
        };
        load_library_impl(&s)
    })
}

pub(crate) fn load_library_impl(name: &str) -> usize {
    sb_debug!(CHANNEL, "LoadLibrary('{name}')");
    match module_manager().load_library(name) {
        Ok(module) => {
            set_last_error(0);
            module.base
        }
        Err(err) => {
            if let Some(handle) = module_manager().virtual_module_handle(name) {
                set_last_error(0);
                return handle;
            }
            sb_warn!(CHANNEL, "LoadLibrary('{name}') a échoué : {err}");
            set_last_error(ERROR_MOD_NOT_FOUND);
            0
        }
    }
}

extern "win64" fn GetProcAddress(module: usize, proc_name: *const i8) -> usize {
    ffi_guard!(CHANNEL, "GetProcAddress", 0, {
        let raw = proc_name as usize;
        if raw == 0 {
            set_last_error(ERROR_PROC_NOT_FOUND);
            return 0;
        }
        let sym = if raw <= 0xFFFF {
            ImportSymbol::Ordinal(raw as u16)
        } else {
            // SAFETY: `proc_name > 0xFFFF` est une chaîne ASCII terminée par nul.
            let name = unsafe { CStr::from_ptr(proc_name) }
                .to_string_lossy()
                .into_owned();
            ImportSymbol::Name(name)
        };

        if let Some(addr) = module_manager().get_proc_address(module, &sym) {
            sb_trace!(CHANNEL, "GetProcAddress({module:#x}, {sym}) -> {addr:#x}");
            set_last_error(0);
            return addr;
        }

        // Fallback si `module` correspond à un handle virtuel de DLL HLE.
        if let Some(m) = module_manager().find_by_base(module)
            && let Some(addr) = module_manager().resolve_hle(&m.name, &sym)
        {
            set_last_error(0);
            return addr;
        }

        sb_warn!(CHANNEL, "GetProcAddress({module:#x}, {sym}) -> introuvable");
        set_last_error(ERROR_PROC_NOT_FOUND);
        0
    })
}

extern "win64" fn FreeLibrary(module: usize) -> WinBool {
    ffi_guard!(CHANNEL, "FreeLibrary", WIN_FALSE, {
        if (module & 3) != 0 && crate::kernel_ext::misc::free_datafile_module(module) {
            return WIN_TRUE;
        }
        if module_manager().unload_library(module)
            || module_manager().virtual_module_name(module).is_some()
        {
            WIN_TRUE
        } else {
            WIN_FALSE
        }
    })
}

extern "win64" fn GetModuleHandleA(name: *const i8) -> usize {
    ffi_guard!(CHANNEL, "GetModuleHandleA", 0, {
        if name.is_null() {
            return module_manager().main_module_base();
        }
        // SAFETY: chaîne C terminée par nul.
        let s = unsafe { CStr::from_ptr(name) }.to_string_lossy();
        module_manager()
            .find_by_name(&s)
            .map(|m| m.base)
            .or_else(|| module_manager().virtual_module_handle(&s))
            .unwrap_or(0)
    })
}

extern "win64" fn GetModuleHandleW(name: *const u16) -> usize {
    ffi_guard!(CHANNEL, "GetModuleHandleW", 0, {
        if name.is_null() {
            return module_manager().main_module_base();
        }
        // SAFETY: chaîne UTF-16LE terminée par nul.
        let Some(s) = (unsafe { from_pcwstr(name) }) else {
            return 0;
        };
        module_manager()
            .find_by_name(&s)
            .map(|m| m.base)
            .or_else(|| module_manager().virtual_module_handle(&s))
            .unwrap_or(0)
    })
}

extern "win64" fn GetModuleHandleExW(
    _flags: u32,
    name: *const u16,
    out_module: *mut usize,
) -> WinBool {
    ffi_guard!(CHANNEL, "GetModuleHandleExW", WIN_FALSE, {
        if out_module.is_null() {
            return WIN_FALSE;
        }
        let handle = GetModuleHandleW(name);
        // SAFETY: `out_module` vérifié non-nul.
        unsafe {
            *out_module = handle;
        }
        if handle != 0 { WIN_TRUE } else { WIN_FALSE }
    })
}

extern "win64" fn GetModuleFileNameW(module: usize, buf: *mut u16, size: u32) -> u32 {
    ffi_guard!(CHANNEL, "GetModuleFileNameW", 0, {
        // Chemin invité (`G:\...`), jamais le chemin hôte.
        crate::kernel_ext::misc::module_file_name_w(module, buf, size)
    })
}

extern "win64" fn ExitProcess(exit_code: u32) -> ! {
    sb_info!(CHANNEL, "ExitProcess(code={exit_code})");
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(exit_code as i32);
}

extern "win64" fn TerminateProcess(_handle: usize, exit_code: u32) -> WinBool {
    ExitProcess(exit_code);
}

extern "win64" fn GetCurrentProcess() -> usize {
    PSEUDO_CURRENT_PROCESS
}

extern "win64" fn GetCurrentProcessId() -> u32 {
    // SAFETY: appel POSIX sans précondition.
    unsafe { libc::getpid() as u32 }
}

extern "win64" fn GetCurrentThread() -> usize {
    PSEUDO_CURRENT_THREAD
}

extern "win64" fn GetCurrentThreadId() -> u32 {
    // SAFETY: syscall `gettid` Linux.
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

extern "win64" fn GetProcessId(_handle: usize) -> u32 {
    GetCurrentProcessId()
}

extern "win64" fn GetCommandLineA() -> *const i8 {
    ffi_guard!(CHANNEL, "GetCommandLineA", ptr::null(), {
        process_state().read().cmdline_narrow.as_ptr()
    })
}

extern "win64" fn GetCommandLineW() -> *const u16 {
    ffi_guard!(CHANNEL, "GetCommandLineW", ptr::null(), {
        process_state().read().cmdline_wide.as_ptr()
    })
}

extern "win64" fn GetEnvironmentVariableW(name: *const u16, buf: *mut u16, size: u32) -> u32 {
    ffi_guard!(CHANNEL, "GetEnvironmentVariableW", 0, {
        // SAFETY: `name` est un `PCWSTR` valide ou nul.
        let Some(key) = (unsafe { from_pcwstr(name) }) else {
            set_last_error(ERROR_ENVVAR_NOT_FOUND);
            return 0;
        };
        let Ok(val) = std::env::var(&key) else {
            set_last_error(ERROR_ENVVAR_NOT_FOUND);
            return 0;
        };
        let units: Vec<u16> = val.encode_utf16().collect();
        if buf.is_null() || (size as usize) <= units.len() {
            return (units.len() + 1) as u32;
        }
        // SAFETY: `buf` a une capacité > `units.len()`.
        unsafe { write_wide_buf(&val, buf, size as usize) as u32 }
    })
}

extern "win64" fn GetEnvironmentStringsW() -> *const u16 {
    ffi_guard!(CHANNEL, "GetEnvironmentStringsW", ptr::null(), {
        process_state().read().env_wide_block.as_ptr()
    })
}

extern "win64" fn FreeEnvironmentStringsW(_block: *const u16) -> WinBool {
    WIN_TRUE
}

#[repr(C)]
struct SystemInfo {
    oem_id: u32,
    page_size: u32,
    min_app_addr: usize,
    max_app_addr: usize,
    active_processor_mask: usize,
    number_of_processors: u32,
    processor_type: u32,
    allocation_granularity: u32,
    processor_level: u16,
    processor_revision: u16,
}

extern "win64" fn GetSystemInfo(info: *mut SystemInfo) {
    ffi_guard!(CHANNEL, "GetSystemInfo", (), {
        if info.is_null() {
            return;
        }
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(8);
        // SAFETY: `info` est un pointeur non-nul vers `SYSTEM_INFO`.
        unsafe {
            *info = SystemInfo {
                oem_id: 9, // PROCESSOR_ARCHITECTURE_AMD64
                page_size: 4096,
                min_app_addr: 0x0001_0000,
                max_app_addr: 0x0000_7FFF_FFFE_FFFF,
                active_processor_mask: (1usize << cpus.min(63)) - 1,
                number_of_processors: cpus,
                processor_type: 8664,
                allocation_granularity: 65536,
                processor_level: 23,
                processor_revision: 0,
            };
        }
    });
}

extern "win64" fn GetLastError() -> u32 {
    last_error()
}

extern "win64" fn SetLastError(code: u32) {
    set_last_error(code);
}

extern "win64" fn EncodePointer(ptr_val: usize) -> usize {
    ptr_val
}

extern "win64" fn DecodePointer(ptr_val: usize) -> usize {
    ptr_val
}

/// Filtre d'exception de niveau supérieur (`SetUnhandledExceptionFilter`).
static TOP_LEVEL_FILTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Filtre courant posé par l'invité (0 si aucun).
pub(crate) fn top_level_exception_filter() -> usize {
    TOP_LEVEL_FILTER.load(Ordering::Acquire)
}

/// Remplace le filtre de niveau supérieur et retourne le précédent.
pub(crate) fn set_top_level_exception_filter(filter: usize) -> usize {
    TOP_LEVEL_FILTER.swap(filter, Ordering::AcqRel)
}

extern "win64" fn SetUnhandledExceptionFilter(filter: usize) -> usize {
    set_top_level_exception_filter(filter)
}

extern "win64" fn SetThreadStackGuarantee(_bytes: *mut u32) -> WinBool {
    WIN_TRUE
}

extern "win64" fn IsThreadAFiber() -> WinBool {
    WIN_FALSE
}

extern "win64" fn InitializeSListHead(head: *mut [u64; 2]) {
    if !head.is_null() {
        // SAFETY: `SLIST_HEADER` fait 16 octets sur x86-64.
        unsafe {
            *head = [0, 0];
        }
    }
}

// ---------- Mémoire & Tas ----------

extern "win64" fn GetProcessHeap() -> usize {
    process_heap_handle()
}

extern "win64" fn HeapAlloc(_heap: usize, flags: u32, bytes: usize) -> *mut c_void {
    ffi_guard!(CHANNEL, "HeapAlloc", ptr::null_mut(), {
        let size = bytes.max(1);
        // SAFETY: `calloc` / `malloc` POSIX standard.
        let p = unsafe {
            if (flags & HEAP_ZERO_MEMORY) != 0 {
                libc::calloc(1, size)
            } else {
                libc::malloc(size)
            }
        };
        p.cast::<c_void>()
    })
}

extern "win64" fn HeapReAlloc(
    _heap: usize,
    _flags: u32,
    mem: *mut c_void,
    bytes: usize,
) -> *mut c_void {
    ffi_guard!(CHANNEL, "HeapReAlloc", ptr::null_mut(), {
        // SAFETY: `realloc` POSIX sur un pointeur issu de `HeapAlloc`.
        unsafe { libc::realloc(mem.cast::<libc::c_void>(), bytes.max(1)) }.cast::<c_void>()
    })
}

extern "win64" fn HeapFree(_heap: usize, _flags: u32, mem: *mut c_void) -> WinBool {
    ffi_guard!(CHANNEL, "HeapFree", WIN_FALSE, {
        if !mem.is_null() {
            // SAFETY: libération d'un pointeur issu de `HeapAlloc`.
            unsafe {
                libc::free(mem.cast::<libc::c_void>());
            }
        }
        WIN_TRUE
    })
}

extern "win64" fn VirtualAlloc(
    addr: *mut c_void,
    size: usize,
    alloc_type: u32,
    protect: u32,
) -> *mut c_void {
    ffi_guard!(CHANNEL, "VirtualAlloc", ptr::null_mut(), {
        crate::kernel_ext::vmem::virtual_alloc(addr as usize, size, alloc_type, protect)
            as *mut c_void
    })
}

extern "win64" fn VirtualFree(addr: *mut c_void, size: usize, free_type: u32) -> WinBool {
    ffi_guard!(CHANNEL, "VirtualFree", WIN_FALSE, {
        if crate::kernel_ext::vmem::virtual_free(addr as usize, size, free_type) {
            WIN_TRUE
        } else {
            WIN_FALSE
        }
    })
}

extern "win64" fn VirtualProtect(
    addr: *mut c_void,
    size: usize,
    new_protect: u32,
    old_protect: *mut u32,
) -> WinBool {
    ffi_guard!(CHANNEL, "VirtualProtect", WIN_FALSE, {
        if old_protect.is_null() || size == 0 {
            set_last_error(87); // ERROR_INVALID_PARAMETER (lpflOldProtect obligatoire)
            return WIN_FALSE;
        }
        let Some(prot) = crate::kernel_ext::vmem::win_protect_to_posix(new_protect) else {
            set_last_error(87);
            return WIN_FALSE;
        };
        let previous = crate::kernel_ext::vmem::query(addr as usize).map(|m| m.protect);
        let page_start = (addr as usize) & !0xFFF;
        let len = ((addr as usize) + size - page_start + 0xFFF) & !0xFFF;
        // SAFETY: plage alignée sur page.
        let rc = unsafe { libc::mprotect(page_start as *mut libc::c_void, len, prot) };
        if rc != 0 {
            set_last_error(487); // ERROR_INVALID_ADDRESS
            return WIN_FALSE;
        }
        // SAFETY: `old_protect` vérifié non-nul.
        unsafe {
            *old_protect = previous.filter(|&p| p != 0).unwrap_or(0x01);
        }
        WIN_TRUE
    })
}

extern "win64" fn VirtualQuery(addr: *const c_void, buf: *mut u8, len: usize) -> usize {
    ffi_guard!(CHANNEL, "VirtualQuery", 0, {
        crate::kernel_ext::vmem::virtual_query(addr as usize, buf.cast(), len)
    })
}

// ---------- Console & E/S ----------

extern "win64" fn GetStdHandle(which: u32) -> usize {
    match which as i32 {
        -10 => HANDLE_STDIN,
        -11 => HANDLE_STDOUT,
        -12 => HANDLE_STDERR,
        _ => usize::MAX,
    }
}

extern "win64" fn SetStdHandle(_which: u32, _handle: usize) -> WinBool {
    WIN_TRUE
}

extern "win64" fn GetFileType(handle: usize) -> u32 {
    match handle {
        HANDLE_STDIN | HANDLE_STDOUT | HANDLE_STDERR => 2, // FILE_TYPE_CHAR
        _ => 1,                                            // FILE_TYPE_DISK
    }
}

extern "win64" fn GetConsoleMode(handle: usize, out_mode: *mut u32) -> WinBool {
    if matches!(handle, HANDLE_STDIN | HANDLE_STDOUT | HANDLE_STDERR) {
        if !out_mode.is_null() {
            // SAFETY: `out_mode` vérifié non-nul.
            unsafe {
                *out_mode = 0x0007;
            }
        }
        WIN_TRUE
    } else {
        WIN_FALSE
    }
}

extern "win64" fn GetConsoleOutputCP() -> u32 {
    65001 // UTF-8
}

extern "win64" fn WriteConsoleW(
    handle: usize,
    buf: *const u16,
    chars_to_write: u32,
    chars_written: *mut u32,
    _reserved: *mut c_void,
) -> WinBool {
    ffi_guard!(CHANNEL, "WriteConsoleW", WIN_FALSE, {
        if buf.is_null() {
            return WIN_FALSE;
        }
        // SAFETY: `buf` contient `chars_to_write` unités UTF-16.
        let slice = unsafe { std::slice::from_raw_parts(buf, chars_to_write as usize) };
        let text = String::from_utf16_lossy(slice);
        let res = if handle == HANDLE_STDERR {
            std::io::stderr().write_all(text.as_bytes())
        } else {
            std::io::stdout().write_all(text.as_bytes())
        };
        if !chars_written.is_null() {
            // SAFETY: `chars_written` vérifié non-nul.
            unsafe {
                *chars_written = chars_to_write;
            }
        }
        if res.is_ok() { WIN_TRUE } else { WIN_FALSE }
    })
}

#[repr(C)]
struct IoStatusBlock {
    status: usize,
    information: usize,
}

#[allow(clippy::too_many_arguments)]
extern "win64" fn NtWriteFile(
    handle: usize,
    _event: usize,
    _apc_routine: usize,
    _apc_ctx: usize,
    io_status: *mut IoStatusBlock,
    buf: *const u8,
    length: u32,
    _byte_offset: *const i64,
    _key: *const u32,
) -> i32 {
    ffi_guard!(CHANNEL, "NtWriteFile", 0xC000_0001_u32 as i32, {
        if buf.is_null() {
            return 0xC000_000D_u32 as i32; // STATUS_INVALID_PARAMETER
        }
        // SAFETY: `buf` dispose de `length` octets lisibles selon le contrat `NtWriteFile`.
        let slice = unsafe { std::slice::from_raw_parts(buf, length as usize) };
        let res = if handle == HANDLE_STDERR {
            std::io::stderr().write_all(slice)
        } else {
            std::io::stdout().write_all(slice)
        };
        if res.is_ok() {
            if !io_status.is_null() {
                // SAFETY: `io_status` vérifié non-nul.
                unsafe {
                    (*io_status).status = 0;
                    (*io_status).information = length as usize;
                }
            }
            0 // STATUS_SUCCESS
        } else {
            0xC000_0001_u32 as i32 // STATUS_UNSUCCESSFUL
        }
    })
}

extern "win64" fn FlushFileBuffers(handle: usize) -> WinBool {
    if file_io::flush_file_handle(handle) {
        return WIN_TRUE;
    }
    let _ = if handle == HANDLE_STDERR {
        std::io::stderr().flush()
    } else {
        std::io::stdout().flush()
    };
    WIN_TRUE
}

extern "win64" fn CloseHandle(handle: usize) -> WinBool {
    ffi_guard!(CHANNEL, "CloseHandle", WIN_FALSE, {
        let _ = sync::close_waitable_handle(handle);
        let _ = file_io::close_io_handle(handle);
        WIN_TRUE
    })
}

extern "win64" fn MultiByteToWideChar(
    _code_page: u32,
    _flags: u32,
    src: *const u8,
    src_len: i32,
    dst: *mut u16,
    dst_len: i32,
) -> i32 {
    ffi_guard!(CHANNEL, "MultiByteToWideChar", 0, {
        if src.is_null() {
            return 0;
        }
        // SAFETY: `src` est valide sur `src_len` octets ou terminé par nul si `src_len == -1`.
        let bytes = unsafe {
            if src_len < 0 {
                CStr::from_ptr(src.cast::<i8>()).to_bytes_with_nul()
            } else {
                std::slice::from_raw_parts(src, src_len as usize)
            }
        };
        let text = String::from_utf8_lossy(bytes);
        let units: Vec<u16> = text.encode_utf16().collect();
        if dst.is_null() || dst_len == 0 {
            return units.len() as i32;
        }
        let n = units.len().min(dst_len as usize);
        // SAFETY: `dst` a une capacité d'au moins `n` u16.
        unsafe {
            ptr::copy_nonoverlapping(units.as_ptr(), dst, n);
        }
        n as i32
    })
}

extern "win64" fn WideCharToMultiByte(
    _code_page: u32,
    _flags: u32,
    src: *const u16,
    src_len: i32,
    dst: *mut u8,
    dst_len: i32,
    _default_char: *const u8,
    _used_default: *mut WinBool,
) -> i32 {
    ffi_guard!(CHANNEL, "WideCharToMultiByte", 0, {
        if src.is_null() {
            return 0;
        }
        let slice = if src_len < 0 {
            let mut len = 0usize;
            // SAFETY: chaîne UTF-16LE terminée par nul.
            while unsafe { *src.add(len) } != 0 {
                len += 1;
            }
            // SAFETY: inclut le terminateur nul.
            unsafe { std::slice::from_raw_parts(src, len + 1) }
        } else {
            // SAFETY: `src_len >= 0` unités UTF-16.
            unsafe { std::slice::from_raw_parts(src, src_len as usize) }
        };
        let utf8 = String::from_utf16_lossy(slice);
        let bytes = utf8.as_bytes();
        if dst.is_null() || dst_len == 0 {
            return bytes.len() as i32;
        }
        let n = bytes.len().min(dst_len as usize);
        // SAFETY: `dst` a une capacité d'au moins `n` octets.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), dst, n);
        }
        n as i32
    })
}

extern "win64" fn lstrlenW(s: *const u16) -> i32 {
    if s.is_null() {
        return 0;
    }
    let mut len = 0usize;
    // SAFETY: chaîne UTF-16LE terminée par nul.
    while unsafe { *s.add(len) } != 0 {
        len += 1;
    }
    len as i32
}

// ---------- Temps & Attente légère ----------

extern "win64" fn Sleep(ms: u32) {
    if ms == 0 {
        std::thread::yield_now();
    } else {
        std::thread::sleep(Duration::from_millis(u64::from(ms)));
    }
}

extern "win64" fn SleepEx(ms: u32, _alertable: WinBool) -> u32 {
    Sleep(ms);
    0
}

extern "win64" fn SwitchToThread() -> WinBool {
    std::thread::yield_now();
    WIN_TRUE
}

extern "win64" fn QueryPerformanceCounter(out: *mut i64) -> WinBool {
    if !out.is_null() {
        let epoch = PERF_EPOCH.get_or_init(Instant::now);
        // SAFETY: `out` vérifié non-nul.
        unsafe {
            *out = epoch.elapsed().as_nanos() as i64;
        }
    }
    WIN_TRUE
}

extern "win64" fn QueryPerformanceFrequency(out: *mut i64) -> WinBool {
    if !out.is_null() {
        // SAFETY: `out` vérifié non-nul (fréquence = 1 GHz pour nanosecondes).
        unsafe {
            *out = 1_000_000_000;
        }
    }
    WIN_TRUE
}

extern "win64" fn GetSystemTimeAsFileTime(out: *mut u64) {
    if out.is_null() {
        return;
    }
    // Époque FILETIME (1601-01-01) = UNIX_EPOCH + 11644473600 secondes (en unités de 100 ns).
    let unix_100ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_nanos() / 100) as u64)
        .unwrap_or(0);
    let filetime = unix_100ns.saturating_add(116_444_736_000_000_000);
    // SAFETY: `out` vérifié non-nul (`FILETIME` = 8 octets).
    unsafe {
        *out = filetime;
    }
}

extern "win64" fn GetSystemTimePreciseAsFileTime(out: *mut u64) {
    GetSystemTimeAsFileTime(out);
}

extern "win64" fn ProcessPrng(buf: *mut u8, len: usize) -> WinBool {
    ffi_guard!(CHANNEL, "ProcessPrng", WIN_FALSE, {
        if buf.is_null() {
            return WIN_FALSE;
        }
        // SAFETY: `buf` dispose de `len` octets en écriture.
        let slice = unsafe { std::slice::from_raw_parts_mut(buf, len) };
        let mut filled = 0usize;
        while filled < slice.len() {
            // SAFETY: écrit au plus `len - filled` octets dans le tampon de l'invité.
            let n = unsafe {
                libc::getrandom(slice[filled..].as_mut_ptr().cast(), slice.len() - filled, 0)
            };
            if n <= 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return WIN_FALSE;
            }
            filled += n as usize;
        }
        WIN_TRUE
    })
}
