//! Classe du runtime GDK : `XNetworkingImpl` (`IXNetworkingImpl2`, slots 3..20), hors ligne strict.
//!
//! Le titre est toujours sans connectivité : le niveau de connectivité est « None », les requêtes
//! de port et d'informations de sécurité d'URL échouent avec
//! `HRESULT_FROM_WIN32(ERROR_NETWORK_UNREACHABLE)` (les variantes asynchrones se terminent avec
//! cette erreur via `XAsyncBlock`), et aucun socket n'est jamais ouvert.

#![allow(clippy::too_many_arguments)]

use core::ffi::c_void;
use std::sync::OnceLock;

use sb_common::abi::{E_FAIL, E_INVALIDARG, HResult, S_OK};
use sb_common::ffi_guard;

use super::ClassDef;
use super::rt_user::{
    E_NETWORK_UNREACHABLE, complete_async, drop_token, guid, new_token, peek_done, qi_fn,
    read_cstr, read_wstr, singleton, submit_completion, take_done, token_is_live, write_out,
};
use crate::com::{ComHeader, Guid, VtblSlot, com_add_ref, com_release, vslot};
use crate::gdk_xs::XAsyncBlock;

const CHANNEL: &str = "gdk_net";

const CLSID_XNETWORKING: Guid = guid("37e56907-2f10-41e8-b72f-36edb185331a");
const IID_XNETWORKING2: Guid = guid("bf2346b2-39af-4658-b5ea-44713c7e83b3");

const NET_SLOT_COUNT: usize = 21;

/// `XNetworkingConnectivityHint` (16 octets).
#[repr(C)]
#[derive(Clone, Copy)]
struct ConnectivityHint {
    connectivity_level: u32,
    connectivity_cost: u32,
    iana_interface_type: u32,
    network_initialized: u8,
    approaching_data_limit: u8,
    over_data_limit: u8,
    roaming: u8,
}
const _: () = assert!(core::mem::size_of::<ConnectivityHint>() == 16);
const _: () = assert!(core::mem::offset_of!(ConnectivityHint, iana_interface_type) == 8);
const _: () = assert!(core::mem::offset_of!(ConnectivityHint, network_initialized) == 12);

/// `XNetworkingConnectivityLevelHint::None`.
const LEVEL_NONE: u32 = 1;
/// `XNetworkingConnectivityCostHint::Unknown`.
const COST_UNKNOWN: u32 = 0;

/// Indice hors ligne : réseau initialisé, aucune connectivité.
static OFFLINE_HINT: ConnectivityHint = ConnectivityHint {
    connectivity_level: LEVEL_NONE,
    connectivity_cost: COST_UNKNOWN,
    iana_interface_type: 0,
    network_initialized: 1,
    approaching_data_limit: 0,
    over_data_limit: 0,
    roaming: 0,
};

/// `XNetworkingTcpQueuedReceivedBufferUsageStatistics` : 5 compteurs 64 bits (= l'union
/// `XNetworkingStatisticsBuffer`).
const STATS_BUFFER_SIZE: usize = 40;

qi_fn!(
    net_qi,
    "XNetworkingImpl",
    &[CLSID_XNETWORKING, IID_XNETWORKING2]
);

// -- port UDP multijoueur préféré (slots 3..7) -------------------------------------------------

extern "win64" fn net_query_port(_this: *mut c_void, out: *mut u16) -> HResult {
    ffi_guard!(
        CHANNEL,
        "XNetworkingQueryPreferredLocalUdpMultiplayerPort",
        E_FAIL,
        {
            if out.is_null() {
                return E_INVALIDARG;
            }
            write_out(out, 0u16);
            E_NETWORK_UNREACHABLE
        }
    )
}

extern "win64" fn net_query_port_async(_this: *mut c_void, block: *mut XAsyncBlock) -> HResult {
    ffi_guard!(
        CHANNEL,
        "XNetworkingQueryPreferredLocalUdpMultiplayerPortAsync",
        E_FAIL,
        {
            if block.is_null() {
                return E_INVALIDARG;
            }
            complete_async(block, E_NETWORK_UNREACHABLE, 0);
            S_OK
        }
    )
}

extern "win64" fn net_query_port_async_result(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    out: *mut u16,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "XNetworkingQueryPreferredLocalUdpMultiplayerPortAsyncResult",
        E_FAIL,
        {
            if block.is_null() || out.is_null() {
                return E_INVALIDARG;
            }
            write_out(out, 0u16);
            match take_done(block) {
                Some(d) => d.hr,
                None => E_INVALIDARG,
            }
        }
    )
}

extern "win64" fn net_register_port_changed(
    _this: *mut c_void,
    _queue: usize,
    _context: *mut c_void,
    callback: usize,
    token: *mut u64,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "XNetworkingRegisterPreferredLocalUdpMultiplayerPortChanged",
        E_FAIL,
        {
            if callback == 0 || token.is_null() {
                return E_INVALIDARG;
            }
            // Le port ne change jamais hors ligne : jeton inerte.
            write_out(token, new_token());
            S_OK
        }
    )
}

extern "win64" fn net_unregister(_this: *mut c_void, token: u64, _wait: u8) -> u8 {
    ffi_guard!(CHANNEL, "XNetworkingUnregister...Changed", 0, {
        drop_token(token);
        1 // rien en cours
    })
}

// -- informations de sécurité d'URL (slots 8..13) ----------------------------------------------

extern "win64" fn net_url_async(
    _this: *mut c_void,
    url: *const u8,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "XNetworkingQuerySecurityInformationForUrlAsync",
        E_FAIL,
        {
            if block.is_null() || url.is_null() {
                return E_INVALIDARG;
            }
            sb_common::sb_debug!(
                CHANNEL,
                "QuerySecurityInformationForUrl({}) -> réseau injoignable",
                read_cstr(url).unwrap_or_default()
            );
            complete_async(block, E_NETWORK_UNREACHABLE, 0);
            S_OK
        }
    )
}

extern "win64" fn net_url_utf16_async(
    _this: *mut c_void,
    url: *const u16,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "XNetworkingQuerySecurityInformationForUrlUtf16Async",
        E_FAIL,
        {
            if block.is_null() || url.is_null() {
                return E_INVALIDARG;
            }
            sb_common::sb_debug!(
                CHANNEL,
                "QuerySecurityInformationForUrlUtf16({}) -> réseau injoignable",
                read_wstr(url).unwrap_or_default()
            );
            complete_async(block, E_NETWORK_UNREACHABLE, 0);
            S_OK
        }
    )
}

extern "win64" fn net_url_result_size(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    out: *mut usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "XNetworkingQuerySecurityInformationForUrlAsyncResultSize",
        E_FAIL,
        {
            if block.is_null() || out.is_null() {
                return E_INVALIDARG;
            }
            write_out(out, 0usize);
            match peek_done(block) {
                Some(d) => d.hr,
                None => E_INVALIDARG,
            }
        }
    )
}

extern "win64" fn net_url_result(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    size: usize,
    used: *mut usize,
    buffer: *mut u8,
    info: *mut *mut c_void,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "XNetworkingQuerySecurityInformationForUrlAsyncResult",
        E_FAIL,
        {
            if block.is_null() || info.is_null() || (buffer.is_null() && size != 0) {
                return E_INVALIDARG;
            }
            write_out(used, 0usize);
            write_out(info, core::ptr::null_mut::<c_void>());
            match take_done(block) {
                Some(d) => d.hr,
                None => E_INVALIDARG,
            }
        }
    )
}

// -- certificat, connectivité (slots 14..17) ---------------------------------------------------

extern "win64" fn net_verify_server_certificate(
    _this: *mut c_void,
    request: *mut c_void,
    info: *const c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "XNetworkingVerifyServerCertificate", E_FAIL, {
        if request.is_null() || info.is_null() {
            return E_INVALIDARG;
        }
        // Aucune connexion n'existe hors ligne : le certificat ne peut pas être validé.
        E_NETWORK_UNREACHABLE
    })
}

extern "win64" fn net_get_connectivity_hint(
    _this: *mut c_void,
    out: *mut ConnectivityHint,
) -> HResult {
    ffi_guard!(CHANNEL, "XNetworkingGetConnectivityHint", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, OFFLINE_HINT);
        S_OK
    })
}

struct HintNotification {
    token: u64,
    context: usize,
    callback: usize,
}

extern "win64" fn hint_trampoline(ctx: *mut c_void, _canceled: u8) {
    ffi_guard!(CHANNEL, "hint_trampoline", (), {
        // SAFETY: `ctx` vient de `Box::into_raw` dans `net_register_hint_changed` et n'est
        // consommé qu'ici (une exécution par soumission).
        let n = unsafe { Box::from_raw(ctx.cast::<HintNotification>()) };
        if !token_is_live(n.token) {
            return; // désinscrit avant la livraison
        }
        // SAFETY: `callback` est le `XNetworkingConnectivityHintChangedCallback` non nul fourni
        // par le titre : `void(void* context, const XNetworkingConnectivityHint*)`.
        let cb: unsafe extern "win64" fn(*mut c_void, *const ConnectivityHint) =
            unsafe { core::mem::transmute(n.callback) };
        // SAFETY: `OFFLINE_HINT` est une donnée statique valide pendant l'appel.
        unsafe { cb(n.context as *mut c_void, &OFFLINE_HINT) };
    })
}

extern "win64" fn net_register_hint_changed(
    _this: *mut c_void,
    queue: usize,
    context: *mut c_void,
    callback: usize,
    token: *mut u64,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "XNetworkingRegisterConnectivityHintChanged",
        E_FAIL,
        {
            if callback == 0 || token.is_null() {
                return E_INVALIDARG;
            }
            write_out(token, 0u64);
            let t = new_token();
            let note = Box::into_raw(Box::new(HintNotification {
                token: t,
                context: context as usize,
                callback,
            }));
            // Une seule notification initiale sur le port de complétion de la file donnée.
            let hr = submit_completion(queue, note.cast(), hint_trampoline as *const () as usize);
            if hr < 0 {
                // SAFETY: la soumission a échoué : `note` n'a pas été consommé.
                drop(unsafe { Box::from_raw(note) });
                drop_token(t);
                return if queue != 0 { E_INVALIDARG } else { hr };
            }
            write_out(token, t);
            S_OK
        }
    )
}

// -- ajouts de IXNetworkingImpl2 (slots 18..20) ------------------------------------------------

extern "win64" fn net_query_statistics(_this: *mut c_void, kind: u32, buffer: *mut u8) -> HResult {
    ffi_guard!(CHANNEL, "XNetworkingQueryStatistics", E_FAIL, {
        if buffer.is_null() || kind > 2 {
            return E_INVALIDARG;
        }
        // Rien n'est jamais reçu ni mis en file : tous les compteurs valent zéro.
        write_out(
            buffer.cast::<[u8; STATS_BUFFER_SIZE]>(),
            [0u8; STATS_BUFFER_SIZE],
        );
        S_OK
    })
}

static NET_VTBL: [VtblSlot; NET_SLOT_COUNT] = [
    vslot(net_qi as *const c_void),
    vslot(com_add_ref as *const c_void),
    vslot(com_release as *const c_void),
    vslot(net_query_port as *const c_void),                // 3
    vslot(net_query_port_async as *const c_void),          // 4
    vslot(net_query_port_async_result as *const c_void),   // 5
    vslot(net_register_port_changed as *const c_void),     // 6
    vslot(net_unregister as *const c_void),                // 7
    vslot(net_url_async as *const c_void),                 // 8
    vslot(net_url_result_size as *const c_void),           // 9
    vslot(net_url_result as *const c_void),                // 10
    vslot(net_url_utf16_async as *const c_void),           // 11
    vslot(net_url_result_size as *const c_void),           // 12
    vslot(net_url_result as *const c_void),                // 13
    vslot(net_verify_server_certificate as *const c_void), // 14
    vslot(net_get_connectivity_hint as *const c_void),     // 15
    vslot(net_register_hint_changed as *const c_void),     // 16
    vslot(net_unregister as *const c_void),                // 17
    unimpl!(18), // XNetworkingQueryConfigurationSetting : valeurs par défaut non documentées
    unimpl!(19), // XNetworkingSetConfigurationSetting
    vslot(net_query_statistics as *const c_void), // 20
];
const _: () = assert!(NET_VTBL.len() == NET_SLOT_COUNT);

fn create_net() -> *mut ComHeader {
    static CELL: OnceLock<usize> = OnceLock::new();
    singleton(&CELL, NET_VTBL.as_ptr().cast())
}

/// Classes fournies par ce module (enregistrées dans `gdk_classes::registry`).
pub fn classes() -> Vec<ClassDef> {
    vec![ClassDef {
        name: "XNetworkingImpl",
        clsid: CLSID_XNETWORKING,
        extra_iids: &[IID_XNETWORKING2],
        create: create_net,
    }]
}

#[cfg(test)]
#[allow(clippy::manual_c_str_literals, clippy::manual_dangling_ptr)]
mod tests {
    use super::super::rt_user::test_util::*;
    use super::*;
    use sb_common::abi::{E_NOINTERFACE, E_NOTIMPL};
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    fn net() -> *mut c_void {
        obj_iid(&CLSID_XNETWORKING, &IID_XNETWORKING2)
    }

    #[test]
    fn vtable_length() {
        const _: () = assert!(NET_VTBL.len() == 21);
    }

    #[test]
    fn iunknown() {
        let o = net();
        assert!(add_ref(o) >= 2);
        let mut p = core::ptr::null_mut();
        assert_eq!(qi(o, &IID_XNETWORKING2, &mut p), S_OK);
        assert_eq!(p, o);
        assert_eq!(qi(o, &CLSID_XNETWORKING, &mut p), S_OK);
        assert_eq!(qi(o, &BOGUS, &mut p), E_NOINTERFACE);
        assert!(p.is_null());
        let mut x = core::ptr::null_mut();
        assert_eq!(
            crate::gdk_classes::query(&CLSID_XNETWORKING, &BOGUS, &mut x),
            E_NOINTERFACE
        );
        release(o);
    }

    #[test]
    fn port_queries_fail_unreachable() {
        let o = net();
        let q: extern "win64" fn(*mut c_void, *mut u16) -> HResult = slot(o, 3);
        let mut port = 9u16;
        assert_eq!(q(o, &mut port), E_NETWORK_UNREACHABLE);
        assert_eq!(port, 0);
        assert_eq!(q(o, core::ptr::null_mut()), E_INVALIDARG);
        let a: extern "win64" fn(*mut c_void, *mut XAsyncBlock) -> HResult = slot(o, 4);
        let r: extern "win64" fn(*mut c_void, *mut XAsyncBlock, *mut u16) -> HResult = slot(o, 5);
        let mut b = new_block();
        assert_eq!(a(o, core::ptr::null_mut()), E_INVALIDARG);
        assert_eq!(r(o, &mut *b, &mut port), E_INVALIDARG); // pas d'opération
        assert_eq!(a(o, &mut *b), S_OK);
        assert_eq!(b.internal[0], 1);
        assert_eq!(r(o, &mut *b, core::ptr::null_mut()), E_INVALIDARG);
        assert_eq!(r(o, &mut *b, &mut port), E_NETWORK_UNREACHABLE);
        let reg: extern "win64" fn(*mut c_void, usize, *mut c_void, usize, *mut u64) -> HResult =
            slot(o, 6);
        let unreg: extern "win64" fn(*mut c_void, u64, u8) -> u8 = slot(o, 7);
        let mut t = 0u64;
        assert_eq!(reg(o, 0, core::ptr::null_mut(), 0, &mut t), E_INVALIDARG);
        assert_eq!(reg(o, 0, core::ptr::null_mut(), 0x10, &mut t), S_OK);
        assert_ne!(t, 0);
        assert_eq!(unreg(o, t, 1), 1);
    }

    #[test]
    fn url_security_information() {
        let o = net();
        let a: extern "win64" fn(*mut c_void, *const u8, *mut XAsyncBlock) -> HResult = slot(o, 8);
        let sz: extern "win64" fn(*mut c_void, *mut XAsyncBlock, *mut usize) -> HResult =
            slot(o, 9);
        let res: extern "win64" fn(
            *mut c_void,
            *mut XAsyncBlock,
            usize,
            *mut usize,
            *mut u8,
            *mut *mut c_void,
        ) -> HResult = slot(o, 10);
        let mut b = new_block();
        assert_eq!(a(o, core::ptr::null(), &mut *b), E_INVALIDARG);
        assert_eq!(a(o, b"https://x.invalid\0".as_ptr(), &mut *b), S_OK);
        let mut n = 7usize;
        assert_eq!(sz(o, &mut *b, &mut n), E_NETWORK_UNREACHABLE);
        assert_eq!(n, 0);
        assert_eq!(sz(o, &mut *b, core::ptr::null_mut()), E_INVALIDARG);
        let mut info: *mut c_void = 1usize as *mut c_void;
        let mut buf = [0u8; 8];
        assert_eq!(
            res(
                o,
                &mut *b,
                8,
                core::ptr::null_mut(),
                buf.as_mut_ptr(),
                core::ptr::null_mut()
            ),
            E_INVALIDARG
        );
        assert_eq!(
            res(
                o,
                &mut *b,
                8,
                core::ptr::null_mut(),
                buf.as_mut_ptr(),
                &mut info
            ),
            E_NETWORK_UNREACHABLE
        );
        assert!(info.is_null());

        // Variante UTF-16.
        let a16: extern "win64" fn(*mut c_void, *const u16, *mut XAsyncBlock) -> HResult =
            slot(o, 11);
        let sz16: extern "win64" fn(*mut c_void, *mut XAsyncBlock, *mut usize) -> HResult =
            slot(o, 12);
        let res16: extern "win64" fn(
            *mut c_void,
            *mut XAsyncBlock,
            usize,
            *mut usize,
            *mut u8,
            *mut *mut c_void,
        ) -> HResult = slot(o, 13);
        let w: Vec<u16> = "https://x.invalid\0".encode_utf16().collect();
        let mut b2 = new_block();
        assert_eq!(a16(o, core::ptr::null(), &mut *b2), E_INVALIDARG);
        assert_eq!(a16(o, w.as_ptr(), core::ptr::null_mut()), E_INVALIDARG);
        assert_eq!(a16(o, w.as_ptr(), &mut *b2), S_OK);
        assert_eq!(sz16(o, &mut *b2, &mut n), E_NETWORK_UNREACHABLE);
        assert_eq!(
            res16(
                o,
                &mut *b2,
                0,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                &mut info
            ),
            E_NETWORK_UNREACHABLE
        );
    }

    #[test]
    fn certificate_and_hint() {
        let o = net();
        let v: extern "win64" fn(*mut c_void, *mut c_void, *const c_void) -> HResult = slot(o, 14);
        let dummy = 1u8;
        let p = &dummy as *const u8 as *mut c_void;
        assert_eq!(v(o, p, p), E_NETWORK_UNREACHABLE);
        assert_eq!(v(o, core::ptr::null_mut(), p), E_INVALIDARG);
        assert_eq!(v(o, p, core::ptr::null()), E_INVALIDARG);
        let g: extern "win64" fn(*mut c_void, *mut ConnectivityHint) -> HResult = slot(o, 15);
        let mut h = ConnectivityHint {
            connectivity_level: 99,
            connectivity_cost: 99,
            iana_interface_type: 99,
            network_initialized: 0,
            approaching_data_limit: 1,
            over_data_limit: 1,
            roaming: 1,
        };
        assert_eq!(g(o, &mut h), S_OK);
        assert_eq!(h.connectivity_level, LEVEL_NONE);
        assert_eq!(h.network_initialized, 1);
        assert_eq!(
            (h.approaching_data_limit, h.over_data_limit, h.roaming),
            (0, 0, 0)
        );
        assert_eq!(g(o, core::ptr::null_mut()), E_INVALIDARG);
    }

    static HINT_CALLS: AtomicU32 = AtomicU32::new(0);
    static HINT_CTX: AtomicUsize = AtomicUsize::new(0);
    static HINT_LEVEL: AtomicU32 = AtomicU32::new(0);

    unsafe extern "win64" fn hint_cb(ctx: *mut c_void, hint: *const ConnectivityHint) {
        HINT_CTX.store(ctx as usize, Ordering::SeqCst);
        // SAFETY: `hint` pointe vers le hint statique fourni par le runtime.
        HINT_LEVEL.store(unsafe { (*hint).connectivity_level }, Ordering::SeqCst);
        HINT_CALLS.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn hint_registration_sends_one_initial_notification() {
        let o = net();
        let reg: extern "win64" fn(*mut c_void, usize, *mut c_void, usize, *mut u64) -> HResult =
            slot(o, 16);
        let unreg: extern "win64" fn(*mut c_void, u64, u8) -> u8 = slot(o, 17);
        let mut t = 0u64;
        assert_eq!(reg(o, 0, core::ptr::null_mut(), 0, &mut t), E_INVALIDARG);
        assert_eq!(
            reg(
                o,
                0,
                core::ptr::null_mut(),
                hint_cb as *const () as usize,
                core::ptr::null_mut()
            ),
            E_INVALIDARG
        );
        assert_eq!(
            reg(
                o,
                0xdead,
                core::ptr::null_mut(),
                hint_cb as *const () as usize,
                &mut t
            ),
            E_INVALIDARG
        );
        assert_eq!(t, 0);
        assert_eq!(
            reg(
                o,
                0,
                0x1234 as *mut c_void,
                hint_cb as *const () as usize,
                &mut t
            ),
            S_OK
        );
        assert_ne!(t, 0);
        assert!(wait_until(|| HINT_CALLS.load(Ordering::SeqCst) >= 1));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(HINT_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(HINT_CTX.load(Ordering::SeqCst), 0x1234);
        assert_eq!(HINT_LEVEL.load(Ordering::SeqCst), LEVEL_NONE);
        assert_eq!(unreg(o, t, 1), 1);
    }

    #[test]
    fn statistics_and_notimpl() {
        let o = net();
        let st: extern "win64" fn(*mut c_void, u32, *mut u8) -> HResult = slot(o, 20);
        let mut buf = [0xffu8; STATS_BUFFER_SIZE];
        assert_eq!(st(o, 1, buf.as_mut_ptr()), S_OK);
        assert!(buf.iter().all(|&b| b == 0));
        assert_eq!(st(o, 3, buf.as_mut_ptr()), E_INVALIDARG);
        assert_eq!(st(o, 0, core::ptr::null_mut()), E_INVALIDARG);
        for n in [18usize, 19] {
            let f: extern "win64" fn(*mut c_void, usize, usize, usize) -> HResult = slot(o, n);
            assert_eq!(f(o, 0, 0, 0), E_NOTIMPL);
        }
    }
}
