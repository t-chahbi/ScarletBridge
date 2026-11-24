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

// ---------------------------------------------------------------------------------------------
// Description d'arguments invités pour les journaux de diagnostic
// ---------------------------------------------------------------------------------------------

/// IID d'interfaces Direct3D 12 / DXGI / DirectStorage courantes (en-têtes publics), pour nommer
/// dans un journal l'interface demandée par un titre.
const KNOWN_IIDS: &[(Guid, &str)] = &[
    (
        Guid::new(
            0x1898_19F1,
            0x1DB6,
            0x4B57,
            [0xBE, 0x54, 0x18, 0x21, 0x33, 0x9B, 0x85, 0xF7],
        ),
        "ID3D12Device",
    ),
    (
        Guid::new(
            0x0EC8_70A6,
            0x5D7E,
            0x4C22,
            [0x8C, 0xFC, 0x5B, 0xAA, 0xE0, 0x76, 0x16, 0xED],
        ),
        "ID3D12CommandQueue",
    ),
    (
        Guid::new(
            0x6102_DEE4,
            0xAF59,
            0x4B09,
            [0xB9, 0x99, 0xB4, 0x4D, 0x73, 0xF0, 0x9B, 0x24],
        ),
        "ID3D12CommandAllocator",
    ),
    (
        Guid::new(
            0x5B16_0D0F,
            0xAC1B,
            0x4185,
            [0x8B, 0xA8, 0xB3, 0xAE, 0x42, 0xA5, 0xA4, 0x55],
        ),
        "ID3D12GraphicsCommandList",
    ),
    (
        Guid::new(
            0x0A75_3DCF,
            0xC4D8,
            0x4B91,
            [0xAD, 0xF6, 0xBE, 0x5A, 0x60, 0xD9, 0x5A, 0x76],
        ),
        "ID3D12Fence",
    ),
    (
        Guid::new(
            0x8EFB_471D,
            0x616C,
            0x4F49,
            [0x90, 0xF7, 0x12, 0x7B, 0xB7, 0x63, 0xFA, 0x51],
        ),
        "ID3D12DescriptorHeap",
    ),
    (
        Guid::new(
            0x6B3B_2502,
            0x6E51,
            0x45B3,
            [0x90, 0xEE, 0x98, 0x84, 0x26, 0x5E, 0x8D, 0xF3],
        ),
        "ID3D12Heap",
    ),
    (
        Guid::new(
            0x6964_42BE,
            0xA72E,
            0x4059,
            [0xBC, 0x79, 0x5B, 0x5C, 0x98, 0x04, 0x0F, 0xAD],
        ),
        "ID3D12Resource",
    ),
    (
        Guid::new(
            0xC54A_6B66,
            0x72DF,
            0x4EE8,
            [0x8B, 0xE5, 0xA9, 0x46, 0xA1, 0x42, 0x92, 0x14],
        ),
        "ID3D12RootSignature",
    ),
    (
        Guid::new(
            0x765A_30F3,
            0xF624,
            0x4C6F,
            [0xA8, 0x28, 0xAC, 0xE9, 0x48, 0x62, 0x24, 0x45],
        ),
        "ID3D12PipelineState",
    ),
    (
        Guid::new(
            0x0D96_58AE,
            0xED45,
            0x469E,
            [0xA6, 0x1D, 0x97, 0x0E, 0xC5, 0x83, 0xCA, 0xE0],
        ),
        "ID3D12QueryHeap",
    ),
    (
        Guid::new(
            0xC36A_797C,
            0xEC80,
            0x4F0A,
            [0x89, 0x85, 0xA7, 0xB2, 0x47, 0x50, 0x82, 0xD1],
        ),
        "ID3D12CommandSignature",
    ),
    (
        Guid::new(
            0xC642_26A8,
            0x9201,
            0x46AF,
            [0xB4, 0xCC, 0x53, 0xFB, 0x9F, 0xF7, 0x41, 0x4F],
        ),
        "ID3D12PipelineLibrary",
    ),
    (IID_IUNKNOWN, "IUnknown"),
];

/// Nom d'une interface connue (`ID3D12Heap`…), le cas échéant.
pub(crate) fn known_iid_name(g: &Guid) -> Option<&'static str> {
    KNOWN_IIDS.iter().find(|(k, _)| k == g).map(|(_, n)| *n)
}

/// Décrit pour un journal un argument reçu d'un titre : si c'est un pointeur vers de la mémoire
/// lisible, affiche l'IID (nommé s'il est connu) ou les premiers octets ; sinon la valeur seule.
/// La lecture ne fait jamais de faute.
pub(crate) fn describe_guest_arg(value: usize) -> String {
    let bytes = sb_loader::trampoline::peek_memory(value, 32);
    if bytes.len() < 16 {
        return format!("{value:#x}");
    }
    let guid = Guid::new(
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u16::from_le_bytes([bytes[4], bytes[5]]),
        u16::from_le_bytes([bytes[6], bytes[7]]),
        [
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ],
    );
    if let Some(name) = known_iid_name(&guid) {
        return format!("{value:#x} -> IID {{{guid}}} = {name}");
    }
    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{value:#x} -> [{}]", hex.join(" "))
}

#[cfg(test)]
mod diag_tests {
    use super::*;

    #[test]
    fn guest_args_are_described_without_faulting() {
        // Pas un pointeur lisible : valeur seule.
        assert_eq!(describe_guest_arg(0x1234), "0x1234");
        assert_eq!(describe_guest_arg(0), "0x0");
        // IID connu : nommé.
        let iid = KNOWN_IIDS[1].0;
        let s = describe_guest_arg((&raw const iid) as usize);
        assert!(s.contains("ID3D12CommandQueue"), "{s}");
        // Octets quelconques : aperçu hexadécimal.
        let blob = [0xABu8; 32];
        let s = describe_guest_arg(blob.as_ptr() as usize);
        assert!(s.contains("ab ab ab"), "{s}");
    }
}
