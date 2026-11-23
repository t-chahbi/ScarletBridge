//! `FormatMessageA/W` (table des messages système Win32 courants, inserts `%1..%99` avec
//! format printf, `ALLOCATE_BUFFER`, `IGNORE_INSERTS`, `FROM_STRING`, `MAX_WIDTH_MASK`)
//! et allocateurs `Local*` / `Global*` (tas libc, compatibles avec `LocalFree`).

use core::ffi::c_void;
use std::ffi::CStr;
use std::ptr;

use sb_common::ffi_guard;
use sb_loader::set_last_error;

use super::{
    CHANNEL, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER,
    ERROR_MR_MID_NOT_FOUND, ERROR_NOT_ENOUGH_MEMORY, write_out,
};

// ==================== Local* / Global* ====================

const LMEM_MOVEABLE: u32 = 0x0002;
const LMEM_ZEROINIT: u32 = 0x0040;
const LMEM_MODIFY: u32 = 0x0080;
/// `LMEM_VALID_FLAGS` | bits `GMEM_*` additionnels (DDESHARE, ...).
const LMEM_VALID_FLAGS: u32 = 0x7F72;
const ERROR_NOT_LOCKED: u32 = 158;

pub(crate) fn local_alloc(flags: u32, bytes: usize) -> *mut c_void {
    if (flags & !LMEM_VALID_FLAGS) != 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return ptr::null_mut();
    }
    // SAFETY: allocation libc standard (libérée par `LocalFree` -> `free`).
    let p = unsafe {
        if (flags & LMEM_ZEROINIT) != 0 {
            libc::calloc(1, bytes.max(1))
        } else {
            libc::malloc(bytes.max(1))
        }
    };
    if p.is_null() {
        set_last_error(ERROR_NOT_ENOUGH_MEMORY);
    }
    p.cast()
}

fn usable_size(p: *mut c_void) -> usize {
    // SAFETY: `p` provient de `malloc`/`calloc`/`realloc`.
    unsafe { libc::malloc_usable_size(p.cast()) }
}

pub(super) extern "win64" fn LocalAlloc(flags: u32, bytes: usize) -> *mut c_void {
    ffi_guard!(CHANNEL, "LocalAlloc", ptr::null_mut(), {
        local_alloc(flags, bytes)
    })
}

pub(crate) extern "win64" fn LocalFree(mem: *mut c_void) -> *mut c_void {
    ffi_guard!(CHANNEL, "LocalFree", mem, {
        if !mem.is_null() {
            // SAFETY: `mem` provient de `LocalAlloc` (allocateur libc).
            unsafe { libc::free(mem.cast()) };
        }
        ptr::null_mut()
    })
}

pub(super) extern "win64" fn LocalReAlloc(
    mem: *mut c_void,
    bytes: usize,
    flags: u32,
) -> *mut c_void {
    ffi_guard!(CHANNEL, "LocalReAlloc", ptr::null_mut(), {
        if mem.is_null() {
            set_last_error(ERROR_INVALID_HANDLE);
            return ptr::null_mut();
        }
        if (flags & LMEM_MODIFY) != 0 {
            return mem;
        }
        let old = usable_size(mem);
        let target = bytes.max(1);
        let p = if target <= old {
            mem
        } else if (flags & LMEM_MOVEABLE) != 0 {
            // SAFETY: `mem` provient de l'allocateur libc.
            let p = unsafe { libc::realloc(mem.cast(), target) }.cast::<c_void>();
            if p.is_null() {
                set_last_error(ERROR_NOT_ENOUGH_MEMORY);
                return ptr::null_mut();
            }
            p
        } else {
            // Bloc fixe sans `LMEM_MOVEABLE` : impossible de l'agrandir sur place.
            set_last_error(ERROR_NOT_ENOUGH_MEMORY);
            return ptr::null_mut();
        };
        if (flags & LMEM_ZEROINIT) != 0 && target > old {
            // SAFETY: `[old, target)` est dans le bloc réalloué.
            unsafe { ptr::write_bytes(p.cast::<u8>().add(old), 0, target - old) };
        }
        p
    })
}

pub(super) extern "win64" fn LocalLock(mem: *mut c_void) -> *mut c_void {
    if mem.is_null() {
        set_last_error(ERROR_INVALID_HANDLE);
    }
    mem
}

pub(super) extern "win64" fn LocalUnlock(mem: *mut c_void) -> i32 {
    // Blocs fixes : rien n'est verrouillé.
    set_last_error(if mem.is_null() {
        ERROR_INVALID_HANDLE
    } else {
        ERROR_NOT_LOCKED
    });
    0
}

pub(super) extern "win64" fn LocalSize(mem: *mut c_void) -> usize {
    ffi_guard!(CHANNEL, "LocalSize", 0, {
        if mem.is_null() {
            set_last_error(ERROR_INVALID_HANDLE);
            return 0;
        }
        usable_size(mem)
    })
}

// ==================== Table des messages système ====================

const SYSTEM_MESSAGES: &[(u32, &str)] = &[
    (0, "The operation completed successfully.\r\n"),
    (1, "Incorrect function.\r\n"),
    (2, "The system cannot find the file specified.\r\n"),
    (3, "The system cannot find the path specified.\r\n"),
    (4, "The system cannot open the file.\r\n"),
    (5, "Access is denied.\r\n"),
    (6, "The handle is invalid.\r\n"),
    (
        8,
        "Not enough memory resources are available to process this command.\r\n",
    ),
    (12, "The access code is invalid.\r\n"),
    (13, "The data is invalid.\r\n"),
    (
        14,
        "Not enough memory resources are available to complete this operation.\r\n",
    ),
    (15, "The system cannot find the drive specified.\r\n"),
    (16, "The directory cannot be removed.\r\n"),
    (
        17,
        "The system cannot move the file to a different disk drive.\r\n",
    ),
    (18, "There are no more files.\r\n"),
    (19, "The media is write protected.\r\n"),
    (21, "The device is not ready.\r\n"),
    (23, "Data error (cyclic redundancy check).\r\n"),
    (
        24,
        "The program issued a command but the command length is incorrect.\r\n",
    ),
    (
        32,
        "The process cannot access the file because it is being used by another process.\r\n",
    ),
    (
        33,
        "The process cannot access the file because another process has locked a portion of the file.\r\n",
    ),
    (38, "Reached the end of the file.\r\n"),
    (39, "The disk is full.\r\n"),
    (50, "The request is not supported.\r\n"),
    (53, "The network path was not found.\r\n"),
    (80, "The file exists.\r\n"),
    (82, "The directory or file cannot be created.\r\n"),
    (87, "The parameter is incorrect.\r\n"),
    (109, "The pipe has been ended.\r\n"),
    (
        110,
        "The system cannot open the device or file specified.\r\n",
    ),
    (111, "The file name is too long.\r\n"),
    (112, "There is not enough space on the disk.\r\n"),
    (120, "This function is not supported on this system.\r\n"),
    (
        122,
        "The data area passed to a system call is too small.\r\n",
    ),
    (
        123,
        "The filename, directory name, or volume label syntax is incorrect.\r\n",
    ),
    (126, "The specified module could not be found.\r\n"),
    (127, "The specified procedure could not be found.\r\n"),
    (145, "The directory is not empty.\r\n"),
    (158, "The segment is already unlocked.\r\n"),
    (161, "The specified path is invalid.\r\n"),
    (170, "The requested resource is in use.\r\n"),
    (
        183,
        "Cannot create a file when that file already exists.\r\n",
    ),
    (193, "%1 is not a valid Win32 application.\r\n"),
    (
        203,
        "The system could not find the environment option that was entered.\r\n",
    ),
    (206, "The filename or extension is too long.\r\n"),
    (230, "The pipe state is invalid.\r\n"),
    (231, "All pipe instances are busy.\r\n"),
    (232, "The pipe is being closed.\r\n"),
    (234, "More data is available.\r\n"),
    (258, "The wait operation timed out.\r\n"),
    (259, "No more data is available.\r\n"),
    (267, "The directory name is invalid.\r\n"),
    (288, "Attempt to release mutex not owned by caller.\r\n"),
    (298, "Too many posts were made to a semaphore.\r\n"),
    (
        317,
        "The system cannot find message text for message number 0x%1 in the message file for %2.\r\n",
    ),
    (487, "Attempt to access invalid address.\r\n"),
    (534, "Arithmetic result exceeded 32 bits.\r\n"),
    (
        995,
        "The I/O operation has been aborted because of either a thread exit or an application request.\r\n",
    ),
    (996, "Overlapped I/O event is not in a signaled state.\r\n"),
    (997, "Overlapped I/O operation is in progress.\r\n"),
    (998, "Invalid access to memory location.\r\n"),
    (1004, "Invalid flags.\r\n"),
    (
        1114,
        "A dynamic link library (DLL) initialization routine failed.\r\n",
    ),
    (
        1150,
        "The specified program requires a newer version of Windows.\r\n",
    ),
    (1168, "Element not found.\r\n"),
    (1223, "The operation was canceled by the user.\r\n"),
    (1400, "Invalid window handle.\r\n"),
    (1407, "Cannot find window class.\r\n"),
    (1410, "Class already exists.\r\n"),
    (1411, "Class does not exist.\r\n"),
    (1413, "Invalid index.\r\n"),
    (
        1460,
        "This operation returned because the timeout period expired.\r\n",
    ),
    (
        1813,
        "The specified resource type cannot be found in the image file.\r\n",
    ),
    (
        10013,
        "An attempt was made to access a socket in a way forbidden by its access permissions.\r\n",
    ),
    (
        10035,
        "A non-blocking socket operation could not be completed immediately.\r\n",
    ),
    (
        10051,
        "A socket operation was attempted to an unreachable network.\r\n",
    ),
    (
        10054,
        "An existing connection was forcibly closed by the remote host.\r\n",
    ),
    (
        10060,
        "A connection attempt failed because the connected party did not properly respond after a period of time, or established connection failed because connected host has failed to respond.\r\n",
    ),
    (
        10061,
        "No connection could be made because the target machine actively refused it.\r\n",
    ),
    (
        10065,
        "A socket operation was attempted to an unreachable host.\r\n",
    ),
    (11001, "No such host is known.\r\n"),
    (15100, "The resource loader failed to find MUI file.\r\n"),
    (0x8000_4001, "Not implemented\r\n"),
    (0x8000_4002, "No such interface supported\r\n"),
    (0x8000_4003, "Invalid pointer\r\n"),
    (0x8000_4004, "Operation aborted\r\n"),
    (0x8000_4005, "Unspecified error\r\n"),
    (0x8000_FFFF, "Catastrophic failure\r\n"),
    (
        0x8001_0106,
        "Cannot change thread mode after it is set.\r\n",
    ),
    (0x8004_0154, "Class not registered\r\n"),
    (0x8004_01F0, "CoInitialize has not been called.\r\n"),
];

/// Texte système d'un code Win32 ou HRESULT (`FACILITY_WIN32` replié sur le code Win32).
pub(crate) fn system_message(id: u32) -> Option<&'static str> {
    let lookup = |k: u32| {
        SYSTEM_MESSAGES
            .iter()
            .find(|(c, _)| *c == k)
            .map(|(_, t)| *t)
    };
    lookup(id).or_else(|| {
        if (id & 0xFFFF_0000) == 0x8007_0000 {
            lookup(id & 0xFFFF)
        } else {
            None
        }
    })
}

// ==================== Moteur FormatMessage ====================

pub(crate) const FORMAT_MESSAGE_ALLOCATE_BUFFER: u32 = 0x0100;
pub(crate) const FORMAT_MESSAGE_IGNORE_INSERTS: u32 = 0x0200;
pub(crate) const FORMAT_MESSAGE_FROM_STRING: u32 = 0x0400;
pub(crate) const FORMAT_MESSAGE_FROM_HMODULE: u32 = 0x0800;
pub(crate) const FORMAT_MESSAGE_FROM_SYSTEM: u32 = 0x1000;
pub(crate) const FORMAT_MESSAGE_ARGUMENT_ARRAY: u32 = 0x2000;
pub(crate) const FORMAT_MESSAGE_MAX_WIDTH_MASK: u32 = 0x00FF;
const ERROR_RESOURCE_LANG_NOT_FOUND: u32 = 15100;

/// Source des arguments d'insertion (`va_list*` ou tableau `DWORD_PTR`), indexés à partir de 1.
#[derive(Clone, Copy)]
pub(crate) struct Args {
    base: *const u64,
}

impl Args {
    fn get(&self, n: usize) -> Option<u64> {
        if self.base.is_null() || n == 0 {
            return None;
        }
        // SAFETY: l'invité fournit au moins `n` emplacements de 8 octets (contrat FormatMessage).
        Some(unsafe { self.base.add(n - 1).read_unaligned() })
    }
}

fn read_ansi(p: u64) -> String {
    if p == 0 {
        return "(null)".to_string();
    }
    // SAFETY: chaîne C terminée par nul fournie en argument d'insertion.
    unsafe { CStr::from_ptr(p as usize as *const i8) }
        .to_string_lossy()
        .into_owned()
}

fn read_wide(p: u64) -> String {
    // SAFETY: chaîne UTF-16 terminée par nul fournie en argument d'insertion.
    unsafe { sb_common::wide::from_pcwstr(p as usize as *const u16) }
        .unwrap_or_else(|| "(null)".to_string())
}

/// Applique une spécification printf (`spec` = contenu de `!...!`) à l'insert `n`.
fn format_insert(spec: &str, n: usize, args: Args, wide: bool) -> Result<String, u32> {
    let b: Vec<char> = spec.chars().collect();
    let mut i = 0;
    let mut left = false;
    let mut plus = false;
    let mut space = false;
    let mut alt = false;
    let mut zero = false;
    while i < b.len() && "-+ #0".contains(b[i]) {
        match b[i] {
            '-' => left = true,
            '+' => plus = true,
            ' ' => space = true,
            '#' => alt = true,
            _ => zero = true,
        }
        i += 1;
    }
    let mut cursor = n;
    let take = |c: &mut usize| -> Result<u64, u32> {
        let v = args.get(*c).ok_or(ERROR_INVALID_PARAMETER)?;
        *c += 1;
        Ok(v)
    };
    let mut width = 0usize;
    if b.get(i) == Some(&'*') {
        let w = take(&mut cursor)? as i32;
        if w < 0 {
            left = true;
        }
        width = w.unsigned_abs() as usize;
        i += 1;
    } else {
        while i < b.len() && b[i].is_ascii_digit() {
            width = width * 10 + (b[i] as usize - '0' as usize);
            i += 1;
        }
    }
    let mut precision: Option<usize> = None;
    if b.get(i) == Some(&'.') {
        i += 1;
        if b.get(i) == Some(&'*') {
            precision = Some((take(&mut cursor)? as i32).max(0) as usize);
            i += 1;
        } else {
            let mut p = 0usize;
            while i < b.len() && b[i].is_ascii_digit() {
                p = p * 10 + (b[i] as usize - '0' as usize);
                i += 1;
            }
            precision = Some(p);
        }
    }
    // Modificateurs de taille.
    let rest: String = b[i..].iter().collect();
    let (bits, narrow, widech, conv) = {
        let (bits, narrow, widech, tail) = if let Some(t) = rest.strip_prefix("I64") {
            (64, false, false, t)
        } else if let Some(t) = rest.strip_prefix("I32") {
            (32, false, false, t)
        } else if let Some(t) = rest.strip_prefix("ll") {
            (64, false, false, t)
        } else if let Some(t) = rest.strip_prefix('I').or_else(|| rest.strip_prefix('z')) {
            (64, false, false, t)
        } else if let Some(t) = rest.strip_prefix('h') {
            (16, true, false, t)
        } else if let Some(t) = rest.strip_prefix('l').or_else(|| rest.strip_prefix('w')) {
            (32, false, true, t)
        } else {
            (32, false, false, rest.as_str())
        };
        (bits, narrow, widech, tail.chars().next().unwrap_or('s'))
    };
    let value = take(&mut cursor)?;
    let signed = |v: u64| -> i64 {
        match bits {
            16 => i64::from(v as i16),
            32 => i64::from(v as i32),
            _ => v as i64,
        }
    };
    let unsigned = |v: u64| -> u64 {
        match bits {
            16 => u64::from(v as u16),
            32 => u64::from(v as u32),
            _ => v,
        }
    };
    let mut prefix = String::new();
    let mut body = match conv {
        'd' | 'i' => {
            let v = signed(value);
            if v < 0 {
                prefix.push('-');
            } else if plus {
                prefix.push('+');
            } else if space {
                prefix.push(' ');
            }
            v.unsigned_abs().to_string()
        }
        'u' => unsigned(value).to_string(),
        'x' => {
            if alt && unsigned(value) != 0 {
                prefix.push_str("0x");
            }
            format!("{:x}", unsigned(value))
        }
        'X' => {
            if alt && unsigned(value) != 0 {
                prefix.push_str("0X");
            }
            format!("{:X}", unsigned(value))
        }
        'o' => {
            if alt {
                prefix.push('0');
            }
            format!("{:o}", unsigned(value))
        }
        'p' => format!("{value:016X}"),
        'c' | 'C' => char::from_u32(value as u32 & 0xFFFF)
            .unwrap_or('\u{FFFD}')
            .to_string(),
        's' | 'S' => {
            // `s` suit la variante (A/W), `S` l'inverse ; `h`/`l`/`w` forcent la largeur.
            let is_wide = if narrow {
                false
            } else if widech {
                true
            } else {
                (conv == 's') == wide
            };
            let s = if is_wide {
                read_wide(value)
            } else {
                read_ansi(value)
            };
            match precision {
                Some(p) => s.chars().take(p).collect(),
                None => s,
            }
        }
        _ => return Err(ERROR_INVALID_PARAMETER),
    };
    if matches!(conv, 'd' | 'i' | 'u' | 'x' | 'X' | 'o')
        && let Some(p) = precision
        && body.len() < p
    {
        body = format!("{}{body}", "0".repeat(p - body.len()));
    }
    let len = prefix.chars().count() + body.chars().count();
    if len >= width {
        return Ok(prefix + &body);
    }
    let pad = width - len;
    Ok(if left {
        format!("{prefix}{body}{}", " ".repeat(pad))
    } else if zero && precision.is_none() && !matches!(conv, 's' | 'S' | 'c' | 'C') {
        format!("{prefix}{}{body}", "0".repeat(pad))
    } else {
        format!("{}{prefix}{body}", " ".repeat(pad))
    })
}

/// Coupe les lignes à `width` caractères (retour à la ligne aux espaces).
fn wrap_lines(text: &str, width: usize) -> String {
    let mut out = String::new();
    for (li, line) in text.split("\r\n").enumerate() {
        if li > 0 {
            out.push_str("\r\n");
        }
        let mut cur = 0usize;
        for (wi, word) in line.split(' ').enumerate() {
            let wl = word.chars().count();
            if wi > 0 {
                if cur + 1 + wl > width && cur > 0 {
                    out.push_str("\r\n");
                    cur = 0;
                } else {
                    out.push(' ');
                    cur += 1;
                }
            }
            out.push_str(word);
            cur += wl;
        }
    }
    out
}

/// Expanse une définition de message (inserts, séquences d'échappement, largeur de ligne).
pub(crate) fn expand_message(
    text: &str,
    flags: u32,
    args: Option<Args>,
    wide: bool,
) -> Result<String, u32> {
    let width = flags & FORMAT_MESSAGE_MAX_WIDTH_MASK;
    let ignore = (flags & FORMAT_MESSAGE_IGNORE_INSERTS) != 0;
    let c: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch == '%' && i + 1 < c.len() {
            let next = c[i + 1];
            if next.is_ascii_digit() && next != '0' {
                // %n ou %nn, suivi éventuellement de !format!.
                let start = i;
                let mut j = i + 1;
                let mut n = 0usize;
                while j < c.len() && c[j].is_ascii_digit() && j < i + 3 {
                    n = n * 10 + (c[j] as usize - '0' as usize);
                    j += 1;
                }
                let mut spec = "s".to_string();
                if j < c.len()
                    && c[j] == '!'
                    && let Some(end) = c[j + 1..].iter().position(|&x| x == '!')
                {
                    spec = c[j + 1..j + 1 + end].iter().collect();
                    j = j + 2 + end;
                }
                if ignore {
                    out.extend(&c[start..j]);
                } else {
                    let a = args.ok_or(ERROR_INVALID_PARAMETER)?;
                    out.push_str(&format_insert(&spec, n, a, wide)?);
                }
                i = j;
                continue;
            }
            i += 2;
            match next {
                '0' => break,
                'n' => out.push_str("\r\n"),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'b' => out.push(' '),
                _ => out.push(next), // %% %! %. %espace ...
            }
            continue;
        }
        if width != 0 && (ch == '\r' || ch == '\n') {
            // Les retours à la ligne « ordinaires » de la définition sont ignorés.
            if ch == '\r' && c.get(i + 1) == Some(&'\n') {
                i += 1;
            }
            out.push(' ');
            i += 1;
            continue;
        }
        out.push(ch);
        i += 1;
    }
    if width != 0 && width != FORMAT_MESSAGE_MAX_WIDTH_MASK {
        out = wrap_lines(&out, width as usize);
    }
    Ok(out)
}

/// Résout le texte source d'un appel FormatMessage (chaîne fournie ou table système).
fn message_source(
    flags: u32,
    source: *const c_void,
    msg_id: u32,
    lang: u32,
    wide: bool,
) -> Result<String, u32> {
    if (flags & FORMAT_MESSAGE_FROM_STRING) != 0 {
        if source.is_null() {
            return Err(ERROR_INVALID_PARAMETER);
        }
        return Ok(if wide {
            read_wide(source as u64)
        } else {
            read_ansi(source as u64)
        });
    }
    if (flags & (FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_FROM_HMODULE)) == 0 {
        return Err(ERROR_INVALID_PARAMETER);
    }
    // Les tables de messages des modules (`RT_MESSAGETABLE`) ne sont pas lues : seul le
    // repli système s'applique.
    if (flags & FORMAT_MESSAGE_FROM_SYSTEM) == 0 {
        return Err(ERROR_MR_MID_NOT_FOUND);
    }
    // Langue : neutre/défaut ou anglais uniquement.
    if lang != 0 && (lang & 0x3FF) != 0 && (lang & 0x3FF) != 0x09 {
        return Err(ERROR_RESOURCE_LANG_NOT_FOUND);
    }
    system_message(msg_id)
        .map(str::to_string)
        .ok_or(ERROR_MR_MID_NOT_FOUND)
}

#[allow(clippy::too_many_arguments)]
fn format_message_common(
    flags: u32,
    source: *const c_void,
    msg_id: u32,
    lang: u32,
    buffer: *mut c_void,
    size: u32,
    args: *const c_void,
    wide: bool,
) -> u32 {
    let a = if args.is_null() {
        None
    } else if (flags & FORMAT_MESSAGE_ARGUMENT_ARRAY) != 0 {
        Some(Args { base: args.cast() })
    } else {
        // SAFETY: `args` est un `va_list*` Win64, c.-à-d. un pointeur vers `char*`.
        Some(Args {
            base: unsafe { args.cast::<*const u64>().read_unaligned() },
        })
    };
    let text = match message_source(flags, source, msg_id, lang, wide)
        .and_then(|src| expand_message(&src, flags, a, wide))
    {
        Ok(t) => t,
        Err(code) => {
            set_last_error(code);
            return 0;
        }
    };
    if buffer.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let units: Vec<u16>;
    let (src_ptr, count, unit) = if wide {
        units = text.encode_utf16().collect();
        (units.as_ptr().cast::<u8>(), units.len(), 2usize)
    } else {
        (text.as_ptr(), text.len(), 1usize)
    };
    let dst: *mut u8 = if (flags & FORMAT_MESSAGE_ALLOCATE_BUFFER) != 0 {
        let chars = count.max(size as usize) + 1;
        let p = local_alloc(LMEM_ZEROINIT, chars * unit).cast::<u8>();
        if p.is_null() {
            return 0;
        }
        write_out(buffer.cast::<*mut u8>(), p);
        p
    } else {
        if (size as usize) < count + 1 {
            set_last_error(ERROR_INSUFFICIENT_BUFFER);
            return 0;
        }
        buffer.cast()
    };
    // SAFETY: `dst` a une capacité d'au moins `count + 1` unités.
    unsafe {
        ptr::copy_nonoverlapping(src_ptr, dst, count * unit);
        ptr::write_bytes(dst.add(count * unit), 0, unit);
    }
    set_last_error(0);
    count as u32
}

#[allow(clippy::too_many_arguments)]
pub(super) extern "win64" fn FormatMessageW(
    flags: u32,
    source: *const c_void,
    msg_id: u32,
    lang: u32,
    buffer: *mut u16,
    size: u32,
    args: *const c_void,
) -> u32 {
    ffi_guard!(CHANNEL, "FormatMessageW", 0, {
        format_message_common(flags, source, msg_id, lang, buffer.cast(), size, args, true)
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) extern "win64" fn FormatMessageA(
    flags: u32,
    source: *const c_void,
    msg_id: u32,
    lang: u32,
    buffer: *mut u8,
    size: u32,
    args: *const c_void,
) -> u32 {
    ffi_guard!(CHANNEL, "FormatMessageA", 0, {
        format_message_common(
            flags,
            source,
            msg_id,
            lang,
            buffer.cast(),
            size,
            args,
            false,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_common::wide::to_wide_null;
    use sb_loader::last_error;

    fn fmt_w(flags: u32, src: Option<&str>, id: u32, args: &[u64]) -> (u32, String) {
        let s = src.map(to_wide_null);
        let sp = s.as_ref().map_or(ptr::null(), |v| v.as_ptr().cast());
        let mut buf = [0u16; 512];
        let n = FormatMessageW(
            flags | FORMAT_MESSAGE_ARGUMENT_ARRAY,
            sp,
            id,
            0,
            buf.as_mut_ptr(),
            512,
            if args.is_empty() {
                ptr::null()
            } else {
                args.as_ptr().cast()
            },
        );
        (n, String::from_utf16_lossy(&buf[..n as usize]))
    }

    #[test]
    fn system_messages() {
        super::super::init_test_thread();
        let (n, s) = fmt_w(FORMAT_MESSAGE_FROM_SYSTEM, None, 2, &[]);
        assert_eq!(s, "The system cannot find the file specified.\r\n");
        assert_eq!(n as usize, s.len());
        let (_, s) = fmt_w(FORMAT_MESSAGE_FROM_SYSTEM, None, 0x8007_0057, &[]);
        assert_eq!(s, "The parameter is incorrect.\r\n");
        let (_, s) = fmt_w(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_MAX_WIDTH_MASK,
            None,
            5,
            &[],
        );
        assert_eq!(s, "Access is denied. ");
        let (n, _) = fmt_w(FORMAT_MESSAGE_FROM_SYSTEM, None, 0xDEAD, &[]);
        assert_eq!(n, 0);
        assert_eq!(last_error(), ERROR_MR_MID_NOT_FOUND);
        // Insert sans arguments mais IGNORE_INSERTS : copié tel quel.
        let (_, s) = fmt_w(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            None,
            193,
            &[],
        );
        assert_eq!(s, "%1 is not a valid Win32 application.\r\n");
        // Sans IGNORE_INSERTS ni arguments : échec.
        let (n, _) = fmt_w(FORMAT_MESSAGE_FROM_SYSTEM, None, 193, &[]);
        assert_eq!(n, 0);
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        // Langue non anglaise : introuvable.
        let mut buf = [0u16; 64];
        let n = FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM,
            ptr::null(),
            2,
            0x040C,
            buf.as_mut_ptr(),
            64,
            ptr::null(),
        );
        assert_eq!(n, 0);
        assert_eq!(last_error(), ERROR_RESOURCE_LANG_NOT_FOUND);
    }

    #[test]
    fn from_string_inserts_and_escapes() {
        super::super::init_test_thread();
        let name = to_wide_null("world");
        let (_, s) = fmt_w(
            FORMAT_MESSAGE_FROM_STRING,
            Some("Hello %1. %2!d! %3!08X! [%4!-5s!] %%%n%t%0ignored"),
            0,
            &[
                name.as_ptr() as u64,
                (-42i32) as u32 as u64,
                0xBEEF,
                name.as_ptr() as u64,
            ],
        );
        assert_eq!(s, "Hello world. -42 0000BEEF [world] %\r\n\t");
        let (_, s) = fmt_w(
            FORMAT_MESSAGE_FROM_STRING,
            Some("%1!*.*s!|%4!I64u!|%5!c!"),
            0,
            &[6, 3, name.as_ptr() as u64, u64::MAX, u64::from(b'Z')],
        );
        assert_eq!(s, "   wor|18446744073709551615|Z");
        // Largeur max : sauts de ligne de la source ignorés, %n conservé.
        let (_, s) = fmt_w(
            FORMAT_MESSAGE_FROM_STRING | FORMAT_MESSAGE_MAX_WIDTH_MASK,
            Some("a\r\nb%nc"),
            0,
            &[],
        );
        assert_eq!(s, "a b\r\nc");
        let (_, s) = fmt_w(
            FORMAT_MESSAGE_FROM_STRING | 10,
            Some("aaaa bbbb cccc dddd"),
            0,
            &[],
        );
        assert_eq!(s, "aaaa bbbb\r\ncccc dddd");

        // Variante ANSI avec va_list (pointeur vers le tableau d'arguments).
        let arg = c"ansi";
        let slots = [arg.as_ptr() as u64, 7u64];
        let va: *const u64 = slots.as_ptr();
        let mut out = [0u8; 64];
        let n = FormatMessageA(
            FORMAT_MESSAGE_FROM_STRING,
            c"%1-%2!u!".as_ptr().cast(),
            0,
            0,
            out.as_mut_ptr(),
            64,
            (&raw const va).cast(),
        );
        assert_eq!(&out[..n as usize], b"ansi-7");
    }

    #[test]
    fn allocate_buffer_and_small_buffer() {
        super::super::init_test_thread();
        let mut p: *mut u16 = ptr::null_mut();
        let n = FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_ALLOCATE_BUFFER,
            ptr::null(),
            6,
            0,
            (&raw mut p).cast(),
            0,
            ptr::null(),
        );
        assert!(n > 0 && !p.is_null());
        // SAFETY: `p` alloué par FormatMessageW, terminé par nul.
        let s = unsafe { sb_common::wide::from_pcwstr(p) }.expect("texte");
        assert_eq!(s, "The handle is invalid.\r\n");
        assert!(LocalFree(p.cast()).is_null());

        let mut small = [0u16; 4];
        let n = FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM,
            ptr::null(),
            6,
            0,
            small.as_mut_ptr(),
            4,
            ptr::null(),
        );
        assert_eq!(n, 0);
        assert_eq!(last_error(), ERROR_INSUFFICIENT_BUFFER);
    }

    #[test]
    fn local_alloc_family() {
        super::super::init_test_thread();
        let p = LocalAlloc(LMEM_ZEROINIT, 16);
        assert!(!p.is_null());
        // SAFETY: 16 octets alloués à zéro.
        assert!(
            unsafe { std::slice::from_raw_parts(p.cast::<u8>(), 16) }
                .iter()
                .all(|&b| b == 0)
        );
        assert!(LocalSize(p) >= 16);
        assert_eq!(LocalLock(p), p);
        assert_eq!(LocalUnlock(p), 0);
        assert!(LocalReAlloc(p, 1 << 20, 0).is_null());
        assert_eq!(last_error(), ERROR_NOT_ENOUGH_MEMORY);
        let q = LocalReAlloc(p, 1 << 20, LMEM_MOVEABLE | LMEM_ZEROINIT);
        assert!(!q.is_null());
        // SAFETY: le bloc fait au moins 1 Mio, la zone agrandie est mise à zéro.
        assert_eq!(unsafe { *q.cast::<u8>().add((1 << 20) - 1) }, 0);
        assert_eq!(LocalReAlloc(q, 8, LMEM_MODIFY), q);
        assert!(LocalFree(q).is_null());
        assert!(LocalAlloc(0x8000_0000, 4).is_null());
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
    }
}
