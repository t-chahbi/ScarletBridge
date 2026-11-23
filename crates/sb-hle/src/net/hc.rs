//! `libHttpClient(.GDK).dll` : API `HC*` (en-têtes publics microsoft/libHttpClient `httpClient.h`,
//! `httpProvider.h`, `trace.h`).
//!
//! - Handles d'appel avec compteur de références ; les chaînes renvoyées restent valides tant que
//!   le handle vit (stockées dans l'objet, comme la bibliothèque officielle).
//! - `HCHttpCallPerformAsync` : moteur HTTP/1.1 en clair partagé avec WinHTTP. En offline strict,
//!   toute destination non loopback complète l'`XAsyncBlock` avec `networkErrorCode = E_HC_NO_NETWORK`
//!   et un statut HTTP 0. HTTPS (pas de TLS) → `HRESULT_FROM_WIN32(ERROR_WINHTTP_SECURE_FAILURE)`.
//! - WebSockets : handles gérés, connexion toujours en échec `E_HC_NO_NETWORK`.

#![allow(non_snake_case, unused_variables, clippy::too_many_arguments)]

use core::ffi::c_void;
use std::collections::HashMap;
use std::ffi::CString;
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use sb_common::abi::{E_INVALIDARG, HResult, S_OK};
use sb_common::{ffi_guard, sb_debug, sb_info};
use sb_loader::{HleResolver, ImportSymbol};

use super::http::{self, Timeouts};
use super::{cstr_arg, is_local_hostname, is_loopback_ip, offline_mode, spawn_guest_worker};
use crate::gdk_xs::XAsyncBlock;

const CH: &str = "libhttpclient";

pub(crate) const E_HC_NOT_INITIALISED: HResult = 0x8923_5001_u32 as i32;
pub(crate) const E_HC_PERFORM_ALREADY_CALLED: HResult = 0x8923_5003_u32 as i32;
pub(crate) const E_HC_ALREADY_INITIALISED: HResult = 0x8923_5004_u32 as i32;
pub(crate) const E_HC_NO_NETWORK: HResult = 0x8923_5006_u32 as i32;
const E_NOT_SUFFICIENT_BUFFER: HResult = 0x8007_007A_u32 as i32;

const fn hresult_from_win32(code: u32) -> HResult {
    if code == 0 {
        0
    } else {
        (0x8007_0000 | (code & 0xFFFF)) as i32
    }
}

const _: () = assert!(size_of::<XAsyncBlock>() == 56);

/// `HCTraceImplArea` : `{ char const* Name; HCTraceLevel Verbosity; }`.
#[repr(C)]
pub(crate) struct HcTraceArea {
    pub name: *const u8,
    pub verbosity: u32,
}
const _: () = assert!(size_of::<HcTraceArea>() == 16);

/// `WebSocketCompletionResult` : `{ HCWebsocketHandle websocket; HRESULT errorCode; uint32_t platformErrorCode; }`.
#[repr(C)]
pub(crate) struct WebSocketCompletionResult {
    pub websocket: usize,
    pub error_code: HResult,
    pub platform_error_code: u32,
}
const _: () = assert!(size_of::<WebSocketCompletionResult>() == 16);

// ---------- État global ----------

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static TRACE_LEVEL: AtomicU32 = AtomicU32::new(0);
static ETW_ENABLED: AtomicBool = AtomicBool::new(false);
static TRACE_CALLBACK: AtomicUsize = AtomicUsize::new(0);
static MEM_FUNCS: Mutex<(usize, usize)> = Mutex::new((0, 0));

struct Defaults {
    retry_allowed: bool,
    timeout: u32,
    retry_delay: u32,
    timeout_window: u32,
}

static DEFAULTS: Mutex<Defaults> = Mutex::new(Defaults {
    retry_allowed: true,
    timeout: 30,
    retry_delay: 2,
    timeout_window: 20,
});

// ---------- Appels HTTP ----------

struct HcCall {
    id: u64,
    refs: u32,
    method: CString,
    url: CString,
    req_headers: Vec<(CString, CString)>,
    body: Vec<u8>,
    body_str: CString,
    retry_allowed: bool,
    retry_cache_id: u32,
    timeout: u32,
    retry_delay: u32,
    timeout_window: u32,
    tracing: bool,
    context: usize,
    perform_count: u32,
    status: u32,
    network_error: HResult,
    platform_error: u32,
    platform_msg: CString,
    resp_headers: Vec<(CString, CString)>,
    resp_body: Vec<u8>,
    resp_string: CString,
}

type CallRef = Arc<Mutex<HcCall>>;

static NEXT_CALL: AtomicUsize = AtomicUsize::new(0x00AC_0010);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static CALLS: LazyLock<Mutex<HashMap<usize, CallRef>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn call(h: usize) -> Result<CallRef, HResult> {
    CALLS.lock().get(&h).cloned().ok_or(E_INVALIDARG)
}

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

/// Lit une chaîne C obligatoire.
fn req_str(p: *const u8) -> Result<String, HResult> {
    // SAFETY: chaîne C terminée ou nul (contrat libHttpClient).
    unsafe { cstr_arg(p) }.ok_or(E_INVALIDARG)
}

/// Écrit un pointeur de sortie (`E_INVALIDARG` si nul).
fn out<T>(p: *mut T, v: T) -> Result<(), HResult> {
    if p.is_null() {
        return Err(E_INVALIDARG);
    }
    // SAFETY: pointeur de sortie non nul (contrat libHttpClient).
    unsafe { ptr::write_unaligned(p, v) };
    Ok(())
}

fn hr(r: Result<(), HResult>) -> HResult {
    match r {
        Ok(()) => S_OK,
        Err(e) => e,
    }
}

/// Accès à un appel en écriture (refusé après `HCHttpCallPerformAsync`).
fn with_mut_call(h: usize, f: impl FnOnce(&mut HcCall) -> Result<(), HResult>) -> HResult {
    let c = match call(h) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let mut g = c.lock();
    if g.perform_count > 0 {
        return E_HC_PERFORM_ALREADY_CALLED;
    }
    hr(f(&mut g))
}

fn with_call(h: usize, f: impl FnOnce(&HcCall) -> Result<(), HResult>) -> HResult {
    match call(h) {
        Ok(c) => hr(f(&c.lock())),
        Err(e) => e,
    }
}

fn set_kv(list: &mut Vec<(CString, CString)>, name: &str, value: &str) {
    if let Some(e) = list
        .iter_mut()
        .find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case(name))
    {
        e.1 = cstring(value);
    } else {
        list.push((cstring(name), cstring(value)));
        // Ordre de `http_header_map` (std::map insensible à la casse).
        list.sort_by_key(|(k, _)| k.to_string_lossy().to_ascii_lowercase());
    }
}

fn get_kv(list: &[(CString, CString)], name: &str) -> *const u8 {
    list.iter()
        .find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case(name))
        .map_or(ptr::null(), |(_, v)| v.as_ptr().cast())
}

// ---------- XAsync ----------

type XTaskQueueSubmitCallbackFn =
    unsafe extern "win64" fn(usize, u32, *mut c_void, usize) -> HResult;

/// `XTaskQueueSubmitCallback` effectif (résolu via le résolveur HLE : réutilise l'implémentation
/// XTaskQueue existante, quelle que soit la table qui la fournit).
fn task_queue_submit() -> Option<XTaskQueueSubmitCallbackFn> {
    static ADDR: OnceLock<usize> = OnceLock::new();
    let a = *ADDR.get_or_init(|| {
        crate::DefaultHleResolver::new()
            .resolve(
                "xgameruntime.dll",
                &ImportSymbol::Name("XTaskQueueSubmitCallback".to_string()),
            )
            .unwrap_or(0)
    });
    if a == 0 {
        None
    } else {
        // SAFETY: adresse d'une fonction `extern "win64"` HLE de signature XTaskQueueSubmitCallback.
        Some(unsafe { std::mem::transmute::<usize, XTaskQueueSubmitCallbackFn>(a) })
    }
}

/// `XTaskQueueCallback` : invoque la routine de complétion de l'`XAsyncBlock` (`context`).
extern "win64" fn async_completion_trampoline(context: *mut c_void, canceled: u8) {
    ffi_guard!(CH, "XAsyncCompletion", (), {
        let block = context.cast::<XAsyncBlock>();
        if block.is_null() {
            return;
        }
        // SAFETY: bloc invité vivant jusqu'à la complétion (contrat XAsync).
        if let Some(cb) = unsafe { (*block).callback } {
            // SAFETY: routine de complétion Win64 de l'invité.
            unsafe { cb(block) };
        }
    })
}

/// Marque l'`XAsyncBlock` comme terminé et planifie sa routine de complétion
/// (port de complétion de la file si fournie, sinon thread de travail « pool »).
fn complete_async(block_addr: usize) {
    let block = block_addr as *mut XAsyncBlock;
    // SAFETY: bloc invité valide (contrat XAsync) ; `internal[0] = 1` = terminé (convention gdk_xs).
    let (queue, has_cb) = unsafe {
        (*block).internal[0] = 1;
        ((*block).queue, (*block).callback.is_some())
    };
    if !has_cb {
        return;
    }
    if queue != 0
        && let Some(submit) = task_queue_submit()
    {
        // SAFETY: file invitée valide ; port 1 = Completion.
        let r = unsafe {
            submit(
                queue,
                1,
                block.cast(),
                async_completion_trampoline as *const () as usize,
            )
        };
        if r == S_OK {
            return;
        }
    }
    spawn_guest_worker("sb-hc-complete", move || {
        async_completion_trampoline(block_addr as *mut c_void, 0);
    });
}

// ---------- Initialisation / mémoire / version ----------

extern "win64" fn hc_default_alloc(size: usize, _tag: u32) -> *mut c_void {
    // SAFETY: allocation C standard.
    unsafe { libc::malloc(size.max(1)) }
}

extern "win64" fn hc_default_free(p: *mut c_void, _tag: u32) {
    // SAFETY: pointeur issu de `hc_default_alloc` (ou nul).
    unsafe { libc::free(p) }
}

extern "win64" fn HCMemSetFunctions(alloc: usize, free: usize) -> HResult {
    ffi_guard!(CH, "HCMemSetFunctions", E_INVALIDARG, {
        if INITIALIZED.load(Ordering::Acquire) {
            return E_HC_ALREADY_INITIALISED;
        }
        *MEM_FUNCS.lock() = (alloc, free);
        S_OK
    })
}

extern "win64" fn HCMemGetFunctions(alloc: *mut usize, free: *mut usize) -> HResult {
    ffi_guard!(CH, "HCMemGetFunctions", E_INVALIDARG, {
        if alloc.is_null() || free.is_null() {
            return E_INVALIDARG;
        }
        let (a, f) = *MEM_FUNCS.lock();
        let a = if a == 0 {
            hc_default_alloc as *const () as usize
        } else {
            a
        };
        let f = if f == 0 {
            hc_default_free as *const () as usize
        } else {
            f
        };
        hr(out(alloc, a).and_then(|()| out(free, f)))
    })
}

extern "win64" fn HCInitialize(args: *mut c_void) -> HResult {
    ffi_guard!(CH, "HCInitialize", E_INVALIDARG, {
        if INITIALIZED.swap(true, Ordering::AcqRel) {
            return E_HC_ALREADY_INITIALISED;
        }
        sb_info!(CH, "HCInitialize() -> S_OK (offline={})", offline_mode());
        S_OK
    })
}

extern "win64" fn HCIsInitialized() -> u8 {
    u8::from(INITIALIZED.load(Ordering::Acquire))
}

fn cleanup() {
    INITIALIZED.store(false, Ordering::Release);
}

extern "win64" fn HCCleanup() {
    ffi_guard!(CH, "HCCleanup", (), { cleanup() })
}

extern "win64" fn HCCleanupAsync(block: *mut XAsyncBlock) -> HResult {
    ffi_guard!(CH, "HCCleanupAsync", E_INVALIDARG, {
        if block.is_null() {
            return E_INVALIDARG;
        }
        cleanup();
        complete_async(block as usize);
        S_OK
    })
}

extern "win64" fn HCGetLibVersion(version: *mut *const u8) -> HResult {
    ffi_guard!(CH, "HCGetLibVersion", E_INVALIDARG, {
        hr(out(
            version,
            c"2024.06.20240620.0-scarletbridge".as_ptr().cast(),
        ))
    })
}

// ---------- Appels : cycle de vie ----------

extern "win64" fn HCHttpCallCreate(out_call: *mut usize) -> HResult {
    ffi_guard!(CH, "HCHttpCallCreate", E_INVALIDARG, {
        if out_call.is_null() {
            return E_INVALIDARG;
        }
        if !INITIALIZED.load(Ordering::Acquire) {
            return E_HC_NOT_INITIALISED;
        }
        let d = DEFAULTS.lock();
        let c = HcCall {
            id: NEXT_ID.fetch_add(1, Ordering::AcqRel),
            refs: 1,
            method: CString::default(),
            url: CString::default(),
            req_headers: Vec::new(),
            body: Vec::new(),
            body_str: CString::default(),
            retry_allowed: d.retry_allowed,
            retry_cache_id: 0,
            timeout: d.timeout,
            retry_delay: d.retry_delay,
            timeout_window: d.timeout_window,
            tracing: true,
            context: 0,
            perform_count: 0,
            status: 0,
            network_error: S_OK,
            platform_error: 0,
            platform_msg: CString::default(),
            resp_headers: Vec::new(),
            resp_body: Vec::new(),
            resp_string: CString::default(),
        };
        drop(d);
        let h = NEXT_CALL.fetch_add(0x10, Ordering::AcqRel);
        CALLS.lock().insert(h, Arc::new(Mutex::new(c)));
        hr(out(out_call, h))
    })
}

extern "win64" fn HCHttpCallDuplicateHandle(h: usize) -> usize {
    ffi_guard!(CH, "HCHttpCallDuplicateHandle", 0, {
        match call(h) {
            Ok(c) => {
                c.lock().refs += 1;
                h
            }
            Err(_) => 0,
        }
    })
}

extern "win64" fn HCHttpCallCloseHandle(h: usize) -> HResult {
    ffi_guard!(CH, "HCHttpCallCloseHandle", E_INVALIDARG, {
        let mut map = CALLS.lock();
        let Some(c) = map.get(&h).cloned() else {
            return E_INVALIDARG;
        };
        let mut g = c.lock();
        g.refs = g.refs.saturating_sub(1);
        if g.refs == 0 {
            map.remove(&h);
        }
        S_OK
    })
}

extern "win64" fn HCHttpCallGetId(h: usize) -> u64 {
    ffi_guard!(CH, "HCHttpCallGetId", 0, {
        call(h).map_or(0, |c| c.lock().id)
    })
}

extern "win64" fn HCHttpCallSetTracing(h: usize, trace: u8) -> HResult {
    ffi_guard!(CH, "HCHttpCallSetTracing", E_INVALIDARG, {
        with_mut_call(h, |c| {
            c.tracing = trace != 0;
            Ok(())
        })
    })
}

extern "win64" fn HCHttpCallGetRequestUrl(h: usize, url: *mut *const u8) -> HResult {
    ffi_guard!(CH, "HCHttpCallGetRequestUrl", E_INVALIDARG, {
        with_call(h, |c| out(url, c.url.as_ptr().cast()))
    })
}

extern "win64" fn HCHttpCallSetContext(h: usize, ctx: usize) -> HResult {
    ffi_guard!(CH, "HCHttpCallSetContext", E_INVALIDARG, {
        match call(h) {
            Ok(c) => {
                c.lock().context = ctx;
                S_OK
            }
            Err(e) => e,
        }
    })
}

extern "win64" fn HCHttpCallGetContext(h: usize, ctx: *mut usize) -> HResult {
    ffi_guard!(CH, "HCHttpCallGetContext", E_INVALIDARG, {
        with_call(h, |c| out(ctx, c.context))
    })
}

extern "win64" fn HCHttpCallGetPerformCount(h: usize, count: *mut u32) -> HResult {
    ffi_guard!(CH, "HCHttpCallGetPerformCount", E_INVALIDARG, {
        with_call(h, |c| out(count, c.perform_count))
    })
}

// ---------- Requête ----------

extern "win64" fn HCHttpCallRequestSetUrl(h: usize, method: *const u8, url: *const u8) -> HResult {
    ffi_guard!(CH, "HCHttpCallRequestSetUrl", E_INVALIDARG, {
        let (m, u) = match (req_str(method), req_str(url)) {
            (Ok(m), Ok(u)) => (m, u),
            _ => return E_INVALIDARG,
        };
        with_mut_call(h, |c| {
            c.method = cstring(&m);
            c.url = cstring(&u);
            Ok(())
        })
    })
}

extern "win64" fn HCHttpCallRequestGetUrl(
    h: usize,
    method: *mut *const u8,
    url: *mut *const u8,
) -> HResult {
    ffi_guard!(CH, "HCHttpCallRequestGetUrl", E_INVALIDARG, {
        with_call(h, |c| {
            out(method, c.method.as_ptr().cast())?;
            out(url, c.url.as_ptr().cast())
        })
    })
}

extern "win64" fn HCHttpCallRequestSetRequestBodyBytes(
    h: usize,
    bytes: *const u8,
    size: u32,
) -> HResult {
    ffi_guard!(CH, "HCHttpCallRequestSetRequestBodyBytes", E_INVALIDARG, {
        if bytes.is_null() && size > 0 {
            return E_INVALIDARG;
        }
        let data = if size == 0 {
            Vec::new()
        } else {
            // SAFETY: `size` octets lisibles (contrat libHttpClient).
            unsafe { std::slice::from_raw_parts(bytes, size as usize) }.to_vec()
        };
        with_mut_call(h, |c| {
            c.body_str = CString::new(
                data.iter()
                    .copied()
                    .take_while(|b| *b != 0)
                    .collect::<Vec<u8>>(),
            )
            .unwrap_or_default();
            c.body = data;
            Ok(())
        })
    })
}

extern "win64" fn HCHttpCallRequestSetRequestBodyString(h: usize, s: *const u8) -> HResult {
    ffi_guard!(CH, "HCHttpCallRequestSetRequestBodyString", E_INVALIDARG, {
        let Ok(s) = req_str(s) else {
            return E_INVALIDARG;
        };
        with_mut_call(h, |c| {
            c.body = s.as_bytes().to_vec();
            c.body_str = cstring(&s);
            Ok(())
        })
    })
}

extern "win64" fn HCHttpCallRequestGetRequestBodyBytes(
    h: usize,
    bytes: *mut *const u8,
    size: *mut u32,
) -> HResult {
    ffi_guard!(CH, "HCHttpCallRequestGetRequestBodyBytes", E_INVALIDARG, {
        with_call(h, |c| {
            let p = if c.body.is_empty() {
                ptr::null()
            } else {
                c.body.as_ptr()
            };
            out(bytes, p)?;
            out(size, c.body.len() as u32)
        })
    })
}

extern "win64" fn HCHttpCallRequestGetRequestBodyString(h: usize, s: *mut *const u8) -> HResult {
    ffi_guard!(CH, "HCHttpCallRequestGetRequestBodyString", E_INVALIDARG, {
        with_call(h, |c| out(s, c.body_str.as_ptr().cast()))
    })
}

extern "win64" fn HCHttpCallRequestSetHeader(
    h: usize,
    name: *const u8,
    value: *const u8,
    allow_tracing: u8,
) -> HResult {
    ffi_guard!(CH, "HCHttpCallRequestSetHeader", E_INVALIDARG, {
        let (n, v) = match (req_str(name), req_str(value)) {
            (Ok(n), Ok(v)) if !n.is_empty() => (n, v),
            _ => return E_INVALIDARG,
        };
        with_mut_call(h, |c| {
            set_kv(&mut c.req_headers, &n, &v);
            Ok(())
        })
    })
}

extern "win64" fn HCHttpCallRequestGetHeader(
    h: usize,
    name: *const u8,
    value: *mut *const u8,
) -> HResult {
    ffi_guard!(CH, "HCHttpCallRequestGetHeader", E_INVALIDARG, {
        let Ok(n) = req_str(name) else {
            return E_INVALIDARG;
        };
        with_call(h, |c| out(value, get_kv(&c.req_headers, &n)))
    })
}

extern "win64" fn HCHttpCallRequestGetNumHeaders(h: usize, num: *mut u32) -> HResult {
    ffi_guard!(CH, "HCHttpCallRequestGetNumHeaders", E_INVALIDARG, {
        with_call(h, |c| out(num, c.req_headers.len() as u32))
    })
}

fn header_at(
    list: &[(CString, CString)],
    idx: u32,
    name: *mut *const u8,
    value: *mut *const u8,
) -> Result<(), HResult> {
    let (n, v) = list
        .get(idx as usize)
        .map_or((ptr::null(), ptr::null()), |(k, v)| {
            (k.as_ptr().cast(), v.as_ptr().cast())
        });
    out(name, n)?;
    out(value, v)
}

extern "win64" fn HCHttpCallRequestGetHeaderAtIndex(
    h: usize,
    idx: u32,
    name: *mut *const u8,
    value: *mut *const u8,
) -> HResult {
    ffi_guard!(CH, "HCHttpCallRequestGetHeaderAtIndex", E_INVALIDARG, {
        with_call(h, |c| header_at(&c.req_headers, idx, name, value))
    })
}

macro_rules! retry_accessors {
    ($set:ident, $get:ident, $field:ident, $ty:ty, $conv_in:expr, $conv_out:expr) => {
        extern "win64" fn $set(h: usize, v: $ty) -> HResult {
            ffi_guard!(CH, stringify!($set), E_INVALIDARG, {
                if h == 0 {
                    DEFAULTS.lock().$field = $conv_in(v);
                    return S_OK;
                }
                with_mut_call(h, |c| {
                    c.$field = $conv_in(v);
                    Ok(())
                })
            })
        }

        extern "win64" fn $get(h: usize, o: *mut $ty) -> HResult {
            ffi_guard!(CH, stringify!($get), E_INVALIDARG, {
                if h == 0 {
                    let v = DEFAULTS.lock().$field;
                    return hr(out(o, $conv_out(v)));
                }
                with_call(h, |c| out(o, $conv_out(c.$field)))
            })
        }
    };
}

retry_accessors!(
    HCHttpCallRequestSetRetryAllowed,
    HCHttpCallRequestGetRetryAllowed,
    retry_allowed,
    u8,
    |v: u8| v != 0,
    u8::from
);
retry_accessors!(
    HCHttpCallRequestSetTimeout,
    HCHttpCallRequestGetTimeout,
    timeout,
    u32,
    |v: u32| v,
    |v: u32| v
);
retry_accessors!(
    HCHttpCallRequestSetRetryDelay,
    HCHttpCallRequestGetRetryDelay,
    retry_delay,
    u32,
    |v: u32| v,
    |v: u32| v
);
retry_accessors!(
    HCHttpCallRequestSetTimeoutWindow,
    HCHttpCallRequestGetTimeoutWindow,
    timeout_window,
    u32,
    |v: u32| v,
    |v: u32| v
);

extern "win64" fn HCHttpCallRequestSetRetryCacheId(h: usize, id: u32) -> HResult {
    ffi_guard!(CH, "HCHttpCallRequestSetRetryCacheId", E_INVALIDARG, {
        with_mut_call(h, |c| {
            c.retry_cache_id = id;
            Ok(())
        })
    })
}

extern "win64" fn HCHttpCallRequestGetRetryCacheId(h: usize, id: *mut u32) -> HResult {
    ffi_guard!(CH, "HCHttpCallRequestGetRetryCacheId", E_INVALIDARG, {
        with_call(h, |c| out(id, c.retry_cache_id))
    })
}

// ---------- Perform ----------

fn is_local_url(url: &str) -> bool {
    http::split_url(url).is_ok_and(|u| {
        let bare = u.host.trim_start_matches('[').trim_end_matches(']');
        bare.parse().is_ok_and(|ip| is_loopback_ip(&ip)) || is_local_hostname(bare)
    })
}

fn perform_call(c: &CallRef) {
    let (method, url, headers, body, timeout) = {
        let g = c.lock();
        (
            g.method.to_string_lossy().into_owned(),
            g.url.to_string_lossy().into_owned(),
            g.req_headers
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
                .collect::<Vec<_>>(),
            g.body.clone(),
            g.timeout,
        )
    };
    let t = if timeout == 0 {
        Timeouts {
            connect: None,
            send: None,
            receive: None,
        }
    } else {
        let d = Some(Duration::from_secs(u64::from(timeout)));
        Timeouts {
            connect: d,
            send: d,
            receive: d,
        }
    };
    let res = http::perform(&method, &url, &headers, &body, &t);
    let mut g = c.lock();
    match res {
        Ok(resp) => {
            g.status = resp.status;
            g.network_error = S_OK;
            g.platform_error = 0;
            g.resp_headers.clear();
            for (k, v) in &resp.headers {
                set_kv(&mut g.resp_headers, k, v);
            }
            g.resp_string = CString::new(
                resp.body
                    .iter()
                    .copied()
                    .take_while(|b| *b != 0)
                    .collect::<Vec<u8>>(),
            )
            .unwrap_or_default();
            g.resp_body = resp.body;
        }
        Err(code) => {
            let offline_block = offline_mode()
                && matches!(
                    code,
                    http::ERROR_WINHTTP_NAME_NOT_RESOLVED | http::ERROR_WINHTTP_CANNOT_CONNECT
                );
            g.status = 0;
            g.network_error = if offline_block {
                E_HC_NO_NETWORK
            } else {
                hresult_from_win32(code)
            };
            g.platform_error = code;
            g.platform_msg = cstring(&if offline_block {
                format!(
                    "ScarletBridge: mode offline strict (SB_OFFLINE_MODE=1), requête vers '{url}' bloquée"
                )
            } else {
                format!("WinHTTP error {code}")
            });
            sb_debug!(
                CH,
                "HCHttpCallPerformAsync({url}) -> networkError={:#x}",
                g.network_error as u32
            );
        }
    }
}

extern "win64" fn HCHttpCallPerformAsync(h: usize, block: *mut XAsyncBlock) -> HResult {
    ffi_guard!(CH, "HCHttpCallPerformAsync", E_INVALIDARG, {
        if block.is_null() {
            return E_INVALIDARG;
        }
        let c = match call(h) {
            Ok(c) => c,
            Err(e) => return e,
        };
        let url = {
            let mut g = c.lock();
            if g.perform_count > 0 {
                return E_HC_PERFORM_ALREADY_CALLED;
            }
            if g.url.as_bytes().is_empty() {
                return E_INVALIDARG;
            }
            g.perform_count += 1;
            g.url.to_string_lossy().into_owned()
        };
        // SAFETY: bloc invité valide ; 0 = en cours (convention gdk_xs).
        unsafe { (*block).internal[0] = 0 };
        let addr = block as usize;
        if offline_mode() || is_local_url(&url) {
            // Échec offline immédiat ou loopback : résultat disponible dès le retour.
            perform_call(&c);
            complete_async(addr);
        } else {
            spawn_guest_worker("sb-hc-perform", move || {
                perform_call(&c);
                complete_async(addr);
            });
        }
        S_OK
    })
}

// ---------- Réponse ----------

extern "win64" fn HCHttpCallResponseGetStatusCode(h: usize, status: *mut u32) -> HResult {
    ffi_guard!(CH, "HCHttpCallResponseGetStatusCode", E_INVALIDARG, {
        with_call(h, |c| out(status, c.status))
    })
}

extern "win64" fn HCHttpCallResponseGetNetworkErrorCode(
    h: usize,
    err: *mut HResult,
    platform: *mut u32,
) -> HResult {
    ffi_guard!(CH, "HCHttpCallResponseGetNetworkErrorCode", E_INVALIDARG, {
        with_call(h, |c| {
            out(err, c.network_error)?;
            out(platform, c.platform_error)
        })
    })
}

extern "win64" fn HCHttpCallResponseGetPlatformNetworkErrorMessage(
    h: usize,
    msg: *mut *const u8,
) -> HResult {
    ffi_guard!(
        CH,
        "HCHttpCallResponseGetPlatformNetworkErrorMessage",
        E_INVALIDARG,
        { with_call(h, |c| out(msg, c.platform_msg.as_ptr().cast())) }
    )
}

extern "win64" fn HCHttpCallResponseGetResponseString(h: usize, s: *mut *const u8) -> HResult {
    ffi_guard!(CH, "HCHttpCallResponseGetResponseString", E_INVALIDARG, {
        with_call(h, |c| out(s, c.resp_string.as_ptr().cast()))
    })
}

extern "win64" fn HCHttpCallResponseGetResponseBodyBytesSize(
    h: usize,
    size: *mut usize,
) -> HResult {
    ffi_guard!(
        CH,
        "HCHttpCallResponseGetResponseBodyBytesSize",
        E_INVALIDARG,
        { with_call(h, |c| out(size, c.resp_body.len())) }
    )
}

extern "win64" fn HCHttpCallResponseGetResponseBodyBytes(
    h: usize,
    size: usize,
    buf: *mut u8,
    used: *mut usize,
) -> HResult {
    ffi_guard!(
        CH,
        "HCHttpCallResponseGetResponseBodyBytes",
        E_INVALIDARG,
        {
            if buf.is_null() {
                return E_INVALIDARG;
            }
            with_call(h, |c| {
                if size < c.resp_body.len() {
                    return Err(E_NOT_SUFFICIENT_BUFFER);
                }
                // SAFETY: tampon invité de `size` ≥ longueur du corps.
                unsafe { ptr::copy_nonoverlapping(c.resp_body.as_ptr(), buf, c.resp_body.len()) };
                if !used.is_null() {
                    out(used, c.resp_body.len())?;
                }
                Ok(())
            })
        }
    )
}

extern "win64" fn HCHttpCallResponseGetHeader(
    h: usize,
    name: *const u8,
    value: *mut *const u8,
) -> HResult {
    ffi_guard!(CH, "HCHttpCallResponseGetHeader", E_INVALIDARG, {
        let Ok(n) = req_str(name) else {
            return E_INVALIDARG;
        };
        with_call(h, |c| out(value, get_kv(&c.resp_headers, &n)))
    })
}

extern "win64" fn HCHttpCallResponseGetNumHeaders(h: usize, num: *mut u32) -> HResult {
    ffi_guard!(CH, "HCHttpCallResponseGetNumHeaders", E_INVALIDARG, {
        with_call(h, |c| out(num, c.resp_headers.len() as u32))
    })
}

extern "win64" fn HCHttpCallResponseGetHeaderAtIndex(
    h: usize,
    idx: u32,
    name: *mut *const u8,
    value: *mut *const u8,
) -> HResult {
    ffi_guard!(CH, "HCHttpCallResponseGetHeaderAtIndex", E_INVALIDARG, {
        with_call(h, |c| header_at(&c.resp_headers, idx, name, value))
    })
}

// ---------- Traces ----------

extern "win64" fn HCSettingsSetTraceLevel(level: u32) -> HResult {
    ffi_guard!(CH, "HCSettingsSetTraceLevel", E_INVALIDARG, {
        if level > 5 {
            return E_INVALIDARG;
        }
        TRACE_LEVEL.store(level, Ordering::Release);
        S_OK
    })
}

extern "win64" fn HCSettingsGetTraceLevel(level: *mut u32) -> HResult {
    ffi_guard!(CH, "HCSettingsGetTraceLevel", E_INVALIDARG, {
        hr(out(level, TRACE_LEVEL.load(Ordering::Acquire)))
    })
}

extern "win64" fn HCTraceSetEtwEnabled(enabled: u8) {
    ETW_ENABLED.store(enabled != 0, Ordering::Release);
}

extern "win64" fn HCTraceSetClientCallback(cb: usize) {
    TRACE_CALLBACK.store(cb, Ordering::Release);
}

type TraceCallback = unsafe extern "win64" fn(*const u8, u32, u64, u64, *const u8);

/// `HCTraceImplMessage(area, level, format, ...)` : les arguments variadiques ne sont pas
/// interprétés (le format est journalisé tel quel vers le log ScarletBridge et le callback client).
extern "win64" fn HCTraceImplMessage(area: *const HcTraceArea, level: u32, format: *const u8) {
    ffi_guard!(CH, "HCTraceImplMessage", (), {
        if level == 0 || area.is_null() {
            return;
        }
        // SAFETY: zone de trace invitée valide (contrat libHttpClient).
        let (name_p, verbosity) = unsafe { ((*area).name, (*area).verbosity) };
        if level > verbosity.min(TRACE_LEVEL.load(Ordering::Acquire)) {
            return;
        }
        // SAFETY: chaînes C ou nul.
        let (name, msg) = unsafe {
            (
                cstr_arg(name_p).unwrap_or_default(),
                cstr_arg(format).unwrap_or_default(),
            )
        };
        sb_debug!(CH, "[{name}] (niveau {level}) {msg}");
        let cb = TRACE_CALLBACK.load(Ordering::Acquire);
        if cb != 0 {
            // SAFETY: `HCTraceCallback` Win64 de l'invité.
            let f: TraceCallback = unsafe { std::mem::transmute::<usize, TraceCallback>(cb) };
            // SAFETY: appel système sans pointeur.
            let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u64;
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            let cmsg = cstring(&msg);
            // SAFETY: chaînes valides pendant l'appel.
            unsafe { f(name_p, level, tid, ts, cmsg.as_ptr().cast()) };
        }
    })
}

extern "win64" fn HCTraceImplMessage_v(
    area: *const HcTraceArea,
    level: u32,
    format: *const u8,
    _args: *mut c_void,
) {
    HCTraceImplMessage(area, level, format);
}

// ---------- WebSockets (échec réseau documenté) ----------

static NEXT_WS: AtomicUsize = AtomicUsize::new(0x00AD_0010);
/// Handle WebSocket → (références, en-têtes).
type WsTable = HashMap<usize, (u32, Vec<(CString, CString)>)>;
static WEBSOCKETS: LazyLock<Mutex<WsTable>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static WS_RESULTS: LazyLock<Mutex<HashMap<usize, (usize, HResult)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

extern "win64" fn HCWebSocketCreate(
    out_ws: *mut usize,
    msg: usize,
    bin: usize,
    close: usize,
    ctx: usize,
) -> HResult {
    ffi_guard!(CH, "HCWebSocketCreate", E_INVALIDARG, {
        if out_ws.is_null() {
            return E_INVALIDARG;
        }
        if !INITIALIZED.load(Ordering::Acquire) {
            return E_HC_NOT_INITIALISED;
        }
        let h = NEXT_WS.fetch_add(0x10, Ordering::AcqRel);
        WEBSOCKETS.lock().insert(h, (1, Vec::new()));
        hr(out(out_ws, h))
    })
}

extern "win64" fn HCWebSocketSetHeader(ws: usize, name: *const u8, value: *const u8) -> HResult {
    ffi_guard!(CH, "HCWebSocketSetHeader", E_INVALIDARG, {
        let (n, v) = match (req_str(name), req_str(value)) {
            (Ok(n), Ok(v)) => (n, v),
            _ => return E_INVALIDARG,
        };
        match WEBSOCKETS.lock().get_mut(&ws) {
            Some(e) => {
                set_kv(&mut e.1, &n, &v);
                S_OK
            }
            None => E_INVALIDARG,
        }
    })
}

extern "win64" fn HCWebSocketSetProxyUri(ws: usize, uri: *const u8) -> HResult {
    ffi_guard!(CH, "HCWebSocketSetProxyUri", E_INVALIDARG, {
        if WEBSOCKETS.lock().contains_key(&ws) && !uri.is_null() {
            S_OK
        } else {
            E_INVALIDARG
        }
    })
}

extern "win64" fn HCWebSocketConnectAsync(
    uri: *const u8,
    sub: *const u8,
    ws: usize,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CH, "HCWebSocketConnectAsync", E_INVALIDARG, {
        if uri.is_null() || block.is_null() || !WEBSOCKETS.lock().contains_key(&ws) {
            return E_INVALIDARG;
        }
        // Aucune pile WebSocket : la connexion échoue toujours (offline ou non).
        WS_RESULTS
            .lock()
            .insert(block as usize, (ws, E_HC_NO_NETWORK));
        complete_async(block as usize);
        S_OK
    })
}

extern "win64" fn HCGetWebSocketConnectResult(
    block: *mut XAsyncBlock,
    result: *mut WebSocketCompletionResult,
) -> HResult {
    ffi_guard!(CH, "HCGetWebSocketConnectResult", E_INVALIDARG, {
        if result.is_null() {
            return E_INVALIDARG;
        }
        let Some((ws, err)) = WS_RESULTS.lock().remove(&(block as usize)) else {
            return E_INVALIDARG;
        };
        hr(out(
            result,
            WebSocketCompletionResult {
                websocket: ws,
                error_code: err,
                platform_error_code: http::ERROR_WINHTTP_CANNOT_CONNECT,
            },
        ))
    })
}

extern "win64" fn HCWebSocketSendMessageAsync(
    ws: usize,
    msg: *const u8,
    block: *mut XAsyncBlock,
) -> HResult {
    E_HC_NO_NETWORK
}

extern "win64" fn HCWebSocketSendBinaryMessageAsync(
    ws: usize,
    p: *const u8,
    n: u32,
    block: *mut XAsyncBlock,
) -> HResult {
    E_HC_NO_NETWORK
}

extern "win64" fn HCWebSocketDisconnect(ws: usize) -> HResult {
    E_HC_NO_NETWORK
}

extern "win64" fn HCWebSocketDuplicateHandle(ws: usize) -> usize {
    ffi_guard!(CH, "HCWebSocketDuplicateHandle", 0, {
        match WEBSOCKETS.lock().get_mut(&ws) {
            Some(e) => {
                e.0 += 1;
                ws
            }
            None => 0,
        }
    })
}

extern "win64" fn HCWebSocketCloseHandle(ws: usize) -> HResult {
    ffi_guard!(CH, "HCWebSocketCloseHandle", E_INVALIDARG, {
        let mut m = WEBSOCKETS.lock();
        let Some(e) = m.get_mut(&ws) else {
            return E_INVALIDARG;
        };
        e.0 = e.0.saturating_sub(1);
        if e.0 == 0 {
            m.remove(&ws);
        }
        S_OK
    })
}

pub(crate) fn exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($($f:ident),* $(,)?) => { $( m.insert(stringify!($f), $f as *const () as usize); )* };
    }
    reg!(
        HCMemSetFunctions,
        HCMemGetFunctions,
        HCInitialize,
        HCIsInitialized,
        HCCleanup,
        HCCleanupAsync,
        HCGetLibVersion,
        HCHttpCallCreate,
        HCHttpCallDuplicateHandle,
        HCHttpCallCloseHandle,
        HCHttpCallGetId,
        HCHttpCallSetTracing,
        HCHttpCallGetRequestUrl,
        HCHttpCallSetContext,
        HCHttpCallGetContext,
        HCHttpCallGetPerformCount,
        HCHttpCallPerformAsync,
        HCHttpCallRequestSetUrl,
        HCHttpCallRequestGetUrl,
        HCHttpCallRequestSetRequestBodyBytes,
        HCHttpCallRequestSetRequestBodyString,
        HCHttpCallRequestGetRequestBodyBytes,
        HCHttpCallRequestGetRequestBodyString,
        HCHttpCallRequestSetHeader,
        HCHttpCallRequestGetHeader,
        HCHttpCallRequestGetNumHeaders,
        HCHttpCallRequestGetHeaderAtIndex,
        HCHttpCallRequestSetRetryAllowed,
        HCHttpCallRequestGetRetryAllowed,
        HCHttpCallRequestSetRetryCacheId,
        HCHttpCallRequestGetRetryCacheId,
        HCHttpCallRequestSetRetryDelay,
        HCHttpCallRequestGetRetryDelay,
        HCHttpCallRequestSetTimeout,
        HCHttpCallRequestGetTimeout,
        HCHttpCallRequestSetTimeoutWindow,
        HCHttpCallRequestGetTimeoutWindow,
        HCHttpCallResponseGetStatusCode,
        HCHttpCallResponseGetNetworkErrorCode,
        HCHttpCallResponseGetPlatformNetworkErrorMessage,
        HCHttpCallResponseGetResponseString,
        HCHttpCallResponseGetResponseBodyBytesSize,
        HCHttpCallResponseGetResponseBodyBytes,
        HCHttpCallResponseGetHeader,
        HCHttpCallResponseGetNumHeaders,
        HCHttpCallResponseGetHeaderAtIndex,
        HCSettingsSetTraceLevel,
        HCSettingsGetTraceLevel,
        HCTraceSetEtwEnabled,
        HCTraceSetClientCallback,
        HCTraceImplMessage,
        HCTraceImplMessage_v,
        HCWebSocketCreate,
        HCWebSocketSetHeader,
        HCWebSocketSetProxyUri,
        HCWebSocketConnectAsync,
        HCGetWebSocketConnectResult,
        HCWebSocketSendMessageAsync,
        HCWebSocketSendBinaryMessageAsync,
        HCWebSocketDisconnect,
        HCWebSocketDuplicateHandle,
        HCWebSocketCloseHandle,
    );
    m.insert(
        "HCGlobalGetLibVersion",
        HCGetLibVersion as *const () as usize,
    );
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Instant;

    fn s(p: *const u8) -> Option<String> {
        // SAFETY: chaîne C produite par l'implémentation (ou nul).
        unsafe { cstr_arg(p) }
    }

    fn new_block() -> Box<XAsyncBlock> {
        Box::new(XAsyncBlock {
            queue: 0,
            context: ptr::null_mut(),
            callback: None,
            internal: [0; 32],
        })
    }

    static COMPLETED: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "win64" fn on_complete(b: *mut XAsyncBlock) {
        COMPLETED.store(b as usize, Ordering::SeqCst);
    }

    /// Un seul test pour l'état global (HCInitialize / HCCleanup) afin d'éviter les courses.
    #[test]
    fn hc_lifecycle_headers_and_perform() {
        let mut h = 0usize;
        if !INITIALIZED.load(Ordering::Acquire) {
            assert_eq!(HCHttpCallCreate(&mut h), E_HC_NOT_INITIALISED);
        }
        assert_eq!(HCInitialize(ptr::null_mut()), S_OK);
        assert_eq!(HCInitialize(ptr::null_mut()), E_HC_ALREADY_INITIALISED);
        assert_eq!(HCIsInitialized(), 1);
        let mut ver: *const u8 = ptr::null();
        assert_eq!(HCGetLibVersion(&mut ver), S_OK);
        assert!(s(ver).is_some());

        assert_eq!(HCHttpCallCreate(&mut h), S_OK);
        assert_ne!(h, 0);
        assert_eq!(
            HCHttpCallRequestSetUrl(
                h,
                c"POST".as_ptr().cast(),
                c"https://example.com/api".as_ptr().cast()
            ),
            S_OK
        );
        let (mut m, mut u): (*const u8, *const u8) = (ptr::null(), ptr::null());
        assert_eq!(HCHttpCallRequestGetUrl(h, &mut m, &mut u), S_OK);
        assert_eq!(s(m).as_deref(), Some("POST"));
        assert_eq!(s(u).as_deref(), Some("https://example.com/api"));
        let mut ru: *const u8 = ptr::null();
        assert_eq!(HCHttpCallGetRequestUrl(h, &mut ru), S_OK);
        assert_eq!(ru, u);

        // En-têtes (remplacement insensible à la casse, ordre trié).
        assert_eq!(
            HCHttpCallRequestSetHeader(h, c"X-B".as_ptr().cast(), c"2".as_ptr().cast(), 1),
            S_OK
        );
        assert_eq!(
            HCHttpCallRequestSetHeader(h, c"x-a".as_ptr().cast(), c"1".as_ptr().cast(), 1),
            S_OK
        );
        assert_eq!(
            HCHttpCallRequestSetHeader(h, c"X-A".as_ptr().cast(), c"3".as_ptr().cast(), 1),
            S_OK
        );
        let mut n = 0u32;
        assert_eq!(HCHttpCallRequestGetNumHeaders(h, &mut n), S_OK);
        assert_eq!(n, 2);
        let (mut hn, mut hv): (*const u8, *const u8) = (ptr::null(), ptr::null());
        assert_eq!(
            HCHttpCallRequestGetHeaderAtIndex(h, 0, &mut hn, &mut hv),
            S_OK
        );
        assert_eq!(
            (s(hn).as_deref(), s(hv).as_deref()),
            (Some("x-a"), Some("3"))
        );
        assert_eq!(
            HCHttpCallRequestGetHeader(h, c"X-B".as_ptr().cast(), &mut hv),
            S_OK
        );
        assert_eq!(s(hv).as_deref(), Some("2"));
        assert_eq!(
            HCHttpCallRequestGetHeader(h, c"none".as_ptr().cast(), &mut hv),
            S_OK
        );
        assert!(hv.is_null());

        // Corps.
        assert_eq!(
            HCHttpCallRequestSetRequestBodyString(h, c"{\"a\":1}".as_ptr().cast()),
            S_OK
        );
        let (mut bp, mut bl): (*const u8, u32) = (ptr::null(), 0);
        assert_eq!(
            HCHttpCallRequestGetRequestBodyBytes(h, &mut bp, &mut bl),
            S_OK
        );
        assert_eq!(bl, 7);

        // Retry / timeouts (par appel et défauts globaux).
        assert_eq!(HCHttpCallRequestSetTimeout(h, 5), S_OK);
        let mut t = 0u32;
        assert_eq!(HCHttpCallRequestGetTimeout(h, &mut t), S_OK);
        assert_eq!(t, 5);
        assert_eq!(HCHttpCallRequestGetTimeoutWindow(0, &mut t), S_OK);
        assert_eq!(t, 20);
        let mut ra = 0u8;
        assert_eq!(HCHttpCallRequestGetRetryAllowed(h, &mut ra), S_OK);
        assert_eq!(ra, 1);
        assert_eq!(HCHttpCallRequestSetRetryCacheId(h, 42), S_OK);
        assert_eq!(HCHttpCallRequestGetRetryCacheId(h, &mut t), S_OK);
        assert_eq!(t, 42);
        assert_eq!(HCHttpCallRequestSetRetryDelay(h, 9), S_OK);
        assert_eq!(HCHttpCallRequestGetRetryDelay(h, &mut t), S_OK);
        assert_eq!(t, 9);

        // Refcount.
        assert_eq!(HCHttpCallDuplicateHandle(h), h);
        assert_eq!(HCHttpCallCloseHandle(h), S_OK);

        // Perform offline.
        if offline_mode() {
            let mut b = new_block();
            b.callback = Some(on_complete);
            let bp_addr = (&raw mut *b) as usize;
            assert_eq!(HCHttpCallPerformAsync(h, &raw mut *b), S_OK);
            assert_eq!(
                HCHttpCallPerformAsync(h, &raw mut *b),
                E_HC_PERFORM_ALREADY_CALLED
            );
            assert_eq!(
                HCHttpCallRequestSetHeader(h, c"late".as_ptr().cast(), c"x".as_ptr().cast(), 0),
                E_HC_PERFORM_ALREADY_CALLED
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            while COMPLETED.load(Ordering::SeqCst) != bp_addr {
                assert!(Instant::now() < deadline, "complétion XAsync non reçue");
                std::thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(b.internal[0], 1);
            let (mut ne, mut pe) = (0i32, 0u32);
            assert_eq!(
                HCHttpCallResponseGetNetworkErrorCode(h, &mut ne, &mut pe),
                S_OK
            );
            assert_eq!(ne, E_HC_NO_NETWORK);
            assert_eq!(pe, http::ERROR_WINHTTP_NAME_NOT_RESOLVED);
            let mut st = 99u32;
            assert_eq!(HCHttpCallResponseGetStatusCode(h, &mut st), S_OK);
            assert_eq!(st, 0);
            let mut msg: *const u8 = ptr::null();
            assert_eq!(
                HCHttpCallResponseGetPlatformNetworkErrorMessage(h, &mut msg),
                S_OK
            );
            assert!(s(msg).is_some_and(|m| m.contains("offline")));
            let mut sz = 1usize;
            assert_eq!(HCHttpCallResponseGetResponseBodyBytesSize(h, &mut sz), S_OK);
            assert_eq!(sz, 0);
        }
        assert_eq!(HCHttpCallCloseHandle(h), S_OK);
        assert_eq!(HCHttpCallCloseHandle(h), E_INVALIDARG);

        // Perform loopback : réponse réelle.
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = l.local_addr().expect("addr").port();
        let th = std::thread::spawn(move || {
            let (mut sock, _) = l.accept().expect("accept");
            let mut buf = [0u8; 2048];
            let n = sock.read(&mut buf).expect("read");
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-Srv: sb\r\n\r\nhello")
                .expect("write");
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        let mut h2 = 0usize;
        assert_eq!(HCHttpCallCreate(&mut h2), S_OK);
        let url = CString::new(format!("http://127.0.0.1:{port}/ping")).expect("url");
        assert_eq!(
            HCHttpCallRequestSetUrl(h2, c"GET".as_ptr().cast(), url.as_ptr().cast()),
            S_OK
        );
        let mut b2 = new_block();
        assert_eq!(HCHttpCallPerformAsync(h2, &raw mut *b2), S_OK);
        assert_eq!(b2.internal[0], 1);
        let req = th.join().expect("join");
        assert!(req.starts_with("GET /ping HTTP/1.1\r\n"));
        let mut st = 0u32;
        assert_eq!(HCHttpCallResponseGetStatusCode(h2, &mut st), S_OK);
        assert_eq!(st, 200);
        let mut body: *const u8 = ptr::null();
        assert_eq!(HCHttpCallResponseGetResponseString(h2, &mut body), S_OK);
        assert_eq!(s(body).as_deref(), Some("hello"));
        let mut small = [0u8; 2];
        let mut used = 0usize;
        assert_eq!(
            HCHttpCallResponseGetResponseBodyBytes(h2, 2, small.as_mut_ptr(), &mut used),
            E_NOT_SUFFICIENT_BUFFER
        );
        let mut big = [0u8; 8];
        assert_eq!(
            HCHttpCallResponseGetResponseBodyBytes(h2, 8, big.as_mut_ptr(), &mut used),
            S_OK
        );
        assert_eq!(&big[..used], b"hello");
        let mut rv: *const u8 = ptr::null();
        assert_eq!(
            HCHttpCallResponseGetHeader(h2, c"x-srv".as_ptr().cast(), &mut rv),
            S_OK
        );
        assert_eq!(s(rv).as_deref(), Some("sb"));
        let mut nh = 0u32;
        assert_eq!(HCHttpCallResponseGetNumHeaders(h2, &mut nh), S_OK);
        assert_eq!(nh, 2);
        let (mut rn, mut rv2): (*const u8, *const u8) = (ptr::null(), ptr::null());
        assert_eq!(
            HCHttpCallResponseGetHeaderAtIndex(h2, 5, &mut rn, &mut rv2),
            S_OK
        );
        assert!(rn.is_null() && rv2.is_null());
        assert_eq!(HCHttpCallCloseHandle(h2), S_OK);

        // WebSocket : échec réseau documenté.
        let mut ws = 0usize;
        assert_eq!(HCWebSocketCreate(&mut ws, 0, 0, 0, 0), S_OK);
        let mut wb = new_block();
        assert_eq!(
            HCWebSocketConnectAsync(
                c"ws://localhost/".as_ptr().cast(),
                c"".as_ptr().cast(),
                ws,
                &raw mut *wb
            ),
            S_OK
        );
        let mut res = WebSocketCompletionResult {
            websocket: 0,
            error_code: 0,
            platform_error_code: 0,
        };
        assert_eq!(HCGetWebSocketConnectResult(&raw mut *wb, &mut res), S_OK);
        assert_eq!((res.websocket, res.error_code), (ws, E_HC_NO_NETWORK));
        assert_eq!(
            HCWebSocketSendMessageAsync(ws, c"x".as_ptr().cast(), &raw mut *wb),
            E_HC_NO_NETWORK
        );
        assert_eq!(HCWebSocketCloseHandle(ws), S_OK);

        // Traces.
        assert_eq!(HCSettingsSetTraceLevel(4), S_OK);
        let mut lv = 0u32;
        assert_eq!(HCSettingsGetTraceLevel(&mut lv), S_OK);
        assert_eq!(lv, 4);
        assert_eq!(HCSettingsSetTraceLevel(9), E_INVALIDARG);
        let area = HcTraceArea {
            name: c"HTTPCLIENT".as_ptr().cast(),
            verbosity: 5,
        };
        HCTraceSetEtwEnabled(0);
        HCTraceImplMessage(&area, 3, c"message %s".as_ptr().cast());

        // Cleanup asynchrone.
        let mut cb = new_block();
        assert_eq!(HCCleanupAsync(&raw mut *cb), S_OK);
        assert_eq!(cb.internal[0], 1);
        assert_eq!(HCIsInitialized(), 0);
        let mut h3 = 0usize;
        assert_eq!(HCHttpCallCreate(&mut h3), E_HC_NOT_INITIALISED);
    }
}
