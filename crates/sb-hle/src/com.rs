//! Infrastructure COM / WinRT partagée (`IUnknown`, `IInspectable`, GUIDs standards, vtables Win64).

use core::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

use sb_common::abi::{E_NOINTERFACE, E_POINTER, HResult, S_OK};
use sb_common::sb_debug;

/// GUID Windows / COM / WinRT 128 bits (`#[repr(C)]`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    pub const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

impl std::fmt::Display for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            self.data1,
            self.data2,
            self.data3,
            self.data4[0],
            self.data4[1],
            self.data4[2],
            self.data4[3],
            self.data4[4],
            self.data4[5],
            self.data4[6],
            self.data4[7],
        )
    }
}

pub(crate) fn format_guid_opt(iid: *const Guid) -> String {
    if iid.is_null() {
        "<null>".to_string()
    } else {
        // SAFETY: `iid` vérifié non-nul.
        format!("{{{}}}", unsafe { *iid })
    }
}

// GUIDs COM / WinRT / DXGI / D3D11 / D3D12 / XAudio2 de référence.
pub const IID_IUNKNOWN: Guid = Guid::new(
    0x0000_0000,
    0x0000,
    0x0000,
    [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
);
pub const IID_IINSPECTABLE: Guid = Guid::new(
    0xAF86_E2E0,
    0xB12D,
    0x4C6A,
    [0x9C, 0x5A, 0xD7, 0xAA, 0x65, 0x10, 0x1E, 0x90],
);
pub const IID_IAGILEOBJECT: Guid = Guid::new(
    0x94EA_2B94,
    0xE9CC,
    0x49E0,
    [0xC0, 0xFF, 0xEE, 0x64, 0xCA, 0x8F, 0x5B, 0x90],
);

// WinRT CoreApplication / CoreWindow / ApplicationData / Gamepad / SystemNavigationManager
pub const IID_ICOREAPPLICATION: Guid = Guid::new(
    0x0AAC_F7A4,
    0x5E1D,
    0x49DF,
    [0x80, 0x34, 0xFB, 0x6A, 0x68, 0xBC, 0x5E, 0xD1],
);
pub const IID_ICOREAPPLICATION_EXIT: Guid = Guid::new(
    0xCF86_461D,
    0x261E,
    0x4B72,
    [0x9A, 0xCD, 0x44, 0xED, 0x2A, 0xCE, 0x6A, 0x29],
);
pub const IID_ICOREAPPLICATION_VIEW: Guid = Guid::new(
    0x638B_B2DB,
    0x451D,
    0x4661,
    [0xB0, 0x99, 0x41, 0x4F, 0x34, 0xFF, 0xB9, 0xF1],
);
pub const IID_ICOREWINDOW: Guid = Guid::new(
    0x79B9_D5F2,
    0x879E,
    0x4B89,
    [0xB7, 0x98, 0x79, 0xED, 0x47, 0x57, 0xBC, 0x47],
);
pub const IID_ICOREDISPATCHER: Guid = Guid::new(
    0x60DB_2FA8,
    0xB705,
    0x4FDE,
    [0xA7, 0xD6, 0xEB, 0xBB, 0x18, 0x91, 0xD3, 0x9E],
);
pub const IID_ISYSTEM_NAV_STATICS: Guid = Guid::new(
    0xDC52_B5CE,
    0xBEE0,
    0x4305,
    [0x8C, 0x54, 0x68, 0x22, 0x8E, 0xD6, 0x83, 0xB5],
);
pub const IID_IAPPLICATION_DATA_STATICS: Guid = Guid::new(
    0x5612_147B,
    0xE843,
    0x45E3,
    [0x94, 0xD8, 0x06, 0x16, 0x9E, 0x3C, 0x8E, 0x17],
);
pub const IID_IGLOBALIZATION_PREFS_STATICS: Guid = Guid::new(
    0x01BF_4326,
    0xED37,
    0x4E96,
    [0xB0, 0xE9, 0xC1, 0x34, 0x0D, 0x1E, 0xA1, 0x58],
);
pub const IID_IGAMEPAD_STATICS: Guid = Guid::new(
    0x8BBC_E529,
    0xD49C,
    0x39E9,
    [0x95, 0x60, 0xE4, 0x7D, 0xDE, 0x96, 0xB7, 0xC8],
);

// DXGI / D3D11 / D3D12
pub const IID_IDXGIDEVICE: Guid = Guid::new(
    0x54EC_77FA,
    0x1377,
    0x44E6,
    [0x8C, 0x32, 0x88, 0xFD, 0x5F, 0x44, 0xC8, 0x4C],
);
pub const IID_IDXGIDEVICE1: Guid = Guid::new(
    0x77DB_970F,
    0x6276,
    0x48BA,
    [0xBA, 0x28, 0x07, 0x01, 0x43, 0xB4, 0x39, 0x2C],
);
pub const IID_IDXGIDEVICE2: Guid = Guid::new(
    0x0500_8617,
    0xFBFD,
    0x4051,
    [0xA7, 0x90, 0x14, 0x48, 0x84, 0xB4, 0xF6, 0xA9],
);
pub const IID_IDXGIDEVICE3: Guid = Guid::new(
    0x6007_896C,
    0x3244,
    0x4AFD,
    [0xBF, 0x18, 0xA6, 0xD3, 0xBE, 0xDA, 0x50, 0x23],
);
pub const IID_IDXGIADAPTER: Guid = Guid::new(
    0x2411_E7E1,
    0x12AC,
    0x4CCF,
    [0xBD, 0x14, 0x97, 0x98, 0xE8, 0x53, 0x4D, 0xC0],
);
pub const IID_IDXGIFACTORY2: Guid = Guid::new(
    0x50C8_3A1C,
    0xE072,
    0x4C48,
    [0x87, 0xB0, 0x36, 0x30, 0xFA, 0x36, 0xA6, 0xD0],
);
pub const IID_ID3D11TEXTURE2D: Guid = Guid::new(
    0x6F15_AAF2,
    0xD208,
    0x4E89,
    [0x9A, 0xB4, 0x48, 0x95, 0x35, 0xD3, 0x4F, 0x9C],
);

/// Slot de vtable COM (pointeur de fonction opaque compatible `static` / `const`).
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct VtblSlot(pub *const c_void);

// SAFETY: les pointeurs vers des fonctions statiques `extern "win64"` sont immuables et thread-safe.
unsafe impl Sync for VtblSlot {}
unsafe impl Send for VtblSlot {}

#[inline]
pub const fn vslot(ptr: *const c_void) -> VtblSlot {
    VtblSlot(ptr)
}

/// En-tête commun de tout objet COM / WinRT alloué sur le tas par ScarletBridge.
#[repr(C)]
pub struct ComHeader {
    pub vtbl: *const c_void,
    pub ref_count: AtomicU32,
}

impl ComHeader {
    pub const fn new(vtbl: *const c_void) -> Self {
        Self {
            vtbl,
            ref_count: AtomicU32::new(1),
        }
    }

    pub fn add_ref(&self) -> u32 {
        self.ref_count.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn release(&self) -> u32 {
        let prev = self.ref_count.load(Ordering::Acquire);
        if prev > 1 {
            self.ref_count.fetch_sub(1, Ordering::AcqRel) - 1
        } else {
            // Les singletons HLE restent vivants pour la durée du processus afin d'éviter
            // tout use-after-free pendant les teardowns C++/CX partiels.
            1
        }
    }
}

/// Implémentation générique de `IUnknown::AddRef`.
pub(crate) extern "win64" fn com_add_ref(this: *mut ComHeader) -> u32 {
    if this.is_null() {
        return 0;
    }
    // SAFETY: `this` commence par `ComHeader`.
    unsafe { (*this).add_ref() }
}

/// Implémentation générique de `IUnknown::Release`.
pub(crate) extern "win64" fn com_release(this: *mut ComHeader) -> u32 {
    if this.is_null() {
        return 0;
    }
    // SAFETY: `this` commence par `ComHeader`.
    unsafe { (*this).release() }
}

/// Helper `QueryInterface` acceptant `IUnknown`, `IInspectable`, `IAgileObject` et une liste d'IID.
pub(crate) fn com_query_interface_list(
    name: &str,
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
    supported: &[Guid],
) -> HResult {
    if ppv.is_null() {
        return E_POINTER;
    }
    if this.is_null() || riid.is_null() {
        // SAFETY: `ppv` vérifié non-nul.
        unsafe {
            *ppv = ptr::null_mut();
        }
        return E_NOINTERFACE;
    }

    // SAFETY: `riid` vérifié non-nul.
    let wanted = unsafe { *riid };
    if wanted == IID_IUNKNOWN
        || wanted == IID_IINSPECTABLE
        || wanted == IID_IAGILEOBJECT
        || supported.contains(&wanted)
    {
        // SAFETY: `this` et `ppv` sont valides.
        unsafe {
            (*this).add_ref();
            *ppv = this.cast::<c_void>();
        }
        return S_OK;
    }

    sb_debug!(
        "com",
        "{name}::QueryInterface({{{wanted}}}) -> E_NOINTERFACE"
    );
    // SAFETY: `ppv` vérifié non-nul.
    unsafe {
        *ppv = ptr::null_mut();
    }
    E_NOINTERFACE
}

/// Implémentation générique `IInspectable::GetIids`.
pub(crate) extern "win64" fn com_get_iids(
    _this: *mut ComHeader,
    out_count: *mut u32,
    out_iids: *mut *mut Guid,
) -> HResult {
    if !out_count.is_null() {
        // SAFETY: `out_count` vérifié non-nul.
        unsafe {
            *out_count = 0;
        }
    }
    if !out_iids.is_null() {
        // SAFETY: `out_iids` vérifié non-nul.
        unsafe {
            *out_iids = ptr::null_mut();
        }
    }
    S_OK
}

/// Implémentation générique `IInspectable::GetRuntimeClassName`.
pub(crate) extern "win64" fn com_get_runtime_class_name(
    _this: *mut ComHeader,
    out_name: *mut u64,
) -> HResult {
    if !out_name.is_null() {
        // SAFETY: `out_name` vérifié non-nul.
        unsafe {
            *out_name = 0;
        }
    }
    S_OK
}

/// Implémentation générique `IInspectable::GetTrustLevel`.
pub(crate) extern "win64" fn com_get_trust_level(
    _this: *mut ComHeader,
    out_level: *mut i32,
) -> HResult {
    if !out_level.is_null() {
        // SAFETY: `out_level` vérifié non-nul (`BaseTrust = 0`).
        unsafe {
            *out_level = 0;
        }
    }
    S_OK
}
