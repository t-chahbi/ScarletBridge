//! Crate stub générée automatiquement par `sb-stubgen` pour `vccorlib140_app.DLL`.
//! Remplacez progressivement chaque stub par un shim vérifié (voir LLM.md).

#![allow(non_snake_case, unused_variables)]

#[cfg(target_os = "windows")]
use core::ffi::c_void;
use sb_common::abi::HResult;
#[cfg(target_os = "windows")]
use sb_common::abi::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH, WIN_TRUE, WinBool};
#[cfg(target_os = "windows")]
use sb_common::sb_info;
use sb_common::{ffi_guard, sb_stub};

const CHANNEL: &str = "vccorlib140_app";

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
                sb_info!(CHANNEL, "DLL stub chargée : vccorlib140_app.DLL");
            }
            DLL_PROCESS_DETACH => {
                sb_info!(CHANNEL, "DLL stub déchargée : vccorlib140_app.DLL");
            }
            _ => {}
        }
        WIN_TRUE
    })
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "??0Delegate@Platform@@QE$AAA@XZ")
)]
pub extern "win64" fn stub_0_0Delegate_Platform__QE_AAA_XZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "??0Delegate@Platform@@QE$AAA@XZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "??0Delegate@Platform@@QE$AAA@XZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "??0DisconnectedException@Platform@@QE$AAA@XZ")
)]
pub extern "win64" fn stub_1_0DisconnectedException_Platform__QE_AAA_XZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "??0DisconnectedException@Platform@@QE$AAA@XZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "??0DisconnectedException@Platform@@QE$AAA@XZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "??0Object@Platform@@QE$AAA@XZ")
)]
pub extern "win64" fn stub_2_0Object_Platform__QE_AAA_XZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "??0Object@Platform@@QE$AAA@XZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "??0Object@Platform@@QE$AAA@XZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?AlignedFree@Heap@Details@Platform@@SAXPEAX@Z")
)]
pub extern "win64" fn stub_3_AlignedFree_Heap_Details_Platform__SAXPEAX_Z(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?AlignedFree@Heap@Details@Platform@@SAXPEAX@Z",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?AlignedFree@Heap@Details@Platform@@SAXPEAX@Z",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?Allocate@Heap@Details@Platform@@SAPEAX_K0@Z")
)]
pub extern "win64" fn stub_4_Allocate_Heap_Details_Platform__SAPEAX_K0_Z(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?Allocate@Heap@Details@Platform@@SAPEAX_K0@Z",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?Allocate@Heap@Details@Platform@@SAPEAX_K0@Z",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?AllocateException@Heap@Details@Platform@@SAPEAX_K0@Z")
)]
pub extern "win64" fn stub_5_AllocateException_Heap_Details_Platform__SAPEAX_K0_Z(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?AllocateException@Heap@Details@Platform@@SAPEAX_K0@Z",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?AllocateException@Heap@Details@Platform@@SAPEAX_K0@Z",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?Free@Heap@Details@Platform@@SAXPEAX@Z")
)]
pub extern "win64" fn stub_6_Free_Heap_Details_Platform__SAXPEAX_Z(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?Free@Heap@Details@Platform@@SAXPEAX@Z",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?Free@Heap@Details@Platform@@SAXPEAX@Z",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?GetActivationFactoryByPCWSTR@@YAJPEAXAEAVGuid@Platform@@PEAPEAX@Z")
)]
pub extern "win64" fn stub_7_GetActivationFactoryByPCWSTR__YAJPEAXAEAVGuid_Platform__PEAPEAX_Z(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?GetActivationFactoryByPCWSTR@@YAJPEAXAEAVGuid@Platform@@PEAPEAX@Z",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?GetActivationFactoryByPCWSTR@@YAJPEAXAEAVGuid@Platform@@PEAPEAX@Z",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?GetCmdArguments@Details@Platform@@YAPEAPEA_WPEAH@Z")
)]
pub extern "win64" fn stub_8_GetCmdArguments_Details_Platform__YAPEAPEA_WPEAH_Z(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?GetCmdArguments@Details@Platform@@YAPEAPEA_WPEAH@Z",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?GetCmdArguments@Details@Platform@@YAPEAPEA_WPEAH@Z",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?GetIBoxArrayVtable@Details@Platform@@YAPEAXPEAX@Z")
)]
pub extern "win64" fn stub_9_GetIBoxArrayVtable_Details_Platform__YAPEAXPEAX_Z(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?GetIBoxArrayVtable@Details@Platform@@YAPEAXPEAX@Z",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?GetIBoxArrayVtable@Details@Platform@@YAPEAXPEAX@Z",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?GetIidsFn@@YAJHPEAKPEBU__s_GUID@@PEAPEAVGuid@Platform@@@Z")
)]
pub extern "win64" fn stub_10_GetIidsFn__YAJHPEAKPEBU__s_GUID__PEAPEAVGuid_Platform___Z(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?GetIidsFn@@YAJHPEAKPEBU__s_GUID@@PEAPEAVGuid@Platform@@@Z",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?GetIidsFn@@YAJHPEAKPEBU__s_GUID@@PEAPEAVGuid@Platform@@@Z",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?InitializeData@Details@Platform@@YAJH@Z")
)]
pub extern "win64" fn stub_11_InitializeData_Details_Platform__YAJH_Z(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?InitializeData@Details@Platform@@YAJH@Z",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?InitializeData@Details@Platform@@YAJH@Z",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?ReleaseTarget@ControlBlock@Details@Platform@@AEAAXXZ")
)]
pub extern "win64" fn stub_12_ReleaseTarget_ControlBlock_Details_Platform__AEAAXXZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?ReleaseTarget@ControlBlock@Details@Platform@@AEAAXXZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?ReleaseTarget@ControlBlock@Details@Platform@@AEAAXXZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?UninitializeData@Details@Platform@@YAXH@Z")
)]
pub extern "win64" fn stub_13_UninitializeData_Details_Platform__YAXH_Z(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?UninitializeData@Details@Platform@@YAXH@Z",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?UninitializeData@Details@Platform@@YAXH@Z",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?__abi_WinRTraiseAccessDeniedException@@YAXXZ")
)]
pub extern "win64" fn stub_14_abi_WinRTraiseAccessDeniedException__YAXXZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?__abi_WinRTraiseAccessDeniedException@@YAXXZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?__abi_WinRTraiseAccessDeniedException@@YAXXZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?__abi_WinRTraiseCOMException@@YAXJ@Z")
)]
pub extern "win64" fn stub_15_abi_WinRTraiseCOMException__YAXJ_Z(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?__abi_WinRTraiseCOMException@@YAXJ@Z",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?__abi_WinRTraiseCOMException@@YAXJ@Z",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?__abi_WinRTraiseChangedStateException@@YAXXZ")
)]
pub extern "win64" fn stub_16_abi_WinRTraiseChangedStateException__YAXXZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?__abi_WinRTraiseChangedStateException@@YAXXZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?__abi_WinRTraiseChangedStateException@@YAXXZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?__abi_WinRTraiseClassNotRegisteredException@@YAXXZ")
)]
pub extern "win64" fn stub_17_abi_WinRTraiseClassNotRegisteredException__YAXXZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?__abi_WinRTraiseClassNotRegisteredException@@YAXXZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?__abi_WinRTraiseClassNotRegisteredException@@YAXXZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?__abi_WinRTraiseDisconnectedException@@YAXXZ")
)]
pub extern "win64" fn stub_18_abi_WinRTraiseDisconnectedException__YAXXZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?__abi_WinRTraiseDisconnectedException@@YAXXZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?__abi_WinRTraiseDisconnectedException@@YAXXZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?__abi_WinRTraiseFailureException@@YAXXZ")
)]
pub extern "win64" fn stub_19_abi_WinRTraiseFailureException__YAXXZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?__abi_WinRTraiseFailureException@@YAXXZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?__abi_WinRTraiseFailureException@@YAXXZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?__abi_WinRTraiseInvalidArgumentException@@YAXXZ")
)]
pub extern "win64" fn stub_20_abi_WinRTraiseInvalidArgumentException__YAXXZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?__abi_WinRTraiseInvalidArgumentException@@YAXXZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?__abi_WinRTraiseInvalidArgumentException@@YAXXZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?__abi_WinRTraiseInvalidCastException@@YAXXZ")
)]
pub extern "win64" fn stub_21_abi_WinRTraiseInvalidCastException__YAXXZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?__abi_WinRTraiseInvalidCastException@@YAXXZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?__abi_WinRTraiseInvalidCastException@@YAXXZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?__abi_WinRTraiseNotImplementedException@@YAXXZ")
)]
pub extern "win64" fn stub_22_abi_WinRTraiseNotImplementedException__YAXXZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?__abi_WinRTraiseNotImplementedException@@YAXXZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?__abi_WinRTraiseNotImplementedException@@YAXXZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?__abi_WinRTraiseNullReferenceException@@YAXXZ")
)]
pub extern "win64" fn stub_23_abi_WinRTraiseNullReferenceException__YAXXZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?__abi_WinRTraiseNullReferenceException@@YAXXZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?__abi_WinRTraiseNullReferenceException@@YAXXZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?__abi_WinRTraiseObjectDisposedException@@YAXXZ")
)]
pub extern "win64" fn stub_24_abi_WinRTraiseObjectDisposedException__YAXXZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?__abi_WinRTraiseObjectDisposedException@@YAXXZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?__abi_WinRTraiseObjectDisposedException@@YAXXZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?__abi_WinRTraiseOperationCanceledException@@YAXXZ")
)]
pub extern "win64" fn stub_25_abi_WinRTraiseOperationCanceledException__YAXXZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?__abi_WinRTraiseOperationCanceledException@@YAXXZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?__abi_WinRTraiseOperationCanceledException@@YAXXZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?__abi_WinRTraiseOutOfBoundsException@@YAXXZ")
)]
pub extern "win64" fn stub_26_abi_WinRTraiseOutOfBoundsException__YAXXZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?__abi_WinRTraiseOutOfBoundsException@@YAXXZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?__abi_WinRTraiseOutOfBoundsException@@YAXXZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?__abi_WinRTraiseOutOfMemoryException@@YAXXZ")
)]
pub extern "win64" fn stub_27_abi_WinRTraiseOutOfMemoryException__YAXXZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?__abi_WinRTraiseOutOfMemoryException@@YAXXZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?__abi_WinRTraiseOutOfMemoryException@@YAXXZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?__abi_WinRTraiseWrongThreadException@@YAXXZ")
)]
pub extern "win64" fn stub_28_abi_WinRTraiseWrongThreadException__YAXXZ(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?__abi_WinRTraiseWrongThreadException@@YAXXZ",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?__abi_WinRTraiseWrongThreadException@@YAXXZ",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}

#[cfg_attr(
    target_os = "windows",
    unsafe(export_name = "?__abi_translateCurrentException@@YAJ_N@Z")
)]
pub extern "win64" fn stub_29_abi_translateCurrentException__YAJ_N_Z(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "?__abi_translateCurrentException@@YAJ_N@Z",
        sb_common::abi::E_UNEXPECTED,
        {
            sb_stub!(
                CHANNEL,
                "?__abi_translateCurrentException@@YAJ_N@Z",
                "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
            );
            sb_common::abi::E_NOTIMPL
        }
    )
}
