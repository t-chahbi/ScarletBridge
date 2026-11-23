//! E/S fichiers 64 bits (> 4 Gio), lectures/écritures concurrentes `OVERLAPPED` (`pread`/`pwrite`),
//! fichiers mappés en mémoire (`CreateFileMappingW` / `MapViewOfFile`), énumération de répertoires
//! (`FindFirstFileW` / `FindNextFileW`) et files IOCP (`CreateIoCompletionPort` / `GetQueuedCompletionStatus`).
//!
//! Tous les chemins passent strictement par le VFS sandboxé (`sb-vfs`), sans jamais exposer `/home`.

#![allow(non_snake_case, unused_variables)]

use core::ffi::c_void;
use core::mem::{offset_of, size_of};
use std::collections::{HashMap, VecDeque};
use std::ffi::CStr;
use std::fs::{File, Metadata, OpenOptions};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use parking_lot::{Condvar, Mutex, RwLock};
use sb_common::abi::{WIN_FALSE, WIN_TRUE, WinBool};
use sb_common::wide::{from_pcwstr, write_wide_buf};
use sb_common::{ffi_guard, sb_trace, sb_warn};
use sb_loader::set_last_error;

use crate::process::{HANDLE_STDERR, HANDLE_STDOUT, process_state};
use crate::sync;

const CHANNEL: &str = "file_io";

pub const INVALID_HANDLE_VALUE: usize = usize::MAX;
const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;

const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_NO_MORE_FILES: u32 = 18;
const ERROR_HANDLE_EOF: u32 = 38;
const ERROR_WAIT_TIMEOUT: u32 = 258;

// ==================== Structs ABI Windows x86-64 ====================

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Overlapped {
    pub internal: usize,
    pub internal_high: usize,
    pub offset: u32,
    pub offset_high: u32,
    pub h_event: usize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Win32FindDataW {
    pub file_attributes: u32,
    pub creation_time: [u32; 2],
    pub last_access_time: [u32; 2],
    pub last_write_time: [u32; 2],
    pub file_size_high: u32,
    pub file_size_low: u32,
    pub reserved0: u32,
    pub reserved1: u32,
    pub file_name: [u16; 260],
    pub alternate_file_name: [u16; 14],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Win32FindDataA {
    pub file_attributes: u32,
    pub creation_time: [u32; 2],
    pub last_access_time: [u32; 2],
    pub last_write_time: [u32; 2],
    pub file_size_high: u32,
    pub file_size_low: u32,
    pub reserved0: u32,
    pub reserved1: u32,
    pub file_name: [u8; 260],
    pub alternate_file_name: [u8; 14],
    pub _pad: [u8; 2],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Win32FileAttributeData {
    pub file_attributes: u32,
    pub creation_time: [u32; 2],
    pub last_access_time: [u32; 2],
    pub last_write_time: [u32; 2],
    pub file_size_high: u32,
    pub file_size_low: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ByHandleFileInformation {
    pub file_attributes: u32,
    pub creation_time: [u32; 2],
    pub last_access_time: [u32; 2],
    pub last_write_time: [u32; 2],
    pub volume_serial_number: u32,
    pub file_size_high: u32,
    pub file_size_low: u32,
    pub number_of_links: u32,
    pub file_index_high: u32,
    pub file_index_low: u32,
}

const _: () = {
    assert!(size_of::<Overlapped>() == 32);
    assert!(offset_of!(Overlapped, offset) == 16);
    assert!(offset_of!(Overlapped, h_event) == 24);
    assert!(size_of::<Win32FindDataW>() == 592);
    assert!(offset_of!(Win32FindDataW, file_name) == 44);
    assert!(size_of::<Win32FindDataA>() == 320);
    assert!(offset_of!(Win32FindDataA, file_name) == 44);
    assert!(size_of::<Win32FileAttributeData>() == 36);
    assert!(size_of::<ByHandleFileInformation>() == 52);
};

// ==================== Tables de handles Fichiers, Mappings, Find & IOCP ====================

struct FileHandleObj {
    file: Arc<File>,
    path: PathBuf,
    cursor: Mutex<u64>,
    iocp: Mutex<Option<(usize, usize)>>, // (iocp_handle, completion_key)
}

struct FileMappingObj {
    file: Option<Arc<File>>,
    size: u64,
    writable: bool,
}

struct MappedView {
    len: usize,
}

struct FindHandleObj {
    entries: Vec<(String, Metadata)>,
    index: usize,
}

struct IocpPacket {
    bytes: u32,
    key: usize,
    overlapped: usize,
}

struct IocpObj {
    queue: Mutex<VecDeque<IocpPacket>>,
    cv: Condvar,
}

static NEXT_IO_HANDLE: AtomicUsize = AtomicUsize::new(0x0004_0000);
static FILE_HANDLES: LazyLock<RwLock<HashMap<usize, Arc<FileHandleObj>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static MAPPING_HANDLES: LazyLock<RwLock<HashMap<usize, Arc<FileMappingObj>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static MAPPED_VIEWS: LazyLock<Mutex<HashMap<usize, MappedView>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static FIND_HANDLES: LazyLock<Mutex<HashMap<usize, FindHandleObj>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static IOCP_HANDLES: LazyLock<RwLock<HashMap<usize, Arc<IocpObj>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

pub(crate) fn close_io_handle(handle: usize) -> bool {
    if FILE_HANDLES.write().remove(&handle).is_some() {
        return true;
    }
    if MAPPING_HANDLES.write().remove(&handle).is_some() {
        return true;
    }
    if IOCP_HANDLES.write().remove(&handle).is_some() {
        return true;
    }
    FIND_HANDLES.lock().remove(&handle).is_some()
}

/// Nombre maximal de chemins distincts signalés en avertissement (évite de noyer le journal).
const MAX_PATH_WARNINGS: usize = 256;

/// Signale en avertissement, une seule fois par couple (opération, chemin), qu'un accès fichier
/// invité a échoué ; le chemin invité et la raison sont indispensables pour comprendre
/// l'arrêt d'un titre qui vérifie l'existence d'un fichier.
pub(crate) fn warn_path_once(op: &str, guest_path: &str, reason: &str) {
    static SEEN: LazyLock<Mutex<std::collections::HashSet<String>>> =
        LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));
    let mut seen = SEEN.lock();
    if seen.len() >= MAX_PATH_WARNINGS {
        return;
    }
    if seen.insert(format!("{op}\0{guest_path}")) {
        sb_warn!(CHANNEL, "{op}('{guest_path}') a échoué : {reason}");
    }
}

pub(crate) fn resolve_vfs_path(guest_path: &str) -> Option<PathBuf> {
    let st = process_state().read();
    if let Some(ref vfs) = st.vfs {
        match vfs.resolve(guest_path) {
            Ok(resolved) => Some(resolved.host_path),
            Err(err) => {
                warn_path_once("VFS", guest_path, &format!("chemin refusé ({err})"));
                None
            }
        }
    } else {
        warn_path_once("VFS", guest_path, "VFS non initialisé");
        None
    }
}

fn metadata_to_filetime(meta: &Metadata) -> [u32; 2] {
    let unix_100ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| (d.as_nanos() / 100) as u64)
        .unwrap_or(0);
    let ft = unix_100ns.saturating_add(116_444_736_000_000_000);
    [ft as u32, (ft >> 32) as u32]
}

fn wildcard_matches(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let s: Vec<char> = name.to_ascii_lowercase().chars().collect();
    if pattern == "*" || pattern == "*.*" {
        return true;
    }
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star_p, mut star_s) = (None, 0usize);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = Some(pi);
            pi += 1;
            star_s = si;
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            star_s += 1;
            si = star_s;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ==================== CreateFile / ReadFile / WriteFile / 64-bit Seek & Size ====================

#[allow(clippy::too_many_arguments)]
pub(crate) extern "win64" fn CreateFileW(
    filename: *const u16,
    desired_access: u32,
    _share_mode: u32,
    _security: *mut c_void,
    creation_disposition: u32,
    _flags_and_attrs: u32,
    _template: usize,
) -> usize {
    ffi_guard!(CHANNEL, "CreateFileW", INVALID_HANDLE_VALUE, {
        // SAFETY: `filename` est un `PCWSTR` valide ou nul.
        let Some(guest) = (unsafe { from_pcwstr(filename) }) else {
            set_last_error(ERROR_FILE_NOT_FOUND);
            return INVALID_HANDLE_VALUE;
        };
        create_file_impl(&guest, desired_access, creation_disposition)
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) extern "win64" fn CreateFileA(
    filename: *const i8,
    desired_access: u32,
    _share_mode: u32,
    _security: *mut c_void,
    creation_disposition: u32,
    _flags_and_attrs: u32,
    _template: usize,
) -> usize {
    ffi_guard!(CHANNEL, "CreateFileA", INVALID_HANDLE_VALUE, {
        if filename.is_null() {
            set_last_error(ERROR_FILE_NOT_FOUND);
            return INVALID_HANDLE_VALUE;
        }
        // SAFETY: `filename` est une chaîne C valide.
        let guest = unsafe { CStr::from_ptr(filename) }.to_string_lossy();
        create_file_impl(&guest, desired_access, creation_disposition)
    })
}

pub(crate) extern "win64" fn CreateFile2(
    filename: *const u16,
    desired_access: u32,
    share_mode: u32,
    creation_disposition: u32,
    _extended_params: *const c_void,
) -> usize {
    CreateFileW(
        filename,
        desired_access,
        share_mode,
        ptr::null_mut(),
        creation_disposition,
        0,
        0,
    )
}

fn create_file_impl(guest_path: &str, desired_access: u32, disposition: u32) -> usize {
    let Some(host_path) = resolve_vfs_path(guest_path) else {
        set_last_error(ERROR_ACCESS_DENIED);
        return INVALID_HANDLE_VALUE;
    };

    let want_write = (desired_access & 0x4000_0000) != 0 || (desired_access & 0x0002) != 0;
    let want_read = (desired_access & 0x8000_0000) != 0 || !want_write;

    let mut opts = OpenOptions::new();
    opts.read(want_read || !want_write);
    opts.write(want_write);

    // Dispositions Win32 : 1=CREATE_NEW, 2=CREATE_ALWAYS, 3=OPEN_EXISTING, 4=OPEN_ALWAYS, 5=TRUNCATE_EXISTING
    match disposition {
        1 => {
            opts.create_new(true).write(true);
        }
        2 => {
            opts.create(true).truncate(true).write(true);
        }
        4 => {
            opts.create(true).write(true);
        }
        5 => {
            opts.truncate(true).write(true);
        }
        _ => {} // OPEN_EXISTING
    }

    if want_write && let Some(parent) = host_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    match opts.open(&host_path) {
        Ok(file) => {
            let handle = NEXT_IO_HANDLE.fetch_add(1, Ordering::AcqRel);
            FILE_HANDLES.write().insert(
                handle,
                Arc::new(FileHandleObj {
                    file: Arc::new(file),
                    path: host_path,
                    cursor: Mutex::new(0),
                    iocp: Mutex::new(None),
                }),
            );
            set_last_error(0);
            sb_trace!(
                CHANNEL,
                "CreateFile('{guest_path}', disp={disposition}) -> {handle:#x}"
            );
            handle
        }
        Err(_) => {
            set_last_error(ERROR_FILE_NOT_FOUND);
            INVALID_HANDLE_VALUE
        }
    }
}

pub(crate) extern "win64" fn ReadFile(
    handle: usize,
    buf: *mut u8,
    bytes_to_read: u32,
    bytes_read: *mut u32,
    overlapped: *mut Overlapped,
) -> WinBool {
    ffi_guard!(CHANNEL, "ReadFile", WIN_FALSE, {
        if buf.is_null() {
            return WIN_FALSE;
        }
        let Some(fh) = FILE_HANDLES.read().get(&handle).cloned() else {
            set_last_error(ERROR_INVALID_HANDLE);
            return WIN_FALSE;
        };
        // SAFETY: `buf` dispose de `bytes_to_read` octets en écriture.
        let dst = unsafe { std::slice::from_raw_parts_mut(buf, bytes_to_read as usize) };

        let n = if !overlapped.is_null() {
            // SAFETY: `overlapped` vérifié non-nul.
            let ov = unsafe { &mut *overlapped };
            let off = (u64::from(ov.offset_high) << 32) | u64::from(ov.offset);
            let read_n = fh.file.read_at(dst, off).unwrap_or(0);
            ov.internal = 0; // STATUS_SUCCESS
            ov.internal_high = read_n;
            if ov.h_event != 0 {
                sync::SetEvent(ov.h_event);
            }
            if let Some((iocp_h, key)) = *fh.iocp.lock() {
                post_iocp_internal(iocp_h, read_n as u32, key, overlapped as usize);
            }
            read_n
        } else {
            let mut cur = fh.cursor.lock();
            let read_n = fh.file.read_at(dst, *cur).unwrap_or(0);
            *cur = cur.saturating_add(read_n as u64);
            read_n
        };

        if !bytes_read.is_null() {
            // SAFETY: `bytes_read` non-nul.
            unsafe {
                *bytes_read = n as u32;
            }
        }
        if n == 0 && bytes_to_read > 0 {
            set_last_error(ERROR_HANDLE_EOF);
        } else {
            set_last_error(0);
        }
        WIN_TRUE
    })
}

pub(crate) extern "win64" fn ReadFileEx(
    handle: usize,
    buf: *mut u8,
    bytes_to_read: u32,
    overlapped: *mut Overlapped,
    completion: Option<unsafe extern "win64" fn(u32, u32, *mut Overlapped)>,
) -> WinBool {
    let mut n = 0u32;
    let ok = ReadFile(handle, buf, bytes_to_read, &mut n, overlapped);
    if ok != 0
        && let Some(cb) = completion
    {
        // SAFETY: `cb` est un `LPOVERLAPPED_COMPLETION_ROUTINE` Win64.
        unsafe {
            cb(0, n, overlapped);
        }
    }
    ok
}

pub(crate) extern "win64" fn WriteFile(
    handle: usize,
    buf: *const u8,
    len: u32,
    written: *mut u32,
    overlapped: *mut Overlapped,
) -> WinBool {
    ffi_guard!(CHANNEL, "WriteFile", WIN_FALSE, {
        if buf.is_null() {
            return WIN_FALSE;
        }
        // SAFETY: `buf` pointe vers `len` octets valides.
        let data = unsafe { std::slice::from_raw_parts(buf, len as usize) };

        if handle == HANDLE_STDOUT || handle == HANDLE_STDERR {
            let ok = if handle == HANDLE_STDERR {
                std::io::stderr().write_all(data).is_ok()
            } else {
                std::io::stdout().write_all(data).is_ok()
            };
            if !written.is_null() {
                // SAFETY: `written` non-nul.
                unsafe {
                    *written = if ok { len } else { 0 };
                }
            }
            return if ok { WIN_TRUE } else { WIN_FALSE };
        }

        let Some(fh) = FILE_HANDLES.read().get(&handle).cloned() else {
            set_last_error(ERROR_INVALID_HANDLE);
            return WIN_FALSE;
        };

        let n = if !overlapped.is_null() {
            // SAFETY: `overlapped` non-nul.
            let ov = unsafe { &mut *overlapped };
            let off = (u64::from(ov.offset_high) << 32) | u64::from(ov.offset);
            let w_n = fh.file.write_at(data, off).unwrap_or(0);
            ov.internal = 0;
            ov.internal_high = w_n;
            if ov.h_event != 0 {
                sync::SetEvent(ov.h_event);
            }
            if let Some((iocp_h, key)) = *fh.iocp.lock() {
                post_iocp_internal(iocp_h, w_n as u32, key, overlapped as usize);
            }
            w_n
        } else {
            let mut cur = fh.cursor.lock();
            let w_n = fh.file.write_at(data, *cur).unwrap_or(0);
            *cur = cur.saturating_add(w_n as u64);
            w_n
        };

        if !written.is_null() {
            // SAFETY: `written` non-nul.
            unsafe {
                *written = n as u32;
            }
        }
        set_last_error(0);
        WIN_TRUE
    })
}

pub(crate) extern "win64" fn GetOverlappedResult(
    _handle: usize,
    overlapped: *mut Overlapped,
    bytes_transferred: *mut u32,
    wait: WinBool,
) -> WinBool {
    if overlapped.is_null() {
        return WIN_FALSE;
    }
    // SAFETY: `overlapped` non-nul.
    let ov = unsafe { &*overlapped };
    if wait != 0 && ov.h_event != 0 {
        let _ = sync::WaitForSingleObject(ov.h_event, sync::INFINITE);
    }
    if !bytes_transferred.is_null() {
        // SAFETY: `bytes_transferred` non-nul.
        unsafe {
            *bytes_transferred = ov.internal_high as u32;
        }
    }
    WIN_TRUE
}

pub(crate) extern "win64" fn GetOverlappedResultEx(
    handle: usize,
    overlapped: *mut Overlapped,
    bytes_transferred: *mut u32,
    _timeout_ms: u32,
    _alertable: WinBool,
) -> WinBool {
    GetOverlappedResult(handle, overlapped, bytes_transferred, WIN_TRUE)
}

pub(crate) extern "win64" fn GetFileSize(handle: usize, out_high: *mut u32) -> u32 {
    let mut size: i64 = 0;
    if GetFileSizeEx(handle, &mut size) == WIN_FALSE {
        return 0xFFFF_FFFF;
    }
    let u = size as u64;
    if !out_high.is_null() {
        // SAFETY: `out_high` non-nul.
        unsafe {
            *out_high = (u >> 32) as u32;
        }
    }
    u as u32
}

pub(crate) extern "win64" fn GetFileSizeEx(handle: usize, out_size: *mut i64) -> WinBool {
    if out_size.is_null() {
        return WIN_FALSE;
    }
    let Some(fh) = FILE_HANDLES.read().get(&handle).cloned() else {
        set_last_error(ERROR_INVALID_HANDLE);
        return WIN_FALSE;
    };
    let Ok(meta) = fh.file.metadata() else {
        return WIN_FALSE;
    };
    // SAFETY: `out_size` vérifié non-nul (`LARGE_INTEGER` 64 bits : fichiers > 4 Gio supportés).
    unsafe {
        *out_size = meta.len() as i64;
    }
    set_last_error(0);
    WIN_TRUE
}

pub(crate) extern "win64" fn SetFilePointerEx(
    handle: usize,
    distance: i64,
    out_new_pos: *mut i64,
    move_method: u32,
) -> WinBool {
    let Some(fh) = FILE_HANDLES.read().get(&handle).cloned() else {
        set_last_error(ERROR_INVALID_HANDLE);
        return WIN_FALSE;
    };
    let mut cur = fh.cursor.lock();
    let base: i64 = match move_method {
        0 => 0,                                                       // FILE_BEGIN
        1 => *cur as i64,                                             // FILE_CURRENT
        2 => fh.file.metadata().map(|m| m.len() as i64).unwrap_or(0), // FILE_END
        _ => return WIN_FALSE,
    };
    let next = base.saturating_add(distance).max(0) as u64;
    *cur = next;
    if !out_new_pos.is_null() {
        // SAFETY: `out_new_pos` non-nul.
        unsafe {
            *out_new_pos = next as i64;
        }
    }
    set_last_error(0);
    WIN_TRUE
}

pub(crate) extern "win64" fn SetFilePointer(
    handle: usize,
    dist_low: i32,
    dist_high: *mut i32,
    move_method: u32,
) -> u32 {
    let high = if dist_high.is_null() {
        0i64
    } else {
        // SAFETY: `dist_high` non-nul.
        i64::from(unsafe { *dist_high })
    };
    let distance = (high << 32) | i64::from(dist_low as u32);
    let mut new_pos = 0i64;
    if SetFilePointerEx(handle, distance, &mut new_pos, move_method) == WIN_FALSE {
        return 0xFFFF_FFFF;
    }
    let u = new_pos as u64;
    if !dist_high.is_null() {
        // SAFETY: `dist_high` non-nul.
        unsafe {
            *dist_high = (u >> 32) as i32;
        }
    }
    u as u32
}

pub(crate) extern "win64" fn SetEndOfFile(handle: usize) -> WinBool {
    let Some(fh) = FILE_HANDLES.read().get(&handle).cloned() else {
        return WIN_FALSE;
    };
    let cur = *fh.cursor.lock();
    if fh.file.set_len(cur).is_ok() {
        WIN_TRUE
    } else {
        WIN_FALSE
    }
}

pub(crate) extern "win64" fn GetFileInformationByHandle(
    handle: usize,
    out_info: *mut ByHandleFileInformation,
) -> WinBool {
    if out_info.is_null() {
        return WIN_FALSE;
    }
    let Some(fh) = FILE_HANDLES.read().get(&handle).cloned() else {
        return WIN_FALSE;
    };
    let Ok(meta) = fh.file.metadata() else {
        return WIN_FALSE;
    };
    let len = meta.len();
    let ft = metadata_to_filetime(&meta);
    let attrs = attributes_from_metadata(&meta);
    // SAFETY: `out_info` non-nul.
    unsafe {
        *out_info = ByHandleFileInformation {
            file_attributes: attrs,
            creation_time: ft,
            last_access_time: ft,
            last_write_time: ft,
            volume_serial_number: 0x5B01_0001,
            file_size_high: (len >> 32) as u32,
            file_size_low: len as u32,
            number_of_links: 1,
            file_index_high: 0,
            file_index_low: handle as u32,
        };
    }
    WIN_TRUE
}

pub(crate) extern "win64" fn GetFileAttributesW(filename: *const u16) -> u32 {
    // SAFETY: `filename` est un `PCWSTR`.
    let Some(guest) = (unsafe { from_pcwstr(filename) }) else {
        return INVALID_FILE_ATTRIBUTES;
    };
    get_file_attrs_impl(&guest)
}

pub(crate) extern "win64" fn GetFileAttributesA(filename: *const i8) -> u32 {
    if filename.is_null() {
        return INVALID_FILE_ATTRIBUTES;
    }
    // SAFETY: `filename` est une chaîne C valide.
    let guest = unsafe { CStr::from_ptr(filename) }.to_string_lossy();
    get_file_attrs_impl(&guest)
}

fn get_file_attrs_impl(guest: &str) -> u32 {
    let Some(host) = resolve_vfs_path(guest) else {
        set_last_error(ERROR_FILE_NOT_FOUND);
        return INVALID_FILE_ATTRIBUTES;
    };
    match std::fs::metadata(&host) {
        Ok(m) => attributes_from_metadata(&m),
        Err(err) => {
            warn_path_once(
                "GetFileAttributes",
                guest,
                &format!("introuvable côté hôte ({err}) : {}", host.display()),
            );
            set_last_error(ERROR_FILE_NOT_FOUND);
            INVALID_FILE_ATTRIBUTES
        }
    }
}

pub(crate) extern "win64" fn GetFileAttributesExW(
    filename: *const u16,
    _info_level: u32,
    out_data: *mut Win32FileAttributeData,
) -> WinBool {
    if out_data.is_null() {
        return WIN_FALSE;
    }
    // SAFETY: `filename` est un `PCWSTR`.
    let Some(guest) = (unsafe { from_pcwstr(filename) }) else {
        return WIN_FALSE;
    };
    let Some(host) = resolve_vfs_path(&guest) else {
        set_last_error(ERROR_FILE_NOT_FOUND);
        return WIN_FALSE;
    };
    let Ok(meta) = std::fs::metadata(&host) else {
        set_last_error(ERROR_FILE_NOT_FOUND);
        return WIN_FALSE;
    };
    let len = meta.len();
    let ft = metadata_to_filetime(&meta);
    let attrs = attributes_from_metadata(&meta);
    // SAFETY: `out_data` non-nul.
    unsafe {
        *out_data = Win32FileAttributeData {
            file_attributes: attrs,
            creation_time: ft,
            last_access_time: ft,
            last_write_time: ft,
            file_size_high: (len >> 32) as u32,
            file_size_low: len as u32,
        };
    }
    WIN_TRUE
}

pub(crate) extern "win64" fn CreateDirectoryW(path: *const u16, _sec: *mut c_void) -> WinBool {
    // SAFETY: `path` est un `PCWSTR`.
    let Some(guest) = (unsafe { from_pcwstr(path) }) else {
        return WIN_FALSE;
    };
    let Some(host) = resolve_vfs_path(&guest) else {
        return WIN_FALSE;
    };
    if std::fs::create_dir_all(&host).is_ok() {
        WIN_TRUE
    } else {
        WIN_FALSE
    }
}

pub(crate) extern "win64" fn DeleteFileW(path: *const u16) -> WinBool {
    // SAFETY: `path` est un `PCWSTR`.
    let Some(guest) = (unsafe { from_pcwstr(path) }) else {
        return WIN_FALSE;
    };
    let Some(host) = resolve_vfs_path(&guest) else {
        return WIN_FALSE;
    };
    if std::fs::remove_file(&host).is_ok() {
        WIN_TRUE
    } else {
        WIN_FALSE
    }
}

pub(crate) extern "win64" fn MoveFileExW(src: *const u16, dst: *const u16, _flags: u32) -> WinBool {
    // SAFETY: `src` et `dst` sont des `PCWSTR`.
    let (Some(g_src), Some(g_dst)) = (unsafe { from_pcwstr(src) }, unsafe { from_pcwstr(dst) })
    else {
        return WIN_FALSE;
    };
    let (Some(h_src), Some(h_dst)) = (resolve_vfs_path(&g_src), resolve_vfs_path(&g_dst)) else {
        return WIN_FALSE;
    };
    if std::fs::rename(&h_src, &h_dst).is_ok() {
        WIN_TRUE
    } else {
        WIN_FALSE
    }
}

pub(crate) extern "win64" fn GetCurrentDirectoryW(buf_len: u32, buf: *mut u16) -> u32 {
    // Retourne toujours le lecteur virtuel `G:\` (jamais `/home`).
    let dir = "G:\\";
    if buf.is_null() || buf_len == 0 {
        return (dir.len() + 1) as u32;
    }
    // SAFETY: `buf` dispose de `buf_len` unités UTF-16.
    let written = unsafe { write_wide_buf(dir, buf, buf_len as usize) };
    written.saturating_sub(1) as u32
}

pub(crate) extern "win64" fn GetFullPathNameW(
    filename: *const u16,
    buf_len: u32,
    buf: *mut u16,
    file_part: *mut *mut u16,
) -> u32 {
    // SAFETY: `filename` est un `PCWSTR`.
    let Some(guest) = (unsafe { from_pcwstr(filename) }) else {
        return 0;
    };
    let normalized = guest.replace('/', "\\");
    let full = if normalized.contains(':') {
        normalized
    } else {
        format!("G:\\{}", normalized.trim_start_matches('\\'))
    };
    if buf.is_null() || buf_len == 0 {
        return (full.encode_utf16().count() + 1) as u32;
    }
    // SAFETY: `buf` dispose de `buf_len` unités UTF-16.
    let written = unsafe { write_wide_buf(&full, buf, buf_len as usize) };
    if !file_part.is_null() {
        let last_sep = full.rfind('\\').map(|p| p + 1).unwrap_or(0);
        // SAFETY: `last_sep < written`.
        unsafe {
            *file_part = buf.add(last_sep.min(written.saturating_sub(1)));
        }
    }
    written.saturating_sub(1) as u32
}

// ==================== Fichiers mappés en mémoire (`CreateFileMappingW` / `MapViewOfFile`) ====================

pub(crate) extern "win64" fn CreateFileMappingW(
    file_handle: usize,
    _attrs: *mut c_void,
    protect: u32,
    max_size_high: u32,
    max_size_low: u32,
    _name: *const u16,
) -> usize {
    ffi_guard!(CHANNEL, "CreateFileMappingW", 0, {
        let requested = (u64::from(max_size_high) << 32) | u64::from(max_size_low);
        let writable = (protect & 0x04) != 0 || (protect & 0x40) != 0; // PAGE_READWRITE / EXECUTE_READWRITE

        let (file_opt, size) = if file_handle == INVALID_HANDLE_VALUE {
            (None, requested.max(4096))
        } else {
            let Some(fh) = FILE_HANDLES.read().get(&file_handle).cloned() else {
                set_last_error(ERROR_INVALID_HANDLE);
                return 0;
            };
            let actual = fh.file.metadata().map(|m| m.len()).unwrap_or(0);
            let final_size = if requested == 0 { actual } else { requested };
            if final_size == 0 {
                return 0;
            }
            if writable && final_size > actual {
                let _ = fh.file.set_len(final_size);
            }
            (Some(Arc::clone(&fh.file)), final_size)
        };

        let handle = NEXT_IO_HANDLE.fetch_add(1, Ordering::AcqRel);
        MAPPING_HANDLES.write().insert(
            handle,
            Arc::new(FileMappingObj {
                file: file_opt,
                size,
                writable,
            }),
        );
        handle
    })
}

pub(crate) extern "win64" fn CreateFileMappingA(
    file_handle: usize,
    attrs: *mut c_void,
    protect: u32,
    max_size_high: u32,
    max_size_low: u32,
    _name: *const i8,
) -> usize {
    CreateFileMappingW(
        file_handle,
        attrs,
        protect,
        max_size_high,
        max_size_low,
        ptr::null(),
    )
}

pub(crate) extern "win64" fn MapViewOfFile(
    mapping_handle: usize,
    desired_access: u32,
    offset_high: u32,
    offset_low: u32,
    bytes_to_map: usize,
) -> *mut c_void {
    ffi_guard!(CHANNEL, "MapViewOfFile", ptr::null_mut(), {
        let Some(mp) = MAPPING_HANDLES.read().get(&mapping_handle).cloned() else {
            set_last_error(ERROR_INVALID_HANDLE);
            return ptr::null_mut();
        };
        let offset = ((u64::from(offset_high) << 32) | u64::from(offset_low)) as i64;
        let map_len = if bytes_to_map == 0 {
            (mp.size.saturating_sub(offset as u64)) as usize
        } else {
            bytes_to_map
        };
        if map_len == 0 {
            return ptr::null_mut();
        }
        let want_write = mp.writable && ((desired_access & 0x02) != 0 || desired_access == 0xF001F);
        let prot = if want_write {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };

        // SAFETY: `mmap` Linux sur fd fichier ouvert ou anonyme `MAP_ANONYMOUS`.
        let ptr_mapped = unsafe {
            if let Some(ref f) = mp.file {
                libc::mmap(
                    ptr::null_mut(),
                    map_len,
                    prot,
                    libc::MAP_SHARED,
                    f.as_raw_fd(),
                    offset,
                )
            } else {
                libc::mmap(
                    ptr::null_mut(),
                    map_len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            }
        };
        if ptr_mapped == libc::MAP_FAILED {
            return ptr::null_mut();
        }
        MAPPED_VIEWS
            .lock()
            .insert(ptr_mapped as usize, MappedView { len: map_len });
        ptr_mapped.cast()
    })
}

pub(crate) extern "win64" fn MapViewOfFileEx(
    mapping_handle: usize,
    desired_access: u32,
    offset_high: u32,
    offset_low: u32,
    bytes_to_map: usize,
    _base_addr: *mut c_void,
) -> *mut c_void {
    MapViewOfFile(
        mapping_handle,
        desired_access,
        offset_high,
        offset_low,
        bytes_to_map,
    )
}

pub(crate) extern "win64" fn UnmapViewOfFile(base_addr: *const c_void) -> WinBool {
    if base_addr.is_null() {
        return WIN_FALSE;
    }
    if let Some(view) = MAPPED_VIEWS.lock().remove(&(base_addr as usize)) {
        // SAFETY: `base_addr` et `view.len` proviennent directement de `MapViewOfFile`.
        unsafe {
            libc::munmap(base_addr as *mut libc::c_void, view.len);
        }
        WIN_TRUE
    } else {
        WIN_FALSE
    }
}

// ==================== Énumération de répertoires (`FindFirstFile*` / `FindNextFile*`) ====================

fn fill_find_data_w(name: &str, meta: &Metadata, out: &mut Win32FindDataW) {
    let len = meta.len();
    let ft = metadata_to_filetime(meta);
    out.file_attributes = attributes_from_metadata(meta);
    out.creation_time = ft;
    out.last_access_time = ft;
    out.last_write_time = ft;
    out.file_size_high = (len >> 32) as u32;
    out.file_size_low = len as u32;
    out.reserved0 = 0;
    out.reserved1 = 0;
    out.file_name = [0u16; 260];
    out.alternate_file_name = [0u16; 14];
    // SAFETY: `out.file_name` a une capacité de 260 unités UTF-16.
    unsafe {
        write_wide_buf(name, out.file_name.as_mut_ptr(), 260);
    }
}

fn fill_find_data_a(name: &str, meta: &Metadata, out: &mut Win32FindDataA) {
    let len = meta.len();
    let ft = metadata_to_filetime(meta);
    out.file_attributes = attributes_from_metadata(meta);
    out.creation_time = ft;
    out.last_access_time = ft;
    out.last_write_time = ft;
    out.file_size_high = (len >> 32) as u32;
    out.file_size_low = len as u32;
    out.reserved0 = 0;
    out.reserved1 = 0;
    out.file_name = [0u8; 260];
    out.alternate_file_name = [0u8; 14];
    let bytes = name.as_bytes();
    let n = bytes.len().min(259);
    out.file_name[..n].copy_from_slice(&bytes[..n]);
}

fn open_find_entries(guest_pattern: &str) -> Option<Vec<(String, Metadata)>> {
    let norm = guest_pattern.replace('\\', "/");
    let (dir_part, pat_part) = match norm.rfind('/') {
        Some(pos) => (&norm[..=pos], &norm[pos + 1..]),
        None => ("", norm.as_str()),
    };
    let dir_query = if dir_part.is_empty() { "G:/" } else { dir_part };
    let host_dir = resolve_vfs_path(dir_query)?;
    let read_dir = std::fs::read_dir(&host_dir).ok()?;
    let mut entries = Vec::new();
    for entry in read_dir.flatten() {
        let fname = entry.file_name().to_string_lossy().into_owned();
        if wildcard_matches(pat_part, &fname)
            && let Ok(meta) = entry.metadata()
        {
            entries.push((fname, meta));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    if entries.is_empty() {
        None
    } else {
        Some(entries)
    }
}

pub(crate) extern "win64" fn FindFirstFileW(
    pattern: *const u16,
    out_data: *mut Win32FindDataW,
) -> usize {
    ffi_guard!(CHANNEL, "FindFirstFileW", INVALID_HANDLE_VALUE, {
        if out_data.is_null() {
            return INVALID_HANDLE_VALUE;
        }
        // SAFETY: `pattern` est un `PCWSTR`.
        let Some(pat) = (unsafe { from_pcwstr(pattern) }) else {
            return INVALID_HANDLE_VALUE;
        };
        let Some(entries) = open_find_entries(&pat) else {
            set_last_error(ERROR_FILE_NOT_FOUND);
            return INVALID_HANDLE_VALUE;
        };
        // SAFETY: `out_data` non-nul.
        unsafe {
            fill_find_data_w(&entries[0].0, &entries[0].1, &mut *out_data);
        }
        let handle = NEXT_IO_HANDLE.fetch_add(1, Ordering::AcqRel);
        FIND_HANDLES
            .lock()
            .insert(handle, FindHandleObj { entries, index: 1 });
        set_last_error(0);
        handle
    })
}

pub(crate) extern "win64" fn FindFirstFileExW(
    pattern: *const u16,
    _info_level: u32,
    out_data: *mut Win32FindDataW,
    _search_op: u32,
    _filter: *mut c_void,
    _flags: u32,
) -> usize {
    FindFirstFileW(pattern, out_data)
}

pub(crate) extern "win64" fn FindNextFileW(
    handle: usize,
    out_data: *mut Win32FindDataW,
) -> WinBool {
    if out_data.is_null() {
        return WIN_FALSE;
    }
    let mut map = FIND_HANDLES.lock();
    let Some(fh) = map.get_mut(&handle) else {
        set_last_error(ERROR_INVALID_HANDLE);
        return WIN_FALSE;
    };
    if fh.index >= fh.entries.len() {
        set_last_error(ERROR_NO_MORE_FILES);
        return WIN_FALSE;
    }
    let (ref name, ref meta) = fh.entries[fh.index];
    // SAFETY: `out_data` non-nul.
    unsafe {
        fill_find_data_w(name, meta, &mut *out_data);
    }
    fh.index += 1;
    set_last_error(0);
    WIN_TRUE
}

pub(crate) extern "win64" fn FindFirstFileA(
    pattern: *const i8,
    out_data: *mut Win32FindDataA,
) -> usize {
    ffi_guard!(CHANNEL, "FindFirstFileA", INVALID_HANDLE_VALUE, {
        if pattern.is_null() || out_data.is_null() {
            return INVALID_HANDLE_VALUE;
        }
        // SAFETY: `pattern` est une chaîne C valide.
        let pat = unsafe { CStr::from_ptr(pattern) }.to_string_lossy();
        let Some(entries) = open_find_entries(&pat) else {
            set_last_error(ERROR_FILE_NOT_FOUND);
            return INVALID_HANDLE_VALUE;
        };
        // SAFETY: `out_data` non-nul.
        unsafe {
            fill_find_data_a(&entries[0].0, &entries[0].1, &mut *out_data);
        }
        let handle = NEXT_IO_HANDLE.fetch_add(1, Ordering::AcqRel);
        FIND_HANDLES
            .lock()
            .insert(handle, FindHandleObj { entries, index: 1 });
        set_last_error(0);
        handle
    })
}

pub(crate) extern "win64" fn FindNextFileA(
    handle: usize,
    out_data: *mut Win32FindDataA,
) -> WinBool {
    if out_data.is_null() {
        return WIN_FALSE;
    }
    let mut map = FIND_HANDLES.lock();
    let Some(fh) = map.get_mut(&handle) else {
        return WIN_FALSE;
    };
    if fh.index >= fh.entries.len() {
        set_last_error(ERROR_NO_MORE_FILES);
        return WIN_FALSE;
    }
    let (ref name, ref meta) = fh.entries[fh.index];
    // SAFETY: `out_data` non-nul.
    unsafe {
        fill_find_data_a(name, meta, &mut *out_data);
    }
    fh.index += 1;
    WIN_TRUE
}

pub(crate) extern "win64" fn FindClose(handle: usize) -> WinBool {
    if FIND_HANDLES.lock().remove(&handle).is_some() {
        WIN_TRUE
    } else {
        WIN_FALSE
    }
}

// ==================== I/O Completion Ports (IOCP) ====================

fn post_iocp_internal(iocp_handle: usize, bytes: u32, key: usize, overlapped: usize) -> bool {
    let Some(port) = IOCP_HANDLES.read().get(&iocp_handle).cloned() else {
        return false;
    };
    port.queue.lock().push_back(IocpPacket {
        bytes,
        key,
        overlapped,
    });
    port.cv.notify_one();
    true
}

pub(crate) extern "win64" fn CreateIoCompletionPort(
    file_handle: usize,
    existing_port: usize,
    completion_key: usize,
    _concurrent_threads: u32,
) -> usize {
    ffi_guard!(CHANNEL, "CreateIoCompletionPort", 0, {
        let port_handle = if existing_port != 0 {
            existing_port
        } else {
            let h = NEXT_IO_HANDLE.fetch_add(1, Ordering::AcqRel);
            IOCP_HANDLES.write().insert(
                h,
                Arc::new(IocpObj {
                    queue: Mutex::new(VecDeque::new()),
                    cv: Condvar::new(),
                }),
            );
            h
        };
        if file_handle != INVALID_HANDLE_VALUE
            && let Some(fh) = FILE_HANDLES.read().get(&file_handle).cloned()
        {
            *fh.iocp.lock() = Some((port_handle, completion_key));
        }
        port_handle
    })
}

pub(crate) extern "win64" fn PostQueuedCompletionStatus(
    iocp_handle: usize,
    bytes: u32,
    completion_key: usize,
    overlapped: *mut Overlapped,
) -> WinBool {
    if post_iocp_internal(iocp_handle, bytes, completion_key, overlapped as usize) {
        WIN_TRUE
    } else {
        WIN_FALSE
    }
}

pub(crate) extern "win64" fn GetQueuedCompletionStatus(
    iocp_handle: usize,
    out_bytes: *mut u32,
    out_key: *mut usize,
    out_overlapped: *mut *mut Overlapped,
    timeout_ms: u32,
) -> WinBool {
    ffi_guard!(CHANNEL, "GetQueuedCompletionStatus", WIN_FALSE, {
        let Some(port) = IOCP_HANDLES.read().get(&iocp_handle).cloned() else {
            set_last_error(ERROR_INVALID_HANDLE);
            return WIN_FALSE;
        };
        let deadline = if timeout_ms == sync::INFINITE {
            None
        } else {
            Some(Instant::now() + Duration::from_millis(u64::from(timeout_ms)))
        };
        let mut q = port.queue.lock();
        while q.is_empty() {
            if let Some(dl) = deadline {
                if Instant::now() >= dl {
                    set_last_error(ERROR_WAIT_TIMEOUT);
                    return WIN_FALSE;
                }
                port.cv.wait_until(&mut q, dl);
            } else {
                port.cv.wait(&mut q);
            }
        }
        let Some(pkt) = q.pop_front() else {
            return WIN_FALSE;
        };
        if !out_bytes.is_null() {
            // SAFETY: non-nul.
            unsafe {
                *out_bytes = pkt.bytes;
            }
        }
        if !out_key.is_null() {
            // SAFETY: non-nul.
            unsafe {
                *out_key = pkt.key;
            }
        }
        if !out_overlapped.is_null() {
            // SAFETY: non-nul.
            unsafe {
                *out_overlapped = pkt.overlapped as *mut Overlapped;
            }
        }
        set_last_error(0);
        WIN_TRUE
    })
}

/// Helper pour `FlushFileBuffers` sur handle fichier ouvert.
pub(crate) fn flush_file_handle(handle: usize) -> bool {
    if let Some(fh) = FILE_HANDLES.read().get(&handle).cloned() {
        let _ = fh.file.sync_all();
        true
    } else {
        false
    }
}

/// Retourne le chemin hôte d'un handle fichier (utilisé pour diagnostics internes uniquement).
#[allow(dead_code)]
pub(crate) fn file_handle_path(handle: usize) -> Option<PathBuf> {
    FILE_HANDLES
        .read()
        .get(&handle)
        .map(|fh| Path::new(&fh.path).to_path_buf())
}

// ==================== Horodatage, attributs, répertoires, espace disque ====================

const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_DIR_NOT_EMPTY: u32 = 145;
const ERROR_DIRECTORY: u32 = 267;
/// Écart FILETIME (1601) ↔ époque Unix, en unités de 100 ns.
const UNIX_TO_FILETIME: i128 = 116_444_736_000_000_000;

/// Attributs Win32 d'après les métadonnées hôte (`READONLY` si le propriétaire ne peut écrire).
/// Attributs Win32 d'une entrée hôte, tels que Windows les rapporte sur NTFS : un fichier ordinaire
/// porte `FILE_ATTRIBUTE_ARCHIVE` (0x20) — `FILE_ATTRIBUTE_NORMAL` (0x80) n'apparaît que lorsqu'aucun
/// autre attribut n'est positionné, donc presque jamais — et un dossier `FILE_ATTRIBUTE_DIRECTORY`.
/// `FILE_ATTRIBUTE_READONLY` s'ajoute si l'entrée n'est pas inscriptible.
pub(crate) fn attributes_from_metadata(meta: &Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    let mut attrs = if meta.is_dir() {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_ARCHIVE
    };
    if (meta.permissions().mode() & 0o200) == 0 {
        attrs |= FILE_ATTRIBUTE_READONLY;
    }
    attrs
}

/// Convertit une erreur d'E/S hôte en code Win32.
pub(crate) fn io_error_to_win32(err: &std::io::Error) -> u32 {
    match err.raw_os_error() {
        Some(libc::ENOENT) => ERROR_FILE_NOT_FOUND,
        Some(libc::EACCES | libc::EPERM) => ERROR_ACCESS_DENIED,
        Some(libc::ENOTEMPTY) => ERROR_DIR_NOT_EMPTY,
        Some(libc::ENOTDIR) => ERROR_DIRECTORY,
        Some(libc::EEXIST) => 183,       // ERROR_ALREADY_EXISTS
        Some(libc::ENOSPC) => 112,       // ERROR_DISK_FULL
        Some(libc::EROFS) => 19,         // ERROR_WRITE_PROTECT
        Some(libc::EBUSY) => 170,        // ERROR_BUSY
        Some(libc::ENAMETOOLONG) => 206, // ERROR_FILENAME_EXCED_RANGE
        Some(libc::EBADF) => ERROR_INVALID_HANDLE,
        _ => 31, // ERROR_GEN_FAILURE
    }
}

fn system_time_to_filetime(t: std::io::Result<std::time::SystemTime>) -> Option<u64> {
    let t = t.ok()?;
    let ns: i128 = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    };
    u64::try_from(ns / 100 + UNIX_TO_FILETIME).ok()
}

fn filetime_to_timespec(ft: *const u64) -> libc::timespec {
    let omit = libc::timespec {
        tv_sec: 0,
        tv_nsec: libc::UTIME_OMIT,
    };
    if ft.is_null() {
        return omit;
    }
    // SAFETY: `ft` non-nul pointe vers un `FILETIME` (8 octets, éventuellement non aligné).
    let v = unsafe { ft.read_unaligned() };
    if v == 0 || v >= 0xFFFF_FFFF_FFFF_FFFE {
        return omit;
    }
    let unix_100ns = i128::from(v) - UNIX_TO_FILETIME;
    libc::timespec {
        tv_sec: unix_100ns.div_euclid(10_000_000) as libc::time_t,
        tv_nsec: (unix_100ns.rem_euclid(10_000_000) * 100) as libc::c_long,
    }
}

pub(crate) extern "win64" fn GetFileTime(
    handle: usize,
    creation: *mut u64,
    access: *mut u64,
    write: *mut u64,
) -> WinBool {
    ffi_guard!(CHANNEL, "GetFileTime", WIN_FALSE, {
        let Some(fh) = FILE_HANDLES.read().get(&handle).cloned() else {
            set_last_error(ERROR_INVALID_HANDLE);
            return WIN_FALSE;
        };
        let meta = match fh.file.metadata() {
            Ok(m) => m,
            Err(e) => {
                set_last_error(io_error_to_win32(&e));
                return WIN_FALSE;
            }
        };
        let mtime = system_time_to_filetime(meta.modified()).unwrap_or(0);
        let atime = system_time_to_filetime(meta.accessed()).unwrap_or(mtime);
        let ctime = system_time_to_filetime(meta.created()).unwrap_or(mtime);
        for (out, v) in [(creation, ctime), (access, atime), (write, mtime)] {
            if !out.is_null() {
                // SAFETY: pointeur de sortie `FILETIME` non-nul fourni par l'invité.
                unsafe { out.write_unaligned(v) };
            }
        }
        WIN_TRUE
    })
}

pub(crate) extern "win64" fn SetFileTime(
    handle: usize,
    creation: *const u64,
    access: *const u64,
    write: *const u64,
) -> WinBool {
    ffi_guard!(CHANNEL, "SetFileTime", WIN_FALSE, {
        let Some(fh) = FILE_HANDLES.read().get(&handle).cloned() else {
            set_last_error(ERROR_INVALID_HANDLE);
            return WIN_FALSE;
        };
        // La date de création n'est pas modifiable sous Linux : elle est ignorée.
        let times = [filetime_to_timespec(access), filetime_to_timespec(write)];
        // SAFETY: descripteur ouvert possédé par `fh`, tableau de 2 `timespec`.
        let rc = unsafe { libc::futimens(fh.file.as_raw_fd(), times.as_ptr()) };
        if rc != 0 {
            set_last_error(io_error_to_win32(&std::io::Error::last_os_error()));
            return WIN_FALSE;
        }
        WIN_TRUE
    })
}

/// Erreur « introuvable » Win32 : `ERROR_PATH_NOT_FOUND` si le parent manque aussi.
fn not_found_code(host: &Path) -> u32 {
    match host.parent() {
        Some(p) if !p.exists() => ERROR_PATH_NOT_FOUND,
        _ => ERROR_FILE_NOT_FOUND,
    }
}

pub(crate) extern "win64" fn SetFileAttributesW(path: *const u16, attrs: u32) -> WinBool {
    ffi_guard!(CHANNEL, "SetFileAttributesW", WIN_FALSE, {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `path` est un `PCWSTR` valide ou nul.
        let Some(guest) = (unsafe { from_pcwstr(path) }) else {
            set_last_error(ERROR_INVALID_PARAMETER);
            return WIN_FALSE;
        };
        let Some(host) = resolve_vfs_path(&guest) else {
            set_last_error(ERROR_ACCESS_DENIED);
            return WIN_FALSE;
        };
        let meta = match std::fs::metadata(&host) {
            Ok(m) => m,
            Err(_) => {
                set_last_error(not_found_code(&host));
                return WIN_FALSE;
            }
        };
        // Seul `FILE_ATTRIBUTE_READONLY` a un équivalent POSIX ; HIDDEN/SYSTEM/ARCHIVE sont
        // acceptés sans effet (pas de métadonnées étendues dans le VFS).
        let mut perms = meta.permissions();
        let mode = perms.mode();
        let new_mode = if (attrs & FILE_ATTRIBUTE_READONLY) != 0 && !meta.is_dir() {
            mode & !0o222
        } else {
            mode | 0o200
        };
        if new_mode != mode {
            perms.set_mode(new_mode);
            if let Err(e) = std::fs::set_permissions(&host, perms) {
                set_last_error(io_error_to_win32(&e));
                return WIN_FALSE;
            }
        }
        WIN_TRUE
    })
}

pub(crate) extern "win64" fn RemoveDirectoryW(path: *const u16) -> WinBool {
    ffi_guard!(CHANNEL, "RemoveDirectoryW", WIN_FALSE, {
        // SAFETY: `path` est un `PCWSTR` valide ou nul.
        let Some(guest) = (unsafe { from_pcwstr(path) }) else {
            set_last_error(ERROR_PATH_NOT_FOUND);
            return WIN_FALSE;
        };
        let st = process_state().read();
        let Some(vfs) = st.vfs.as_ref() else {
            set_last_error(ERROR_ACCESS_DENIED);
            return WIN_FALSE;
        };
        let resolved = match vfs.resolve(&guest) {
            Ok(r) => r,
            Err(_) => {
                set_last_error(ERROR_ACCESS_DENIED);
                return WIN_FALSE;
            }
        };
        // Une racine de montage (G:\, S:\, T:\) ne peut jamais être supprimée.
        if resolved.relative_path.as_os_str().is_empty() {
            set_last_error(ERROR_ACCESS_DENIED);
            return WIN_FALSE;
        }
        let host = resolved.host_path;
        drop(st);
        match std::fs::symlink_metadata(&host) {
            Err(_) => {
                set_last_error(not_found_code(&host));
                return WIN_FALSE;
            }
            Ok(m) if !m.is_dir() => {
                set_last_error(ERROR_DIRECTORY);
                return WIN_FALSE;
            }
            Ok(_) => {}
        }
        match std::fs::remove_dir(&host) {
            Ok(()) => WIN_TRUE,
            Err(e) => {
                set_last_error(io_error_to_win32(&e));
                WIN_FALSE
            }
        }
    })
}

/// Espace disque `(libre pour l'appelant, total, libre)` du volume VFS contenant `guest`.
/// Le montage `save:` est borné par son quota `PersistentLocalStorage` s'il est défini.
pub(crate) fn disk_free_space(guest: &str) -> Result<(u64, u64, u64), u32> {
    let st = process_state().read();
    let vfs = st.vfs.as_ref().ok_or(ERROR_PATH_NOT_FOUND)?;
    let resolved = vfs.resolve(guest).map_err(|e| match e {
        sb_vfs::vfs::VfsError::EscapeAttempt(_) | sb_vfs::vfs::VfsError::UnsupportedPrefix(_) => {
            ERROR_ACCESS_DENIED
        }
        _ => ERROR_PATH_NOT_FOUND,
    })?;
    let meta = std::fs::metadata(&resolved.host_path).map_err(|_| ERROR_PATH_NOT_FOUND)?;
    if !meta.is_dir() {
        return Err(ERROR_DIRECTORY);
    }
    let c_path = std::ffi::CString::new(resolved.host_path.as_os_str().as_encoded_bytes())
        .map_err(|_| ERROR_PATH_NOT_FOUND)?;
    // SAFETY: `statvfs` remplit une struct locale à partir d'un chemin C valide.
    let sv = unsafe {
        let mut sv: libc::statvfs = core::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut sv) != 0 {
            return Err(ERROR_PATH_NOT_FOUND);
        }
        sv
    };
    let frsize = sv.f_frsize as u64;
    let mut avail = sv.f_bavail as u64 * frsize;
    let mut total = sv.f_blocks as u64 * frsize;
    let mut free = sv.f_bfree as u64 * frsize;
    if resolved.mount == sb_vfs::vfs::VfsMount::Save
        && let Some(quota) = vfs.save_quota_bytes()
    {
        let used = vfs.save_usage_bytes().unwrap_or(0);
        let left = quota.saturating_sub(used);
        total = total.min(quota);
        avail = avail.min(left);
        free = free.min(left);
    }
    Ok((avail, total, free))
}

pub(crate) extern "win64" fn GetDiskFreeSpaceExW(
    dir: *const u16,
    avail_to_caller: *mut u64,
    total: *mut u64,
    total_free: *mut u64,
) -> WinBool {
    ffi_guard!(CHANNEL, "GetDiskFreeSpaceExW", WIN_FALSE, {
        // SAFETY: `dir` est un `PCWSTR` valide ou nul (NULL = répertoire courant `G:\`).
        let guest = unsafe { from_pcwstr(dir) }.unwrap_or_else(|| "G:\\".to_string());
        match disk_free_space(&guest) {
            Ok((a, t, f)) => {
                for (out, v) in [(avail_to_caller, a), (total, t), (total_free, f)] {
                    if !out.is_null() {
                        // SAFETY: pointeur de sortie `ULARGE_INTEGER` non-nul.
                        unsafe { out.write_unaligned(v) };
                    }
                }
                WIN_TRUE
            }
            Err(code) => {
                set_last_error(code);
                WIN_FALSE
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
    use sb_common::wide::to_wide_null;
    use sb_loader::last_error;

    /// Installe un VFS de test si aucun n'est configuré ; les tests utilisent `T:\`.
    fn ensure_vfs() {
        let _ = sb_loader::init_thread_teb(0);
        let mut st = process_state().write();
        if st.vfs.is_none() {
            let root = std::env::temp_dir().join(format!("sb-agentd-vfs-{}", std::process::id()));
            let game = root.join("game");
            std::fs::create_dir_all(&game).expect("game root");
            st.vfs = Some(
                sb_vfs::VirtualFs::new(&game, &root.join("save"), &root.join("temp"), Some(1))
                    .expect("vfs"),
            );
        }
    }

    #[test]
    fn file_times_attributes_directories_and_disk_space() {
        ensure_vfs();
        let dir = to_wide_null("T:\\agentd_dir");
        let file = to_wide_null("T:\\agentd_dir\\f.bin");
        let _ = RemoveDirectoryW(dir.as_ptr());
        assert_eq!(CreateDirectoryW(dir.as_ptr(), ptr::null_mut()), WIN_TRUE);
        let h = CreateFileW(file.as_ptr(), 0xC000_0000, 0, ptr::null_mut(), 2, 0, 0);
        assert_ne!(h, INVALID_HANDLE_VALUE);

        // SetFileTime / GetFileTime aller-retour (2020-01-01 00:00:00 UTC).
        let ft: u64 = 132_223_104_000_000_000;
        assert_eq!(SetFileTime(h, ptr::null(), &ft, &ft), WIN_TRUE);
        let (mut c, mut a, mut w) = (0u64, 0u64, 0u64);
        assert_eq!(GetFileTime(h, &mut c, &mut a, &mut w), WIN_TRUE);
        assert_eq!((a, w), (ft, ft));
        assert!(c > 0);
        assert_eq!(GetFileTime(0xDEAD, &mut c, &mut a, &mut w), WIN_FALSE);
        assert_eq!(last_error(), ERROR_INVALID_HANDLE);
        close_io_handle(h);

        // Attribut lecture seule.
        assert_eq!(
            SetFileAttributesW(file.as_ptr(), FILE_ATTRIBUTE_READONLY),
            WIN_TRUE
        );
        assert_eq!(
            GetFileAttributesW(file.as_ptr()) & FILE_ATTRIBUTE_READONLY,
            FILE_ATTRIBUTE_READONLY
        );
        assert_eq!(
            SetFileAttributesW(file.as_ptr(), FILE_ATTRIBUTE_NORMAL),
            WIN_TRUE
        );
        // Retour à un fichier inscriptible : plus de READONLY. (Windows rapporterait NORMAL ici car
        // `SetFileAttributes` efface ARCHIVE ; le bridge ne mémorise pas ce bit et garde ARCHIVE.)
        let attrs = GetFileAttributesW(file.as_ptr());
        assert_ne!(attrs, INVALID_FILE_ATTRIBUTES);
        assert_eq!(attrs & FILE_ATTRIBUTE_READONLY, 0);
        assert_eq!(attrs & FILE_ATTRIBUTE_DIRECTORY, 0);
        let missing = to_wide_null("T:\\nope\\x");
        assert_eq!(SetFileAttributesW(missing.as_ptr(), 0x80), WIN_FALSE);
        assert_eq!(last_error(), ERROR_PATH_NOT_FOUND);

        // RemoveDirectoryW : non vide, pas un répertoire, racine interdite, succès.
        assert_eq!(RemoveDirectoryW(dir.as_ptr()), WIN_FALSE);
        assert_eq!(last_error(), ERROR_DIR_NOT_EMPTY);
        assert_eq!(RemoveDirectoryW(file.as_ptr()), WIN_FALSE);
        assert_eq!(last_error(), ERROR_DIRECTORY);
        let root = to_wide_null("T:\\");
        assert_eq!(RemoveDirectoryW(root.as_ptr()), WIN_FALSE);
        assert_eq!(last_error(), ERROR_ACCESS_DENIED);
        let escape = to_wide_null("T:\\..\\..");
        assert_eq!(RemoveDirectoryW(escape.as_ptr()), WIN_FALSE);
        assert_eq!(DeleteFileW(file.as_ptr()), WIN_TRUE);
        assert_eq!(RemoveDirectoryW(dir.as_ptr()), WIN_TRUE);
        assert_eq!(RemoveDirectoryW(dir.as_ptr()), WIN_FALSE);
        assert_eq!(last_error(), ERROR_FILE_NOT_FOUND);

        // Espace disque.
        let (mut av, mut tot, mut free) = (0u64, 0u64, 0u64);
        assert_eq!(
            GetDiskFreeSpaceExW(root.as_ptr(), &mut av, &mut tot, &mut free),
            WIN_TRUE
        );
        assert!(tot > 0 && free <= tot && av <= tot);
        assert_eq!(
            GetDiskFreeSpaceExW(missing.as_ptr(), &mut av, &mut tot, &mut free),
            WIN_FALSE
        );
        assert_eq!(last_error(), ERROR_PATH_NOT_FOUND);
    }

    /// Attributs rapportés comme sous Windows/NTFS : ARCHIVE pour un fichier, DIRECTORY pour un
    /// dossier, READONLY en plus si non inscriptible ; jamais NORMAL (0x80) seul.
    #[test]
    fn attributes_follow_ntfs_conventions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("sb_attrs_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dossier de test");
        let file = dir.join("f.bin");
        std::fs::write(&file, b"x").expect("fichier de test");

        let a = attributes_from_metadata(&std::fs::metadata(&file).expect("meta"));
        assert_eq!(a, FILE_ATTRIBUTE_ARCHIVE);
        let d = attributes_from_metadata(&std::fs::metadata(&dir).expect("meta"));
        assert_eq!(d, FILE_ATTRIBUTE_DIRECTORY);

        let mut perm = std::fs::metadata(&file).expect("meta").permissions();
        perm.set_mode(0o444);
        std::fs::set_permissions(&file, perm).expect("chmod");
        let r = attributes_from_metadata(&std::fs::metadata(&file).expect("meta"));
        assert_eq!(r, FILE_ATTRIBUTE_ARCHIVE | FILE_ATTRIBUTE_READONLY);

        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
