//! Moteur HTTP/1.1 minimal en clair (partagé par WinHTTP et libHttpClient),
//! parseur d'URL compatible `WinHttpCrackUrl` et politique réseau offline.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::ops::Range;
use std::time::Duration;

use super::{is_local_hostname, is_loopback_ip, offline_mode};

pub(crate) const INTERNET_SCHEME_HTTP: u32 = 1;
pub(crate) const INTERNET_SCHEME_HTTPS: u32 = 2;

pub(crate) const ERROR_WINHTTP_TIMEOUT: u32 = 12002;
pub(crate) const ERROR_WINHTTP_INVALID_URL: u32 = 12005;
pub(crate) const ERROR_WINHTTP_UNRECOGNIZED_SCHEME: u32 = 12006;
pub(crate) const ERROR_WINHTTP_NAME_NOT_RESOLVED: u32 = 12007;
pub(crate) const ERROR_WINHTTP_CANNOT_CONNECT: u32 = 12029;
pub(crate) const ERROR_WINHTTP_CONNECTION_ERROR: u32 = 12030;
pub(crate) const ERROR_WINHTTP_INVALID_SERVER_RESPONSE: u32 = 12152;
pub(crate) const ERROR_WINHTTP_SECURE_FAILURE: u32 = 12175;

/// Taille maximale d'un corps de réponse conservé en mémoire.
const MAX_BODY: usize = 256 * 1024 * 1024;

/// Composants d'une URL (plages en unités UTF-16 dans la chaîne source).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedUrl {
    pub scheme: Range<usize>,
    pub scheme_kind: u32,
    pub user: Option<Range<usize>>,
    pub password: Option<Range<usize>>,
    pub host: Range<usize>,
    pub port: u16,
    pub path: Range<usize>,
    pub extra: Range<usize>,
}

fn find(s: &[u16], from: usize, pred: impl Fn(u16) -> bool) -> Option<usize> {
    s[from..].iter().position(|&c| pred(c)).map(|p| p + from)
}

/// Parse une URL `http(s)://[user[:pass]@]host[:port][/path][?extra|#extra]`.
/// Erreurs : `ERROR_WINHTTP_UNRECOGNIZED_SCHEME` / `ERROR_WINHTTP_INVALID_URL`.
pub(crate) fn parse_url(s: &[u16]) -> Result<ParsedUrl, u32> {
    let colon = find(s, 0, |c| c == u16::from(b':')).ok_or(ERROR_WINHTTP_UNRECOGNIZED_SCHEME)?;
    let scheme_str = String::from_utf16_lossy(&s[..colon]).to_ascii_lowercase();
    let scheme_kind = match scheme_str.as_str() {
        "http" => INTERNET_SCHEME_HTTP,
        "https" => INTERNET_SCHEME_HTTPS,
        _ => return Err(ERROR_WINHTTP_UNRECOGNIZED_SCHEME),
    };
    let slash = u16::from(b'/');
    if s.len() < colon + 3 || s[colon + 1] != slash || s[colon + 2] != slash {
        return Err(ERROR_WINHTTP_INVALID_URL);
    }
    let auth_start = colon + 3;
    let auth_end = find(s, auth_start, |c| {
        c == slash || c == u16::from(b'?') || c == u16::from(b'#')
    })
    .unwrap_or(s.len());

    // userinfo
    let mut host_start = auth_start;
    let mut user = None;
    let mut password = None;
    if let Some(at) = s[auth_start..auth_end]
        .iter()
        .rposition(|&c| c == u16::from(b'@'))
        .map(|p| p + auth_start)
    {
        match find(&s[..at], auth_start, |c| c == u16::from(b':')) {
            Some(pc) => {
                user = Some(auth_start..pc);
                password = Some(pc + 1..at);
            }
            None => user = Some(auth_start..at),
        }
        host_start = at + 1;
    }

    // host[:port]
    let (host, port_range) = if s.get(host_start) == Some(&u16::from(b'[')) {
        let close = find(&s[..auth_end], host_start, |c| c == u16::from(b']'))
            .ok_or(ERROR_WINHTTP_INVALID_URL)?;
        let after = close + 1;
        if after < auth_end {
            if s[after] != u16::from(b':') {
                return Err(ERROR_WINHTTP_INVALID_URL);
            }
            (host_start..after, Some(after + 1..auth_end))
        } else {
            (host_start..after, None)
        }
    } else {
        match s[host_start..auth_end]
            .iter()
            .rposition(|&c| c == u16::from(b':'))
            .map(|p| p + host_start)
        {
            Some(pc) => (host_start..pc, Some(pc + 1..auth_end)),
            None => (host_start..auth_end, None),
        }
    };
    if host.is_empty() {
        return Err(ERROR_WINHTTP_INVALID_URL);
    }
    let default_port = if scheme_kind == INTERNET_SCHEME_HTTPS {
        443
    } else {
        80
    };
    let port = match port_range {
        Some(r) if !r.is_empty() => {
            let txt = String::from_utf16_lossy(&s[r]);
            if !txt.bytes().all(|b| b.is_ascii_digit()) {
                return Err(ERROR_WINHTTP_INVALID_URL);
            }
            match txt.parse::<u32>() {
                Ok(p) if (1..=65535).contains(&p) => p as u16,
                _ => return Err(ERROR_WINHTTP_INVALID_URL),
            }
        }
        _ => default_port,
    };

    let extra_start = find(s, auth_end, |c| {
        c == u16::from(b'?') || c == u16::from(b'#')
    })
    .unwrap_or(s.len());
    Ok(ParsedUrl {
        scheme: 0..colon,
        scheme_kind,
        user,
        password,
        host,
        port,
        path: auth_end..extra_start,
        extra: extra_start..s.len(),
    })
}

/// URL décomposée en chaînes (usage interne HTTP).
#[derive(Debug, Clone)]
pub(crate) struct UrlParts {
    pub secure: bool,
    pub host: String,
    pub port: u16,
    /// Chemin + requête (`/a/b?x=1`), jamais vide.
    pub path_and_query: String,
}

pub(crate) fn split_url(url: &str) -> Result<UrlParts, u32> {
    let w: Vec<u16> = url.encode_utf16().collect();
    let p = parse_url(&w)?;
    let path = String::from_utf16_lossy(&w[p.path.clone()]);
    let extra = String::from_utf16_lossy(&w[p.extra.clone()]);
    let extra = extra.split('#').next().unwrap_or("").to_string();
    let mut pq = if path.is_empty() {
        "/".to_string()
    } else {
        path
    };
    pq.push_str(&extra);
    Ok(UrlParts {
        secure: p.scheme_kind == INTERNET_SCHEME_HTTPS,
        host: String::from_utf16_lossy(&w[p.host]),
        port: p.port,
        path_and_query: pq,
    })
}

/// Résout `host:port` selon la politique réseau.
/// Offline : seules les adresses loopback et les noms locaux sont acceptés.
pub(crate) fn resolve_target(host: &str, port: u16) -> Result<Vec<SocketAddr>, u32> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        if offline_mode() && !is_loopback_ip(&ip) {
            return Err(ERROR_WINHTTP_CANNOT_CONNECT);
        }
        if ip.is_unspecified() {
            return Err(ERROR_WINHTTP_CANNOT_CONNECT);
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    if is_local_hostname(bare) {
        return Ok(vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)]);
    }
    if offline_mode() {
        return Err(ERROR_WINHTTP_NAME_NOT_RESOLVED);
    }
    match (bare, port).to_socket_addrs() {
        Ok(it) => {
            let v: Vec<SocketAddr> = it.collect();
            if v.is_empty() {
                Err(ERROR_WINHTTP_NAME_NOT_RESOLVED)
            } else {
                Ok(v)
            }
        }
        Err(_) => Err(ERROR_WINHTTP_NAME_NOT_RESOLVED),
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Timeouts {
    pub connect: Option<Duration>,
    pub send: Option<Duration>,
    pub receive: Option<Duration>,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Some(Duration::from_secs(60)),
            send: Some(Duration::from_secs(30)),
            receive: Some(Duration::from_secs(30)),
        }
    }
}

fn io_err(e: &std::io::Error) -> u32 {
    use std::io::ErrorKind as K;
    match e.kind() {
        K::TimedOut | K::WouldBlock => ERROR_WINHTTP_TIMEOUT,
        K::ConnectionRefused => ERROR_WINHTTP_CANNOT_CONNECT,
        _ => ERROR_WINHTTP_CONNECTION_ERROR,
    }
}

/// Ouvre une connexion TCP vers la cible (après résolution soumise à la politique offline).
pub(crate) fn connect(host: &str, port: u16, secure: bool, t: &Timeouts) -> Result<TcpStream, u32> {
    let addrs = resolve_target(host, port)?;
    if secure {
        // Pas de pile TLS : HTTPS est refusé proprement.
        return Err(ERROR_WINHTTP_SECURE_FAILURE);
    }
    let mut last = ERROR_WINHTTP_CANNOT_CONNECT;
    for a in addrs {
        let res = match t.connect {
            Some(d) if !d.is_zero() => TcpStream::connect_timeout(&a, d),
            _ => TcpStream::connect(a),
        };
        match res {
            Ok(s) => {
                let _ = s.set_write_timeout(t.send.filter(|d| !d.is_zero()));
                let _ = s.set_read_timeout(t.receive.filter(|d| !d.is_zero()));
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => {
                last = if e.kind() == std::io::ErrorKind::TimedOut {
                    ERROR_WINHTTP_TIMEOUT
                } else {
                    ERROR_WINHTTP_CANNOT_CONNECT
                };
            }
        }
    }
    Err(last)
}

/// Écrit la ligne de requête et les en-têtes.
pub(crate) fn write_head(
    stream: &mut TcpStream,
    method: &str,
    target: &str,
    version: &str,
    headers: &[(String, String)],
) -> Result<(), u32> {
    let mut head = format!("{method} {target} {version}\r\n");
    for (k, v) in headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).map_err(|e| io_err(&e))
}

pub(crate) fn write_body(stream: &mut TcpStream, data: &[u8]) -> Result<(), u32> {
    stream.write_all(data).map_err(|e| io_err(&e))
}

#[derive(Debug, Clone, Default)]
pub(crate) struct HttpResponse {
    pub version: String,
    pub status: u32,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn read_line(r: &mut impl BufRead) -> Result<String, u32> {
    let mut buf = Vec::new();
    let n = r.read_until(b'\n', &mut buf).map_err(|e| io_err(&e))?;
    if n == 0 {
        return Err(ERROR_WINHTTP_CONNECTION_ERROR);
    }
    while buf.last().is_some_and(|&b| b == b'\n' || b == b'\r') {
        buf.pop();
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Lit une réponse complète (statut, en-têtes, corps : `Content-Length`, `chunked` ou fin de flux).
pub(crate) fn read_response(stream: &mut TcpStream, method: &str) -> Result<HttpResponse, u32> {
    let mut r = BufReader::new(stream);
    loop {
        let status_line = read_line(&mut r)?;
        let mut parts = status_line.splitn(3, ' ');
        let version = parts.next().unwrap_or_default().to_string();
        if !version.starts_with("HTTP/") {
            return Err(ERROR_WINHTTP_INVALID_SERVER_RESPONSE);
        }
        let status: u32 = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or(ERROR_WINHTTP_INVALID_SERVER_RESPONSE)?;
        let reason = parts.next().unwrap_or_default().to_string();
        let mut headers = Vec::new();
        loop {
            let line = read_line(&mut r)?;
            if line.is_empty() {
                break;
            }
            let Some((k, v)) = line.split_once(':') else {
                return Err(ERROR_WINHTTP_INVALID_SERVER_RESPONSE);
            };
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
        let mut resp = HttpResponse {
            version,
            status,
            reason,
            headers,
            body: Vec::new(),
        };
        if (100..200).contains(&status) {
            continue; // 100-continue & co : réponse intermédiaire
        }
        let no_body = method.eq_ignore_ascii_case("HEAD") || status == 204 || status == 304;
        if !no_body {
            let chunked = resp
                .header("Transfer-Encoding")
                .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
            if chunked {
                loop {
                    let size_line = read_line(&mut r)?;
                    let hex = size_line.split(';').next().unwrap_or("").trim();
                    let size = usize::from_str_radix(hex, 16)
                        .map_err(|_| ERROR_WINHTTP_INVALID_SERVER_RESPONSE)?;
                    if size == 0 {
                        // trailers
                        while !read_line(&mut r)?.is_empty() {}
                        break;
                    }
                    if resp.body.len() + size > MAX_BODY {
                        return Err(ERROR_WINHTTP_INVALID_SERVER_RESPONSE);
                    }
                    let start = resp.body.len();
                    resp.body.resize(start + size, 0);
                    r.read_exact(&mut resp.body[start..])
                        .map_err(|e| io_err(&e))?;
                    let _ = read_line(&mut r)?;
                }
            } else if let Some(cl) = resp.header("Content-Length") {
                let len: usize = cl
                    .trim()
                    .parse()
                    .map_err(|_| ERROR_WINHTTP_INVALID_SERVER_RESPONSE)?;
                if len > MAX_BODY {
                    return Err(ERROR_WINHTTP_INVALID_SERVER_RESPONSE);
                }
                resp.body.resize(len, 0);
                r.read_exact(&mut resp.body).map_err(|e| io_err(&e))?;
            } else {
                (&mut r)
                    .take(MAX_BODY as u64)
                    .read_to_end(&mut resp.body)
                    .map_err(|e| io_err(&e))?;
            }
        }
        return Ok(resp);
    }
}

/// Requête complète (utilisée par libHttpClient).
pub(crate) fn perform(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    t: &Timeouts,
) -> Result<HttpResponse, u32> {
    let u = split_url(url)?;
    let mut stream = connect(&u.host, u.port, u.secure, t)?;
    let mut hdrs: Vec<(String, String)> = Vec::new();
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("Host")) {
        hdrs.push(("Host".into(), host_header(&u.host, u.port, u.secure)));
    }
    hdrs.extend(headers.iter().cloned());
    if !body.is_empty() || !method.eq_ignore_ascii_case("GET") {
        hdrs.retain(|(k, _)| !k.eq_ignore_ascii_case("Content-Length"));
        hdrs.push(("Content-Length".into(), body.len().to_string()));
    }
    if !hdrs
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("Connection"))
    {
        hdrs.push(("Connection".into(), "close".into()));
    }
    write_head(&mut stream, method, &u.path_and_query, "HTTP/1.1", &hdrs)?;
    write_body(&mut stream, body)?;
    read_response(&mut stream, method)
}

pub(crate) fn host_header(host: &str, port: u16, secure: bool) -> String {
    let default = if secure { 443 } else { 80 };
    if port == default {
        host.to_string()
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn parse_full_url() {
        let s = w("https://u:p@example.com:8443/a/b?x=1#f");
        let p = parse_url(&s).expect("url");
        assert_eq!(p.scheme_kind, INTERNET_SCHEME_HTTPS);
        assert_eq!(String::from_utf16_lossy(&s[p.host.clone()]), "example.com");
        assert_eq!(p.port, 8443);
        assert_eq!(String::from_utf16_lossy(&s[p.path.clone()]), "/a/b");
        assert_eq!(String::from_utf16_lossy(&s[p.extra.clone()]), "?x=1#f");
        assert_eq!(
            String::from_utf16_lossy(&s[p.user.clone().expect("u")]),
            "u"
        );
        assert_eq!(
            String::from_utf16_lossy(&s[p.password.clone().expect("p")]),
            "p"
        );
    }

    #[test]
    fn parse_errors_and_ipv6() {
        assert_eq!(
            parse_url(&w("ftp://x/")),
            Err(ERROR_WINHTTP_UNRECOGNIZED_SCHEME)
        );
        assert_eq!(
            parse_url(&w("nothing")),
            Err(ERROR_WINHTTP_UNRECOGNIZED_SCHEME)
        );
        assert_eq!(parse_url(&w("http:/x")), Err(ERROR_WINHTTP_INVALID_URL));
        assert_eq!(
            parse_url(&w("http://h:99999/")),
            Err(ERROR_WINHTTP_INVALID_URL)
        );
        assert_eq!(parse_url(&w("http:///")), Err(ERROR_WINHTTP_INVALID_URL));
        let s = w("http://[::1]:8080");
        let p = parse_url(&s).expect("v6");
        assert_eq!(String::from_utf16_lossy(&s[p.host.clone()]), "[::1]");
        assert_eq!(p.port, 8080);
        assert!(p.path.is_empty());
    }

    #[test]
    fn offline_policy() {
        if !offline_mode() {
            return;
        }
        assert_eq!(
            resolve_target("example.com", 80),
            Err(ERROR_WINHTTP_NAME_NOT_RESOLVED)
        );
        assert_eq!(
            resolve_target("8.8.8.8", 80),
            Err(ERROR_WINHTTP_CANNOT_CONNECT)
        );
        assert!(resolve_target("localhost", 80).is_ok());
        assert!(resolve_target("[::1]", 80).is_ok());
    }

    #[test]
    fn loopback_roundtrip_chunked() {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = l.local_addr().expect("addr").port();
        let th = std::thread::spawn(move || {
            let (mut s, _) = l.accept().expect("accept");
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nX-A: 1\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
            );
        });
        let r = perform(
            "GET",
            &format!("http://127.0.0.1:{port}/x"),
            &[],
            &[],
            &Timeouts::default(),
        )
        .expect("perform");
        th.join().expect("join");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"abcde");
        assert_eq!(r.header("x-a"), Some("1"));
    }
}
