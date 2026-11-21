//! Logger léger, thread-safe et sûr vis-à-vis de `DllMain` pour ScarletBridge.
//!
//! Configuration par variables d'environnement :
//! - `SB_LOG` : niveau global ou par canal (ex: `debug`, `info,xmem=trace,vfs=debug`).
//! - `SB_LOG_FILE` : chemin optionnel de fichier journal en complément de `stderr`.
//! - `SB_STUB_ONCE` : `1` (défaut) pour ne logguer chaque stub qu'une fois ; `0` pour tout logguer.
//! - `SB_STUB_BACKTRACE` : `1` pour inclure la stack trace lors du premier appel d'un stub.

use std::backtrace::Backtrace;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::OnceLock;
use std::time::Instant;

use parking_lot::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl LogLevel {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "err" | "error" => Some(Self::Error),
            "warn" | "warning" | "fixme" | "stub" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }

    pub const fn tag(self) -> &'static str {
        match self {
            Self::Error => "ERR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogFilter {
    default_level: LogLevel,
    channels: HashMap<String, LogLevel>,
}

impl LogFilter {
    pub fn parse(spec: &str) -> Self {
        let mut default_level = LogLevel::Info;
        let mut channels = HashMap::new();

        for part in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some((ch, lvl_str)) = part.split_once('=') {
                if let Some(lvl) = LogLevel::parse(lvl_str) {
                    channels.insert(ch.trim().to_ascii_lowercase(), lvl);
                }
            } else if let Some(lvl) = LogLevel::parse(part) {
                default_level = lvl;
            }
        }

        Self {
            default_level,
            channels,
        }
    }

    pub fn enabled(&self, channel: &str, level: LogLevel) -> bool {
        let max_level = self
            .channels
            .get(&channel.to_ascii_lowercase())
            .copied()
            .unwrap_or(self.default_level);
        level <= max_level
    }
}

struct LoggerState {
    filter: LogFilter,
    start: Instant,
    stub_once: bool,
    stub_backtrace: bool,
    seen_stubs: Mutex<HashSet<String>>,
    file: Mutex<Option<File>>,
}

static LOGGER: OnceLock<LoggerState> = OnceLock::new();

fn state() -> &'static LoggerState {
    LOGGER.get_or_init(|| {
        let spec = std::env::var("SB_LOG").unwrap_or_else(|_| "info".to_string());
        let filter = LogFilter::parse(&spec);
        let stub_once = std::env::var("SB_STUB_ONCE")
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(true);
        let stub_backtrace = std::env::var("SB_STUB_BACKTRACE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let file = std::env::var("SB_LOG_FILE")
            .ok()
            .and_then(|path| OpenOptions::new().create(true).append(true).open(path).ok());

        LoggerState {
            filter,
            start: Instant::now(),
            stub_once,
            stub_backtrace,
            seen_stubs: Mutex::new(HashSet::new()),
            file: Mutex::new(file),
        }
    })
}

/// Initialise explicitement le logger (idempotent).
pub fn init() {
    let _ = state();
}

/// Émet une ligne de journal formatée si le niveau/canal est actif.
pub fn log_raw(
    channel: &str,
    level: LogLevel,
    kind_override: Option<&str>,
    args: fmt::Arguments<'_>,
) {
    let st = state();
    if !st.filter.enabled(channel, level) {
        return;
    }

    let elapsed = st.start.elapsed();
    let tag = kind_override.unwrap_or_else(|| level.tag());
    let line = format!(
        "[{:04}.{:03}][SB][{}][{}] {}\n",
        elapsed.as_secs(),
        elapsed.subsec_millis(),
        tag,
        channel,
        args
    );

    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(line.as_bytes());
    drop(stderr);

    if let Some(mut guard) = st.file.try_lock()
        && let Some(ref mut f) = *guard
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Enregistre l'appel à une fonction stubbée avec déduplication optionnelle et stack trace.
pub fn log_stub(channel: &str, fn_name: &str, args: fmt::Arguments<'_>) {
    let st = state();
    let key = format!("{channel}::{fn_name}");
    if st.stub_once {
        let mut seen = st.seen_stubs.lock();
        if !seen.insert(key) {
            return;
        }
    }

    if st.stub_backtrace {
        let bt = Backtrace::force_capture();
        log_raw(
            channel,
            LogLevel::Warn,
            Some("STUB"),
            format_args!("{fn_name}({args})\n{bt}"),
        );
    } else {
        log_raw(
            channel,
            LogLevel::Warn,
            Some("STUB"),
            format_args!("{fn_name}({args})"),
        );
    }
}

#[macro_export]
macro_rules! sb_err {
    ($ch:expr, $($arg:tt)+) => {
        $crate::log::log_raw($ch, $crate::log::LogLevel::Error, None, format_args!($($arg)+))
    };
}

#[macro_export]
macro_rules! sb_warn {
    ($ch:expr, $($arg:tt)+) => {
        $crate::log::log_raw($ch, $crate::log::LogLevel::Warn, None, format_args!($($arg)+))
    };
}

#[macro_export]
macro_rules! sb_fixme {
    ($ch:expr, $($arg:tt)+) => {
        $crate::log::log_raw($ch, $crate::log::LogLevel::Warn, Some("FIXME"), format_args!($($arg)+))
    };
}

#[macro_export]
macro_rules! sb_info {
    ($ch:expr, $($arg:tt)+) => {
        $crate::log::log_raw($ch, $crate::log::LogLevel::Info, None, format_args!($($arg)+))
    };
}

#[macro_export]
macro_rules! sb_debug {
    ($ch:expr, $($arg:tt)+) => {
        $crate::log::log_raw($ch, $crate::log::LogLevel::Debug, None, format_args!($($arg)+))
    };
}

#[macro_export]
macro_rules! sb_trace {
    ($ch:expr, $($arg:tt)+) => {
        $crate::log::log_raw($ch, $crate::log::LogLevel::Trace, None, format_args!($($arg)+))
    };
}

#[macro_export]
macro_rules! sb_stub {
    ($ch:expr, $fn_name:expr, $($arg:tt)+) => {
        $crate::log::log_stub($ch, $fn_name, format_args!($($arg)+))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_parses_global_and_channel_overrides() {
        let filter = LogFilter::parse("warn,xmem=trace,vfs=debug");
        assert!(filter.enabled("other", LogLevel::Warn));
        assert!(!filter.enabled("other", LogLevel::Info));
        assert!(filter.enabled("xmem", LogLevel::Trace));
        assert!(filter.enabled("XMEM", LogLevel::Debug));
        assert!(filter.enabled("vfs", LogLevel::Debug));
        assert!(!filter.enabled("vfs", LogLevel::Trace));
    }
}
