//! Débogage / gestion d'erreurs, caractéristiques processeur (CPUID réel), topologie
//! (`/sys/devices/system`), mémoire (`/proc/meminfo`), compteurs de ticks et affinités de thread.

use core::ffi::c_void;
use core::mem::{offset_of, size_of};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use sb_common::abi::{WIN_FALSE, WIN_TRUE, WinBool};
use sb_common::wide::from_pcwstr;
use sb_common::{ffi_guard, sb_debug, sb_err};
use sb_loader::{module_manager, set_last_error};

use super::{
    CHANNEL, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER,
    ERROR_NOACCESS, ansi_to_string, write_out,
};
use crate::process::{HANDLE_STDERR, HANDLE_STDIN, HANDLE_STDOUT, PSEUDO_CURRENT_PROCESS};
use crate::sync;

// ==================== Débogage & gestion d'erreurs ====================

pub(super) extern "win64" fn IsDebuggerPresent() -> WinBool {
    WIN_FALSE
}

pub(super) extern "win64" fn CheckRemoteDebuggerPresent(
    process: usize,
    out_present: *mut WinBool,
) -> WinBool {
    ffi_guard!(CHANNEL, "CheckRemoteDebuggerPresent", WIN_FALSE, {
        if process == 0 || out_present.is_null() {
            set_last_error(ERROR_INVALID_PARAMETER);
            return WIN_FALSE;
        }
        write_out(out_present, WIN_FALSE);
        WIN_TRUE
    })
}

pub(super) extern "win64" fn OutputDebugStringA(text: *const i8) {
    ffi_guard!(CHANNEL, "OutputDebugStringA", (), {
        if let Some(s) = ansi_to_string(text) {
            sb_debug!("debugstr", "{}", s.trim_end_matches(['\r', '\n']));
        }
    })
}

pub(super) extern "win64" fn OutputDebugStringW(text: *const u16) {
    ffi_guard!(CHANNEL, "OutputDebugStringW", (), {
        // SAFETY: `text` est un `PCWSTR` valide ou nul.
        if let Some(s) = unsafe { from_pcwstr(text) } {
            sb_debug!("debugstr", "{}", s.trim_end_matches(['\r', '\n']));
        }
    })
}

/// `EXCEPTION_RECORD` x64 (152 octets).
#[repr(C)]
pub(super) struct ExceptionRecord {
    pub exception_code: u32,
    pub exception_flags: u32,
    pub exception_record: *mut ExceptionRecord,
    pub exception_address: usize,
    pub number_parameters: u32,
    pub exception_information: [usize; 15],
}

/// `EXCEPTION_POINTERS`.
#[repr(C)]
pub(super) struct ExceptionPointers {
    pub exception_record: *mut ExceptionRecord,
    pub context_record: *mut c_void,
}

const _: () = {
    assert!(size_of::<ExceptionRecord>() == 152);
    assert!(offset_of!(ExceptionRecord, exception_address) == 16);
    assert!(offset_of!(ExceptionRecord, number_parameters) == 24);
    assert!(offset_of!(ExceptionRecord, exception_information) == 32);
    assert!(size_of::<ExceptionPointers>() == 16);
};

const EXCEPTION_EXECUTE_HANDLER: i32 = 1;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

static IN_UNHANDLED_FILTER: AtomicBool = AtomicBool::new(false);

pub(super) extern "win64" fn UnhandledExceptionFilter(info: *mut ExceptionPointers) -> i32 {
    ffi_guard!(
        CHANNEL,
        "UnhandledExceptionFilter",
        EXCEPTION_EXECUTE_HANDLER,
        {
            let rec = if info.is_null() {
                core::ptr::null_mut()
            } else {
                // SAFETY: `info` est un `EXCEPTION_POINTERS` fourni par l'invité.
                unsafe { (*info).exception_record }
            };
            if rec.is_null() {
                sb_err!(CHANNEL, "UnhandledExceptionFilter(<sans ExceptionRecord>)");
            } else {
                // SAFETY: `rec` vérifié non-nul.
                let (code, addr, flags) = unsafe {
                    (
                        (*rec).exception_code,
                        (*rec).exception_address,
                        (*rec).exception_flags,
                    )
                };
                let where_ = module_manager()
                    .describe_address(addr)
                    .unwrap_or_else(|| "<hors module PE>".to_string());
                sb_err!(
                    CHANNEL,
                    "exception non gérée code={code:#010x} flags={flags:#x} adresse={addr:#x} ({where_})"
                );
            }
            let filter = crate::kernel32::top_level_exception_filter();
            if filter != 0 && !IN_UNHANDLED_FILTER.swap(true, Ordering::AcqRel) {
                // SAFETY: `filter` est un `LPTOP_LEVEL_EXCEPTION_FILTER` Win64 posé par l'invité.
                let f: unsafe extern "win64" fn(*mut ExceptionPointers) -> i32 =
                    unsafe { core::mem::transmute(filter) };
                // SAFETY: appel du filtre invité avec les pointeurs reçus.
                let ret = unsafe { f(info) };
                IN_UNHANDLED_FILTER.store(false, Ordering::Release);
                if ret != EXCEPTION_CONTINUE_SEARCH {
                    return ret;
                }
            }
            // Aucun débogueur attaché : le processus doit être terminé par l'appelant.
            EXCEPTION_EXECUTE_HANDLER
        }
    )
}

// ==================== IsProcessorFeaturePresent (CPUID réel) ====================

#[allow(unused_unsafe)]
fn cpuid(leaf: u32, sub: u32) -> (u32, u32, u32, u32) {
    // SAFETY: `cpuid` est disponible sur tout processeur x86-64.
    let r = unsafe { core::arch::x86_64::__cpuid_count(leaf, sub) };
    (r.eax, r.ebx, r.ecx, r.edx)
}

fn max_leaf(ext: bool) -> u32 {
    cpuid(if ext { 0x8000_0000 } else { 0 }, 0).0
}

fn hwcap2_fsgsbase() -> bool {
    // SAFETY: `getauxval` sans précondition.
    let caps = unsafe { libc::getauxval(libc::AT_HWCAP2) };
    (caps & 0x2) != 0 // HWCAP2_FSGSBASE
}

pub(super) fn processor_feature(feature: u32) -> bool {
    let (_, _, ecx1, edx1) = cpuid(1, 0);
    let (_, ebx7, ecx7, _) = if max_leaf(false) >= 7 {
        cpuid(7, 0)
    } else {
        (0, 0, 0, 0)
    };
    let (_, _, ecx_e, edx_e) = if max_leaf(true) >= 0x8000_0001 {
        cpuid(0x8000_0001, 0)
    } else {
        (0, 0, 0, 0)
    };
    let bit = |v: u32, b: u32| (v >> b) & 1 == 1;
    match feature {
        2 => bit(edx1, 8),                                 // PF_COMPARE_EXCHANGE_DOUBLE
        3 => bit(edx1, 23),                                // PF_MMX
        6 => std::arch::is_x86_feature_detected!("sse"),   // PF_XMMI
        7 => bit(edx_e, 31),                               // PF_3DNOW
        8 => bit(edx1, 4),                                 // PF_RDTSC
        9 => bit(edx1, 6),                                 // PF_PAE
        10 => std::arch::is_x86_feature_detected!("sse2"), // PF_XMMI64
        12 => bit(edx_e, 20),                              // PF_NX
        13 => std::arch::is_x86_feature_detected!("sse3"), // PF_SSE3
        14 => std::arch::is_x86_feature_detected!("cmpxchg16b"), // PF_COMPARE_EXCHANGE128
        17 => std::arch::is_x86_feature_detected!("xsave") && bit(ecx1, 27), // PF_XSAVE_ENABLED
        22 => bit(ebx7, 0) && hwcap2_fsgsbase(),           // PF_RDWRFSGSBASE
        23 => sb_loader::signal::fastfail_supported(),     // PF_FASTFAIL_AVAILABLE
        28 => std::arch::is_x86_feature_detected!("rdrand"), // PF_RDRAND_INSTRUCTION
        32 => bit(edx_e, 27),                              // PF_RDTSCP
        33 => bit(ecx7, 22),                               // PF_RDPID
        35 => bit(ecx_e, 29),                              // PF_MONITORX
        36 => std::arch::is_x86_feature_detected!("ssse3"), // PF_SSSE3
        37 => std::arch::is_x86_feature_detected!("sse4.1"), // PF_SSE4_1
        38 => std::arch::is_x86_feature_detected!("sse4.2"), // PF_SSE4_2
        39 => std::arch::is_x86_feature_detected!("avx"),  // PF_AVX
        40 => std::arch::is_x86_feature_detected!("avx2"), // PF_AVX2
        41 => std::arch::is_x86_feature_detected!("avx512f"), // PF_AVX512F
        42 => bit(ebx7, 9),                                // PF_ERMS
        _ => false,
    }
}

pub(super) extern "win64" fn IsProcessorFeaturePresent(feature: u32) -> WinBool {
    ffi_guard!(CHANNEL, "IsProcessorFeaturePresent", WIN_FALSE, {
        WinBool::from(processor_feature(feature))
    })
}

// ==================== Thread : pile, startup info, OpenThread, affinités ====================

/// Bornes réelles `[low, high)` de la pile du thread courant.
pub(super) fn current_stack_limits() -> Option<(usize, usize)> {
    // SAFETY: `pthread_getattr_np` remplit un attribut que l'on détruit ensuite.
    unsafe {
        let mut attr: libc::pthread_attr_t = core::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return None;
        }
        let mut addr: *mut libc::c_void = core::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let rc = libc::pthread_attr_getstack(&attr, &mut addr, &mut size);
        libc::pthread_attr_destroy(&mut attr);
        if rc != 0 {
            return None;
        }
        Some((addr as usize, addr as usize + size))
    }
}

pub(super) extern "win64" fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize) {
    ffi_guard!(CHANNEL, "GetCurrentThreadStackLimits", (), {
        let (lo, hi) = current_stack_limits().unwrap_or((0, 0));
        write_out(low, lo);
        write_out(high, hi);
    })
}

/// `STARTUPINFOW` / `STARTUPINFOA` (104 octets, mêmes offsets).
#[repr(C)]
pub(super) struct StartupInfo {
    cb: u32,
    reserved: usize,
    desktop: usize,
    title: usize,
    x: u32,
    y: u32,
    x_size: u32,
    y_size: u32,
    x_count_chars: u32,
    y_count_chars: u32,
    fill_attribute: u32,
    flags: u32,
    show_window: u16,
    cb_reserved2: u16,
    reserved2: usize,
    std_input: usize,
    std_output: usize,
    std_error: usize,
}

const _: () = {
    assert!(size_of::<StartupInfo>() == 104);
    assert!(offset_of!(StartupInfo, flags) == 60);
    assert!(offset_of!(StartupInfo, show_window) == 64);
    assert!(offset_of!(StartupInfo, std_input) == 80);
    assert!(offset_of!(StartupInfo, std_error) == 96);
};

const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
const SW_SHOWNORMAL: u16 = 1;

fn fill_startup_info(out: *mut StartupInfo) {
    write_out(
        out,
        StartupInfo {
            cb: size_of::<StartupInfo>() as u32,
            reserved: 0,
            desktop: 0,
            title: 0,
            x: 0,
            y: 0,
            x_size: 0,
            y_size: 0,
            x_count_chars: 0,
            y_count_chars: 0,
            fill_attribute: 0,
            flags: STARTF_USESTDHANDLES,
            show_window: SW_SHOWNORMAL,
            cb_reserved2: 0,
            reserved2: 0,
            std_input: HANDLE_STDIN,
            std_output: HANDLE_STDOUT,
            std_error: HANDLE_STDERR,
        },
    );
}

pub(super) extern "win64" fn GetStartupInfoW(out: *mut StartupInfo) {
    ffi_guard!(CHANNEL, "GetStartupInfoW", (), { fill_startup_info(out) })
}

pub(super) extern "win64" fn GetStartupInfoA(out: *mut StartupInfo) {
    ffi_guard!(CHANNEL, "GetStartupInfoA", (), { fill_startup_info(out) })
}

pub(super) extern "win64" fn OpenThread(access: u32, inherit: WinBool, tid: u32) -> usize {
    ffi_guard!(CHANNEL, "OpenThread", 0, {
        match sync::open_thread_handle(tid) {
            Some(h) => {
                set_last_error(0);
                h
            }
            None => {
                set_last_error(ERROR_INVALID_PARAMETER);
                0
            }
        }
    })
}

static PRIORITY_BOOST_DISABLED: AtomicBool = AtomicBool::new(false);

fn is_current_process(handle: usize) -> bool {
    // SAFETY: `getpid` sans précondition.
    handle == PSEUDO_CURRENT_PROCESS || handle == unsafe { libc::getpid() } as usize
}

pub(super) extern "win64" fn SetProcessPriorityBoost(process: usize, disable: WinBool) -> WinBool {
    ffi_guard!(CHANNEL, "SetProcessPriorityBoost", WIN_FALSE, {
        if !is_current_process(process) {
            set_last_error(ERROR_INVALID_HANDLE);
            return WIN_FALSE;
        }
        PRIORITY_BOOST_DISABLED.store(disable != 0, Ordering::Release);
        WIN_TRUE
    })
}

pub(super) extern "win64" fn GetProcessPriorityBoost(process: usize, out: *mut WinBool) -> WinBool {
    ffi_guard!(CHANNEL, "GetProcessPriorityBoost", WIN_FALSE, {
        if !is_current_process(process) {
            set_last_error(ERROR_INVALID_HANDLE);
            return WIN_FALSE;
        }
        if out.is_null() {
            set_last_error(ERROR_NOACCESS);
            return WIN_FALSE;
        }
        write_out(
            out,
            WinBool::from(PRIORITY_BOOST_DISABLED.load(Ordering::Acquire)),
        );
        WIN_TRUE
    })
}

/// `GROUP_AFFINITY` (16 octets).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct GroupAffinity {
    pub mask: u64,
    pub group: u16,
    pub reserved: [u16; 3],
}

const _: () = assert!(size_of::<GroupAffinity>() == 16);

/// TID Linux (0 = thread appelant) pour un handle de thread, ou `None` si invalide.
fn linux_tid_for(handle: usize) -> Option<libc::pid_t> {
    if handle == crate::process::PSEUDO_CURRENT_THREAD {
        return Some(0);
    }
    sync::thread_tid_of_handle(handle).map(|t| t as libc::pid_t)
}

fn get_affinity(tid: libc::pid_t) -> Option<u64> {
    // SAFETY: `cpu_set_t` initialisé à zéro puis rempli par le noyau.
    unsafe {
        let mut set: libc::cpu_set_t = core::mem::zeroed();
        if libc::sched_getaffinity(tid, size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            return None;
        }
        let mut mask = 0u64;
        for cpu in 0..64 {
            if libc::CPU_ISSET(cpu, &set) {
                mask |= 1 << cpu;
            }
        }
        Some(mask)
    }
}

fn set_affinity(tid: libc::pid_t, mask: u64) -> bool {
    // SAFETY: `cpu_set_t` local construit à partir du masque.
    unsafe {
        let mut set: libc::cpu_set_t = core::mem::zeroed();
        for cpu in 0..64 {
            if (mask >> cpu) & 1 == 1 {
                libc::CPU_SET(cpu, &mut set);
            }
        }
        libc::sched_setaffinity(tid, size_of::<libc::cpu_set_t>(), &set) == 0
    }
}

/// Masque des processeurs logiques actifs du groupe 0.
pub(super) fn active_processor_mask() -> u64 {
    read_cpu_list(Path::new("/sys/devices/system/cpu/online"))
        .filter(|&m| m != 0)
        .unwrap_or_else(|| {
            let n = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .min(64);
            if n == 64 { u64::MAX } else { (1u64 << n) - 1 }
        })
}

pub(super) extern "win64" fn GetThreadGroupAffinity(
    thread: usize,
    out: *mut GroupAffinity,
) -> WinBool {
    ffi_guard!(CHANNEL, "GetThreadGroupAffinity", WIN_FALSE, {
        let Some(tid) = linux_tid_for(thread) else {
            set_last_error(ERROR_INVALID_HANDLE);
            return WIN_FALSE;
        };
        if out.is_null() {
            set_last_error(ERROR_NOACCESS);
            return WIN_FALSE;
        }
        let Some(mask) = get_affinity(tid) else {
            set_last_error(ERROR_INVALID_HANDLE);
            return WIN_FALSE;
        };
        write_out(
            out,
            GroupAffinity {
                mask,
                group: 0,
                reserved: [0; 3],
            },
        );
        WIN_TRUE
    })
}

pub(super) extern "win64" fn SetThreadGroupAffinity(
    thread: usize,
    affinity: *const GroupAffinity,
    previous: *mut GroupAffinity,
) -> WinBool {
    ffi_guard!(CHANNEL, "SetThreadGroupAffinity", WIN_FALSE, {
        let Some(tid) = linux_tid_for(thread) else {
            set_last_error(ERROR_INVALID_HANDLE);
            return WIN_FALSE;
        };
        if affinity.is_null() {
            set_last_error(ERROR_NOACCESS);
            return WIN_FALSE;
        }
        // SAFETY: `affinity` vérifié non-nul.
        let wanted = unsafe { *affinity };
        let active = active_processor_mask();
        if wanted.group != 0
            || wanted.reserved != [0; 3]
            || wanted.mask == 0
            || (wanted.mask & !active) != 0
        {
            set_last_error(ERROR_INVALID_PARAMETER);
            return WIN_FALSE;
        }
        let prev = get_affinity(tid).unwrap_or(active);
        if !set_affinity(tid, wanted.mask) {
            set_last_error(ERROR_INVALID_PARAMETER);
            return WIN_FALSE;
        }
        write_out(
            previous,
            GroupAffinity {
                mask: prev,
                group: 0,
                reserved: [0; 3],
            },
        );
        WIN_TRUE
    })
}

// ==================== Topologie processeur (/sys/devices/system) ====================

fn read_trim(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Parse une liste de CPU Linux (`0-3,8,10-11`) en masque 64 bits.
pub(super) fn parse_cpu_list(s: &str) -> Option<u64> {
    let mut mask = 0u64;
    for part in s.trim().split(',').filter(|p| !p.is_empty()) {
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (a.trim().parse::<u32>().ok()?, b.trim().parse::<u32>().ok()?),
            None => {
                let v = part.trim().parse::<u32>().ok()?;
                (v, v)
            }
        };
        for cpu in a..=b.min(63) {
            mask |= 1 << cpu;
        }
    }
    Some(mask)
}

fn read_cpu_list(path: &Path) -> Option<u64> {
    parse_cpu_list(&read_trim(path)?)
}

fn parse_size(s: &str) -> u32 {
    let s = s.trim();
    let (num, mul) = if let Some(n) = s.strip_suffix('K') {
        (n, 1024)
    } else if let Some(n) = s.strip_suffix('M') {
        (n, 1024 * 1024)
    } else {
        (s, 1)
    };
    num.parse::<u32>().unwrap_or(0).saturating_mul(mul)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CacheInfo {
    pub level: u8,
    /// 0 = unifié, 1 = instruction, 2 = données (`PROCESSOR_CACHE_TYPE`).
    pub kind: u32,
    pub size: u32,
    pub line: u16,
    pub assoc: u8,
    pub mask: u64,
}

#[derive(Debug, Clone, Default)]
pub(super) struct Topology {
    /// Masques des cœurs physiques (threads SMT frères).
    pub cores: Vec<u64>,
    pub caches: Vec<CacheInfo>,
    pub packages: Vec<u64>,
    /// (numéro de nœud, masque).
    pub numa: Vec<(u32, u64)>,
    pub active: u64,
}

/// Lit la topologie réelle à partir de `root` (normalement `/sys/devices/system`).
pub(super) fn read_topology(root: &Path) -> Topology {
    let cpu_root = root.join("cpu");
    let active = read_cpu_list(&cpu_root.join("online")).unwrap_or(0);
    let mut topo = Topology {
        active,
        ..Topology::default()
    };
    let mut pkg_ids: Vec<(i64, u64)> = Vec::new();
    for cpu in (0..64).filter(|c| (active >> c) & 1 == 1) {
        let dir = cpu_root.join(format!("cpu{cpu}"));
        let tdir = dir.join("topology");
        let siblings = read_cpu_list(&tdir.join("thread_siblings_list"))
            .or_else(|| read_cpu_list(&tdir.join("core_cpus_list")))
            .unwrap_or(1 << cpu)
            & active;
        if !topo.cores.contains(&siblings) {
            topo.cores.push(siblings);
        }
        let pkg = read_trim(&tdir.join("physical_package_id"))
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        match pkg_ids.iter_mut().find(|(id, _)| *id == pkg) {
            Some((_, m)) => *m |= 1 << cpu,
            None => pkg_ids.push((pkg, 1 << cpu)),
        }
        for idx in 0..16 {
            let cdir = dir.join("cache").join(format!("index{idx}"));
            let Some(level) = read_trim(&cdir.join("level")).and_then(|s| s.parse::<u8>().ok())
            else {
                break;
            };
            let kind = match read_trim(&cdir.join("type")).as_deref() {
                Some("Instruction") => 1,
                Some("Data") => 2,
                _ => 0,
            };
            let mask = read_cpu_list(&cdir.join("shared_cpu_list")).unwrap_or(1 << cpu) & active;
            let ways = read_trim(&cdir.join("ways_of_associativity"))
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            let info = CacheInfo {
                level,
                kind,
                size: read_trim(&cdir.join("size"))
                    .map(|s| parse_size(&s))
                    .unwrap_or(0),
                line: read_trim(&cdir.join("coherency_line_size"))
                    .and_then(|s| s.parse::<u16>().ok())
                    .unwrap_or(64),
                assoc: if ways == 0 || ways > 254 {
                    0xFF
                } else {
                    ways as u8
                },
                mask,
            };
            if !topo
                .caches
                .iter()
                .any(|c| c.level == level && c.kind == kind && c.mask == mask)
            {
                topo.caches.push(info);
            }
        }
    }
    pkg_ids.sort_by_key(|(id, _)| *id);
    topo.packages = pkg_ids.into_iter().map(|(_, m)| m).collect();
    topo.caches
        .sort_by_key(|c| (c.level, c.mask.trailing_zeros(), c.kind));

    if let Ok(entries) = std::fs::read_dir(root.join("node")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(n) = name
                .strip_prefix("node")
                .and_then(|n| n.parse::<u32>().ok())
                && let Some(mask) = read_cpu_list(&e.path().join("cpulist"))
                && (mask & active) != 0
            {
                topo.numa.push((n, mask & active));
            }
        }
    }
    topo.numa.sort_by_key(|(n, _)| *n);

    if topo.active == 0 {
        // Repli (sysfs indisponible) : un cœur par processeur logique, un seul package.
        let n = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(64);
        topo.active = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
        topo.cores = (0..n).map(|c| 1u64 << c).collect();
    }
    if topo.packages.is_empty() {
        topo.packages.push(topo.active);
    }
    if topo.numa.is_empty() {
        topo.numa.push((0, topo.active));
    }
    topo
}

fn system_topology() -> Topology {
    read_topology(Path::new("/sys/devices/system"))
}

const RELATION_PROCESSOR_CORE: u32 = 0;
const RELATION_NUMA_NODE: u32 = 1;
const RELATION_CACHE: u32 = 2;
const RELATION_PROCESSOR_PACKAGE: u32 = 3;
const RELATION_GROUP: u32 = 4;
const RELATION_NUMA_NODE_EX: u32 = 6;
const RELATION_ALL: u32 = 0xFFFF;
const LTP_PC_SMT: u8 = 1;

/// `SYSTEM_LOGICAL_PROCESSOR_INFORMATION` (32 octets).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct SystemLogicalProcessorInformation {
    pub processor_mask: u64,
    pub relationship: u32,
    pub _pad: u32,
    pub data: [u8; 16],
}

const _: () = {
    assert!(size_of::<SystemLogicalProcessorInformation>() == 32);
    assert!(offset_of!(SystemLogicalProcessorInformation, data) == 16);
};

pub(super) fn slpi_entries(topo: &Topology) -> Vec<SystemLogicalProcessorInformation> {
    let mut out = Vec::new();
    for &core in &topo.cores {
        let mut e = SystemLogicalProcessorInformation {
            processor_mask: core,
            relationship: RELATION_PROCESSOR_CORE,
            ..Default::default()
        };
        e.data[0] = if core.count_ones() > 1 { LTP_PC_SMT } else { 0 };
        out.push(e);
    }
    for c in &topo.caches {
        let mut e = SystemLogicalProcessorInformation {
            processor_mask: c.mask,
            relationship: RELATION_CACHE,
            ..Default::default()
        };
        e.data[0] = c.level;
        e.data[1] = c.assoc;
        e.data[2..4].copy_from_slice(&c.line.to_le_bytes());
        e.data[4..8].copy_from_slice(&c.size.to_le_bytes());
        e.data[8..12].copy_from_slice(&c.kind.to_le_bytes());
        out.push(e);
    }
    for &p in &topo.packages {
        out.push(SystemLogicalProcessorInformation {
            processor_mask: p,
            relationship: RELATION_PROCESSOR_PACKAGE,
            ..Default::default()
        });
    }
    for &(node, mask) in &topo.numa {
        let mut e = SystemLogicalProcessorInformation {
            processor_mask: mask,
            relationship: RELATION_NUMA_NODE,
            ..Default::default()
        };
        e.data[0..4].copy_from_slice(&node.to_le_bytes());
        out.push(e);
    }
    out
}

/// Copie `bytes` dans `buf` si `*len` suffit, sinon `ERROR_INSUFFICIENT_BUFFER` + taille requise.
fn return_blob(bytes: &[u8], buf: *mut u8, len: *mut u32) -> WinBool {
    if len.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return WIN_FALSE;
    }
    // SAFETY: `len` vérifié non-nul.
    let cap = unsafe { *len } as usize;
    let needed = bytes.len() as u32;
    write_out(len, needed);
    if cap < bytes.len() {
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return WIN_FALSE;
    }
    if buf.is_null() {
        set_last_error(ERROR_NOACCESS);
        return WIN_FALSE;
    }
    // SAFETY: `buf` dispose de `cap >= bytes.len()` octets en écriture.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len()) };
    WIN_TRUE
}

pub(super) fn slpi_bytes(topo: &Topology) -> Vec<u8> {
    let mut v = Vec::new();
    for e in slpi_entries(topo) {
        v.extend_from_slice(&e.processor_mask.to_le_bytes());
        v.extend_from_slice(&e.relationship.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&e.data);
    }
    v
}

pub(super) extern "win64" fn GetLogicalProcessorInformation(
    buf: *mut u8,
    len: *mut u32,
) -> WinBool {
    ffi_guard!(CHANNEL, "GetLogicalProcessorInformation", WIN_FALSE, {
        let bytes = slpi_bytes(&system_topology());
        return_blob(&bytes, buf, len)
    })
}

fn group_affinity_bytes(mask: u64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&mask.to_le_bytes());
    b
}

/// Sérialise les entrées `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX` demandées.
pub(super) fn slpi_ex_bytes(topo: &Topology, relation: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let all = relation == RELATION_ALL;
    let mut push = |rel: u32, body: &[u8]| {
        out.extend_from_slice(&rel.to_le_bytes());
        out.extend_from_slice(&(8 + body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
    };
    let processor = |flags: u8, mask: u64| {
        // PROCESSOR_RELATIONSHIP : Flags, EfficiencyClass, Reserved[20], GroupCount, GroupMask[1].
        let mut b = vec![0u8; 40];
        b[0] = flags;
        b[22..24].copy_from_slice(&1u16.to_le_bytes());
        b[24..40].copy_from_slice(&group_affinity_bytes(mask));
        b
    };
    if all || relation == RELATION_PROCESSOR_CORE {
        for &core in &topo.cores {
            let flags = if core.count_ones() > 1 { LTP_PC_SMT } else { 0 };
            push(RELATION_PROCESSOR_CORE, &processor(flags, core));
        }
    }
    if all || relation == RELATION_NUMA_NODE || relation == RELATION_NUMA_NODE_EX {
        for &(node, mask) in &topo.numa {
            // NUMA_NODE_RELATIONSHIP : NodeNumber, Reserved[18], GroupCount, GroupMask.
            let mut b = vec![0u8; 40];
            b[0..4].copy_from_slice(&node.to_le_bytes());
            b[22..24].copy_from_slice(&1u16.to_le_bytes());
            b[24..40].copy_from_slice(&group_affinity_bytes(mask));
            push(RELATION_NUMA_NODE, &b);
        }
    }
    if all || relation == RELATION_CACHE {
        for c in &topo.caches {
            // CACHE_RELATIONSHIP : Level, Associativity, LineSize, CacheSize, Type,
            // Reserved[18], GroupCount, GroupMask.
            let mut b = vec![0u8; 48];
            b[0] = c.level;
            b[1] = c.assoc;
            b[2..4].copy_from_slice(&c.line.to_le_bytes());
            b[4..8].copy_from_slice(&c.size.to_le_bytes());
            b[8..12].copy_from_slice(&c.kind.to_le_bytes());
            b[30..32].copy_from_slice(&1u16.to_le_bytes());
            b[32..48].copy_from_slice(&group_affinity_bytes(c.mask));
            push(RELATION_CACHE, &b);
        }
    }
    if all || relation == RELATION_PROCESSOR_PACKAGE {
        for &p in &topo.packages {
            push(RELATION_PROCESSOR_PACKAGE, &processor(0, p));
        }
    }
    if all || relation == RELATION_GROUP {
        // GROUP_RELATIONSHIP : MaximumGroupCount, ActiveGroupCount, Reserved[20],
        // PROCESSOR_GROUP_INFO { MaxCount, ActiveCount, Reserved[38], ActiveProcessorMask }.
        let mut b = vec![0u8; 72];
        b[0..2].copy_from_slice(&1u16.to_le_bytes());
        b[2..4].copy_from_slice(&1u16.to_le_bytes());
        let count = topo.active.count_ones() as u8;
        b[24] = count;
        b[25] = count;
        b[64..72].copy_from_slice(&topo.active.to_le_bytes());
        push(RELATION_GROUP, &b);
    }
    out
}

pub(super) extern "win64" fn GetLogicalProcessorInformationEx(
    relation: u32,
    buf: *mut u8,
    len: *mut u32,
) -> WinBool {
    ffi_guard!(CHANNEL, "GetLogicalProcessorInformationEx", WIN_FALSE, {
        if !matches!(relation, 0..=4 | RELATION_NUMA_NODE_EX | RELATION_ALL) {
            set_last_error(ERROR_INVALID_PARAMETER);
            return WIN_FALSE;
        }
        let bytes = slpi_ex_bytes(&system_topology(), relation);
        return_blob(&bytes, buf, len)
    })
}

// ==================== Mémoire & ticks ====================

/// `MEMORYSTATUSEX` (64 octets).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct MemoryStatusEx {
    pub length: u32,
    pub memory_load: u32,
    pub total_phys: u64,
    pub avail_phys: u64,
    pub total_page_file: u64,
    pub avail_page_file: u64,
    pub total_virtual: u64,
    pub avail_virtual: u64,
    pub avail_extended_virtual: u64,
}

const _: () = {
    assert!(size_of::<MemoryStatusEx>() == 64);
    assert!(offset_of!(MemoryStatusEx, total_phys) == 8);
    assert!(offset_of!(MemoryStatusEx, avail_extended_virtual) == 56);
};

/// Espace d'adressage utilisateur Windows x64 (`0x7FFFFFFE0000`, 128 Tio - 128 Kio).
const TOTAL_USER_VIRTUAL: u64 = 0x7FFF_FFFE_0000;

fn meminfo_kb(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
}

pub(super) fn memory_status() -> MemoryStatusEx {
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    // SAFETY: `sysinfo` remplit une struct locale.
    let si = unsafe {
        let mut si: libc::sysinfo = core::mem::zeroed();
        libc::sysinfo(&mut si);
        si
    };
    let unit = u64::from(si.mem_unit.max(1));
    let total = meminfo_kb(&meminfo, "MemTotal").map_or(si.totalram * unit, |k| k * 1024);
    let avail = meminfo_kb(&meminfo, "MemAvailable").map_or(si.freeram * unit, |k| k * 1024);
    let swap_total = meminfo_kb(&meminfo, "SwapTotal").map_or(si.totalswap * unit, |k| k * 1024);
    let swap_free = meminfo_kb(&meminfo, "SwapFree").map_or(si.freeswap * unit, |k| k * 1024);
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let vm_size = meminfo_kb(&status, "VmSize").unwrap_or(0) * 1024;
    let load = (total.saturating_sub(avail) * 100)
        .checked_div(total)
        .unwrap_or(0) as u32;
    MemoryStatusEx {
        length: size_of::<MemoryStatusEx>() as u32,
        memory_load: load,
        total_phys: total,
        avail_phys: avail.min(total),
        total_page_file: total + swap_total,
        avail_page_file: avail.min(total) + swap_free,
        total_virtual: TOTAL_USER_VIRTUAL,
        avail_virtual: TOTAL_USER_VIRTUAL.saturating_sub(vm_size),
        avail_extended_virtual: 0,
    }
}

pub(super) extern "win64" fn GlobalMemoryStatusEx(out: *mut MemoryStatusEx) -> WinBool {
    ffi_guard!(CHANNEL, "GlobalMemoryStatusEx", WIN_FALSE, {
        if out.is_null() {
            set_last_error(ERROR_NOACCESS);
            return WIN_FALSE;
        }
        // SAFETY: `out` non-nul ; `dwLength` doit être initialisé par l'appelant.
        if unsafe { (*out).length } as usize != size_of::<MemoryStatusEx>() {
            set_last_error(ERROR_INVALID_PARAMETER);
            return WIN_FALSE;
        }
        write_out(out, memory_status());
        WIN_TRUE
    })
}

/// Millisecondes depuis le démarrage du système (`CLOCK_BOOTTIME`).
pub(super) fn tick_count_ms() -> u64 {
    // SAFETY: `clock_gettime` remplit une struct locale.
    let ts = unsafe {
        let mut ts: libc::timespec = core::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts);
        ts
    };
    (ts.tv_sec as u64) * 1000 + (ts.tv_nsec as u64) / 1_000_000
}

pub(super) extern "win64" fn GetTickCount64() -> u64 {
    tick_count_ms()
}

pub(super) extern "win64" fn GetTickCount() -> u32 {
    tick_count_ms() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_loader::last_error;

    #[test]
    fn debugger_and_features() {
        super::super::init_test_thread();
        assert_eq!(IsDebuggerPresent(), WIN_FALSE);
        let mut present = 7;
        assert_eq!(
            CheckRemoteDebuggerPresent(PSEUDO_CURRENT_PROCESS, &mut present),
            WIN_TRUE
        );
        assert_eq!(present, WIN_FALSE);
        // SSE2 et RDTSC sont garantis sur x86-64.
        assert_eq!(IsProcessorFeaturePresent(10), WIN_TRUE);
        assert_eq!(IsProcessorFeaturePresent(8), WIN_TRUE);
        assert_eq!(IsProcessorFeaturePresent(23), WIN_TRUE);
        assert_eq!(IsProcessorFeaturePresent(1000), WIN_FALSE);
        assert_eq!(
            IsProcessorFeaturePresent(39) == WIN_TRUE,
            std::arch::is_x86_feature_detected!("avx")
        );
        OutputDebugStringA(c"hello\r\n".as_ptr());
        let w = sb_common::wide::to_wide_null("wide");
        OutputDebugStringW(w.as_ptr());
    }

    extern "win64" fn test_filter(_p: *mut ExceptionPointers) -> i32 {
        -1 // EXCEPTION_CONTINUE_EXECUTION
    }

    #[test]
    fn unhandled_exception_filter_logs_and_calls_filter() {
        super::super::init_test_thread();
        let mut rec = ExceptionRecord {
            exception_code: 0xC000_0005,
            exception_flags: 0,
            exception_record: core::ptr::null_mut(),
            exception_address: 0x1234,
            number_parameters: 0,
            exception_information: [0; 15],
        };
        let mut ptrs = ExceptionPointers {
            exception_record: &mut rec,
            context_record: core::ptr::null_mut(),
        };
        let prev =
            crate::kernel32::set_top_level_exception_filter(test_filter as *const () as usize);
        assert_eq!(UnhandledExceptionFilter(&mut ptrs), -1);
        crate::kernel32::set_top_level_exception_filter(prev);
        if prev == 0 {
            assert_eq!(
                UnhandledExceptionFilter(&mut ptrs),
                EXCEPTION_EXECUTE_HANDLER
            );
        }
    }

    #[test]
    fn stack_limits_contain_local() {
        super::super::init_test_thread();
        let mut lo = 0usize;
        let mut hi = 0usize;
        GetCurrentThreadStackLimits(&mut lo, &mut hi);
        let local = &raw const lo as usize;
        assert!(lo < local && local < hi, "{lo:#x} < {local:#x} < {hi:#x}");
    }

    #[test]
    fn startup_info_and_open_thread() {
        super::super::init_test_thread();
        // SAFETY: struct POD mise à zéro.
        let mut si: StartupInfo = unsafe { core::mem::zeroed() };
        GetStartupInfoW(&mut si);
        assert_eq!(si.cb, 104);
        assert_eq!(si.std_output, HANDLE_STDOUT);

        // SAFETY: `gettid` sans précondition.
        let me = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
        let h = OpenThread(0x1F_FFFF, WIN_FALSE, me);
        assert_ne!(h, 0);
        assert_eq!(sync::GetThreadId(h), me);
        assert!(sync::close_waitable_handle(h));
        assert_eq!(OpenThread(0x1F_FFFF, WIN_FALSE, 0), 0);
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        assert_eq!(OpenThread(0x1F_FFFF, WIN_FALSE, 0x7FFF_FFF0), 0);

        assert_eq!(
            SetProcessPriorityBoost(PSEUDO_CURRENT_PROCESS, WIN_TRUE),
            WIN_TRUE
        );
        let mut b = 0;
        assert_eq!(
            GetProcessPriorityBoost(PSEUDO_CURRENT_PROCESS, &mut b),
            WIN_TRUE
        );
        assert_eq!(b, WIN_TRUE);
        assert_eq!(SetProcessPriorityBoost(0x1234, WIN_TRUE), WIN_FALSE);
        assert_eq!(last_error(), ERROR_INVALID_HANDLE);
    }

    #[test]
    fn group_affinity_roundtrip() {
        super::super::init_test_thread();
        let cur = crate::process::PSEUDO_CURRENT_THREAD;
        let mut ga = GroupAffinity::default();
        assert_eq!(GetThreadGroupAffinity(cur, &mut ga), WIN_TRUE);
        assert_ne!(ga.mask, 0);
        assert_eq!(ga.group, 0);
        let original = ga;
        let lowest = ga.mask & ga.mask.wrapping_neg();
        let wanted = GroupAffinity {
            mask: lowest,
            ..GroupAffinity::default()
        };
        let mut prev = GroupAffinity::default();
        assert_eq!(SetThreadGroupAffinity(cur, &wanted, &mut prev), WIN_TRUE);
        assert_eq!(prev.mask, original.mask);
        let mut now = GroupAffinity::default();
        GetThreadGroupAffinity(cur, &mut now);
        assert_eq!(now.mask, lowest);
        assert_eq!(
            SetThreadGroupAffinity(cur, &original, core::ptr::null_mut()),
            WIN_TRUE
        );
        let bad = GroupAffinity {
            mask: 1,
            group: 1,
            reserved: [0; 3],
        };
        assert_eq!(
            SetThreadGroupAffinity(cur, &bad, core::ptr::null_mut()),
            WIN_FALSE
        );
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        assert_eq!(GetThreadGroupAffinity(0xDEAD_0000, &mut now), WIN_FALSE);
        assert_eq!(last_error(), ERROR_INVALID_HANDLE);
    }

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(p, content).expect("write");
    }

    #[test]
    fn topology_from_fake_sysfs() {
        super::super::init_test_thread();
        let root = std::env::temp_dir().join(format!("sb-agentd-sysfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write(&root, "cpu/online", "0-3\n");
        for cpu in 0..4 {
            let core = cpu / 2;
            write(
                &root,
                &format!("cpu/cpu{cpu}/topology/thread_siblings_list"),
                &format!("{}-{}", core * 2, core * 2 + 1),
            );
            write(
                &root,
                &format!("cpu/cpu{cpu}/topology/physical_package_id"),
                "0",
            );
            for (idx, (lvl, ty, size, shared)) in [
                ("1", "Data", "32K", format!("{}-{}", core * 2, core * 2 + 1)),
                (
                    "1",
                    "Instruction",
                    "32K",
                    format!("{}-{}", core * 2, core * 2 + 1),
                ),
                (
                    "2",
                    "Unified",
                    "512K",
                    format!("{}-{}", core * 2, core * 2 + 1),
                ),
                ("3", "Unified", "16384K", "0-3".to_string()),
            ]
            .into_iter()
            .enumerate()
            {
                let d = format!("cpu/cpu{cpu}/cache/index{idx}");
                write(&root, &format!("{d}/level"), lvl);
                write(&root, &format!("{d}/type"), ty);
                write(&root, &format!("{d}/size"), size);
                write(&root, &format!("{d}/coherency_line_size"), "64");
                write(&root, &format!("{d}/ways_of_associativity"), "8");
                write(&root, &format!("{d}/shared_cpu_list"), &shared);
            }
        }
        write(&root, "node/node0/cpulist", "0-3");
        let topo = read_topology(&root);
        assert_eq!(topo.cores, vec![0b0011, 0b1100]);
        assert_eq!(topo.packages, vec![0b1111]);
        assert_eq!(topo.caches.len(), 7); // 2×L1d + 2×L1i + 2×L2 + 1×L3
        let l3 = topo.caches.iter().find(|c| c.level == 3).expect("L3");
        assert_eq!((l3.size, l3.mask, l3.kind), (16 * 1024 * 1024, 0b1111, 0));
        assert_eq!(topo.numa, vec![(0, 0b1111)]);
        let entries = slpi_entries(&topo);
        assert_eq!(entries.len(), 2 + 7 + 1 + 1);
        assert_eq!(entries[0].data[0], LTP_PC_SMT);
        let ex = slpi_ex_bytes(&topo, RELATION_ALL);
        assert_eq!(ex.len(), 2 * 48 + 48 + 7 * 56 + 48 + 80);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn logical_processor_information_buffer_semantics() {
        super::super::init_test_thread();
        let mut len = 0u32;
        assert_eq!(
            GetLogicalProcessorInformation(core::ptr::null_mut(), &mut len),
            WIN_FALSE
        );
        assert_eq!(last_error(), ERROR_INSUFFICIENT_BUFFER);
        assert!(len >= 32 && len.is_multiple_of(32));
        let mut buf = vec![SystemLogicalProcessorInformation::default(); len as usize / 32];
        let mut small = 31u32;
        assert_eq!(
            GetLogicalProcessorInformation(buf.as_mut_ptr().cast(), &mut small),
            WIN_FALSE
        );
        assert_eq!(small, len);
        assert_eq!(
            GetLogicalProcessorInformation(buf.as_mut_ptr().cast(), &mut len),
            WIN_TRUE
        );
        assert!(
            buf.iter()
                .any(|e| e.relationship == RELATION_PROCESSOR_CORE)
        );
        assert!(
            buf.iter()
                .any(|e| e.relationship == RELATION_PROCESSOR_PACKAGE)
        );

        let mut ex_len = 0u32;
        assert_eq!(
            GetLogicalProcessorInformationEx(RELATION_GROUP, core::ptr::null_mut(), &mut ex_len),
            WIN_FALSE
        );
        assert_eq!(ex_len, 80);
        let mut ex = vec![0u8; 80];
        assert_eq!(
            GetLogicalProcessorInformationEx(RELATION_GROUP, ex.as_mut_ptr(), &mut ex_len),
            WIN_TRUE
        );
        assert_eq!(u32::from_le_bytes([ex[4], ex[5], ex[6], ex[7]]), 80);
        assert_eq!(
            GetLogicalProcessorInformationEx(99, ex.as_mut_ptr(), &mut ex_len),
            WIN_FALSE
        );
    }

    #[test]
    fn memory_status_and_ticks() {
        super::super::init_test_thread();
        let mut ms = MemoryStatusEx::default();
        assert_eq!(GlobalMemoryStatusEx(&mut ms), WIN_FALSE);
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        ms.length = 64;
        assert_eq!(GlobalMemoryStatusEx(&mut ms), WIN_TRUE);
        assert!(ms.total_phys > 0 && ms.avail_phys <= ms.total_phys);
        assert!(ms.memory_load <= 100);
        assert_eq!(ms.total_virtual, TOTAL_USER_VIRTUAL);
        let a = GetTickCount64();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = GetTickCount64();
        assert!(b >= a + 4);
        let t32 = u64::from(GetTickCount());
        assert!((GetTickCount64() & 0xFFFF_FFFF).wrapping_sub(t32) < 1000);
    }
}
