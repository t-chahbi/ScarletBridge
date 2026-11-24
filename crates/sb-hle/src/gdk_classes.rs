//! Classes COM du runtime GDK (`xgameruntime.dll!QueryApiImpl`).
//!
//! Les titres GDK liés statiquement à `xgameruntime.lib` n'appellent pas les fonctions `X*` par
//! import : chaque thunk appelle `QueryApiImpl(clsid, iid, &obj)`, puis une méthode de la vtable
//! de l'objet (`this` suivi des arguments de la fonction), puis `Release` (slot 2). Les méthodes
//! d'une classe suivent l'ordre des fonctions publiques de l'interface (`IXxxImpl`, versions
//! `IXxxImplN` qui ajoutent des slots en fin de vtable).
//!
//! Ce module fournit le registre ; chaque famille de classes vit dans un sous-module qui expose
//! `classes() -> Vec<ClassDef>`. Un slot dont le comportement n'est pas implémenté utilise
//! [`unimplemented_slot`] : il journalise la classe et le numéro de slot puis renvoie
//! `E_NOTIMPL`, sans jamais lire hors vtable.

/// Slot de vtable non implémenté : `unimpl!(N)` → `VtblSlot` qui journalise « classe::slot N ».
macro_rules! unimpl {
    ($n:literal) => {
        $crate::com::vslot(
            $crate::gdk_classes::unimplemented_slot::<$n> as *const core::ffi::c_void,
        )
    };
}

pub mod rt_core;
pub mod rt_net;
pub mod rt_offline;
pub mod rt_save_store;
pub mod rt_user;

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;
use sb_common::abi::{E_NOINTERFACE, E_NOTIMPL, E_POINTER, HResult};
use sb_common::{sb_debug, sb_warn};

use crate::com::{ComHeader, Guid, IID_IUNKNOWN};

const CHANNEL: &str = "gdk_classes";

/// `HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED)` : erreur documentée pour une classe non fournie.
pub const E_GDK_CLASS_NOT_PROVIDED: HResult = 0x8007_0032_u32 as i32;

/// Description d'une classe du runtime GDK.
#[derive(Clone, Copy)]
pub struct ClassDef {
    /// Nom lisible (`"XThreadingImpl"`), utilisé dans les journaux.
    pub name: &'static str,
    /// CLSID ; c'est aussi l'IID de l'interface de base (`IXxxImpl`).
    pub clsid: Guid,
    /// IID supplémentaires acceptés (`IXxxImpl2`…`N` : même objet, vtable plus longue).
    pub extra_iids: &'static [Guid],
    /// Retourne l'objet (singleton recommandé). `ComHeader::ref_count` vaut ≥ 1 ; le registre
    /// appelle `add_ref` avant de le rendre à l'appelant.
    pub create: fn() -> *mut ComHeader,
}

fn registry() -> &'static [ClassDef] {
    static REG: OnceLock<Vec<ClassDef>> = OnceLock::new();
    REG.get_or_init(|| {
        let mut v = Vec::new();
        v.extend(rt_core::classes());
        v.extend(rt_user::classes());
        v.extend(rt_net::classes());
        v.extend(rt_offline::classes());
        v.extend(rt_save_store::classes());
        v
    })
}

/// Objets créés → nom de classe (pour nommer la classe dans [`unimplemented_slot`]).
fn live_objects() -> &'static Mutex<HashMap<usize, &'static str>> {
    static LIVE: OnceLock<Mutex<HashMap<usize, &'static str>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// CLSID des classes documentées du runtime GDK `(nom, CLSID)`, y compris celles que le bridge ne
/// fournit pas encore : sert à repérer, par analyse statique, les classes qu'un binaire référence.
pub const KNOWN_GDK_CLASSES: &[(&str, Guid)] = &[
    (
        "XAccessibility",
        Guid::new(
            0x3e24_1977,
            0x6237,
            0x41e9,
            [0x85, 0x59, 0x10, 0x2c, 0x2d, 0x99, 0x83, 0xf1],
        ),
    ),
    (
        "XAppCapture",
        Guid::new(
            0xa4f1_aee2,
            0x4bf1,
            0x4485,
            [0xb0, 0x08, 0xa7, 0xc2, 0x6d, 0x52, 0xac, 0x27],
        ),
    ),
    (
        "XThreading / XAsync",
        Guid::new(
            0x073b_7dcb,
            0x1fcf,
            0x4030,
            [0x94, 0xbe, 0xe3, 0xc9, 0xeb, 0x62, 0x34, 0x28],
        ),
    ),
    (
        "XGame",
        Guid::new(
            0x973a_344e,
            0x24bf,
            0x4d0f,
            [0x84, 0x57, 0x56, 0xc5, 0x34, 0x89, 0x2b, 0x29],
        ),
    ),
    (
        "XGameInvite",
        Guid::new(
            0x0651_aae2,
            0x4012,
            0x4077,
            [0xbf, 0x84, 0x8b, 0x90, 0x97, 0x09, 0x0e, 0x2c],
        ),
    ),
    (
        "XGameSave",
        Guid::new(
            0x704c_3f58,
            0xe629,
            0x4cc2,
            [0xb1, 0x97, 0x30, 0x51, 0x1b, 0x99, 0x6f, 0xe2],
        ),
    ),
    (
        "XGameUI",
        Guid::new(
            0xdfcd_4649,
            0x4ff8,
            0x4043,
            [0xba, 0x07, 0x35, 0xd6, 0x07, 0xdf, 0x98, 0xb0],
        ),
    ),
    (
        "XLauncher",
        Guid::new(
            0x1b33_9674,
            0x328d,
            0x4283,
            [0xa2, 0x00, 0x31, 0x71, 0xf1, 0x8d, 0x36, 0x39],
        ),
    ),
    (
        "XNetworking",
        Guid::new(
            0x37e5_6907,
            0x2f10,
            0x41e8,
            [0xb7, 0x2f, 0x36, 0xed, 0xb1, 0x85, 0x33, 0x1a],
        ),
    ),
    (
        "XPackage",
        Guid::new(
            0xaf40_6016,
            0xe850,
            0x4aa8,
            [0xa8, 0x8d, 0x2f, 0x3d, 0xcb, 0x9d, 0xac, 0x7e],
        ),
    ),
    (
        "XPersistentLocalStorage",
        Guid::new(
            0xf4fa_f4d4,
            0x2d04,
            0x4fce,
            [0xb3, 0xe0, 0x47, 0x4a, 0x71, 0x3a, 0x3e, 0x84],
        ),
    ),
    (
        "XStore",
        Guid::new(
            0x0dd1_12ac,
            0x7c24,
            0x448c,
            [0xb9, 0x2b, 0x39, 0x60, 0xfb, 0x5b, 0xd3, 0x0c],
        ),
    ),
    (
        "XSystem",
        Guid::new(
            0xe349_bd1a,
            0xfc20,
            0x4e40,
            [0xb9, 0x9c, 0x41, 0x78, 0xcc, 0x6b, 0x40, 0x9f],
        ),
    ),
    (
        "XUser",
        Guid::new(
            0x01ac_d177,
            0x91f9,
            0x4763,
            [0xa3, 0x8e, 0xcc, 0xbb, 0x55, 0xce, 0x32, 0xe0],
        ),
    ),
    (
        "XError",
        Guid::new(
            0x8ca4_67f7,
            0x22e8,
            0x4096,
            [0x84, 0x56, 0xbb, 0x8a, 0xa1, 0x3f, 0x79, 0xd8],
        ),
    ),
    (
        "XGameEvent",
        Guid::new(
            0xbbfb_dcc7,
            0xbfe7,
            0x409b,
            [0xa5, 0xca, 0xed, 0xf0, 0x54, 0x96, 0x0b, 0x4d],
        ),
    ),
    (
        "XGameStreaming",
        Guid::new(
            0x0a21_92aa,
            0xb2d5,
            0x4d58,
            [0x83, 0xbe, 0x38, 0x3b, 0x6d, 0x80, 0x79, 0x9e],
        ),
    ),
];

/// Vrai si `QueryApiImpl` fournit cette classe (utile à `XGameRuntimeFeature`).
pub fn is_class_served(clsid: &Guid) -> bool {
    registry().iter().any(|c| c.clsid == *clsid)
}

/// Noms et CLSID des classes enregistrées (diagnostic, tests).
pub fn served_classes() -> Vec<(&'static str, Guid)> {
    registry().iter().map(|c| (c.name, c.clsid)).collect()
}

/// Corps de `QueryApiImpl(const GUID* clsid, const GUID* iid, void** out)`.
///
/// Fonction sûre malgré ses pointeurs bruts : c'est le point d'entrée FFI (pointeurs de l'appelant
/// invité). `clsid`, `iid` et `out` sont testés pour NULL avant toute lecture/écriture et les GUID
/// sont lus sans exigence d'alignement ; l'appelant invité garantit que les non-NULL sont valides.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn query(clsid: *const Guid, iid: *const Guid, out: *mut *mut c_void) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: `out` vérifié non-nul.
    unsafe { *out = core::ptr::null_mut() };
    if clsid.is_null() {
        return E_POINTER;
    }
    // SAFETY: `clsid` non-nul ; lecture non alignée d'un GUID.
    let class_id = unsafe { core::ptr::read_unaligned(clsid) };
    let Some(def) = registry().iter().find(|c| c.clsid == class_id) else {
        sb_warn!(
            CHANNEL,
            "QueryApiImpl(classe={class_id}, interface={}) -> classe non fournie (0x{:08X})",
            crate::com::format_guid_opt(iid),
            E_GDK_CLASS_NOT_PROVIDED as u32
        );
        return E_GDK_CLASS_NOT_PROVIDED;
    };
    // L'IID peut être NULL (traité comme IUnknown).
    let wanted = if iid.is_null() {
        IID_IUNKNOWN
    } else {
        // SAFETY: `iid` non-nul.
        unsafe { core::ptr::read_unaligned(iid) }
    };
    if wanted != IID_IUNKNOWN && wanted != def.clsid && !def.extra_iids.contains(&wanted) {
        sb_warn!(
            CHANNEL,
            "QueryApiImpl({}, interface={wanted}) -> interface non fournie (E_NOINTERFACE)",
            def.name
        );
        return E_NOINTERFACE;
    }
    let obj = (def.create)();
    if obj.is_null() {
        return E_POINTER;
    }
    // SAFETY: `obj` vient de `create()` : objet commençant par un `ComHeader`.
    unsafe { (*obj).add_ref() };
    live_objects().lock().insert(obj as usize, def.name);
    // SAFETY: `out` non-nul.
    unsafe { *out = obj.cast() };
    sb_debug!(CHANNEL, "QueryApiImpl({}) -> {obj:p}", def.name);
    0
}

/// Slot non implémenté `N` : journalise « classe::slot N » (une fois par couple) et `E_NOTIMPL`.
pub extern "win64" fn unimplemented_slot<const N: usize>(
    this: *mut c_void,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    static SEEN: OnceLock<Mutex<std::collections::HashSet<(usize, usize)>>> = OnceLock::new();
    let class = live_objects()
        .lock()
        .get(&(this as usize))
        .copied()
        .unwrap_or("classe GDK inconnue");
    let key = (class.as_ptr() as usize, N);
    if SEEN
        .get_or_init(|| Mutex::new(Default::default()))
        .lock()
        .insert(key)
    {
        sb_warn!(
            CHANNEL,
            "{class}::slot {N} non implémenté appelé (a2={}, a3={}, a4={}) -> E_NOTIMPL",
            crate::com::describe_guest_arg(a2),
            crate::com::describe_guest_arg(a3),
            crate::com::describe_guest_arg(a4)
        );
    }
    E_NOTIMPL
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Les trois demandes `QueryApiImpl` d'un titre GDK au démarrage (classe, interface).
    #[test]
    fn startup_queries_of_a_gdk_title_are_served() {
        let threading = Guid::new(
            0x073B_7DCB,
            0x1FCF,
            0x4030,
            [0x94, 0xBE, 0xE3, 0xC9, 0xEB, 0x62, 0x34, 0x28],
        );
        let networking = Guid::new(
            0x37E5_6907,
            0x2F10,
            0x41E8,
            [0xB7, 0x2F, 0x36, 0xED, 0xB1, 0x85, 0x33, 0x1A],
        );
        let networking2 = Guid::new(
            0xBF23_46B2,
            0x39AF,
            0x4658,
            [0xB5, 0xEA, 0x44, 0x71, 0x3C, 0x7E, 0x83, 0xB3],
        );
        let user = Guid::new(
            0x01AC_D177,
            0x91F9,
            0x4763,
            [0xA3, 0x8E, 0xCC, 0xBB, 0x55, 0xCE, 0x32, 0xE0],
        );
        for (name, clsid, iid) in [
            ("XThreadingImpl", threading, threading),
            ("XNetworkingImpl", networking, networking2),
            ("XUserImpl", user, user),
        ] {
            let mut obj: *mut c_void = core::ptr::null_mut();
            assert_eq!(query(&clsid, &iid, &mut obj), 0, "{name}");
            assert!(!obj.is_null(), "{name}");
            assert!(is_class_served(&clsid), "{name}");
        }
        // Classe inconnue : erreur documentée, sortie mise à NULL.
        let unknown = Guid::new(1, 2, 3, [4; 8]);
        let mut obj = core::ptr::dangling_mut::<c_void>();
        assert_eq!(
            query(&unknown, &unknown, &mut obj),
            E_GDK_CLASS_NOT_PROVIDED
        );
        assert!(obj.is_null());
        assert!(served_classes().len() >= 15);
    }
}
