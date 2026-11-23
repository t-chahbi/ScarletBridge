//! `XPackageImpl` (clsid `af406016-e850-4aa8-a88d-2f3dcb9dac7e`) : paquets GDK hors ligne.
//!
//! Vtable : 3 (IUnknown) + 38 méthodes = 41 entrées. Slots 3-34 = `IXPackageImpl` (32 entrées),
//! 35-38 = `IXPackageImpl2`, 39-40 = `IXPackageImpl3`. Les slots 20, 21, 27 et 31 sont des entrées
//! réservées (inconnues dans la doc publique).
//!
//! # Réponses hors ligne
//!
//! * identifiant du paquet courant (32 caractères + NUL) et processus empaqueté (slots 3-4) ;
//! * locale utilisateur (11), dérivée de `LC_ALL`/`LANG` (`en-US` par défaut) ;
//! * tous les contenus sont installés : `XPackageFindChunkAvailability` répond `Ready`,
//!   `XPackageEstimateDownloadSize` répond 0 octet sans confirmation ;
//! * montage (23-26, 37-38) : le seul paquet montable est le paquet courant, dont le chemin invité
//!   est `G:\` (racine du jeu du VFS) ; fermeture du handle en 26 ;
//! * énumérations (paquets 28/35/39, fonctionnalités 33, disponibilité de chunks 13) : vides, sans
//!   rappel ; enregistrements « paquet installé » (29/36/40) : jeton inerte, rappel jamais appelé ;
//! * installation, moniteurs d'installation, ordre d'installation, désinstallation, statistiques
//!   d'écriture : journalisés puis `E_NOTIMPL` ; les slots à retour `BOOLEAN` (8, 10, 34) répondent
//!   faux car aucun moniteur ni installation n'existe.

use core::ffi::c_void;
use std::sync::{LazyLock, OnceLock};

use sb_common::abi::{E_FAIL, E_INVALIDARG, HResult, S_OK};
use sb_common::ffi_guard;

use super::{
    AsyncPayload, CHANNEL, HandleTable, async_check, async_complete, async_take, guid, read_cstr,
    singleton, warn_once, write_cstr_buf, write_out,
};
use crate::com::{
    ComHeader, Guid, VtblSlot, com_add_ref, com_query_interface_list, com_release, vslot,
};
use crate::gdk_classes::ClassDef;
use crate::gdk_xs::XAsyncBlock;

const NAME: &str = "XPackageImpl";
pub(super) const CLSID: Guid = guid("af406016-e850-4aa8-a88d-2f3dcb9dac7e");
/// Identifiant `IXPackageImpl` de l'IDL public (distinct du CLSID).
const IID_IMPL1: Guid = guid("3720de07-e8e4-44a3-ad32-b359e8adbe55");
const IID_IMPL2: Guid = guid("f92d8712-2b27-4d8a-bf01-11a6f8e3eb42");
const IID_IMPL3: Guid = guid("e2a4734b-2f4a-456d-aa8f-d065e04fb209");
const EXTRA: [Guid; 3] = [IID_IMPL1, IID_IMPL2, IID_IMPL3];

/// Nombre d'entrées de la vtable (IUnknown + 38 méthodes).
pub(super) const VTBL_SLOTS: usize = 41;

/// `E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER`.
pub(super) const E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER: HResult = 0x8924_5208_u32 as i32;

/// Identifiant du paquet courant (`XPACKAGE_IDENTIFIER_MAX_LENGTH` = 33 avec le NUL).
pub(super) const PACKAGE_ID: &str = "0000000000000000SCARLETBRIDGE000";
/// Chemin invité du montage du paquet courant : la racine du jeu du VFS.
pub(super) const MOUNT_PATH: &str = "G:\\";

const _: () = assert!(PACKAGE_ID.len() == 32);

type InstalledCb = unsafe extern "win64" fn(*mut c_void, *const c_void);
type EnumCb = unsafe extern "win64" fn(*mut c_void, *const c_void) -> u8;
type ChunkCb = unsafe extern "win64" fn(*mut c_void, *const c_void, u32) -> u8;

static MOUNTS: LazyLock<HandleTable<()>> = LazyLock::new(|| HandleTable::new(0x8A00_0010));
static TOKENS: LazyLock<HandleTable<()>> = LazyLock::new(|| HandleTable::new(0x8B00_0010));

fn is_current_package(p: *const u8) -> Result<(), HResult> {
    match read_cstr(p) {
        None => Err(E_INVALIDARG),
        Some(id) if id == PACKAGE_ID => Ok(()),
        Some(_) => Err(E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER),
    }
}

/// Locale `ll-CC` depuis `LC_ALL`/`LANG` (`fr_FR.UTF-8` → `fr-FR`), `en-US` à défaut.
fn user_locale() -> String {
    let raw = std::env::var("LC_ALL")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("LANG").ok())
        .unwrap_or_default();
    let base = raw.split(['.', '@']).next().unwrap_or("");
    let mut parts = base.split('_');
    match (parts.next(), parts.next()) {
        (Some(l), Some(c))
            if l.len() == 2
                && c.len() == 2
                && l.bytes().all(|b| b.is_ascii_lowercase())
                && c.bytes().all(|b| b.is_ascii_uppercase()) =>
        {
            format!("{l}-{c}")
        }
        _ => "en-US".to_string(),
    }
}

extern "win64" fn qi(this: *mut ComHeader, riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    let mut all = vec![CLSID];
    all.extend_from_slice(&EXTRA);
    com_query_interface_list(NAME, this, riid, ppv, &all)
}

// 3
extern "win64" fn get_current_process_package_identifier(
    _this: *mut c_void,
    size: usize,
    buf: *mut u8,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "XPackageGetCurrentProcessPackageIdentifier",
        E_FAIL,
        {
            if buf.is_null() || size < PACKAGE_ID.len() + 1 {
                return E_INVALIDARG;
            }
            write_cstr_buf(buf, PACKAGE_ID);
            S_OK
        }
    )
}

// 4
extern "win64" fn is_packaged_process(_this: *mut c_void) -> u8 {
    ffi_guard!(CHANNEL, "XPackageIsPackagedProcess", 0u8, { 1u8 })
}

// 8 / 10 / 34 : BOOLEAN faux (aucun moniteur, aucune installation).
extern "win64" fn bool_false<const N: usize>(
    _this: *mut c_void,
    _a2: usize,
    _a3: usize,
    _a4: usize,
) -> u8 {
    ffi_guard!(CHANNEL, "XPackage(bool)", 0u8, {
        warn_once(NAME, N, "BOOLEAN faux hors ligne");
        0u8
    })
}

// 11
extern "win64" fn get_user_locale(_this: *mut c_void, size: usize, buf: *mut u8) -> HResult {
    ffi_guard!(CHANNEL, "XPackageGetUserLocale", E_FAIL, {
        let locale = user_locale();
        if buf.is_null() || size < locale.len() + 1 {
            return E_INVALIDARG;
        }
        write_cstr_buf(buf, &locale);
        S_OK
    })
}

fn check_selectors(count: u32, selectors: *const c_void) -> Result<(), HResult> {
    if count > 0 && selectors.is_null() {
        Err(E_INVALIDARG)
    } else {
        Ok(())
    }
}

// 12 : (packageIdentifier, selectorCount, selectors, &availability) ; tout est « Ready ».
extern "win64" fn find_chunk_availability(
    _this: *mut c_void,
    pkg: *const u8,
    count: u32,
    selectors: *const c_void,
    out: *mut u32,
) -> HResult {
    ffi_guard!(CHANNEL, "XPackageFindChunkAvailability", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        if let Err(hr) = is_current_package(pkg).and_then(|()| check_selectors(count, selectors)) {
            return hr;
        }
        write_out(out, 0); // XPackageChunkAvailability_Ready
        S_OK
    })
}

// 13 : énumération vide.
extern "win64" fn enumerate_chunk_availability(
    _this: *mut c_void,
    pkg: *const u8,
    selector_type: u32,
    _ctx: *mut c_void,
    cb: Option<ChunkCb>,
) -> HResult {
    ffi_guard!(CHANNEL, "XPackageEnumerateChunkAvailability", E_FAIL, {
        if cb.is_none() || selector_type > 3 {
            return E_INVALIDARG;
        }
        is_current_package(pkg).err().unwrap_or(S_OK)
    })
}

// 18 : (packageIdentifier, selectorCount, selectors, &downloadSize, &shouldPresentConfirmation)
extern "win64" fn estimate_download_size(
    _this: *mut c_void,
    pkg: *const u8,
    count: u32,
    selectors: *const c_void,
    size: *mut u64,
    confirm: *mut u8,
) -> HResult {
    ffi_guard!(CHANNEL, "XPackageEstimateDownloadSize", E_FAIL, {
        if size.is_null() {
            return E_INVALIDARG;
        }
        if let Err(hr) = is_current_package(pkg).and_then(|()| check_selectors(count, selectors)) {
            return hr;
        }
        write_out(size, 0);
        write_out(confirm, 0);
        S_OK
    })
}

// 22 : (token, wait) -> BOOLEAN
extern "win64" fn unregister_package_installed(_this: *mut c_void, token: u64, _wait: u8) -> u8 {
    ffi_guard!(CHANNEL, "XPackageUnregisterPackageInstalled", 0u8, {
        u8::from(TOKENS.remove(token as usize).is_some())
    })
}

// 23 : XPackageMount(packageIdentifier, &mount)
extern "win64" fn mount(_this: *mut c_void, pkg: *const u8, out: *mut usize) -> HResult {
    ffi_guard!(CHANNEL, "XPackageMount", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, 0);
        match is_current_package(pkg) {
            Ok(()) => {
                write_out(out, MOUNTS.insert(()));
                S_OK
            }
            Err(hr) => hr,
        }
    })
}

// 24 : taille du chemin, NUL final compris.
extern "win64" fn get_mount_path_size(
    _this: *mut c_void,
    mount: usize,
    out: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XPackageGetMountPathSize", E_FAIL, {
        if out.is_null() || !MOUNTS.contains(mount) {
            return E_INVALIDARG;
        }
        write_out(out, MOUNT_PATH.len() + 1);
        S_OK
    })
}

// 25
extern "win64" fn get_mount_path(
    _this: *mut c_void,
    mount: usize,
    size: usize,
    buf: *mut u8,
) -> HResult {
    ffi_guard!(CHANNEL, "XPackageGetMountPath", E_FAIL, {
        if buf.is_null() || !MOUNTS.contains(mount) || size < MOUNT_PATH.len() + 1 {
            return E_INVALIDARG;
        }
        write_cstr_buf(buf, MOUNT_PATH);
        S_OK
    })
}

// 26
extern "win64" fn close_mount_handle(_this: *mut c_void, mount: usize) {
    ffi_guard!(CHANNEL, "XPackageCloseMountHandle", (), {
        MOUNTS.remove(mount);
    })
}

// 28 / 35 / 39 : (kind, scope, context, callback) ; aucun paquet énuméré.
extern "win64" fn enumerate_packages(
    _this: *mut c_void,
    kind: u32,
    scope: u32,
    _ctx: *mut c_void,
    cb: Option<EnumCb>,
) -> HResult {
    ffi_guard!(CHANNEL, "XPackageEnumeratePackages", E_FAIL, {
        if cb.is_none() || kind > 2 || scope > 2 {
            return E_INVALIDARG;
        }
        S_OK
    })
}

// 29 / 36 / 40 : (queue, context, callback, &token) ; jeton inerte.
extern "win64" fn register_package_installed(
    _this: *mut c_void,
    _queue: usize,
    _ctx: *mut c_void,
    cb: Option<InstalledCb>,
    token: *mut u64,
) -> HResult {
    ffi_guard!(CHANNEL, "XPackageRegisterPackageInstalled", E_FAIL, {
        if cb.is_none() || token.is_null() {
            return E_INVALIDARG;
        }
        write_out(token, TOKENS.insert(()) as u64);
        S_OK
    })
}

// 33 : (packageIdentifier, context, callback) ; aucune fonctionnalité.
extern "win64" fn enumerate_features(
    _this: *mut c_void,
    pkg: *const u8,
    _ctx: *mut c_void,
    cb: Option<EnumCb>,
) -> HResult {
    ffi_guard!(CHANNEL, "XPackageEnumerateFeatures", E_FAIL, {
        if cb.is_none() {
            return E_INVALIDARG;
        }
        is_current_package(pkg).err().unwrap_or(S_OK)
    })
}

// 37 : XPackageMountWithUiAsync(packageIdentifier, async) ; aucune UI hors ligne.
extern "win64" fn mount_with_ui_async(
    _this: *mut c_void,
    pkg: *const u8,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XPackageMountWithUiAsync", E_FAIL, {
        if let Err(hr) =
            async_check(block).and_then(|()| read_cstr(pkg).map(|_| ()).ok_or(E_INVALIDARG))
        {
            return hr;
        }
        let outcome = is_current_package(pkg)
            .map(|()| (size_of::<usize>(), AsyncPayload::Handle(MOUNTS.insert(()))));
        match outcome {
            Err(hr) => async_complete(block, Err(hr)),
            ok => async_complete(block, ok),
        }
    })
}

// 38
extern "win64" fn mount_with_ui_result(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    out: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XPackageMountWithUiResult", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, 0);
        match async_take(block, |p| matches!(p, AsyncPayload::Handle(_))) {
            Ok(rec) => {
                if let AsyncPayload::Handle(h) = rec.payload {
                    write_out(out, h);
                }
                S_OK
            }
            Err(hr) => hr,
        }
    })
}

static VTBL: [VtblSlot; VTBL_SLOTS] = [
    vslot(qi as *const c_void),
    vslot(com_add_ref as *const c_void),
    vslot(com_release as *const c_void),
    vslot(get_current_process_package_identifier as *const c_void), // 3
    vslot(is_packaged_process as *const c_void),                    // 4
    unimpl!(5),
    unimpl!(6),
    unimpl!(7),
    vslot(bool_false::<8> as *const c_void),
    unimpl!(9),
    vslot(bool_false::<10> as *const c_void),
    vslot(get_user_locale as *const c_void),         // 11
    vslot(find_chunk_availability as *const c_void), // 12
    vslot(enumerate_chunk_availability as *const c_void), // 13
    unimpl!(14),
    unimpl!(15),
    unimpl!(16),
    unimpl!(17),
    vslot(estimate_download_size as *const c_void), // 18
    unimpl!(19),
    unimpl!(20),
    unimpl!(21),
    vslot(unregister_package_installed as *const c_void), // 22
    vslot(mount as *const c_void),                        // 23
    vslot(get_mount_path_size as *const c_void),          // 24
    vslot(get_mount_path as *const c_void),               // 25
    vslot(close_mount_handle as *const c_void),           // 26
    unimpl!(27),
    vslot(enumerate_packages as *const c_void), // 28
    vslot(register_package_installed as *const c_void), // 29
    unimpl!(30),
    unimpl!(31),
    unimpl!(32),
    vslot(enumerate_features as *const c_void), // 33
    vslot(bool_false::<34> as *const c_void),
    vslot(enumerate_packages as *const c_void), // 35
    vslot(register_package_installed as *const c_void), // 36
    vslot(mount_with_ui_async as *const c_void), // 37
    vslot(mount_with_ui_result as *const c_void), // 38
    vslot(enumerate_packages as *const c_void), // 39
    vslot(register_package_installed as *const c_void), // 40
];

const _: () = assert!(VTBL.len() == VTBL_SLOTS);

fn create() -> *mut ComHeader {
    static OBJ: OnceLock<usize> = OnceLock::new();
    singleton(&OBJ, VTBL.as_ptr().cast())
}

pub(super) fn class() -> ClassDef {
    ClassDef {
        name: NAME,
        clsid: CLSID,
        extra_iids: &EXTRA,
        create,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests_support::*;
    use super::*;
    use sb_common::abi::{E_NOINTERFACE, E_NOTIMPL};
    use std::ffi::CString;

    #[test]
    fn vtable_size_and_iunknown() {
        const _: () = assert!(VTBL_SLOTS == 41);
        let obj = query_class(&CLSID);
        assert!(vcall!(obj, 1, () -> u32) >= 2);
        assert!(vcall!(obj, 2, () -> u32) >= 1);
        for iid in [CLSID, IID_IMPL1, IID_IMPL2, IID_IMPL3] {
            let mut out: *mut c_void = core::ptr::null_mut();
            assert_eq!(
                vcall!(obj, 0, (*const Guid, *mut *mut c_void) -> HResult, &iid, &mut out),
                S_OK
            );
            vcall!(out, 2, () -> u32);
        }
        let bad = guid("00000000-1111-2222-3333-444444444444");
        let mut out: *mut c_void = core::ptr::null_mut();
        assert_eq!(
            vcall!(obj, 0, (*const Guid, *mut *mut c_void) -> HResult, &bad, &mut out),
            E_NOINTERFACE
        );
        assert_eq!(
            crate::gdk_classes::query(&CLSID, &bad, &mut out),
            E_NOINTERFACE
        );
        assert_eq!(
            crate::gdk_classes::query(&CLSID, &IID_IMPL3, &mut out),
            S_OK
        );
    }

    #[test]
    fn identifier_packaged_and_locale() {
        let obj = query_class(&CLSID);
        let mut buf = [0xAAu8; 40];
        assert_eq!(
            vcall!(obj, 3, (usize, *mut u8) -> HResult, 33, buf.as_mut_ptr()),
            S_OK
        );
        assert_eq!(&buf[..32], PACKAGE_ID.as_bytes());
        assert_eq!(buf[32], 0);
        assert_eq!(
            vcall!(obj, 3, (usize, *mut u8) -> HResult, 32, buf.as_mut_ptr()),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 3, (usize, *mut u8) -> HResult, 33, core::ptr::null_mut()),
            E_INVALIDARG
        );
        assert_eq!(vcall!(obj, 4, () -> u8), 1);

        let mut loc = [0xAAu8; 16];
        assert_eq!(
            vcall!(obj, 11, (usize, *mut u8) -> HResult, 16, loc.as_mut_ptr()),
            S_OK
        );
        let s = std::ffi::CStr::from_bytes_until_nul(&loc).expect("nul");
        let l = s.to_str().expect("utf8");
        assert_eq!(l.len(), 5);
        assert_eq!(l.as_bytes()[2], b'-');
        assert_eq!(
            vcall!(obj, 11, (usize, *mut u8) -> HResult, 3, loc.as_mut_ptr()),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 11, (usize, *mut u8) -> HResult, 16, core::ptr::null_mut()),
            E_INVALIDARG
        );
    }

    #[test]
    fn mount_slots_and_path_consistency() {
        let obj = query_class(&CLSID);
        let id = CString::new(PACKAGE_ID).expect("cstr");
        let other = CString::new("other").expect("cstr");
        let mut m = 5usize;
        assert_eq!(
            vcall!(obj, 23, (*const u8, *mut usize) -> HResult, id.as_ptr().cast(), core::ptr::null_mut()),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 23, (*const u8, *mut usize) -> HResult, core::ptr::null(), &mut m),
            E_INVALIDARG
        );
        assert_eq!(m, 0);
        assert_eq!(
            vcall!(obj, 23, (*const u8, *mut usize) -> HResult, other.as_ptr().cast(), &mut m),
            E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER
        );
        assert_eq!(
            vcall!(obj, 23, (*const u8, *mut usize) -> HResult, id.as_ptr().cast(), &mut m),
            S_OK
        );
        assert_ne!(m, 0);

        let mut size = 0usize;
        assert_eq!(
            vcall!(obj, 24, (usize, *mut usize) -> HResult, m, &mut size),
            S_OK
        );
        assert_eq!(size, 4);
        assert_eq!(
            vcall!(obj, 24, (usize, *mut usize) -> HResult, 0xBAD, &mut size),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 24, (usize, *mut usize) -> HResult, m, core::ptr::null_mut()),
            E_INVALIDARG
        );
        let mut path = vec![0u8; size];
        assert_eq!(
            vcall!(obj, 25, (usize, usize, *mut u8) -> HResult, m, size - 1, path.as_mut_ptr()),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 25, (usize, usize, *mut u8) -> HResult, m, size, path.as_mut_ptr()),
            S_OK
        );
        assert_eq!(&path, b"G:\\\0");
        // Le chemin invité est cohérent avec le VFS (racine du jeu).
        ensure_vfs();
        assert!(crate::file_io::resolve_vfs_path("G:\\").is_some());

        vcall!(obj, 26, (usize) -> (), m);
        assert_eq!(
            vcall!(obj, 24, (usize, *mut usize) -> HResult, m, &mut size),
            E_INVALIDARG
        );

        // Montage avec UI (slots 37-38).
        let mut blk = XAsyncBlock {
            queue: 0,
            context: core::ptr::null_mut(),
            callback: None,
            internal: [0; 32],
        };
        assert_eq!(
            vcall!(obj, 37, (*const u8, *mut XAsyncBlock) -> HResult, core::ptr::null(), &mut blk),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 37, (*const u8, *mut XAsyncBlock) -> HResult, id.as_ptr().cast(), &mut blk),
            S_OK
        );
        let mut h = 0usize;
        assert_eq!(
            vcall!(obj, 38, (*mut XAsyncBlock, *mut usize) -> HResult, &mut blk, core::ptr::null_mut()),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 38, (*mut XAsyncBlock, *mut usize) -> HResult, &mut blk, &mut h),
            S_OK
        );
        assert_eq!(
            vcall!(obj, 24, (usize, *mut usize) -> HResult, h, &mut size),
            S_OK
        );
        // Paquet inconnu : l'échec est rendu par Result.
        assert_eq!(
            vcall!(obj, 37, (*const u8, *mut XAsyncBlock) -> HResult, other.as_ptr().cast(), &mut blk),
            S_OK
        );
        assert_eq!(
            vcall!(obj, 38, (*mut XAsyncBlock, *mut usize) -> HResult, &mut blk, &mut h),
            E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER
        );
    }

    #[test]
    fn enumerations_are_empty_and_registrations_inert() {
        let obj = query_class(&CLSID);
        extern "win64" fn never_enum(_: *mut c_void, _: *const c_void) -> u8 {
            panic!("énumération vide attendue");
        }
        for n in [28usize, 35, 39] {
            assert_eq!(
                vcall!(obj, n, (u32, u32, *mut c_void, Option<EnumCb>) -> HResult, 0, 1, core::ptr::null_mut(), Some(never_enum as EnumCb)),
                S_OK,
                "slot {n}"
            );
            assert_eq!(
                vcall!(obj, n, (u32, u32, *mut c_void, Option<EnumCb>) -> HResult, 0, 1, core::ptr::null_mut(), None),
                E_INVALIDARG
            );
            assert_eq!(
                vcall!(obj, n, (u32, u32, *mut c_void, Option<EnumCb>) -> HResult, 3, 1, core::ptr::null_mut(), Some(never_enum as EnumCb)),
                E_INVALIDARG
            );
        }
        let id = CString::new(PACKAGE_ID).expect("cstr");
        assert_eq!(
            vcall!(obj, 33, (*const u8, *mut c_void, Option<EnumCb>) -> HResult, id.as_ptr().cast(), core::ptr::null_mut(), Some(never_enum as EnumCb)),
            S_OK
        );
        assert_eq!(
            vcall!(obj, 33, (*const u8, *mut c_void, Option<EnumCb>) -> HResult, core::ptr::null(), core::ptr::null_mut(), Some(never_enum as EnumCb)),
            E_INVALIDARG
        );

        extern "win64" fn never_installed(_: *mut c_void, _: *const c_void) {
            panic!("jamais appelé");
        }
        for n in [29usize, 36, 40] {
            let mut tok = 0u64;
            assert_eq!(
                vcall!(obj, n, (usize, *mut c_void, Option<InstalledCb>, *mut u64) -> HResult, 0, core::ptr::null_mut(), Some(never_installed as InstalledCb), &mut tok),
                S_OK
            );
            assert_ne!(tok, 0);
            assert_eq!(vcall!(obj, 22, (u64, u8) -> u8, tok, 1), 1);
            assert_eq!(vcall!(obj, 22, (u64, u8) -> u8, tok, 1), 0);
            assert_eq!(
                vcall!(obj, n, (usize, *mut c_void, Option<InstalledCb>, *mut u64) -> HResult, 0, core::ptr::null_mut(), None, &mut tok),
                E_INVALIDARG
            );
            assert_eq!(
                vcall!(obj, n, (usize, *mut c_void, Option<InstalledCb>, *mut u64) -> HResult, 0, core::ptr::null_mut(), Some(never_installed as InstalledCb), core::ptr::null_mut()),
                E_INVALIDARG
            );
        }
    }

    #[test]
    fn chunks_are_ready_and_nothing_to_download() {
        let obj = query_class(&CLSID);
        let id = CString::new(PACKAGE_ID).expect("cstr");
        let mut av = 9u32;
        assert_eq!(
            vcall!(obj, 12, (*const u8, u32, *const c_void, *mut u32) -> HResult, id.as_ptr().cast(), 0, core::ptr::null(), &mut av),
            S_OK
        );
        assert_eq!(av, 0);
        assert_eq!(
            vcall!(obj, 12, (*const u8, u32, *const c_void, *mut u32) -> HResult, id.as_ptr().cast(), 2, core::ptr::null(), &mut av),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 12, (*const u8, u32, *const c_void, *mut u32) -> HResult, id.as_ptr().cast(), 0, core::ptr::null(), core::ptr::null_mut()),
            E_INVALIDARG
        );
        let (mut size, mut confirm) = (7u64, 7u8);
        assert_eq!(
            vcall!(obj, 18, (*const u8, u32, *const c_void, *mut u64, *mut u8) -> HResult, id.as_ptr().cast(), 0, core::ptr::null(), &mut size, &mut confirm),
            S_OK
        );
        assert_eq!((size, confirm), (0, 0));
        assert_eq!(
            vcall!(obj, 18, (*const u8, u32, *const c_void, *mut u64, *mut u8) -> HResult, core::ptr::null(), 0, core::ptr::null(), &mut size, &mut confirm),
            E_INVALIDARG
        );
        extern "win64" fn never_chunk(_: *mut c_void, _: *const c_void, _: u32) -> u8 {
            panic!("énumération vide attendue");
        }
        assert_eq!(
            vcall!(obj, 13, (*const u8, u32, *mut c_void, Option<ChunkCb>) -> HResult, id.as_ptr().cast(), 0, core::ptr::null_mut(), Some(never_chunk as ChunkCb)),
            S_OK
        );
        assert_eq!(
            vcall!(obj, 13, (*const u8, u32, *mut c_void, Option<ChunkCb>) -> HResult, id.as_ptr().cast(), 9, core::ptr::null_mut(), Some(never_chunk as ChunkCb)),
            E_INVALIDARG
        );
    }

    #[test]
    fn install_related_slots_are_not_implemented() {
        let obj = query_class(&CLSID);
        for n in [5usize, 9, 14, 15, 16, 17, 19, 20, 21, 27, 30, 31, 32] {
            assert_eq!(vcall!(obj, n, (usize) -> HResult, 0), E_NOTIMPL, "slot {n}");
        }
        for n in [8usize, 10, 34] {
            assert_eq!(vcall!(obj, n, (usize) -> u8, 0), 0, "slot {n}");
        }
    }
}
