//! Classes du runtime GDK : `XUserImpl` (+ `IXUserGamertagImpl`) et utilitaires communs aux
//! classes hors ligne (`rt_net`, `rt_offline`) : GUID, complétion `XAsyncBlock`, jetons,
//! handles utilisateur, lecture de chaînes invité.
//!
//! Un seul utilisateur local fictif, toujours connecté, adulte, avec tous les privilèges.
//! Les handles `XUserHandle` sont des allocations distinctes, validées par un ensemble vivant :
//! un handle inconnu n'est jamais déréférencé et un double `CloseHandle` est sans effet.
//! Tout ce qui exige le réseau (jetons, signatures, jeton MSA, image du joueur, résolution
//! d'incident) échoue de façon synchrone, avant le démarrage de l'opération asynchrone.

#![allow(clippy::too_many_arguments)]

use core::ffi::c_void;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use sb_common::abi::{E_FAIL, E_INVALIDARG, HResult, S_OK, WinBool};
use sb_common::{ffi_guard, sb_debug, sb_info};

use super::ClassDef;
use crate::com::{
    ComHeader, Guid, VtblSlot, com_add_ref, com_query_interface_list, com_release, vslot,
};
use crate::gdk_xs::XAsyncBlock;

const CHANNEL: &str = "gdk_user";

// ---------------------------------------------------------------------------------------------
// GUID
// ---------------------------------------------------------------------------------------------

const fn hex_digit(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => panic!("chiffre hexadécimal attendu"),
    }
}

const fn hex_run(s: &[u8], from: usize, n: usize) -> u64 {
    let mut v = 0u64;
    let mut i = 0;
    while i < n {
        v = (v << 4) | hex_digit(s[from + i]) as u64;
        i += 1;
    }
    v
}

/// GUID depuis sa forme textuelle `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (compile-time).
pub(super) const fn guid(s: &str) -> Guid {
    let b = s.as_bytes();
    assert!(b.len() == 36);
    Guid::new(
        hex_run(b, 0, 8) as u32,
        hex_run(b, 9, 4) as u16,
        hex_run(b, 14, 4) as u16,
        [
            hex_run(b, 19, 2) as u8,
            hex_run(b, 21, 2) as u8,
            hex_run(b, 24, 2) as u8,
            hex_run(b, 26, 2) as u8,
            hex_run(b, 28, 2) as u8,
            hex_run(b, 30, 2) as u8,
            hex_run(b, 32, 2) as u8,
            hex_run(b, 34, 2) as u8,
        ],
    )
}

// ---------------------------------------------------------------------------------------------
// HRESULT documentés (xgameerr.h / winerror.h)
// ---------------------------------------------------------------------------------------------

pub(super) const E_GAMEUSER_DEFERRAL_NOT_AVAILABLE: HResult = 0x8924_5103_u32 as i32;
pub(super) const E_GAMEUSER_USER_NOT_FOUND: HResult = 0x8924_5104_u32 as i32;
pub(super) const E_GAMEUSER_FAILED_TO_RESOLVE: HResult = 0x8924_5107_u32 as i32;
pub(super) const E_GAMEUSER_FAILED_TO_GET_TOKEN: HResult = 0x8924_5111_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER)`.
pub(super) const E_INSUFFICIENT_BUFFER: HResult = 0x8007_007A_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_NETWORK_UNREACHABLE)` (1231).
pub(super) const E_NETWORK_UNREACHABLE: HResult = 0x8007_04CF_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_NOT_FOUND)` (1168).
pub(super) const E_NOT_FOUND: HResult = 0x8007_0490_u32 as i32;

// ---------------------------------------------------------------------------------------------
// Aides FFI invité
// ---------------------------------------------------------------------------------------------

/// Écrit `v` dans une sortie invité (écriture non alignée). `p` doit être non-nul (vérifié par
/// l'appelant) ; un pointeur nul est ignoré par prudence.
pub(super) fn write_out<T: Copy>(p: *mut T, v: T) {
    if p.is_null() {
        return;
    }
    // SAFETY: `p` non-nul ; l'invité fournit un emplacement de sortie valide pour `T`
    // (contrat de l'API appelée), écriture non alignée.
    unsafe { p.write_unaligned(v) };
}

/// Lit une chaîne C UTF-8 de l'invité (bornée à 64 Kio) ; `None` si le pointeur est nul.
pub(super) fn read_cstr(p: *const u8) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut v = Vec::new();
    for i in 0..65_536usize {
        // SAFETY: `p` est une chaîne C terminée par NUL fournie par l'invité (contrat de
        // l'API) ; la lecture s'arrête au premier NUL.
        let b = unsafe { p.add(i).read() };
        if b == 0 {
            break;
        }
        v.push(b);
    }
    Some(String::from_utf8_lossy(&v).into_owned())
}

/// Lit une chaîne UTF-16 terminée par NUL de l'invité (bornée à 64 Ki unités).
pub(super) fn read_wstr(p: *const u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut v = Vec::new();
    for i in 0..65_536usize {
        // SAFETY: chaîne large terminée par NUL fournie par l'invité ; arrêt au premier NUL.
        let c = unsafe { p.add(i).read_unaligned() };
        if c == 0 {
            break;
        }
        v.push(c);
    }
    Some(String::from_utf16_lossy(&v))
}

/// Singleton d'objet COM sans état propre (en-tête seul).
pub(super) fn singleton(cell: &'static OnceLock<usize>, vtbl: *const c_void) -> *mut ComHeader {
    *cell.get_or_init(|| Box::into_raw(Box::new(ComHeader::new(vtbl))) as usize) as *mut ComHeader
}

/// Fonction `QueryInterface` d'une classe : `iunknown_fns!(nom_fn, "Classe", &[IID...])`.
macro_rules! qi_fn {
    ($fname:ident, $class:literal, $iids:expr) => {
        extern "win64" fn $fname(
            this: *mut $crate::com::ComHeader,
            riid: *const $crate::com::Guid,
            ppv: *mut *mut core::ffi::c_void,
        ) -> sb_common::abi::HResult {
            sb_common::ffi_guard!("gdk_classes", "QueryInterface", sb_common::abi::E_FAIL, {
                $crate::com::com_query_interface_list($class, this, riid, ppv, $iids)
            })
        }
    };
}
pub(super) use qi_fn;

// ---------------------------------------------------------------------------------------------
// Jetons d'enregistrement (callbacks jamais déclenchés)
// ---------------------------------------------------------------------------------------------

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(0x7000_0001);

fn live_tokens() -> &'static Mutex<HashSet<u64>> {
    static T: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Nouveau jeton d'enregistrement (`XTaskQueueRegistrationToken`), non nul.
pub(super) fn new_token() -> u64 {
    let t = NEXT_TOKEN.fetch_add(1, Ordering::AcqRel);
    live_tokens().lock().insert(t);
    t
}

/// Retire un jeton ; vrai s'il était enregistré.
pub(super) fn drop_token(t: u64) -> bool {
    live_tokens().lock().remove(&t)
}

pub(super) fn token_is_live(t: u64) -> bool {
    live_tokens().lock().contains(&t)
}

// ---------------------------------------------------------------------------------------------
// Complétion XAsync
// ---------------------------------------------------------------------------------------------

/// Résultat d'une opération asynchrone terminée.
#[derive(Clone, Copy)]
pub(super) struct Done {
    pub hr: HResult,
    pub value: u64,
}

fn done_map() -> &'static Mutex<HashMap<usize, Done>> {
    static M: OnceLock<Mutex<HashMap<usize, Done>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Résultat enregistré pour ce bloc, sans le consommer (appels `...ResultSize`).
pub(super) fn peek_done(block: *mut XAsyncBlock) -> Option<Done> {
    done_map().lock().get(&(block as usize)).copied()
}

/// Résultat enregistré pour ce bloc, consommé (appels `...Result`).
pub(super) fn take_done(block: *mut XAsyncBlock) -> Option<Done> {
    done_map().lock().remove(&(block as usize))
}

type SubmitFn = extern "win64" fn(usize, u32, *mut c_void, usize) -> HResult;
type ProcessQueueFn = extern "win64" fn(*mut usize) -> WinBool;

fn task_fns() -> Option<(SubmitFn, ProcessQueueFn)> {
    static F: OnceLock<Option<(usize, usize)>> = OnceLock::new();
    let r = (*F.get_or_init(|| {
        let m = crate::gdk_xs::xgameruntime_exports();
        Some((
            *m.get("XTaskQueueSubmitCallback")?,
            *m.get("XTaskQueueGetCurrentProcessTaskQueue")?,
        ))
    }))?;
    // SAFETY: adresses des fonctions `XTaskQueueSubmitCallback` /
    // `XTaskQueueGetCurrentProcessTaskQueue` de `gdk_xs`, de ces signatures exactes.
    Some(unsafe {
        (
            core::mem::transmute::<usize, SubmitFn>(r.0),
            core::mem::transmute::<usize, ProcessQueueFn>(r.1),
        )
    })
}

/// Soumet `callback(context, canceled)` sur le port de complétion de `queue` (file de
/// processus si `queue` vaut 0). Renvoie le HRESULT de la soumission.
pub(super) fn submit_completion(queue: usize, context: *mut c_void, callback: usize) -> HResult {
    let Some((submit, process_queue)) = task_fns() else {
        return E_FAIL;
    };
    let mut q = queue;
    if q == 0 {
        let mut h = 0usize;
        if process_queue(&mut h) == 0 {
            return E_FAIL;
        }
        q = h;
    }
    submit(q, 1, context, callback)
}

struct AsyncCbCtx {
    block: usize,
    callback: usize,
}

extern "win64" fn async_trampoline(ctx: *mut c_void, _canceled: u8) {
    ffi_guard!(CHANNEL, "async_trampoline", (), {
        // SAFETY: `ctx` vient de `Box::into_raw` dans `deliver_async_callback` et n'est
        // consommé qu'ici (une seule exécution par soumission).
        let c = unsafe { Box::from_raw(ctx.cast::<AsyncCbCtx>()) };
        // SAFETY: `callback` est le champ `XAsyncBlock::callback` non nul (convention Win64).
        let cb: unsafe extern "win64" fn(*mut XAsyncBlock) =
            unsafe { core::mem::transmute(c.callback) };
        // SAFETY: le bloc reste valide jusqu'à la fin de son callback (contrat XAsync).
        unsafe { cb(c.block as *mut XAsyncBlock) };
    })
}

fn deliver_async_callback(block: *mut XAsyncBlock, queue: usize, callback: usize) {
    let ctx = Box::into_raw(Box::new(AsyncCbCtx {
        block: block as usize,
        callback,
    }));
    let hr = submit_completion(queue, ctx.cast(), async_trampoline as *const () as usize);
    if hr < 0 {
        // File invalide : on retombe sur l'appel direct (convention de `gdk_xs`).
        // SAFETY: `ctx` n'a pas été consommé par la file (soumission échouée).
        let c = unsafe { Box::from_raw(ctx) };
        // SAFETY: voir `async_trampoline`.
        let cb: unsafe extern "win64" fn(*mut XAsyncBlock) =
            unsafe { core::mem::transmute(c.callback) };
        // SAFETY: le bloc est valide (fourni par l'appelant de l'API asynchrone).
        unsafe { cb(block) };
    }
}

/// Termine immédiatement l'opération asynchrone `block` avec `hr` / `value` : enregistre le
/// résultat, marque le bloc terminé (`internal[0] = 1`) puis planifie son callback sur la file
/// de complétion du bloc (file de processus si aucune). `block` doit être non-nul.
pub(super) fn complete_async(block: *mut XAsyncBlock, hr: HResult, value: u64) {
    if block.is_null() {
        return;
    }
    done_map().lock().insert(block as usize, Done { hr, value });
    // SAFETY: `block` non-nul, pointe vers l'`XAsyncBlock` du titre (vérifié par l'appelant).
    let (queue, callback) = unsafe {
        (*block).internal[0] = 1;
        // Convention commune des blocs terminés hors `XAsyncBegin` (voir `rt_core/xasync.rs`) :
        // statut en `internal[4..8]`, taille de résultat en `internal[8..16]`.
        let internal = core::ptr::addr_of_mut!((*block).internal).cast::<u8>();
        core::ptr::copy_nonoverlapping(hr.to_le_bytes().as_ptr(), internal.add(4), 4);
        core::ptr::copy_nonoverlapping(0u64.to_le_bytes().as_ptr(), internal.add(8), 8);
        ((*block).queue, (*block).callback)
    };
    if let Some(cb) = callback {
        deliver_async_callback(block, queue, cb as usize);
    }
}

// ---------------------------------------------------------------------------------------------
// Utilisateur fictif
// ---------------------------------------------------------------------------------------------

/// `XUserLocalId` fixe, non nul, du joueur fictif.
pub(super) const USER_LOCAL_ID: u64 = 1;

/// XUID fixe, non nul, du joueur fictif (identique au profil local).
pub(super) fn user_id() -> u64 {
    static ID: OnceLock<u64> = OnceLock::new();
    *ID.get_or_init(|| sb_runtime::profile::LocalUserProfile::default().xuid)
}

/// Gamertag : `SB_GAMERTAG` (ASCII imprimable, 15 caractères max) sinon `Player`.
pub(super) fn gamertag() -> &'static str {
    static TAG: OnceLock<String> = OnceLock::new();
    TAG.get_or_init(|| {
        let raw = std::env::var("SB_GAMERTAG").unwrap_or_default();
        sanitize_gamertag(&raw)
    })
}

fn sanitize_gamertag(raw: &str) -> String {
    let s: String = raw
        .chars()
        .filter(|c| (' '..='~').contains(c))
        .take(15)
        .collect();
    let s = s.trim().to_string();
    if s.is_empty() {
        "Player".to_string()
    } else {
        s
    }
}

fn live_users() -> &'static Mutex<HashSet<usize>> {
    static S: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Alloue un nouveau `XUserHandle` pour l'utilisateur fictif.
pub(super) fn new_user_handle() -> usize {
    let p = Box::into_raw(Box::new(USER_LOCAL_ID)) as usize;
    live_users().lock().insert(p);
    p
}

/// Vrai si `h` est un handle utilisateur vivant (jamais déréférencé).
pub(super) fn is_live_user(h: usize) -> bool {
    h != 0 && live_users().lock().contains(&h)
}

fn close_user_handle(h: usize) {
    if live_users().lock().remove(&h) {
        // SAFETY: `h` a été produit par `Box::into_raw(Box<u64>)` dans `new_user_handle` et
        // vient d'être retiré de l'ensemble vivant : un seul appelant libère.
        drop(unsafe { Box::from_raw(h as *mut u64) });
    }
}

// ---------------------------------------------------------------------------------------------
// IXUserImpl (slots 3..49)
// ---------------------------------------------------------------------------------------------

const CLSID_XUSER: Guid = guid("01acd177-91f9-4763-a38e-ccbb55ce32e0");
const IID_XUSER2: Guid = guid("eb9bf948-18dc-4d82-bbcc-40e0a809c4c0");
const IID_XUSER3: Guid = guid("1bf2f8c5-d507-4e52-bb05-f726d0e71161");
const IID_XUSER4: Guid = guid("079415e3-6727-437f-8e9d-8f8f9b2439f7");
const IID_XUSER5: Guid = guid("26f3c674-a2fe-44fa-b6c4-a323bc94ff53");
const IID_XUSER6: Guid = guid("5131d685-4394-4ee6-8c18-bfb5d4aef1ff");
const IID_XUSER_GAMERTAG: Guid = guid("cef4fac0-7676-4a94-a119-4c43f9eb5b74");

const USER_EXTRA_IIDS: &[Guid] = &[IID_XUSER2, IID_XUSER3, IID_XUSER4, IID_XUSER5, IID_XUSER6];

const USER_SLOT_COUNT: usize = 50;

// XUserAddOptions
const ADD_SILENTLY: u32 = 0x01;
const ADD_ALLOW_GUESTS: u32 = 0x02;
const ADD_ALLOW_UI: u32 = 0x04;

fn user_all_iids() -> &'static [Guid] {
    static IIDS: [Guid; 6] = [
        CLSID_XUSER,
        IID_XUSER2,
        IID_XUSER3,
        IID_XUSER4,
        IID_XUSER5,
        IID_XUSER6,
    ];
    &IIDS
}

extern "win64" fn user_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserImpl::QueryInterface", E_FAIL, {
        if !riid.is_null() && !ppv.is_null() {
            // SAFETY: `riid` non-nul, lecture non alignée d'un GUID.
            let wanted = unsafe { riid.read_unaligned() };
            if wanted == IID_XUSER_GAMERTAG {
                let g = create_gamertag();
                // SAFETY: `g` est le singleton gamertag (vivant pour le processus).
                unsafe { (*g).add_ref() };
                write_out(ppv, g.cast::<c_void>());
                return S_OK;
            }
        }
        com_query_interface_list("XUserImpl", this, riid, ppv, user_all_iids())
    })
}

extern "win64" fn user_duplicate(_this: *mut c_void, h: usize, out: *mut usize) -> HResult {
    ffi_guard!(CHANNEL, "XUserDuplicateHandle", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, 0usize);
        if !is_live_user(h) {
            return E_INVALIDARG;
        }
        write_out(out, new_user_handle());
        S_OK
    })
}

extern "win64" fn user_close(_this: *mut c_void, h: usize) {
    ffi_guard!(CHANNEL, "XUserCloseHandle", (), {
        close_user_handle(h);
    })
}

extern "win64" fn user_compare(_this: *mut c_void, a: usize, b: usize) -> i32 {
    ffi_guard!(CHANNEL, "XUserCompare", 0, {
        // Un seul utilisateur : deux handles vivants désignent le même ; NULL (ou handle
        // inconnu) est ordonné avant tout utilisateur.
        match (is_live_user(a), is_live_user(b)) {
            (true, true) | (false, false) => 0,
            (false, true) => -1,
            (true, false) => 1,
        }
    })
}

extern "win64" fn user_get_max_users(_this: *mut c_void, out: *mut u32) -> HResult {
    ffi_guard!(CHANNEL, "XUserGetMaxUsers", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, 1u32);
        S_OK
    })
}

extern "win64" fn user_add_async(
    _this: *mut c_void,
    options: u32,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserAddAsync", E_FAIL, {
        if block.is_null() {
            return E_INVALIDARG;
        }
        if options & !(ADD_SILENTLY | ADD_ALLOW_GUESTS | ADD_ALLOW_UI) != 0 {
            return E_INVALIDARG;
        }
        if options & ADD_SILENTLY != 0 && options & ADD_ALLOW_GUESTS != 0 {
            return E_INVALIDARG;
        }
        sb_debug!(
            CHANNEL,
            "XUserAddAsync(options={options:#x}) -> joueur local"
        );
        complete_async(block, S_OK, user_id());
        S_OK
    })
}

fn add_result(block: *mut XAsyncBlock, out: *mut usize) -> HResult {
    if block.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    write_out(out, 0usize);
    let Some(d) = take_done(block) else {
        return E_INVALIDARG;
    };
    if d.hr < 0 {
        return d.hr;
    }
    write_out(out, new_user_handle());
    S_OK
}

extern "win64" fn user_add_result(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    out: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserAddResult", E_FAIL, {
        add_result(block, out)
    })
}

extern "win64" fn user_get_local_id(_this: *mut c_void, h: usize, out: *mut u64) -> HResult {
    ffi_guard!(CHANNEL, "XUserGetLocalId", E_FAIL, {
        if out.is_null() || !is_live_user(h) {
            return E_INVALIDARG;
        }
        write_out(out, USER_LOCAL_ID);
        S_OK
    })
}

extern "win64" fn user_find_by_local_id(_this: *mut c_void, id: u64, out: *mut usize) -> HResult {
    ffi_guard!(CHANNEL, "XUserFindUserByLocalId", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, 0usize);
        if id != USER_LOCAL_ID {
            return E_GAMEUSER_USER_NOT_FOUND;
        }
        write_out(out, new_user_handle());
        S_OK
    })
}

extern "win64" fn user_get_id(_this: *mut c_void, h: usize, out: *mut u64) -> HResult {
    ffi_guard!(CHANNEL, "XUserGetId", E_FAIL, {
        if out.is_null() || !is_live_user(h) {
            return E_INVALIDARG;
        }
        write_out(out, user_id());
        S_OK
    })
}

extern "win64" fn user_find_by_id(_this: *mut c_void, id: u64, out: *mut usize) -> HResult {
    ffi_guard!(CHANNEL, "XUserFindUserById", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, 0usize);
        if id != user_id() {
            return E_GAMEUSER_USER_NOT_FOUND;
        }
        write_out(out, new_user_handle());
        S_OK
    })
}

extern "win64" fn user_get_is_guest(_this: *mut c_void, h: usize, out: *mut u8) -> HResult {
    ffi_guard!(CHANNEL, "XUserGetIsGuest", E_FAIL, {
        if out.is_null() || !is_live_user(h) {
            return E_INVALIDARG;
        }
        write_out(out, 0u8);
        S_OK
    })
}

extern "win64" fn user_get_state(_this: *mut c_void, h: usize, out: *mut u32) -> HResult {
    ffi_guard!(CHANNEL, "XUserGetState", E_FAIL, {
        if out.is_null() || !is_live_user(h) {
            return E_INVALIDARG;
        }
        write_out(out, 0u32); // XUserState::SignedIn
        S_OK
    })
}

extern "win64" fn user_get_gamer_picture_async(
    _this: *mut c_void,
    h: usize,
    size: u32,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserGetGamerPictureAsync", E_FAIL, {
        if block.is_null() || !is_live_user(h) || size > 3 {
            return E_INVALIDARG;
        }
        sb_info!(CHANNEL, "XUserGetGamerPictureAsync: hors ligne -> E_FAIL");
        E_FAIL
    })
}

/// `...ResultSize` d'une opération qui n'a jamais démarré (hors ligne).
extern "win64" fn user_result_size_none(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    out: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XUser...ResultSize", E_FAIL, {
        if block.is_null() || out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, 0usize);
        E_INVALIDARG
    })
}

/// `XUserGetGamerPictureResult` / `XUserGetMsaTokenSilentlyResult` d'une opération absente.
extern "win64" fn user_result_buffer_none(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    size: usize,
    buf: *mut c_void,
    used: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XUser...Result", E_FAIL, {
        if block.is_null() || (buf.is_null() && size != 0) {
            return E_INVALIDARG;
        }
        write_out(used, 0usize);
        E_INVALIDARG
    })
}

/// `XUserGetTokenAndSignature[Utf16]Result` d'une opération absente.
extern "win64" fn user_token_result_none(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    size: usize,
    buf: *mut c_void,
    ptr_to_buffer: *mut *mut c_void,
    used: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserGetTokenAndSignatureResult", E_FAIL, {
        if block.is_null() || ptr_to_buffer.is_null() || (buf.is_null() && size != 0) {
            return E_INVALIDARG;
        }
        write_out(ptr_to_buffer, core::ptr::null_mut::<c_void>());
        write_out(used, 0usize);
        E_INVALIDARG
    })
}

extern "win64" fn user_get_age_group(_this: *mut c_void, h: usize, out: *mut u32) -> HResult {
    ffi_guard!(CHANNEL, "XUserGetAgeGroup", E_FAIL, {
        if out.is_null() || !is_live_user(h) {
            return E_INVALIDARG;
        }
        write_out(out, 3u32); // XUserAgeGroup::Adult
        S_OK
    })
}

fn privilege_is_known(p: u32) -> bool {
    matches!(
        p,
        185 | 188 | 189 | 190 | 196 | 198 | 203 | 207 | 208 | 209 | 220 | 247 | 252 | 254 | 255
    )
}

extern "win64" fn user_check_privilege(
    _this: *mut c_void,
    h: usize,
    options: u32,
    privilege: u32,
    has: *mut u8,
    reason: *mut u32,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserCheckPrivilege", E_FAIL, {
        if has.is_null() || !is_live_user(h) || options > 1 || !privilege_is_known(privilege) {
            return E_INVALIDARG;
        }
        write_out(has, 1u8);
        write_out(reason, 0u32); // XUserPrivilegeDenyReason::None
        S_OK
    })
}

extern "win64" fn user_resolve_privilege_async(
    _this: *mut c_void,
    h: usize,
    options: u32,
    privilege: u32,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserResolvePrivilegeWithUiAsync", E_FAIL, {
        if block.is_null() || !is_live_user(h) || options > 1 || !privilege_is_known(privilege) {
            return E_INVALIDARG;
        }
        // Le joueur détient déjà tous les privilèges : rien à résoudre, aucune UI.
        complete_async(block, S_OK, 0);
        S_OK
    })
}

/// `...Result` sans valeur (HRESULT seul) : renvoie le HRESULT enregistré.
extern "win64" fn user_result_plain(_this: *mut c_void, block: *mut XAsyncBlock) -> HResult {
    ffi_guard!(CHANNEL, "XUser...Result", E_FAIL, {
        if block.is_null() {
            return E_INVALIDARG;
        }
        match take_done(block) {
            Some(d) => d.hr,
            None => E_INVALIDARG,
        }
    })
}

extern "win64" fn user_token_async(
    _this: *mut c_void,
    h: usize,
    options: u32,
    method: *const u8,
    url: *const u8,
    header_count: usize,
    headers: *const c_void,
    body_size: usize,
    body: *const c_void,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserGetTokenAndSignatureAsync", E_FAIL, {
        if block.is_null()
            || !is_live_user(h)
            || options > 3
            || method.is_null()
            || url.is_null()
            || (header_count != 0 && headers.is_null())
            || (body_size != 0 && body.is_null())
        {
            return E_INVALIDARG;
        }
        sb_info!(
            CHANNEL,
            "XUserGetTokenAndSignatureAsync({}): hors ligne -> E_GAMEUSER_FAILED_TO_GET_TOKEN",
            read_cstr(url).unwrap_or_default()
        );
        E_GAMEUSER_FAILED_TO_GET_TOKEN
    })
}

extern "win64" fn user_token_utf16_async(
    _this: *mut c_void,
    h: usize,
    options: u32,
    method: *const u16,
    url: *const u16,
    header_count: usize,
    headers: *const c_void,
    body_size: usize,
    body: *const c_void,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserGetTokenAndSignatureUtf16Async", E_FAIL, {
        if block.is_null()
            || !is_live_user(h)
            || options > 3
            || method.is_null()
            || url.is_null()
            || (header_count != 0 && headers.is_null())
            || (body_size != 0 && body.is_null())
        {
            return E_INVALIDARG;
        }
        sb_info!(
            CHANNEL,
            "XUserGetTokenAndSignatureUtf16Async({}): hors ligne -> E_GAMEUSER_FAILED_TO_GET_TOKEN",
            read_wstr(url).unwrap_or_default()
        );
        E_GAMEUSER_FAILED_TO_GET_TOKEN
    })
}

extern "win64" fn user_resolve_issue_async(
    _this: *mut c_void,
    h: usize,
    _url: *const u8,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserResolveIssueWithUiAsync", E_FAIL, {
        if block.is_null() || !is_live_user(h) {
            return E_INVALIDARG;
        }
        E_GAMEUSER_FAILED_TO_RESOLVE
    })
}

extern "win64" fn user_resolve_issue_utf16_async(
    _this: *mut c_void,
    h: usize,
    _url: *const u16,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserResolveIssueWithUiUtf16Async", E_FAIL, {
        if block.is_null() || !is_live_user(h) {
            return E_INVALIDARG;
        }
        E_GAMEUSER_FAILED_TO_RESOLVE
    })
}

extern "win64" fn user_register_change(
    _this: *mut c_void,
    _queue: usize,
    _context: *mut c_void,
    callback: usize,
    token: *mut u64,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserRegisterForChangeEvent", E_FAIL, {
        if callback == 0 || token.is_null() {
            return E_INVALIDARG;
        }
        // L'utilisateur ne change jamais : le jeton existe, le callback n'est jamais appelé.
        write_out(token, new_token());
        S_OK
    })
}

extern "win64" fn user_unregister_change(_this: *mut c_void, token: u64, _wait: u8) -> u8 {
    ffi_guard!(CHANNEL, "XUserUnregisterForChangeEvent", 0, {
        drop_token(token);
        1 // jamais de callback en cours
    })
}

extern "win64" fn user_get_sign_out_deferral(_this: *mut c_void, out: *mut usize) -> HResult {
    ffi_guard!(CHANNEL, "XUserGetSignOutDeferral", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, 0usize);
        E_GAMEUSER_DEFERRAL_NOT_AVAILABLE
    })
}

extern "win64" fn user_close_sign_out_deferral(_this: *mut c_void, _h: usize) {}

extern "win64" fn user_add_by_id_async(
    _this: *mut c_void,
    id: u64,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserAddByIdWithUiAsync", E_FAIL, {
        if block.is_null() {
            return E_INVALIDARG;
        }
        if id != user_id() {
            return E_GAMEUSER_USER_NOT_FOUND;
        }
        complete_async(block, S_OK, id);
        S_OK
    })
}

extern "win64" fn user_add_by_id_result(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    out: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserAddByIdWithUiResult", E_FAIL, {
        add_result(block, out)
    })
}

extern "win64" fn user_msa_async(
    _this: *mut c_void,
    h: usize,
    options: u32,
    scope: *const u8,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserGetMsaTokenSilentlyAsync", E_FAIL, {
        if block.is_null() || !is_live_user(h) || options != 0 || scope.is_null() {
            return E_INVALIDARG;
        }
        E_GAMEUSER_FAILED_TO_GET_TOKEN
    })
}

extern "win64" fn user_is_store_user(_this: *mut c_void, h: usize) -> u8 {
    ffi_guard!(CHANNEL, "XUserIsStoreUser", 0, {
        u8::from(is_live_user(h)) // console : l'utilisateur est un utilisateur du Store
    })
}

extern "win64" fn user_is_sign_out_present(_this: *mut c_void) -> u8 {
    0
}

static USER_VTBL: [VtblSlot; USER_SLOT_COUNT] = [
    vslot(user_qi as *const c_void),
    vslot(com_add_ref as *const c_void),
    vslot(com_release as *const c_void),
    vslot(user_duplicate as *const c_void),               // 3
    vslot(user_close as *const c_void),                   // 4
    vslot(user_compare as *const c_void),                 // 5
    vslot(user_get_max_users as *const c_void),           // 6
    vslot(user_add_async as *const c_void),               // 7
    vslot(user_add_result as *const c_void),              // 8
    vslot(user_get_local_id as *const c_void),            // 9
    vslot(user_find_by_local_id as *const c_void),        // 10
    vslot(user_get_id as *const c_void),                  // 11
    vslot(user_find_by_id as *const c_void),              // 12
    vslot(user_get_is_guest as *const c_void),            // 13
    vslot(user_get_state as *const c_void),               // 14
    unimpl!(15),                                          // `_unknown_` réservé
    vslot(user_get_gamer_picture_async as *const c_void), // 16
    vslot(user_result_size_none as *const c_void),        // 17
    vslot(user_result_buffer_none as *const c_void),      // 18
    vslot(user_get_age_group as *const c_void),           // 19
    vslot(user_check_privilege as *const c_void),         // 20
    vslot(user_resolve_privilege_async as *const c_void), // 21
    vslot(user_result_plain as *const c_void),            // 22
    vslot(user_token_async as *const c_void),             // 23
    vslot(user_result_size_none as *const c_void),        // 24
    vslot(user_token_result_none as *const c_void),       // 25
    vslot(user_token_utf16_async as *const c_void),       // 26
    vslot(user_result_size_none as *const c_void),        // 27
    vslot(user_token_result_none as *const c_void),       // 28
    vslot(user_resolve_issue_async as *const c_void),     // 29
    vslot(user_result_plain as *const c_void),            // 30
    vslot(user_resolve_issue_utf16_async as *const c_void), // 31
    vslot(user_result_plain as *const c_void),            // 32
    vslot(user_register_change as *const c_void),         // 33
    vslot(user_unregister_change as *const c_void),       // 34
    vslot(user_get_sign_out_deferral as *const c_void),   // 35
    vslot(user_close_sign_out_deferral as *const c_void), // 36
    vslot(user_add_by_id_async as *const c_void),         // 37 (IXUserImpl2)
    vslot(user_add_by_id_result as *const c_void),        // 38
    vslot(user_msa_async as *const c_void),               // 39 (IXUserImpl3)
    vslot(user_result_buffer_none as *const c_void),      // 40
    vslot(user_result_size_none as *const c_void),        // 41
    vslot(user_is_store_user as *const c_void),           // 42 (IXUserImpl4)
    unimpl!(43),                                          // 43..46 (IXUserImpl5, PC)
    unimpl!(44),
    unimpl!(45),
    unimpl!(46),
    vslot(user_is_sign_out_present as *const c_void), // 47 (IXUserImpl6)
    unimpl!(48),
    unimpl!(49),
];
const _: () = assert!(USER_VTBL.len() == USER_SLOT_COUNT);

fn create_user() -> *mut ComHeader {
    static CELL: OnceLock<usize> = OnceLock::new();
    singleton(&CELL, USER_VTBL.as_ptr().cast())
}

// ---------------------------------------------------------------------------------------------
// IXUserGamertagImpl (objet séparé, 1 méthode, slot 3)
// ---------------------------------------------------------------------------------------------

const GAMERTAG_SLOT_COUNT: usize = 4;

qi_fn!(gamertag_qi, "XUserGamertagImpl", &[IID_XUSER_GAMERTAG]);

fn gamertag_component(component: u32) -> Option<String> {
    let tag = gamertag();
    match component {
        0 | 1 => Some(tag.to_string()),   // Classic, Modern
        2 => Some("0001".to_string()),    // ModernSuffix
        3 => Some(format!("{tag}#0001")), // UniqueModern
        _ => None,
    }
}

extern "win64" fn gamertag_get(
    _this: *mut c_void,
    h: usize,
    component: u32,
    size: usize,
    buf: *mut u8,
    used: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserGetGamertag", E_FAIL, {
        if !is_live_user(h) || (buf.is_null() && size != 0) {
            return E_INVALIDARG;
        }
        let Some(text) = gamertag_component(component) else {
            return E_INVALIDARG;
        };
        let needed = text.len() + 1;
        write_out(used, needed);
        if size < needed {
            return E_INSUFFICIENT_BUFFER;
        }
        // SAFETY: `buf` est non-nul et fait au moins `size >= needed` octets (vérifié).
        unsafe {
            core::ptr::copy_nonoverlapping(text.as_ptr(), buf, text.len());
            buf.add(text.len()).write(0);
        }
        S_OK
    })
}

static GAMERTAG_VTBL: [VtblSlot; GAMERTAG_SLOT_COUNT] = [
    vslot(gamertag_qi as *const c_void),
    vslot(com_add_ref as *const c_void),
    vslot(com_release as *const c_void),
    vslot(gamertag_get as *const c_void),
];
const _: () = assert!(GAMERTAG_VTBL.len() == GAMERTAG_SLOT_COUNT);

fn create_gamertag() -> *mut ComHeader {
    static CELL: OnceLock<usize> = OnceLock::new();
    singleton(&CELL, GAMERTAG_VTBL.as_ptr().cast())
}

/// Classes fournies par ce module (enregistrées dans `gdk_classes::registry`).
///
/// `IXUserGamertagImpl` est une interface distincte de `XUserImpl` (vtable propre, slot 3 =
/// `XUserGetGamertag`) : elle est servie à la fois par `QueryInterface` depuis l'objet
/// `XUserImpl` et comme classe dont le CLSID est son propre IID (thunk passant le même GUID
/// comme classe et interface). Le registre actuel ne dispatche pas l'objet selon l'IID :
/// `QueryApiImpl(CLSID_XUser, IID_XUserGamertag)` renvoie `E_NOINTERFACE`.
pub fn classes() -> Vec<ClassDef> {
    vec![
        ClassDef {
            name: "XUserImpl",
            clsid: CLSID_XUSER,
            extra_iids: USER_EXTRA_IIDS,
            create: create_user,
        },
        ClassDef {
            name: "XUserGamertagImpl",
            clsid: IID_XUSER_GAMERTAG,
            extra_iids: &[],
            create: create_gamertag,
        },
    ]
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
pub(super) mod test_util {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    /// Obtient l'objet de la classe `clsid` via `QueryApiImpl` (même GUID comme IID).
    pub fn obj(clsid: &Guid) -> *mut c_void {
        obj_iid(clsid, clsid)
    }

    pub fn obj_iid(clsid: &Guid, iid: &Guid) -> *mut c_void {
        let mut o: *mut c_void = core::ptr::null_mut();
        assert_eq!(crate::gdk_classes::query(clsid, iid, &mut o), 0);
        assert!(!o.is_null());
        o
    }

    /// Adresse du slot `n` de la vtable de `obj`.
    pub fn slot_addr(obj: *mut c_void, n: usize) -> usize {
        // SAFETY: `obj` est un objet COM de ce module : premier champ = pointeur de vtable.
        unsafe {
            let vtbl = *(obj as *const *const usize);
            *vtbl.add(n)
        }
    }

    /// Slot `n` converti au type de fonction `F`.
    pub fn slot<F: Copy>(obj: *mut c_void, n: usize) -> F {
        assert_eq!(core::mem::size_of::<F>(), core::mem::size_of::<usize>());
        let a = slot_addr(obj, n);
        // SAFETY: `F` est un pointeur de fonction de la taille d'un usize ; l'appelant donne
        // la signature réelle du slot.
        unsafe { core::mem::transmute_copy::<usize, F>(&a) }
    }

    pub fn new_block() -> Box<XAsyncBlock> {
        Box::new(XAsyncBlock {
            queue: 0,
            context: core::ptr::null_mut(),
            callback: None,
            internal: [0; 32],
        })
    }

    pub static CB_HIT: AtomicBool = AtomicBool::new(false);

    pub unsafe extern "win64" fn flag_callback(_b: *mut XAsyncBlock) {
        CB_HIT.store(true, Ordering::SeqCst);
    }

    pub fn wait_until(f: impl Fn() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// IID aléatoire jamais fourni.
    pub const BOGUS: Guid = guid("deadbeef-0000-4000-8000-0123456789ab");

    pub fn release(o: *mut c_void) -> u32 {
        let f: extern "win64" fn(*mut c_void) -> u32 = slot(o, 2);
        f(o)
    }

    pub fn add_ref(o: *mut c_void) -> u32 {
        let f: extern "win64" fn(*mut c_void) -> u32 = slot(o, 1);
        f(o)
    }

    pub fn qi(o: *mut c_void, iid: &Guid, out: &mut *mut c_void) -> HResult {
        let f: extern "win64" fn(*mut c_void, *const Guid, *mut *mut c_void) -> HResult =
            slot(o, 0);
        f(o, iid, out)
    }
}

#[cfg(test)]
#[allow(clippy::manual_c_str_literals, clippy::manual_dangling_ptr)]
mod tests {
    use super::test_util::*;
    use super::*;

    type F1H = extern "win64" fn(*mut c_void, usize, *mut usize) -> HResult;

    fn user() -> *mut c_void {
        obj(&CLSID_XUSER)
    }

    fn add_user(o: *mut c_void) -> usize {
        let add: extern "win64" fn(*mut c_void, u32, *mut XAsyncBlock) -> HResult = slot(o, 7);
        let res: extern "win64" fn(*mut c_void, *mut XAsyncBlock, *mut usize) -> HResult =
            slot(o, 8);
        let mut b = new_block();
        assert_eq!(add(o, 0, &mut *b), S_OK);
        assert_eq!(b.internal[0], 1);
        let mut h = 0usize;
        assert_eq!(res(o, &mut *b, &mut h), S_OK);
        assert!(h != 0);
        h
    }

    #[test]
    fn vtable_lengths() {
        const _: () = assert!(USER_VTBL.len() == 50);
        const _: () = assert!(GAMERTAG_VTBL.len() == 4);
    }

    #[test]
    fn guid_parse() {
        assert_eq!(CLSID_XUSER.data1, 0x01ac_d177);
        assert_eq!(
            CLSID_XUSER.data4,
            [0xa3, 0x8e, 0xcc, 0xbb, 0x55, 0xce, 0x32, 0xe0]
        );
    }

    #[test]
    fn iunknown_and_interfaces() {
        let o = user();
        let before = add_ref(o);
        assert!(before >= 2);
        assert!(release(o) >= 1);
        for iid in USER_EXTRA_IIDS {
            let mut p = core::ptr::null_mut();
            assert_eq!(qi(o, iid, &mut p), S_OK);
            assert_eq!(p, o);
            release(p);
        }
        let mut p = core::ptr::null_mut();
        assert_eq!(qi(o, &BOGUS, &mut p), sb_common::abi::E_NOINTERFACE);
        assert!(p.is_null());
        // IXUserGamertagImpl : autre objet.
        assert_eq!(qi(o, &IID_XUSER_GAMERTAG, &mut p), S_OK);
        assert_ne!(p, o);
        release(p);
        // QueryApiImpl : IID inconnu.
        let mut x = core::ptr::null_mut();
        assert_eq!(
            crate::gdk_classes::query(&CLSID_XUSER, &BOGUS, &mut x),
            sb_common::abi::E_NOINTERFACE
        );
        release(o);
    }

    #[test]
    fn add_async_options_and_completion() {
        let o = user();
        let add: extern "win64" fn(*mut c_void, u32, *mut XAsyncBlock) -> HResult = slot(o, 7);
        let mut b = new_block();
        assert_eq!(
            add(o, ADD_SILENTLY | ADD_ALLOW_GUESTS, &mut *b),
            E_INVALIDARG
        );
        assert_eq!(add(o, 0x80, &mut *b), E_INVALIDARG);
        assert_eq!(add(o, 0, core::ptr::null_mut()), E_INVALIDARG);
        // Callback planifié sur la file de processus.
        CB_HIT.store(false, Ordering::SeqCst);
        b.callback = Some(flag_callback);
        assert_eq!(add(o, ADD_SILENTLY, &mut *b), S_OK);
        assert!(wait_until(|| CB_HIT.load(Ordering::SeqCst)));
        let res: extern "win64" fn(*mut c_void, *mut XAsyncBlock, *mut usize) -> HResult =
            slot(o, 8);
        let mut h = 0usize;
        assert_eq!(res(o, &mut *b, core::ptr::null_mut()), E_INVALIDARG);
        assert_eq!(res(o, &mut *b, &mut h), S_OK);
        // Résultat consommé.
        assert_eq!(res(o, &mut *b, &mut h), E_INVALIDARG);
        assert_eq!(h, 0);
    }

    #[test]
    fn handles_are_validated() {
        let o = user();
        let h = add_user(o);
        let dup: F1H = slot(o, 3);
        let close: extern "win64" fn(*mut c_void, usize) = slot(o, 4);
        let cmp: extern "win64" fn(*mut c_void, usize, usize) -> i32 = slot(o, 5);
        let mut h2 = 0usize;
        assert_eq!(dup(o, h, &mut h2), S_OK);
        assert_ne!(h, h2);
        assert_eq!(cmp(o, h, h2), 0);
        assert_eq!(cmp(o, 0, h), -1);
        assert_eq!(cmp(o, h, 0), 1);
        assert_eq!(cmp(o, 0, 0), 0);
        assert_eq!(dup(o, 0, &mut h2), E_INVALIDARG);
        assert_eq!(h2, 0);
        assert_eq!(dup(o, 0xdead_beef, &mut h2), E_INVALIDARG);
        assert_eq!(dup(o, h, core::ptr::null_mut()), E_INVALIDARG);
        close(o, h);
        close(o, h); // double close sans effet
        let mut id = 0u64;
        let get_id: extern "win64" fn(*mut c_void, usize, *mut u64) -> HResult = slot(o, 11);
        assert_eq!(get_id(o, h, &mut id), E_INVALIDARG);
    }

    #[test]
    fn identity_and_state() {
        let o = user();
        let h = add_user(o);
        let get_id: extern "win64" fn(*mut c_void, usize, *mut u64) -> HResult = slot(o, 11);
        let get_local: extern "win64" fn(*mut c_void, usize, *mut u64) -> HResult = slot(o, 9);
        let guest: extern "win64" fn(*mut c_void, usize, *mut u8) -> HResult = slot(o, 13);
        let state: extern "win64" fn(*mut c_void, usize, *mut u32) -> HResult = slot(o, 14);
        let age: extern "win64" fn(*mut c_void, usize, *mut u32) -> HResult = slot(o, 19);
        let max: extern "win64" fn(*mut c_void, *mut u32) -> HResult = slot(o, 6);
        let mut id = 0u64;
        assert_eq!(get_id(o, h, &mut id), S_OK);
        assert_ne!(id, 0);
        assert_eq!(get_id(o, h, core::ptr::null_mut()), E_INVALIDARG);
        let mut lid = 0u64;
        assert_eq!(get_local(o, h, &mut lid), S_OK);
        assert_ne!(lid, 0);
        assert_eq!(get_local(o, 0, &mut lid), E_INVALIDARG);
        let mut g = 9u8;
        assert_eq!(guest(o, h, &mut g), S_OK);
        assert_eq!(g, 0);
        let mut s = 9u32;
        assert_eq!(state(o, h, &mut s), S_OK);
        assert_eq!(s, 0);
        assert_eq!(state(o, h, core::ptr::null_mut()), E_INVALIDARG);
        let mut a = 0u32;
        assert_eq!(age(o, h, &mut a), S_OK);
        assert_eq!(a, 3);
        let mut m = 0u32;
        assert_eq!(max(o, &mut m), S_OK);
        assert_eq!(m, 1);
        assert_eq!(max(o, core::ptr::null_mut()), E_INVALIDARG);
        // Recherche
        let by_id: extern "win64" fn(*mut c_void, u64, *mut usize) -> HResult = slot(o, 12);
        let by_local: extern "win64" fn(*mut c_void, u64, *mut usize) -> HResult = slot(o, 10);
        let mut f = 0usize;
        assert_eq!(by_id(o, id, &mut f), S_OK);
        assert!(is_live_user(f));
        assert_eq!(by_id(o, id + 1, &mut f), E_GAMEUSER_USER_NOT_FOUND);
        assert_eq!(f, 0);
        assert_eq!(by_local(o, lid, &mut f), S_OK);
        assert_eq!(by_local(o, lid + 7, &mut f), E_GAMEUSER_USER_NOT_FOUND);
    }

    #[test]
    fn privileges() {
        let o = user();
        let h = add_user(o);
        let chk: extern "win64" fn(*mut c_void, usize, u32, u32, *mut u8, *mut u32) -> HResult =
            slot(o, 20);
        let mut has = 0u8;
        let mut why = 7u32;
        assert_eq!(chk(o, h, 0, 254, &mut has, &mut why), S_OK);
        assert_eq!((has, why), (1, 0));
        assert_eq!(chk(o, h, 0, 254, &mut has, core::ptr::null_mut()), S_OK);
        assert_eq!(chk(o, h, 0, 1, &mut has, &mut why), E_INVALIDARG);
        assert_eq!(chk(o, h, 5, 254, &mut has, &mut why), E_INVALIDARG);
        assert_eq!(chk(o, 0, 0, 254, &mut has, &mut why), E_INVALIDARG);
        assert_eq!(
            chk(o, h, 0, 254, core::ptr::null_mut(), &mut why),
            E_INVALIDARG
        );
        let rp: extern "win64" fn(*mut c_void, usize, u32, u32, *mut XAsyncBlock) -> HResult =
            slot(o, 21);
        let rr: extern "win64" fn(*mut c_void, *mut XAsyncBlock) -> HResult = slot(o, 22);
        let mut b = new_block();
        assert_eq!(rp(o, h, 0, 999, &mut *b), E_INVALIDARG);
        assert_eq!(rp(o, h, 0, 254, &mut *b), S_OK);
        assert_eq!(rr(o, &mut *b), S_OK);
        assert_eq!(rr(o, &mut *b), E_INVALIDARG); // consommé
        assert_eq!(rr(o, core::ptr::null_mut()), E_INVALIDARG);
    }

    #[test]
    fn offline_failures_are_synchronous() {
        let o = user();
        let h = add_user(o);
        let mut b = new_block();
        let url = b"https://example.invalid/\0";
        let method = b"GET\0";
        let tok: extern "win64" fn(
            *mut c_void,
            usize,
            u32,
            *const u8,
            *const u8,
            usize,
            *const c_void,
            usize,
            *const c_void,
            *mut XAsyncBlock,
        ) -> HResult = slot(o, 23);
        let nullp = core::ptr::null();
        assert_eq!(
            tok(
                o,
                h,
                0,
                method.as_ptr(),
                url.as_ptr(),
                0,
                nullp,
                0,
                nullp,
                &mut *b
            ),
            E_GAMEUSER_FAILED_TO_GET_TOKEN
        );
        assert_eq!(
            tok(
                o,
                h,
                0,
                nullp.cast(),
                url.as_ptr(),
                0,
                nullp,
                0,
                nullp,
                &mut *b
            ),
            E_INVALIDARG
        );
        assert_eq!(b.internal[0], 0, "aucun XAsyncBlock en attente");
        let wurl: Vec<u16> = "https://x/\0".encode_utf16().collect();
        let wm: Vec<u16> = "GET\0".encode_utf16().collect();
        let tok16: extern "win64" fn(
            *mut c_void,
            usize,
            u32,
            *const u16,
            *const u16,
            usize,
            *const c_void,
            usize,
            *const c_void,
            *mut XAsyncBlock,
        ) -> HResult = slot(o, 26);
        assert_eq!(
            tok16(
                o,
                h,
                0,
                wm.as_ptr(),
                wurl.as_ptr(),
                0,
                nullp,
                0,
                nullp,
                &mut *b
            ),
            E_GAMEUSER_FAILED_TO_GET_TOKEN
        );
        assert_eq!(
            tok16(
                o,
                0,
                0,
                wm.as_ptr(),
                wurl.as_ptr(),
                0,
                nullp,
                0,
                nullp,
                &mut *b
            ),
            E_INVALIDARG
        );
        let pic: extern "win64" fn(*mut c_void, usize, u32, *mut XAsyncBlock) -> HResult =
            slot(o, 16);
        assert_eq!(pic(o, h, 0, &mut *b), E_FAIL);
        assert_eq!(pic(o, h, 9, &mut *b), E_INVALIDARG);
        let msa: extern "win64" fn(
            *mut c_void,
            usize,
            u32,
            *const u8,
            *mut XAsyncBlock,
        ) -> HResult = slot(o, 39);
        assert_eq!(
            msa(o, h, 0, b"scope\0".as_ptr(), &mut *b),
            E_GAMEUSER_FAILED_TO_GET_TOKEN
        );
        assert_eq!(msa(o, h, 0, nullp.cast(), &mut *b), E_INVALIDARG);
        let ri: extern "win64" fn(*mut c_void, usize, *const u8, *mut XAsyncBlock) -> HResult =
            slot(o, 29);
        assert_eq!(
            ri(o, h, nullp.cast(), &mut *b),
            E_GAMEUSER_FAILED_TO_RESOLVE
        );
        assert_eq!(ri(o, 0, nullp.cast(), &mut *b), E_INVALIDARG);
        let ri16: extern "win64" fn(*mut c_void, usize, *const u16, *mut XAsyncBlock) -> HResult =
            slot(o, 31);
        assert_eq!(
            ri16(o, h, nullp.cast(), &mut *b),
            E_GAMEUSER_FAILED_TO_RESOLVE
        );
        // Résultats sans opération.
        let sz: extern "win64" fn(*mut c_void, *mut XAsyncBlock, *mut usize) -> HResult =
            slot(o, 24);
        let mut n = 5usize;
        assert_eq!(sz(o, &mut *b, &mut n), E_INVALIDARG);
        assert_eq!(n, 0);
        assert_eq!(sz(o, &mut *b, core::ptr::null_mut()), E_INVALIDARG);
        // Déferral.
        let d: extern "win64" fn(*mut c_void, *mut usize) -> HResult = slot(o, 35);
        let mut dh = 5usize;
        assert_eq!(d(o, &mut dh), E_GAMEUSER_DEFERRAL_NOT_AVAILABLE);
        assert_eq!(dh, 0);
        assert_eq!(d(o, core::ptr::null_mut()), E_INVALIDARG);
    }

    #[test]
    fn change_events_and_misc() {
        let o = user();
        let h = add_user(o);
        let reg: extern "win64" fn(*mut c_void, usize, *mut c_void, usize, *mut u64) -> HResult =
            slot(o, 33);
        let unreg: extern "win64" fn(*mut c_void, u64, u8) -> u8 = slot(o, 34);
        let mut t = 0u64;
        assert_eq!(reg(o, 0, core::ptr::null_mut(), 0x1000, &mut t), S_OK);
        assert_ne!(t, 0);
        assert!(token_is_live(t));
        assert_eq!(unreg(o, t, 1), 1);
        assert!(!token_is_live(t));
        assert_eq!(reg(o, 0, core::ptr::null_mut(), 0, &mut t), E_INVALIDARG);
        assert_eq!(
            reg(o, 0, core::ptr::null_mut(), 0x1000, core::ptr::null_mut()),
            E_INVALIDARG
        );
        let store: extern "win64" fn(*mut c_void, usize) -> u8 = slot(o, 42);
        assert_eq!(store(o, h), 1);
        assert_eq!(store(o, 0), 0);
        let sop: extern "win64" fn(*mut c_void) -> u8 = slot(o, 47);
        assert_eq!(sop(o), 0);
        // AddById
        let byid: extern "win64" fn(*mut c_void, u64, *mut XAsyncBlock) -> HResult = slot(o, 37);
        let byidr: extern "win64" fn(*mut c_void, *mut XAsyncBlock, *mut usize) -> HResult =
            slot(o, 38);
        let mut b = new_block();
        assert_eq!(byid(o, 12345, &mut *b), E_GAMEUSER_USER_NOT_FOUND);
        assert_eq!(byid(o, user_id(), &mut *b), S_OK);
        let mut nh = 0usize;
        assert_eq!(byidr(o, &mut *b, &mut nh), S_OK);
        assert!(is_live_user(nh));
        // Slots réservés / PC.
        for n in [15usize, 43, 44, 45, 46, 48, 49] {
            let f: extern "win64" fn(*mut c_void, usize, usize, usize) -> HResult = slot(o, n);
            assert_eq!(f(o, 0, 0, 0), sb_common::abi::E_NOTIMPL);
        }
    }

    #[test]
    fn gamertag_interface() {
        let g = obj(&IID_XUSER_GAMERTAG);
        let u = user();
        let h = add_user(u);
        let get: extern "win64" fn(*mut c_void, usize, u32, usize, *mut u8, *mut usize) -> HResult =
            slot(g, 3);
        let mut buf = [0u8; 32];
        let mut used = 0usize;
        assert_eq!(get(g, h, 0, buf.len(), buf.as_mut_ptr(), &mut used), S_OK);
        assert_eq!(used, gamertag().len() + 1);
        assert_eq!(&buf[..gamertag().len()], gamertag().as_bytes());
        assert!(gamertag().len() <= 15);
        // Taille insuffisante.
        assert_eq!(
            get(g, h, 0, 1, buf.as_mut_ptr(), &mut used),
            E_INSUFFICIENT_BUFFER
        );
        assert_eq!(used, gamertag().len() + 1);
        // Arguments invalides.
        assert_eq!(get(g, 0, 0, 32, buf.as_mut_ptr(), &mut used), E_INVALIDARG);
        assert_eq!(get(g, h, 9, 32, buf.as_mut_ptr(), &mut used), E_INVALIDARG);
        assert_eq!(
            get(g, h, 0, 32, core::ptr::null_mut(), &mut used),
            E_INVALIDARG
        );
        // Via QueryInterface depuis XUserImpl.
        let mut p = core::ptr::null_mut();
        assert_eq!(qi(u, &IID_XUSER_GAMERTAG, &mut p), S_OK);
        assert_eq!(slot_addr(p, 3), slot_addr(g, 3));
    }

    #[test]
    fn gamertag_sanitization() {
        assert_eq!(sanitize_gamertag(""), "Player");
        assert_eq!(sanitize_gamertag("été"), "t");
        assert_eq!(sanitize_gamertag("ABCDEFGHIJKLMNOPQRST"), "ABCDEFGHIJKLMNO");
        assert_eq!(sanitize_gamertag("Jo\x01e"), "Joe");
    }
}
