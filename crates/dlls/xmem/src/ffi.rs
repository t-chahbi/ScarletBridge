//! Frontière ABI Windows x86-64 (`extern "win64"`) pour `xmem.dll`.

use core::ffi::c_void;
use std::ptr;

#[cfg(target_os = "windows")]
use sb_common::abi::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH, WIN_TRUE, WinBool};
#[cfg(target_os = "windows")]
use sb_common::sb_info;
use sb_common::{ffi_guard, sb_fixme, sb_trace, sb_warn};

use crate::allocator;
use crate::attributes::XMemAttributes;

const CHANNEL: &str = "xmem";

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
                sb_info!(CHANNEL, "xmem.dll ScarletBridge chargée");
            }
            DLL_PROCESS_DETACH => {
                let stats = allocator::global().stats();
                sb_info!(
                    CHANNEL,
                    "xmem.dll déchargée (allocations restantes: {}, {} octets)",
                    stats.live_allocations,
                    stats.total_allocated_bytes
                );
            }
            _ => {}
        }
        WIN_TRUE
    })
}

#[unsafe(no_mangle)]
pub extern "win64" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
    XMemAllocDefault(size, attributes)
}

#[unsafe(no_mangle)]
pub extern "win64" fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
    ffi_guard!(CHANNEL, "XMemAllocDefault", ptr::null_mut(), {
        let decoded = XMemAttributes::decode(attributes);
        let ptr = allocator::global()
            .alloc(size, decoded)
            .map(|p| p.cast::<c_void>())
            .unwrap_or_else(ptr::null_mut);

        sb_trace!(
            CHANNEL,
            "XMemAlloc(size={size}, align={}, type={:?}, zero={}) -> {:p}",
            decoded.alignment,
            decoded.memory_type,
            decoded.zero_initialize,
            ptr
        );
        ptr
    })
}

#[unsafe(no_mangle)]
pub extern "win64" fn XMemFree(address: *mut c_void, attributes: u64) {
    XMemFreeDefault(address, attributes);
}

#[unsafe(no_mangle)]
pub extern "win64" fn XMemFreeDefault(address: *mut c_void, attributes: u64) {
    ffi_guard!(CHANNEL, "XMemFreeDefault", (), {
        sb_trace!(
            CHANNEL,
            "XMemFree(address={:p}, attrs={attributes:#x})",
            address
        );
        // SAFETY: contrat ABI `XMemFree` — l'appelant cède la propriété du bloc mémoire.
        if !unsafe { allocator::global().free(address.cast::<u8>()) } {
            sb_warn!(
                CHANNEL,
                "XMemFree appelé sur une adresse inconnue ou déjà libérée : {:p}",
                address
            );
        }
    });
}

#[unsafe(no_mangle)]
pub extern "win64" fn XMemSize(address: *const c_void, attributes: u64) -> usize {
    XMemSizeDefault(address, attributes)
}

#[unsafe(no_mangle)]
pub extern "win64" fn XMemSizeDefault(address: *const c_void, _attributes: u64) -> usize {
    ffi_guard!(CHANNEL, "XMemSizeDefault", 0, {
        let size = allocator::global().size_of(address.cast::<u8>());
        sb_trace!(CHANNEL, "XMemSize(address={:p}) -> {size}", address);
        size
    })
}

#[unsafe(no_mangle)]
pub extern "win64" fn XMemSetAllocationHooks(alloc_hook: *const c_void, free_hook: *const c_void) {
    ffi_guard!(CHANNEL, "XMemSetAllocationHooks", (), {
        sb_fixme!(
            CHANNEL,
            "XMemSetAllocationHooks(alloc={:p}, free={:p}) ignoré en Phase 0",
            alloc_hook,
            free_hook
        );
    });
}
