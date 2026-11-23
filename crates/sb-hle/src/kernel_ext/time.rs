//! Temps (`SYSTEMTIME` ↔ `FILETIME`, heure locale via le fuseau hôte) et paramètres régionaux
//! (`en-US` par défaut : `GetDateFormatEx`, `GetTimeFormatEx`, `LocaleNameToLCID`, ...).

use core::mem::size_of;
use std::time::{SystemTime as HostTime, UNIX_EPOCH};

use sb_common::abi::{WIN_FALSE, WIN_TRUE, WinBool};
use sb_common::ffi_guard;
use sb_common::wide::from_pcwstr;
use sb_loader::set_last_error;

#[cfg(test)]
use super::ERROR_INSUFFICIENT_BUFFER;
use super::{CHANNEL, ERROR_INVALID_FLAGS, ERROR_INVALID_PARAMETER, return_wide, write_out};

/// `SYSTEMTIME` (16 octets).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SystemTime {
    pub year: u16,
    pub month: u16,
    pub day_of_week: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub milliseconds: u16,
}

const _: () = assert!(size_of::<SystemTime>() == 16);

/// Écart FILETIME (1601) ↔ époque Unix, en unités de 100 ns.
const UNIX_TO_FILETIME: u64 = 116_444_736_000_000_000;
const TICKS_PER_MS: u64 = 10_000;
const TICKS_PER_DAY: u64 = 864_000_000_000;
/// Nombre de jours entre le 1601-01-01 et le 1970-01-01.
const DAYS_1601_TO_1970: i64 = 134_774;

const fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

const fn days_in_month(y: i64, m: u16) -> u16 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Jours depuis le 1970-01-01 (algorithme civil proleptique grégorien).
const fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

const fn civil_from_days(z: i64) -> (i64, u16, u16) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u16, d as u16)
}

/// Jour de la semaine (0 = dimanche) d'une date civile.
fn day_of_week(y: i64, m: u16, d: u16) -> u16 {
    // Le 1970-01-01 était un jeudi (4).
    (days_from_civil(y, i64::from(m), i64::from(d)) + 4).rem_euclid(7) as u16
}

pub(crate) fn filetime_to_systemtime(ft: u64) -> Option<SystemTime> {
    if ft >= 0x8000_0000_0000_0000 {
        return None;
    }
    let days = (ft / TICKS_PER_DAY) as i64;
    let rem = ft % TICKS_PER_DAY;
    let (y, m, d) = civil_from_days(days - DAYS_1601_TO_1970);
    let ms_of_day = rem / TICKS_PER_MS;
    Some(SystemTime {
        year: y as u16,
        month: m,
        day_of_week: ((days + 1) % 7) as u16, // le 1601-01-01 était un lundi
        day: d,
        hour: (ms_of_day / 3_600_000) as u16,
        minute: (ms_of_day / 60_000 % 60) as u16,
        second: (ms_of_day / 1000 % 60) as u16,
        milliseconds: (ms_of_day % 1000) as u16,
    })
}

fn valid_date(st: &SystemTime) -> bool {
    (1601..=30827).contains(&st.year)
        && (1..=12).contains(&st.month)
        && st.day >= 1
        && st.day <= days_in_month(i64::from(st.year), st.month)
}

fn valid_time(st: &SystemTime) -> bool {
    st.hour < 24 && st.minute < 60 && st.second < 60 && st.milliseconds < 1000
}

pub(crate) fn systemtime_to_filetime(st: &SystemTime) -> Option<u64> {
    if !valid_date(st) || !valid_time(st) {
        return None;
    }
    let days = days_from_civil(i64::from(st.year), i64::from(st.month), i64::from(st.day))
        + DAYS_1601_TO_1970;
    let ms = u64::from(st.hour) * 3_600_000
        + u64::from(st.minute) * 60_000
        + u64::from(st.second) * 1000
        + u64::from(st.milliseconds);
    Some(days as u64 * TICKS_PER_DAY + ms * TICKS_PER_MS)
}

/// FILETIME UTC courant.
pub(crate) fn now_filetime() -> u64 {
    HostTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_nanos() / 100) as u64)
        .unwrap_or(0)
        .saturating_add(UNIX_TO_FILETIME)
}

/// Décalage local courant (secondes à ajouter à UTC), selon le fuseau de l'hôte.
pub(crate) fn local_offset_seconds() -> i64 {
    // SAFETY: `time` puis `localtime_r` sur des variables locales.
    unsafe {
        let now = libc::time(core::ptr::null_mut());
        let mut tm: libc::tm = core::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff
    }
}

fn shift_filetime(ft: u64, seconds: i64) -> u64 {
    let delta = seconds.saturating_mul(10_000_000);
    if delta >= 0 {
        ft.saturating_add(delta as u64)
    } else {
        ft.saturating_sub(delta.unsigned_abs())
    }
}

fn local_now() -> SystemTime {
    let ft = shift_filetime(now_filetime(), local_offset_seconds());
    filetime_to_systemtime(ft).unwrap_or_default()
}

pub(super) extern "win64" fn GetSystemTime(out: *mut SystemTime) {
    ffi_guard!(CHANNEL, "GetSystemTime", (), {
        write_out(
            out,
            filetime_to_systemtime(now_filetime()).unwrap_or_default(),
        );
    })
}

pub(super) extern "win64" fn GetLocalTime(out: *mut SystemTime) {
    ffi_guard!(CHANNEL, "GetLocalTime", (), { write_out(out, local_now()) })
}

pub(super) extern "win64" fn FileTimeToSystemTime(ft: *const u64, out: *mut SystemTime) -> WinBool {
    ffi_guard!(CHANNEL, "FileTimeToSystemTime", WIN_FALSE, {
        if ft.is_null() || out.is_null() {
            set_last_error(ERROR_INVALID_PARAMETER);
            return WIN_FALSE;
        }
        // SAFETY: `ft` non-nul (FILETIME 8 octets, éventuellement non aligné).
        let v = unsafe { ft.read_unaligned() };
        match filetime_to_systemtime(v) {
            Some(st) => {
                write_out(out, st);
                WIN_TRUE
            }
            None => {
                set_last_error(ERROR_INVALID_PARAMETER);
                WIN_FALSE
            }
        }
    })
}

pub(super) extern "win64" fn SystemTimeToFileTime(st: *const SystemTime, out: *mut u64) -> WinBool {
    ffi_guard!(CHANNEL, "SystemTimeToFileTime", WIN_FALSE, {
        if st.is_null() || out.is_null() {
            set_last_error(ERROR_INVALID_PARAMETER);
            return WIN_FALSE;
        }
        // SAFETY: `st` non-nul.
        let v = unsafe { st.read_unaligned() };
        match systemtime_to_filetime(&v) {
            Some(ft) => {
                write_out(out, ft);
                WIN_TRUE
            }
            None => {
                set_last_error(ERROR_INVALID_PARAMETER);
                WIN_FALSE
            }
        }
    })
}

fn convert_filetime(input: *const u64, out: *mut u64, seconds: i64) -> WinBool {
    if input.is_null() || out.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return WIN_FALSE;
    }
    // SAFETY: `input` non-nul.
    let v = unsafe { input.read_unaligned() };
    write_out(out, shift_filetime(v, seconds));
    WIN_TRUE
}

pub(super) extern "win64" fn FileTimeToLocalFileTime(ft: *const u64, out: *mut u64) -> WinBool {
    ffi_guard!(CHANNEL, "FileTimeToLocalFileTime", WIN_FALSE, {
        convert_filetime(ft, out, local_offset_seconds())
    })
}

pub(super) extern "win64" fn LocalFileTimeToFileTime(ft: *const u64, out: *mut u64) -> WinBool {
    ffi_guard!(CHANNEL, "LocalFileTimeToFileTime", WIN_FALSE, {
        convert_filetime(ft, out, -local_offset_seconds())
    })
}

// ==================== Paramètres régionaux ====================

const DEFAULT_LOCALE: &str = "en-US";
const LCID_EN_US: u32 = 0x0409;
const LOCALE_INVARIANT: u32 = 0x007F;
const LOCALE_USER_DEFAULT: u32 = 0x0400;
const LOCALE_SYSTEM_DEFAULT: u32 = 0x0800;
const LOCALE_CUSTOM_DEFAULT: u32 = 0x0C00;
const LOCALE_CUSTOM_UI_DEFAULT: u32 = 0x1400;
const LOCALE_ALLOW_NEUTRAL_NAMES: u32 = 0x0800_0000;
const LOCALE_NAME_SYSTEM_DEFAULT: &str = "!x-sys-default-locale";

/// Table nom ↔ LCID des locales courantes.
const LOCALES: &[(&str, u32)] = &[
    ("en-US", 0x0409),
    ("en-GB", 0x0809),
    ("en-AU", 0x0C09),
    ("en-CA", 0x1009),
    ("fr-FR", 0x040C),
    ("fr-CA", 0x0C0C),
    ("de-DE", 0x0407),
    ("es-ES", 0x0C0A),
    ("es-MX", 0x080A),
    ("it-IT", 0x0410),
    ("ja-JP", 0x0411),
    ("ko-KR", 0x0412),
    ("zh-CN", 0x0804),
    ("zh-TW", 0x0404),
    ("pt-BR", 0x0416),
    ("pt-PT", 0x0816),
    ("ru-RU", 0x0419),
    ("pl-PL", 0x0415),
    ("nl-NL", 0x0413),
    ("sv-SE", 0x041D),
    ("tr-TR", 0x041F),
    ("ar-SA", 0x0401),
    ("cs-CZ", 0x0405),
    ("da-DK", 0x0406),
    ("fi-FI", 0x040B),
    ("nb-NO", 0x0414),
    ("hu-HU", 0x040E),
    ("el-GR", 0x0408),
    ("he-IL", 0x040D),
    ("th-TH", 0x041E),
    ("uk-UA", 0x0422),
];

/// Résout un nom de locale en LCID (`None` si inconnu).
pub(crate) fn locale_name_to_lcid(name: Option<&str>, flags: u32) -> Option<u32> {
    let Some(name) = name else {
        return Some(LCID_EN_US);
    };
    if name.is_empty() {
        return Some(LOCALE_INVARIANT);
    }
    if name.eq_ignore_ascii_case(LOCALE_NAME_SYSTEM_DEFAULT) {
        return Some(LCID_EN_US);
    }
    if let Some(&(_, lcid)) = LOCALES.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        return Some(lcid);
    }
    // Nom neutre (`en`) : langue primaire ou locale spécifique par défaut.
    if !name.contains('-') {
        let prefix = format!("{}-", name.to_ascii_lowercase());
        let &(_, lcid) = LOCALES
            .iter()
            .find(|(n, _)| n.to_ascii_lowercase().starts_with(&prefix))?;
        return Some(if (flags & LOCALE_ALLOW_NEUTRAL_NAMES) != 0 {
            lcid & 0x3FF
        } else {
            lcid
        });
    }
    None
}

fn lcid_to_locale_name(lcid: u32) -> Option<&'static str> {
    match lcid {
        LOCALE_USER_DEFAULT
        | LOCALE_SYSTEM_DEFAULT
        | LOCALE_CUSTOM_DEFAULT
        | LOCALE_CUSTOM_UI_DEFAULT
        | 0 => Some(DEFAULT_LOCALE),
        LOCALE_INVARIANT => Some(""),
        _ => LOCALES.iter().find(|(_, l)| *l == lcid).map(|(n, _)| *n),
    }
}

pub(super) extern "win64" fn GetUserDefaultLocaleName(buf: *mut u16, cch: i32) -> i32 {
    ffi_guard!(CHANNEL, "GetUserDefaultLocaleName", 0, {
        return_wide(DEFAULT_LOCALE, buf, cch)
    })
}

pub(super) extern "win64" fn GetSystemDefaultLocaleName(buf: *mut u16, cch: i32) -> i32 {
    ffi_guard!(CHANNEL, "GetSystemDefaultLocaleName", 0, {
        return_wide(DEFAULT_LOCALE, buf, cch)
    })
}

pub(super) extern "win64" fn GetUserDefaultGeoName(buf: *mut u16, cch: i32) -> i32 {
    ffi_guard!(CHANNEL, "GetUserDefaultGeoName", 0, {
        return_wide("US", buf, cch)
    })
}

const GEOCLASS_NATION: u32 = 16;
const GEOID_US: i32 = 244;
const GEOID_NOT_AVAILABLE: i32 = -1;

pub(super) extern "win64" fn GetUserGeoID(class: u32) -> i32 {
    if class == GEOCLASS_NATION {
        GEOID_US
    } else {
        GEOID_NOT_AVAILABLE
    }
}

pub(super) extern "win64" fn GetUserDefaultLCID() -> u32 {
    LCID_EN_US
}

pub(super) extern "win64" fn GetSystemDefaultLCID() -> u32 {
    LCID_EN_US
}

pub(super) extern "win64" fn GetUserDefaultLangID() -> u16 {
    LCID_EN_US as u16
}

pub(super) extern "win64" fn GetUserDefaultUILanguage() -> u16 {
    LCID_EN_US as u16
}

pub(super) extern "win64" fn GetSystemDefaultUILanguage() -> u16 {
    LCID_EN_US as u16
}

pub(super) extern "win64" fn GetThreadLocale() -> u32 {
    LCID_EN_US
}

pub(super) extern "win64" fn LocaleNameToLCID(name: *const u16, flags: u32) -> u32 {
    ffi_guard!(CHANNEL, "LocaleNameToLCID", 0, {
        // SAFETY: `name` est un `PCWSTR` valide ou nul (LOCALE_NAME_USER_DEFAULT).
        let n = unsafe { from_pcwstr(name) };
        match locale_name_to_lcid(n.as_deref(), flags) {
            Some(l) => l,
            None => {
                set_last_error(ERROR_INVALID_PARAMETER);
                0
            }
        }
    })
}

pub(super) extern "win64" fn LCIDToLocaleName(
    lcid: u32,
    buf: *mut u16,
    cch: i32,
    flags: u32,
) -> i32 {
    ffi_guard!(CHANNEL, "LCIDToLocaleName", 0, {
        match lcid_to_locale_name(lcid) {
            Some(n) => return_wide(n, buf, cch),
            None => {
                set_last_error(ERROR_INVALID_PARAMETER);
                0
            }
        }
    })
}

/// Formats d'une locale (seuls `en-US` et la locale invariante sont distingués).
struct LocaleFormats {
    short_date: &'static str,
    long_date: &'static str,
    year_month: &'static str,
    month_day: &'static str,
    time: &'static str,
}

const EN_US_FORMATS: LocaleFormats = LocaleFormats {
    short_date: "M/d/yyyy",
    long_date: "dddd, MMMM d, yyyy",
    year_month: "MMMM yyyy",
    month_day: "MMMM d",
    time: "h:mm:ss tt",
};

const INVARIANT_FORMATS: LocaleFormats = LocaleFormats {
    short_date: "MM/dd/yyyy",
    long_date: "dddd, dd MMMM yyyy",
    year_month: "yyyy MMMM",
    month_day: "MMMM dd",
    time: "HH:mm:ss",
};

/// Formats associés à une locale reconnue ; `None` si la locale est inconnue.
/// Toute locale connue autre que l'invariante utilise les formats `en-US` (seule locale fournie).
fn formats_for(lcid: u32) -> &'static LocaleFormats {
    if lcid == LOCALE_INVARIANT {
        &INVARIANT_FORMATS
    } else {
        &EN_US_FORMATS
    }
}

const DAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Field(char, usize),
    Lit(String),
}

/// Découpe une image de format en champs (`dd`, `MMMM`, ...) et littéraux (`'...'`).
fn tokenize(pic: &str, fields: &str) -> Vec<Tok> {
    let chars: Vec<char> = pic.chars().collect();
    let mut out: Vec<Tok> = Vec::new();
    let mut lit = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' && chars.get(i + 1) == Some(&'\'') {
            lit.push('\'');
            i += 2;
        } else if c == '\'' {
            i += 1;
            while i < chars.len() {
                if chars[i] == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        lit.push('\'');
                        i += 2;
                        continue;
                    }
                    break;
                }
                lit.push(chars[i]);
                i += 1;
            }
            i += 1;
        } else if fields.contains(c) {
            let mut n = 0;
            while i < chars.len() && chars[i] == c {
                n += 1;
                i += 1;
            }
            if !lit.is_empty() {
                out.push(Tok::Lit(std::mem::take(&mut lit)));
            }
            out.push(Tok::Field(c, n));
        } else {
            lit.push(c);
            i += 1;
        }
    }
    if !lit.is_empty() {
        out.push(Tok::Lit(lit));
    }
    out
}

fn render_date(pic: &str, st: &SystemTime) -> String {
    let mut s = String::new();
    let dow = usize::from(day_of_week(i64::from(st.year), st.month, st.day));
    let month = usize::from(st.month.clamp(1, 12)) - 1;
    for t in tokenize(pic, "dMyg") {
        match t {
            Tok::Lit(l) => s.push_str(&l),
            Tok::Field('d', 1) => s.push_str(&st.day.to_string()),
            Tok::Field('d', 2) => s.push_str(&format!("{:02}", st.day)),
            Tok::Field('d', 3) => s.push_str(&DAY_NAMES[dow][..3]),
            Tok::Field('d', _) => s.push_str(DAY_NAMES[dow]),
            Tok::Field('M', 1) => s.push_str(&st.month.to_string()),
            Tok::Field('M', 2) => s.push_str(&format!("{:02}", st.month)),
            Tok::Field('M', 3) => s.push_str(&MONTH_NAMES[month][..3]),
            Tok::Field('M', _) => s.push_str(MONTH_NAMES[month]),
            Tok::Field('y', 1) => s.push_str(&(st.year % 100).to_string()),
            Tok::Field('y', 2) => s.push_str(&format!("{:02}", st.year % 100)),
            Tok::Field('y', _) => s.push_str(&format!("{:04}", st.year)),
            Tok::Field('g', _) => s.push_str("A.D."),
            Tok::Field(..) => {}
        }
    }
    s
}

const TIME_NOMINUTESORSECONDS: u32 = 0x1;
const TIME_NOSECONDS: u32 = 0x2;
const TIME_NOTIMEMARKER: u32 = 0x4;
const TIME_FORCE24HOURFORMAT: u32 = 0x8;
const LOCALE_NOUSEROVERRIDE: u32 = 0x8000_0000;
const LOCALE_USE_CP_ACP: u32 = 0x4000_0000;

/// Retire les champs `remove` et le littéral séparateur qui les précède.
fn strip_fields(tokens: Vec<Tok>, remove: &[char]) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::new();
    for t in tokens {
        if let Tok::Field(c, _) = t
            && remove.contains(&c)
        {
            if matches!(out.last(), Some(Tok::Lit(_))) {
                out.pop();
            }
            continue;
        }
        out.push(t);
    }
    out
}

fn render_time(pic: &str, st: &SystemTime, flags: u32) -> String {
    let mut tokens = tokenize(pic, "hHmst");
    let mut remove = Vec::new();
    if (flags & TIME_NOMINUTESORSECONDS) != 0 {
        remove.extend(['m', 's']);
    } else if (flags & TIME_NOSECONDS) != 0 {
        remove.push('s');
    }
    if (flags & TIME_NOTIMEMARKER) != 0 {
        remove.push('t');
    }
    if !remove.is_empty() {
        tokens = strip_fields(tokens, &remove);
    }
    let h12 = match st.hour % 12 {
        0 => 12,
        h => h,
    };
    let pm = st.hour >= 12;
    let mut s = String::new();
    for t in tokens {
        match t {
            Tok::Lit(l) => s.push_str(&l),
            Tok::Field('h', n) if (flags & TIME_FORCE24HOURFORMAT) == 0 => {
                if n == 1 {
                    s.push_str(&h12.to_string());
                } else {
                    s.push_str(&format!("{h12:02}"));
                }
            }
            Tok::Field('h' | 'H', 1) => s.push_str(&st.hour.to_string()),
            Tok::Field('h' | 'H', _) => s.push_str(&format!("{:02}", st.hour)),
            Tok::Field('m', 1) => s.push_str(&st.minute.to_string()),
            Tok::Field('m', _) => s.push_str(&format!("{:02}", st.minute)),
            Tok::Field('s', 1) => s.push_str(&st.second.to_string()),
            Tok::Field('s', _) => s.push_str(&format!("{:02}", st.second)),
            Tok::Field('t', 1) => s.push(if pm { 'P' } else { 'A' }),
            Tok::Field('t', _) => s.push_str(if pm { "PM" } else { "AM" }),
            Tok::Field(..) => {}
        }
    }
    s
}

const DATE_SHORTDATE: u32 = 0x01;
const DATE_LONGDATE: u32 = 0x02;
const DATE_USE_ALT_CALENDAR: u32 = 0x04;
const DATE_YEARMONTH: u32 = 0x08;
const DATE_LTRREADING: u32 = 0x10;
const DATE_RTLREADING: u32 = 0x20;
const DATE_AUTOLAYOUT: u32 = 0x40;
const DATE_MONTHDAY: u32 = 0x80;

/// Implémentation commune de `GetDateFormat(Ex)` : retourne la chaîne ou un code d'erreur.
pub(crate) fn format_date(
    lcid: u32,
    flags: u32,
    date: Option<SystemTime>,
    format: Option<&str>,
) -> Result<String, u32> {
    let kinds = DATE_SHORTDATE | DATE_LONGDATE | DATE_YEARMONTH | DATE_MONTHDAY;
    let allowed = kinds
        | DATE_USE_ALT_CALENDAR
        | DATE_LTRREADING
        | DATE_RTLREADING
        | DATE_AUTOLAYOUT
        | LOCALE_NOUSEROVERRIDE
        | LOCALE_USE_CP_ACP;
    if (flags & !allowed) != 0
        || (flags & kinds).count_ones() > 1
        || (format.is_some() && (flags & kinds) != 0)
        || (flags & (DATE_LTRREADING | DATE_RTLREADING)) == (DATE_LTRREADING | DATE_RTLREADING)
    {
        return Err(ERROR_INVALID_FLAGS);
    }
    let st = match date {
        Some(d) if !valid_date(&d) => return Err(ERROR_INVALID_PARAMETER),
        Some(d) => d,
        None => local_now(),
    };
    let f = formats_for(lcid);
    let pic = match format {
        Some(p) => p,
        None if (flags & DATE_LONGDATE) != 0 => f.long_date,
        None if (flags & DATE_YEARMONTH) != 0 => f.year_month,
        None if (flags & DATE_MONTHDAY) != 0 => f.month_day,
        None => f.short_date,
    };
    let mut s = String::new();
    if (flags & DATE_LTRREADING) != 0 {
        s.push('\u{200E}');
    } else if (flags & DATE_RTLREADING) != 0 {
        s.push('\u{200F}');
    }
    s.push_str(&render_date(pic, &st));
    Ok(s)
}

/// Implémentation commune de `GetTimeFormat(Ex)`.
pub(crate) fn format_time(
    lcid: u32,
    flags: u32,
    time: Option<SystemTime>,
    format: Option<&str>,
) -> Result<String, u32> {
    let allowed = TIME_NOMINUTESORSECONDS
        | TIME_NOSECONDS
        | TIME_NOTIMEMARKER
        | TIME_FORCE24HOURFORMAT
        | LOCALE_NOUSEROVERRIDE
        | LOCALE_USE_CP_ACP;
    if (flags & !allowed) != 0 {
        return Err(ERROR_INVALID_FLAGS);
    }
    let st = match time {
        Some(t) if !valid_time(&t) => return Err(ERROR_INVALID_PARAMETER),
        Some(t) => t,
        None => local_now(),
    };
    let pic = format.unwrap_or(formats_for(lcid).time);
    Ok(render_time(pic, &st, flags))
}

fn emit(result: Result<String, u32>, out: *mut u16, cch: i32) -> i32 {
    match result {
        Ok(s) => return_wide(&s, out, cch),
        Err(code) => {
            set_last_error(code);
            0
        }
    }
}

fn read_st(p: *const SystemTime) -> Option<SystemTime> {
    if p.is_null() {
        None
    } else {
        // SAFETY: `p` non-nul pointe vers un `SYSTEMTIME` de l'invité.
        Some(unsafe { p.read_unaligned() })
    }
}

fn locale_from_name(name: *const u16) -> Result<u32, u32> {
    // SAFETY: `name` est un `PCWSTR` valide ou nul.
    let n = unsafe { from_pcwstr(name) };
    locale_name_to_lcid(n.as_deref(), 0).ok_or(ERROR_INVALID_PARAMETER)
}

fn locale_from_lcid(lcid: u32) -> Result<u32, u32> {
    match lcid_to_locale_name(lcid) {
        Some("") => Ok(LOCALE_INVARIANT),
        Some(_) => Ok(lcid),
        None => Err(ERROR_INVALID_PARAMETER),
    }
}

fn check_out(out: *mut u16, cch: i32) -> Result<(), u32> {
    if cch < 0 || (cch > 0 && out.is_null()) {
        Err(ERROR_INVALID_PARAMETER)
    } else {
        Ok(())
    }
}

pub(super) extern "win64" fn GetDateFormatEx(
    locale: *const u16,
    flags: u32,
    date: *const SystemTime,
    format: *const u16,
    out: *mut u16,
    cch: i32,
    calendar: *const u16,
) -> i32 {
    ffi_guard!(CHANNEL, "GetDateFormatEx", 0, {
        // SAFETY: `format` est un `PCWSTR` valide ou nul.
        let fmt = unsafe { from_pcwstr(format) };
        let r = check_out(out, cch)
            .and_then(|()| {
                if calendar.is_null() {
                    Ok(())
                } else {
                    Err(ERROR_INVALID_PARAMETER)
                }
            })
            .and_then(|()| locale_from_name(locale))
            .and_then(|lcid| format_date(lcid, flags, read_st(date), fmt.as_deref()));
        emit(r, out, cch)
    })
}

pub(super) extern "win64" fn GetDateFormatW(
    lcid: u32,
    flags: u32,
    date: *const SystemTime,
    format: *const u16,
    out: *mut u16,
    cch: i32,
) -> i32 {
    ffi_guard!(CHANNEL, "GetDateFormatW", 0, {
        // SAFETY: `format` est un `PCWSTR` valide ou nul.
        let fmt = unsafe { from_pcwstr(format) };
        let r = check_out(out, cch)
            .and_then(|()| locale_from_lcid(lcid))
            .and_then(|l| format_date(l, flags, read_st(date), fmt.as_deref()));
        emit(r, out, cch)
    })
}

pub(super) extern "win64" fn GetTimeFormatEx(
    locale: *const u16,
    flags: u32,
    time: *const SystemTime,
    format: *const u16,
    out: *mut u16,
    cch: i32,
) -> i32 {
    ffi_guard!(CHANNEL, "GetTimeFormatEx", 0, {
        // SAFETY: `format` est un `PCWSTR` valide ou nul.
        let fmt = unsafe { from_pcwstr(format) };
        let r = check_out(out, cch)
            .and_then(|()| locale_from_name(locale))
            .and_then(|lcid| format_time(lcid, flags, read_st(time), fmt.as_deref()));
        emit(r, out, cch)
    })
}

pub(super) extern "win64" fn GetTimeFormatW(
    lcid: u32,
    flags: u32,
    time: *const SystemTime,
    format: *const u16,
    out: *mut u16,
    cch: i32,
) -> i32 {
    ffi_guard!(CHANNEL, "GetTimeFormatW", 0, {
        // SAFETY: `format` est un `PCWSTR` valide ou nul.
        let fmt = unsafe { from_pcwstr(format) };
        let r = check_out(out, cch)
            .and_then(|()| locale_from_lcid(lcid))
            .and_then(|l| format_time(l, flags, read_st(time), fmt.as_deref()));
        emit(r, out, cch)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_common::wide::to_wide_null;
    use sb_loader::last_error;

    fn st(y: u16, mo: u16, d: u16, h: u16, mi: u16, s: u16, ms: u16) -> SystemTime {
        SystemTime {
            year: y,
            month: mo,
            day_of_week: 0,
            day: d,
            hour: h,
            minute: mi,
            second: s,
            milliseconds: ms,
        }
    }

    #[test]
    fn filetime_systemtime_roundtrip() {
        super::super::init_test_thread();
        // 1601-01-01 00:00:00.000 (lundi)
        let mut out = SystemTime::default();
        let zero = 0u64;
        assert_eq!(FileTimeToSystemTime(&zero, &mut out), WIN_TRUE);
        assert_eq!(
            out,
            SystemTime {
                day_of_week: 1,
                ..st(1601, 1, 1, 0, 0, 0, 0)
            }
        );
        // Époque Unix : 1970-01-01 (jeudi)
        assert_eq!(FileTimeToSystemTime(&UNIX_TO_FILETIME, &mut out), WIN_TRUE);
        assert_eq!(
            (out.year, out.month, out.day, out.day_of_week),
            (1970, 1, 1, 4)
        );

        for &(y, mo, d, h, mi, s, ms) in &[
            (2000u16, 2u16, 29u16, 23u16, 59u16, 59u16, 999u16),
            (2024, 12, 31, 12, 0, 1, 5),
            (1601, 1, 1, 0, 0, 0, 1),
            (30827, 12, 31, 23, 59, 59, 999),
        ] {
            let input = st(y, mo, d, h, mi, s, ms);
            let mut ft = 0u64;
            assert_eq!(SystemTimeToFileTime(&input, &mut ft), WIN_TRUE);
            let mut back = SystemTime::default();
            assert_eq!(FileTimeToSystemTime(&ft, &mut back), WIN_TRUE);
            assert_eq!(
                SystemTime {
                    day_of_week: 0,
                    ..back
                },
                input
            );
            assert_eq!(back.day_of_week, day_of_week(i64::from(y), mo, d));
        }
        // 2000-02-29 était un mardi.
        assert_eq!(day_of_week(2000, 2, 29), 2);

        let bad = st(2023, 2, 29, 0, 0, 0, 0);
        let mut ft = 0u64;
        assert_eq!(SystemTimeToFileTime(&bad, &mut ft), WIN_FALSE);
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        let neg = 0x8000_0000_0000_0000u64;
        assert_eq!(FileTimeToSystemTime(&neg, &mut out), WIN_FALSE);

        // Local ↔ UTC aller-retour.
        let utc = 133_000_000_000_000_000u64;
        let mut local = 0u64;
        let mut back = 0u64;
        assert_eq!(FileTimeToLocalFileTime(&utc, &mut local), WIN_TRUE);
        assert_eq!(LocalFileTimeToFileTime(&local, &mut back), WIN_TRUE);
        assert_eq!(back, utc);

        let mut now = SystemTime::default();
        GetSystemTime(&mut now);
        assert!(now.year >= 2024 && valid_date(&now) && valid_time(&now));
        GetLocalTime(&mut now);
        assert!(valid_date(&now));
    }

    fn date_ex(flags: u32, d: &SystemTime, fmt: Option<&str>) -> (i32, String) {
        let fmt_w = fmt.map(to_wide_null);
        let fmt_p = fmt_w.as_ref().map_or(core::ptr::null(), |v| v.as_ptr());
        let loc = to_wide_null("en-US");
        let mut buf = [0u16; 128];
        let n = GetDateFormatEx(
            loc.as_ptr(),
            flags,
            d,
            fmt_p,
            buf.as_mut_ptr(),
            128,
            core::ptr::null(),
        );
        let s = String::from_utf16_lossy(&buf[..(n.max(1) - 1) as usize]);
        (n, s)
    }

    fn time_ex(flags: u32, t: &SystemTime, fmt: Option<&str>) -> String {
        let fmt_w = fmt.map(to_wide_null);
        let fmt_p = fmt_w.as_ref().map_or(core::ptr::null(), |v| v.as_ptr());
        let mut buf = [0u16; 64];
        let n = GetTimeFormatEx(core::ptr::null(), flags, t, fmt_p, buf.as_mut_ptr(), 64);
        assert!(n > 0);
        String::from_utf16_lossy(&buf[..(n - 1) as usize])
    }

    #[test]
    fn date_and_time_formats() {
        super::super::init_test_thread();
        let d = st(2024, 3, 5, 13, 7, 9, 0);
        assert_eq!(date_ex(0, &d, None).1, "3/5/2024");
        assert_eq!(date_ex(DATE_SHORTDATE, &d, None), (9, "3/5/2024".into()));
        assert_eq!(date_ex(DATE_LONGDATE, &d, None).1, "Tuesday, March 5, 2024");
        assert_eq!(date_ex(DATE_YEARMONTH, &d, None).1, "March 2024");
        assert_eq!(date_ex(DATE_MONTHDAY, &d, None).1, "March 5");
        assert_eq!(
            date_ex(0, &d, Some("ddd dd MMM yy 'of' g ''")).1,
            "Tue 05 Mar 24 of A.D. '"
        );
        assert_eq!(date_ex(DATE_SHORTDATE, &d, Some("d")).0, 0);
        assert_eq!(last_error(), ERROR_INVALID_FLAGS);
        assert_eq!(date_ex(DATE_SHORTDATE | DATE_LONGDATE, &d, None).0, 0);
        assert_eq!(last_error(), ERROR_INVALID_FLAGS);
        assert_eq!(date_ex(0, &st(2024, 2, 30, 0, 0, 0, 0), None).0, 0);
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);

        // Taille requise et tampon trop petit.
        let loc = to_wide_null("en-US");
        assert_eq!(
            GetDateFormatEx(
                loc.as_ptr(),
                0,
                &d,
                core::ptr::null(),
                core::ptr::null_mut(),
                0,
                core::ptr::null()
            ),
            9
        );
        let mut small = [0u16; 4];
        assert_eq!(
            GetDateFormatEx(
                loc.as_ptr(),
                0,
                &d,
                core::ptr::null(),
                small.as_mut_ptr(),
                4,
                core::ptr::null()
            ),
            0
        );
        assert_eq!(last_error(), ERROR_INSUFFICIENT_BUFFER);
        let bogus = to_wide_null("xx-ZZ");
        assert_eq!(
            GetDateFormatEx(
                bogus.as_ptr(),
                0,
                &d,
                core::ptr::null(),
                core::ptr::null_mut(),
                0,
                core::ptr::null()
            ),
            0
        );
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);

        assert_eq!(time_ex(0, &d, None), "1:07:09 PM");
        assert_eq!(time_ex(TIME_NOSECONDS, &d, None), "1:07 PM");
        assert_eq!(time_ex(TIME_NOMINUTESORSECONDS, &d, None), "1 PM");
        assert_eq!(time_ex(TIME_NOTIMEMARKER, &d, None), "1:07:09");
        assert_eq!(
            time_ex(TIME_FORCE24HOURFORMAT | TIME_NOTIMEMARKER, &d, None),
            "13:07:09"
        );
        assert_eq!(
            time_ex(0, &st(2024, 1, 1, 0, 5, 0, 0), Some("hh:mm t")),
            "12:05 A"
        );
        assert_eq!(time_ex(0, &d, Some("HH'h'mm")), "13h07");

        let mut buf = [0u16; 32];
        assert_eq!(
            GetDateFormatW(
                LOCALE_INVARIANT,
                0,
                &d,
                core::ptr::null(),
                buf.as_mut_ptr(),
                32
            ),
            11
        );
        assert_eq!(String::from_utf16_lossy(&buf[..10]), "03/05/2024");
        assert!(
            GetTimeFormatW(
                LOCALE_USER_DEFAULT,
                0,
                &d,
                core::ptr::null(),
                buf.as_mut_ptr(),
                32
            ) > 0
        );
    }

    #[test]
    fn locale_names_and_ids() {
        super::super::init_test_thread();
        let mut buf = [0u16; 85];
        assert_eq!(GetUserDefaultLocaleName(buf.as_mut_ptr(), 85), 6);
        assert_eq!(String::from_utf16_lossy(&buf[..5]), "en-US");
        assert_eq!(GetUserDefaultLocaleName(buf.as_mut_ptr(), 3), 0);
        assert_eq!(last_error(), ERROR_INSUFFICIENT_BUFFER);
        assert_eq!(GetUserDefaultGeoName(core::ptr::null_mut(), 0), 3);
        assert_eq!(GetUserDefaultGeoName(buf.as_mut_ptr(), 3), 3);
        assert_eq!(String::from_utf16_lossy(&buf[..2]), "US");
        assert_eq!(GetUserDefaultLCID(), 0x0409);
        assert_eq!(GetUserGeoID(GEOCLASS_NATION), 244);

        let fr = to_wide_null("fr-FR");
        assert_eq!(LocaleNameToLCID(fr.as_ptr(), 0), 0x040C);
        assert_eq!(LocaleNameToLCID(core::ptr::null(), 0), 0x0409);
        let inv = to_wide_null("");
        assert_eq!(LocaleNameToLCID(inv.as_ptr(), 0), LOCALE_INVARIANT);
        let en = to_wide_null("en");
        assert_eq!(LocaleNameToLCID(en.as_ptr(), 0), 0x0409);
        assert_eq!(
            LocaleNameToLCID(en.as_ptr(), LOCALE_ALLOW_NEUTRAL_NAMES),
            0x0009
        );
        let bad = to_wide_null("zz-QQ");
        assert_eq!(LocaleNameToLCID(bad.as_ptr(), 0), 0);
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        assert_eq!(LCIDToLocaleName(0x0411, buf.as_mut_ptr(), 85, 0), 6);
        assert_eq!(String::from_utf16_lossy(&buf[..5]), "ja-JP");
    }
}
