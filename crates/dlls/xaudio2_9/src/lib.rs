//! Façade ScarletBridge pour `XAudio2_9.dll` (Phase 0 stub vérifié → Phase 4 FAudio/PipeWire).
//!
//! Sur Xbox/UWP, les titres importent fréquemment `XAudio2Create` par ordinal (`@1`).

#![allow(non_snake_case)]

use core::ffi::c_void;
use std::ptr;

#[cfg(target_os = "windows")]
use sb_common::abi::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH, WIN_TRUE, WinBool};
use sb_common::abi::{E_NOTIMPL, E_POINTER, E_UNEXPECTED, HResult};
#[cfg(target_os = "windows")]
use sb_common::sb_info;
use sb_common::{ffi_guard, sb_stub};

const CHANNEL: &str = "xaudio2_9";

#[cfg(target_os = "windows")]
#[unsafe(no_mangle)]
pub extern "win64" fn DllMain(
    _instance: *mut c_void,
    reason: u32,
    _reserved: *mut c_void,
) -> WinBool {
    ffi_guard!(CHANNEL, "DllMain", WIN_TRUE, {
        match reason {
            DLL_PROCESS_ATTACH => {
                sb_common::log::init();
                sb_info!(CHANNEL, "XAudio2_9.dll ScarletBridge chargée");
            }
            DLL_PROCESS_DETACH => {
                sb_info!(CHANNEL, "XAudio2_9.dll ScarletBridge déchargée");
            }
            _ => {}
        }
        WIN_TRUE
    })
}

/// Point d'entrée `XAudio2Create` (exporté par nom et à l'ordinal `@1`).
///
/// # Safety
/// `pp_xaudio2` doit être un pointeur valide vers un `*mut c_void` (ou nul, auquel cas `E_POINTER` est renvoyé).
#[unsafe(no_mangle)]
pub unsafe extern "win64" fn XAudio2Create(
    pp_xaudio2: *mut *mut c_void,
    flags: u32,
    xaudio2_processor: u32,
) -> HResult {
    ffi_guard!(CHANNEL, "XAudio2Create", E_UNEXPECTED, {
        sb_stub!(
            CHANNEL,
            "XAudio2Create",
            "pp_xaudio2={pp_xaudio2:p}, flags={flags:#x}, processor={xaudio2_processor:#x}"
        );
        if pp_xaudio2.is_null() {
            return E_POINTER;
        }
        // SAFETY: `pp_xaudio2` est non-nul et l'appelant attend son initialisation en sortie.
        unsafe {
            *pp_xaudio2 = ptr::null_mut();
        }
        E_NOTIMPL
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xaudio2_create_validates_null_and_clears_out_ptr() {
        // SAFETY: pointeur nul géré explicitement.
        assert_eq!(unsafe { XAudio2Create(ptr::null_mut(), 0, 1) }, E_POINTER);

        let mut out: *mut c_void = 0x1234 as *mut c_void;
        // SAFETY: `&mut out` est valide en écriture.
        let hr = unsafe { XAudio2Create(&mut out, 0, 1) };
        assert_eq!(hr, E_NOTIMPL);
        assert!(out.is_null());
    }
}
