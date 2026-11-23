//! Extensions HLE de la famille kernel (`kernel32`, `kernelbase`, `ntdll`, `advapi32`, `bcrypt`,
//! `user32`, `ole32`, `api-ms-win-core-*`, `ext-ms-win-*`).
//!
//! Les entrées de `kernel32.rs` sont prioritaires : aucun nom n'est enregistré deux fois.
//! - [`sysinfo`] : débogage, `UnhandledExceptionFilter`, CPUID, topologie, mémoire, ticks, affinités.
//! - [`time`] : SYSTEMTIME/FILETIME, heure locale, formats de date/heure `en-US`, LCID.
//! - [`msg`] : `FormatMessage`, `Local*`/`Global*`.
//! - [`misc`] : `lstrcmp*`, WER, notifications d'état, pipes, `GetModuleFileNameA`, `LoadLibraryEx*`.
//! - [`vmem`] : `VirtualAlloc`/`VirtualFree`/`VirtualQuery(Ex)` réels.
//! - [`rtl`] : `RtlCaptureContext`, unwind x64 ; [`bcrypt`] : RNG ; [`com_ext`] : COM de base.
//! - [`user`] : classes, fenêtres logiques et files de messages.

#![allow(non_snake_case, unused_variables)]

mod bcrypt;
mod com_ext;
pub(crate) mod misc;
mod msg;
mod rtl;
mod sysinfo;
mod time;
mod user;
pub(crate) mod vmem;

use std::collections::HashMap;
use std::ffi::CStr;

pub(crate) const CHANNEL: &str = "kernel_ext";

pub(crate) const ERROR_SUCCESS: u32 = 0;
pub(crate) const ERROR_FILE_NOT_FOUND: u32 = 2;
pub(crate) const ERROR_ACCESS_DENIED: u32 = 5;
pub(crate) const ERROR_INVALID_HANDLE: u32 = 6;
pub(crate) const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
pub(crate) const ERROR_BAD_LENGTH: u32 = 24;
pub(crate) const ERROR_INVALID_PARAMETER: u32 = 87;
pub(crate) const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
pub(crate) const ERROR_MOD_NOT_FOUND: u32 = 126;
pub(crate) const ERROR_MR_MID_NOT_FOUND: u32 = 317;
pub(crate) const ERROR_NOACCESS: u32 = 998;
pub(crate) const ERROR_INVALID_FLAGS: u32 = 1004;

/// Écrit `value` dans `*out` si `out` est non-nul.
pub(crate) fn write_out<T>(out: *mut T, value: T) {
    if !out.is_null() {
        // SAFETY: `out` est un pointeur de sortie fourni par l'invité, vérifié non-nul.
        unsafe { out.write_unaligned(value) };
    }
}

/// Convertit une chaîne ANSI (`LPCSTR`, traitée en UTF-8) en `String`.
pub(crate) fn ansi_to_string(p: *const i8) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` est une chaîne C terminée par nul fournie par l'invité.
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// Retourne une chaîne UTF-16 terminée par nul selon la convention Win32 « taille en caractères » :
/// `cch == 0` → taille requise (nul inclus) ; tampon trop petit → 0 + `ERROR_INSUFFICIENT_BUFFER` ;
/// sinon copie et retourne le nombre de caractères écrits, nul inclus.
pub(crate) fn return_wide(s: &str, buf: *mut u16, cch: i32) -> i32 {
    let units: Vec<u16> = s.encode_utf16().collect();
    let needed = units.len() + 1;
    if cch == 0 {
        return needed as i32;
    }
    if cch < 0 || buf.is_null() {
        sb_loader::set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if (cch as usize) < needed {
        sb_loader::set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    // SAFETY: `buf` dispose d'au moins `cch >= needed` unités UTF-16 en écriture.
    unsafe {
        std::ptr::copy_nonoverlapping(units.as_ptr(), buf, units.len());
        *buf.add(units.len()) = 0;
    }
    needed as i32
}

/// Initialise le TEB du thread de test (nécessaire pour `GetLastError`).
#[cfg(test)]
pub(crate) fn init_test_thread() {
    let _ = sb_loader::init_thread_teb(0);
}

/// Exports `nom -> adresse` fusionnés dans la famille kernel du résolveur.
pub fn exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:literal, $path:path) => {
            m.insert($name, $path as *const () as usize);
        };
    }

    // debug / errorhandling / sysinfo
    reg!("IsDebuggerPresent", sysinfo::IsDebuggerPresent);
    reg!(
        "CheckRemoteDebuggerPresent",
        sysinfo::CheckRemoteDebuggerPresent
    );
    reg!("OutputDebugStringA", sysinfo::OutputDebugStringA);
    reg!("OutputDebugStringW", sysinfo::OutputDebugStringW);
    reg!(
        "UnhandledExceptionFilter",
        sysinfo::UnhandledExceptionFilter
    );
    reg!(
        "IsProcessorFeaturePresent",
        sysinfo::IsProcessorFeaturePresent
    );
    reg!(
        "GetCurrentThreadStackLimits",
        sysinfo::GetCurrentThreadStackLimits
    );
    reg!("GetStartupInfoW", sysinfo::GetStartupInfoW);
    reg!("GetStartupInfoA", sysinfo::GetStartupInfoA);
    reg!("OpenThread", sysinfo::OpenThread);
    reg!("SetProcessPriorityBoost", sysinfo::SetProcessPriorityBoost);
    reg!("GetProcessPriorityBoost", sysinfo::GetProcessPriorityBoost);
    reg!("GetThreadGroupAffinity", sysinfo::GetThreadGroupAffinity);
    reg!("SetThreadGroupAffinity", sysinfo::SetThreadGroupAffinity);
    reg!(
        "GetLogicalProcessorInformation",
        sysinfo::GetLogicalProcessorInformation
    );
    reg!(
        "GetLogicalProcessorInformationEx",
        sysinfo::GetLogicalProcessorInformationEx
    );
    reg!("GlobalMemoryStatusEx", sysinfo::GlobalMemoryStatusEx);
    reg!("GetTickCount64", sysinfo::GetTickCount64);
    reg!("GetTickCount", sysinfo::GetTickCount);

    // FormatMessage & Local*/Global*
    reg!("FormatMessageA", msg::FormatMessageA);
    reg!("FormatMessageW", msg::FormatMessageW);
    reg!("LocalAlloc", msg::LocalAlloc);
    reg!("LocalFree", msg::LocalFree);
    reg!("LocalReAlloc", msg::LocalReAlloc);
    reg!("LocalLock", msg::LocalLock);
    reg!("LocalUnlock", msg::LocalUnlock);
    reg!("LocalSize", msg::LocalSize);
    reg!("GlobalAlloc", msg::LocalAlloc);
    reg!("GlobalFree", msg::LocalFree);
    reg!("GlobalReAlloc", msg::LocalReAlloc);
    reg!("GlobalLock", msg::LocalLock);
    reg!("GlobalUnlock", msg::LocalUnlock);
    reg!("GlobalSize", msg::LocalSize);

    // divers
    reg!("lstrcmpW", misc::lstrcmpW);
    reg!("lstrcmpiW", misc::lstrcmpiW);
    reg!("lstrcmpA", misc::lstrcmpA);
    reg!("lstrcmpiA", misc::lstrcmpiA);
    reg!("lstrlenA", misc::lstrlenA);
    reg!("WerRegisterFile", misc::WerRegisterFile);
    reg!("WerUnregisterFile", misc::WerUnregisterFile);
    reg!(
        "RegisterAppStateChangeNotification",
        misc::RegisterAppStateChangeNotification
    );
    reg!(
        "UnregisterAppStateChangeNotification",
        misc::UnregisterAppStateChangeNotification
    );
    reg!(
        "RegisterAppConstrainedChangeNotification",
        misc::RegisterAppConstrainedChangeNotification
    );
    reg!(
        "UnregisterAppConstrainedChangeNotification",
        misc::UnregisterAppConstrainedChangeNotification
    );
    reg!("SetNamedPipeHandleState", misc::SetNamedPipeHandleState);
    reg!("WaitNamedPipeW", misc::WaitNamedPipeW);
    reg!("WaitNamedPipeA", misc::WaitNamedPipeA);
    reg!("GetModuleFileNameA", misc::GetModuleFileNameA);
    reg!("LoadLibraryExW", misc::LoadLibraryExW);
    reg!("LoadLibraryExA", misc::LoadLibraryExA);

    // mémoire virtuelle
    reg!("VirtualQueryEx", vmem::VirtualQueryEx);
    reg!("VirtualAllocEx", vmem::VirtualAllocEx);
    reg!("VirtualFreeEx", vmem::VirtualFreeEx);

    // fichiers (VFS)
    reg!("GetFileTime", crate::file_io::GetFileTime);
    reg!("SetFileTime", crate::file_io::SetFileTime);
    reg!("SetFileAttributesW", crate::file_io::SetFileAttributesW);
    reg!("RemoveDirectoryW", crate::file_io::RemoveDirectoryW);
    reg!("GetDiskFreeSpaceExW", crate::file_io::GetDiskFreeSpaceExW);

    // rtlsupport / unwind x64
    reg!("RtlCaptureContext", rtl::RtlCaptureContext);
    reg!("RtlLookupFunctionEntry", rtl::RtlLookupFunctionEntry);
    reg!("RtlVirtualUnwind", rtl::RtlVirtualUnwind);
    reg!("RtlPcToFileHeader", rtl::RtlPcToFileHeader);
    reg!("RtlAddFunctionTable", rtl::RtlAddFunctionTable);
    reg!("RtlDeleteFunctionTable", rtl::RtlDeleteFunctionTable);

    // bcrypt
    reg!(
        "BCryptOpenAlgorithmProvider",
        bcrypt::BCryptOpenAlgorithmProvider
    );
    reg!(
        "BCryptCloseAlgorithmProvider",
        bcrypt::BCryptCloseAlgorithmProvider
    );
    reg!("BCryptGenRandom", bcrypt::BCryptGenRandom);
    reg!("SystemFunction036", bcrypt::SystemFunction036);

    // COM
    reg!("CoInitializeEx", com_ext::CoInitializeEx);
    reg!("CoInitialize", com_ext::CoInitialize);
    reg!("CoUninitialize", com_ext::CoUninitialize);
    reg!("CoCreateGuid", com_ext::CoCreateGuid);
    reg!("CoCreateInstance", com_ext::CoCreateInstance);
    reg!("CoGetClassObject", com_ext::CoGetClassObject);
    reg!("CoTaskMemRealloc", com_ext::CoTaskMemRealloc);
    reg!("PropVariantClear", com_ext::PropVariantClear);
    reg!("StringFromGUID2", com_ext::StringFromGUID2);
    reg!("StringFromCLSID", com_ext::StringFromCLSID);
    reg!("StringFromIID", com_ext::StringFromCLSID);
    reg!("CLSIDFromString", com_ext::CLSIDFromString);
    reg!("IIDFromString", com_ext::IIDFromString);

    // user32 / ntuser : fenêtres logiques & messages
    reg!("RegisterClassExW", user::RegisterClassExW);
    reg!("RegisterClassExA", user::RegisterClassExA);
    reg!("RegisterClassW", user::RegisterClassW);
    reg!("RegisterClassA", user::RegisterClassA);
    reg!("UnregisterClassW", user::UnregisterClassW);
    reg!("CreateWindowExW", user::CreateWindowExW);
    reg!("CreateWindowExA", user::CreateWindowExA);
    reg!("DestroyWindow", user::DestroyWindow);
    reg!("IsWindow", user::IsWindow);
    reg!("ShowWindow", user::ShowWindow);
    reg!("UpdateWindow", user::UpdateWindow);
    reg!("DefWindowProcW", user::DefWindowProcW);
    reg!("DefWindowProcA", user::DefWindowProcA);
    reg!("SendMessageW", user::SendMessageW);
    reg!("SendMessageA", user::SendMessageW);
    reg!("PostMessageW", user::PostMessageW);
    reg!("PostMessageA", user::PostMessageW);
    reg!("PostThreadMessageW", user::PostThreadMessageW);
    reg!("PostThreadMessageA", user::PostThreadMessageW);
    reg!("PostQuitMessage", user::PostQuitMessage);
    reg!("PeekMessageW", user::PeekMessageW);
    reg!("PeekMessageA", user::PeekMessageW);
    reg!("GetMessageW", user::GetMessageW);
    reg!("GetMessageA", user::GetMessageW);
    reg!("TranslateMessage", user::TranslateMessage);
    reg!("DispatchMessageW", user::DispatchMessageW);
    reg!("DispatchMessageA", user::DispatchMessageW);
    reg!("GetClientRect", user::GetClientRect);
    reg!("GetWindowLongPtrW", user::GetWindowLongPtrW);
    reg!("GetWindowLongPtrA", user::GetWindowLongPtrW);
    reg!("SetWindowLongPtrW", user::SetWindowLongPtrW);
    reg!("SetWindowLongPtrA", user::SetWindowLongPtrW);

    // temps & paramètres régionaux
    reg!("GetSystemTime", time::GetSystemTime);
    reg!("GetLocalTime", time::GetLocalTime);
    reg!("FileTimeToSystemTime", time::FileTimeToSystemTime);
    reg!("SystemTimeToFileTime", time::SystemTimeToFileTime);
    reg!("FileTimeToLocalFileTime", time::FileTimeToLocalFileTime);
    reg!("LocalFileTimeToFileTime", time::LocalFileTimeToFileTime);
    reg!("GetDateFormatEx", time::GetDateFormatEx);
    reg!("GetDateFormatW", time::GetDateFormatW);
    reg!("GetTimeFormatEx", time::GetTimeFormatEx);
    reg!("GetTimeFormatW", time::GetTimeFormatW);
    reg!("GetUserDefaultLocaleName", time::GetUserDefaultLocaleName);
    reg!(
        "GetSystemDefaultLocaleName",
        time::GetSystemDefaultLocaleName
    );
    reg!("GetUserDefaultGeoName", time::GetUserDefaultGeoName);
    reg!("GetUserGeoID", time::GetUserGeoID);
    reg!("GetUserDefaultLCID", time::GetUserDefaultLCID);
    reg!("GetSystemDefaultLCID", time::GetSystemDefaultLCID);
    reg!("GetUserDefaultLangID", time::GetUserDefaultLangID);
    reg!("GetUserDefaultUILanguage", time::GetUserDefaultUILanguage);
    reg!(
        "GetSystemDefaultUILanguage",
        time::GetSystemDefaultUILanguage
    );
    reg!("GetThreadLocale", time::GetThreadLocale);
    reg!("LocaleNameToLCID", time::LocaleNameToLCID);
    reg!("LCIDToLocaleName", time::LCIDToLocaleName);

    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_do_not_shadow_kernel32() {
        let k = crate::kernel32::exports();
        for name in exports().keys() {
            assert!(!k.contains_key(name), "{name} déjà exporté par kernel32.rs");
        }
    }
}
