//! Réseau HLE : `ws2_32.dll` (Winsock 2), `winhttp.dll` et `libHttpClient(.GDK).dll`.
//!
//! Politique **offline strict** (`SB_OFFLINE_MODE=1`, défaut) : aucune connexion sortante.
//! Seules les opérations locales (loopback `127.0.0.0/8` / `::1`, résolution de `localhost`)
//! fonctionnent ; toute autre destination échoue avec le code d'erreur Windows documenté
//! (`WSAENETUNREACH`, `WSAHOST_NOT_FOUND`, `ERROR_WINHTTP_NAME_NOT_RESOLVED`, `E_HC_NO_NETWORK`…).
//!
//! Sous-modules :
//! - [`ws2`] : sockets BSD Winsock mappés sur les fd hôtes (par nom et par ordinal),
//! - [`winhttp`] : arborescence de handles session/connect/request + client HTTP/1.1 en clair,
//! - [`hc`] : API `libHttpClient` (GDK) au-dessus du même moteur HTTP,
//! - [`http`] : moteur HTTP/1.1 minimal partagé + parseur d'URL + politique réseau.

#![allow(non_snake_case, unused_variables)]

mod hc;
mod http;
mod winhttp;
mod ws2;

use std::cell::Cell;
use std::collections::HashMap;
use std::net::IpAddr;

use sb_loader::module_manager;
use sb_runtime::lifecycle::{LifecycleManager, NetworkMode};

const CHANNEL: &str = "net";

/// Nom d'hôte virtuel exposé à l'invité (`gethostname`) : n'expose jamais le nom de la machine hôte.
pub(crate) const VIRTUAL_HOSTNAME: &str = "SCARLETBRIDGE";

/// Exports par nom, indexés par *stem* de DLL en minuscules sans `.dll` (ex. `"ws2_32"`).
/// Prioritaires sur les familles historiques ; un stem listé ici est considéré HLE.
pub fn by_dll() -> HashMap<&'static str, HashMap<&'static str, usize>> {
    let mut m = HashMap::new();
    m.insert("ws2_32", ws2::exports());
    m.insert("winhttp", winhttp::exports());
    m.insert("libhttpclient.gdk", hc::exports());
    m.insert("libhttpclient", hc::exports());
    m
}

/// Exports par ordinal, indexés par *stem* de DLL.
pub fn ordinals_by_dll() -> HashMap<&'static str, HashMap<u16, usize>> {
    let mut m = HashMap::new();
    m.insert("ws2_32", ws2::ordinals());
    m
}

/// `true` si le mode offline strict est actif (défaut ; `SB_OFFLINE_MODE=0|false` pour le lever).
pub(crate) fn offline_mode() -> bool {
    LifecycleManager::from_env().snapshot().network_mode == NetworkMode::StrictOffline
}

/// Adresse de bouclage locale (`127.0.0.0/8`, `::1`, `::ffff:127.x.y.z`).
pub(crate) fn is_loopback_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// Noms résolus localement sans DNS (y compris en mode offline).
pub(crate) fn is_local_hostname(name: &str) -> bool {
    let n = name.trim_end_matches('.');
    n.is_empty()
        || n.eq_ignore_ascii_case("localhost")
        || n.eq_ignore_ascii_case(VIRTUAL_HOSTNAME)
        || n.to_ascii_lowercase().ends_with(".localhost")
}

thread_local! {
    static LAST_ERROR_FALLBACK: Cell<u32> = const { Cell::new(0) };
}

/// `SetLastError` / `WSASetLastError` (TEB invité si présent + repli thread-local pour les threads hôtes).
pub(crate) fn set_error(code: u32) {
    LAST_ERROR_FALLBACK.with(|c| c.set(code));
    sb_loader::set_last_error(code);
}

/// `GetLastError` / `WSAGetLastError`.
pub(crate) fn get_error() -> u32 {
    if sb_loader::teb::current_teb_ptr().is_null() {
        LAST_ERROR_FALLBACK.with(Cell::get)
    } else {
        sb_loader::last_error()
    }
}

/// Lance un thread de travail capable d'appeler du code invité (TEB + TLS initialisés).
pub(crate) fn spawn_guest_worker<F>(name: &str, f: F)
where
    F: FnOnce() + Send + 'static,
{
    let res = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let _ = sb_loader::init_thread_teb(0);
            module_manager().attach_current_thread_tls();
            f();
        });
    if let Err(e) = res {
        sb_common::sb_err!(CHANNEL, "impossible de lancer le thread {name}: {e}");
    }
}

/// Lit une chaîne C ANSI/UTF-8 terminée par un nul.
///
/// # Safety
/// `p` doit être nul ou pointer vers une chaîne terminée par `0`.
pub(crate) unsafe fn cstr_arg(p: *const u8) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: garanti par l'appelant (chaîne C terminée).
    let s = unsafe { std::ffi::CStr::from_ptr(p.cast()) };
    Some(s.to_string_lossy().into_owned())
}

/// Lit une chaîne UTF-16 terminée par un nul.
///
/// # Safety
/// `p` doit être nul ou pointer vers une chaîne UTF-16 terminée par `0`.
pub(crate) unsafe fn wstr_arg(p: *const u16) -> Option<String> {
    // SAFETY: garanti par l'appelant.
    unsafe { sb_common::wide::from_pcwstr(p) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_expose_all_stems() {
        let t = by_dll();
        for stem in ["ws2_32", "winhttp", "libhttpclient.gdk", "libhttpclient"] {
            assert!(t.get(stem).is_some_and(|m| !m.is_empty()), "{stem}");
        }
        assert!(ordinals_by_dll()["ws2_32"].contains_key(&23));
    }

    #[test]
    fn loopback_and_local_names() {
        assert!(is_loopback_ip(&"127.0.0.1".parse().expect("ip")));
        assert!(is_loopback_ip(&"127.8.0.1".parse().expect("ip")));
        assert!(is_loopback_ip(&"::1".parse().expect("ip")));
        assert!(is_loopback_ip(&"::ffff:127.0.0.1".parse().expect("ip")));
        assert!(!is_loopback_ip(&"8.8.8.8".parse().expect("ip")));
        assert!(is_local_hostname("LocalHost."));
        assert!(!is_local_hostname("example.com"));
    }

    #[test]
    fn offline_is_default() {
        if std::env::var("SB_OFFLINE_MODE").is_err() {
            assert!(offline_mode());
        }
    }
}
