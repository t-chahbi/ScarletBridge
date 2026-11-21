//! Crate stub générée automatiquement par `sb-stubgen` pour `VCRUNTIME140_APP.dll`.
//! Remplacez progressivement chaque stub par un shim vérifié (voir LLM.md).

#![allow(non_snake_case, unused_variables)]

#[cfg(target_os = "windows")]
use core::ffi::c_void;
#[cfg(target_os = "windows")]
use sb_common::abi::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH, WIN_TRUE, WinBool};
#[cfg(target_os = "windows")]
use sb_common::ffi_guard;
#[cfg(target_os = "windows")]
use sb_common::sb_info;

#[cfg(target_os = "windows")]
const CHANNEL: &str = "vcruntime140_app";

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
                sb_info!(CHANNEL, "DLL stub chargée : VCRUNTIME140_APP.dll");
            }
            DLL_PROCESS_DETACH => {
                sb_info!(CHANNEL, "DLL stub déchargée : VCRUNTIME140_APP.dll");
            }
            _ => {}
        }
        WIN_TRUE
    })
}
