//! `winhttp.dll` : arborescence de handles session → connect → request, client HTTP/1.1 en clair
//! (pas de TLS : `https` → `ERROR_WINHTTP_SECURE_FAILURE`), mode synchrone et asynchrone
//! (`WINHTTP_FLAG_ASYNC` : complétions notifiées depuis un thread de travail via le callback).
//!
//! Offline strict : `WinHttpSendRequest` échoue avec `ERROR_WINHTTP_NAME_NOT_RESOLVED` (nom non
//! local) ou `ERROR_WINHTTP_CANNOT_CONNECT` (IP non loopback) ; le loopback reste utilisable.

#![allow(non_snake_case, unused_variables, clippy::too_many_arguments)]

use core::ffi::c_void;
use std::collections::HashMap;
use std::mem::{offset_of, size_of};
use std::net::TcpStream;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use parking_lot::Mutex;
use sb_common::abi::{WIN_FALSE, WIN_TRUE, WinBool};
use sb_common::{ffi_guard, sb_debug, sb_trace};

use super::http::{self, HttpResponse, Timeouts};
use super::{set_error, spawn_guest_worker, wstr_arg};

const CH: &str = "winhttp";

// ---------- Codes d'erreur ----------
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_WINHTTP_INVALID_OPTION: u32 = 12009;
const ERROR_WINHTTP_OPTION_NOT_SETTABLE: u32 = 12011;
const ERROR_WINHTTP_INCORRECT_HANDLE_TYPE: u32 = 12018;
const ERROR_WINHTTP_INCORRECT_HANDLE_STATE: u32 = 12019;
const ERROR_WINHTTP_HEADER_NOT_FOUND: u32 = 12150;
const ERROR_WINHTTP_INVALID_HEADER: u32 = 12153;
const ERROR_WINHTTP_INVALID_QUERY_REQUEST: u32 = 12154;
const ERROR_WINHTTP_HEADER_ALREADY_EXISTS: u32 = 12155;

// ---------- Constantes ----------
const WINHTTP_FLAG_ASYNC: u32 = 0x1000_0000;
const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;

const HANDLE_TYPE_SESSION: u32 = 1;
const HANDLE_TYPE_CONNECT: u32 = 2;
const HANDLE_TYPE_REQUEST: u32 = 3;

const ADDREQ_FLAG_ADD_IF_NEW: u32 = 0x1000_0000;
const ADDREQ_FLAG_ADD: u32 = 0x2000_0000;
const ADDREQ_FLAG_COALESCE_WITH_COMMA: u32 = 0x4000_0000;
const ADDREQ_FLAG_COALESCE_WITH_SEMICOLON: u32 = 0x0100_0000;
const ADDREQ_FLAG_REPLACE: u32 = 0x8000_0000;

const QUERY_FLAG_REQUEST_HEADERS: u32 = 0x8000_0000;
const QUERY_FLAG_SYSTEMTIME: u32 = 0x4000_0000;
const QUERY_FLAG_NUMBER: u32 = 0x2000_0000;
const QUERY_FLAG_NUMBER64: u32 = 0x0800_0000;
const QUERY_VERSION: u32 = 18;
const QUERY_STATUS_CODE: u32 = 19;
const QUERY_STATUS_TEXT: u32 = 20;
const QUERY_RAW_HEADERS: u32 = 21;
const QUERY_RAW_HEADERS_CRLF: u32 = 22;
const QUERY_REQUEST_METHOD: u32 = 45;
const QUERY_CUSTOM: u32 = 65535;

const STATUS_HANDLE_CLOSING: u32 = 0x0000_0800;
const STATUS_HEADERS_AVAILABLE: u32 = 0x0002_0000;
const STATUS_DATA_AVAILABLE: u32 = 0x0004_0000;
const STATUS_READ_COMPLETE: u32 = 0x0008_0000;
const STATUS_WRITE_COMPLETE: u32 = 0x0010_0000;
const STATUS_REQUEST_ERROR: u32 = 0x0020_0000;
const STATUS_SENDREQUEST_COMPLETE: u32 = 0x0040_0000;

const API_RECEIVE_RESPONSE: usize = 1;
const API_QUERY_DATA_AVAILABLE: usize = 2;
const API_READ_DATA: usize = 3;
const API_WRITE_DATA: usize = 4;
const API_SEND_REQUEST: usize = 5;

const OPTION_RESOLVE_TIMEOUT: u32 = 2;
const OPTION_CONNECT_TIMEOUT: u32 = 3;
const OPTION_SEND_TIMEOUT: u32 = 5;
const OPTION_RECEIVE_TIMEOUT: u32 = 6;
const OPTION_RECEIVE_RESPONSE_TIMEOUT: u32 = 7;
const OPTION_HANDLE_TYPE: u32 = 9;
const OPTION_PARENT_HANDLE: u32 = 21;
const OPTION_URL: u32 = 34;
const OPTION_USER_AGENT: u32 = 41;
const OPTION_CONTEXT_VALUE: u32 = 45;
/// Options mémorisées telles quelles (DWORD ou blob) : sans effet sur le client en clair.
const STORED_OPTIONS: &[u32] = &[
    4,   // CONNECT_RETRIES
    31,  // SECURITY_FLAGS
    38,  // PROXY
    47,  // CLIENT_CERT_CONTEXT
    63,  // DISABLE_FEATURE
    73,  // MAX_CONNS_PER_SERVER
    74,  // MAX_CONNS_PER_1_0_SERVER
    77,  // AUTOLOGON_POLICY
    79,  // ENABLE_FEATURE
    84,  // SECURE_PROTOCOLS
    88,  // REDIRECT_POLICY
    89,  // MAX_HTTP_AUTOMATIC_REDIRECTS
    91,  // MAX_RESPONSE_HEADER_SIZE
    92,  // MAX_RESPONSE_DRAIN_SIZE
    114, // UPGRADE_TO_WEB_SOCKET
    118, // DECOMPRESSION
    133, // ENABLE_HTTP_PROTOCOL
];

// ---------- Structures ----------

/// `URL_COMPONENTS` (x64).
#[repr(C)]
pub(crate) struct UrlComponents {
    pub dw_struct_size: u32,
    pub lpsz_scheme: *mut u16,
    pub dw_scheme_length: u32,
    pub n_scheme: i32,
    pub lpsz_host_name: *mut u16,
    pub dw_host_name_length: u32,
    pub n_port: u16,
    pub lpsz_user_name: *mut u16,
    pub dw_user_name_length: u32,
    pub lpsz_password: *mut u16,
    pub dw_password_length: u32,
    pub lpsz_url_path: *mut u16,
    pub dw_url_path_length: u32,
    pub lpsz_extra_info: *mut u16,
    pub dw_extra_info_length: u32,
}
const _: () = assert!(size_of::<UrlComponents>() == 104);
const _: () = assert!(offset_of!(UrlComponents, n_scheme) == 20);
const _: () = assert!(offset_of!(UrlComponents, n_port) == 36);
const _: () = assert!(offset_of!(UrlComponents, lpsz_url_path) == 72);
const _: () = assert!(offset_of!(UrlComponents, dw_extra_info_length) == 96);

/// `WINHTTP_ASYNC_RESULT` (x64).
#[repr(C)]
pub(crate) struct AsyncResult {
    pub dw_result: usize,
    pub dw_error: u32,
}
const _: () = assert!(size_of::<AsyncResult>() == 16);

// ---------- Objets ----------

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum ReqState {
    Open,
    Sent,
    Received,
}

struct Req {
    verb: String,
    path: String,
    version: String,
    secure: bool,
    headers: Vec<(String, String)>,
    state: ReqState,
    stream: Option<TcpStream>,
    remaining_write: u64,
    resp: Option<HttpResponse>,
    read_pos: usize,
}

struct Obj {
    kind: u32,
    parent: usize,
    is_async: bool,
    callback: usize,
    notify: u32,
    context: usize,
    timeouts: Timeouts,
    resolve_timeout: u32,
    agent: String,
    server: String,
    port: u16,
    options: HashMap<u32, Vec<u8>>,
    req: Option<Req>,
}

type ObjRef = Arc<Mutex<Obj>>;

static NEXT_HANDLE: AtomicUsize = AtomicUsize::new(0x00CC_0010);
static HANDLES: LazyLock<Mutex<HashMap<usize, ObjRef>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn fail(code: u32) -> WinBool {
    set_error(code);
    WIN_FALSE
}

fn get(h: usize) -> Result<ObjRef, u32> {
    HANDLES.lock().get(&h).cloned().ok_or(ERROR_INVALID_HANDLE)
}

fn get_kind(h: usize, kind: u32) -> Result<ObjRef, u32> {
    let o = get(h)?;
    if o.lock().kind != kind {
        return Err(ERROR_WINHTTP_INCORRECT_HANDLE_TYPE);
    }
    Ok(o)
}

fn register(o: Obj) -> usize {
    let h = NEXT_HANDLE.fetch_add(0x10, Ordering::AcqRel);
    HANDLES.lock().insert(h, Arc::new(Mutex::new(o)));
    h
}

fn new_obj(kind: u32, parent: Option<&Obj>, parent_h: usize) -> Obj {
    Obj {
        kind,
        parent: parent_h,
        is_async: parent.is_some_and(|p| p.is_async),
        callback: parent.map_or(0, |p| p.callback),
        notify: parent.map_or(0, |p| p.notify),
        context: 0,
        timeouts: parent.map_or_else(Timeouts::default, |p| p.timeouts),
        resolve_timeout: parent.map_or(0, |p| p.resolve_timeout),
        agent: parent.map(|p| p.agent.clone()).unwrap_or_default(),
        server: parent.map(|p| p.server.clone()).unwrap_or_default(),
        port: parent.map_or(0, |p| p.port),
        options: HashMap::new(),
        req: None,
    }
}

/// Lit une chaîne UTF-16 de longueur `len` (`u32::MAX` = terminée par un nul).
///
/// # Safety
/// `p` nul ou valide sur la longueur indiquée.
unsafe fn wstr_len(p: *const u16, len: u32) -> Option<Vec<u16>> {
    if p.is_null() {
        return None;
    }
    if len == u32::MAX || len == 0 {
        let mut n = 0usize;
        // SAFETY: chaîne terminée par un nul (contrat appelant).
        while unsafe { *p.add(n) } != 0 {
            n += 1;
        }
        // SAFETY: `n` unités lisibles.
        Some(unsafe { std::slice::from_raw_parts(p, n) }.to_vec())
    } else {
        // SAFETY: `len` unités lisibles (contrat appelant).
        Some(unsafe { std::slice::from_raw_parts(p, len as usize) }.to_vec())
    }
}

// ---------- Callbacks ----------

#[derive(Clone, Copy)]
enum Info {
    None,
    Dword(u32),
    GuestPtr(usize, u32),
}

type StatusCallback = unsafe extern "win64" fn(usize, usize, u32, *mut c_void, u32);

fn invoke_callback(cb: usize, h: usize, ctx: usize, status: u32, info: Info) {
    if cb == 0 {
        return;
    }
    // SAFETY: `WINHTTP_STATUS_CALLBACK` (HINTERNET, DWORD_PTR, DWORD, LPVOID, DWORD) Win64.
    let f: StatusCallback = unsafe { std::mem::transmute::<usize, StatusCallback>(cb) };
    match info {
        Info::None => {
            // SAFETY: callback invité, information nulle.
            unsafe { f(h, ctx, status, ptr::null_mut(), 0) }
        }
        Info::Dword(mut v) => {
            // SAFETY: `v` vit pendant l'appel.
            unsafe { f(h, ctx, status, (&raw mut v).cast(), 4) }
        }
        Info::GuestPtr(p, n) => {
            // SAFETY: tampon invité transmis tel quel.
            unsafe { f(h, ctx, status, p as *mut c_void, n) }
        }
    }
}

type Op = Box<dyn FnOnce(&mut Obj) -> Result<(u32, Info), u32> + Send>;

/// Exécute une opération de requête en mode synchrone ou asynchrone.
fn run_op(h: usize, obj: ObjRef, api: usize, op: Op) -> WinBool {
    let is_async = obj.lock().is_async;
    if !is_async {
        let r = {
            let mut g = obj.lock();
            op(&mut g)
        };
        return match r {
            Ok(_) => WIN_TRUE,
            Err(e) => fail(e),
        };
    }
    spawn_guest_worker("sb-winhttp", move || {
        let (res, cb, notify, ctx) = {
            let mut g = obj.lock();
            let r = op(&mut g);
            (r, g.callback, g.notify, g.context)
        };
        match res {
            Ok((status, info)) => {
                if notify & status != 0 {
                    invoke_callback(cb, h, ctx, status, info);
                }
            }
            Err(e) => {
                sb_debug!(CH, "requête {h:#x} : échec asynchrone api={api} err={e}");
                if notify & STATUS_REQUEST_ERROR != 0 {
                    let mut ar = AsyncResult {
                        dw_result: api,
                        dw_error: e,
                    };
                    invoke_callback(
                        cb,
                        h,
                        ctx,
                        STATUS_REQUEST_ERROR,
                        Info::GuestPtr((&raw mut ar) as usize, size_of::<AsyncResult>() as u32),
                    );
                }
            }
        }
    });
    WIN_TRUE
}

// ---------- Session / connexion / requête ----------

extern "win64" fn WinHttpOpen(
    agent: *const u16,
    access: u32,
    proxy: *const u16,
    bypass: *const u16,
    flags: u32,
) -> usize {
    ffi_guard!(CH, "WinHttpOpen", 0, {
        if !matches!(access, 0 | 1 | 3 | 4) || (access == 3 && proxy.is_null()) {
            set_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        let mut o = new_obj(HANDLE_TYPE_SESSION, None, 0);
        // SAFETY: chaîne UTF-16 ou nul.
        o.agent = unsafe { wstr_arg(agent) }.unwrap_or_default();
        o.is_async = flags & WINHTTP_FLAG_ASYNC != 0;
        let h = register(o);
        sb_debug!(
            CH,
            "WinHttpOpen(async={}) -> {h:#x}",
            flags & WINHTTP_FLAG_ASYNC != 0
        );
        set_error(0);
        h
    })
}

extern "win64" fn WinHttpConnect(
    session: usize,
    server: *const u16,
    port: u16,
    reserved: u32,
) -> usize {
    ffi_guard!(CH, "WinHttpConnect", 0, {
        let s = match get_kind(session, HANDLE_TYPE_SESSION) {
            Ok(s) => s,
            Err(e) => {
                set_error(e);
                return 0;
            }
        };
        // SAFETY: chaîne UTF-16 ou nul.
        let Some(name) = (unsafe { wstr_arg(server) }).filter(|n| !n.is_empty()) else {
            set_error(ERROR_INVALID_PARAMETER);
            return 0;
        };
        let mut o = {
            let g = s.lock();
            new_obj(HANDLE_TYPE_CONNECT, Some(&g), session)
        };
        o.server = name;
        o.port = port;
        set_error(0);
        register(o)
    })
}

extern "win64" fn WinHttpOpenRequest(
    connect: usize,
    verb: *const u16,
    object: *const u16,
    version: *const u16,
    referrer: *const u16,
    accept_types: *const *const u16,
    flags: u32,
) -> usize {
    ffi_guard!(CH, "WinHttpOpenRequest", 0, {
        let c = match get_kind(connect, HANDLE_TYPE_CONNECT) {
            Ok(c) => c,
            Err(e) => {
                set_error(e);
                return 0;
            }
        };
        // SAFETY: chaînes UTF-16 ou nul (contrat WinHTTP).
        let (verb, object, version, referrer) = unsafe {
            (
                wstr_arg(verb),
                wstr_arg(object),
                wstr_arg(version),
                wstr_arg(referrer),
            )
        };
        let mut path = object
            .filter(|o| !o.is_empty())
            .unwrap_or_else(|| "/".into());
        if !path.starts_with('/') && !path.contains("://") {
            path.insert(0, '/');
        }
        let mut headers = Vec::new();
        if !accept_types.is_null() {
            let mut list = Vec::new();
            let mut i = 0;
            loop {
                // SAFETY: tableau de chaînes terminé par NULL (contrat WinHTTP).
                let p = unsafe { *accept_types.add(i) };
                if p.is_null() {
                    break;
                }
                // SAFETY: chaîne UTF-16 terminée.
                if let Some(s) = unsafe { wstr_arg(p) } {
                    list.push(s);
                }
                i += 1;
            }
            if !list.is_empty() {
                headers.push(("Accept".to_string(), list.join(", ")));
            }
        }
        if let Some(r) = referrer.filter(|r| !r.is_empty()) {
            headers.push(("Referer".to_string(), r));
        }
        let mut o = {
            let g = c.lock();
            new_obj(HANDLE_TYPE_REQUEST, Some(&g), connect)
        };
        o.req = Some(Req {
            verb: verb
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "GET".into()),
            path,
            version: version
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "HTTP/1.1".into()),
            secure: flags & WINHTTP_FLAG_SECURE != 0,
            headers,
            state: ReqState::Open,
            stream: None,
            remaining_write: 0,
            resp: None,
            read_pos: 0,
        });
        set_error(0);
        register(o)
    })
}

fn apply_header(
    headers: &mut Vec<(String, String)>,
    name: &str,
    value: &str,
    modifiers: u32,
) -> Result<(), u32> {
    let pos = headers
        .iter()
        .position(|(k, _)| k.eq_ignore_ascii_case(name));
    if modifiers & ADDREQ_FLAG_REPLACE != 0 {
        match pos {
            Some(i) if value.is_empty() => {
                headers.remove(i);
            }
            Some(i) => headers[i].1 = value.to_string(),
            None if value.is_empty() => {}
            None if modifiers & ADDREQ_FLAG_ADD != 0 => headers.push((name.into(), value.into())),
            None => return Err(ERROR_WINHTTP_HEADER_NOT_FOUND),
        }
        return Ok(());
    }
    if modifiers & ADDREQ_FLAG_ADD_IF_NEW != 0 {
        if pos.is_some() {
            return Err(ERROR_WINHTTP_HEADER_ALREADY_EXISTS);
        }
        headers.push((name.into(), value.into()));
        return Ok(());
    }
    let coalesce = if modifiers & ADDREQ_FLAG_COALESCE_WITH_SEMICOLON != 0 {
        Some("; ")
    } else if modifiers & ADDREQ_FLAG_COALESCE_WITH_COMMA != 0 {
        Some(", ")
    } else {
        None
    };
    match (coalesce, pos) {
        (Some(sep), Some(i)) => {
            headers[i].1.push_str(sep);
            headers[i].1.push_str(value);
        }
        _ => headers.push((name.into(), value.into())),
    }
    Ok(())
}

fn apply_header_block(
    headers: &mut Vec<(String, String)>,
    block: &str,
    modifiers: u32,
) -> Result<(), u32> {
    for line in block.split(['\r', '\n']) {
        if line.trim().is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            return Err(ERROR_WINHTTP_INVALID_HEADER);
        };
        let k = k.trim();
        if k.is_empty() {
            return Err(ERROR_WINHTTP_INVALID_HEADER);
        }
        apply_header(headers, k, v.trim(), modifiers)?;
    }
    Ok(())
}

extern "win64" fn WinHttpAddRequestHeaders(
    req: usize,
    headers: *const u16,
    len: u32,
    modifiers: u32,
) -> WinBool {
    ffi_guard!(CH, "WinHttpAddRequestHeaders", WIN_FALSE, {
        let o = match get_kind(req, HANDLE_TYPE_REQUEST) {
            Ok(o) => o,
            Err(e) => return fail(e),
        };
        // SAFETY: chaîne UTF-16 de longueur `len` (ou terminée) – contrat WinHTTP.
        let Some(w) = (unsafe { wstr_len(headers, len) }) else {
            return fail(ERROR_INVALID_PARAMETER);
        };
        let block = String::from_utf16_lossy(&w);
        let mut g = o.lock();
        let Some(r) = g.req.as_mut() else {
            return fail(ERROR_WINHTTP_INCORRECT_HANDLE_TYPE);
        };
        match apply_header_block(&mut r.headers, &block, modifiers) {
            Ok(()) => WIN_TRUE,
            Err(e) => fail(e),
        }
    })
}

fn do_send(o: &mut Obj, body: &[u8], total: u64) -> Result<(), u32> {
    let server = o.server.clone();
    let port = o.port;
    let agent = o.agent.clone();
    let t = o.timeouts;
    let r = o.req.as_mut().ok_or(ERROR_WINHTTP_INCORRECT_HANDLE_TYPE)?;
    let port = if port == 0 {
        if r.secure { 443 } else { 80 }
    } else {
        port
    };
    r.stream = None;
    r.resp = None;
    r.read_pos = 0;
    let mut stream = http::connect(&server, port, r.secure, &t)?;
    let mut hdrs: Vec<(String, String)> = Vec::new();
    if !r
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("Host"))
    {
        hdrs.push(("Host".into(), http::host_header(&server, port, r.secure)));
    }
    if !agent.is_empty()
        && !r
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("User-Agent"))
    {
        hdrs.push(("User-Agent".into(), agent));
    }
    hdrs.extend(r.headers.iter().cloned());
    let needs_len =
        total > 0 || r.verb.eq_ignore_ascii_case("POST") || r.verb.eq_ignore_ascii_case("PUT");
    if needs_len
        && !hdrs
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
    {
        hdrs.push(("Content-Length".into(), total.to_string()));
    }
    http::write_head(&mut stream, &r.verb, &r.path, &r.version, &hdrs)?;
    http::write_body(&mut stream, body)?;
    r.remaining_write = total.saturating_sub(body.len() as u64);
    r.stream = Some(stream);
    r.state = ReqState::Sent;
    Ok(())
}

extern "win64" fn WinHttpSendRequest(
    req: usize,
    headers: *const u16,
    headers_len: u32,
    optional: *const u8,
    optional_len: u32,
    total_len: u32,
    context: usize,
) -> WinBool {
    ffi_guard!(CH, "WinHttpSendRequest", WIN_FALSE, {
        let o = match get_kind(req, HANDLE_TYPE_REQUEST) {
            Ok(o) => o,
            Err(e) => return fail(e),
        };
        if total_len < optional_len || (optional.is_null() && optional_len > 0) {
            return fail(ERROR_INVALID_PARAMETER);
        }
        let body = if optional.is_null() || optional_len == 0 {
            Vec::new()
        } else {
            // SAFETY: `optional_len` octets lisibles (contrat WinHTTP) ; copiés pour l'async.
            unsafe { std::slice::from_raw_parts(optional, optional_len as usize) }.to_vec()
        };
        // SAFETY: chaîne UTF-16 ou nul.
        let extra = unsafe { wstr_len(headers, headers_len) };
        {
            let mut g = o.lock();
            g.context = context;
            let Some(r) = g.req.as_mut() else {
                return fail(ERROR_WINHTTP_INCORRECT_HANDLE_TYPE);
            };
            if let Some(w) = extra
                && let Err(e) = apply_header_block(
                    &mut r.headers,
                    &String::from_utf16_lossy(&w),
                    ADDREQ_FLAG_ADD | ADDREQ_FLAG_REPLACE,
                )
            {
                return fail(e);
            }
        }
        sb_trace!(
            CH,
            "WinHttpSendRequest({req:#x}, body={optional_len}/{total_len})"
        );
        let total = u64::from(total_len);
        run_op(
            req,
            o,
            API_SEND_REQUEST,
            Box::new(move |g: &mut Obj| {
                do_send(g, &body, total)?;
                Ok((STATUS_SENDREQUEST_COMPLETE, Info::None))
            }),
        )
    })
}

extern "win64" fn WinHttpWriteData(
    req: usize,
    buf: *const u8,
    len: u32,
    written: *mut u32,
) -> WinBool {
    ffi_guard!(CH, "WinHttpWriteData", WIN_FALSE, {
        let o = match get_kind(req, HANDLE_TYPE_REQUEST) {
            Ok(o) => o,
            Err(e) => return fail(e),
        };
        if buf.is_null() && len > 0 {
            return fail(ERROR_INVALID_PARAMETER);
        }
        let data = if len == 0 {
            Vec::new()
        } else {
            // SAFETY: `len` octets lisibles (contrat WinHTTP).
            unsafe { std::slice::from_raw_parts(buf, len as usize) }.to_vec()
        };
        let is_async = o.lock().is_async;
        let out = if is_async { 0 } else { written as usize };
        run_op(
            req,
            o,
            API_WRITE_DATA,
            Box::new(move |g: &mut Obj| {
                let r = g.req.as_mut().ok_or(ERROR_WINHTTP_INCORRECT_HANDLE_TYPE)?;
                if r.state != ReqState::Sent {
                    return Err(ERROR_WINHTTP_INCORRECT_HANDLE_STATE);
                }
                let s = r
                    .stream
                    .as_mut()
                    .ok_or(ERROR_WINHTTP_INCORRECT_HANDLE_STATE)?;
                http::write_body(s, &data)?;
                r.remaining_write = r.remaining_write.saturating_sub(data.len() as u64);
                if out != 0 {
                    // SAFETY: pointeur de sortie invité non nul (mode synchrone).
                    unsafe { *(out as *mut u32) = data.len() as u32 };
                }
                Ok((STATUS_WRITE_COMPLETE, Info::Dword(data.len() as u32)))
            }),
        )
    })
}

extern "win64" fn WinHttpReceiveResponse(req: usize, reserved: *mut c_void) -> WinBool {
    ffi_guard!(CH, "WinHttpReceiveResponse", WIN_FALSE, {
        let o = match get_kind(req, HANDLE_TYPE_REQUEST) {
            Ok(o) => o,
            Err(e) => return fail(e),
        };
        if !reserved.is_null() {
            return fail(ERROR_INVALID_PARAMETER);
        }
        run_op(
            req,
            o,
            API_RECEIVE_RESPONSE,
            Box::new(|g: &mut Obj| {
                let r = g.req.as_mut().ok_or(ERROR_WINHTTP_INCORRECT_HANDLE_TYPE)?;
                if r.state != ReqState::Sent || r.remaining_write > 0 {
                    return Err(ERROR_WINHTTP_INCORRECT_HANDLE_STATE);
                }
                let mut s = r
                    .stream
                    .take()
                    .ok_or(ERROR_WINHTTP_INCORRECT_HANDLE_STATE)?;
                let resp = http::read_response(&mut s, &r.verb)?;
                r.resp = Some(resp);
                r.read_pos = 0;
                r.state = ReqState::Received;
                Ok((STATUS_HEADERS_AVAILABLE, Info::None))
            }),
        )
    })
}

extern "win64" fn WinHttpQueryDataAvailable(req: usize, avail: *mut u32) -> WinBool {
    ffi_guard!(CH, "WinHttpQueryDataAvailable", WIN_FALSE, {
        let o = match get_kind(req, HANDLE_TYPE_REQUEST) {
            Ok(o) => o,
            Err(e) => return fail(e),
        };
        let out = avail as usize;
        run_op(
            req,
            o,
            API_QUERY_DATA_AVAILABLE,
            Box::new(move |g: &mut Obj| {
                let r = g.req.as_mut().ok_or(ERROR_WINHTTP_INCORRECT_HANDLE_TYPE)?;
                let resp = r
                    .resp
                    .as_ref()
                    .ok_or(ERROR_WINHTTP_INCORRECT_HANDLE_STATE)?;
                let n = resp.body.len().saturating_sub(r.read_pos) as u32;
                if out != 0 {
                    // SAFETY: pointeur de sortie invité non nul.
                    unsafe { *(out as *mut u32) = n };
                }
                Ok((STATUS_DATA_AVAILABLE, Info::Dword(n)))
            }),
        )
    })
}

extern "win64" fn WinHttpReadData(
    req: usize,
    buf: *mut u8,
    to_read: u32,
    read: *mut u32,
) -> WinBool {
    ffi_guard!(CH, "WinHttpReadData", WIN_FALSE, {
        let o = match get_kind(req, HANDLE_TYPE_REQUEST) {
            Ok(o) => o,
            Err(e) => return fail(e),
        };
        if buf.is_null() && to_read > 0 {
            return fail(ERROR_INVALID_PARAMETER);
        }
        let (dst, out) = (buf as usize, read as usize);
        run_op(
            req,
            o,
            API_READ_DATA,
            Box::new(move |g: &mut Obj| {
                let r = g.req.as_mut().ok_or(ERROR_WINHTTP_INCORRECT_HANDLE_TYPE)?;
                let resp = r
                    .resp
                    .as_ref()
                    .ok_or(ERROR_WINHTTP_INCORRECT_HANDLE_STATE)?;
                let avail = resp.body.len().saturating_sub(r.read_pos);
                let n = avail.min(to_read as usize);
                if n > 0 {
                    // SAFETY: tampon invité de `to_read` octets ≥ `n`.
                    unsafe {
                        ptr::copy_nonoverlapping(
                            resp.body.as_ptr().add(r.read_pos),
                            dst as *mut u8,
                            n,
                        );
                    }
                }
                r.read_pos += n;
                if out != 0 {
                    // SAFETY: pointeur de sortie invité non nul.
                    unsafe { *(out as *mut u32) = n as u32 };
                }
                Ok((STATUS_READ_COMPLETE, Info::GuestPtr(dst, n as u32)))
            }),
        )
    })
}

// ---------- En-têtes de réponse ----------

const HEADER_IDS: &[(u32, &str)] = &[
    (0, "Mime-Version"),
    (1, "Content-Type"),
    (2, "Content-Transfer-Encoding"),
    (3, "Content-ID"),
    (4, "Content-Description"),
    (5, "Content-Length"),
    (6, "Content-Language"),
    (7, "Allow"),
    (8, "Public"),
    (9, "Date"),
    (10, "Expires"),
    (11, "Last-Modified"),
    (12, "Message-ID"),
    (13, "URI"),
    (14, "Derived-From"),
    (15, "Cost"),
    (16, "Link"),
    (17, "Pragma"),
    (23, "Connection"),
    (24, "Accept"),
    (25, "Accept-Charset"),
    (26, "Accept-Encoding"),
    (27, "Accept-Language"),
    (28, "Authorization"),
    (29, "Content-Encoding"),
    (30, "Forwarded"),
    (31, "From"),
    (32, "If-Modified-Since"),
    (33, "Location"),
    (34, "Orig-Uri"),
    (35, "Referer"),
    (36, "Retry-After"),
    (37, "Server"),
    (38, "Title"),
    (39, "User-Agent"),
    (40, "WWW-Authenticate"),
    (41, "Proxy-Authenticate"),
    (42, "Accept-Ranges"),
    (43, "Set-Cookie"),
    (44, "Cookie"),
    (46, "Refresh"),
    (47, "Content-Disposition"),
    (48, "Age"),
    (49, "Cache-Control"),
    (50, "Content-Base"),
    (51, "Content-Location"),
    (52, "Content-MD5"),
    (53, "Content-Range"),
    (54, "ETag"),
    (55, "Host"),
    (56, "If-Match"),
    (57, "If-None-Match"),
    (58, "If-Range"),
    (59, "If-Unmodified-Since"),
    (60, "Max-Forwards"),
    (61, "Proxy-Authorization"),
    (62, "Range"),
    (63, "Transfer-Encoding"),
    (64, "Upgrade"),
    (65, "Vary"),
    (66, "Via"),
    (67, "Warning"),
    (68, "Expect"),
    (69, "Proxy-Connection"),
    (70, "Unless-Modified-Since"),
];

/// Convertit une date HTTP (RFC 1123) en `SYSTEMTIME` (8 × u16).
fn parse_http_date(s: &str) -> Option<[u16; 8]> {
    // "Sun, 06 Nov 1994 08:49:37 GMT"
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 5 {
        return None;
    }
    let day: u16 = parts[1].parse().ok()?;
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS
        .iter()
        .position(|m| m.eq_ignore_ascii_case(parts[2]))? as u16
        + 1;
    let year: u16 = parts[3].parse().ok()?;
    let mut hms = parts[4].split(':').map(|x| x.parse::<u16>().ok());
    let (h, m, sec) = (hms.next()??, hms.next()??, hms.next()??);
    // Jour de la semaine (Zeller / Sakamoto).
    let t = [0u16, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if month < 3 { year - 1 } else { year };
    let dow = (y + y / 4 - y / 100 + y / 400 + t[(month - 1) as usize] + day) % 7;
    Some([year, month, dow, day, h, m, sec, 0])
}

extern "win64" fn WinHttpQueryHeaders(
    req: usize,
    level: u32,
    name: *const u16,
    buf: *mut c_void,
    buflen: *mut u32,
    index: *mut u32,
) -> WinBool {
    ffi_guard!(CH, "WinHttpQueryHeaders", WIN_FALSE, {
        let o = match get_kind(req, HANDLE_TYPE_REQUEST) {
            Ok(o) => o,
            Err(e) => return fail(e),
        };
        if buflen.is_null() {
            return fail(ERROR_INVALID_PARAMETER);
        }
        let id = level & 0xFFFF;
        let mods = level & 0xFFFF_0000;
        let from_request = mods & QUERY_FLAG_REQUEST_HEADERS != 0;
        // SAFETY: `index` nul ou valide.
        let want_idx = if index.is_null() {
            0
        } else {
            unsafe { *index }
        } as usize;
        let g = o.lock();
        let Some(r) = g.req.as_ref() else {
            return fail(ERROR_WINHTTP_INCORRECT_HANDLE_TYPE);
        };
        let empty = Vec::new();
        let (hdrs, resp) = if from_request {
            (&r.headers, None)
        } else {
            match r.resp.as_ref() {
                Some(resp) => (&resp.headers, Some(resp)),
                None => return fail(ERROR_WINHTTP_INCORRECT_HANDLE_STATE),
            }
        };
        let hdrs = if hdrs.is_empty() { &empty } else { hdrs };
        let value: Option<String> = match (id, resp) {
            (QUERY_STATUS_CODE, Some(rs)) => (want_idx == 0).then(|| rs.status.to_string()),
            (QUERY_STATUS_TEXT, Some(rs)) => (want_idx == 0).then(|| rs.reason.clone()),
            (QUERY_VERSION, Some(rs)) => (want_idx == 0).then(|| rs.version.clone()),
            (QUERY_RAW_HEADERS | QUERY_RAW_HEADERS_CRLF, _) => {
                let sep = if id == QUERY_RAW_HEADERS {
                    "\0"
                } else {
                    "\r\n"
                };
                let mut s = match resp {
                    Some(rs) => format!("{} {} {}{sep}", rs.version, rs.status, rs.reason),
                    None => format!("{} {} {}{sep}", r.verb, r.path, r.version),
                };
                for (k, v) in hdrs {
                    s.push_str(&format!("{k}: {v}{sep}"));
                }
                s.push_str(sep);
                (want_idx == 0).then_some(s)
            }
            (QUERY_REQUEST_METHOD, _) => (want_idx == 0).then(|| r.verb.clone()),
            (QUERY_STATUS_CODE | QUERY_STATUS_TEXT | QUERY_VERSION, None) => None,
            _ => {
                let hname = if id == QUERY_CUSTOM {
                    // SAFETY: chaîne UTF-16 ou nul.
                    match unsafe { wstr_arg(name) } {
                        Some(n) => n,
                        None => return fail(ERROR_INVALID_PARAMETER),
                    }
                } else {
                    match HEADER_IDS.iter().find(|(i, _)| *i == id) {
                        Some((_, n)) => (*n).to_string(),
                        None => return fail(ERROR_WINHTTP_INVALID_QUERY_REQUEST),
                    }
                };
                hdrs.iter()
                    .filter(|(k, _)| k.eq_ignore_ascii_case(&hname))
                    .nth(want_idx)
                    .map(|(_, v)| v.clone())
            }
        };
        let Some(value) = value else {
            return fail(ERROR_WINHTTP_HEADER_NOT_FOUND);
        };
        // SAFETY: `buflen` non nul.
        let cap = unsafe { *buflen } as usize;
        if mods & (QUERY_FLAG_NUMBER | QUERY_FLAG_NUMBER64) != 0 {
            let n: u64 = match value.trim().parse() {
                Ok(n) => n,
                Err(_) => return fail(ERROR_WINHTTP_INVALID_QUERY_REQUEST),
            };
            let size = if mods & QUERY_FLAG_NUMBER64 != 0 {
                8
            } else {
                4
            };
            if buf.is_null() || cap < size {
                // SAFETY: `buflen` non nul.
                unsafe { *buflen = size as u32 };
                return fail(ERROR_INSUFFICIENT_BUFFER);
            }
            // SAFETY: capacité vérifiée.
            unsafe {
                if size == 8 {
                    ptr::write_unaligned(buf.cast::<u64>(), n);
                } else {
                    ptr::write_unaligned(buf.cast::<u32>(), n as u32);
                }
                *buflen = size as u32;
            }
        } else if mods & QUERY_FLAG_SYSTEMTIME != 0 {
            let Some(st) = parse_http_date(&value) else {
                return fail(ERROR_WINHTTP_INVALID_QUERY_REQUEST);
            };
            if buf.is_null() || cap < 16 {
                // SAFETY: `buflen` non nul.
                unsafe { *buflen = 16 };
                return fail(ERROR_INSUFFICIENT_BUFFER);
            }
            // SAFETY: 16 octets inscriptibles (vérifié).
            unsafe {
                ptr::copy_nonoverlapping(st.as_ptr(), buf.cast::<u16>(), 8);
                *buflen = 16;
            }
        } else {
            let w: Vec<u16> = value.encode_utf16().collect();
            let need = (w.len() + 1) * 2;
            if buf.is_null() || cap < need {
                // SAFETY: `buflen` non nul.
                unsafe { *buflen = need as u32 };
                return fail(ERROR_INSUFFICIENT_BUFFER);
            }
            // SAFETY: capacité vérifiée (octets).
            unsafe {
                let d = buf.cast::<u16>();
                ptr::copy_nonoverlapping(w.as_ptr(), d, w.len());
                *d.add(w.len()) = 0;
                *buflen = (w.len() * 2) as u32;
            }
        }
        if !index.is_null() {
            // SAFETY: non nul.
            unsafe { *index = (want_idx + 1) as u32 };
        }
        WIN_TRUE
    })
}

// ---------- Options & timeouts ----------

fn ms_to_timeout(ms: i32) -> Option<Duration> {
    if ms <= 0 {
        None
    } else {
        Some(Duration::from_millis(ms as u64))
    }
}

fn timeout_to_ms(t: Option<Duration>) -> u32 {
    t.map_or(u32::MAX, |d| {
        d.as_millis().min(u128::from(u32::MAX - 1)) as u32
    })
}

extern "win64" fn WinHttpSetTimeouts(
    h: usize,
    resolve: i32,
    connect: i32,
    send: i32,
    receive: i32,
) -> WinBool {
    ffi_guard!(CH, "WinHttpSetTimeouts", WIN_FALSE, {
        let o = match get(h) {
            Ok(o) => o,
            Err(e) => return fail(e),
        };
        if [resolve, connect, send, receive].iter().any(|v| *v < -1) {
            return fail(ERROR_INVALID_PARAMETER);
        }
        let mut g = o.lock();
        if g.kind == HANDLE_TYPE_CONNECT {
            return fail(ERROR_WINHTTP_INCORRECT_HANDLE_TYPE);
        }
        g.resolve_timeout = resolve as u32;
        g.timeouts = Timeouts {
            connect: ms_to_timeout(connect),
            send: ms_to_timeout(send),
            receive: ms_to_timeout(receive),
        };
        WIN_TRUE
    })
}

extern "win64" fn WinHttpSetOption(h: usize, option: u32, buf: *const c_void, len: u32) -> WinBool {
    ffi_guard!(CH, "WinHttpSetOption", WIN_FALSE, {
        let o = match get(h) {
            Ok(o) => o,
            Err(e) => return fail(e),
        };
        if buf.is_null() && len > 0 {
            return fail(ERROR_INVALID_PARAMETER);
        }
        let read_u32 = || -> Option<u32> {
            if buf.is_null() || len < 4 {
                None
            } else {
                // SAFETY: 4 octets lisibles (vérifié).
                Some(unsafe { ptr::read_unaligned(buf.cast::<u32>()) })
            }
        };
        let mut g = o.lock();
        match option {
            OPTION_CONTEXT_VALUE => {
                if buf.is_null() || (len as usize) < size_of::<usize>() {
                    return fail(ERROR_INSUFFICIENT_BUFFER);
                }
                // SAFETY: 8 octets lisibles (vérifié).
                g.context = unsafe { ptr::read_unaligned(buf.cast::<usize>()) };
            }
            OPTION_RESOLVE_TIMEOUT
            | OPTION_CONNECT_TIMEOUT
            | OPTION_SEND_TIMEOUT
            | OPTION_RECEIVE_TIMEOUT
            | OPTION_RECEIVE_RESPONSE_TIMEOUT => {
                let Some(v) = read_u32() else {
                    return fail(ERROR_INSUFFICIENT_BUFFER);
                };
                let t = ms_to_timeout(v as i32);
                match option {
                    OPTION_RESOLVE_TIMEOUT => g.resolve_timeout = v,
                    OPTION_CONNECT_TIMEOUT => g.timeouts.connect = t,
                    OPTION_SEND_TIMEOUT => g.timeouts.send = t,
                    _ => g.timeouts.receive = t,
                }
            }
            OPTION_USER_AGENT => {
                if g.kind == HANDLE_TYPE_CONNECT {
                    return fail(ERROR_WINHTTP_INCORRECT_HANDLE_TYPE);
                }
                // SAFETY: `len` caractères UTF-16 lisibles (contrat WinHTTP).
                let w = unsafe { wstr_len(buf.cast(), len) }.unwrap_or_default();
                g.agent = String::from_utf16_lossy(&w);
            }
            OPTION_HANDLE_TYPE | OPTION_PARENT_HANDLE | OPTION_URL => {
                return fail(ERROR_WINHTTP_OPTION_NOT_SETTABLE);
            }
            o if STORED_OPTIONS.contains(&o) => {
                let bytes = if buf.is_null() {
                    Vec::new()
                } else {
                    // SAFETY: `len` octets lisibles (contrat WinHTTP).
                    unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len as usize) }.to_vec()
                };
                g.options.insert(o, bytes);
            }
            _ => return fail(ERROR_WINHTTP_INVALID_OPTION),
        }
        WIN_TRUE
    })
}

fn request_url(g: &Obj) -> Option<String> {
    let r = g.req.as_ref()?;
    let scheme = if r.secure { "https" } else { "http" };
    let default = if r.secure { 443 } else { 80 };
    let port = if g.port == 0 { default } else { g.port };
    if port == default {
        Some(format!("{scheme}://{}{}", g.server, r.path))
    } else {
        Some(format!("{scheme}://{}:{port}{}", g.server, r.path))
    }
}

extern "win64" fn WinHttpQueryOption(
    h: usize,
    option: u32,
    buf: *mut c_void,
    len: *mut u32,
) -> WinBool {
    ffi_guard!(CH, "WinHttpQueryOption", WIN_FALSE, {
        let o = match get(h) {
            Ok(o) => o,
            Err(e) => return fail(e),
        };
        if len.is_null() {
            return fail(ERROR_INVALID_PARAMETER);
        }
        let g = o.lock();
        enum V {
            Bytes(Vec<u8>),
            Str(String),
        }
        let v = match option {
            OPTION_CONTEXT_VALUE => V::Bytes(g.context.to_le_bytes().to_vec()),
            OPTION_HANDLE_TYPE => V::Bytes(g.kind.to_le_bytes().to_vec()),
            OPTION_PARENT_HANDLE => V::Bytes(g.parent.to_le_bytes().to_vec()),
            OPTION_RESOLVE_TIMEOUT => V::Bytes(g.resolve_timeout.to_le_bytes().to_vec()),
            OPTION_CONNECT_TIMEOUT => {
                V::Bytes(timeout_to_ms(g.timeouts.connect).to_le_bytes().to_vec())
            }
            OPTION_SEND_TIMEOUT => V::Bytes(timeout_to_ms(g.timeouts.send).to_le_bytes().to_vec()),
            OPTION_RECEIVE_TIMEOUT | OPTION_RECEIVE_RESPONSE_TIMEOUT => {
                V::Bytes(timeout_to_ms(g.timeouts.receive).to_le_bytes().to_vec())
            }
            OPTION_URL => match request_url(&g) {
                Some(u) => V::Str(u),
                None => return fail(ERROR_WINHTTP_INCORRECT_HANDLE_TYPE),
            },
            OPTION_USER_AGENT => V::Str(g.agent.clone()),
            o if STORED_OPTIONS.contains(&o) => V::Bytes(
                g.options
                    .get(&o)
                    .cloned()
                    .unwrap_or_else(|| 0u32.to_le_bytes().to_vec()),
            ),
            _ => return fail(ERROR_WINHTTP_INVALID_OPTION),
        };
        // SAFETY: `len` non nul.
        let cap = unsafe { *len } as usize;
        match v {
            V::Bytes(b) => {
                if buf.is_null() || cap < b.len() {
                    // SAFETY: `len` non nul.
                    unsafe { *len = b.len() as u32 };
                    return fail(ERROR_INSUFFICIENT_BUFFER);
                }
                // SAFETY: capacité vérifiée.
                unsafe {
                    ptr::copy_nonoverlapping(b.as_ptr(), buf.cast::<u8>(), b.len());
                    *len = b.len() as u32;
                }
            }
            V::Str(s) => {
                let w: Vec<u16> = s.encode_utf16().collect();
                let need = (w.len() + 1) * 2;
                if buf.is_null() || cap < need {
                    // SAFETY: `len` non nul.
                    unsafe { *len = need as u32 };
                    return fail(ERROR_INSUFFICIENT_BUFFER);
                }
                // SAFETY: capacité vérifiée.
                unsafe {
                    let d = buf.cast::<u16>();
                    ptr::copy_nonoverlapping(w.as_ptr(), d, w.len());
                    *d.add(w.len()) = 0;
                    *len = (w.len() * 2) as u32;
                }
            }
        }
        WIN_TRUE
    })
}

extern "win64" fn WinHttpSetStatusCallback(
    h: usize,
    cb: usize,
    flags: u32,
    reserved: usize,
) -> usize {
    ffi_guard!(CH, "WinHttpSetStatusCallback", usize::MAX, {
        let o = match get(h) {
            Ok(o) => o,
            Err(e) => {
                set_error(e);
                return usize::MAX; // WINHTTP_INVALID_STATUS_CALLBACK
            }
        };
        let mut g = o.lock();
        let prev = g.callback;
        g.callback = cb;
        g.notify = flags;
        prev
    })
}

extern "win64" fn WinHttpCloseHandle(h: usize) -> WinBool {
    ffi_guard!(CH, "WinHttpCloseHandle", WIN_FALSE, {
        let Some(o) = HANDLES.lock().remove(&h) else {
            return fail(ERROR_INVALID_HANDLE);
        };
        let (cb, notify, ctx) = {
            let mut g = o.lock();
            if let Some(r) = g.req.as_mut() {
                r.stream = None;
            }
            (g.callback, g.notify, g.context)
        };
        if cb != 0 && notify & STATUS_HANDLE_CLOSING != 0 {
            let mut hv = h;
            invoke_callback(
                cb,
                h,
                ctx,
                STATUS_HANDLE_CLOSING,
                Info::GuestPtr((&raw mut hv) as usize, size_of::<usize>() as u32),
            );
        }
        WIN_TRUE
    })
}

// ---------- WinHttpCrackUrl ----------

const ICU_DECODE: u32 = 0x1000_0000;
const ICU_ESCAPE: u32 = 0x8000_0000;

fn percent_decode(s: &[u16]) -> Vec<u16> {
    let txt = String::from_utf16_lossy(s);
    let b = txt.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (
                (b[i + 1] as char).to_digit(16),
                (b[i + 2] as char).to_digit(16),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).encode_utf16().collect()
}

/// Remplit un composant (mode pointeur si `*p` nul et `*l` ≠ 0, mode copie si `*p` non nul).
/// Retourne `Err(code)` pour une erreur, `Ok(false)` si le tampon est trop petit.
///
/// # Safety
/// `*p` nul ou tampon de `*l` WCHAR inscriptibles.
unsafe fn fill_component(
    src: &[u16],
    range: Option<std::ops::Range<usize>>,
    p: &mut *mut u16,
    l: &mut u32,
    decode: bool,
) -> Result<bool, u32> {
    if p.is_null() {
        if *l == 0 {
            return Ok(true);
        }
        match range {
            Some(r) => {
                if decode && !r.is_empty() {
                    return Err(ERROR_INVALID_PARAMETER);
                }
                *p = src[r.start..].as_ptr().cast_mut();
                *l = r.len() as u32;
            }
            None => {
                *p = ptr::null_mut();
                *l = 0;
            }
        }
        return Ok(true);
    }
    let data: Vec<u16> = match range {
        Some(r) if decode => percent_decode(&src[r]),
        Some(r) => src[r].to_vec(),
        None => Vec::new(),
    };
    if (*l as usize) <= data.len() {
        *l = (data.len() + 1) as u32;
        return Ok(false);
    }
    // SAFETY: capacité `*l > data.len()` vérifiée.
    unsafe {
        ptr::copy_nonoverlapping(data.as_ptr(), *p, data.len());
        *(*p).add(data.len()) = 0;
    }
    *l = data.len() as u32;
    Ok(true)
}

extern "win64" fn WinHttpCrackUrl(
    url: *const u16,
    url_len: u32,
    flags: u32,
    uc: *mut UrlComponents,
) -> WinBool {
    ffi_guard!(CH, "WinHttpCrackUrl", WIN_FALSE, {
        if url.is_null() || uc.is_null() {
            return fail(ERROR_INVALID_PARAMETER);
        }
        // SAFETY: `uc` non nul (contrat WinHTTP).
        let c = unsafe { &mut *uc };
        if c.dw_struct_size as usize != size_of::<UrlComponents>() {
            return fail(ERROR_INVALID_PARAMETER);
        }
        // SAFETY: chaîne UTF-16 de `url_len` caractères (0 = terminée par un nul).
        let Some(src_vec) =
            (unsafe { wstr_len(url, if url_len == 0 { u32::MAX } else { url_len }) })
        else {
            return fail(ERROR_INVALID_PARAMETER);
        };
        // Les pointeurs renvoyés (mode pointeur) doivent viser la chaîne d'origine.
        // SAFETY: `src_vec.len()` unités lisibles depuis `url` (déjà lues ci-dessus).
        let src = unsafe { std::slice::from_raw_parts(url, src_vec.len()) };
        let p = match http::parse_url(src) {
            Ok(p) => p,
            Err(e) => return fail(e),
        };
        let decode = flags & (ICU_DECODE | ICU_ESCAPE) != 0;
        let mut ok = true;
        // SAFETY: tampons fournis par l'appelant selon le contrat URL_COMPONENTS.
        let steps: [Result<bool, u32>; 6] = unsafe {
            [
                fill_component(
                    src,
                    Some(p.scheme.clone()),
                    &mut c.lpsz_scheme,
                    &mut c.dw_scheme_length,
                    false,
                ),
                fill_component(
                    src,
                    Some(p.host.clone()),
                    &mut c.lpsz_host_name,
                    &mut c.dw_host_name_length,
                    false,
                ),
                fill_component(
                    src,
                    p.user.clone(),
                    &mut c.lpsz_user_name,
                    &mut c.dw_user_name_length,
                    decode,
                ),
                fill_component(
                    src,
                    p.password.clone(),
                    &mut c.lpsz_password,
                    &mut c.dw_password_length,
                    decode,
                ),
                fill_component(
                    src,
                    Some(p.path.clone()),
                    &mut c.lpsz_url_path,
                    &mut c.dw_url_path_length,
                    decode,
                ),
                fill_component(
                    src,
                    Some(p.extra.clone()),
                    &mut c.lpsz_extra_info,
                    &mut c.dw_extra_info_length,
                    decode,
                ),
            ]
        };
        for s in steps {
            match s {
                Ok(true) => {}
                Ok(false) => ok = false,
                Err(e) => return fail(e),
            }
        }
        c.n_scheme = p.scheme_kind as i32;
        c.n_port = p.port;
        if !ok {
            return fail(ERROR_INSUFFICIENT_BUFFER);
        }
        set_error(0);
        WIN_TRUE
    })
}

pub(crate) fn exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($($f:ident),* $(,)?) => { $( m.insert(stringify!($f), $f as *const () as usize); )* };
    }
    reg!(
        WinHttpOpen,
        WinHttpConnect,
        WinHttpOpenRequest,
        WinHttpAddRequestHeaders,
        WinHttpSendRequest,
        WinHttpWriteData,
        WinHttpReceiveResponse,
        WinHttpQueryHeaders,
        WinHttpQueryDataAvailable,
        WinHttpReadData,
        WinHttpSetOption,
        WinHttpSetTimeouts,
        WinHttpSetStatusCallback,
        WinHttpCloseHandle,
        WinHttpCrackUrl,
        WinHttpQueryOption,
    );
    m
}

#[cfg(test)]
mod tests {
    use super::super::{get_error, offline_mode};
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Instant;

    fn w(s: &str) -> Vec<u16> {
        sb_common::wide::to_wide_null(s)
    }

    fn empty_uc() -> UrlComponents {
        UrlComponents {
            dw_struct_size: size_of::<UrlComponents>() as u32,
            lpsz_scheme: ptr::null_mut(),
            dw_scheme_length: 0,
            n_scheme: 0,
            lpsz_host_name: ptr::null_mut(),
            dw_host_name_length: 0,
            n_port: 0,
            lpsz_user_name: ptr::null_mut(),
            dw_user_name_length: 0,
            lpsz_password: ptr::null_mut(),
            dw_password_length: 0,
            lpsz_url_path: ptr::null_mut(),
            dw_url_path_length: 0,
            lpsz_extra_info: ptr::null_mut(),
            dw_extra_info_length: 0,
        }
    }

    #[test]
    fn crack_url_pointer_and_copy_modes() {
        let url = w("http://user:pw@example.com:8080/path/x%20y?q=1");
        let mut uc = empty_uc();
        uc.dw_host_name_length = u32::MAX;
        uc.dw_url_path_length = u32::MAX;
        uc.dw_extra_info_length = u32::MAX;
        uc.dw_user_name_length = u32::MAX;
        assert_eq!(WinHttpCrackUrl(url.as_ptr(), 0, 0, &mut uc), WIN_TRUE);
        assert_eq!(uc.n_scheme, 1);
        assert_eq!(uc.n_port, 8080);
        // SAFETY: pointeurs dans `url`.
        let host = unsafe {
            std::slice::from_raw_parts(uc.lpsz_host_name, uc.dw_host_name_length as usize)
        };
        assert_eq!(String::from_utf16_lossy(host), "example.com");
        // SAFETY: idem.
        let user = unsafe {
            std::slice::from_raw_parts(uc.lpsz_user_name, uc.dw_user_name_length as usize)
        };
        assert_eq!(String::from_utf16_lossy(user), "user");
        assert_eq!(uc.dw_url_path_length, 11);
        assert_eq!(uc.dw_extra_info_length, 4);
        assert!(uc.lpsz_scheme.is_null()); // non demandé

        // Mode copie + décodage + tampon trop petit.
        let mut path = [0u16; 32];
        let mut host_small = [0u16; 4];
        let mut uc = empty_uc();
        uc.lpsz_url_path = path.as_mut_ptr();
        uc.dw_url_path_length = 32;
        uc.lpsz_host_name = host_small.as_mut_ptr();
        uc.dw_host_name_length = 4;
        assert_eq!(
            WinHttpCrackUrl(url.as_ptr(), 0, ICU_DECODE, &mut uc),
            WIN_FALSE
        );
        assert_eq!(get_error(), ERROR_INSUFFICIENT_BUFFER);
        assert_eq!(uc.dw_host_name_length, 12);
        assert_eq!(uc.dw_url_path_length, 9);
        assert_eq!(String::from_utf16_lossy(&path[..9]), "/path/x y");

        // https par défaut sur 443 ; erreurs.
        let mut uc = empty_uc();
        assert_eq!(
            WinHttpCrackUrl(w("https://h").as_ptr(), 0, 0, &mut uc),
            WIN_TRUE
        );
        assert_eq!((uc.n_scheme, uc.n_port), (2, 443));
        assert_eq!(
            WinHttpCrackUrl(w("gopher://h").as_ptr(), 0, 0, &mut uc),
            WIN_FALSE
        );
        assert_eq!(get_error(), http::ERROR_WINHTTP_UNRECOGNIZED_SCHEME);
        assert_eq!(
            WinHttpCrackUrl(w("http://h:abc/").as_ptr(), 0, 0, &mut uc),
            WIN_FALSE
        );
        assert_eq!(get_error(), http::ERROR_WINHTTP_INVALID_URL);
        uc.dw_struct_size = 10;
        assert_eq!(
            WinHttpCrackUrl(w("http://h").as_ptr(), 0, 0, &mut uc),
            WIN_FALSE
        );
        assert_eq!(get_error(), ERROR_INVALID_PARAMETER);
        assert_eq!(WinHttpCrackUrl(ptr::null(), 0, 0, &mut uc), WIN_FALSE);
    }

    #[test]
    fn handle_tree_and_options() {
        let s = WinHttpOpen(w("agent/1.0").as_ptr(), 0, ptr::null(), ptr::null(), 0);
        assert_ne!(s, 0);
        assert_eq!(WinHttpConnect(s, ptr::null(), 80, 0), 0);
        assert_eq!(get_error(), ERROR_INVALID_PARAMETER);
        let c = WinHttpConnect(s, w("localhost").as_ptr(), 8080, 0);
        assert_ne!(c, 0);
        assert_eq!(
            WinHttpOpenRequest(
                s,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                0
            ),
            0
        );
        assert_eq!(get_error(), ERROR_WINHTTP_INCORRECT_HANDLE_TYPE);
        let r = WinHttpOpenRequest(
            c,
            w("POST").as_ptr(),
            w("api/v1").as_ptr(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            0,
        );
        assert_ne!(r, 0);

        let mut ty = 0u32;
        let mut len = 4u32;
        assert_eq!(
            WinHttpQueryOption(r, OPTION_HANDLE_TYPE, (&raw mut ty).cast(), &mut len),
            WIN_TRUE
        );
        assert_eq!(ty, HANDLE_TYPE_REQUEST);
        let mut url = [0u16; 64];
        let mut ulen = 4u32;
        assert_eq!(
            WinHttpQueryOption(r, OPTION_URL, url.as_mut_ptr().cast(), &mut ulen),
            WIN_FALSE
        );
        assert_eq!(get_error(), ERROR_INSUFFICIENT_BUFFER);
        ulen = 128;
        assert_eq!(
            WinHttpQueryOption(r, OPTION_URL, url.as_mut_ptr().cast(), &mut ulen),
            WIN_TRUE
        );
        assert_eq!(
            String::from_utf16_lossy(&url[..(ulen / 2) as usize]),
            "http://localhost:8080/api/v1"
        );

        let ctx: usize = 0xABCD;
        assert_eq!(
            WinHttpSetOption(r, OPTION_CONTEXT_VALUE, (&raw const ctx).cast(), 8),
            WIN_TRUE
        );
        let mut got = 0usize;
        len = 8;
        assert_eq!(
            WinHttpQueryOption(r, OPTION_CONTEXT_VALUE, (&raw mut got).cast(), &mut len),
            WIN_TRUE
        );
        assert_eq!(got, 0xABCD);
        let v = 1u32;
        assert_eq!(
            WinHttpSetOption(r, 9999, (&raw const v).cast(), 4),
            WIN_FALSE
        );
        assert_eq!(get_error(), ERROR_WINHTTP_INVALID_OPTION);
        assert_eq!(
            WinHttpSetOption(r, OPTION_URL, (&raw const v).cast(), 4),
            WIN_FALSE
        );
        assert_eq!(get_error(), ERROR_WINHTTP_OPTION_NOT_SETTABLE);

        assert_eq!(WinHttpSetTimeouts(s, 0, 1000, 2000, 3000), WIN_TRUE);
        assert_eq!(WinHttpSetTimeouts(c, 0, 1000, 2000, 3000), WIN_FALSE);
        assert_eq!(WinHttpSetTimeouts(s, -2, 0, 0, 0), WIN_FALSE);
        let mut ct = 0u32;
        len = 4;
        assert_eq!(
            WinHttpQueryOption(s, OPTION_CONNECT_TIMEOUT, (&raw mut ct).cast(), &mut len),
            WIN_TRUE
        );
        assert_eq!(ct, 1000);

        // En-têtes de requête.
        let hdr = w("X-A: 1\r\nX-B: 2\r\n");
        assert_eq!(
            WinHttpAddRequestHeaders(r, hdr.as_ptr(), u32::MAX, ADDREQ_FLAG_ADD),
            WIN_TRUE
        );
        let dup = w("X-A: 3");
        assert_eq!(
            WinHttpAddRequestHeaders(r, dup.as_ptr(), u32::MAX, ADDREQ_FLAG_ADD_IF_NEW),
            WIN_FALSE
        );
        assert_eq!(get_error(), ERROR_WINHTTP_HEADER_ALREADY_EXISTS);
        assert_eq!(
            WinHttpAddRequestHeaders(r, dup.as_ptr(), u32::MAX, ADDREQ_FLAG_COALESCE_WITH_COMMA),
            WIN_TRUE
        );
        let mut buf = [0u16; 32];
        let mut blen = 64u32;
        assert_eq!(
            WinHttpQueryHeaders(
                r,
                QUERY_CUSTOM | QUERY_FLAG_REQUEST_HEADERS,
                w("x-a").as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut blen,
                ptr::null_mut()
            ),
            WIN_TRUE
        );
        assert_eq!(
            String::from_utf16_lossy(&buf[..(blen / 2) as usize]),
            "1, 3"
        );
        let bad = w("NoColonHere");
        assert_eq!(
            WinHttpAddRequestHeaders(r, bad.as_ptr(), u32::MAX, 0),
            WIN_FALSE
        );
        assert_eq!(get_error(), ERROR_WINHTTP_INVALID_HEADER);
        // Réponse pas encore reçue.
        blen = 64;
        assert_eq!(
            WinHttpQueryHeaders(
                r,
                QUERY_STATUS_CODE,
                ptr::null(),
                buf.as_mut_ptr().cast(),
                &mut blen,
                ptr::null_mut()
            ),
            WIN_FALSE
        );
        assert_eq!(get_error(), ERROR_WINHTTP_INCORRECT_HANDLE_STATE);

        for h in [r, c, s] {
            assert_eq!(WinHttpCloseHandle(h), WIN_TRUE);
        }
        assert_eq!(WinHttpCloseHandle(s), WIN_FALSE);
        assert_eq!(get_error(), ERROR_INVALID_HANDLE);
    }

    fn send_get(server: &str, secure: bool) -> (WinBool, u32) {
        let s = WinHttpOpen(ptr::null(), 1, ptr::null(), ptr::null(), 0);
        let c = WinHttpConnect(s, w(server).as_ptr(), 0, 0);
        let flags = if secure { WINHTTP_FLAG_SECURE } else { 0 };
        let r = WinHttpOpenRequest(
            c,
            ptr::null(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            flags,
        );
        let ok = WinHttpSendRequest(r, ptr::null(), 0, ptr::null(), 0, 0, 0);
        let e = get_error();
        for h in [r, c, s] {
            WinHttpCloseHandle(h);
        }
        (ok, e)
    }

    #[test]
    fn offline_send_fails_with_documented_codes() {
        if !offline_mode() {
            return;
        }
        assert_eq!(
            send_get("example.com", false),
            (WIN_FALSE, http::ERROR_WINHTTP_NAME_NOT_RESOLVED)
        );
        assert_eq!(
            send_get("8.8.8.8", false),
            (WIN_FALSE, http::ERROR_WINHTTP_CANNOT_CONNECT)
        );
        assert_eq!(
            send_get("localhost", true),
            (WIN_FALSE, http::ERROR_WINHTTP_SECURE_FAILURE)
        );
    }

    type Event = (usize, usize, u32, usize, u32);
    static EVENTS: Mutex<Vec<Event>> = Mutex::new(Vec::new());

    extern "win64" fn test_cb(h: usize, ctx: usize, status: u32, info: *mut c_void, len: u32) {
        let (res, err) = if status == STATUS_REQUEST_ERROR && !info.is_null() {
            // SAFETY: WINHTTP_ASYNC_RESULT fourni par l'implémentation.
            let ar = unsafe { &*(info as *const AsyncResult) };
            (ar.dw_result, ar.dw_error)
        } else {
            (0, 0)
        };
        EVENTS.lock().push((h, ctx, status, res, err));
    }

    #[test]
    fn async_offline_reports_request_error_via_callback() {
        if !offline_mode() {
            return;
        }
        let s = WinHttpOpen(ptr::null(), 1, ptr::null(), ptr::null(), WINHTTP_FLAG_ASYNC);
        assert_eq!(
            WinHttpSetStatusCallback(s, test_cb as *const () as usize, 0xFFFF_FFFF, 0),
            0
        );
        let c = WinHttpConnect(s, w("unreachable.example").as_ptr(), 0, 0);
        let r = WinHttpOpenRequest(
            c,
            ptr::null(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            0,
        );
        assert_eq!(
            WinHttpSendRequest(r, ptr::null(), 0, ptr::null(), 0, 0, 0x77),
            WIN_TRUE
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let ev = loop {
            if let Some(e) = EVENTS.lock().iter().find(|e| e.0 == r).copied() {
                break e;
            }
            assert!(Instant::now() < deadline, "callback non reçu");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(
            ev,
            (
                r,
                0x77,
                STATUS_REQUEST_ERROR,
                API_SEND_REQUEST,
                http::ERROR_WINHTTP_NAME_NOT_RESOLVED
            )
        );
        assert_eq!(WinHttpCloseHandle(r), WIN_TRUE);
        assert!(
            EVENTS
                .lock()
                .iter()
                .any(|e| e.0 == r && e.2 == STATUS_HANDLE_CLOSING)
        );
        WinHttpCloseHandle(c);
        WinHttpCloseHandle(s);
    }

    #[test]
    fn loopback_post_roundtrip() {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = l.local_addr().expect("addr").port();
        let th = std::thread::spawn(move || {
            let (mut sock, _) = l.accept().expect("accept");
            let mut req = Vec::new();
            let mut buf = [0u8; 1024];
            while !String::from_utf8_lossy(&req).contains("hello world") {
                let n = sock.read(&mut buf).expect("read");
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
            }
            sock.write_all(
                b"HTTP/1.1 201 Created\r\nContent-Type: text/plain\r\nContent-Length: 4\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\n\r\npong",
            )
            .expect("write");
            String::from_utf8_lossy(&req).into_owned()
        });
        let s = WinHttpOpen(w("sbtest").as_ptr(), 1, ptr::null(), ptr::null(), 0);
        let c = WinHttpConnect(s, w("127.0.0.1").as_ptr(), port, 0);
        let r = WinHttpOpenRequest(
            c,
            w("POST").as_ptr(),
            w("/echo").as_ptr(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            0,
        );
        let h = w("Content-Type: text/plain\r\n");
        assert_eq!(
            WinHttpAddRequestHeaders(r, h.as_ptr(), u32::MAX, ADDREQ_FLAG_ADD),
            WIN_TRUE
        );
        let part1 = b"hello ";
        assert_eq!(
            WinHttpSendRequest(r, ptr::null(), 0, part1.as_ptr(), 6, 11, 0),
            WIN_TRUE,
            "err {}",
            get_error()
        );
        // ReceiveResponse avant la fin du corps → état incorrect.
        assert_eq!(WinHttpReceiveResponse(r, ptr::null_mut()), WIN_FALSE);
        assert_eq!(get_error(), ERROR_WINHTTP_INCORRECT_HANDLE_STATE);
        let mut wr = 0u32;
        assert_eq!(WinHttpWriteData(r, b"world".as_ptr(), 5, &mut wr), WIN_TRUE);
        assert_eq!(wr, 5);
        assert_eq!(
            WinHttpReceiveResponse(r, ptr::null_mut()),
            WIN_TRUE,
            "err {}",
            get_error()
        );

        let mut code = 0u32;
        let mut len = 4u32;
        assert_eq!(
            WinHttpQueryHeaders(
                r,
                QUERY_STATUS_CODE | QUERY_FLAG_NUMBER,
                ptr::null(),
                (&raw mut code).cast(),
                &mut len,
                ptr::null_mut()
            ),
            WIN_TRUE
        );
        assert_eq!(code, 201);
        // Taille requise puis lecture.
        let mut need = 0u32;
        assert_eq!(
            WinHttpQueryHeaders(
                r,
                1,
                ptr::null(),
                ptr::null_mut(),
                &mut need,
                ptr::null_mut()
            ),
            WIN_FALSE
        );
        assert_eq!(get_error(), ERROR_INSUFFICIENT_BUFFER);
        assert_eq!(need, 22);
        let mut ct = [0u16; 11];
        assert_eq!(
            WinHttpQueryHeaders(
                r,
                1,
                ptr::null(),
                ct.as_mut_ptr().cast(),
                &mut need,
                ptr::null_mut()
            ),
            WIN_TRUE
        );
        assert_eq!(String::from_utf16_lossy(&ct[..10]), "text/plain");
        // Index multiple.
        let mut idx = 0u32;
        let mut ck = [0u16; 16];
        let mut cl = 32u32;
        assert_eq!(
            WinHttpQueryHeaders(
                r,
                43,
                ptr::null(),
                ck.as_mut_ptr().cast(),
                &mut cl,
                &mut idx
            ),
            WIN_TRUE
        );
        assert_eq!(idx, 1);
        cl = 32;
        assert_eq!(
            WinHttpQueryHeaders(
                r,
                43,
                ptr::null(),
                ck.as_mut_ptr().cast(),
                &mut cl,
                &mut idx
            ),
            WIN_TRUE
        );
        assert_eq!(String::from_utf16_lossy(&ck[..3]), "b=2");
        cl = 32;
        assert_eq!(
            WinHttpQueryHeaders(
                r,
                43,
                ptr::null(),
                ck.as_mut_ptr().cast(),
                &mut cl,
                &mut idx
            ),
            WIN_FALSE
        );
        assert_eq!(get_error(), ERROR_WINHTTP_HEADER_NOT_FOUND);

        let mut avail = 0u32;
        assert_eq!(WinHttpQueryDataAvailable(r, &mut avail), WIN_TRUE);
        assert_eq!(avail, 4);
        let mut body = [0u8; 8];
        let mut n = 0u32;
        assert_eq!(WinHttpReadData(r, body.as_mut_ptr(), 8, &mut n), WIN_TRUE);
        assert_eq!(&body[..n as usize], b"pong");
        assert_eq!(WinHttpReadData(r, body.as_mut_ptr(), 8, &mut n), WIN_TRUE);
        assert_eq!(n, 0);

        let raw = th.join().expect("join");
        assert!(raw.starts_with("POST /echo HTTP/1.1\r\n"));
        assert!(raw.contains("User-Agent: sbtest\r\n"));
        assert!(raw.contains("Content-Length: 11\r\n"));
        assert!(raw.ends_with("hello world"));
        for h in [r, c, s] {
            WinHttpCloseHandle(h);
        }
    }

    #[test]
    fn http_date_parsing() {
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some([1994, 11, 0, 6, 8, 49, 37, 0])
        );
        assert_eq!(parse_http_date("garbage"), None);
    }
}
