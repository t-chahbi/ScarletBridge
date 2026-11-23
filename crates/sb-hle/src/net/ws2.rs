//! `ws2_32.dll` (Winsock 2) : sockets BSD Windows mappés vers des fd hôtes Linux.
//!
//! - `SOCKET` (UINT_PTR) = handle opaque (multiple de 4) → fd hôte.
//! - Conversions de layout `sockaddr*`, `addrinfo`, `fd_set`, `timeval`, `hostent`… (Windows x64).
//! - Conversions d'options `SOL_SOCKET` / `IPPROTO_TCP` / `IPPROTO_IP` / `IPPROTO_IPV6`.
//! - Codes `WSAE*` via `WSAGetLastError` (= `GetLastError`, TEB).
//! - Offline strict : connexions / envois limités au loopback (`WSAENETUNREACH`), DNS non local
//!   refusé (`WSAHOST_NOT_FOUND`), `bind` limité au loopback (`INADDR_ANY` → loopback hôte).

#![allow(non_snake_case, unused_variables, clippy::too_many_arguments)]

use core::ffi::c_void;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::mem::{offset_of, size_of};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs};
use std::ptr;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use parking_lot::Mutex;
use sb_common::{ffi_guard, sb_debug, sb_trace};

use super::{
    VIRTUAL_HOSTNAME, cstr_arg, get_error, is_local_hostname, is_loopback_ip, offline_mode,
    set_error, wstr_arg,
};

const CH: &str = "ws2_32";

pub(crate) const INVALID_SOCKET: usize = usize::MAX;
const SOCKET_ERROR: i32 = -1;

// ---------- Codes d'erreur Winsock ----------
pub(crate) const ERROR_INVALID_PARAMETER: u32 = 87;
pub(crate) const WSAEINTR: u32 = 10004;
pub(crate) const WSAEACCES: u32 = 10013;
pub(crate) const WSAEFAULT: u32 = 10014;
pub(crate) const WSAEINVAL: u32 = 10022;
pub(crate) const WSAEMFILE: u32 = 10024;
pub(crate) const WSAEWOULDBLOCK: u32 = 10035;
pub(crate) const WSAEALREADY: u32 = 10037;
pub(crate) const WSAENOTSOCK: u32 = 10038;
pub(crate) const WSAEDESTADDRREQ: u32 = 10039;
pub(crate) const WSAEMSGSIZE: u32 = 10040;
pub(crate) const WSAEPROTOTYPE: u32 = 10041;
pub(crate) const WSAENOPROTOOPT: u32 = 10042;
pub(crate) const WSAEPROTONOSUPPORT: u32 = 10043;
pub(crate) const WSAESOCKTNOSUPPORT: u32 = 10044;
pub(crate) const WSAEOPNOTSUPP: u32 = 10045;
pub(crate) const WSAEPFNOSUPPORT: u32 = 10046;
pub(crate) const WSAEAFNOSUPPORT: u32 = 10047;
pub(crate) const WSAEADDRINUSE: u32 = 10048;
pub(crate) const WSAEADDRNOTAVAIL: u32 = 10049;
pub(crate) const WSAENETDOWN: u32 = 10050;
pub(crate) const WSAENETUNREACH: u32 = 10051;
pub(crate) const WSAENETRESET: u32 = 10052;
pub(crate) const WSAECONNABORTED: u32 = 10053;
pub(crate) const WSAECONNRESET: u32 = 10054;
pub(crate) const WSAENOBUFS: u32 = 10055;
pub(crate) const WSAEISCONN: u32 = 10056;
pub(crate) const WSAENOTCONN: u32 = 10057;
pub(crate) const WSAESHUTDOWN: u32 = 10058;
pub(crate) const WSAETIMEDOUT: u32 = 10060;
pub(crate) const WSAECONNREFUSED: u32 = 10061;
pub(crate) const WSAEHOSTUNREACH: u32 = 10065;
pub(crate) const WSAVERNOTSUPPORTED: u32 = 10092;
pub(crate) const WSANOTINITIALISED: u32 = 10093;
pub(crate) const WSATYPE_NOT_FOUND: u32 = 10109;
pub(crate) const WSAHOST_NOT_FOUND: u32 = 11001;
pub(crate) const WSANO_DATA: u32 = 11004;

// ---------- Constantes Windows ----------
const AF_UNSPEC_W: i32 = 0;
const AF_INET_W: i32 = 2;
const AF_INET6_W: i32 = 23;
const SOL_SOCKET_W: i32 = 0xFFFF;
const IPPROTO_IP_W: i32 = 0;
const IPPROTO_TCP_W: i32 = 6;
const IPPROTO_UDP_W: i32 = 17;
const IPPROTO_IPV6_W: i32 = 41;

const FIONBIO: u32 = 0x8004_667E;
const FIONREAD: u32 = 0x4004_667F;
const SIOCATMARK: u32 = 0x4004_7307;
const SIO_KEEPALIVE_VALS: u32 = 0x9800_0004;
const SIO_UDP_CONNRESET: u32 = 0x9800_000C;
const SIO_UDP_NETRESET: u32 = 0x9800_000F;

const MSG_OOB_W: i32 = 0x1;
const MSG_PEEK_W: i32 = 0x2;
const MSG_DONTROUTE_W: i32 = 0x4;
const MSG_WAITALL_W: i32 = 0x8;

const AI_PASSIVE: i32 = 0x1;
const AI_CANONNAME: i32 = 0x2;
const AI_NUMERICHOST: i32 = 0x4;
const AI_NUMERICSERV: i32 = 0x8;

const NI_NUMERICHOST: i32 = 0x02;
const NI_NAMEREQD: i32 = 0x04;
const NI_NUMERICSERV: i32 = 0x08;
const NI_DGRAM: i32 = 0x10;

// WSAPoll (valeurs Windows)
const POLLRDNORM_W: i16 = 0x0100;
const POLLRDBAND_W: i16 = 0x0200;
const POLLPRI_W: i16 = 0x0400;
const POLLWRNORM_W: i16 = 0x0010;
const POLLWRBAND_W: i16 = 0x0020;
const POLLERR_W: i16 = 0x0001;
const POLLHUP_W: i16 = 0x0002;
const POLLNVAL_W: i16 = 0x0004;

// ---------- Structures (layout Windows x64) ----------

#[repr(C)]
pub(crate) struct WsaData {
    pub w_version: u16,
    pub w_high_version: u16,
    pub i_max_sockets: u16,
    pub i_max_udp_dg: u16,
    pub lp_vendor_info: *mut u8,
    pub sz_description: [u8; 257],
    pub sz_system_status: [u8; 129],
}
const _: () = assert!(size_of::<WsaData>() == 408);
const _: () = assert!(offset_of!(WsaData, sz_description) == 16);

#[repr(C)]
pub(crate) struct FdSet {
    pub fd_count: u32,
    pub fd_array: [usize; 64],
}
const _: () = assert!(size_of::<FdSet>() == 520);
const _: () = assert!(offset_of!(FdSet, fd_array) == 8);

#[repr(C)]
pub(crate) struct TimeVal {
    pub tv_sec: i32,
    pub tv_usec: i32,
}
const _: () = assert!(size_of::<TimeVal>() == 8);

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct SockAddrIn {
    pub sin_family: u16,
    pub sin_port: u16,
    pub sin_addr: [u8; 4],
    pub sin_zero: [u8; 8],
}
const _: () = assert!(size_of::<SockAddrIn>() == 16);

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct SockAddrIn6 {
    pub sin6_family: u16,
    pub sin6_port: u16,
    pub sin6_flowinfo: u32,
    pub sin6_addr: [u8; 16],
    pub sin6_scope_id: u32,
}
const _: () = assert!(size_of::<SockAddrIn6>() == 28);
const _: () = assert!(offset_of!(SockAddrIn6, sin6_addr) == 8);

/// `ADDRINFOA` / `ADDRINFOW` (même layout ; `ai_canonname` = `char*` ou `wchar_t*`).
#[repr(C)]
pub(crate) struct AddrInfo {
    pub ai_flags: i32,
    pub ai_family: i32,
    pub ai_socktype: i32,
    pub ai_protocol: i32,
    pub ai_addrlen: usize,
    pub ai_canonname: *mut c_void,
    pub ai_addr: *mut u8,
    pub ai_next: *mut AddrInfo,
}
const _: () = assert!(size_of::<AddrInfo>() == 48);
const _: () = assert!(offset_of!(AddrInfo, ai_canonname) == 24);
const _: () = assert!(offset_of!(AddrInfo, ai_addr) == 32);
const _: () = assert!(offset_of!(AddrInfo, ai_next) == 40);

#[repr(C)]
pub(crate) struct HostEnt {
    pub h_name: *mut i8,
    pub h_aliases: *mut *mut i8,
    pub h_addrtype: i16,
    pub h_length: i16,
    pub h_addr_list: *mut *mut i8,
}
const _: () = assert!(size_of::<HostEnt>() == 32);
const _: () = assert!(offset_of!(HostEnt, h_addr_list) == 24);

#[repr(C)]
pub(crate) struct ProtoEnt {
    pub p_name: *mut i8,
    pub p_aliases: *mut *mut i8,
    pub p_proto: i16,
}
const _: () = assert!(size_of::<ProtoEnt>() == 24);

#[repr(C)]
pub(crate) struct ServEnt {
    pub s_name: *mut i8,
    pub s_aliases: *mut *mut i8,
    pub s_proto: *mut i8,
    pub s_port: i16,
}
const _: () = assert!(size_of::<ServEnt>() == 32);
const _: () = assert!(offset_of!(ServEnt, s_port) == 24);

#[repr(C)]
pub(crate) struct WsaBuf {
    pub len: u32,
    pub buf: *mut u8,
}
const _: () = assert!(size_of::<WsaBuf>() == 16);

#[repr(C)]
pub(crate) struct Overlapped {
    pub internal: usize,
    pub internal_high: usize,
    pub offset: u32,
    pub offset_high: u32,
    pub h_event: usize,
}
const _: () = assert!(size_of::<Overlapped>() == 32);

#[repr(C)]
pub(crate) struct WsaPollFd {
    pub fd: usize,
    pub events: i16,
    pub revents: i16,
}
const _: () = assert!(size_of::<WsaPollFd>() == 16);

#[repr(C)]
#[derive(Clone, Copy)]
struct LingerW {
    l_onoff: u16,
    l_linger: u16,
}
const _: () = assert!(size_of::<LingerW>() == 4);

// ---------- Table des sockets ----------

#[derive(Clone, Copy, Debug)]
struct Sock {
    fd: i32,
    af: i32,
    ty: i32,
    nonblocking: bool,
    connecting: bool,
}

static NEXT_SOCKET: AtomicUsize = AtomicUsize::new(0x1000);
static SOCKETS: LazyLock<Mutex<HashMap<usize, Sock>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Options acceptées sans équivalent Linux (`SO_EXCLUSIVEADDRUSE`, …) : valeur mémorisée.
type StoredOpts = HashMap<(usize, i32, i32), i32>;
static STORED_OPTS: LazyLock<Mutex<StoredOpts>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static STARTUP_COUNT: AtomicI32 = AtomicI32::new(0);

fn register_socket(sock: Sock) -> usize {
    let h = NEXT_SOCKET.fetch_add(4, Ordering::AcqRel);
    SOCKETS.lock().insert(h, sock);
    h
}

fn lookup(s: usize) -> Result<Sock, u32> {
    SOCKETS.lock().get(&s).copied().ok_or(WSAENOTSOCK)
}

fn update(s: usize, f: impl FnOnce(&mut Sock)) {
    if let Some(e) = SOCKETS.lock().get_mut(&s) {
        f(e);
    }
}

fn ensure_started() -> Result<(), u32> {
    if STARTUP_COUNT.load(Ordering::Acquire) > 0 {
        Ok(())
    } else {
        Err(WSANOTINITIALISED)
    }
}

fn fail(code: u32) -> i32 {
    set_error(code);
    SOCKET_ERROR
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

pub(crate) fn errno_to_wsa(e: i32) -> u32 {
    match e {
        0 => 0,
        libc::EINTR => WSAEINTR,
        libc::EBADF => WSAENOTSOCK,
        libc::EACCES | libc::EPERM => WSAEACCES,
        libc::EFAULT => WSAEFAULT,
        libc::EINVAL => WSAEINVAL,
        libc::EMFILE | libc::ENFILE => WSAEMFILE,
        libc::EAGAIN => WSAEWOULDBLOCK,
        libc::EINPROGRESS => WSAEWOULDBLOCK,
        libc::EALREADY => WSAEALREADY,
        libc::ENOTSOCK => WSAENOTSOCK,
        libc::EDESTADDRREQ => WSAEDESTADDRREQ,
        libc::EMSGSIZE => WSAEMSGSIZE,
        libc::EPROTOTYPE => WSAEPROTOTYPE,
        libc::ENOPROTOOPT => WSAENOPROTOOPT,
        libc::EPROTONOSUPPORT => WSAEPROTONOSUPPORT,
        libc::ESOCKTNOSUPPORT => WSAESOCKTNOSUPPORT,
        libc::EOPNOTSUPP => WSAEOPNOTSUPP,
        libc::EPFNOSUPPORT => WSAEPFNOSUPPORT,
        libc::EAFNOSUPPORT => WSAEAFNOSUPPORT,
        libc::EADDRINUSE => WSAEADDRINUSE,
        libc::EADDRNOTAVAIL => WSAEADDRNOTAVAIL,
        libc::ENETDOWN => WSAENETDOWN,
        libc::ENETUNREACH => WSAENETUNREACH,
        libc::ENETRESET => WSAENETRESET,
        libc::ECONNABORTED => WSAECONNABORTED,
        libc::ECONNRESET => WSAECONNRESET,
        libc::ENOBUFS | libc::ENOMEM => WSAENOBUFS,
        libc::EISCONN => WSAEISCONN,
        libc::ENOTCONN => WSAENOTCONN,
        libc::ESHUTDOWN | libc::EPIPE => WSAESHUTDOWN,
        libc::ETIMEDOUT => WSAETIMEDOUT,
        libc::ECONNREFUSED => WSAECONNREFUSED,
        libc::EHOSTUNREACH | libc::EHOSTDOWN => WSAEHOSTUNREACH,
        _ => WSAEINVAL,
    }
}

fn fail_errno() -> i32 {
    fail(errno_to_wsa(errno()))
}

// ---------- Conversions sockaddr ----------

/// Lit un `sockaddr` Windows.
///
/// # Safety
/// `p` doit être nul ou lisible sur `len` octets.
unsafe fn read_sockaddr(p: *const u8, len: i32) -> Result<SocketAddr, u32> {
    if p.is_null() || len < 2 {
        return Err(WSAEFAULT);
    }
    // SAFETY: au moins 2 octets lisibles (vérifié).
    let fam = i32::from(unsafe { ptr::read_unaligned(p.cast::<u16>()) });
    match fam {
        AF_INET_W => {
            if (len as usize) < size_of::<SockAddrIn>() {
                return Err(WSAEFAULT);
            }
            // SAFETY: 16 octets lisibles (vérifié).
            let sa = unsafe { ptr::read_unaligned(p.cast::<SockAddrIn>()) };
            Ok(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::from(sa.sin_addr),
                u16::from_be(sa.sin_port),
            )))
        }
        AF_INET6_W => {
            if len < 24 {
                return Err(WSAEFAULT);
            }
            let mut raw = [0u8; 28];
            let n = (len as usize).min(28);
            // SAFETY: `n` octets lisibles (vérifié), destination locale de 28 octets.
            unsafe { ptr::copy_nonoverlapping(p, raw.as_mut_ptr(), n) };
            // SAFETY: `raw` fait exactement `size_of::<SockAddrIn6>()` octets.
            let sa = unsafe { ptr::read_unaligned(raw.as_ptr().cast::<SockAddrIn6>()) };
            Ok(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(sa.sin6_addr),
                u16::from_be(sa.sin6_port),
                u32::from_be(sa.sin6_flowinfo),
                sa.sin6_scope_id,
            )))
        }
        _ => Err(WSAEAFNOSUPPORT),
    }
}

fn win_sockaddr_bytes(a: &SocketAddr) -> ([u8; 28], usize) {
    let mut out = [0u8; 28];
    match a {
        SocketAddr::V4(v4) => {
            let sa = SockAddrIn {
                sin_family: AF_INET_W as u16,
                sin_port: v4.port().to_be(),
                sin_addr: v4.ip().octets(),
                sin_zero: [0; 8],
            };
            // SAFETY: `out` (28 octets) ≥ 16 octets.
            unsafe { ptr::write_unaligned(out.as_mut_ptr().cast::<SockAddrIn>(), sa) };
            (out, 16)
        }
        SocketAddr::V6(v6) => {
            let sa = SockAddrIn6 {
                sin6_family: AF_INET6_W as u16,
                sin6_port: v6.port().to_be(),
                sin6_flowinfo: v6.flowinfo().to_be(),
                sin6_addr: v6.ip().octets(),
                sin6_scope_id: v6.scope_id(),
            };
            // SAFETY: `out` fait 28 octets.
            unsafe { ptr::write_unaligned(out.as_mut_ptr().cast::<SockAddrIn6>(), sa) };
            (out, 28)
        }
    }
}

/// Écrit un `sockaddr` Windows ; `*lenp` en entrée = capacité, en sortie = taille écrite.
///
/// # Safety
/// `p` / `lenp` nuls ou valides (capacité `*lenp`).
unsafe fn write_sockaddr(a: &SocketAddr, p: *mut u8, lenp: *mut i32) -> Result<(), u32> {
    if p.is_null() {
        return Ok(());
    }
    if lenp.is_null() {
        return Err(WSAEFAULT);
    }
    let (bytes, n) = win_sockaddr_bytes(a);
    // SAFETY: `lenp` non nul.
    let cap = unsafe { *lenp };
    if cap < n as i32 {
        return Err(WSAEFAULT);
    }
    // SAFETY: capacité vérifiée.
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), p, n);
        *lenp = n as i32;
    }
    Ok(())
}

fn to_libc(a: &SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: `sockaddr_storage` est POD, zéro valide.
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = match a {
        SocketAddr::V4(v4) => {
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: v4.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(v4.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: `sockaddr_storage` ≥ `sockaddr_in`.
            unsafe { ptr::write((&raw mut ss).cast::<libc::sockaddr_in>(), sin) };
            size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(v6) => {
            let sin6 = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: v6.port().to_be(),
                sin6_flowinfo: v6.flowinfo().to_be(),
                sin6_addr: libc::in6_addr {
                    s6_addr: v6.ip().octets(),
                },
                sin6_scope_id: v6.scope_id(),
            };
            // SAFETY: `sockaddr_storage` ≥ `sockaddr_in6`.
            unsafe { ptr::write((&raw mut ss).cast::<libc::sockaddr_in6>(), sin6) };
            size_of::<libc::sockaddr_in6>()
        }
    };
    (ss, len as libc::socklen_t)
}

fn from_libc(ss: &libc::sockaddr_storage) -> Option<SocketAddr> {
    match i32::from(ss.ss_family) {
        libc::AF_INET => {
            // SAFETY: famille AF_INET → contenu `sockaddr_in`.
            let sin =
                unsafe { &*(ss as *const libc::sockaddr_storage).cast::<libc::sockaddr_in>() };
            Some(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes()),
                u16::from_be(sin.sin_port),
            )))
        }
        libc::AF_INET6 => {
            // SAFETY: famille AF_INET6 → contenu `sockaddr_in6`.
            let sin6 =
                unsafe { &*(ss as *const libc::sockaddr_storage).cast::<libc::sockaddr_in6>() };
            Some(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(sin6.sin6_addr.s6_addr),
                u16::from_be(sin6.sin6_port),
                u32::from_be(sin6.sin6_flowinfo),
                sin6.sin6_scope_id,
            )))
        }
        _ => None,
    }
}

fn host_local_addr(fd: i32) -> Option<SocketAddr> {
    // SAFETY: zéro valide.
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: tampon local valide.
    let rc = unsafe { libc::getsockname(fd, (&raw mut ss).cast(), &mut len) };
    if rc != 0 { None } else { from_libc(&ss) }
}

/// Politique de destination sortante (connect / sendto).
fn check_outbound(a: &SocketAddr) -> Result<(), u32> {
    if a.ip().is_unspecified() {
        return Err(WSAEADDRNOTAVAIL);
    }
    if offline_mode() && !is_loopback_ip(&a.ip()) {
        sb_debug!(
            CH,
            "offline strict : destination {a} refusée (WSAENETUNREACH)"
        );
        return Err(WSAENETUNREACH);
    }
    Ok(())
}

/// Politique de `bind` (offline : loopback uniquement, `INADDR_ANY` remappé vers le loopback).
fn bind_policy(a: SocketAddr) -> Result<SocketAddr, u32> {
    if !offline_mode() {
        return Ok(a);
    }
    if a.ip().is_unspecified() {
        let ip = match a {
            SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        };
        return Ok(SocketAddr::new(ip, a.port()));
    }
    if is_loopback_ip(&a.ip()) {
        Ok(a)
    } else {
        Err(WSAEADDRNOTAVAIL)
    }
}

/// Offline : lie au loopback un socket UDP pas encore lié (évite l'autobind sur `INADDR_ANY`).
fn offline_autobind(sock: &Sock) {
    if !offline_mode() || sock.ty != libc::SOCK_DGRAM {
        return;
    }
    if host_local_addr(sock.fd).is_some_and(|a| a.port() != 0) {
        return;
    }
    let lo = if sock.af == AF_INET6_W {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0)
    } else {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
    };
    let (ss, len) = to_libc(&lo);
    // SAFETY: adresse locale valide.
    let _ = unsafe { libc::bind(sock.fd, (&raw const ss).cast(), len) };
}

// ---------- Startup ----------

extern "win64" fn WSAStartup(version: u16, data: *mut WsaData) -> i32 {
    ffi_guard!(CH, "WSAStartup", WSAEFAULT as i32, {
        let major = (version & 0xFF) as u8;
        let minor = (version >> 8) as u8;
        if major < 1 || (major == 1 && minor == 0) {
            return WSAVERNOTSUPPORTED as i32;
        }
        if data.is_null() {
            return WSAEFAULT as i32;
        }
        let negotiated = if major > 2 || (major == 2 && minor > 2) {
            0x0202
        } else {
            version
        };
        let mut d = WsaData {
            w_version: negotiated,
            w_high_version: 0x0202,
            i_max_sockets: 0,
            i_max_udp_dg: 0,
            lp_vendor_info: ptr::null_mut(),
            sz_description: [0; 257],
            sz_system_status: [0; 129],
        };
        let desc = b"WinSock 2.0";
        d.sz_description[..desc.len()].copy_from_slice(desc);
        let st = b"Running";
        d.sz_system_status[..st.len()].copy_from_slice(st);
        // SAFETY: `data` non nul, taille `WSADATA` x64 garantie par l'appelant.
        unsafe { ptr::write_unaligned(data, d) };
        STARTUP_COUNT.fetch_add(1, Ordering::AcqRel);
        sb_debug!(CH, "WSAStartup({version:#06x}) -> 0");
        0
    })
}

extern "win64" fn WSACleanup() -> i32 {
    ffi_guard!(CH, "WSACleanup", SOCKET_ERROR, {
        let mut cur = STARTUP_COUNT.load(Ordering::Acquire);
        loop {
            if cur <= 0 {
                return fail(WSANOTINITIALISED);
            }
            match STARTUP_COUNT.compare_exchange(cur, cur - 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return 0,
                Err(v) => cur = v,
            }
        }
    })
}

extern "win64" fn WSAGetLastError() -> i32 {
    get_error() as i32
}

extern "win64" fn WSASetLastError(code: i32) {
    set_error(code as u32);
}

// ---------- Création / fermeture ----------

fn create_socket(af: i32, ty: i32, proto: i32) -> usize {
    if let Err(e) = ensure_started() {
        set_error(e);
        return INVALID_SOCKET;
    }
    let host_af = match af {
        AF_INET_W => libc::AF_INET,
        AF_INET6_W => libc::AF_INET6,
        _ => {
            set_error(WSAEAFNOSUPPORT);
            return INVALID_SOCKET;
        }
    };
    let host_ty = match ty {
        1 => libc::SOCK_STREAM,
        2 => libc::SOCK_DGRAM,
        3 => {
            // Sockets bruts : privilèges administrateur sous Windows, refusés dans le bac à sable.
            set_error(WSAEACCES);
            return INVALID_SOCKET;
        }
        _ => {
            set_error(WSAESOCKTNOSUPPORT);
            return INVALID_SOCKET;
        }
    };
    let host_proto = match proto {
        0 | IPPROTO_TCP_W | IPPROTO_UDP_W => proto,
        _ => {
            set_error(WSAEPROTONOSUPPORT);
            return INVALID_SOCKET;
        }
    };
    if (host_ty == libc::SOCK_STREAM && host_proto == IPPROTO_UDP_W)
        || (host_ty == libc::SOCK_DGRAM && host_proto == IPPROTO_TCP_W)
    {
        set_error(WSAEPROTOTYPE);
        return INVALID_SOCKET;
    }
    // SAFETY: appel système sans pointeur.
    let fd = unsafe { libc::socket(host_af, host_ty | libc::SOCK_CLOEXEC, host_proto) };
    if fd < 0 {
        set_error(errno_to_wsa(errno()));
        return INVALID_SOCKET;
    }
    let h = register_socket(Sock {
        fd,
        af,
        ty: host_ty,
        nonblocking: false,
        connecting: false,
    });
    sb_trace!(
        CH,
        "socket(af={af}, type={ty}, proto={proto}) -> {h:#x} (fd {fd})"
    );
    h
}

extern "win64" fn socket(af: i32, ty: i32, proto: i32) -> usize {
    ffi_guard!(CH, "socket", INVALID_SOCKET, {
        create_socket(af, ty, proto)
    })
}

extern "win64" fn WSASocketW(
    af: i32,
    ty: i32,
    proto: i32,
    info: *mut c_void,
    g: u32,
    flags: u32,
) -> usize {
    ffi_guard!(CH, "WSASocketW", INVALID_SOCKET, {
        // `WSA_FLAG_OVERLAPPED` accepté : les opérations sont exécutées de façon synchrone.
        create_socket(af, ty, proto)
    })
}

extern "win64" fn WSASocketA(
    af: i32,
    ty: i32,
    proto: i32,
    info: *mut c_void,
    g: u32,
    flags: u32,
) -> usize {
    ffi_guard!(CH, "WSASocketA", INVALID_SOCKET, {
        create_socket(af, ty, proto)
    })
}

extern "win64" fn closesocket(s: usize) -> i32 {
    ffi_guard!(CH, "closesocket", SOCKET_ERROR, {
        let Some(sock) = SOCKETS.lock().remove(&s) else {
            return fail(WSAENOTSOCK);
        };
        STORED_OPTS.lock().retain(|k, _| k.0 != s);
        // SAFETY: fd possédé par l'entrée retirée ; `shutdown` réveille les appels bloqués
        // (accept/recv) comme `closesocket` sous Windows.
        unsafe {
            if sock.ty == libc::SOCK_STREAM {
                libc::shutdown(sock.fd, libc::SHUT_RDWR);
            }
            libc::close(sock.fd);
        }
        0
    })
}

// ---------- bind / listen / accept / connect ----------

extern "win64" fn bind(s: usize, name: *const u8, namelen: i32) -> i32 {
    ffi_guard!(CH, "bind", SOCKET_ERROR, {
        let sock = match lookup(s) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        // SAFETY: `name` lisible sur `namelen` octets (contrat Winsock), vérifié non nul.
        let addr = match unsafe { read_sockaddr(name, namelen) } {
            Ok(a) => a,
            Err(e) => return fail(e),
        };
        if (addr.is_ipv4() && sock.af != AF_INET_W) || (addr.is_ipv6() && sock.af != AF_INET6_W) {
            return fail(WSAEFAULT);
        }
        let addr = match bind_policy(addr) {
            Ok(a) => a,
            Err(e) => return fail(e),
        };
        let (ss, len) = to_libc(&addr);
        // SAFETY: adresse convertie localement.
        if unsafe { libc::bind(sock.fd, (&raw const ss).cast(), len) } != 0 {
            return fail_errno();
        }
        0
    })
}

extern "win64" fn listen(s: usize, backlog: i32) -> i32 {
    ffi_guard!(CH, "listen", SOCKET_ERROR, {
        let sock = match lookup(s) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        if sock.ty != libc::SOCK_STREAM {
            return fail(WSAEOPNOTSUPP);
        }
        // Windows : `listen` sur un socket non lié → WSAEINVAL (Linux lierait à INADDR_ANY).
        if host_local_addr(sock.fd).is_none_or(|a| a.port() == 0) {
            return fail(WSAEINVAL);
        }
        let bl = backlog.clamp(0, libc::SOMAXCONN);
        // SAFETY: appel système sans pointeur.
        if unsafe { libc::listen(sock.fd, bl) } != 0 {
            return fail_errno();
        }
        0
    })
}

extern "win64" fn accept(s: usize, addr: *mut u8, addrlen: *mut i32) -> usize {
    ffi_guard!(CH, "accept", INVALID_SOCKET, {
        let sock = match lookup(s) {
            Ok(v) => v,
            Err(e) => {
                set_error(e);
                return INVALID_SOCKET;
            }
        };
        if !addr.is_null() {
            // SAFETY: `addrlen` vérifié non nul avant lecture.
            if addrlen.is_null() || unsafe { *addrlen } < size_of::<SockAddrIn>() as i32 {
                set_error(WSAEFAULT);
                return INVALID_SOCKET;
            }
        }
        // SAFETY: zéro valide.
        let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let flags = libc::SOCK_CLOEXEC
            | if sock.nonblocking {
                libc::SOCK_NONBLOCK
            } else {
                0
            };
        let fd = loop {
            // SAFETY: tampon local valide.
            let fd = unsafe { libc::accept4(sock.fd, (&raw mut ss).cast(), &mut len, flags) };
            if fd < 0 && errno() == libc::EINTR {
                continue;
            }
            break fd;
        };
        if fd < 0 {
            set_error(errno_to_wsa(errno()));
            return INVALID_SOCKET;
        }
        if let Some(peer) = from_libc(&ss) {
            // SAFETY: capacité vérifiée par `write_sockaddr`.
            if let Err(e) = unsafe { write_sockaddr(&peer, addr, addrlen) } {
                // SAFETY: fd fraîchement accepté.
                unsafe { libc::close(fd) };
                set_error(e);
                return INVALID_SOCKET;
            }
        }
        register_socket(Sock {
            fd,
            af: sock.af,
            ty: sock.ty,
            nonblocking: sock.nonblocking,
            connecting: false,
        })
    })
}

extern "win64" fn connect(s: usize, name: *const u8, namelen: i32) -> i32 {
    ffi_guard!(CH, "connect", SOCKET_ERROR, {
        let sock = match lookup(s) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        // SAFETY: `name` lisible sur `namelen` octets (contrat Winsock).
        let addr = match unsafe { read_sockaddr(name, namelen) } {
            Ok(a) => a,
            Err(e) => return fail(e),
        };
        if let Err(e) = check_outbound(&addr) {
            return fail(e);
        }
        offline_autobind(&sock);
        let (ss, len) = to_libc(&addr);
        // SAFETY: adresse convertie localement.
        let rc = unsafe { libc::connect(sock.fd, (&raw const ss).cast(), len) };
        if rc != 0 {
            let e = errno();
            if e == libc::EINPROGRESS {
                update(s, |x| x.connecting = true);
                return fail(WSAEWOULDBLOCK);
            }
            if e == libc::EALREADY {
                return fail(WSAEALREADY);
            }
            return fail(errno_to_wsa(e));
        }
        update(s, |x| x.connecting = false);
        0
    })
}

fn name_query(s: usize, name: *mut u8, namelen: *mut i32, peer: bool) -> i32 {
    let sock = match lookup(s) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    if name.is_null() || namelen.is_null() {
        return fail(WSAEFAULT);
    }
    // SAFETY: zéro valide.
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: tampon local valide.
    let rc = unsafe {
        if peer {
            libc::getpeername(sock.fd, (&raw mut ss).cast(), &mut len)
        } else {
            libc::getsockname(sock.fd, (&raw mut ss).cast(), &mut len)
        }
    };
    if rc != 0 {
        return fail_errno();
    }
    let Some(a) = from_libc(&ss) else {
        return fail(WSAEINVAL);
    };
    if !peer && a.port() == 0 && a.ip().is_unspecified() {
        // Windows : getsockname sur un socket non lié → WSAEINVAL.
        return fail(WSAEINVAL);
    }
    // SAFETY: pointeurs vérifiés non nuls, capacité contrôlée.
    match unsafe { write_sockaddr(&a, name, namelen) } {
        Ok(()) => 0,
        Err(e) => fail(e),
    }
}

extern "win64" fn getsockname(s: usize, name: *mut u8, namelen: *mut i32) -> i32 {
    ffi_guard!(CH, "getsockname", SOCKET_ERROR, {
        name_query(s, name, namelen, false)
    })
}

extern "win64" fn getpeername(s: usize, name: *mut u8, namelen: *mut i32) -> i32 {
    ffi_guard!(CH, "getpeername", SOCKET_ERROR, {
        name_query(s, name, namelen, true)
    })
}

extern "win64" fn shutdown(s: usize, how: i32) -> i32 {
    ffi_guard!(CH, "shutdown", SOCKET_ERROR, {
        let sock = match lookup(s) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        let h = match how {
            0 => libc::SHUT_RD,
            1 => libc::SHUT_WR,
            2 => libc::SHUT_RDWR,
            _ => return fail(WSAEINVAL),
        };
        // SAFETY: appel système sans pointeur.
        if unsafe { libc::shutdown(sock.fd, h) } != 0 {
            return fail_errno();
        }
        0
    })
}

// ---------- send / recv ----------

fn map_flags(flags: i32) -> i32 {
    let mut f = 0;
    if flags & MSG_OOB_W != 0 {
        f |= libc::MSG_OOB;
    }
    if flags & MSG_PEEK_W != 0 {
        f |= libc::MSG_PEEK;
    }
    if flags & MSG_DONTROUTE_W != 0 {
        f |= libc::MSG_DONTROUTE;
    }
    if flags & MSG_WAITALL_W != 0 {
        f |= libc::MSG_WAITALL;
    }
    f
}

fn do_recv(s: usize, buf: *mut u8, len: i32, flags: i32, from: *mut u8, fromlen: *mut i32) -> i32 {
    let sock = match lookup(s) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    if len < 0 || (buf.is_null() && len > 0) {
        return fail(WSAEFAULT);
    }
    if !from.is_null() && fromlen.is_null() {
        return fail(WSAEFAULT);
    }
    let mut f = map_flags(flags);
    let dgram = sock.ty == libc::SOCK_DGRAM;
    if dgram {
        f |= libc::MSG_TRUNC;
    }
    // SAFETY: zéro valide.
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut sl = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let n = loop {
        // SAFETY: `buf` valide sur `len` octets (contrat Winsock), tampon d'adresse local.
        let n = unsafe {
            libc::recvfrom(
                sock.fd,
                buf.cast(),
                len as usize,
                f,
                (&raw mut ss).cast(),
                &mut sl,
            )
        };
        if n < 0 && errno() == libc::EINTR {
            continue;
        }
        break n;
    };
    if n < 0 {
        return fail_errno();
    }
    if !from.is_null()
        && let Some(a) = from_libc(&ss)
    {
        // SAFETY: capacité contrôlée par `write_sockaddr`.
        if let Err(e) = unsafe { write_sockaddr(&a, from, fromlen) } {
            return fail(e);
        }
    }
    if dgram && n as usize > len as usize {
        // Datagramme tronqué : Windows remplit le tampon puis renvoie WSAEMSGSIZE.
        return fail(WSAEMSGSIZE);
    }
    n as i32
}

extern "win64" fn recv(s: usize, buf: *mut u8, len: i32, flags: i32) -> i32 {
    ffi_guard!(CH, "recv", SOCKET_ERROR, {
        do_recv(s, buf, len, flags, ptr::null_mut(), ptr::null_mut())
    })
}

extern "win64" fn recvfrom(
    s: usize,
    buf: *mut u8,
    len: i32,
    flags: i32,
    from: *mut u8,
    fromlen: *mut i32,
) -> i32 {
    ffi_guard!(CH, "recvfrom", SOCKET_ERROR, {
        do_recv(s, buf, len, flags, from, fromlen)
    })
}

fn do_send(s: usize, buf: *const u8, len: i32, flags: i32, to: Option<SocketAddr>) -> i32 {
    let sock = match lookup(s) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    if len < 0 || (buf.is_null() && len > 0) {
        return fail(WSAEFAULT);
    }
    let f = map_flags(flags & (MSG_OOB_W | MSG_DONTROUTE_W)) | libc::MSG_NOSIGNAL;
    let n = loop {
        // SAFETY: `buf` lisible sur `len` octets (contrat Winsock).
        let n = unsafe {
            match &to {
                Some(a) => {
                    let (ss, sl) = to_libc(a);
                    libc::sendto(
                        sock.fd,
                        buf.cast(),
                        len as usize,
                        f,
                        (&raw const ss).cast(),
                        sl,
                    )
                }
                None => libc::send(sock.fd, buf.cast(), len as usize, f),
            }
        };
        if n < 0 && errno() == libc::EINTR {
            continue;
        }
        break n;
    };
    if n < 0 {
        return fail_errno();
    }
    n as i32
}

extern "win64" fn send(s: usize, buf: *const u8, len: i32, flags: i32) -> i32 {
    ffi_guard!(CH, "send", SOCKET_ERROR, {
        do_send(s, buf, len, flags, None)
    })
}

extern "win64" fn sendto(
    s: usize,
    buf: *const u8,
    len: i32,
    flags: i32,
    to: *const u8,
    tolen: i32,
) -> i32 {
    ffi_guard!(CH, "sendto", SOCKET_ERROR, {
        let sock = match lookup(s) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        if to.is_null() || sock.ty == libc::SOCK_STREAM {
            // Socket connecté : l'adresse est ignorée (comme Windows pour TCP).
            return do_send(s, buf, len, flags, None);
        }
        // SAFETY: `to` lisible sur `tolen` octets (contrat Winsock).
        let addr = match unsafe { read_sockaddr(to, tolen) } {
            Ok(a) => a,
            Err(e) => return fail(e),
        };
        if let Err(e) = check_outbound(&addr) {
            return fail(e);
        }
        offline_autobind(&sock);
        do_send(s, buf, len, flags, Some(addr))
    })
}

/// Complète un `WSAOVERLAPPED` d'une opération exécutée de façon synchrone.
///
/// # Safety
/// `ov` nul ou valide en écriture.
unsafe fn complete_overlapped(ov: *mut Overlapped, status: usize, bytes: usize) {
    if ov.is_null() {
        return;
    }
    // SAFETY: `ov` non nul (contrat appelant).
    let ev = unsafe {
        (*ov).internal = status;
        (*ov).internal_high = bytes;
        (*ov).h_event
    };
    if ev != 0 {
        let _ = crate::sync::SetEvent(ev);
    }
}

fn collect_iov(bufs: *const WsaBuf, count: u32) -> Result<Vec<libc::iovec>, u32> {
    if bufs.is_null() && count > 0 {
        return Err(WSAEFAULT);
    }
    let mut v = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        // SAFETY: tableau de `count` WSABUF (contrat Winsock).
        let b = unsafe { &*bufs.add(i) };
        if b.buf.is_null() && b.len > 0 {
            return Err(WSAEFAULT);
        }
        v.push(libc::iovec {
            iov_base: b.buf.cast(),
            iov_len: b.len as usize,
        });
    }
    Ok(v)
}

extern "win64" fn WSASend(
    s: usize,
    bufs: *const WsaBuf,
    count: u32,
    sent: *mut u32,
    flags: u32,
    ov: *mut Overlapped,
    completion: usize,
) -> i32 {
    ffi_guard!(CH, "WSASend", SOCKET_ERROR, {
        let sock = match lookup(s) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        if completion != 0 {
            // Routines de complétion APC non prises en charge (pas d'alertable wait).
            return fail(WSAEOPNOTSUPP);
        }
        let mut iov = match collect_iov(bufs, count) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        // SAFETY: zéro valide pour `msghdr`.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = iov.as_mut_ptr();
        msg.msg_iovlen = iov.len();
        let f = map_flags(flags as i32 & (MSG_OOB_W | MSG_DONTROUTE_W)) | libc::MSG_NOSIGNAL;
        let n = loop {
            // SAFETY: iovecs construits depuis les WSABUF de l'invité.
            let n = unsafe { libc::sendmsg(sock.fd, &msg, f) };
            if n < 0 && errno() == libc::EINTR {
                continue;
            }
            break n;
        };
        if n < 0 {
            return fail_errno();
        }
        if !sent.is_null() {
            // SAFETY: non nul.
            unsafe { *sent = n as u32 };
        }
        // SAFETY: `ov` nul ou valide (contrat Winsock).
        unsafe { complete_overlapped(ov, 0, n as usize) };
        0
    })
}

extern "win64" fn WSARecv(
    s: usize,
    bufs: *const WsaBuf,
    count: u32,
    recvd: *mut u32,
    flags: *mut u32,
    ov: *mut Overlapped,
    completion: usize,
) -> i32 {
    ffi_guard!(CH, "WSARecv", SOCKET_ERROR, {
        let sock = match lookup(s) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        if completion != 0 {
            return fail(WSAEOPNOTSUPP);
        }
        if flags.is_null() {
            return fail(WSAEFAULT);
        }
        let mut iov = match collect_iov(bufs, count) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        // SAFETY: zéro valide pour `msghdr`.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = iov.as_mut_ptr();
        msg.msg_iovlen = iov.len();
        // SAFETY: `flags` non nul.
        let f = map_flags(unsafe { *flags } as i32);
        let n = loop {
            // SAFETY: iovecs construits depuis les WSABUF de l'invité.
            let n = unsafe { libc::recvmsg(sock.fd, &mut msg, f) };
            if n < 0 && errno() == libc::EINTR {
                continue;
            }
            break n;
        };
        if n < 0 {
            return fail_errno();
        }
        let truncated = msg.msg_flags & libc::MSG_TRUNC != 0;
        // SAFETY: pointeurs vérifiés non nuls.
        unsafe {
            *flags = if truncated { 0x8000 } else { 0 }; // MSG_PARTIAL
            if !recvd.is_null() {
                *recvd = n as u32;
            }
            complete_overlapped(ov, 0, n as usize);
        }
        if truncated && sock.ty == libc::SOCK_DGRAM {
            return fail(WSAEMSGSIZE);
        }
        0
    })
}

// ---------- ioctl ----------

fn set_nonblocking(s: usize, fd: i32, on: bool) -> Result<(), u32> {
    // SAFETY: fcntl sur fd possédé.
    let fl = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if fl < 0 {
        return Err(errno_to_wsa(errno()));
    }
    let nfl = if on {
        fl | libc::O_NONBLOCK
    } else {
        fl & !libc::O_NONBLOCK
    };
    // SAFETY: fcntl sur fd possédé.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, nfl) } < 0 {
        return Err(errno_to_wsa(errno()));
    }
    update(s, |x| x.nonblocking = on);
    Ok(())
}

fn ioctl_common(s: usize, cmd: u32, arg_in: Option<u32>) -> Result<u32, u32> {
    let sock = lookup(s)?;
    match cmd {
        FIONBIO => {
            set_nonblocking(s, sock.fd, arg_in.ok_or(WSAEFAULT)? != 0)?;
            Ok(0)
        }
        FIONREAD => {
            let mut n: libc::c_int = 0;
            // SAFETY: FIONREAD écrit un int dans `n`.
            if unsafe { libc::ioctl(sock.fd, libc::FIONREAD, &mut n) } < 0 {
                return Err(errno_to_wsa(errno()));
            }
            Ok(n.max(0) as u32)
        }
        SIOCATMARK => {
            let mut n: libc::c_int = 0;
            // SAFETY: SIOCATMARK écrit un int dans `n`.
            if unsafe {
                libc::ioctl(sock.fd, 0x8905 /* SIOCATMARK Linux */, &mut n)
            } < 0
            {
                // Pas de données OOB en attente (sémantique Windows : TRUE).
                return Ok(1);
            }
            Ok(u32::from(n != 0 || sock.ty != libc::SOCK_STREAM))
        }
        _ => Err(WSAEINVAL),
    }
}

extern "win64" fn ioctlsocket(s: usize, cmd: i32, argp: *mut u32) -> i32 {
    ffi_guard!(CH, "ioctlsocket", SOCKET_ERROR, {
        if argp.is_null() {
            return fail(WSAEFAULT);
        }
        let cmd = cmd as u32;
        // SAFETY: `argp` non nul (u_long*).
        let input = unsafe { *argp };
        match ioctl_common(s, cmd, Some(input)) {
            Ok(v) => {
                if cmd != FIONBIO {
                    // SAFETY: `argp` non nul.
                    unsafe { *argp = v };
                }
                0
            }
            Err(e) => fail(e),
        }
    })
}

extern "win64" fn WSAIoctl(
    s: usize,
    code: u32,
    inbuf: *const u8,
    inlen: u32,
    outbuf: *mut u8,
    outlen: u32,
    returned: *mut u32,
    ov: *mut Overlapped,
    completion: usize,
) -> i32 {
    ffi_guard!(CH, "WSAIoctl", SOCKET_ERROR, {
        let sock = match lookup(s) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        if completion != 0 {
            return fail(WSAEOPNOTSUPP);
        }
        let read_u32 = |p: *const u8, len: u32| -> Option<u32> {
            if p.is_null() || len < 4 {
                None
            } else {
                // SAFETY: au moins 4 octets lisibles (vérifié).
                Some(unsafe { ptr::read_unaligned(p.cast::<u32>()) })
            }
        };
        let set_ret = |n: u32| {
            if !returned.is_null() {
                // SAFETY: non nul.
                unsafe { *returned = n };
            }
        };
        match code {
            FIONBIO => {
                let Some(v) = read_u32(inbuf, inlen) else {
                    return fail(WSAEFAULT);
                };
                if let Err(e) = ioctl_common(s, FIONBIO, Some(v)) {
                    return fail(e);
                }
                set_ret(0);
            }
            FIONREAD | SIOCATMARK => {
                if outbuf.is_null() || outlen < 4 {
                    return fail(WSAEFAULT);
                }
                match ioctl_common(s, code, None) {
                    Ok(v) => {
                        // SAFETY: 4 octets inscriptibles (vérifié).
                        unsafe { ptr::write_unaligned(outbuf.cast::<u32>(), v) };
                        set_ret(4);
                    }
                    Err(e) => return fail(e),
                }
            }
            SIO_KEEPALIVE_VALS => {
                if inbuf.is_null() || inlen < 12 {
                    return fail(WSAEFAULT);
                }
                // SAFETY: 12 octets lisibles (`struct tcp_keepalive`).
                let (on, time_ms, intv_ms) = unsafe {
                    (
                        ptr::read_unaligned(inbuf.cast::<u32>()),
                        ptr::read_unaligned(inbuf.add(4).cast::<u32>()),
                        ptr::read_unaligned(inbuf.add(8).cast::<u32>()),
                    )
                };
                let set = |lvl: i32, opt: i32, v: i32| {
                    // SAFETY: option entière.
                    unsafe {
                        libc::setsockopt(
                            sock.fd,
                            lvl,
                            opt,
                            (&raw const v).cast(),
                            size_of::<i32>() as libc::socklen_t,
                        )
                    }
                };
                set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, i32::from(on != 0));
                if on != 0 {
                    set(
                        libc::IPPROTO_TCP,
                        libc::TCP_KEEPIDLE,
                        (time_ms / 1000).max(1) as i32,
                    );
                    set(
                        libc::IPPROTO_TCP,
                        libc::TCP_KEEPINTVL,
                        (intv_ms / 1000).max(1) as i32,
                    );
                }
                set_ret(0);
            }
            SIO_UDP_CONNRESET | SIO_UDP_NETRESET => {
                // Linux ne remonte pas les ICMP port-unreachable sur UDP non connecté : rien à faire.
                set_ret(0);
            }
            _ => {
                sb_debug!(
                    CH,
                    "WSAIoctl({code:#x}) non pris en charge -> WSAEOPNOTSUPP"
                );
                return fail(WSAEOPNOTSUPP);
            }
        }
        // SAFETY: `ov` nul ou valide (contrat Winsock).
        unsafe { complete_overlapped(ov, 0, 0) };
        0
    })
}

// ---------- Options ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OptKind {
    Int,
    Bool,
    TimeoutMs,
    Linger,
    DontLinger,
    Error,
    MaxMsgSize,
    Raw,
    Stored,
}

fn map_opt(level: i32, name: i32) -> Result<(i32, i32, OptKind), u32> {
    use OptKind::*;
    let r = match level {
        SOL_SOCKET_W => {
            let s = libc::SOL_SOCKET;
            match name {
                0x0001 => (s, libc::SO_DEBUG, Bool),
                0x0002 => (s, libc::SO_ACCEPTCONN, Bool),
                0x0004 => (s, libc::SO_REUSEADDR, Bool),
                0x0008 => (s, libc::SO_KEEPALIVE, Bool),
                0x0010 => (s, libc::SO_DONTROUTE, Bool),
                0x0020 => (s, libc::SO_BROADCAST, Bool),
                0x0080 => (s, libc::SO_LINGER, Linger),
                0x0100 => (s, libc::SO_OOBINLINE, Bool),
                -129 => (s, libc::SO_LINGER, DontLinger), // SO_DONTLINGER = ~SO_LINGER
                -5 => (level, name, Stored),              // SO_EXCLUSIVEADDRUSE
                0x1001 => (s, libc::SO_SNDBUF, Int),
                0x1002 => (s, libc::SO_RCVBUF, Int),
                0x1003 => (s, libc::SO_SNDLOWAT, Int),
                0x1004 => (s, libc::SO_RCVLOWAT, Int),
                0x1005 => (s, libc::SO_SNDTIMEO, TimeoutMs),
                0x1006 => (s, libc::SO_RCVTIMEO, TimeoutMs),
                0x1007 => (s, libc::SO_ERROR, Error),
                0x1008 => (s, libc::SO_TYPE, Int),
                0x2003 => (level, name, MaxMsgSize),
                0x3002 | 0x7010 | 0x7016 => (level, name, Stored), // CONDITIONAL_ACCEPT, UPDATE_*_CONTEXT
                _ => return Err(WSAENOPROTOOPT),
            }
        }
        IPPROTO_TCP_W => {
            let t = libc::IPPROTO_TCP;
            match name {
                1 => (t, libc::TCP_NODELAY, Bool),
                3 => (t, libc::TCP_KEEPIDLE, Int),
                16 => (t, libc::TCP_KEEPCNT, Int),
                17 => (t, libc::TCP_KEEPINTVL, Int),
                _ => return Err(WSAENOPROTOOPT),
            }
        }
        IPPROTO_IP_W => {
            let i = libc::IPPROTO_IP;
            match name {
                2 => (i, libc::IP_HDRINCL, Bool),
                3 => (i, libc::IP_TOS, Int),
                4 => (i, libc::IP_TTL, Int),
                9 => (i, libc::IP_MULTICAST_IF, Raw),
                10 => (i, libc::IP_MULTICAST_TTL, Int),
                11 => (i, libc::IP_MULTICAST_LOOP, Int),
                12 => (i, libc::IP_ADD_MEMBERSHIP, Raw),
                13 => (i, libc::IP_DROP_MEMBERSHIP, Raw),
                14 => (level, name, Stored), // IP_DONTFRAGMENT
                _ => return Err(WSAENOPROTOOPT),
            }
        }
        IPPROTO_IPV6_W => {
            let i = libc::IPPROTO_IPV6;
            match name {
                4 => (i, libc::IPV6_UNICAST_HOPS, Int),
                9 => (i, libc::IPV6_MULTICAST_IF, Int),
                10 => (i, libc::IPV6_MULTICAST_HOPS, Int),
                11 => (i, libc::IPV6_MULTICAST_LOOP, Int),
                12 => (i, libc::IPV6_ADD_MEMBERSHIP, Raw),
                13 => (i, libc::IPV6_DROP_MEMBERSHIP, Raw),
                27 => (i, libc::IPV6_V6ONLY, Bool),
                _ => return Err(WSAENOPROTOOPT),
            }
        }
        IPPROTO_UDP_W => (level, name, Stored), // UDP_NOCHECKSUM & co
        _ => return Err(WSAEINVAL),
    };
    Ok(r)
}

fn host_setopt<T>(fd: i32, lvl: i32, opt: i32, v: &T) -> Result<(), u32> {
    // SAFETY: `v` référence valide de taille `size_of::<T>()`.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            lvl,
            opt,
            (v as *const T).cast(),
            size_of::<T>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        Err(errno_to_wsa(errno()))
    } else {
        Ok(())
    }
}

fn host_getopt<T: Copy>(fd: i32, lvl: i32, opt: i32, init: T) -> Result<T, u32> {
    let mut v = init;
    let mut len = size_of::<T>() as libc::socklen_t;
    // SAFETY: `v` local de taille `len`.
    let rc = unsafe { libc::getsockopt(fd, lvl, opt, (&raw mut v).cast(), &mut len) };
    if rc != 0 {
        Err(errno_to_wsa(errno()))
    } else {
        Ok(v)
    }
}

extern "win64" fn setsockopt(
    s: usize,
    level: i32,
    optname: i32,
    optval: *const u8,
    optlen: i32,
) -> i32 {
    ffi_guard!(CH, "setsockopt", SOCKET_ERROR, {
        let sock = match lookup(s) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        let (lvl, opt, kind) = match map_opt(level, optname) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        if optval.is_null() || optlen <= 0 {
            return fail(WSAEFAULT);
        }
        // Valeur entière : DWORD/BOOL (4 octets) ou `char` (1 octet) toléré comme Windows.
        let int_val = || -> i32 {
            // SAFETY: `optlen` octets lisibles (contrat Winsock).
            unsafe {
                if optlen >= 4 {
                    ptr::read_unaligned(optval.cast::<i32>())
                } else {
                    i32::from(*optval)
                }
            }
        };
        let r = match kind {
            OptKind::Int => host_setopt(sock.fd, lvl, opt, &int_val()),
            OptKind::Bool => host_setopt(sock.fd, lvl, opt, &i32::from(int_val() != 0)),
            OptKind::TimeoutMs => {
                let ms = int_val() as u32;
                let tv = libc::timeval {
                    tv_sec: (ms / 1000) as libc::time_t,
                    tv_usec: ((ms % 1000) * 1000) as libc::suseconds_t,
                };
                host_setopt(sock.fd, lvl, opt, &tv)
            }
            OptKind::Linger => {
                if (optlen as usize) < size_of::<LingerW>() {
                    return fail(WSAEFAULT);
                }
                // SAFETY: 4 octets lisibles (vérifié).
                let l = unsafe { ptr::read_unaligned(optval.cast::<LingerW>()) };
                let hl = libc::linger {
                    l_onoff: i32::from(l.l_onoff != 0),
                    l_linger: i32::from(l.l_linger),
                };
                host_setopt(sock.fd, lvl, opt, &hl)
            }
            OptKind::DontLinger => {
                let hl = libc::linger {
                    l_onoff: i32::from(int_val() == 0),
                    l_linger: 0,
                };
                host_setopt(sock.fd, lvl, opt, &hl)
            }
            OptKind::Error | OptKind::MaxMsgSize => Err(WSAENOPROTOOPT),
            OptKind::Raw => {
                // SAFETY: `optlen` octets lisibles (contrat Winsock) ; layouts identiques
                // (`ip_mreq`, `ipv6_mreq`, `in_addr`).
                let rc = unsafe {
                    libc::setsockopt(sock.fd, lvl, opt, optval.cast(), optlen as libc::socklen_t)
                };
                if rc != 0 {
                    Err(errno_to_wsa(errno()))
                } else {
                    Ok(())
                }
            }
            OptKind::Stored => {
                STORED_OPTS.lock().insert((s, level, optname), int_val());
                Ok(())
            }
        };
        match r {
            Ok(()) => 0,
            Err(e) => fail(e),
        }
    })
}

extern "win64" fn getsockopt(
    s: usize,
    level: i32,
    optname: i32,
    optval: *mut u8,
    optlen: *mut i32,
) -> i32 {
    ffi_guard!(CH, "getsockopt", SOCKET_ERROR, {
        let sock = match lookup(s) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        let (lvl, opt, kind) = match map_opt(level, optname) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        if optval.is_null() || optlen.is_null() {
            return fail(WSAEFAULT);
        }
        // SAFETY: `optlen` non nul.
        let cap = unsafe { *optlen };
        let value: Result<Vec<u8>, u32> = match kind {
            OptKind::Int => host_getopt(sock.fd, lvl, opt, 0i32).map(|v| v.to_le_bytes().to_vec()),
            OptKind::Bool => host_getopt(sock.fd, lvl, opt, 0i32)
                .map(|v| i32::from(v != 0).to_le_bytes().to_vec()),
            OptKind::TimeoutMs => host_getopt(
                sock.fd,
                lvl,
                opt,
                libc::timeval {
                    tv_sec: 0,
                    tv_usec: 0,
                },
            )
            .map(|tv| {
                let ms = tv.tv_sec as u64 * 1000 + tv.tv_usec as u64 / 1000;
                (ms.min(u64::from(u32::MAX)) as u32).to_le_bytes().to_vec()
            }),
            OptKind::Linger => host_getopt(
                sock.fd,
                lvl,
                opt,
                libc::linger {
                    l_onoff: 0,
                    l_linger: 0,
                },
            )
            .map(|l| {
                let mut v = (u16::from(l.l_onoff != 0)).to_le_bytes().to_vec();
                v.extend_from_slice(&(l.l_linger.clamp(0, 0xFFFF) as u16).to_le_bytes());
                v
            }),
            OptKind::DontLinger => host_getopt(
                sock.fd,
                lvl,
                opt,
                libc::linger {
                    l_onoff: 0,
                    l_linger: 0,
                },
            )
            .map(|l| i32::from(l.l_onoff == 0).to_le_bytes().to_vec()),
            OptKind::Error => host_getopt(sock.fd, lvl, opt, 0i32)
                .map(|e| (errno_to_wsa(e) as i32).to_le_bytes().to_vec()),
            OptKind::MaxMsgSize => {
                let v: i32 = if sock.ty == libc::SOCK_DGRAM {
                    65507
                } else {
                    i32::MAX
                };
                Ok(v.to_le_bytes().to_vec())
            }
            OptKind::Raw => {
                let mut buf = [0u8; 64];
                let mut len = buf.len() as libc::socklen_t;
                // SAFETY: tampon local de 64 octets.
                let rc = unsafe {
                    libc::getsockopt(sock.fd, lvl, opt, buf.as_mut_ptr().cast(), &mut len)
                };
                if rc != 0 {
                    Err(errno_to_wsa(errno()))
                } else {
                    Ok(buf[..len as usize].to_vec())
                }
            }
            OptKind::Stored => {
                let v = STORED_OPTS
                    .lock()
                    .get(&(s, level, optname))
                    .copied()
                    .unwrap_or(0);
                Ok(v.to_le_bytes().to_vec())
            }
        };
        let bytes = match value {
            Ok(b) => b,
            Err(e) => return fail(e),
        };
        // BOOL : un `char` (1 octet) est accepté par Windows.
        let n = if (cap as usize) < bytes.len() {
            if matches!(kind, OptKind::Bool) && cap >= 1 {
                1
            } else {
                return fail(WSAEFAULT);
            }
        } else {
            bytes.len()
        };
        // SAFETY: capacité `cap ≥ n` vérifiée.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), optval, n);
            *optlen = n as i32;
        }
        0
    })
}

// ---------- select / WSAPoll ----------

fn fdset_entries(set: *const FdSet) -> Vec<usize> {
    if set.is_null() {
        return Vec::new();
    }
    // SAFETY: `fd_set` Winsock valide (contrat appelant).
    let fs = unsafe { &*set };
    let n = (fs.fd_count as usize).min(64);
    fs.fd_array[..n].to_vec()
}

fn fdset_store(set: *mut FdSet, socks: &[usize]) {
    if set.is_null() {
        return;
    }
    // SAFETY: `fd_set` Winsock valide en écriture (contrat appelant).
    let fs = unsafe { &mut *set };
    fs.fd_count = socks.len() as u32;
    for (i, s) in socks.iter().enumerate() {
        fs.fd_array[i] = *s;
    }
}

fn poll_retry(fds: &mut [libc::pollfd], timeout_ms: i32) -> Result<i32, u32> {
    loop {
        // SAFETY: tableau de pollfd local.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if rc < 0 {
            let e = errno();
            if e == libc::EINTR {
                continue;
            }
            return Err(errno_to_wsa(e));
        }
        return Ok(rc);
    }
}

extern "win64" fn select(
    nfds: i32,
    readfds: *mut FdSet,
    writefds: *mut FdSet,
    exceptfds: *mut FdSet,
    timeout: *const TimeVal,
) -> i32 {
    ffi_guard!(CH, "select", SOCKET_ERROR, {
        let r = fdset_entries(readfds);
        let w = fdset_entries(writefds);
        let x = fdset_entries(exceptfds);
        if r.is_empty() && w.is_empty() && x.is_empty() {
            return fail(WSAEINVAL);
        }
        let timeout_ms = if timeout.is_null() {
            -1
        } else {
            // SAFETY: `timeval` valide (contrat appelant).
            let tv = unsafe { &*timeout };
            if tv.tv_sec < 0 || tv.tv_usec < 0 {
                return fail(WSAEINVAL);
            }
            let ms = i64::from(tv.tv_sec) * 1000 + (i64::from(tv.tv_usec) + 999) / 1000;
            ms.min(i64::from(i32::MAX)) as i32
        };
        let mut uniq: Vec<usize> = Vec::new();
        for s in r.iter().chain(&w).chain(&x) {
            if !uniq.contains(s) {
                uniq.push(*s);
            }
        }
        let mut socks = Vec::with_capacity(uniq.len());
        for s in &uniq {
            match lookup(*s) {
                Ok(v) => socks.push(v),
                Err(e) => return fail(e),
            }
        }
        let mut pfds: Vec<libc::pollfd> = uniq
            .iter()
            .zip(&socks)
            .map(|(s, so)| {
                let mut ev = 0;
                if r.contains(s) {
                    ev |= libc::POLLIN;
                }
                if w.contains(s) {
                    ev |= libc::POLLOUT;
                }
                if x.contains(s) {
                    ev |= libc::POLLPRI;
                }
                libc::pollfd {
                    fd: so.fd,
                    events: ev,
                    revents: 0,
                }
            })
            .collect();
        if let Err(e) = poll_retry(&mut pfds, timeout_ms) {
            return fail(e);
        }
        let (mut ro, mut wo, mut xo) = (Vec::new(), Vec::new(), Vec::new());
        for ((s, so), p) in uniq.iter().zip(&socks).zip(&pfds) {
            let re = p.revents;
            let failed_connect = so.connecting
                && re & (libc::POLLERR | libc::POLLHUP) != 0
                && host_getopt(so.fd, libc::SOL_SOCKET, libc::SO_ERROR, 0i32).unwrap_or(0) != 0;
            if r.contains(s) && re & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                ro.push(*s);
            }
            if w.contains(s) && re & libc::POLLOUT != 0 && !failed_connect {
                wo.push(*s);
                if so.connecting {
                    update(*s, |e| e.connecting = false);
                }
            }
            if x.contains(s) && (re & libc::POLLPRI != 0 || failed_connect) {
                xo.push(*s);
            }
        }
        fdset_store(readfds, &ro);
        fdset_store(writefds, &wo);
        fdset_store(exceptfds, &xo);
        (ro.len() + wo.len() + xo.len()) as i32
    })
}

extern "win64" fn __WSAFDIsSet(s: usize, set: *const FdSet) -> i32 {
    ffi_guard!(CH, "__WSAFDIsSet", 0, {
        i32::from(fdset_entries(set).contains(&s))
    })
}

extern "win64" fn WSAPoll(fds: *mut WsaPollFd, n: u32, timeout: i32) -> i32 {
    ffi_guard!(CH, "WSAPoll", SOCKET_ERROR, {
        if fds.is_null() || n == 0 {
            return fail(WSAEINVAL);
        }
        // SAFETY: tableau de `n` WSAPOLLFD (contrat appelant).
        let arr = unsafe { std::slice::from_raw_parts_mut(fds, n as usize) };
        let mut pfds = Vec::with_capacity(arr.len());
        for e in arr.iter() {
            let fd = lookup(e.fd).map(|s| s.fd).unwrap_or(-1);
            let mut ev = 0;
            if e.events & (POLLRDNORM_W | POLLRDBAND_W) != 0 {
                ev |= libc::POLLIN;
            }
            if e.events & POLLPRI_W != 0 {
                ev |= libc::POLLPRI;
            }
            if e.events & (POLLWRNORM_W | POLLWRBAND_W) != 0 {
                ev |= libc::POLLOUT;
            }
            pfds.push(libc::pollfd {
                fd,
                events: ev,
                revents: 0,
            });
        }
        let invalid = pfds.iter().filter(|p| p.fd < 0).count() as i32;
        let rc = if invalid > 0 {
            // Les entrées invalides sont immédiatement signalées POLLNVAL (pas d'attente).
            poll_retry(&mut pfds, 0)
        } else {
            poll_retry(&mut pfds, timeout)
        };
        let rc = match rc {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        for (e, p) in arr.iter_mut().zip(&pfds) {
            let mut re: i16 = 0;
            if p.fd < 0 {
                re = POLLNVAL_W;
            } else {
                if p.revents & libc::POLLIN != 0 {
                    re |= POLLRDNORM_W;
                }
                if p.revents & libc::POLLPRI != 0 {
                    re |= POLLPRI_W;
                }
                if p.revents & libc::POLLOUT != 0 {
                    re |= POLLWRNORM_W;
                }
                if p.revents & libc::POLLERR != 0 {
                    re |= POLLERR_W;
                }
                if p.revents & libc::POLLHUP != 0 {
                    re |= POLLHUP_W;
                }
                re &= e.events | POLLERR_W | POLLHUP_W | POLLNVAL_W;
            }
            e.revents = re;
        }
        rc + invalid
    })
}

// ---------- Conversions d'octets / adresses textuelles ----------

extern "win64" fn htonl(v: u32) -> u32 {
    v.to_be()
}
extern "win64" fn htons(v: u16) -> u16 {
    v.to_be()
}
extern "win64" fn ntohl(v: u32) -> u32 {
    u32::from_be(v)
}
extern "win64" fn ntohs(v: u16) -> u16 {
    u16::from_be(v)
}

/// Parse BSD `inet_addr` (formes a, a.b, a.b.c, a.b.c.d ; décimal, octal `0`, hexa `0x`).
fn parse_inet_addr(s: &str) -> Option<u32> {
    let s = s.trim_end_matches([' ', '\t']);
    if s.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for p in s.split('.') {
        let v = if let Some(h) = p.strip_prefix("0x").or_else(|| p.strip_prefix("0X")) {
            if h.is_empty() {
                0
            } else {
                u32::from_str_radix(h, 16).ok()?
            }
        } else if p.len() > 1 && p.starts_with('0') {
            u32::from_str_radix(&p[1..], 8).ok()?
        } else {
            if p.is_empty() {
                return None;
            }
            p.parse::<u32>().ok()?
        };
        parts.push(v);
    }
    let v = match parts.as_slice() {
        [a] => *a,
        [a, b] if *a <= 0xFF && *b <= 0xFF_FFFF => (a << 24) | b,
        [a, b, c] if *a <= 0xFF && *b <= 0xFF && *c <= 0xFFFF => (a << 24) | (b << 16) | c,
        [a, b, c, d] if *a <= 0xFF && *b <= 0xFF && *c <= 0xFF && *d <= 0xFF => {
            (a << 24) | (b << 16) | (c << 8) | d
        }
        _ => return None,
    };
    Some(v)
}

extern "win64" fn inet_addr(cp: *const u8) -> u32 {
    ffi_guard!(CH, "inet_addr", u32::MAX, {
        // SAFETY: chaîne C (contrat Winsock).
        let Some(s) = (unsafe { cstr_arg(cp) }) else {
            return u32::MAX; // INADDR_NONE
        };
        parse_inet_addr(&s).map_or(u32::MAX, u32::to_be)
    })
}

thread_local! {
    static NTOA_BUF: RefCell<[u8; 16]> = const { RefCell::new([0; 16]) };
}

extern "win64" fn inet_ntoa(addr: u32) -> *const u8 {
    ffi_guard!(CH, "inet_ntoa", ptr::null(), {
        let txt = Ipv4Addr::from(addr.to_ne_bytes()).to_string();
        NTOA_BUF.with(|b| {
            let mut b = b.borrow_mut();
            *b = [0; 16];
            b[..txt.len()].copy_from_slice(txt.as_bytes());
            b.as_ptr()
        })
    })
}

fn pton(family: i32, s: &str, dst: *mut u8) -> i32 {
    match family {
        AF_INET_W => match s.parse::<Ipv4Addr>() {
            Ok(a) => {
                // SAFETY: `dst` reçoit un `IN_ADDR` (4 octets) (contrat appelant).
                unsafe { ptr::copy_nonoverlapping(a.octets().as_ptr(), dst, 4) };
                1
            }
            Err(_) => 0,
        },
        AF_INET6_W => match s.parse::<Ipv6Addr>() {
            Ok(a) => {
                // SAFETY: `dst` reçoit un `IN6_ADDR` (16 octets) (contrat appelant).
                unsafe { ptr::copy_nonoverlapping(a.octets().as_ptr(), dst, 16) };
                1
            }
            Err(_) => 0,
        },
        _ => {
            set_error(WSAEAFNOSUPPORT);
            -1
        }
    }
}

extern "win64" fn inet_pton(family: i32, src: *const u8, dst: *mut u8) -> i32 {
    ffi_guard!(CH, "inet_pton", -1, {
        if src.is_null() || dst.is_null() {
            set_error(WSAEFAULT);
            return -1;
        }
        // SAFETY: chaîne C non nulle.
        let s = unsafe { cstr_arg(src) }.unwrap_or_default();
        pton(family, &s, dst)
    })
}

extern "win64" fn InetPtonW(family: i32, src: *const u16, dst: *mut u8) -> i32 {
    ffi_guard!(CH, "InetPtonW", -1, {
        if src.is_null() || dst.is_null() {
            set_error(WSAEFAULT);
            return -1;
        }
        // SAFETY: chaîne UTF-16 non nulle.
        let s = unsafe { wstr_arg(src) }.unwrap_or_default();
        pton(family, &s, dst)
    })
}

fn ntop(family: i32, src: *const u8) -> Result<String, u32> {
    if src.is_null() {
        return Err(WSAEFAULT);
    }
    match family {
        AF_INET_W => {
            let mut o = [0u8; 4];
            // SAFETY: `src` pointe un IN_ADDR (contrat appelant).
            unsafe { ptr::copy_nonoverlapping(src, o.as_mut_ptr(), 4) };
            Ok(Ipv4Addr::from(o).to_string())
        }
        AF_INET6_W => {
            let mut o = [0u8; 16];
            // SAFETY: `src` pointe un IN6_ADDR (contrat appelant).
            unsafe { ptr::copy_nonoverlapping(src, o.as_mut_ptr(), 16) };
            Ok(Ipv6Addr::from(o).to_string())
        }
        _ => Err(WSAEAFNOSUPPORT),
    }
}

extern "win64" fn inet_ntop(family: i32, src: *const u8, dst: *mut u8, size: usize) -> *const u8 {
    ffi_guard!(CH, "inet_ntop", ptr::null(), {
        let txt = match ntop(family, src) {
            Ok(t) => t,
            Err(e) => {
                set_error(e);
                return ptr::null();
            }
        };
        if dst.is_null() || size < txt.len() + 1 {
            set_error(ERROR_INVALID_PARAMETER);
            return ptr::null();
        }
        // SAFETY: capacité `size` vérifiée.
        unsafe {
            ptr::copy_nonoverlapping(txt.as_ptr(), dst, txt.len());
            *dst.add(txt.len()) = 0;
        }
        dst
    })
}

extern "win64" fn InetNtopW(family: i32, src: *const u8, dst: *mut u16, size: usize) -> *const u16 {
    ffi_guard!(CH, "InetNtopW", ptr::null(), {
        let txt = match ntop(family, src) {
            Ok(t) => t,
            Err(e) => {
                set_error(e);
                return ptr::null();
            }
        };
        let w: Vec<u16> = txt.encode_utf16().collect();
        if dst.is_null() || size < w.len() + 1 {
            set_error(ERROR_INVALID_PARAMETER);
            return ptr::null();
        }
        // SAFETY: capacité `size` (en WCHAR) vérifiée.
        unsafe {
            ptr::copy_nonoverlapping(w.as_ptr(), dst, w.len());
            *dst.add(w.len()) = 0;
        }
        dst
    })
}

// ---------- Résolution de noms ----------

const SERVICES: &[(&str, u16, &str)] = &[
    ("echo", 7, "tcp"),
    ("echo", 7, "udp"),
    ("discard", 9, "tcp"),
    ("discard", 9, "udp"),
    ("daytime", 13, "tcp"),
    ("ftp-data", 20, "tcp"),
    ("ftp", 21, "tcp"),
    ("ssh", 22, "tcp"),
    ("telnet", 23, "tcp"),
    ("smtp", 25, "tcp"),
    ("time", 37, "tcp"),
    ("domain", 53, "tcp"),
    ("domain", 53, "udp"),
    ("bootps", 67, "udp"),
    ("bootpc", 68, "udp"),
    ("tftp", 69, "udp"),
    ("http", 80, "tcp"),
    ("www", 80, "tcp"),
    ("pop3", 110, "tcp"),
    ("ntp", 123, "udp"),
    ("imap", 143, "tcp"),
    ("snmp", 161, "udp"),
    ("ldap", 389, "tcp"),
    ("https", 443, "tcp"),
    ("https", 443, "udp"),
    ("syslog", 514, "udp"),
    ("imaps", 993, "tcp"),
    ("pop3s", 995, "tcp"),
];

const PROTOCOLS: &[(&str, i16)] = &[
    ("ip", 0),
    ("icmp", 1),
    ("ggp", 3),
    ("tcp", 6),
    ("pup", 12),
    ("udp", 17),
    ("idp", 22),
    ("ipv6", 41),
    ("icmpv6", 58),
    ("nd", 77),
    ("raw", 255),
];

fn service_port(name: &str, proto: Option<&str>) -> Option<u16> {
    if let Ok(p) = name.parse::<u16>() {
        return Some(p);
    }
    SERVICES
        .iter()
        .find(|(n, _, p)| {
            n.eq_ignore_ascii_case(name) && proto.is_none_or(|q| q.eq_ignore_ascii_case(p))
        })
        .map(|(_, port, _)| *port)
}

/// Résolution soumise à la politique offline (`Err` = code WSA).
fn resolve_host(name: &str, family: i32) -> Result<Vec<IpAddr>, u32> {
    let want = |ip: &IpAddr| match family {
        AF_INET_W => ip.is_ipv4(),
        AF_INET6_W => ip.is_ipv6(),
        _ => true,
    };
    let bare = name.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return if want(&ip) {
            Ok(vec![ip])
        } else {
            Err(WSAHOST_NOT_FOUND)
        };
    }
    if is_local_hostname(name) {
        let all = [
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        ];
        return Ok(all.into_iter().filter(|ip| want(ip)).collect());
    }
    if offline_mode() {
        sb_debug!(
            CH,
            "offline strict : résolution DNS de '{name}' refusée (WSAHOST_NOT_FOUND)"
        );
        return Err(WSAHOST_NOT_FOUND);
    }
    let v: Vec<IpAddr> = (name, 0u16)
        .to_socket_addrs()
        .map_err(|_| WSAHOST_NOT_FOUND)?
        .map(|a| a.ip())
        .filter(|ip| want(ip))
        .collect();
    if v.is_empty() {
        Err(WSAHOST_NOT_FOUND)
    } else {
        Ok(v)
    }
}

type ProtoStore = (CString, [*mut i8; 1], ProtoEnt);
type ServStore = (CString, CString, [*mut i8; 1], ServEnt);

struct HostStore {
    _name: CString,
    aliases: [*mut i8; 1],
    _addrs: Vec<[u8; 16]>,
    addr_ptrs: Vec<*mut i8>,
    ent: HostEnt,
}

thread_local! {
    static HOSTENT: RefCell<Option<Box<HostStore>>> = const { RefCell::new(None) };
    static PROTOENT: RefCell<Option<Box<ProtoStore>>> = const { RefCell::new(None) };
    static SERVENT: RefCell<Option<Box<ServStore>>> = const { RefCell::new(None) };
}

fn make_hostent(name: &str, ips: &[IpAddr]) -> *mut HostEnt {
    let v4 = ips.iter().all(IpAddr::is_ipv4);
    let addrs: Vec<[u8; 16]> = ips
        .iter()
        .filter(|ip| ip.is_ipv4() == v4)
        .map(|ip| {
            let mut a = [0u8; 16];
            match ip {
                IpAddr::V4(x) => a[..4].copy_from_slice(&x.octets()),
                IpAddr::V6(x) => a.copy_from_slice(&x.octets()),
            }
            a
        })
        .collect();
    let cname = CString::new(name.replace('\0', "")).unwrap_or_default();
    let mut st = Box::new(HostStore {
        _name: cname,
        aliases: [ptr::null_mut()],
        _addrs: addrs,
        addr_ptrs: Vec::new(),
        ent: HostEnt {
            h_name: ptr::null_mut(),
            h_aliases: ptr::null_mut(),
            h_addrtype: if v4 {
                AF_INET_W as i16
            } else {
                AF_INET6_W as i16
            },
            h_length: if v4 { 4 } else { 16 },
            h_addr_list: ptr::null_mut(),
        },
    });
    let mut ptrs: Vec<*mut i8> = st
        ._addrs
        .iter_mut()
        .map(|a| a.as_mut_ptr().cast())
        .collect();
    ptrs.push(ptr::null_mut());
    st.addr_ptrs = ptrs;
    st.ent.h_name = st._name.as_ptr().cast_mut();
    st.ent.h_aliases = st.aliases.as_mut_ptr();
    st.ent.h_addr_list = st.addr_ptrs.as_mut_ptr();
    let p = &raw mut st.ent;
    HOSTENT.with(|h| *h.borrow_mut() = Some(st));
    p
}

extern "win64" fn gethostbyname(name: *const u8) -> *mut HostEnt {
    ffi_guard!(CH, "gethostbyname", ptr::null_mut(), {
        if let Err(e) = ensure_started() {
            set_error(e);
            return ptr::null_mut();
        }
        // SAFETY: chaîne C ou nul (contrat Winsock).
        let n = unsafe { cstr_arg(name) }.unwrap_or_else(|| VIRTUAL_HOSTNAME.to_string());
        match resolve_host(&n, AF_INET_W) {
            Ok(ips) if !ips.is_empty() => make_hostent(&n, &ips),
            Ok(_) => {
                set_error(WSANO_DATA);
                ptr::null_mut()
            }
            Err(e) => {
                set_error(e);
                ptr::null_mut()
            }
        }
    })
}

extern "win64" fn gethostbyaddr(addr: *const u8, len: i32, ty: i32) -> *mut HostEnt {
    ffi_guard!(CH, "gethostbyaddr", ptr::null_mut(), {
        if let Err(e) = ensure_started() {
            set_error(e);
            return ptr::null_mut();
        }
        if addr.is_null() {
            set_error(WSAEFAULT);
            return ptr::null_mut();
        }
        let ip = match (ty, len) {
            (AF_INET_W, 4..) => {
                let mut o = [0u8; 4];
                // SAFETY: 4 octets lisibles (vérifié).
                unsafe { ptr::copy_nonoverlapping(addr, o.as_mut_ptr(), 4) };
                IpAddr::V4(Ipv4Addr::from(o))
            }
            (AF_INET6_W, 16..) => {
                let mut o = [0u8; 16];
                // SAFETY: 16 octets lisibles (vérifié).
                unsafe { ptr::copy_nonoverlapping(addr, o.as_mut_ptr(), 16) };
                IpAddr::V6(Ipv6Addr::from(o))
            }
            (AF_INET_W | AF_INET6_W, _) => {
                set_error(WSAEFAULT);
                return ptr::null_mut();
            }
            _ => {
                set_error(WSAEAFNOSUPPORT);
                return ptr::null_mut();
            }
        };
        if is_loopback_ip(&ip) {
            return make_hostent("localhost", &[ip]);
        }
        if offline_mode() {
            set_error(WSAHOST_NOT_FOUND);
            return ptr::null_mut();
        }
        match reverse_lookup(&SocketAddr::new(ip, 0)) {
            Some(n) => make_hostent(&n, &[ip]),
            None => {
                set_error(WSAHOST_NOT_FOUND);
                ptr::null_mut()
            }
        }
    })
}

/// Résolution inverse hôte (mode en ligne uniquement).
fn reverse_lookup(a: &SocketAddr) -> Option<String> {
    let (ss, len) = to_libc(a);
    let mut host = [0 as libc::c_char; 1025];
    // SAFETY: tampons locaux, adresse convertie.
    let rc = unsafe {
        libc::getnameinfo(
            (&raw const ss).cast(),
            len,
            host.as_mut_ptr(),
            host.len() as libc::socklen_t,
            ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        )
    };
    if rc != 0 {
        return None;
    }
    // SAFETY: `getnameinfo` termine la chaîne.
    Some(
        unsafe { std::ffi::CStr::from_ptr(host.as_ptr()) }
            .to_string_lossy()
            .into_owned(),
    )
}

extern "win64" fn gethostname(name: *mut u8, namelen: i32) -> i32 {
    ffi_guard!(CH, "gethostname", SOCKET_ERROR, {
        if let Err(e) = ensure_started() {
            return fail(e);
        }
        let n = VIRTUAL_HOSTNAME.as_bytes();
        if name.is_null() || namelen < n.len() as i32 + 1 {
            return fail(WSAEFAULT);
        }
        // SAFETY: capacité vérifiée.
        unsafe {
            ptr::copy_nonoverlapping(n.as_ptr(), name, n.len());
            *name.add(n.len()) = 0;
        }
        0
    })
}

fn make_protoent(name: &str, num: i16) -> *mut ProtoEnt {
    let mut b = Box::new((
        CString::new(name).unwrap_or_default(),
        [ptr::null_mut::<i8>()],
        ProtoEnt {
            p_name: ptr::null_mut(),
            p_aliases: ptr::null_mut(),
            p_proto: num,
        },
    ));
    b.2.p_name = b.0.as_ptr().cast_mut();
    b.2.p_aliases = b.1.as_mut_ptr();
    let p = &raw mut b.2;
    PROTOENT.with(|c| *c.borrow_mut() = Some(b));
    p
}

extern "win64" fn getprotobyname(name: *const u8) -> *mut ProtoEnt {
    ffi_guard!(CH, "getprotobyname", ptr::null_mut(), {
        // SAFETY: chaîne C ou nul.
        let Some(n) = (unsafe { cstr_arg(name) }) else {
            set_error(WSAEFAULT);
            return ptr::null_mut();
        };
        match PROTOCOLS.iter().find(|(p, _)| p.eq_ignore_ascii_case(&n)) {
            Some((p, num)) => make_protoent(p, *num),
            None => {
                set_error(WSANO_DATA);
                ptr::null_mut()
            }
        }
    })
}

extern "win64" fn getprotobynumber(num: i32) -> *mut ProtoEnt {
    ffi_guard!(CH, "getprotobynumber", ptr::null_mut(), {
        match PROTOCOLS.iter().find(|(_, n)| i32::from(*n) == num) {
            Some((p, n)) => make_protoent(p, *n),
            None => {
                set_error(WSANO_DATA);
                ptr::null_mut()
            }
        }
    })
}

fn make_servent(name: &str, port: u16, proto: &str) -> *mut ServEnt {
    let mut b = Box::new((
        CString::new(name).unwrap_or_default(),
        CString::new(proto).unwrap_or_default(),
        [ptr::null_mut::<i8>()],
        ServEnt {
            s_name: ptr::null_mut(),
            s_aliases: ptr::null_mut(),
            s_proto: ptr::null_mut(),
            s_port: port.to_be() as i16,
        },
    ));
    b.3.s_name = b.0.as_ptr().cast_mut();
    b.3.s_proto = b.1.as_ptr().cast_mut();
    b.3.s_aliases = b.2.as_mut_ptr();
    let p = &raw mut b.3;
    SERVENT.with(|c| *c.borrow_mut() = Some(b));
    p
}

extern "win64" fn getservbyname(name: *const u8, proto: *const u8) -> *mut ServEnt {
    ffi_guard!(CH, "getservbyname", ptr::null_mut(), {
        // SAFETY: chaînes C ou nul.
        let (n, p) = unsafe { (cstr_arg(name), cstr_arg(proto)) };
        let Some(n) = n else {
            set_error(WSAEFAULT);
            return ptr::null_mut();
        };
        match SERVICES.iter().find(|(sn, _, sp)| {
            sn.eq_ignore_ascii_case(&n) && p.as_deref().is_none_or(|q| q.eq_ignore_ascii_case(sp))
        }) {
            Some((sn, port, sp)) => make_servent(sn, *port, sp),
            None => {
                set_error(WSANO_DATA);
                ptr::null_mut()
            }
        }
    })
}

extern "win64" fn getservbyport(port: i32, proto: *const u8) -> *mut ServEnt {
    ffi_guard!(CH, "getservbyport", ptr::null_mut(), {
        let port = u16::from_be(port as u16);
        // SAFETY: chaîne C ou nul.
        let p = unsafe { cstr_arg(proto) };
        match SERVICES.iter().find(|(_, sp, pr)| {
            *sp == port && p.as_deref().is_none_or(|q| q.eq_ignore_ascii_case(pr))
        }) {
            Some((sn, port, sp)) => make_servent(sn, *port, sp),
            None => {
                set_error(WSANO_DATA);
                ptr::null_mut()
            }
        }
    })
}

// ---------- getaddrinfo / GetAddrInfoW ----------

struct AiBlock {
    _nodes: Box<[AddrInfo]>,
    _addrs: Box<[[u8; 28]]>,
    _canon_a: Option<CString>,
    _canon_w: Option<Vec<u16>>,
}

// SAFETY: les pointeurs internes ne référencent que la mémoire possédée par le bloc.
unsafe impl Send for AiBlock {}

static AI_BLOCKS: LazyLock<Mutex<HashMap<usize, AiBlock>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct Hints {
    flags: i32,
    family: i32,
    socktype: i32,
    protocol: i32,
}

fn read_hints(h: *const AddrInfo) -> Hints {
    if h.is_null() {
        return Hints {
            flags: 0,
            family: AF_UNSPEC_W,
            socktype: 0,
            protocol: 0,
        };
    }
    // SAFETY: `hints` valide (contrat appelant).
    let h = unsafe { &*h };
    Hints {
        flags: h.ai_flags,
        family: h.ai_family,
        socktype: h.ai_socktype,
        protocol: h.ai_protocol,
    }
}

fn addrinfo_resolve(
    node: Option<&str>,
    service: Option<&str>,
    h: &Hints,
) -> Result<Vec<SocketAddr>, u32> {
    ensure_started()?;
    if !matches!(h.family, AF_UNSPEC_W | AF_INET_W | AF_INET6_W) {
        return Err(WSAEAFNOSUPPORT);
    }
    if !(0..=3).contains(&h.socktype) {
        return Err(WSAESOCKTNOSUPPORT);
    }
    if node.is_none() && service.is_none() {
        return Err(WSAHOST_NOT_FOUND);
    }
    if node.is_none() && h.flags & AI_CANONNAME != 0 {
        return Err(WSAEINVAL);
    }
    let proto_name = match (h.socktype, h.protocol) {
        (2, _) | (_, IPPROTO_UDP_W) => Some("udp"),
        (1, _) | (_, IPPROTO_TCP_W) => Some("tcp"),
        _ => None,
    };
    let port = match service {
        None => 0,
        Some(s) => {
            if h.flags & AI_NUMERICSERV != 0 && s.parse::<u16>().is_err() {
                return Err(WSATYPE_NOT_FOUND);
            }
            service_port(s, proto_name).ok_or(WSATYPE_NOT_FOUND)?
        }
    };
    let ips: Vec<IpAddr> = match node {
        None => {
            let (v6, v4) = if h.flags & AI_PASSIVE != 0 {
                (
                    IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                    IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                )
            } else {
                (
                    IpAddr::V6(Ipv6Addr::LOCALHOST),
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                )
            };
            match h.family {
                AF_INET_W => vec![v4],
                AF_INET6_W => vec![v6],
                _ => vec![v6, v4],
            }
        }
        Some(n) => {
            let bare = n.trim_start_matches('[').trim_end_matches(']');
            if h.flags & AI_NUMERICHOST != 0 && bare.parse::<IpAddr>().is_err() {
                return Err(WSAHOST_NOT_FOUND);
            }
            resolve_host(n, h.family)?
        }
    };
    if ips.is_empty() {
        return Err(WSAHOST_NOT_FOUND);
    }
    Ok(ips
        .into_iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect())
}

fn build_addrinfo(
    addrs: &[SocketAddr],
    h: &Hints,
    canon: Option<&str>,
    wide: bool,
) -> *mut AddrInfo {
    let mut raw: Box<[[u8; 28]]> = addrs.iter().map(|a| win_sockaddr_bytes(a).0).collect();
    let mut nodes: Box<[AddrInfo]> = addrs
        .iter()
        .enumerate()
        .map(|(i, a)| AddrInfo {
            ai_flags: 0,
            ai_family: if a.is_ipv4() { AF_INET_W } else { AF_INET6_W },
            ai_socktype: h.socktype,
            ai_protocol: h.protocol,
            ai_addrlen: if a.is_ipv4() { 16 } else { 28 },
            ai_canonname: ptr::null_mut(),
            ai_addr: raw[i].as_mut_ptr(),
            ai_next: ptr::null_mut(),
        })
        .collect();
    let mut canon_a = None;
    let mut canon_w = None;
    if let Some(c) = canon {
        if wide {
            let mut w: Vec<u16> = c.encode_utf16().chain(std::iter::once(0)).collect();
            nodes[0].ai_canonname = w.as_mut_ptr().cast();
            canon_w = Some(w);
        } else {
            let cs = CString::new(c.replace('\0', "")).unwrap_or_default();
            nodes[0].ai_canonname = cs.as_ptr().cast_mut().cast();
            canon_a = Some(cs);
        }
    }
    let base = nodes.as_mut_ptr();
    for i in 0..nodes.len().saturating_sub(1) {
        // SAFETY: `i + 1 < len`, pointeur dans le même bloc.
        nodes[i].ai_next = unsafe { base.add(i + 1) };
    }
    let head = base;
    AI_BLOCKS.lock().insert(
        head as usize,
        AiBlock {
            _nodes: nodes,
            _addrs: raw,
            _canon_a: canon_a,
            _canon_w: canon_w,
        },
    );
    head
}

fn getaddrinfo_impl(
    node: Option<String>,
    service: Option<String>,
    hints: *const AddrInfo,
    res: *mut *mut AddrInfo,
    wide: bool,
) -> i32 {
    if res.is_null() {
        set_error(WSAEFAULT);
        return WSAEFAULT as i32;
    }
    // SAFETY: `res` non nul.
    unsafe { *res = ptr::null_mut() };
    let h = read_hints(hints);
    match addrinfo_resolve(node.as_deref(), service.as_deref(), &h) {
        Ok(addrs) => {
            let canon = if h.flags & AI_CANONNAME != 0 {
                node.as_deref()
            } else {
                None
            };
            let head = build_addrinfo(&addrs, &h, canon, wide);
            // SAFETY: `res` non nul.
            unsafe { *res = head };
            set_error(0);
            0
        }
        Err(e) => {
            set_error(e);
            e as i32
        }
    }
}

extern "win64" fn getaddrinfo(
    node: *const u8,
    service: *const u8,
    hints: *const AddrInfo,
    res: *mut *mut AddrInfo,
) -> i32 {
    ffi_guard!(CH, "getaddrinfo", WSAEFAULT as i32, {
        // SAFETY: chaînes C ou nul.
        let (n, s) = unsafe { (cstr_arg(node), cstr_arg(service)) };
        getaddrinfo_impl(n, s, hints, res, false)
    })
}

extern "win64" fn GetAddrInfoW(
    node: *const u16,
    service: *const u16,
    hints: *const AddrInfo,
    res: *mut *mut AddrInfo,
) -> i32 {
    ffi_guard!(CH, "GetAddrInfoW", WSAEFAULT as i32, {
        // SAFETY: chaînes UTF-16 ou nul.
        let (n, s) = unsafe { (wstr_arg(node), wstr_arg(service)) };
        getaddrinfo_impl(n, s, hints, res, true)
    })
}

extern "win64" fn freeaddrinfo(ai: *mut AddrInfo) {
    ffi_guard!(CH, "freeaddrinfo", (), {
        if !ai.is_null() {
            AI_BLOCKS.lock().remove(&(ai as usize));
        }
    })
}

extern "win64" fn FreeAddrInfoW(ai: *mut AddrInfo) {
    freeaddrinfo(ai);
}

extern "win64" fn getnameinfo(
    sa: *const u8,
    salen: i32,
    host: *mut u8,
    hostlen: u32,
    serv: *mut u8,
    servlen: u32,
    flags: i32,
) -> i32 {
    ffi_guard!(CH, "getnameinfo", WSAEFAULT as i32, {
        let err = |e: u32| {
            set_error(e);
            e as i32
        };
        // SAFETY: `sa` lisible sur `salen` octets (contrat Winsock).
        let a = match unsafe { read_sockaddr(sa, salen) } {
            Ok(a) => a,
            Err(e) => return err(e),
        };
        if (host.is_null() || hostlen == 0) && (serv.is_null() || servlen == 0) {
            return err(WSAHOST_NOT_FOUND);
        }
        if !host.is_null() && hostlen > 0 {
            let name = if flags & NI_NUMERICHOST != 0 {
                a.ip().to_string()
            } else if is_loopback_ip(&a.ip()) {
                "localhost".to_string()
            } else if offline_mode() {
                if flags & NI_NAMEREQD != 0 {
                    return err(WSAHOST_NOT_FOUND);
                }
                a.ip().to_string()
            } else {
                match reverse_lookup(&a) {
                    Some(n) => n,
                    None if flags & NI_NAMEREQD != 0 => return err(WSAHOST_NOT_FOUND),
                    None => a.ip().to_string(),
                }
            };
            if name.len() + 1 > hostlen as usize {
                return err(WSAEFAULT);
            }
            // SAFETY: capacité vérifiée.
            unsafe {
                ptr::copy_nonoverlapping(name.as_ptr(), host, name.len());
                *host.add(name.len()) = 0;
            }
        }
        if !serv.is_null() && servlen > 0 {
            let proto = if flags & NI_DGRAM != 0 { "udp" } else { "tcp" };
            let sname = if flags & NI_NUMERICSERV != 0 {
                a.port().to_string()
            } else {
                SERVICES
                    .iter()
                    .find(|(_, p, pr)| *p == a.port() && *pr == proto)
                    .map_or_else(|| a.port().to_string(), |(n, _, _)| (*n).to_string())
            };
            if sname.len() + 1 > servlen as usize {
                return err(WSAEFAULT);
            }
            // SAFETY: capacité vérifiée.
            unsafe {
                ptr::copy_nonoverlapping(sname.as_ptr(), serv, sname.len());
                *serv.add(sname.len()) = 0;
            }
        }
        set_error(0);
        0
    })
}

// ---------- Tables d'exports ----------

macro_rules! ws2_table {
    ($( $name:literal, $ord:expr, $f:ident; )*) => {
        fn table() -> Vec<(&'static str, Option<u16>, usize)> {
            vec![ $( ($name, $ord, $f as *const () as usize), )* ]
        }
    };
}

ws2_table! {
    "accept", Some(1), accept;
    "bind", Some(2), bind;
    "closesocket", Some(3), closesocket;
    "connect", Some(4), connect;
    "getpeername", Some(5), getpeername;
    "getsockname", Some(6), getsockname;
    "getsockopt", Some(7), getsockopt;
    "htonl", Some(8), htonl;
    "htons", Some(9), htons;
    "ioctlsocket", Some(10), ioctlsocket;
    "inet_addr", Some(11), inet_addr;
    "inet_ntoa", Some(12), inet_ntoa;
    "listen", Some(13), listen;
    "ntohl", Some(14), ntohl;
    "ntohs", Some(15), ntohs;
    "recv", Some(16), recv;
    "recvfrom", Some(17), recvfrom;
    "select", Some(18), select;
    "send", Some(19), send;
    "sendto", Some(20), sendto;
    "setsockopt", Some(21), setsockopt;
    "shutdown", Some(22), shutdown;
    "socket", Some(23), socket;
    "gethostbyaddr", Some(51), gethostbyaddr;
    "gethostbyname", Some(52), gethostbyname;
    "getprotobyname", Some(53), getprotobyname;
    "getprotobynumber", Some(54), getprotobynumber;
    "getservbyname", Some(55), getservbyname;
    "getservbyport", Some(56), getservbyport;
    "gethostname", Some(57), gethostname;
    "WSAGetLastError", Some(111), WSAGetLastError;
    "WSASetLastError", Some(112), WSASetLastError;
    "WSAStartup", Some(115), WSAStartup;
    "WSACleanup", Some(116), WSACleanup;
    "__WSAFDIsSet", Some(151), __WSAFDIsSet;
    "getaddrinfo", None, getaddrinfo;
    "freeaddrinfo", None, freeaddrinfo;
    "GetAddrInfoW", None, GetAddrInfoW;
    "FreeAddrInfoW", None, FreeAddrInfoW;
    "getnameinfo", None, getnameinfo;
    "WSASocketW", None, WSASocketW;
    "WSASocketA", None, WSASocketA;
    "WSASend", None, WSASend;
    "WSARecv", None, WSARecv;
    "WSAIoctl", None, WSAIoctl;
    "WSAPoll", None, WSAPoll;
    "inet_pton", None, inet_pton;
    "inet_ntop", None, inet_ntop;
    "InetPtonA", None, inet_pton;
    "InetNtopA", None, inet_ntop;
    "InetPtonW", None, InetPtonW;
    "InetNtopW", None, InetNtopW;
}

pub(crate) fn exports() -> HashMap<&'static str, usize> {
    table().into_iter().map(|(n, _, a)| (n, a)).collect()
}

pub(crate) fn ordinals() -> HashMap<u16, usize> {
    table()
        .into_iter()
        .filter_map(|(_, o, a)| o.map(|o| (o, a)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn startup() {
        // SAFETY: zéro valide pour WSADATA.
        let mut d: WsaData = unsafe { std::mem::zeroed() };
        assert_eq!(WSAStartup(0x0202, &mut d), 0);
    }

    fn sa_v4(ip: [u8; 4], port: u16) -> SockAddrIn {
        SockAddrIn {
            sin_family: AF_INET_W as u16,
            sin_port: port.to_be(),
            sin_addr: ip,
            sin_zero: [0; 8],
        }
    }

    fn sa_ptr(sa: &SockAddrIn) -> *const u8 {
        (sa as *const SockAddrIn).cast()
    }

    fn local_port(s: usize) -> u16 {
        let mut out = sa_v4([0; 4], 0);
        let mut len = 16;
        assert_eq!(getsockname(s, (&raw mut out).cast(), &mut len), 0);
        assert_eq!(len, 16);
        u16::from_be(out.sin_port)
    }

    fn listener() -> (usize, u16) {
        let l = socket(AF_INET_W, 1, IPPROTO_TCP_W);
        assert_ne!(l, INVALID_SOCKET);
        let sa = sa_v4([127, 0, 0, 1], 0);
        assert_eq!(bind(l, sa_ptr(&sa), 16), 0);
        assert_eq!(listen(l, 0x7FFF_FFFF), 0);
        let p = local_port(l);
        (l, p)
    }

    #[test]
    fn ordinals_match_names() {
        let names = exports();
        let ords = ordinals();
        let expected = [
            (1, "accept"),
            (2, "bind"),
            (3, "closesocket"),
            (4, "connect"),
            (5, "getpeername"),
            (6, "getsockname"),
            (7, "getsockopt"),
            (8, "htonl"),
            (9, "htons"),
            (10, "ioctlsocket"),
            (11, "inet_addr"),
            (12, "inet_ntoa"),
            (13, "listen"),
            (14, "ntohl"),
            (15, "ntohs"),
            (16, "recv"),
            (17, "recvfrom"),
            (18, "select"),
            (19, "send"),
            (20, "sendto"),
            (21, "setsockopt"),
            (22, "shutdown"),
            (23, "socket"),
            (51, "gethostbyaddr"),
            (52, "gethostbyname"),
            (53, "getprotobyname"),
            (55, "getservbyname"),
            (57, "gethostname"),
            (111, "WSAGetLastError"),
            (112, "WSASetLastError"),
            (115, "WSAStartup"),
            (116, "WSACleanup"),
            (151, "__WSAFDIsSet"),
        ];
        for (o, n) in expected {
            assert_eq!(ords.get(&o), names.get(n), "ordinal {o} ({n})");
            assert!(ords.contains_key(&o));
        }
    }

    #[test]
    fn startup_fills_wsadata() {
        // SAFETY: zéro valide.
        let mut d: WsaData = unsafe { std::mem::zeroed() };
        assert_eq!(WSAStartup(0x0101 - 1, &mut d), WSAVERNOTSUPPORTED as i32);
        assert_eq!(WSAStartup(0x0202, ptr::null_mut()), WSAEFAULT as i32);
        assert_eq!(WSAStartup(0x0303, &mut d), 0);
        assert_eq!(d.w_version, 0x0202);
        assert_eq!(d.w_high_version, 0x0202);
        assert_eq!(&d.sz_description[..11], b"WinSock 2.0");
        assert_eq!(&d.sz_system_status[..8], b"Running\0");
        assert_eq!(WSACleanup(), 0);
    }

    #[test]
    fn tcp_loopback_roundtrip_with_select() {
        startup();
        let (l, port) = listener();
        let c = socket(AF_INET_W, 1, 0);
        let sa = sa_v4([127, 0, 0, 1], port);
        assert_eq!(connect(c, sa_ptr(&sa), 16), 0);

        // select : le listener devient lisible.
        let mut rs = FdSet {
            fd_count: 1,
            fd_array: [0; 64],
        };
        rs.fd_array[0] = l;
        let tv = TimeVal {
            tv_sec: 2,
            tv_usec: 0,
        };
        assert_eq!(select(0, &mut rs, ptr::null_mut(), ptr::null_mut(), &tv), 1);
        assert_eq!(__WSAFDIsSet(l, &rs), 1);

        let mut peer = sa_v4([0; 4], 0);
        let mut plen = 16;
        let a = accept(l, (&raw mut peer).cast(), &mut plen);
        assert_ne!(a, INVALID_SOCKET);
        assert_eq!(peer.sin_addr, [127, 0, 0, 1]);

        assert_eq!(send(c, b"hello".as_ptr(), 5, 0), 5);
        let mut rs = FdSet {
            fd_count: 1,
            fd_array: [0; 64],
        };
        rs.fd_array[0] = a;
        assert_eq!(select(0, &mut rs, ptr::null_mut(), ptr::null_mut(), &tv), 1);
        let mut avail = 0u32;
        assert_eq!(ioctlsocket(a, FIONREAD as i32, &mut avail), 0);
        assert_eq!(avail, 5);
        let mut buf = [0u8; 16];
        assert_eq!(recv(a, buf.as_mut_ptr(), 16, 0), 5);
        assert_eq!(&buf[..5], b"hello");

        // WSASend / WSARecv
        let mut d1 = *b"ab";
        let mut d2 = *b"cd";
        let bufs = [
            WsaBuf {
                len: 2,
                buf: d1.as_mut_ptr(),
            },
            WsaBuf {
                len: 2,
                buf: d2.as_mut_ptr(),
            },
        ];
        let mut sent = 0u32;
        assert_eq!(
            WSASend(a, bufs.as_ptr(), 2, &mut sent, 0, ptr::null_mut(), 0),
            0
        );
        assert_eq!(sent, 4);
        let mut rb = [0u8; 8];
        let rbuf = [WsaBuf {
            len: 8,
            buf: rb.as_mut_ptr(),
        }];
        let (mut got, mut fl) = (0u32, 0u32);
        assert_eq!(
            WSARecv(c, rbuf.as_ptr(), 1, &mut got, &mut fl, ptr::null_mut(), 0),
            0
        );
        assert_eq!(&rb[..got as usize], b"abcd");

        // Non bloquant : WSAEWOULDBLOCK.
        let mut one = 1u32;
        assert_eq!(ioctlsocket(c, FIONBIO as i32, &mut one), 0);
        assert_eq!(recv(c, buf.as_mut_ptr(), 16, 0), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), WSAEWOULDBLOCK as i32);

        // Fermeture côté serveur → recv renvoie 0.
        assert_eq!(shutdown(a, 1), 0);
        let mut zero = 0u32;
        assert_eq!(ioctlsocket(c, FIONBIO as i32, &mut zero), 0);
        assert_eq!(recv(c, buf.as_mut_ptr(), 16, 0), 0);

        for s in [a, c, l] {
            assert_eq!(closesocket(s), 0);
        }
        assert_eq!(closesocket(l), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), WSAENOTSOCK as i32);
        assert_eq!(WSACleanup(), 0);
    }

    #[test]
    fn nonblocking_connect_reports_writable() {
        startup();
        let (l, port) = listener();
        let c = socket(AF_INET_W, 1, 0);
        let mut one = 1u32;
        assert_eq!(ioctlsocket(c, FIONBIO as i32, &mut one), 0);
        let sa = sa_v4([127, 0, 0, 1], port);
        let rc = connect(c, sa_ptr(&sa), 16);
        if rc != 0 {
            assert_eq!(WSAGetLastError(), WSAEWOULDBLOCK as i32);
        }
        let mut ws = FdSet {
            fd_count: 1,
            fd_array: [0; 64],
        };
        ws.fd_array[0] = c;
        let tv = TimeVal {
            tv_sec: 2,
            tv_usec: 0,
        };
        assert_eq!(select(0, ptr::null_mut(), &mut ws, ptr::null_mut(), &tv), 1);
        let mut err = -1i32;
        let mut len = 4;
        assert_eq!(
            getsockopt(c, SOL_SOCKET_W, 0x1007, (&raw mut err).cast(), &mut len),
            0
        );
        assert_eq!(err, 0);
        closesocket(c);
        closesocket(l);
        WSACleanup();
    }

    #[test]
    fn offline_blocks_remote_destinations() {
        if !offline_mode() {
            return;
        }
        startup();
        let s = socket(AF_INET_W, 1, 0);
        let sa = sa_v4([8, 8, 8, 8], 53);
        assert_eq!(connect(s, sa_ptr(&sa), 16), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), WSAENETUNREACH as i32);
        let u = socket(AF_INET_W, 2, 0);
        assert_eq!(
            sendto(u, b"x".as_ptr(), 1, 0, sa_ptr(&sa), 16),
            SOCKET_ERROR
        );
        assert_eq!(WSAGetLastError(), WSAENETUNREACH as i32);
        // bind sur une adresse non locale refusé.
        let b = sa_v4([10, 0, 0, 1], 0);
        assert_eq!(bind(u, sa_ptr(&b), 16), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), WSAEADDRNOTAVAIL as i32);
        // DNS non local.
        let mut res: *mut AddrInfo = ptr::null_mut();
        assert_eq!(
            getaddrinfo(
                c"example.com".as_ptr().cast(),
                ptr::null(),
                ptr::null(),
                &mut res
            ),
            WSAHOST_NOT_FOUND as i32
        );
        assert!(res.is_null());
        assert!(gethostbyname(c"example.com".as_ptr().cast()).is_null());
        assert_eq!(WSAGetLastError(), WSAHOST_NOT_FOUND as i32);
        closesocket(s);
        closesocket(u);
        WSACleanup();
    }

    #[test]
    fn udp_loopback_and_truncation() {
        startup();
        let r = socket(AF_INET_W, 2, IPPROTO_UDP_W);
        let any = sa_v4([0, 0, 0, 0], 0);
        assert_eq!(bind(r, sa_ptr(&any), 16), 0);
        let port = local_port(r);
        let s = socket(AF_INET_W, 2, 0);
        let dst = sa_v4([127, 0, 0, 1], port);
        assert_eq!(
            sendto(s, b"0123456789".as_ptr(), 10, 0, sa_ptr(&dst), 16),
            10
        );
        let mut from = sa_v4([0; 4], 0);
        let mut flen = 16;
        let mut buf = [0u8; 4];
        assert_eq!(
            recvfrom(r, buf.as_mut_ptr(), 4, 0, (&raw mut from).cast(), &mut flen),
            SOCKET_ERROR
        );
        assert_eq!(WSAGetLastError(), WSAEMSGSIZE as i32);
        assert_eq!(&buf, b"0123");
        assert_eq!(from.sin_addr, [127, 0, 0, 1]);
        let mut big = 0i32;
        let mut l = 4;
        assert_eq!(
            getsockopt(r, SOL_SOCKET_W, 0x2003, (&raw mut big).cast(), &mut l),
            0
        );
        assert_eq!(big, 65507);
        closesocket(r);
        closesocket(s);
        WSACleanup();
    }

    #[test]
    fn socket_option_conversions() {
        startup();
        let s = socket(AF_INET_W, 1, 0);
        let one = 1i32;
        assert_eq!(
            setsockopt(s, IPPROTO_TCP_W, 1, (&raw const one).cast(), 4),
            0
        );
        let mut v = 0i32;
        let mut len = 4;
        assert_eq!(
            getsockopt(s, IPPROTO_TCP_W, 1, (&raw mut v).cast(), &mut len),
            0
        );
        assert_eq!(v, 1);
        let ms = 1500u32;
        assert_eq!(
            setsockopt(s, SOL_SOCKET_W, 0x1006, (&raw const ms).cast(), 4),
            0
        );
        let mut got = 0u32;
        len = 4;
        assert_eq!(
            getsockopt(s, SOL_SOCKET_W, 0x1006, (&raw mut got).cast(), &mut len),
            0
        );
        assert_eq!(got, 1500);
        let lg = LingerW {
            l_onoff: 1,
            l_linger: 7,
        };
        assert_eq!(
            setsockopt(s, SOL_SOCKET_W, 0x80, (&raw const lg).cast(), 4),
            0
        );
        let mut lo = LingerW {
            l_onoff: 0,
            l_linger: 0,
        };
        len = 4;
        assert_eq!(
            getsockopt(s, SOL_SOCKET_W, 0x80, (&raw mut lo).cast(), &mut len),
            0
        );
        assert_eq!((lo.l_onoff, lo.l_linger), (1, 7));
        let mut ty = 0i32;
        len = 4;
        assert_eq!(
            getsockopt(s, SOL_SOCKET_W, 0x1008, (&raw mut ty).cast(), &mut len),
            0
        );
        assert_eq!(ty, 1);
        assert_eq!(
            setsockopt(s, SOL_SOCKET_W, -5, (&raw const one).cast(), 4),
            0
        );
        assert_eq!(
            setsockopt(s, SOL_SOCKET_W, 0x4242, (&raw const one).cast(), 4),
            SOCKET_ERROR
        );
        assert_eq!(WSAGetLastError(), WSAENOPROTOOPT as i32);
        len = 2;
        assert_eq!(
            getsockopt(s, SOL_SOCKET_W, 0x1008, (&raw mut ty).cast(), &mut len),
            SOCKET_ERROR
        );
        assert_eq!(WSAGetLastError(), WSAEFAULT as i32);
        closesocket(s);
        WSACleanup();
    }

    #[test]
    fn addrinfo_local_resolution() {
        startup();
        let hints = AddrInfo {
            ai_flags: AI_CANONNAME,
            ai_family: AF_INET_W,
            ai_socktype: 1,
            ai_protocol: IPPROTO_TCP_W,
            ai_addrlen: 0,
            ai_canonname: ptr::null_mut(),
            ai_addr: ptr::null_mut(),
            ai_next: ptr::null_mut(),
        };
        let mut res: *mut AddrInfo = ptr::null_mut();
        assert_eq!(
            getaddrinfo(
                c"localhost".as_ptr().cast(),
                c"http".as_ptr().cast(),
                &hints,
                &mut res
            ),
            0
        );
        assert!(!res.is_null());
        // SAFETY: résultat produit par getaddrinfo.
        let ai = unsafe { &*res };
        assert_eq!(ai.ai_family, AF_INET_W);
        assert_eq!(ai.ai_addrlen, 16);
        // SAFETY: sockaddr_in de 16 octets.
        let sin = unsafe { ptr::read_unaligned(ai.ai_addr.cast::<SockAddrIn>()) };
        assert_eq!(sin.sin_addr, [127, 0, 0, 1]);
        assert_eq!(u16::from_be(sin.sin_port), 80);
        // SAFETY: canonname C.
        assert_eq!(
            unsafe { cstr_arg(ai.ai_canonname.cast()) }.as_deref(),
            Some("localhost")
        );
        freeaddrinfo(res);

        // AF_UNSPEC : ::1 puis 127.0.0.1 (ordre Windows).
        let node = sb_common::wide::to_wide_null("localhost");
        let mut resw: *mut AddrInfo = ptr::null_mut();
        assert_eq!(
            GetAddrInfoW(node.as_ptr(), ptr::null(), ptr::null(), &mut resw),
            0
        );
        // SAFETY: liste produite par GetAddrInfoW.
        let (f1, f2) = unsafe { ((*resw).ai_family, (*(*resw).ai_next).ai_family) };
        assert_eq!((f1, f2), (AF_INET6_W, AF_INET_W));
        FreeAddrInfoW(resw);

        // Numérique + service inconnu.
        let mut r2: *mut AddrInfo = ptr::null_mut();
        assert_eq!(
            getaddrinfo(
                c"10.1.2.3".as_ptr().cast(),
                c"nope".as_ptr().cast(),
                ptr::null(),
                &mut r2
            ),
            WSATYPE_NOT_FOUND as i32
        );
        assert_eq!(
            getaddrinfo(
                c"10.1.2.3".as_ptr().cast(),
                c"8080".as_ptr().cast(),
                ptr::null(),
                &mut r2
            ),
            0
        );
        freeaddrinfo(r2);

        // getnameinfo
        let sa = sa_v4([127, 0, 0, 1], 443);
        let mut host = [0u8; 64];
        let mut serv = [0u8; 16];
        assert_eq!(
            getnameinfo(
                sa_ptr(&sa),
                16,
                host.as_mut_ptr(),
                64,
                serv.as_mut_ptr(),
                16,
                0
            ),
            0
        );
        // SAFETY: chaînes terminées.
        unsafe {
            assert_eq!(cstr_arg(host.as_ptr()).as_deref(), Some("localhost"));
            assert_eq!(cstr_arg(serv.as_ptr()).as_deref(), Some("https"));
        }

        // gethostbyname / gethostname
        let he = gethostbyname(c"localhost".as_ptr().cast());
        assert!(!he.is_null());
        // SAFETY: hostent thread-local valide.
        unsafe {
            assert_eq!((*he).h_length, 4);
            let a0 = *(*he).h_addr_list;
            assert_eq!(
                std::slice::from_raw_parts(a0.cast::<u8>(), 4),
                &[127, 0, 0, 1]
            );
            assert!((*(*he).h_addr_list.add(1)).is_null());
        }
        let mut hn = [0u8; 64];
        assert_eq!(gethostname(hn.as_mut_ptr(), 64), 0);
        assert_eq!(gethostname(hn.as_mut_ptr(), 2), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), WSAEFAULT as i32);
        WSACleanup();
    }

    #[test]
    fn text_address_conversions() {
        assert_eq!(
            inet_addr(c"127.0.0.1".as_ptr().cast()),
            u32::from_ne_bytes([127, 0, 0, 1])
        );
        assert_eq!(
            inet_addr(c"0x7f.1".as_ptr().cast()),
            u32::from_ne_bytes([127, 0, 0, 1])
        );
        assert_eq!(inet_addr(c"1.2.3.256".as_ptr().cast()), u32::MAX);
        assert_eq!(inet_addr(c"abc".as_ptr().cast()), u32::MAX);
        let p = inet_ntoa(u32::from_ne_bytes([192, 168, 1, 2]));
        // SAFETY: tampon thread-local terminé.
        assert_eq!(unsafe { cstr_arg(p) }.as_deref(), Some("192.168.1.2"));
        let mut a6 = [0u8; 16];
        assert_eq!(
            inet_pton(AF_INET6_W, c"fe80::1".as_ptr().cast(), a6.as_mut_ptr()),
            1
        );
        assert_eq!(
            inet_pton(AF_INET_W, c"1.2.3".as_ptr().cast(), a6.as_mut_ptr()),
            0
        );
        assert_eq!(
            inet_pton(99, c"1.2.3.4".as_ptr().cast(), a6.as_mut_ptr()),
            -1
        );
        assert_eq!(WSAGetLastError(), WSAEAFNOSUPPORT as i32);
        let mut out = [0u8; 46];
        let r = inet_ntop(AF_INET6_W, a6.as_ptr(), out.as_mut_ptr(), 46);
        assert!(!r.is_null());
        // SAFETY: chaîne terminée.
        assert_eq!(unsafe { cstr_arg(r) }.as_deref(), Some("fe80::1"));
        assert!(inet_ntop(AF_INET6_W, a6.as_ptr(), out.as_mut_ptr(), 3).is_null());
        assert_eq!(WSAGetLastError(), ERROR_INVALID_PARAMETER as i32);
        let mut wout = [0u16; 16];
        let ip4 = [10u8, 0, 0, 7];
        assert!(!InetNtopW(AF_INET_W, ip4.as_ptr(), wout.as_mut_ptr(), 16).is_null());
        assert_eq!(htons(0x1234), 0x3412);
        assert_eq!(ntohl(htonl(0xDEAD_BEEF)), 0xDEAD_BEEF);
        // getprotobyname / getservbyname
        let pe = getprotobyname(c"udp".as_ptr().cast());
        // SAFETY: protoent thread-local.
        assert_eq!(unsafe { (*pe).p_proto }, 17);
        let se = getservbyname(c"https".as_ptr().cast(), c"tcp".as_ptr().cast());
        // SAFETY: servent thread-local.
        assert_eq!(unsafe { u16::from_be((*se).s_port as u16) }, 443);
        assert!(getservbyname(c"nope".as_ptr().cast(), ptr::null()).is_null());
    }

    #[test]
    fn requires_startup_and_validates_args() {
        // Sans WSAStartup actif dans ce test, d'autres tests parallèles peuvent l'avoir appelé :
        // on vérifie uniquement les erreurs indépendantes de l'état global.
        // SAFETY: zéro valide pour WSADATA.
        let mut d: WsaData = unsafe { std::mem::zeroed() };
        assert_eq!(WSAStartup(0x0202, &mut d), 0);
        assert_eq!(socket(99, 1, 0), INVALID_SOCKET);
        assert_eq!(WSAGetLastError(), WSAEAFNOSUPPORT as i32);
        assert_eq!(socket(AF_INET_W, 3, 0), INVALID_SOCKET);
        assert_eq!(socket(AF_INET_W, 1, IPPROTO_UDP_W), INVALID_SOCKET);
        assert_eq!(WSAGetLastError(), WSAEPROTOTYPE as i32);
        assert_eq!(bind(0x7777, ptr::null(), 0), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), WSAENOTSOCK as i32);
        let s = socket(AF_INET_W, 1, 0);
        assert_eq!(listen(s, 5), SOCKET_ERROR);
        assert_eq!(WSAGetLastError(), WSAEINVAL as i32);
        let mut empty = FdSet {
            fd_count: 0,
            fd_array: [0; 64],
        };
        assert_eq!(
            select(0, &mut empty, ptr::null_mut(), ptr::null_mut(), ptr::null()),
            SOCKET_ERROR
        );
        assert_eq!(WSAGetLastError(), WSAEINVAL as i32);
        let mut pf = [WsaPollFd {
            fd: s,
            events: POLLWRNORM_W,
            revents: 0,
        }];
        assert!(WSAPoll(pf.as_mut_ptr(), 1, 0) >= 0);
        WSASetLastError(1234);
        assert_eq!(WSAGetLastError(), 1234);
        closesocket(s);
        WSACleanup();
    }
}
