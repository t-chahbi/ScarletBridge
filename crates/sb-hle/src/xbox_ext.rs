//! Extensions HLE Xbox GDK (Scarlett) indexées par *stem* de DLL :
//! - `xg_xs.dll` : `XGCreateTextureComputer` (objet `XGTextureAddressComputer`) et
//!   `XGSuggestSwizzleMode`, sur la disposition **linéaire dense** utilisée par `d3d12_xs`.
//! - `xcrdapi.dll` : DirectStorage (`DStorageGetFactory` → `IDStorageFactory`, files de
//!   requêtes exécutées sur un thread de travail, fichiers ouverts via le VFS sandboxé).
//! - `gameinput.dll` : ordinal 1 → `GameInputCreate`.
//! - `d3d12_xs.dll` : ordinal 13 (`D3D12ConsCreateDevice`) → `D3D12XboxCreateDevice`.
//!
//! Les interfaces DirectStorage suivent `dstorage.h` public (PC, GUIDs et vtables identiques) ;
//! les dispositions XG ne sont pas publiques et sont **déduites** (voir commentaires).

#![allow(non_snake_case, unused_variables)]

use core::ffi::c_void;
use core::mem::{offset_of, size_of};
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

use parking_lot::{Condvar, Mutex};
use sb_common::abi::{
    E_ABORT, E_ACCESSDENIED, E_INVALIDARG, E_NOINTERFACE, E_POINTER, E_UNEXPECTED, HResult, S_OK,
};
use sb_common::{ffi_guard, sb_debug, sb_info, sb_warn};
use sb_vfs::VirtualFs;

use crate::com::{
    ComHeader, Guid, IID_IUNKNOWN, VtblSlot, com_add_ref, com_query_interface_list, com_release,
    vslot,
};
use crate::d3d12_xs::{
    D3D12Box, D3D12ResourceDesc, resource_write_bytes, resource_write_texture_region,
    texture_row_layout,
};

const XG_CHANNEL: &str = "xg_xs";
const GDK_RT_CHANNEL: &str = "xgameruntime";
const SYS_CHANNEL: &str = "syslibs";

const RPC_S_OK: i32 = 0;
const RPC_S_INVALID_ARG: i32 = 87;
const RPC_S_OUT_OF_MEMORY: i32 = 14;
const ERROR_NOT_SUPPORTED_WIN32: u32 = 50;

macro_rules! sys_table {
    ($($name:literal => $f:ident),* $(,)?) => {{
        let mut m: HashMap<&'static str, usize> = HashMap::new();
        $(m.insert($name, $f as *const () as usize);)*
        m
    }};
}

fn sys_rpcrt4_exports() -> HashMap<&'static str, usize> {
    sys_table! {
        "UuidCreate" => UuidCreate,
        "UuidCreateSequential" => UuidCreate,
        "UuidCreateNil" => UuidCreateNil,
        "UuidIsNil" => UuidIsNil,
        "UuidEqual" => UuidEqual,
        "UuidToStringA" => UuidToStringA,
        "RpcStringFreeA" => RpcStringFreeA,
    }
}

/// `dbghelp.dll` : aucun symbole de débogage n'est disponible dans le bridge. L'initialisation
/// réussit (les titres l'appellent au démarrage pour leurs rapports de crash) ; les requêtes de
/// symboles et le `StackWalk64` échouent proprement.
fn sys_dbghelp_exports() -> HashMap<&'static str, usize> {
    sys_table! {
        "SymInitialize" => SymInitialize,
        "SymInitializeW" => SymInitialize,
        "SymCleanup" => SymCleanup,
        "SymSetOptions" => SymSetOptions,
        "SymGetOptions" => SymGetOptions,
        "SymFromAddr" => SymFail,
        "SymFromAddrW" => SymFail,
        "SymGetLineFromAddr64" => SymFail,
        "SymGetLineFromAddrW64" => SymFail,
        "SymGetModuleInfo64" => SymFail,
        "SymGetModuleInfoW64" => SymFail,
        "SymLoadModule64" => SymZero,
        "SymLoadModuleExW" => SymZero,
        "SymGetModuleBase64" => SymZero,
        "SymFunctionTableAccess64" => SymZero,
        "StackWalk64" => SymFail,
        "MiniDumpWriteDump" => SymFail,
    }
}

extern "win64" fn UuidCreateNil(uuid: *mut Guid) -> i32 {
    ffi_guard!(SYS_CHANNEL, "UuidCreateNil", RPC_S_INVALID_ARG, {
        if uuid.is_null() {
            return RPC_S_INVALID_ARG;
        }
        // SAFETY: `uuid` non-nul, GUID de 16 octets fourni par l'appelant.
        unsafe { ptr::write_unaligned(uuid, Guid::new(0, 0, 0, [0; 8])) };
        RPC_S_OK
    })
}

/// `UuidCreate` : UUID v4 (aléatoire) depuis `getrandom`.
extern "win64" fn UuidCreate(uuid: *mut Guid) -> i32 {
    ffi_guard!(SYS_CHANNEL, "UuidCreate", RPC_S_INVALID_ARG, {
        if uuid.is_null() {
            return RPC_S_INVALID_ARG;
        }
        let mut b = [0u8; 16];
        // SAFETY: tampon de 16 octets valide ; `getrandom` sans flag.
        let n = unsafe { libc::getrandom(b.as_mut_ptr().cast(), b.len(), 0) };
        if n != 16 {
            return RPC_S_OUT_OF_MEMORY;
        }
        b[7] = (b[7] & 0x0F) | 0x40; // version 4 : nibble haut de data3
        b[8] = (b[8] & 0x3F) | 0x80;
        let g = Guid::new(
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            u16::from_le_bytes([b[4], b[5]]),
            u16::from_le_bytes([b[6], b[7]]),
            [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
        );
        // SAFETY: `uuid` non-nul, écriture non alignée d'un GUID.
        unsafe { ptr::write_unaligned(uuid, g) };
        RPC_S_OK
    })
}

extern "win64" fn UuidIsNil(uuid: *const Guid, status: *mut i32) -> i32 {
    ffi_guard!(SYS_CHANNEL, "UuidIsNil", 0, {
        if !status.is_null() {
            // SAFETY: `status` non-nul.
            unsafe { *status = RPC_S_OK };
        }
        if uuid.is_null() {
            return 1; // un UUID NULL est considéré nul
        }
        // SAFETY: `uuid` non-nul.
        let g = unsafe { ptr::read_unaligned(uuid) };
        i32::from(g == Guid::new(0, 0, 0, [0; 8]))
    })
}

extern "win64" fn UuidEqual(a: *const Guid, b: *const Guid, status: *mut i32) -> i32 {
    ffi_guard!(SYS_CHANNEL, "UuidEqual", 0, {
        if !status.is_null() {
            // SAFETY: `status` non-nul.
            unsafe { *status = RPC_S_OK };
        }
        let nil = Guid::new(0, 0, 0, [0; 8]);
        // SAFETY: lectures non alignées de pointeurs vérifiés non-nuls ; NULL = UUID nul.
        let ga = if a.is_null() {
            nil
        } else {
            unsafe { ptr::read_unaligned(a) }
        };
        let gb = if b.is_null() {
            nil
        } else {
            unsafe { ptr::read_unaligned(b) }
        };
        i32::from(ga == gb)
    })
}

/// `UuidToStringA(const UUID*, RPC_CSTR*)` : chaîne minuscule, allouée avec `malloc`
/// (libérée par `RpcStringFreeA`).
extern "win64" fn UuidToStringA(uuid: *const Guid, out: *mut *mut u8) -> i32 {
    ffi_guard!(SYS_CHANNEL, "UuidToStringA", RPC_S_INVALID_ARG, {
        if out.is_null() {
            return RPC_S_INVALID_ARG;
        }
        let nil = Guid::new(0, 0, 0, [0; 8]);
        // SAFETY: lecture non alignée si non-nul ; NULL = UUID nul (comme Windows).
        let g = if uuid.is_null() {
            nil
        } else {
            unsafe { ptr::read_unaligned(uuid) }
        };
        let text = fmt_guid(&g).trim_matches(['{', '}']).to_ascii_lowercase();
        // SAFETY: 37 octets (36 caractères + nul) alloués puis remplis intégralement.
        let buf = unsafe { libc::malloc(text.len() + 1) }.cast::<u8>();
        if buf.is_null() {
            return RPC_S_OUT_OF_MEMORY;
        }
        // SAFETY: `buf` a `text.len() + 1` octets ; `out` non-nul.
        unsafe {
            ptr::copy_nonoverlapping(text.as_ptr(), buf, text.len());
            *buf.add(text.len()) = 0;
            *out = buf;
        }
        RPC_S_OK
    })
}

extern "win64" fn RpcStringFreeA(s: *mut *mut u8) -> i32 {
    ffi_guard!(SYS_CHANNEL, "RpcStringFreeA", RPC_S_INVALID_ARG, {
        if s.is_null() {
            return RPC_S_INVALID_ARG;
        }
        // SAFETY: pointeur issu de `UuidToStringA` (malloc) ou NULL ; mis à NULL ensuite.
        unsafe {
            libc::free((*s).cast());
            *s = ptr::null_mut();
        }
        RPC_S_OK
    })
}

extern "win64" fn SymInitialize(_process: usize, _search_path: usize, _invade: i32) -> i32 {
    1
}
extern "win64" fn SymCleanup(_process: usize) -> i32 {
    1
}
extern "win64" fn SymSetOptions(options: u32) -> u32 {
    options
}
extern "win64" fn SymGetOptions() -> u32 {
    0
}
/// Échec propre (`FALSE` + `ERROR_NOT_SUPPORTED`) pour les requêtes de symboles / pile.
extern "win64" fn SymFail() -> i32 {
    sb_loader::set_last_error(ERROR_NOT_SUPPORTED_WIN32);
    0
}
/// Retourne 0 / NULL (module de base inconnu, pas de table de fonction).
extern "win64" fn SymZero() -> usize {
    sb_loader::set_last_error(ERROR_NOT_SUPPORTED_WIN32);
    0
}

static GDK_INIT_LOGGED: AtomicBool = AtomicBool::new(false);

fn fmt_guid(g: &Guid) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7]
    )
}

/// `InitializeApiImpl` / `Ex` / `Ex2` : le runtime hors ligne s'initialise toujours.
/// Les paramètres (version GDK, mode) ne sont pas publics : on les journalise une fois.
extern "win64" fn GdkInitializeApiImpl(a1: usize, a2: usize, a3: usize, a4: usize) -> HResult {
    ffi_guard!(GDK_RT_CHANNEL, "InitializeApiImpl", E_UNEXPECTED, {
        if !GDK_INIT_LOGGED.swap(true, Ordering::AcqRel) {
            sb_info!(
                GDK_RT_CHANNEL,
                "InitializeApiImpl*(a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}) -> S_OK (runtime hors ligne)"
            );
        }
        S_OK
    })
}

extern "win64" fn GdkUninitializeApiImpl() -> HResult {
    ffi_guard!(GDK_RT_CHANNEL, "UninitializeApiImpl", E_UNEXPECTED, {
        sb_debug!(GDK_RT_CHANNEL, "UninitializeApiImpl()");
        S_OK
    })
}

/// `QueryApiImpl(const GUID* classId, const GUID* interfaceId, void** out)` : délègue au registre
/// des classes du runtime GDK (`gdk_classes`). Classe inconnue : `0x80070032`, journalisée.
extern "win64" fn GdkQueryApiImpl(
    class_id: *const Guid,
    interface_id: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    ffi_guard!(GDK_RT_CHANNEL, "QueryApiImpl", E_UNEXPECTED, {
        crate::gdk_classes::query(class_id, interface_id, out)
    })
}

/// `XErrorReport` : signature non publique, point d'entrée de rapport d'erreur du runtime.
/// Journalise ses arguments et réussit (aucun service de rapport hors ligne).
extern "win64" fn GdkXErrorReport(a1: usize, a2: usize, a3: usize, a4: usize) -> HResult {
    ffi_guard!(GDK_RT_CHANNEL, "XErrorReport", E_UNEXPECTED, {
        sb_warn!(
            GDK_RT_CHANNEL,
            "XErrorReport(a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}) ignoré"
        );
        S_OK
    })
}

/// `GetApiVersionImpl(uint64_t*, uint64_t*)` : écrit deux valeurs constantes à chaque appel.
extern "win64" fn GdkGetApiVersionImpl(major_minor: *mut u64, build_rev: *mut u64) -> HResult {
    ffi_guard!(GDK_RT_CHANNEL, "GetApiVersionImpl", E_UNEXPECTED, {
        if major_minor.is_null() || build_rev.is_null() {
            return E_POINTER;
        }
        // SAFETY: deux pointeurs vers des `u64` de l'appelant, vérifiés non-nuls.
        unsafe {
            *major_minor = 0x0001_0000_0000_0000;
            *build_rev = 0;
        }
        S_OK
    })
}
const DS_CHANNEL: &str = "dstorage";

/// Exports par nom, indexés par *stem* de DLL en minuscules sans `.dll` (ex. `"ws2_32"`).
/// Prioritaires sur les familles historiques ; un stem listé ici est considéré HLE.
pub fn by_dll() -> HashMap<&'static str, HashMap<&'static str, usize>> {
    let mut out = HashMap::new();

    let mut xg = HashMap::new();
    xg.insert(
        "XGCreateTextureComputer",
        XGCreateTextureComputer as *const () as usize,
    );
    xg.insert(
        "XGSuggestSwizzleMode",
        XGSuggestSwizzleMode as *const () as usize,
    );
    out.insert("xg_xs", xg);

    let mut ds = HashMap::new();
    ds.insert(
        "DStorageGetFactory",
        DStorageGetFactory as *const () as usize,
    );
    ds.insert(
        "DStorageSetConfiguration",
        DStorageSetConfiguration as *const () as usize,
    );
    out.insert("xcrdapi", ds);

    // Points d'entrée `*Impl` : `xgameruntime.lib` (liée statiquement dans les titres GDK) charge
    // `xgameruntime.dll` à l'exécution puis les résout par `GetProcAddress`.
    let mut rt = HashMap::new();
    rt.insert(
        "InitializeApiImpl",
        GdkInitializeApiImpl as *const () as usize,
    );
    rt.insert(
        "InitializeApiImplEx",
        GdkInitializeApiImpl as *const () as usize,
    );
    rt.insert(
        "InitializeApiImplEx2",
        GdkInitializeApiImpl as *const () as usize,
    );
    rt.insert(
        "UninitializeApiImpl",
        GdkUninitializeApiImpl as *const () as usize,
    );
    rt.insert("QueryApiImpl", GdkQueryApiImpl as *const () as usize);
    rt.insert("XErrorReport", GdkXErrorReport as *const () as usize);
    out.insert("xgameruntime", rt);

    // Bibliothèques Windows standard attendues par la plupart des titres.
    out.insert("rpcrt4", sys_rpcrt4_exports());
    out.insert("dbghelp", sys_dbghelp_exports());

    // `gameplatformservices.dll` : même `QueryApiImpl` hors ligne + versions de l'API.
    let mut gps = HashMap::new();
    gps.insert("QueryApiImpl", GdkQueryApiImpl as *const () as usize);
    gps.insert(
        "GetApiVersionImpl",
        GdkGetApiVersionImpl as *const () as usize,
    );
    out.insert("gameplatformservices", gps);
    out
}

/// Exports par ordinal, indexés par *stem* de DLL.
pub fn ordinals_by_dll() -> HashMap<&'static str, HashMap<u16, usize>> {
    let mut out = HashMap::new();
    if let Some(&create) = crate::gdk_xs::gameinput_exports().get("GameInputCreate") {
        out.insert("gameinput", HashMap::from([(1u16, create)]));
    }
    if let Some(&create) = crate::d3d12_xs::d3d12_xs_exports().get("D3D12XboxCreateDevice") {
        // Ordinal 13 = `D3D12ConsCreateDevice` (nom interne), même ABI que `D3D12XboxCreateDevice`.
        out.insert("d3d12_xs", HashMap::from([(13u16, create)]));
    }
    out
}

// ============================================================================================
// xg_xs.dll — XGTextureAddressComputer (disposition linéaire dense)
// ============================================================================================
//
// Signatures déduites (en-tête `xg_xs.h` non public) :
//   HRESULT        XGCreateTextureComputer(const XG_RESOURCE_DESC*, XGTextureAddressComputer**)
//   XG_SWIZZLE_MODE XGSuggestSwizzleMode(const XG_RESOURCE_DESC*)
// `XG_RESOURCE_DESC` est supposé calqué sur `D3D12_RESOURCE_DESC` (56 octets, `Layout` =
// mode de swizzle). Vtable `XGTextureAddressComputer` (après `IUnknown`) supposée :
//   3 GetResourceLayout(XG_RESOURCE_LAYOUT*) -> HRESULT
//   4 GetResourceSizeBytes() -> UINT64
//   5 GetResourceBaseAlignmentBytes() -> UINT64
//   6 GetMipLevelOffsetBytes(UINT32 Plane, UINT32 MipLevel) -> UINT64
//   7 GetTexelElementOffsetBytes(UINT32 Plane, UINT32 Mip, UINT64 X, UINT32 Y, UINT32 ZOrSlice,
//                                UINT32 Sample) -> UINT64
//   8 GetTexelCoordinate(UINT32 Plane, UINT32 Mip, UINT64 Offset, UINT64* X, UINT32* Y,
//                        UINT32* ZOrSlice, UINT32* Sample) -> HRESULT
//   9 CopyIntoSubresource(void* Dst, UINT32 Plane, UINT32 Subresource, const void* Src,
//                         UINT32 RowPitch, UINT32 SlicePitch) -> HRESULT
//  10 CopyFromSubresource(void* Dst, UINT32 Plane, UINT32 Subresource, const void* Src,
//                         UINT32 RowPitch, UINT32 SlicePitch) -> HRESULT
// Les ressources ScarletBridge étant linéaires (identiques au stockage `d3d12_xs`), la seule
// suggestion de swizzle possible est `XG_SWIZZLE_MODE_LINEAR` (0).

/// `XG_SWIZZLE_MODE_LINEAR` (valeur déduite).
pub const XG_SWIZZLE_MODE_LINEAR: u32 = 0;
const XG_MAX_MIPS: usize = 15;
const XG_BASE_ALIGNMENT: u64 = 64 * 1024;
const XG_INVALID_OFFSET: u64 = u64::MAX;

/// Description d'une ressource XG (déduite : même disposition que `D3D12_RESOURCE_DESC`).
pub type XgResourceDesc = D3D12ResourceDesc;

/// Disposition d'un niveau de mip (déduite).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct XgMipLevelLayout {
    pub size_bytes: u64,
    pub offset_bytes: u64,
    pub slice_2d_size_bytes: u64,
    pub pitch_pixels: u32,
    pub pitch_bytes: u32,
    pub alignment_bytes: u32,
    pub padded_width_elements: u32,
    pub padded_height_elements: u32,
    pub padded_depth_or_array_slices: u32,
    pub swizzle_mode: u32,
    pub _reserved: u32,
}

/// Disposition d'un plan (déduite ; un seul plan pour les formats non planaires).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct XgPlaneLayout {
    pub usage: u32,
    pub bytes_per_element: u32,
    pub size_bytes: u64,
    pub base_offset_bytes: u64,
    pub base_alignment_bytes: u64,
    pub mip_layout: [XgMipLevelLayout; XG_MAX_MIPS],
}

/// `XG_RESOURCE_LAYOUT` (déduite, simplifiée à un plan).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct XgResourceLayout {
    pub size_bytes: u64,
    pub base_alignment_bytes: u64,
    pub mip_levels: u32,
    pub plane_count: u32,
    pub dimension: u32,
    pub _pad: u32,
    pub plane: [XgPlaneLayout; 1],
}

const _: () = {
    assert!(size_of::<XgMipLevelLayout>() == 56);
    assert!(offset_of!(XgMipLevelLayout, pitch_bytes) == 28);
    assert!(size_of::<XgPlaneLayout>() == 32 + 56 * XG_MAX_MIPS);
    assert!(offset_of!(XgPlaneLayout, mip_layout) == 32);
    assert!(size_of::<XgResourceLayout>() == 32 + size_of::<XgPlaneLayout>());
    assert!(offset_of!(XgResourceLayout, plane) == 32);
};

/// Géométrie calculée d'un mip.
#[derive(Debug, Clone, Copy)]
struct MipGeom {
    width: u32,
    height: u32,
    depth: u32,
    row_bytes: usize,
    rows: usize,
    offset: u64,
    slice_2d: u64,
    size: u64,
}

#[derive(Debug, Clone)]
struct TextureLayout {
    desc: XgResourceDesc,
    /// Bloc (1 ou 4 texels) et octets par élément.
    block: u32,
    bpe: usize,
    array_size: u32,
    mips: Vec<MipGeom>,
    /// Taille d'une tranche de tableau (tous mips).
    slice_stride: u64,
    total: u64,
}

impl TextureLayout {
    fn new(desc: XgResourceDesc) -> Result<Self, HResult> {
        // Dimensions D3D12 : 2 = TEXTURE1D, 3 = TEXTURE2D, 4 = TEXTURE3D.
        if !(2..=4).contains(&desc.dimension) || desc.width == 0 || desc.width > u64::from(u32::MAX)
        {
            return Err(E_INVALIDARG);
        }
        if desc.sample_desc.count > 1 {
            return Err(E_INVALIDARG);
        }
        let width = desc.width as u32;
        let height = if desc.dimension == 2 {
            1
        } else {
            desc.height.max(1)
        };
        let is_3d = desc.dimension == 4;
        let depth0 = if is_3d {
            u32::from(desc.depth_or_array_size.max(1))
        } else {
            1
        };
        let array_size = if is_3d {
            1
        } else {
            u32::from(desc.depth_or_array_size.max(1))
        };
        let full_chain = 32 - width.max(height).max(depth0).leading_zeros();
        let mip_count = if desc.mip_levels == 0 {
            full_chain
        } else {
            u32::from(desc.mip_levels)
        };
        if mip_count == 0 || mip_count > full_chain || mip_count as usize > XG_MAX_MIPS {
            return Err(E_INVALIDARG);
        }
        let (rb4, rows4) = texture_row_layout(4, 4, desc.format);
        let (block, bpe) = if rows4 == 1 { (4, rb4) } else { (1, rb4 / 4) };
        let mut mips = Vec::with_capacity(mip_count as usize);
        let mut offset = 0u64;
        for m in 0..mip_count {
            let w = (width >> m).max(1);
            let h = (height >> m).max(1);
            let d = (depth0 >> m).max(1);
            let (row_bytes, rows) = texture_row_layout(w, h, desc.format);
            let slice_2d = (row_bytes * rows) as u64;
            let size = slice_2d * u64::from(d);
            mips.push(MipGeom {
                width: w,
                height: h,
                depth: d,
                row_bytes,
                rows,
                offset,
                slice_2d,
                size,
            });
            offset += size;
        }
        let slice_stride = offset;
        let total = slice_stride * u64::from(array_size);
        Ok(Self {
            desc,
            block,
            bpe,
            array_size,
            mips,
            slice_stride,
            total,
        })
    }

    fn subresource(&self, index: u32) -> Option<(u32, u32)> {
        let n = self.mips.len() as u32;
        let slice = index / n;
        (slice < self.array_size).then_some((index % n, slice))
    }

    fn texel_offset(&self, mip: u32, x: u64, y: u32, z_or_slice: u32, sample: u32) -> u64 {
        let Some(g) = self.mips.get(mip as usize) else {
            return XG_INVALID_OFFSET;
        };
        if sample != 0 || x >= u64::from(g.width) || y >= g.height {
            return XG_INVALID_OFFSET;
        }
        let block = u64::from(self.block);
        let in_slice =
            u64::from(y / self.block) * g.row_bytes as u64 + (x / block) * self.bpe as u64;
        if self.desc.dimension == 4 {
            if z_or_slice >= g.depth {
                return XG_INVALID_OFFSET;
            }
            g.offset + u64::from(z_or_slice) * g.slice_2d + in_slice
        } else {
            if z_or_slice >= self.array_size {
                return XG_INVALID_OFFSET;
            }
            u64::from(z_or_slice) * self.slice_stride + g.offset + in_slice
        }
    }

    fn fill_layout(&self, out: &mut XgResourceLayout) {
        *out = XgResourceLayout::default();
        out.size_bytes = self.total;
        out.base_alignment_bytes = XG_BASE_ALIGNMENT;
        out.mip_levels = self.mips.len() as u32;
        out.plane_count = 1;
        out.dimension = self.desc.dimension;
        let plane = &mut out.plane[0];
        plane.bytes_per_element = self.bpe as u32;
        plane.size_bytes = self.total;
        plane.base_offset_bytes = 0;
        plane.base_alignment_bytes = XG_BASE_ALIGNMENT;
        for (i, g) in self.mips.iter().enumerate() {
            plane.mip_layout[i] = XgMipLevelLayout {
                size_bytes: g.size,
                offset_bytes: g.offset,
                slice_2d_size_bytes: g.slice_2d,
                pitch_pixels: (g.row_bytes / self.bpe.max(1)) as u32 * self.block,
                pitch_bytes: g.row_bytes as u32,
                alignment_bytes: 1,
                padded_width_elements: g.width.div_ceil(self.block),
                padded_height_elements: g.rows as u32,
                padded_depth_or_array_slices: if self.desc.dimension == 4 {
                    g.depth
                } else {
                    self.array_size
                },
                swizzle_mode: XG_SWIZZLE_MODE_LINEAR,
                _reserved: 0,
            };
        }
    }

    /// Copie entre une image linéaire (pas `row_pitch` / `slice_pitch`) et le sous-ressource dense.
    fn copy_subresource(
        &self,
        subresource: u32,
        dense: *mut u8,
        linear: *mut u8,
        row_pitch: usize,
        slice_pitch: usize,
        into_dense: bool,
    ) -> HResult {
        let Some((mip, slice)) = self.subresource(subresource) else {
            return E_INVALIDARG;
        };
        let g = self.mips[mip as usize];
        if row_pitch < g.row_bytes || (g.depth > 1 && slice_pitch < row_pitch * g.rows) {
            return E_INVALIDARG;
        }
        let base = u64::from(slice) * self.slice_stride + g.offset;
        for z in 0..g.depth as usize {
            for y in 0..g.rows {
                let d_off = base as usize + z * g.slice_2d as usize + y * g.row_bytes;
                let l_off = z * slice_pitch + y * row_pitch;
                // SAFETY: l'appelant garantit des tampons couvrant la sous-ressource (contrat XG).
                unsafe {
                    if into_dense {
                        ptr::copy_nonoverlapping(linear.add(l_off), dense.add(d_off), g.row_bytes);
                    } else {
                        ptr::copy_nonoverlapping(dense.add(d_off), linear.add(l_off), g.row_bytes);
                    }
                }
            }
        }
        S_OK
    }
}

#[repr(C)]
struct XgComputerVtbl {
    slots: [VtblSlot; 11],
}

#[repr(C)]
struct XgComputerObject {
    header: ComHeader,
    layout: TextureLayout,
}

fn xg_obj<'a>(this: *mut XgComputerObject) -> Option<&'a TextureLayout> {
    // SAFETY: `this` provient de `XGCreateTextureComputer` (objet vivant, jamais libéré).
    (!this.is_null()).then(|| unsafe { &(*this).layout })
}

extern "win64" fn xg_qi(this: *mut ComHeader, riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    com_query_interface_list("XGTextureAddressComputer", this, riid, ppv, &[])
}

extern "win64" fn xg_get_resource_layout(
    this: *mut XgComputerObject,
    out: *mut XgResourceLayout,
) -> HResult {
    ffi_guard!(XG_CHANNEL, "GetResourceLayout", E_UNEXPECTED, {
        let Some(l) = xg_obj(this) else {
            return E_INVALIDARG;
        };
        if out.is_null() {
            return E_POINTER;
        }
        let mut tmp = XgResourceLayout::default();
        l.fill_layout(&mut tmp);
        // SAFETY: `out` non-nul (structure de sortie de l'appelant).
        unsafe { ptr::write_unaligned(out, tmp) };
        S_OK
    })
}

extern "win64" fn xg_get_size(this: *mut XgComputerObject) -> u64 {
    xg_obj(this).map_or(0, |l| l.total)
}

extern "win64" fn xg_get_base_alignment(this: *mut XgComputerObject) -> u64 {
    xg_obj(this).map_or(0, |_| XG_BASE_ALIGNMENT)
}

extern "win64" fn xg_get_mip_offset(this: *mut XgComputerObject, plane: u32, mip: u32) -> u64 {
    match xg_obj(this) {
        Some(l) if plane == 0 => l
            .mips
            .get(mip as usize)
            .map_or(XG_INVALID_OFFSET, |g| g.offset),
        _ => XG_INVALID_OFFSET,
    }
}

extern "win64" fn xg_get_texel_offset(
    this: *mut XgComputerObject,
    plane: u32,
    mip: u32,
    x: u64,
    y: u32,
    z_or_slice: u32,
    sample: u32,
) -> u64 {
    ffi_guard!(
        XG_CHANNEL,
        "GetTexelElementOffsetBytes",
        XG_INVALID_OFFSET,
        {
            match xg_obj(this) {
                Some(l) if plane == 0 => l.texel_offset(mip, x, y, z_or_slice, sample),
                _ => XG_INVALID_OFFSET,
            }
        }
    )
}

#[allow(clippy::too_many_arguments)]
extern "win64" fn xg_get_texel_coordinate(
    this: *mut XgComputerObject,
    plane: u32,
    mip: u32,
    offset: u64,
    out_x: *mut u64,
    out_y: *mut u32,
    out_z: *mut u32,
    out_sample: *mut u32,
) -> HResult {
    ffi_guard!(XG_CHANNEL, "GetTexelCoordinate", E_UNEXPECTED, {
        let Some(l) = xg_obj(this) else {
            return E_INVALIDARG;
        };
        let Some(g) = l.mips.get(mip as usize).copied() else {
            return E_INVALIDARG;
        };
        if plane != 0 || out_x.is_null() || out_y.is_null() || out_z.is_null() {
            return E_INVALIDARG;
        }
        let (zs, rel) = if l.desc.dimension == 4 {
            let rel = offset.checked_sub(g.offset).ok_or(E_INVALIDARG);
            let Ok(rel) = rel else { return E_INVALIDARG };
            if rel >= g.size {
                return E_INVALIDARG;
            }
            ((rel / g.slice_2d) as u32, rel % g.slice_2d)
        } else {
            let slice = offset / l.slice_stride.max(1);
            let within = offset % l.slice_stride.max(1);
            if slice >= u64::from(l.array_size) || within < g.offset || within >= g.offset + g.size
            {
                return E_INVALIDARG;
            }
            (slice as u32, within - g.offset)
        };
        let row = rel / g.row_bytes as u64;
        let col = (rel % g.row_bytes as u64) / l.bpe as u64;
        // SAFETY: pointeurs de sortie vérifiés non-nuls.
        unsafe {
            *out_x = col * u64::from(l.block);
            *out_y = row as u32 * l.block;
            *out_z = zs;
            if !out_sample.is_null() {
                *out_sample = 0;
            }
        }
        S_OK
    })
}

extern "win64" fn xg_copy_into_subresource(
    this: *mut XgComputerObject,
    dst: *mut c_void,
    plane: u32,
    subresource: u32,
    src: *const c_void,
    row_pitch: u32,
    slice_pitch: u32,
) -> HResult {
    ffi_guard!(XG_CHANNEL, "CopyIntoSubresource", E_UNEXPECTED, {
        let Some(l) = xg_obj(this) else {
            return E_INVALIDARG;
        };
        if plane != 0 || dst.is_null() || src.is_null() {
            return E_INVALIDARG;
        }
        l.copy_subresource(
            subresource,
            dst.cast(),
            src.cast_mut().cast(),
            row_pitch as usize,
            slice_pitch as usize,
            true,
        )
    })
}

extern "win64" fn xg_copy_from_subresource(
    this: *mut XgComputerObject,
    dst: *mut c_void,
    plane: u32,
    subresource: u32,
    src: *const c_void,
    row_pitch: u32,
    slice_pitch: u32,
) -> HResult {
    ffi_guard!(XG_CHANNEL, "CopyFromSubresource", E_UNEXPECTED, {
        let Some(l) = xg_obj(this) else {
            return E_INVALIDARG;
        };
        if plane != 0 || dst.is_null() || src.is_null() {
            return E_INVALIDARG;
        }
        l.copy_subresource(
            subresource,
            src.cast_mut().cast(),
            dst.cast(),
            row_pitch as usize,
            slice_pitch as usize,
            false,
        )
    })
}

const fn build_xg_vtbl() -> XgComputerVtbl {
    XgComputerVtbl {
        slots: [
            vslot(xg_qi as *const c_void),
            vslot(com_add_ref as *const c_void),
            vslot(com_release as *const c_void),
            vslot(xg_get_resource_layout as *const c_void),
            vslot(xg_get_size as *const c_void),
            vslot(xg_get_base_alignment as *const c_void),
            vslot(xg_get_mip_offset as *const c_void),
            vslot(xg_get_texel_offset as *const c_void),
            vslot(xg_get_texel_coordinate as *const c_void),
            vslot(xg_copy_into_subresource as *const c_void),
            vslot(xg_copy_from_subresource as *const c_void),
        ],
    }
}

static XG_COMPUTER_VTBL: XgComputerVtbl = build_xg_vtbl();

extern "win64" fn XGCreateTextureComputer(
    desc: *const XgResourceDesc,
    out: *mut *mut c_void,
) -> HResult {
    ffi_guard!(XG_CHANNEL, "XGCreateTextureComputer", E_UNEXPECTED, {
        if out.is_null() {
            return E_POINTER;
        }
        // SAFETY: `out` vérifié non-nul.
        unsafe { *out = ptr::null_mut() };
        if desc.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: `desc` non-nul (56 octets, lecture seule).
        let d = unsafe { ptr::read_unaligned(desc) };
        if d.layout != XG_SWIZZLE_MODE_LINEAR {
            sb_debug!(
                XG_CHANNEL,
                "XGCreateTextureComputer: swizzle {} demandé -> disposition linéaire ScarletBridge",
                d.layout
            );
        }
        let layout = match TextureLayout::new(d) {
            Ok(l) => l,
            Err(hr) => return hr,
        };
        let obj = Box::new(XgComputerObject {
            header: ComHeader::new((&raw const XG_COMPUTER_VTBL).cast()),
            layout,
        });
        // SAFETY: `out` vérifié non-nul.
        unsafe { *out = Box::into_raw(obj).cast() };
        S_OK
    })
}

extern "win64" fn XGSuggestSwizzleMode(desc: *const XgResourceDesc) -> u32 {
    ffi_guard!(
        XG_CHANNEL,
        "XGSuggestSwizzleMode",
        XG_SWIZZLE_MODE_LINEAR,
        {
            sb_debug!(
                XG_CHANNEL,
                "XGSuggestSwizzleMode({desc:p}) -> LINEAR (seule disposition prise en charge)"
            );
            XG_SWIZZLE_MODE_LINEAR
        }
    )
}

// ============================================================================================
// xcrdapi.dll — DirectStorage (`dstorage.h` public)
// ============================================================================================

/// IID de la fabrique DirectStorage **console** demandée par les titres Xbox (`xcrdapi.dll`).
/// Valeur relevée sur un appel réel, non documentée publiquement : on suppose qu'elle étend
/// `IDStorageFactory` (mêmes 8 premiers slots).
pub const IID_IDSTORAGE_FACTORY_X: Guid = Guid::new(
    0x6295_9d11,
    0xd45f,
    0x4884,
    [0xa0, 0xa3, 0xf7, 0x86, 0x2e, 0xa2, 0x43, 0xae],
);

pub const IID_IDSTORAGE_FACTORY: Guid = Guid::new(
    0x6924_ea0c,
    0xc3cd,
    0x4826,
    [0xb1, 0x0a, 0xf6, 0x4f, 0x4e, 0xd9, 0x27, 0xc1],
);
/// IID de l'interface de fichier DirectStorage **console** demandée à `OpenFile` par les titres
/// Xbox. Valeur relevée sur un appel réel, non documentée publiquement : on suppose qu'elle étend
/// `IDStorageFile` (mêmes slots 0..5 : IUnknown, `Close`, `GetFileInformation`).
pub const IID_IDSTORAGE_FILE_X: Guid = Guid::new(
    0x4370_4F9F,
    0x27C6,
    0x4A9F,
    [0x88, 0x15, 0xD1, 0x65, 0x27, 0x89, 0x4D, 0xF4],
);

pub const IID_IDSTORAGE_FILE: Guid = Guid::new(
    0x5de9_5e7b,
    0x955a,
    0x4868,
    [0xa7, 0x3c, 0x24, 0x3b, 0x29, 0xf4, 0xb8, 0xda],
);
pub const IID_IDSTORAGE_STATUS_ARRAY: Guid = Guid::new(
    0x8239_7587,
    0x7cd5,
    0x453b,
    [0xa0, 0x2e, 0x31, 0x37, 0x9b, 0xd6, 0x46, 0x56],
);
/// IID de tableau de statut DirectStorage **console** demandé à `CreateStatusArray` par les titres
/// Xbox (`cd66d752-2786-44f7-9070-e2a6fdad3f1b`). Vtable identique à `IDStorageStatusArray`.
pub const IID_IDSTORAGE_STATUS_ARRAY_X: Guid = Guid::new(
    0xcd66_d752,
    0x2786,
    0x44f7,
    [0x90, 0x70, 0xe2, 0xa6, 0xfd, 0xad, 0x3f, 0x1b],
);
pub const IID_IDSTORAGE_QUEUE: Guid = Guid::new(
    0xcfdb_d83f,
    0x9e06,
    0x4fda,
    [0x8e, 0xa5, 0x69, 0x04, 0x21, 0x37, 0xf4, 0x9b],
);
pub const IID_IDSTORAGE_QUEUE1: Guid = Guid::new(
    0xdd2f_482c,
    0x5eff,
    0x41e8,
    [0x9c, 0x9e, 0xd2, 0x37, 0x4b, 0x27, 0x81, 0x28],
);
pub const IID_IDSTORAGE_QUEUE2: Guid = Guid::new(
    0xb1c9_d643,
    0x3a49,
    0x44a2,
    [0xb4, 0x6f, 0x65, 0x36, 0x49, 0x47, 0x0d, 0x18],
);
/// IID de file d'attente DirectStorage **console** demandé par `CreateQueueX` (slot 10 de la
/// fabrique console). Valeur relevée sur un appel réel (`3c02cb0c-c050-4195-99a1-a1ce94ba33c5`),
/// non documentée publiquement : vtable identique à `IDStorageQueue` pour les slots publics.
pub const IID_IDSTORAGE_QUEUE_X: Guid = Guid::new(
    0x3c02_cb0c,
    0xc050,
    0x4195,
    [0x99, 0xa1, 0xa1, 0xce, 0x94, 0xba, 0x33, 0xc5],
);

const fn hr(v: u32) -> HResult {
    v as i32
}
pub const E_DSTORAGE_ALREADY_RUNNING: HResult = hr(0x8924_0001);
pub const E_DSTORAGE_INVALID_QUEUE_CAPACITY: HResult = hr(0x8924_0003);
pub const E_DSTORAGE_END_OF_FILE: HResult = hr(0x8924_0007);
pub const E_DSTORAGE_FILE_NOT_OPEN: HResult = hr(0x8924_000B);
pub const E_DSTORAGE_INVALID_DESTINATION_SIZE: HResult = hr(0x8924_000F);
pub const E_DSTORAGE_QUEUE_CLOSED: HResult = hr(0x8924_0010);
pub const E_DSTORAGE_INVALID_QUEUE_PRIORITY: HResult = hr(0x8924_0013);
pub const E_DSTORAGE_INVALID_FILE_HANDLE: HResult = hr(0x8924_0017);
pub const E_DSTORAGE_INVALID_SOURCE_TYPE: HResult = hr(0x8924_001B);
pub const E_DSTORAGE_INVALID_STAGING_BUFFER_SIZE: HResult = hr(0x8924_0020);
pub const E_DSTORAGE_DECOMPRESSION_ERROR: HResult = hr(0x8924_0030);
pub const E_DSTORAGE_INVALID_DESTINATION_TYPE: HResult = hr(0x8924_0040);
const E_PENDING: HResult = hr(0x8000_000A);
const HRESULT_FILE_NOT_FOUND: HResult = hr(0x8007_0002);
const HRESULT_PATH_NOT_FOUND: HResult = hr(0x8007_0003);

pub const DSTORAGE_MIN_QUEUE_CAPACITY: u16 = 0x80;
pub const DSTORAGE_MAX_QUEUE_CAPACITY: u16 = 0x2000;

const DSTORAGE_REQUEST_SOURCE_FILE: u64 = 0;
const DSTORAGE_REQUEST_SOURCE_MEMORY: u64 = 1;
const DSTORAGE_DEST_MEMORY: u64 = 0;
const DSTORAGE_DEST_BUFFER: u64 = 1;
const DSTORAGE_DEST_TEXTURE_REGION: u64 = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DStorageQueueDesc {
    pub source_type: u64,
    pub capacity: u16,
    pub priority: i8,
    pub _pad: [u8; 5],
    pub name: usize,
    pub device: usize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DStorageQueueInfo {
    pub desc: DStorageQueueDesc,
    pub empty_slot_count: u16,
    pub request_count_until_auto_submit: u16,
    pub _pad: u32,
}

/// `DSTORAGE_COMPRESSION_FORMAT_NONE`.
pub const DSTORAGE_COMPRESSION_FORMAT_NONE: u8 = 0;
/// `DSTORAGE_COMPRESSION_FORMAT_GDEFLATE`.
pub const DSTORAGE_COMPRESSION_FORMAT_GDEFLATE: u8 = 1;
/// `DSTORAGE_COMPRESSION_SUPPORT_CPU_FALLBACK` (décompression logicielle sur le CPU).
pub const DSTORAGE_COMPRESSION_SUPPORT_CPU_FALLBACK: u32 = 0x4;

/// Décompresse la charge utile GDeflate d'une requête vers un tampon de `uncompressed_size`
/// octets (allocation fallible, bornée par la taille annoncée par la requête).
fn gdeflate_request_payload(
    compressed: &[u8],
    uncompressed_size: usize,
) -> Result<Vec<u8>, crate::gdeflate::GDeflateError> {
    use crate::gdeflate::{GDeflateError, gdeflate_decompress, gdeflate_uncompressed_size};
    if gdeflate_uncompressed_size(compressed) != Some(uncompressed_size) {
        return Err(GDeflateError::BadHeader);
    }
    let mut out = Vec::new();
    out.try_reserve_exact(uncompressed_size)
        .map_err(|_| GDeflateError::OutputTooSmall)?;
    out.resize(uncompressed_size, 0);
    gdeflate_decompress(compressed, &mut out)?;
    Ok(out)
}

/// `DSTORAGE_REQUEST` (104 octets). Les unions source/destination sont conservées brutes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DStorageRequest {
    /// `CompressionFormat : 8` (octet de tête).
    pub compression_format: u8,
    pub _pad0: [u8; 7],
    /// `SourceType : 1`, `DestinationType : 7`, `Reserved : 48`.
    pub options_bits: u64,
    /// `DSTORAGE_SOURCE` : `File {IDStorageFile*, UINT64 Offset, UINT32 Size}` ou
    /// `Memory {const void*, UINT32 Size}`.
    pub source: [u64; 3],
    /// `DSTORAGE_DESTINATION` (union de 40 octets).
    pub destination: [u64; 5],
    pub uncompressed_size: u32,
    pub _pad1: u32,
    pub cancellation_tag: u64,
    pub name: usize,
}

impl DStorageRequest {
    fn source_type(&self) -> u64 {
        self.options_bits & 1
    }
    fn destination_type(&self) -> u64 {
        (self.options_bits >> 1) & 0x7F
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DStorageErrorParametersRequest {
    pub filename: [u16; 260],
    pub request_name: [u8; 64],
    pub request: DStorageRequest,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DStorageErrorFirstFailure {
    pub hresult: HResult,
    /// `DSTORAGE_COMMAND_TYPE` (0 = REQUEST, 1 = STATUS, 2 = SIGNAL, 3 = EVENT, -1 = NONE).
    pub command_type: i32,
    /// Union : seule la variante `Request` (la plus grande) est utilisée.
    pub request: DStorageErrorParametersRequest,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DStorageErrorRecord {
    pub failure_count: u32,
    pub _pad: u32,
    pub first_failure: DStorageErrorFirstFailure,
}

impl Default for DStorageErrorRecord {
    fn default() -> Self {
        Self {
            failure_count: 0,
            _pad: 0,
            first_failure: DStorageErrorFirstFailure {
                hresult: S_OK,
                command_type: -1,
                request: DStorageErrorParametersRequest {
                    filename: [0; 260],
                    request_name: [0; 64],
                    request: DStorageRequest::default(),
                },
            },
        }
    }
}

const _: () = {
    assert!(size_of::<DStorageQueueDesc>() == 32);
    assert!(offset_of!(DStorageQueueDesc, name) == 16);
    assert!(size_of::<DStorageQueueInfo>() == 40);
    assert!(offset_of!(DStorageQueueInfo, empty_slot_count) == 32);
    assert!(size_of::<DStorageRequest>() == 104);
    assert!(offset_of!(DStorageRequest, source) == 16);
    assert!(offset_of!(DStorageRequest, destination) == 40);
    assert!(offset_of!(DStorageRequest, uncompressed_size) == 80);
    assert!(offset_of!(DStorageRequest, cancellation_tag) == 88);
    assert!(size_of::<DStorageErrorParametersRequest>() == 688);
    assert!(size_of::<DStorageErrorFirstFailure>() == 696);
    assert!(size_of::<DStorageErrorRecord>() == 704);
    assert!(offset_of!(DStorageErrorRecord, first_failure) == 8);
};

// --- IDStorageFile ---

#[repr(C)]
struct DsFileVtbl {
    slots: [VtblSlot; 5],
}

#[repr(C)]
struct DsFileObject {
    header: ComHeader,
    file: Mutex<Option<Arc<File>>>,
    guest_path: String,
}

fn ds_file_from_ptr<'a>(p: usize) -> Option<&'a DsFileObject> {
    if p == 0 {
        return None;
    }
    // SAFETY: lecture du pointeur de vtable pour authentifier l'objet.
    let vtbl = unsafe { (*(p as *const ComHeader)).vtbl };
    if vtbl != (&raw const DS_FILE_VTBL).cast::<c_void>()
        && vtbl != (&raw const DS_FILE_X_VTBL).cast::<c_void>()
    {
        return None;
    }
    // SAFETY: vtable reconnue → `DsFileObject` vivant (jamais libéré).
    Some(unsafe { &*(p as *const DsFileObject) })
}

extern "win64" fn ds_file_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "IDStorageFile",
        this,
        riid,
        ppv,
        &[IID_IDSTORAGE_FILE, IID_IDSTORAGE_FILE_X],
    )
}

extern "win64" fn ds_file_close(this: *mut DsFileObject) {
    if let Some(f) = ds_file_from_ptr(this as usize) {
        f.file.lock().take();
    }
}

extern "win64" fn ds_file_get_info(
    this: *mut DsFileObject,
    out: *mut crate::file_io::ByHandleFileInformation,
) -> HResult {
    ffi_guard!(
        DS_CHANNEL,
        "IDStorageFile::GetFileInformation",
        E_UNEXPECTED,
        {
            let Some(f) = ds_file_from_ptr(this as usize) else {
                return E_INVALIDARG;
            };
            if out.is_null() {
                return E_POINTER;
            }
            let Some(file) = f.file.lock().clone() else {
                return E_DSTORAGE_FILE_NOT_OPEN;
            };
            let Ok(meta) = file.metadata() else {
                return E_UNEXPECTED;
            };
            let ft = {
                let t = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| (d.as_nanos() / 100) as u64)
                    .saturating_add(116_444_736_000_000_000);
                [t as u32, (t >> 32) as u32]
            };
            use std::os::unix::fs::MetadataExt;
            let info = crate::file_io::ByHandleFileInformation {
                file_attributes: 0x20, // FILE_ATTRIBUTE_ARCHIVE (fichier ordinaire NTFS)
                creation_time: ft,
                last_access_time: ft,
                last_write_time: ft,
                volume_serial_number: meta.dev() as u32,
                file_size_high: (meta.len() >> 32) as u32,
                file_size_low: meta.len() as u32,
                number_of_links: meta.nlink() as u32,
                file_index_high: (meta.ino() >> 32) as u32,
                file_index_low: meta.ino() as u32,
            };
            // SAFETY: `out` vérifié non-nul.
            unsafe { ptr::write_unaligned(out, info) };
            S_OK
        }
    )
}

static DS_FILE_VTBL: DsFileVtbl = DsFileVtbl {
    slots: [
        vslot(ds_file_qi as *const c_void),
        vslot(com_add_ref as *const c_void),
        vslot(com_release as *const c_void),
        vslot(ds_file_close as *const c_void),
        vslot(ds_file_get_info as *const c_void),
    ],
};

/// Slots de réserve de l'objet fichier console après les 5 slots publics.
const DS_FILE_X_EXTRA_SLOTS: usize = 24;

#[repr(C)]
struct DsFileXVtbl {
    slots: [VtblSlot; 5 + DS_FILE_X_EXTRA_SLOTS],
}

macro_rules! ds_file_x_extra_slots {
    ($($n:literal),* $(,)?) => {
        [$(vslot(ds_x_unknown_slot::<1, $n> as *const c_void)),*]
    };
}

/// Vtable du fichier console : slots 0..5 = `IDStorageFile` (IUnknown, `Close`,
/// `GetFileInformation`, supposés en tête de l'interface console), puis slots de réserve 5..29.
static DS_FILE_X_VTBL: DsFileXVtbl = {
    let extra = ds_file_x_extra_slots!(
        5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28
    );
    let mut slots = [vslot(ds_file_qi as *const c_void); 5 + DS_FILE_X_EXTRA_SLOTS];
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[3] = vslot(ds_file_close as *const c_void);
    slots[4] = vslot(ds_file_get_info as *const c_void);
    let mut j = 0;
    while j < DS_FILE_X_EXTRA_SLOTS {
        slots[5 + j] = extra[j];
        j += 1;
    }
    DsFileXVtbl { slots }
};

// --- IDStorageStatusArray ---

#[repr(C)]
struct DsStatusVtbl {
    slots: [VtblSlot; 5],
}

#[repr(C)]
struct DsStatusArrayObject {
    header: ComHeader,
    /// (terminé, HRESULT) par entrée.
    entries: Mutex<Vec<(bool, HResult)>>,
}

fn ds_status_from_ptr<'a>(p: usize) -> Option<&'a DsStatusArrayObject> {
    if p == 0 {
        return None;
    }
    // SAFETY: lecture du pointeur de vtable pour authentifier l'objet.
    let vtbl = unsafe { (*(p as *const ComHeader)).vtbl };
    if vtbl != (&raw const DS_STATUS_VTBL).cast::<c_void>() {
        return None;
    }
    // SAFETY: vtable reconnue → objet vivant.
    Some(unsafe { &*(p as *const DsStatusArrayObject) })
}

extern "win64" fn ds_status_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "IDStorageStatusArray",
        this,
        riid,
        ppv,
        &[IID_IDSTORAGE_STATUS_ARRAY],
    )
}

/// `bool IsComplete(UINT32 index)`.
extern "win64" fn ds_status_is_complete(this: *mut DsStatusArrayObject, index: u32) -> u32 {
    ds_status_from_ptr(this as usize)
        .and_then(|s| s.entries.lock().get(index as usize).map(|e| u32::from(e.0)))
        .unwrap_or(0)
}

/// `HRESULT GetHResult(UINT32 index)` : `E_PENDING` tant que l'entrée n'est pas terminée.
extern "win64" fn ds_status_get_hresult(this: *mut DsStatusArrayObject, index: u32) -> HResult {
    match ds_status_from_ptr(this as usize)
        .and_then(|s| s.entries.lock().get(index as usize).copied())
    {
        Some((true, hr)) => hr,
        Some((false, _)) => E_PENDING,
        None => E_INVALIDARG,
    }
}

static DS_STATUS_VTBL: DsStatusVtbl = DsStatusVtbl {
    slots: [
        vslot(ds_status_qi as *const c_void),
        vslot(com_add_ref as *const c_void),
        vslot(com_release as *const c_void),
        vslot(ds_status_is_complete as *const c_void),
        vslot(ds_status_get_hresult as *const c_void),
    ],
};

// --- IDStorageQueue / Queue1 / Queue2 ---

#[derive(Debug, Clone)]
enum DsCommand {
    Request { req: DStorageRequest, name: String },
    Cancelled,
    Status { array: usize, index: u32 },
    Signal { fence: usize, value: u64 },
    SetEvent(usize),
}

#[derive(Default)]
struct DsQueueState {
    pending: Vec<DsCommand>,
    submitted: VecDeque<DsCommand>,
    shutdown: bool,
}

struct DsQueueShared {
    desc: DStorageQueueDesc,
    state: Mutex<DsQueueState>,
    cv: Condvar,
    error: Mutex<DStorageErrorRecord>,
    error_event: usize,
    closed: AtomicBool,
}

#[repr(C)]
struct DsQueueVtbl {
    slots: [VtblSlot; 14],
}

#[repr(C)]
struct DsQueueObject {
    header: ComHeader,
    shared: Arc<DsQueueShared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

fn ds_queue<'a>(this: *mut DsQueueObject) -> Option<&'a DsQueueObject> {
    if this.is_null() {
        return None;
    }
    // SAFETY: lecture du pointeur de vtable pour authentifier l'objet.
    let vtbl = unsafe { (*this.cast::<ComHeader>()).vtbl };
    if vtbl != (&raw const DS_QUEUE_VTBL).cast::<c_void>() {
        return None;
    }
    // SAFETY: vtable reconnue → `DsQueueObject` vivant.
    Some(unsafe { &*this })
}

impl DsQueueShared {
    fn record_failure(
        &self,
        hr: HResult,
        command_type: i32,
        req: Option<(&DStorageRequest, &str)>,
    ) {
        let mut rec = self.error.lock();
        rec.failure_count = rec.failure_count.saturating_add(1);
        if rec.failure_count == 1 {
            rec.first_failure.hresult = hr;
            rec.first_failure.command_type = command_type;
            if let Some((r, name)) = req {
                rec.first_failure.request.request = *r;
                let n = name.as_bytes();
                let len = n.len().min(63);
                rec.first_failure.request.request_name[..len].copy_from_slice(&n[..len]);
                if r.source_type() == DSTORAGE_REQUEST_SOURCE_FILE
                    && let Some(f) = ds_file_from_ptr(r.source[0] as usize)
                {
                    for (dst, c) in rec
                        .first_failure
                        .request
                        .filename
                        .iter_mut()
                        .take(259)
                        .zip(f.guest_path.encode_utf16())
                    {
                        *dst = c;
                    }
                }
            }
            crate::sync::SetEvent(self.error_event);
        }
        sb_warn!(DS_CHANNEL, "requête DirectStorage en échec : {hr:#010x}");
    }

    fn enqueue(&self, cmd: DsCommand) {
        if self.closed.load(Ordering::Acquire) {
            self.record_failure(E_DSTORAGE_QUEUE_CLOSED, 0, None);
            return;
        }
        let mut st = self.state.lock();
        st.pending.push(cmd);
        if st.pending.len() >= usize::from(self.desc.capacity) {
            // File pleine : soumission automatique (comportement DirectStorage).
            let batch = std::mem::take(&mut st.pending);
            st.submitted.extend(batch);
            self.cv.notify_all();
        }
    }

    fn execute_request(&self, req: &DStorageRequest) -> HResult {
        if req.source_type() != self.desc.source_type {
            return E_DSTORAGE_INVALID_SOURCE_TYPE;
        }
        let format = req.compression_format;
        if format != DSTORAGE_COMPRESSION_FORMAT_NONE
            && format != DSTORAGE_COMPRESSION_FORMAT_GDEFLATE
        {
            sb_warn!(
                DS_CHANNEL,
                "format de compression {format:#04x} non pris en charge (formats custom absents)"
            );
            return E_DSTORAGE_DECOMPRESSION_ERROR;
        }
        // Lecture de la source.
        let data: Vec<u8> = match req.source_type() {
            DSTORAGE_REQUEST_SOURCE_FILE => {
                // Source fichier console : `{Offset, Size, IDStorageFile*}` (ordre des champs
                // inversé par rapport à `dstorage.h` PC `{Source, Offset, Size}`).
                let Some(f) = ds_file_from_ptr(req.source[2] as usize) else {
                    sb_warn!(
                        DS_CHANNEL,
                        "poignée de fichier inconnue : source[0]={:#x}, source[1]={:#x}, source[2]={:#x}",
                        req.source[0],
                        req.source[1],
                        req.source[2]
                    );
                    return E_DSTORAGE_INVALID_FILE_HANDLE;
                };
                let Some(file) = f.file.lock().clone() else {
                    return E_DSTORAGE_FILE_NOT_OPEN;
                };
                let size = req.source[1] as u32 as usize;
                let mut buf = vec![0u8; size];
                if let Err(e) = file.read_exact_at(&mut buf, req.source[0]) {
                    return if e.kind() == std::io::ErrorKind::UnexpectedEof {
                        E_DSTORAGE_END_OF_FILE
                    } else {
                        E_UNEXPECTED
                    };
                }
                buf
            }
            _ => {
                let src = req.source[0] as usize as *const u8;
                let size = req.source[1] as u32 as usize;
                if src.is_null() && size != 0 {
                    return E_INVALIDARG;
                }
                if size == 0 {
                    Vec::new()
                } else {
                    // SAFETY: la mémoire source reste valide jusqu'à complétion (contrat DirectStorage).
                    unsafe { std::slice::from_raw_parts(src, size) }.to_vec()
                }
            }
        };
        let data = if format == DSTORAGE_COMPRESSION_FORMAT_GDEFLATE {
            match gdeflate_request_payload(&data, req.uncompressed_size as usize) {
                Ok(d) => d,
                Err(e) => {
                    sb_warn!(
                        DS_CHANNEL,
                        "décompression GDeflate en échec ({} octets -> {} attendus) : {e}",
                        data.len(),
                        req.uncompressed_size
                    );
                    return E_DSTORAGE_DECOMPRESSION_ERROR;
                }
            }
        } else {
            data
        };
        match req.destination_type() {
            DSTORAGE_DEST_MEMORY => {
                let dst = req.destination[0] as usize as *mut u8;
                let size = req.destination[1] as u32 as usize;
                if dst.is_null() || size < data.len() {
                    return E_DSTORAGE_INVALID_DESTINATION_SIZE;
                }
                // SAFETY: tampon destination de `size` octets fourni par l'appelant.
                unsafe { ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len()) };
                S_OK
            }
            DSTORAGE_DEST_BUFFER => {
                let size = req.destination[2] as u32 as usize;
                if size != data.len() {
                    return E_DSTORAGE_INVALID_DESTINATION_SIZE;
                }
                match resource_write_bytes(
                    req.destination[0] as usize as *mut c_void,
                    req.destination[1],
                    &data,
                ) {
                    Ok(()) => S_OK,
                    Err(hr) => hr,
                }
            }
            DSTORAGE_DEST_TEXTURE_REGION => {
                let res = req.destination[0] as usize as *mut c_void;
                // SAFETY: lecture brute de l'union destination (40 octets, entièrement initialisée).
                let raw: [u32; 10] = unsafe { core::mem::transmute(req.destination) };
                let subresource = raw[2];
                let bx = D3D12Box {
                    left: raw[3],
                    top: raw[4],
                    front: raw[5],
                    right: raw[6],
                    bottom: raw[7],
                    back: raw[8],
                };
                match resource_write_texture_region(res, subresource, &bx, &data) {
                    Ok(()) => S_OK,
                    Err(hr) => hr,
                }
            }
            other => {
                sb_warn!(DS_CHANNEL, "type de destination {other} non pris en charge");
                E_DSTORAGE_INVALID_DESTINATION_TYPE
            }
        }
    }

    fn worker_loop(self: Arc<Self>) {
        let _ = sb_loader::init_thread_teb(0);
        sb_loader::module_manager().attach_current_thread_tls();
        let mut batch_hr = S_OK;
        loop {
            let cmd = {
                let mut st = self.state.lock();
                loop {
                    if let Some(c) = st.submitted.pop_front() {
                        break c;
                    }
                    if st.shutdown {
                        return;
                    }
                    self.cv.wait(&mut st);
                }
            };
            match cmd {
                DsCommand::Request { req, name } => {
                    let hr = self.execute_request(&req);
                    if hr != S_OK {
                        self.record_failure(hr, 0, Some((&req, &name)));
                        if batch_hr == S_OK {
                            batch_hr = hr;
                        }
                    }
                }
                DsCommand::Cancelled => {
                    if batch_hr == S_OK {
                        batch_hr = E_ABORT;
                    }
                }
                DsCommand::Status { array, index } => {
                    if let Some(s) = ds_status_from_ptr(array)
                        && let Some(e) = s.entries.lock().get_mut(index as usize)
                    {
                        *e = (true, batch_hr);
                    }
                    batch_hr = S_OK;
                }
                DsCommand::Signal { fence, value } => signal_fence(fence, value),
                DsCommand::SetEvent(h) => {
                    crate::sync::SetEvent(h);
                }
            }
        }
    }
}

/// Appelle `ID3D12Fence::Signal(value)` (slot 10 : IUnknown 3 + ID3D12Object 4 +
/// ID3D12DeviceChild 1 + GetCompletedValue + SetEventOnCompletion).
fn signal_fence(fence: usize, value: u64) {
    if fence == 0 {
        return;
    }
    // SAFETY: `fence` est un `ID3D12Fence*` COM valide fourni à `EnqueueSignal` ; appel du
    // slot `Signal` de sa vtable selon l'ABI Win64.
    unsafe {
        let vtbl = *(fence as *const *const usize);
        let f: extern "win64" fn(usize, u64) -> HResult = core::mem::transmute(*vtbl.add(10));
        f(fence, value);
    }
}

extern "win64" fn ds_queue_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "IDStorageQueue",
        this,
        riid,
        ppv,
        &[
            IID_IDSTORAGE_QUEUE,
            IID_IDSTORAGE_QUEUE1,
            IID_IDSTORAGE_QUEUE2,
        ],
    )
}

extern "win64" fn ds_queue_enqueue_request(this: *mut DsQueueObject, req: *const DStorageRequest) {
    ffi_guard!(DS_CHANNEL, "IDStorageQueue::EnqueueRequest", (), {
        let Some(q) = ds_queue(this) else { return };
        if req.is_null() {
            q.shared.record_failure(E_INVALIDARG, 0, None);
            return;
        }
        // SAFETY: `req` non-nul (copie de 104 octets).
        let r = unsafe { ptr::read_unaligned(req) };
        let name = if r.name == 0 {
            String::new()
        } else {
            // SAFETY: `Name` est une chaîne C terminée par NUL (optionnelle).
            unsafe { std::ffi::CStr::from_ptr(r.name as *const core::ffi::c_char) }
                .to_string_lossy()
                .into_owned()
        };
        q.shared.enqueue(DsCommand::Request { req: r, name });
    })
}

extern "win64" fn ds_queue_enqueue_status(
    this: *mut DsQueueObject,
    array: *mut c_void,
    index: u32,
) {
    ffi_guard!(DS_CHANNEL, "IDStorageQueue::EnqueueStatus", (), {
        let Some(q) = ds_queue(this) else { return };
        let Some(s) = ds_status_from_ptr(array as usize) else {
            q.shared.record_failure(E_INVALIDARG, 1, None);
            return;
        };
        match s.entries.lock().get_mut(index as usize) {
            Some(e) => *e = (false, S_OK),
            None => {
                q.shared.record_failure(E_INVALIDARG, 1, None);
                return;
            }
        }
        q.shared.enqueue(DsCommand::Status {
            array: array as usize,
            index,
        });
    })
}

extern "win64" fn ds_queue_enqueue_signal(
    this: *mut DsQueueObject,
    fence: *mut c_void,
    value: u64,
) {
    ffi_guard!(DS_CHANNEL, "IDStorageQueue::EnqueueSignal", (), {
        let Some(q) = ds_queue(this) else { return };
        if fence.is_null() {
            q.shared.record_failure(E_INVALIDARG, 2, None);
            return;
        }
        q.shared.enqueue(DsCommand::Signal {
            fence: fence as usize,
            value,
        });
    })
}

extern "win64" fn ds_queue_submit(this: *mut DsQueueObject) {
    ffi_guard!(DS_CHANNEL, "IDStorageQueue::Submit", (), {
        let Some(q) = ds_queue(this) else { return };
        let mut st = q.shared.state.lock();
        let batch = std::mem::take(&mut st.pending);
        st.submitted.extend(batch);
        q.shared.cv.notify_all();
    })
}

extern "win64" fn ds_queue_cancel(this: *mut DsQueueObject, mask: u64, value: u64) {
    ffi_guard!(DS_CHANNEL, "IDStorageQueue::CancelRequestsWithTag", (), {
        let Some(q) = ds_queue(this) else { return };
        let mut st = q.shared.state.lock();
        let cancel = |c: &mut DsCommand| {
            if let DsCommand::Request { req, .. } = c
                && req.cancellation_tag & mask == value
            {
                *c = DsCommand::Cancelled;
            }
        };
        st.pending.iter_mut().for_each(cancel);
        st.submitted.iter_mut().for_each(cancel);
    })
}

extern "win64" fn ds_queue_close(this: *mut DsQueueObject) {
    ffi_guard!(DS_CHANNEL, "IDStorageQueue::Close", (), {
        let Some(q) = ds_queue(this) else { return };
        if q.shared.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        {
            let mut st = q.shared.state.lock();
            st.pending.clear();
            st.shutdown = true;
            q.shared.cv.notify_all();
        }
        if let Some(h) = q.worker.lock().take()
            && h.thread().id() != std::thread::current().id()
        {
            let _ = h.join();
        }
    })
}

extern "win64" fn ds_queue_get_error_event(this: *mut DsQueueObject) -> usize {
    ds_queue(this).map_or(0, |q| q.shared.error_event)
}

extern "win64" fn ds_queue_retrieve_error_record(
    this: *mut DsQueueObject,
    out: *mut DStorageErrorRecord,
) {
    ffi_guard!(DS_CHANNEL, "IDStorageQueue::RetrieveErrorRecord", (), {
        let Some(q) = ds_queue(this) else { return };
        if out.is_null() {
            return;
        }
        let rec = *q.shared.error.lock();
        // SAFETY: `out` non-nul (704 octets côté appelant).
        unsafe { ptr::write_unaligned(out, rec) };
    })
}

extern "win64" fn ds_queue_query(this: *mut DsQueueObject, out: *mut DStorageQueueInfo) {
    ffi_guard!(DS_CHANNEL, "IDStorageQueue::Query", (), {
        let Some(q) = ds_queue(this) else { return };
        if out.is_null() {
            return;
        }
        let pending = q.shared.state.lock().pending.len();
        let free = usize::from(q.shared.desc.capacity).saturating_sub(pending) as u16;
        let info = DStorageQueueInfo {
            desc: q.shared.desc,
            empty_slot_count: free,
            request_count_until_auto_submit: free,
            _pad: 0,
        };
        // SAFETY: `out` vérifié non-nul.
        unsafe { ptr::write_unaligned(out, info) };
    })
}

extern "win64" fn ds_queue_enqueue_set_event(this: *mut DsQueueObject, handle: usize) {
    ffi_guard!(DS_CHANNEL, "IDStorageQueue1::EnqueueSetEvent", (), {
        let Some(q) = ds_queue(this) else { return };
        q.shared.enqueue(DsCommand::SetEvent(handle));
    })
}

/// `DSTORAGE_COMPRESSION_SUPPORT GetCompressionSupport(DSTORAGE_COMPRESSION_FORMAT)` : GDeflate est
/// décompressé par le CPU (Rust pur) → `DSTORAGE_COMPRESSION_SUPPORT_CPU_FALLBACK` (0x4) ; aucun
/// support matériel (GPU) ; `NONE` (0) pour les formats non compressés/custom (voir ci-dessus).
extern "win64" fn ds_queue_get_compression_support(this: *mut DsQueueObject, format: u8) -> u32 {
    if format == DSTORAGE_COMPRESSION_FORMAT_GDEFLATE {
        DSTORAGE_COMPRESSION_SUPPORT_CPU_FALLBACK
    } else {
        0
    }
}

static DS_QUEUE_VTBL: DsQueueVtbl = DsQueueVtbl {
    slots: [
        vslot(ds_queue_qi as *const c_void),
        vslot(com_add_ref as *const c_void),
        vslot(com_release as *const c_void),
        vslot(ds_queue_enqueue_request as *const c_void),
        vslot(ds_queue_enqueue_status as *const c_void),
        vslot(ds_queue_enqueue_signal as *const c_void),
        vslot(ds_queue_submit as *const c_void),
        vslot(ds_queue_cancel as *const c_void),
        vslot(ds_queue_close as *const c_void),
        vslot(ds_queue_get_error_event as *const c_void),
        vslot(ds_queue_retrieve_error_record as *const c_void),
        vslot(ds_queue_query as *const c_void),
        vslot(ds_queue_enqueue_set_event as *const c_void),
        vslot(ds_queue_get_compression_support as *const c_void),
    ],
};

// --- IDStorageFactory ---

#[repr(C)]
struct DsFactoryVtbl {
    slots: [VtblSlot; 8],
}

#[repr(C)]
struct DsFactoryObject {
    header: ComHeader,
    /// VFS dédié (tests) ; sinon le VFS du processus invité est utilisé.
    vfs_override: Option<VirtualFs>,
    staging_buffer_size: Mutex<u32>,
    debug_flags: Mutex<u32>,
}

fn ds_factory<'a>(this: *mut DsFactoryObject) -> Option<&'a DsFactoryObject> {
    if this.is_null() {
        return None;
    }
    // SAFETY: lecture du pointeur de vtable pour authentifier l'objet.
    let vtbl = unsafe { (*this.cast::<ComHeader>()).vtbl };
    if vtbl != (&raw const DS_FACTORY_VTBL).cast::<c_void>()
        && vtbl != (&raw const DS_FACTORY_X_VTBL).cast::<c_void>()
    {
        return None;
    }
    // SAFETY: vtable reconnue → fabrique vivante.
    Some(unsafe { &*this })
}

fn write_out(out: *mut *mut c_void, v: *mut c_void) {
    if !out.is_null() {
        // SAFETY: pointeur de sortie vérifié non-nul.
        unsafe { *out = v };
    }
}

extern "win64" fn ds_factory_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "IDStorageFactory",
        this,
        riid,
        ppv,
        &[IID_IDSTORAGE_FACTORY, IID_IDSTORAGE_FACTORY_X],
    )
}

/// Vrai si `riid` est `IUnknown` ou l'une des interfaces `wanted`. Un refus est journalisé en
/// avertissement avec l'IID demandé : les interfaces DirectStorage de la console ont des IID
/// propres, et un `E_NOINTERFACE` silencieux fait échouer le titre sans indice.
fn riid_matches(riid: *const Guid, wanted: &[Guid]) -> bool {
    if riid.is_null() {
        sb_warn!(DS_CHANNEL, "interface demandée NULL refusée");
        return false;
    }
    // SAFETY: `riid` non-nul ; GUID lu sans exigence d'alignement.
    let g = unsafe { ptr::read_unaligned(riid) };
    let ok = g == IID_IUNKNOWN || wanted.contains(&g);
    if !ok {
        sb_warn!(
            DS_CHANNEL,
            "interface {{{g}}} demandée non fournie (acceptées : {}) -> E_NOINTERFACE",
            wanted
                .iter()
                .map(|w| format!("{{{w}}}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    ok
}

/// Valide un descripteur de file d'attente (`CreateQueue` / `CreateQueueX`).
fn validate_queue_desc(d: &DStorageQueueDesc) -> HResult {
    if !(DSTORAGE_MIN_QUEUE_CAPACITY..=DSTORAGE_MAX_QUEUE_CAPACITY).contains(&d.capacity) {
        return E_DSTORAGE_INVALID_QUEUE_CAPACITY;
    }
    if !(-1..=2).contains(&d.priority) {
        return E_DSTORAGE_INVALID_QUEUE_PRIORITY;
    }
    if d.source_type != DSTORAGE_REQUEST_SOURCE_FILE && d.source_type != DSTORAGE_REQUEST_SOURCE_MEMORY {
        return E_DSTORAGE_INVALID_SOURCE_TYPE;
    }
    S_OK
}

/// Crée une file d'attente DirectStorage et l'écrit dans `out` (code commun à `CreateQueue` et
/// `CreateQueueX`).
fn spawn_dstorage_queue(out: *mut *mut c_void, d: DStorageQueueDesc) -> HResult {
    let shared = Arc::new(DsQueueShared {
        desc: d,
        state: Mutex::new(DsQueueState::default()),
        cv: Condvar::new(),
        error: Mutex::new(DStorageErrorRecord::default()),
        error_event: crate::sync::create_event_handle(true, false),
        closed: AtomicBool::new(false),
    });
    let worker_shared = Arc::clone(&shared);
    let worker = match std::thread::Builder::new()
        .name("sb-dstorage".to_string())
        .spawn(move || worker_shared.worker_loop())
    {
        Ok(h) => h,
        Err(_) => return hr(0x8007_000E), // E_OUTOFMEMORY
    };
    let q = Box::new(DsQueueObject {
        header: ComHeader::new((&raw const DS_QUEUE_VTBL).cast()),
        shared,
        worker: Mutex::new(Some(worker)),
    });
    write_out(out, Box::into_raw(q).cast());
    sb_debug!(
        DS_CHANNEL,
        "CreateQueue(source={}, capacity={}, priority={}) -> S_OK",
        d.source_type,
        d.capacity,
        d.priority
    );
    S_OK
}

extern "win64" fn ds_factory_create_queue(
    this: *mut DsFactoryObject,
    desc: *const DStorageQueueDesc,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    ffi_guard!(DS_CHANNEL, "IDStorageFactory::CreateQueue", E_UNEXPECTED, {
        write_out(out, ptr::null_mut());
        if ds_factory(this).is_none() || desc.is_null() {
            return E_INVALIDARG;
        }
        if out.is_null() {
            return E_POINTER;
        }
        // SAFETY: `desc` non-nul (32 octets).
        let d = unsafe { ptr::read_unaligned(desc) };
        let valid = validate_queue_desc(&d);
        if valid != S_OK {
            return valid;
        }
        if !riid_matches(
            riid,
            &[
                IID_IDSTORAGE_QUEUE,
                IID_IDSTORAGE_QUEUE1,
                IID_IDSTORAGE_QUEUE2,
                IID_IDSTORAGE_QUEUE_X,
            ],
        ) {
            return E_NOINTERFACE;
        }
        spawn_dstorage_queue(out, d)
    })
}

/// Slot 10 de la fabrique console : `CreateQueueX`, variante console de `CreateQueue`.
///
/// Signature déduite d'un appel réel : `(this, const DSTORAGE_QUEUE_DESC*, REFIID, void** ppv)`.
/// Le `riid` (IID file d'attente console, non public) est accepté tel quel. Le résultat est la
/// file d'attente écrite dans `*ppv` (déréférencée ensuite par le titre via son propre wrapper).
extern "win64" fn ds_factory_create_queue_x(
    this: *mut DsFactoryObject,
    desc: *const DStorageQueueDesc,
    _riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    ffi_guard!(DS_CHANNEL, "IDStorageFactoryX::CreateQueueX", E_UNEXPECTED, {
        write_out(out, ptr::null_mut());
        if ds_factory(this).is_none() || desc.is_null() {
            return E_INVALIDARG;
        }
        if out.is_null() {
            return E_POINTER;
        }
        // SAFETY: `desc` non-nul (32 octets, layout console `DStorageQueueDesc`).
        let d = unsafe { ptr::read_unaligned(desc) };
        let valid = validate_queue_desc(&d);
        if valid != S_OK {
            return valid;
        }
        spawn_dstorage_queue(out, d)
    })
}

fn resolve_guest_path(factory: &DsFactoryObject, guest: &str) -> Result<PathBuf, HResult> {
    let resolve = |vfs: &VirtualFs| {
        vfs.resolve(guest).map(|r| r.host_path).map_err(|e| {
            sb_warn!(DS_CHANNEL, "OpenFile('{guest}') refusé par le VFS : {e}");
            E_ACCESSDENIED
        })
    };
    if let Some(vfs) = &factory.vfs_override {
        return resolve(vfs);
    }
    let st = crate::process::process_state().read();
    match st.vfs.as_ref() {
        Some(vfs) => resolve(vfs),
        None => Err(HRESULT_PATH_NOT_FOUND),
    }
}

extern "win64" fn ds_factory_open_file(
    this: *mut DsFactoryObject,
    path: *const u16,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    ffi_guard!(DS_CHANNEL, "IDStorageFactory::OpenFile", E_UNEXPECTED, {
        write_out(out, ptr::null_mut());
        let Some(factory) = ds_factory(this) else {
            return E_INVALIDARG;
        };
        if out.is_null() {
            return E_POINTER;
        }
        // SAFETY: `path` est un PCWSTR terminé par NUL (ou NULL).
        let Some(guest) = (unsafe { sb_common::wide::from_pcwstr(path) }) else {
            return E_INVALIDARG;
        };
        if !riid_matches(riid, &[IID_IDSTORAGE_FILE, IID_IDSTORAGE_FILE_X]) {
            return E_NOINTERFACE;
        }
        // SAFETY: `riid` validé non-nul par `riid_matches`.
        let console_file = unsafe { ptr::read_unaligned(riid) } == IID_IDSTORAGE_FILE_X;
        let host = match resolve_guest_path(factory, &guest) {
            Ok(p) => p,
            Err(hr) => {
                sb_warn!(
                    DS_CHANNEL,
                    "OpenFile('{guest}') -> chemin refusé (0x{:08X})",
                    hr as u32
                );
                return hr;
            }
        };
        if host.is_dir() {
            sb_warn!(
                DS_CHANNEL,
                "OpenFile('{guest}') -> c'est un dossier (E_ACCESSDENIED)"
            );
            return E_ACCESSDENIED;
        }
        let file = match File::open(&host) {
            Ok(f) => f,
            Err(e) => {
                sb_warn!(
                    DS_CHANNEL,
                    "OpenFile('{guest}') -> ouverture hôte impossible : {e} ({})",
                    host.display()
                );
                return if e.kind() == std::io::ErrorKind::NotFound {
                    HRESULT_FILE_NOT_FOUND
                } else {
                    E_ACCESSDENIED
                };
            }
        };
        let vtbl: *const c_void = if console_file {
            (&raw const DS_FILE_X_VTBL).cast()
        } else {
            (&raw const DS_FILE_VTBL).cast()
        };
        let obj = Box::new(DsFileObject {
            header: ComHeader::new(vtbl),
            file: Mutex::new(Some(Arc::new(file))),
            guest_path: guest.clone(),
        });
        write_out(out, Box::into_raw(obj).cast());
        sb_info!(
            DS_CHANNEL,
            "OpenFile('{guest}') -> S_OK ({})",
            if console_file {
                "IDStorageFileX : slots 0..5 = IDStorageFile, au-delà E_NOTIMPL journalisé"
            } else {
                "IDStorageFile"
            }
        );
        S_OK
    })
}

extern "win64" fn ds_factory_create_status_array(
    this: *mut DsFactoryObject,
    capacity: u32,
    name: *const core::ffi::c_char,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    ffi_guard!(
        DS_CHANNEL,
        "IDStorageFactory::CreateStatusArray",
        E_UNEXPECTED,
        {
            write_out(out, ptr::null_mut());
            if ds_factory(this).is_none() || capacity == 0 {
                return E_INVALIDARG;
            }
            if out.is_null() {
                return E_POINTER;
            }
            if !riid_matches(riid, &[IID_IDSTORAGE_STATUS_ARRAY, IID_IDSTORAGE_STATUS_ARRAY_X]) {
                return E_NOINTERFACE;
            }
            let obj = Box::new(DsStatusArrayObject {
                header: ComHeader::new((&raw const DS_STATUS_VTBL).cast()),
                // Entrées initialement « terminées » : seul `EnqueueStatus` les arme.
                entries: Mutex::new(vec![(true, S_OK); capacity as usize]),
            });
            write_out(out, Box::into_raw(obj).cast());
            S_OK
        }
    )
}

extern "win64" fn ds_factory_set_debug_flags(this: *mut DsFactoryObject, flags: u32) {
    if let Some(f) = ds_factory(this) {
        *f.debug_flags.lock() = flags;
        sb_debug!(DS_CHANNEL, "SetDebugFlags({flags:#x})");
    }
}

extern "win64" fn ds_factory_set_staging_buffer_size(
    this: *mut DsFactoryObject,
    size: u32,
) -> HResult {
    let Some(f) = ds_factory(this) else {
        return E_INVALIDARG;
    };
    if size == 0 {
        return E_DSTORAGE_INVALID_STAGING_BUFFER_SIZE;
    }
    *f.staging_buffer_size.lock() = size;
    S_OK
}

static DS_FACTORY_VTBL: DsFactoryVtbl = DsFactoryVtbl {
    slots: [
        vslot(ds_factory_qi as *const c_void),
        vslot(com_add_ref as *const c_void),
        vslot(com_release as *const c_void),
        vslot(ds_factory_create_queue as *const c_void),
        vslot(ds_factory_open_file as *const c_void),
        vslot(ds_factory_create_status_array as *const c_void),
        vslot(ds_factory_set_debug_flags as *const c_void),
        vslot(ds_factory_set_staging_buffer_size as *const c_void),
    ],
};

/// Nombre de slots de réserve après les 8 slots publics de la vtable console.
const DS_X_EXTRA_SLOTS: usize = 24;

#[repr(C)]
struct DsFactoryXVtbl {
    slots: [VtblSlot; 8 + DS_X_EXTRA_SLOTS],
}

/// Méthode de la fabrique console au-delà de `IDStorageFactory` (slot `N`) : layout non public,
/// donc on journalise le slot appelé et on réussit sans effet plutôt que d'échouer. Le titre
/// vérifie le HRESULT de ces appels (ex. slot 10) et piège sur un échec (`int3`), alors qu'aucun
/// état n'est réellement exigé pour lire des fichiers via le VFS.
extern "win64" fn ds_x_unknown_slot<const KIND: u8, const N: usize>(
    this: *mut c_void,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    let object = match KIND {
        0 => "IDStorageFactoryX",
        1 => "IDStorageFileX",
        _ => "IDStorage?X",
    };
    sb_warn!(
        DS_CHANNEL,
        "{object} slot {N} (non publique) appelée (this={this:p}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}) -> S_OK par tolérance"
    );
    S_OK
}

/// Slot 8 de la fabrique console : premier slot au-delà de `IDStorageFactory`.
///
/// **Signature déduite d'un appel réel** (aucune doc publique) : `(this, u32 taille, const void*
/// config, u32 taille_config)` avec `taille` = 0x2000000 (32 MiB, la taille de zone de transit par
/// défaut) et `taille_config` = 0x140. On l'interprète comme « configurer la fabrique » : la taille
/// devient celle de la zone de transit, le bloc de configuration n'est pas lu (son contenu est
/// inconnu et rien ne l'exige pour lire des fichiers via le VFS).
extern "win64" fn ds_x_configure(
    this: *mut DsFactoryObject,
    size: u32,
    config: *const c_void,
    config_size: u32,
) -> HResult {
    ffi_guard!(DS_CHANNEL, "IDStorageFactoryX::slot8", E_UNEXPECTED, {
        let Some(f) = ds_factory(this) else {
            return E_INVALIDARG;
        };
        if size == 0 {
            return E_DSTORAGE_INVALID_STAGING_BUFFER_SIZE;
        }
        *f.staging_buffer_size.lock() = size;
        sb_info!(
            DS_CHANNEL,
            "IDStorageFactoryX::slot8(taille={size:#x}, config={config:p}, taille_config={config_size:#x}) -> S_OK (signature déduite : zone de transit mise à jour, config ignorée)"
        );
        S_OK
    })
}

macro_rules! ds_x_extra_slots {
    ($($n:literal),* $(,)?) => {
        [$(vslot(ds_x_unknown_slot::<0, $n> as *const c_void)),*]
    };
}

/// Vtable de la fabrique console : les 8 slots publics (IUnknown + `IDStorageFactory`, supposés
/// en tête de l'interface console) puis des slots de réserve numérotés 8..32.
static DS_FACTORY_X_VTBL: DsFactoryXVtbl = {
    let extra = ds_x_extra_slots!(
        8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30,
        31
    );
    let base = [
        vslot(ds_factory_qi as *const c_void),
        vslot(com_add_ref as *const c_void),
        vslot(com_release as *const c_void),
        vslot(ds_factory_create_queue as *const c_void),
        vslot(ds_factory_open_file as *const c_void),
        vslot(ds_factory_create_status_array as *const c_void),
        vslot(ds_factory_set_debug_flags as *const c_void),
        vslot(ds_factory_set_staging_buffer_size as *const c_void),
    ];
    let mut slots = [vslot(ds_factory_qi as *const c_void); 8 + DS_X_EXTRA_SLOTS];
    let mut i = 0;
    while i < 8 {
        slots[i] = base[i];
        i += 1;
    }
    let mut j = 0;
    while j < DS_X_EXTRA_SLOTS {
        slots[8 + j] = extra[j];
        j += 1;
    }
    slots[8] = vslot(ds_x_configure as *const c_void);
    slots[10] = vslot(ds_factory_create_queue_x as *const c_void);
    DsFactoryXVtbl { slots }
};

fn new_factory_x(vfs_override: Option<VirtualFs>) -> *mut DsFactoryObject {
    Box::into_raw(Box::new(DsFactoryObject {
        header: ComHeader::new((&raw const DS_FACTORY_X_VTBL).cast()),
        vfs_override,
        staging_buffer_size: Mutex::new(32 * 1024 * 1024),
        debug_flags: Mutex::new(0),
    }))
}

fn new_factory(vfs_override: Option<VirtualFs>) -> *mut DsFactoryObject {
    Box::into_raw(Box::new(DsFactoryObject {
        header: ComHeader::new((&raw const DS_FACTORY_VTBL).cast()),
        vfs_override,
        staging_buffer_size: Mutex::new(32 * 1024 * 1024),
        debug_flags: Mutex::new(0),
    }))
}

/// Fabrique DirectStorage du processus (singleton), adresse stockée en `usize`.
static DS_FACTORY: OnceLock<usize> = OnceLock::new();
/// Fabrique console (vtable étendue), distincte du singleton PC.
static DS_FACTORY_X: OnceLock<usize> = OnceLock::new();

/// `HRESULT DStorageGetFactory(REFIID riid, void** ppv)`.
extern "win64" fn DStorageGetFactory(riid: *const Guid, out: *mut *mut c_void) -> HResult {
    ffi_guard!(DS_CHANNEL, "DStorageGetFactory", E_UNEXPECTED, {
        if out.is_null() {
            return E_POINTER;
        }
        write_out(out, ptr::null_mut());
        // SAFETY: `riid` lu seulement s'il est non-nul.
        if !riid.is_null() && unsafe { *riid } == IID_IDSTORAGE_FACTORY_X {
            let f =
                *DS_FACTORY_X.get_or_init(|| new_factory_x(None) as usize) as *mut DsFactoryObject;
            // SAFETY: singleton vivant pour la durée du processus.
            unsafe { (*f).header.add_ref() };
            write_out(out, f.cast());
            sb_info!(
                DS_CHANNEL,
                "DStorageGetFactory(IDStorageFactoryX) -> fabrique console (slots 0..8 = IDStorageFactory, au-delà : E_NOTIMPL journalisé)"
            );
            return S_OK;
        }
        if !riid_matches(riid, &[IID_IDSTORAGE_FACTORY]) {
            sb_warn!(
                DS_CHANNEL,
                "DStorageGetFactory({}) -> E_NOINTERFACE",
                crate::com::format_guid_opt(riid)
            );
            return E_NOINTERFACE;
        }
        let f = *DS_FACTORY.get_or_init(|| new_factory(None) as usize) as *mut DsFactoryObject;
        // SAFETY: singleton vivant pour la durée du processus.
        unsafe { (*f).header.add_ref() };
        write_out(out, f.cast());
        sb_info!(
            DS_CHANNEL,
            "DStorageGetFactory() -> IDStorageFactory (VFS sandboxé)"
        );
        S_OK
    })
}

/// `HRESULT DStorageSetConfiguration(const DSTORAGE_CONFIGURATION*)` : doit précéder
/// `DStorageGetFactory`. Les options (GPU/BypassIO/...) n'ont pas d'effet sur le backend POSIX.
extern "win64" fn DStorageSetConfiguration(config: *const c_void) -> HResult {
    if config.is_null() {
        return E_INVALIDARG;
    }
    if DS_FACTORY.get().is_some() {
        return E_DSTORAGE_ALREADY_RUNNING;
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn tables_resolve_gameinput_ordinal_and_new_stems() {
        let ords = ordinals_by_dll();
        let gi = ords["gameinput"][&1];
        assert_eq!(
            Some(&gi),
            crate::gdk_xs::gameinput_exports().get("GameInputCreate")
        );
        assert!(ords["d3d12_xs"].contains_key(&13));
        let res = crate::DefaultHleResolver::new();
        use sb_loader::{HleResolver, ImportSymbol};
        assert_eq!(
            res.resolve("GameInput.dll", &ImportSymbol::Ordinal(1)),
            Some(gi)
        );
        assert!(
            res.resolve(
                "XCRDAPI.dll",
                &ImportSymbol::Name("DStorageGetFactory".into())
            )
            .is_some()
        );
        assert!(
            res.resolve(
                "xg_xs.dll",
                &ImportSymbol::Name("XGSuggestSwizzleMode".into())
            )
            .is_some()
        );
        assert!(
            res.resolve("d3d12_xs.dll", &ImportSymbol::Ordinal(13))
                .is_some()
        );
        // GameInputCreate via l'ordinal renvoie un IGameInput.
        // SAFETY: adresse d'une fonction `extern "win64" fn(*mut *mut c_void) -> HRESULT`.
        let create: extern "win64" fn(*mut *mut c_void) -> HResult =
            unsafe { core::mem::transmute(gi) };
        let mut obj = ptr::null_mut();
        assert_eq!(create(&mut obj), S_OK);
        assert!(!obj.is_null());
    }

    fn tex2d(w: u64, h: u32, mips: u16, array: u16, format: u32) -> XgResourceDesc {
        XgResourceDesc {
            dimension: 3,
            width: w,
            height: h,
            depth_or_array_size: array,
            mip_levels: mips,
            format,
            sample_desc: crate::d3d12_xs::DxgiSampleDesc {
                count: 1,
                quality: 0,
            },
            ..Default::default()
        }
    }

    #[test]
    fn xg_suggest_swizzle_and_linear_texture_computer() {
        let desc = tex2d(16, 8, 0, 2, 28);
        assert_eq!(XGSuggestSwizzleMode(&desc), XG_SWIZZLE_MODE_LINEAR);
        assert_eq!(XGSuggestSwizzleMode(ptr::null()), XG_SWIZZLE_MODE_LINEAR);

        let mut comp: *mut c_void = ptr::null_mut();
        assert_eq!(XGCreateTextureComputer(&desc, &mut comp), S_OK);
        let c = comp.cast::<XgComputerObject>();
        // Chaîne complète 16x8 : 5 mips (16x8, 8x4, 4x2, 2x1, 1x1) en RGBA8.
        let slice = (16 * 8 + 8 * 4 + 4 * 2 + 2 + 1) * 4u64;
        assert_eq!(xg_get_size(c), slice * 2);
        assert_eq!(xg_get_mip_offset(c, 0, 0), 0);
        assert_eq!(xg_get_mip_offset(c, 0, 1), 16 * 8 * 4);
        assert_eq!(xg_get_mip_offset(c, 0, 5), XG_INVALID_OFFSET);
        // Mip 0 identique au stockage dense d3d12_xs : y * width * 4 + x * 4.
        assert_eq!(xg_get_texel_offset(c, 0, 0, 3, 2, 0, 0), 2 * 64 + 12);
        assert_eq!(
            xg_get_texel_offset(c, 0, 1, 1, 1, 1, 0),
            slice + 512 + 32 + 4
        );
        assert_eq!(xg_get_texel_offset(c, 0, 0, 16, 0, 0, 0), XG_INVALID_OFFSET);
        let (mut x, mut y, mut z, mut s) = (0u64, 0u32, 0u32, 9u32);
        assert_eq!(
            xg_get_texel_coordinate(
                c,
                0,
                1,
                slice + 512 + 32 + 4,
                &mut x,
                &mut y,
                &mut z,
                &mut s
            ),
            S_OK
        );
        assert_eq!((x, y, z, s), (1, 1, 1, 0));

        let mut layout = XgResourceLayout::default();
        assert_eq!(xg_get_resource_layout(c, &mut layout), S_OK);
        assert_eq!(layout.mip_levels, 5);
        assert_eq!(layout.plane[0].mip_layout[1].pitch_bytes, 32);
        assert_eq!(layout.plane[0].mip_layout[0].size_bytes, 512);

        // Aller-retour CopyInto/CopyFrom sur la sous-ressource (mip 1, slice 1) = index 6.
        let linear: Vec<u8> = (0..64u8).flat_map(|i| [i; 1]).collect::<Vec<_>>();
        let mut padded = vec![0u8; 4 * 64];
        for row in 0..4 {
            padded[row * 64..row * 64 + 32].copy_from_slice(&linear[..32]);
        }
        let mut dense = vec![0u8; xg_get_size(c) as usize];
        assert_eq!(
            xg_copy_into_subresource(
                c,
                dense.as_mut_ptr().cast(),
                0,
                6,
                padded.as_ptr().cast(),
                64,
                0
            ),
            S_OK
        );
        assert_eq!(
            &dense[(slice + 512) as usize..(slice + 512 + 32) as usize],
            &linear[..32]
        );
        let mut back = vec![0u8; 4 * 32];
        assert_eq!(
            xg_copy_from_subresource(
                c,
                back.as_mut_ptr().cast(),
                0,
                6,
                dense.as_ptr().cast(),
                32,
                0
            ),
            S_OK
        );
        assert_eq!(&back[96..128], &linear[..32]);
        assert_eq!(
            xg_copy_into_subresource(
                c,
                dense.as_mut_ptr().cast(),
                0,
                10,
                padded.as_ptr().cast(),
                64,
                0
            ),
            E_INVALIDARG
        );

        // BC1 : blocs 4x4 de 8 octets.
        let bc = tex2d(16, 16, 1, 1, 71);
        let mut cb: *mut c_void = ptr::null_mut();
        assert_eq!(XGCreateTextureComputer(&bc, &mut cb), S_OK);
        assert_eq!(xg_get_size(cb.cast()), 4 * 4 * 8);
        assert_eq!(xg_get_texel_offset(cb.cast(), 0, 0, 5, 9, 0, 0), 2 * 32 + 8);

        // Paramètres invalides.
        let mut bad = tex2d(16, 8, 9, 1, 28);
        assert_eq!(XGCreateTextureComputer(&bad, &mut comp), E_INVALIDARG);
        assert!(comp.is_null());
        bad.mip_levels = 1;
        bad.dimension = 1;
        assert_eq!(XGCreateTextureComputer(&bad, &mut comp), E_INVALIDARG);
        assert_eq!(XGCreateTextureComputer(&desc, ptr::null_mut()), E_POINTER);
    }

    fn temp_vfs() -> (VirtualFs, PathBuf) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("horloge")
            .as_nanos();
        let base = std::env::temp_dir().join(format!("sb-dstorage-{}-{nanos}", std::process::id()));
        let game = base.join("game");
        std::fs::create_dir_all(&game).expect("mkdir");
        let data: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        std::fs::write(game.join("data.bin"), &data).expect("write");
        let vfs = VirtualFs::new(&game, &base.join("save"), &base.join("temp"), None).expect("vfs");
        (vfs, base)
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    #[repr(C)]
    struct TestFence {
        vtbl: *const usize,
        value: std::sync::atomic::AtomicU64,
    }

    extern "win64" fn test_fence_signal(this: *mut TestFence, v: u64) -> HResult {
        // SAFETY: objet de test vivant.
        unsafe { (*this).value.store(v, Ordering::Release) };
        S_OK
    }

    fn wait_for(cond: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    /// L'interface de fichier console est servie par `OpenFile` : les 5 premiers slots sont ceux
    /// d'`IDStorageFile` et le reste échoue proprement ; l'objet reste valide comme source de requête.
    #[test]
    fn dstorage_console_file_interface_is_served() {
        let (vfs, _base) = temp_vfs();
        let factory = new_factory(Some(vfs));
        let path = wide("G:\\data.bin"); // forme Windows, comme les titres Xbox
        let mut file: *mut c_void = ptr::null_mut();
        assert_eq!(
            ds_factory_open_file(factory, path.as_ptr(), &IID_IDSTORAGE_FILE_X, &mut file),
            S_OK
        );
        assert!(!file.is_null());
        assert!(
            ds_file_from_ptr(file as usize).is_some(),
            "valide comme source"
        );

        // SAFETY: objet fraîchement créé, vtable statique de 29 slots.
        unsafe {
            let vtbl = *(file as *const *const usize);
            // Slot 4 : GetFileInformation (IDStorageFile) — taille du fichier de test.
            let get_info: extern "win64" fn(
                *mut c_void,
                *mut crate::file_io::ByHandleFileInformation,
            ) -> HResult = core::mem::transmute(*vtbl.add(4));
            let mut info = crate::file_io::ByHandleFileInformation::default();
            assert_eq!(get_info(file, &mut info), S_OK);
            assert_eq!(info.file_size_low, 4096);
            assert_eq!(info.file_attributes, 0x20);
            // Slot 5 : première méthode non publique → E_NOTIMPL journalisé.
            let slot5: extern "win64" fn(*mut c_void, usize, usize, usize) -> HResult =
                core::mem::transmute(*vtbl.add(5));
            assert_eq!(slot5(file, 0, 0, 0), sb_common::abi::E_NOTIMPL);
        }
        // Un IID inconnu reste refusé.
        let bogus = Guid::new(9, 9, 9, [9; 8]);
        let mut other: *mut c_void = ptr::null_mut();
        assert_eq!(
            ds_factory_open_file(factory, path.as_ptr(), &bogus, &mut other),
            E_NOINTERFACE
        );
        assert!(other.is_null());
    }

    #[test]
    fn dstorage_reads_file_through_vfs_into_memory() {
        let (vfs, base) = temp_vfs();
        let factory = new_factory(Some(vfs));

        // Sécurité VFS : traversal et chemins absolus refusés.
        let mut file: *mut c_void = ptr::null_mut();
        for bad in ["game:/../../etc/passwd", "/etc/passwd", "game:/../data.bin"] {
            let w = wide(bad);
            assert_eq!(
                ds_factory_open_file(factory, w.as_ptr(), &IID_IDSTORAGE_FILE, &mut file),
                E_ACCESSDENIED,
                "{bad}"
            );
            assert!(file.is_null());
        }
        let missing = wide("game:/absent.bin");
        assert_eq!(
            ds_factory_open_file(factory, missing.as_ptr(), &IID_IDSTORAGE_FILE, &mut file),
            HRESULT_FILE_NOT_FOUND
        );
        let path = wide("game:/data.bin");
        assert_eq!(
            ds_factory_open_file(factory, path.as_ptr(), &IID_IDSTORAGE_FILE, &mut file),
            S_OK
        );
        let mut info = crate::file_io::ByHandleFileInformation::default();
        assert_eq!(ds_file_get_info(file.cast(), &mut info), S_OK);
        assert_eq!(info.file_size_low, 4096);

        let qdesc = DStorageQueueDesc {
            source_type: DSTORAGE_REQUEST_SOURCE_FILE,
            capacity: DSTORAGE_MIN_QUEUE_CAPACITY,
            ..Default::default()
        };
        let mut queue: *mut c_void = ptr::null_mut();
        let bad_cap = DStorageQueueDesc {
            capacity: 4,
            ..qdesc
        };
        assert_eq!(
            ds_factory_create_queue(factory, &bad_cap, &IID_IDSTORAGE_QUEUE, &mut queue),
            E_DSTORAGE_INVALID_QUEUE_CAPACITY
        );
        assert_eq!(
            ds_factory_create_queue(factory, &qdesc, &IID_IDSTORAGE_QUEUE, &mut queue),
            S_OK
        );
        let q = queue.cast::<DsQueueObject>();
        let mut status: *mut c_void = ptr::null_mut();
        assert_eq!(
            ds_factory_create_status_array(
                factory,
                2,
                ptr::null(),
                &IID_IDSTORAGE_STATUS_ARRAY,
                &mut status
            ),
            S_OK
        );
        let st = status.cast::<DsStatusArrayObject>();

        let mut dst = vec![0u8; 100];
        let req = DStorageRequest {
            options_bits: DSTORAGE_REQUEST_SOURCE_FILE | (DSTORAGE_DEST_MEMORY << 1),
            source: [file as u64, 300, 100],
            destination: [dst.as_mut_ptr() as u64, 100, 0, 0, 0],
            ..Default::default()
        };
        let vtbl: Box<[usize; 11]> = Box::new({
            let mut v = [0usize; 11];
            v[10] = test_fence_signal as *const () as usize;
            v
        });
        let mut fence = TestFence {
            vtbl: vtbl.as_ptr(),
            value: std::sync::atomic::AtomicU64::new(0),
        };

        ds_queue_enqueue_request(q, &req);
        ds_queue_enqueue_status(q, status, 0);
        ds_queue_enqueue_signal(q, (&raw mut fence).cast(), 7);
        let mut qinfo = DStorageQueueInfo::default();
        ds_queue_query(q, &mut qinfo);
        assert_eq!(qinfo.empty_slot_count, DSTORAGE_MIN_QUEUE_CAPACITY - 3);
        assert_eq!(ds_status_is_complete(st, 0), 0);
        assert_eq!(ds_status_get_hresult(st, 0), E_PENDING);
        ds_queue_submit(q);
        assert!(wait_for(|| fence.value.load(Ordering::Acquire) == 7));
        assert_eq!(ds_status_is_complete(st, 0), 1);
        assert_eq!(ds_status_get_hresult(st, 0), S_OK);
        let expected: Vec<u8> = (300..400).map(|i| (i % 256) as u8).collect();
        assert_eq!(dst, expected);

        // Lecture au-delà de la fin : E_DSTORAGE_END_OF_FILE + enregistrement d'erreur.
        let eof = DStorageRequest {
            source: [file as u64, 4000, 200],
            destination: [dst.as_mut_ptr() as u64, 200, 0, 0, 0],
            ..req
        };
        let ev = ds_queue_get_error_event(q);
        assert_ne!(ev, 0);
        ds_queue_enqueue_request(q, &eof);
        ds_queue_enqueue_status(q, status, 1);
        ds_queue_submit(q);
        assert!(wait_for(|| ds_status_is_complete(st, 1) == 1));
        assert_eq!(ds_status_get_hresult(st, 1), E_DSTORAGE_END_OF_FILE);
        assert_eq!(crate::sync::WaitForSingleObject(ev, 0), 0);
        let mut rec = DStorageErrorRecord::default();
        ds_queue_retrieve_error_record(q, &mut rec);
        assert_eq!(rec.failure_count, 1);
        assert_eq!(rec.first_failure.hresult, E_DSTORAGE_END_OF_FILE);
        assert_eq!(rec.first_failure.command_type, 0);
        let fname: String = char::decode_utf16(
            rec.first_failure
                .request
                .filename
                .iter()
                .copied()
                .take_while(|&c| c != 0),
        )
        .map(|c| c.expect("utf16"))
        .collect();
        assert_eq!(fname, "game:/data.bin");

        // Fichier fermé → E_DSTORAGE_FILE_NOT_OPEN ; file fermée → QUEUE_CLOSED.
        ds_file_close(file.cast());
        ds_queue_enqueue_request(q, &req);
        ds_queue_enqueue_status(q, status, 0);
        ds_queue_submit(q);
        assert!(wait_for(|| ds_status_is_complete(st, 0) == 1));
        assert_eq!(ds_status_get_hresult(st, 0), E_DSTORAGE_FILE_NOT_OPEN);
        ds_queue_close(q);
        ds_queue_enqueue_request(q, &req);
        ds_queue_retrieve_error_record(q, &mut rec);
        assert_eq!(rec.failure_count, 3);

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn dstorage_memory_queue_and_factory_entry_points() {
        let mut f: *mut c_void = ptr::null_mut();
        let bogus = Guid::new(1, 2, 3, [4; 8]);
        assert_eq!(DStorageGetFactory(&bogus, &mut f), E_NOINTERFACE);
        // Fabrique console : accessible, et la vtable étendue échoue proprement au-delà du slot 7.
        let mut fx: *mut c_void = ptr::null_mut();
        assert_eq!(DStorageGetFactory(&IID_IDSTORAGE_FACTORY_X, &mut fx), S_OK);
        assert!(!fx.is_null());
        // SAFETY: objet fraîchement créé, vtable statique de 32 slots.
        unsafe {
            let vtbl = *(fx as *const *const usize);
            let slot20: extern "win64" fn(*mut c_void, usize, usize, usize) -> HResult =
                core::mem::transmute(*vtbl.add(20));
            assert_eq!(slot20(fx, 0, 0, 0), sb_common::abi::E_NOTIMPL);
            // Slot 8 : configuration (signature déduite) → S_OK ; taille nulle refusée.
            let slot8: extern "win64" fn(*mut c_void, u32, *const c_void, u32) -> HResult =
                core::mem::transmute(*vtbl.add(8));
            assert_eq!(slot8(fx, 0x200_0000, ptr::null(), 0x140), S_OK);
            assert_eq!(
                slot8(fx, 0, ptr::null(), 0x140),
                E_DSTORAGE_INVALID_STAGING_BUFFER_SIZE
            );
        }
        assert!(f.is_null());
        assert_eq!(DStorageGetFactory(&IID_IDSTORAGE_FACTORY, &mut f), S_OK);
        assert!(!f.is_null());
        let cfg = [0u8; 28];
        assert_eq!(
            DStorageSetConfiguration(cfg.as_ptr().cast()),
            E_DSTORAGE_ALREADY_RUNNING
        );
        let factory = f.cast::<DsFactoryObject>();
        assert_eq!(
            ds_factory_set_staging_buffer_size(factory, 0),
            E_DSTORAGE_INVALID_STAGING_BUFFER_SIZE
        );
        assert_eq!(ds_factory_set_staging_buffer_size(factory, 1 << 20), S_OK);

        let qdesc = DStorageQueueDesc {
            source_type: DSTORAGE_REQUEST_SOURCE_MEMORY,
            capacity: DSTORAGE_MIN_QUEUE_CAPACITY,
            priority: 1,
            ..Default::default()
        };
        let mut queue: *mut c_void = ptr::null_mut();
        assert_eq!(
            ds_factory_create_queue(factory, &qdesc, &IID_IDSTORAGE_QUEUE2, &mut queue),
            S_OK
        );
        let q = queue.cast::<DsQueueObject>();
        let mut status: *mut c_void = ptr::null_mut();
        assert_eq!(
            ds_factory_create_status_array(
                factory,
                1,
                ptr::null(),
                &IID_IDSTORAGE_STATUS_ARRAY,
                &mut status
            ),
            S_OK
        );
        let src: Vec<u8> = (0..64).collect();
        let mut dst = vec![0u8; 64];
        let req = DStorageRequest {
            options_bits: DSTORAGE_REQUEST_SOURCE_MEMORY | (DSTORAGE_DEST_MEMORY << 1),
            source: [src.as_ptr() as u64, 64, 0],
            destination: [dst.as_mut_ptr() as u64, 64, 0, 0, 0],
            cancellation_tag: 0x10,
            ..Default::default()
        };
        let ev = crate::sync::create_event_handle(true, false);
        ds_queue_enqueue_request(q, &req);
        ds_queue_enqueue_set_event(q, ev);
        ds_queue_submit(q);
        assert_eq!(crate::sync::WaitForSingleObject(ev, 5000), 0);
        assert_eq!(dst, src);

        // Annulation par tag avant soumission → E_ABORT sur le statut.
        let mut dst2 = vec![0u8; 64];
        let req2 = DStorageRequest {
            destination: [dst2.as_mut_ptr() as u64, 64, 0, 0, 0],
            ..req
        };
        ds_queue_enqueue_request(q, &req2);
        ds_queue_enqueue_status(q, status, 0);
        ds_queue_cancel(q, 0xF0, 0x10);
        ds_queue_submit(q);
        let st = status.cast::<DsStatusArrayObject>();
        assert!(wait_for(|| ds_status_is_complete(st, 0) == 1));
        assert_eq!(ds_status_get_hresult(st, 0), E_ABORT);
        assert_eq!(dst2, vec![0u8; 64]);

        // GDeflate : support CPU ; une charge utile qui n'est pas un flux GDeflate → échec propre.
        assert_eq!(
            ds_queue_get_compression_support(q, 1),
            DSTORAGE_COMPRESSION_SUPPORT_CPU_FALLBACK
        );
        assert_eq!(ds_queue_get_compression_support(q, 0), 0);
        assert_eq!(ds_queue_get_compression_support(q, 0x80), 0);
        for format in [1u8, 0x80] {
            let bad = DStorageRequest {
                compression_format: format,
                uncompressed_size: 64,
                ..req
            };
            ds_queue_enqueue_request(q, &bad);
            ds_queue_enqueue_status(q, status, 0);
            ds_queue_submit(q);
            assert!(wait_for(|| ds_status_is_complete(st, 0) == 1));
            assert_eq!(ds_status_get_hresult(st, 0), E_DSTORAGE_DECOMPRESSION_ERROR);
        }
        ds_queue_close(q);
    }

    #[test]
    fn dstorage_gdeflate_file_is_decompressed_into_memory() {
        // Flux GDeflate de référence : 1 Mio de motif « ABCDEFGH » répété.
        let stream: &[u8] = include_bytes!("../tests/data/gdeflate/rep1m.gdf");
        let (vfs, base) = temp_vfs();
        std::fs::write(base.join("game").join("rep.gdf"), stream).expect("write gdf");
        let factory = new_factory(Some(vfs));
        let mut file: *mut c_void = ptr::null_mut();
        let path = wide("game:/rep.gdf");
        assert_eq!(
            ds_factory_open_file(factory, path.as_ptr(), &IID_IDSTORAGE_FILE, &mut file),
            S_OK
        );
        let qdesc = DStorageQueueDesc {
            source_type: DSTORAGE_REQUEST_SOURCE_FILE,
            capacity: DSTORAGE_MIN_QUEUE_CAPACITY,
            ..Default::default()
        };
        let mut queue: *mut c_void = ptr::null_mut();
        assert_eq!(
            ds_factory_create_queue(factory, &qdesc, &IID_IDSTORAGE_QUEUE, &mut queue),
            S_OK
        );
        let q = queue.cast::<DsQueueObject>();
        let mut status: *mut c_void = ptr::null_mut();
        assert_eq!(
            ds_factory_create_status_array(
                factory,
                2,
                ptr::null(),
                &IID_IDSTORAGE_STATUS_ARRAY,
                &mut status
            ),
            S_OK
        );
        let st = status.cast::<DsStatusArrayObject>();
        let mut dst = vec![0u8; 1 << 20];
        let req = DStorageRequest {
            compression_format: DSTORAGE_COMPRESSION_FORMAT_GDEFLATE,
            options_bits: DSTORAGE_REQUEST_SOURCE_FILE | (DSTORAGE_DEST_MEMORY << 1),
            source: [file as u64, 0, stream.len() as u64],
            destination: [dst.as_mut_ptr() as u64, 1 << 20, 0, 0, 0],
            uncompressed_size: 1 << 20,
            ..Default::default()
        };
        ds_queue_enqueue_request(q, &req);
        ds_queue_enqueue_status(q, status, 0);
        ds_queue_submit(q);
        assert!(wait_for(|| ds_status_is_complete(st, 0) == 1));
        assert_eq!(ds_status_get_hresult(st, 0), S_OK);
        assert!(
            dst.iter()
                .enumerate()
                .all(|(i, &b)| b == b"ABCDEFGH"[i % 8])
        );

        // Taille décompressée annoncée incohérente → erreur de décompression, destination intacte.
        let mut dst2 = vec![0u8; 1 << 20];
        let wrong = DStorageRequest {
            destination: [dst2.as_mut_ptr() as u64, 1 << 20, 0, 0, 0],
            uncompressed_size: 1000,
            ..req
        };
        ds_queue_enqueue_request(q, &wrong);
        ds_queue_enqueue_status(q, status, 1);
        ds_queue_submit(q);
        assert!(wait_for(|| ds_status_is_complete(st, 1) == 1));
        assert_eq!(ds_status_get_hresult(st, 1), E_DSTORAGE_DECOMPRESSION_ERROR);
        assert!(dst2.iter().all(|&b| b == 0));

        // Flux tronqué (CompressedSize trop petit) → erreur de décompression.
        let cut = DStorageRequest {
            source: [file as u64, 0, (stream.len() / 2) as u64],
            destination: [dst2.as_mut_ptr() as u64, 1 << 20, 0, 0, 0],
            ..req
        };
        ds_queue_enqueue_request(q, &cut);
        ds_queue_enqueue_status(q, status, 0);
        ds_queue_submit(q);
        assert!(wait_for(
            || ds_status_get_hresult(st, 0) != E_PENDING && ds_status_get_hresult(st, 0) != S_OK
        ));
        assert_eq!(ds_status_get_hresult(st, 0), E_DSTORAGE_DECOMPRESSION_ERROR);
        ds_queue_close(q);
    }
}

#[cfg(test)]
mod gdk_runtime_tests {
    use super::*;

    #[test]
    fn gdk_runtime_entry_points_resolve_and_behave_offline() {
        let t = by_dll();
        let rt = t.get("xgameruntime").expect("xgameruntime");
        for n in [
            "InitializeApiImpl",
            "InitializeApiImplEx",
            "InitializeApiImplEx2",
            "UninitializeApiImpl",
            "QueryApiImpl",
            "XErrorReport",
        ] {
            assert!(rt.contains_key(n), "{n} manquant");
        }
        assert!(t["gameplatformservices"].contains_key("GetApiVersionImpl"));

        assert_eq!(GdkInitializeApiImpl(0x10, 1, 0, 0), S_OK);
        assert_eq!(GdkUninitializeApiImpl(), S_OK);

        let clsid = Guid::new(0x1234_5678, 1, 2, [3; 8]);
        let mut out = ptr::dangling_mut::<c_void>();
        let hr = GdkQueryApiImpl(&clsid, &clsid, &mut out);
        assert_eq!(hr, crate::gdk_classes::E_GDK_CLASS_NOT_PROVIDED);
        assert!(out.is_null());
        assert_eq!(
            GdkQueryApiImpl(ptr::null(), ptr::null(), &mut out),
            E_POINTER
        );

        let (mut a, mut b) = (7u64, 7u64);
        assert_eq!(GdkGetApiVersionImpl(&mut a, &mut b), S_OK);
        assert_eq!(GdkGetApiVersionImpl(ptr::null_mut(), &mut b), E_POINTER);
    }
}

#[cfg(test)]
mod syslib_tests {
    use super::*;

    #[test]
    fn rpcrt4_uuid_roundtrip_and_dbghelp_degrade_cleanly() {
        let t = by_dll();
        assert!(t["rpcrt4"].contains_key("UuidToStringA"));
        assert!(t["dbghelp"].contains_key("StackWalk64"));

        let mut g = Guid::new(0, 0, 0, [0; 8]);
        assert_eq!(UuidCreate(&mut g), RPC_S_OK);
        assert_eq!(g.data3 >> 12, 4, "version 4");
        assert_eq!(g.data4[0] & 0xC0, 0x80, "variante RFC 4122");
        let mut st = -1;
        assert_eq!(UuidIsNil(&g, &mut st), 0);
        assert_eq!(UuidEqual(&g, &g, &mut st), 1);

        let mut s: *mut u8 = ptr::null_mut();
        assert_eq!(UuidToStringA(&g, &mut s), RPC_S_OK);
        // SAFETY: chaîne nul-terminée fraîchement allouée par UuidToStringA.
        let txt = unsafe { std::ffi::CStr::from_ptr(s.cast()) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(txt.len(), 36);
        assert_eq!(txt, txt.to_ascii_lowercase());
        assert_eq!(RpcStringFreeA(&mut s), RPC_S_OK);
        assert!(s.is_null());

        assert_eq!(UuidCreateNil(ptr::null_mut()), RPC_S_INVALID_ARG);
        assert_eq!(SymInitialize(1, 0, 0), 1);
        // `set_last_error` a besoin d'un TEB invité sur le thread de test.
        assert!(sb_loader::init_thread_teb(0).is_ok());
        assert_eq!(SymFail(), 0);
        assert_eq!(sb_loader::last_error(), ERROR_NOT_SUPPORTED_WIN32);
    }
}
