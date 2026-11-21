//! Implémentations HLE natives Rust des runtimes MSVC standards (100 % clean-room, ABI Win64) :
//! - `msvcp140_atomic_wait.dll` (`__std_atomic_wait_direct`, `__std_atomic_notify_*` sur futex réel)
//! - `msvcp140.dll` / `msvcp140_1.dll` / `msvcp140_2.dll` (`_Mtx_*`, `_Cnd_*`, `_Thrd_*`, `_Xtime_get_ticks`)
//! - `vcruntime140_1.dll` (`__CxxFrameHandler4`, `__CxxFrameHandler3`, `__C_specific_handler`)
//! - `concrt140.dll` (`Concurrency::Alloc/Free`, `Context::Yield`, `CurrentScheduler`, `critical_section`)
//! - `vcomp140.dll` (OpenMP : `omp_*`, `_vcomp_enter_critsect`, `_vcomp_fork`, `_vcomp_for_static_*`).

#![allow(non_snake_case, unused_variables)]

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sb_common::abi::{WIN_TRUE, WinBool};
use sb_common::{ffi_guard, sb_debug};

use crate::sync;

const CHANNEL: &str = "msvc_rt";

static OMP_NUM_THREADS: AtomicI32 = AtomicI32::new(0);
static OMP_EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

pub fn atomic_wait_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("__std_atomic_wait_direct", __std_atomic_wait_direct);
    reg!(
        "__std_atomic_notify_one_direct",
        __std_atomic_notify_one_direct
    );
    reg!(
        "__std_atomic_notify_all_direct",
        __std_atomic_notify_all_direct
    );
    reg!("__std_atomic_wait_indirect", __std_atomic_wait_indirect);
    reg!(
        "__std_atomic_notify_one_indirect",
        __std_atomic_notify_one_indirect
    );
    reg!(
        "__std_atomic_notify_all_indirect",
        __std_atomic_notify_all_indirect
    );
    reg!(
        "__std_acquire_shared_mutex_for_instance",
        __std_acquire_shared_mutex_for_instance
    );
    reg!(
        "__std_release_shared_mutex_for_instance",
        __std_release_shared_mutex_for_instance
    );
    m
}

pub fn msvcp_threading_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("_Mtx_init_in_situ", _Mtx_init_in_situ);
    reg!("_Mtx_destroy_in_situ", _Mtx_destroy_in_situ);
    reg!("_Mtx_lock", _Mtx_lock);
    reg!("_Mtx_trylock", _Mtx_trylock);
    reg!("_Mtx_unlock", _Mtx_unlock);
    reg!("_Cnd_init_in_situ", _Cnd_init_in_situ);
    reg!("_Cnd_destroy_in_situ", _Cnd_destroy_in_situ);
    reg!("_Cnd_wait", _Cnd_wait);
    reg!("_Cnd_timedwait", _Cnd_timedwait);
    reg!("_Cnd_signal", _Cnd_signal);
    reg!("_Cnd_broadcast", _Cnd_broadcast);
    reg!("_Thrd_hardware_concurrency", _Thrd_hardware_concurrency);
    reg!("_Thrd_id", _Thrd_id);
    reg!("_Thrd_yield", _Thrd_yield);
    reg!("_Thrd_sleep", _Thrd_sleep);
    reg!("_Xtime_get_ticks", _Xtime_get_ticks);
    reg!("_Query_perf_counter", _Query_perf_counter);
    reg!("_Query_perf_frequency", _Query_perf_frequency);
    m
}

pub fn vcruntime_1_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("__CxxFrameHandler4", __CxxFrameHandler4);
    reg!("__CxxFrameHandler3", __CxxFrameHandler4);
    reg!("__C_specific_handler", __CxxFrameHandler4);
    m
}

pub fn concrt_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("?Alloc@Concurrency@@YAPEAX_K@Z", concrt_alloc);
    reg!("?Free@Concurrency@@YAXPEAX@Z", concrt_free);
    reg!("?Yield@Context@Concurrency@@SAXXZ", concrt_yield);
    reg!(
        "?GetNumberOfVirtualProcessors@CurrentScheduler@Concurrency@@SAIXZ",
        concrt_vprocs
    );
    reg!(
        "?_Acquire@critical_section@Concurrency@@QEAAXXZ",
        concrt_cs_lock
    );
    reg!(
        "?_Release@critical_section@Concurrency@@QEAAXXZ",
        concrt_cs_unlock
    );
    reg!(
        "?_Try_lock@critical_section@Concurrency@@QEAA_NXZ",
        concrt_cs_trylock
    );
    m
}

pub fn vcomp_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("omp_get_num_procs", omp_get_num_procs);
    reg!("omp_get_max_threads", omp_get_max_threads);
    reg!("omp_set_num_threads", omp_set_num_threads);
    reg!("omp_get_num_threads", omp_get_num_threads);
    reg!("omp_get_thread_num", omp_get_thread_num);
    reg!("omp_in_parallel", omp_in_parallel);
    reg!("omp_get_wtime", omp_get_wtime);
    reg!("omp_get_wtick", omp_get_wtick);
    reg!("omp_init_lock", omp_init_lock);
    reg!("omp_destroy_lock", omp_destroy_lock);
    reg!("omp_set_lock", omp_set_lock);
    reg!("omp_unset_lock", omp_unset_lock);
    reg!("omp_test_lock", omp_test_lock);
    reg!("_vcomp_enter_critsect", _vcomp_enter_critsect);
    reg!("_vcomp_leave_critsect", _vcomp_leave_critsect);
    reg!("_vcomp_barrier", _vcomp_barrier);
    reg!("_vcomp_flush", _vcomp_flush);
    reg!(
        "_vcomp_for_static_simple_init",
        _vcomp_for_static_simple_init
    );
    reg!("_vcomp_for_static_end", _vcomp_for_static_end);
    reg!("_vcomp_fork", _vcomp_fork);
    m
}

// ---------- msvcp140_atomic_wait.dll ----------

extern "win64" fn __std_atomic_wait_direct(
    storage: *const u8,
    comparand: *const u8,
    size: usize,
    timeout_ms: u32,
) -> WinBool {
    sync::WaitOnAddress(storage, comparand, size, timeout_ms)
}

extern "win64" fn __std_atomic_notify_one_direct(storage: *const c_void) {
    sync::WakeByAddressSingle(storage);
}

extern "win64" fn __std_atomic_notify_all_direct(storage: *const c_void) {
    sync::WakeByAddressAll(storage);
}

extern "win64" fn __std_atomic_wait_indirect(
    storage: *const u8,
    comparand: *const u8,
    size: usize,
    _param: *mut c_void,
    cmp_fn: Option<unsafe extern "win64" fn(*const u8, *const u8, usize) -> u8>,
    timeout_ms: u32,
) -> WinBool {
    if let Some(f) = cmp_fn {
        // SAFETY: prédicat de comparaison MSVC STL.
        let equal = unsafe { f(storage, comparand, size) } != 0;
        if !equal {
            return WIN_TRUE;
        }
    }
    let clamped_size = match size {
        1 | 2 | 4 | 8 => size,
        _ => 4,
    };
    sync::WaitOnAddress(storage, comparand, clamped_size, timeout_ms)
}

extern "win64" fn __std_atomic_notify_one_indirect(storage: *const c_void) {
    sync::WakeByAddressSingle(storage);
}

extern "win64" fn __std_atomic_notify_all_indirect(storage: *const c_void) {
    sync::WakeByAddressAll(storage);
}

extern "win64" fn __std_acquire_shared_mutex_for_instance(instance: *mut c_void) -> *mut c_void {
    sync::AcquireSRWLockExclusive(instance);
    instance
}

extern "win64" fn __std_release_shared_mutex_for_instance(instance: *mut c_void) {
    sync::ReleaseSRWLockExclusive(instance);
}

// ---------- msvcp140.dll (`_Mtx_*`, `_Cnd_*`, `_Thrd_*`) ----------

extern "win64" fn _Mtx_init_in_situ(mtx: *mut c_void, _flags: i32) {
    sync::InitializeCriticalSection(mtx);
}

extern "win64" fn _Mtx_destroy_in_situ(mtx: *mut c_void) {
    sync::DeleteCriticalSection(mtx);
}

extern "win64" fn _Mtx_lock(mtx: *mut c_void) -> i32 {
    sync::EnterCriticalSection(mtx);
    0 // _Thrd_success
}

extern "win64" fn _Mtx_trylock(mtx: *mut c_void) -> i32 {
    if sync::TryEnterCriticalSection(mtx) == WIN_TRUE {
        0 // _Thrd_success
    } else {
        3 // _Thrd_busy
    }
}

extern "win64" fn _Mtx_unlock(mtx: *mut c_void) -> i32 {
    sync::LeaveCriticalSection(mtx);
    0
}

extern "win64" fn _Cnd_init_in_situ(cnd: *mut c_void) {
    sync::InitializeConditionVariable(cnd);
}

extern "win64" fn _Cnd_destroy_in_situ(_cnd: *mut c_void) {}

extern "win64" fn _Cnd_wait(cnd: *mut c_void, mtx: *mut c_void) -> i32 {
    sync::SleepConditionVariableCS(cnd, mtx, sync::INFINITE);
    0
}

#[repr(C)]
struct Xtime {
    sec: i64,
    nsec: i32,
}

extern "win64" fn _Cnd_timedwait(cnd: *mut c_void, mtx: *mut c_void, target: *const Xtime) -> i32 {
    let ms = if target.is_null() {
        sync::INFINITE
    } else {
        // SAFETY: `target` vérifié non-nul.
        let t = unsafe { &*target };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let target_ms = (t.sec.max(0) as u128) * 1000 + (t.nsec.max(0) as u128) / 1_000_000;
        let now_ms = now.as_millis();
        target_ms
            .saturating_sub(now_ms)
            .min(u128::from(u32::MAX - 1)) as u32
    };
    if sync::SleepConditionVariableCS(cnd, mtx, ms) == WIN_TRUE {
        0 // _Thrd_success
    } else {
        2 // _Thrd_timedout
    }
}

extern "win64" fn _Cnd_signal(cnd: *mut c_void) -> i32 {
    sync::WakeConditionVariable(cnd);
    0
}

extern "win64" fn _Cnd_broadcast(cnd: *mut c_void) -> i32 {
    sync::WakeAllConditionVariable(cnd);
    0
}

extern "win64" fn _Thrd_hardware_concurrency() -> u32 {
    sync::GetActiveProcessorCount(0)
}

extern "win64" fn _Thrd_id() -> u32 {
    // SAFETY: `SYS_gettid` sans précondition.
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

extern "win64" fn _Thrd_yield() {
    std::thread::yield_now();
}

extern "win64" fn _Thrd_sleep(target: *const Xtime) {
    if target.is_null() {
        std::thread::yield_now();
        return;
    }
    // SAFETY: `target` non-nul.
    let t = unsafe { &*target };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let target_ns = (t.sec.max(0) as u128) * 1_000_000_000 + (t.nsec.max(0) as u128);
    let now_ns = now.as_nanos();
    if target_ns > now_ns {
        std::thread::sleep(Duration::from_nanos(
            (target_ns - now_ns).min(u128::from(u64::MAX)) as u64,
        ));
    }
}

extern "win64" fn _Xtime_get_ticks() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_nanos() / 100) as i64)
        .unwrap_or(0)
}

extern "win64" fn _Query_perf_counter() -> i64 {
    OMP_EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as i64
}

extern "win64" fn _Query_perf_frequency() -> i64 {
    1_000_000_000
}

// ---------- vcruntime140_1.dll ----------

extern "win64" fn __CxxFrameHandler4(
    _rec: *mut c_void,
    _frame: *mut c_void,
    _ctx: *mut c_void,
    _dispatcher: *mut c_void,
) -> i32 {
    1 // ExceptionContinueSearch
}

// ---------- concrt140.dll (Concurrency Runtime) ----------

extern "win64" fn concrt_alloc(size: usize) -> *mut c_void {
    // SAFETY: `libc::malloc` standard.
    unsafe { libc::malloc(size.max(1)).cast() }
}

extern "win64" fn concrt_free(ptr_val: *mut c_void) {
    if !ptr_val.is_null() {
        // SAFETY: `ptr_val` provient de `concrt_alloc`.
        unsafe { libc::free(ptr_val.cast()) };
    }
}

extern "win64" fn concrt_yield() {
    std::thread::yield_now();
}

extern "win64" fn concrt_vprocs() -> u32 {
    sync::GetActiveProcessorCount(0)
}

extern "win64" fn concrt_cs_lock(this: *mut c_void) {
    sync::EnterCriticalSection(this);
}

extern "win64" fn concrt_cs_unlock(this: *mut c_void) {
    sync::LeaveCriticalSection(this);
}

extern "win64" fn concrt_cs_trylock(this: *mut c_void) -> u8 {
    u8::from(sync::TryEnterCriticalSection(this) == WIN_TRUE)
}

// ---------- vcomp140.dll (OpenMP) ----------

extern "win64" fn omp_get_num_procs() -> i32 {
    sync::GetActiveProcessorCount(0) as i32
}

extern "win64" fn omp_get_max_threads() -> i32 {
    let custom = OMP_NUM_THREADS.load(Ordering::Acquire);
    if custom > 0 {
        custom
    } else {
        omp_get_num_procs()
    }
}

extern "win64" fn omp_set_num_threads(n: i32) {
    if n > 0 {
        OMP_NUM_THREADS.store(n, Ordering::Release);
    }
}

extern "win64" fn omp_get_num_threads() -> i32 {
    1
}

extern "win64" fn omp_get_thread_num() -> i32 {
    0
}

extern "win64" fn omp_in_parallel() -> i32 {
    0
}

extern "win64" fn omp_get_wtime() -> f64 {
    OMP_EPOCH.get_or_init(Instant::now).elapsed().as_secs_f64()
}

extern "win64" fn omp_get_wtick() -> f64 {
    1e-9
}

extern "win64" fn omp_init_lock(lock: *mut c_void) {
    sync::InitializeCriticalSection(lock);
}

extern "win64" fn omp_destroy_lock(lock: *mut c_void) {
    sync::DeleteCriticalSection(lock);
}

extern "win64" fn omp_set_lock(lock: *mut c_void) {
    sync::EnterCriticalSection(lock);
}

extern "win64" fn omp_unset_lock(lock: *mut c_void) {
    sync::LeaveCriticalSection(lock);
}

extern "win64" fn omp_test_lock(lock: *mut c_void) -> i32 {
    sync::TryEnterCriticalSection(lock)
}

extern "win64" fn _vcomp_enter_critsect(cs: *mut *mut c_void) {
    if !cs.is_null() {
        sync::EnterCriticalSection(cs.cast());
    }
}

extern "win64" fn _vcomp_leave_critsect(cs: *mut *mut c_void) {
    if !cs.is_null() {
        sync::LeaveCriticalSection(cs.cast());
    }
}

extern "win64" fn _vcomp_barrier() {}
extern "win64" fn _vcomp_flush() {
    std::sync::atomic::fence(Ordering::SeqCst);
}

extern "win64" fn _vcomp_for_static_simple_init(
    first: u32,
    last: u32,
    _step: i32,
    _increment: i32,
    out_begin: *mut u32,
    out_end: *mut u32,
) {
    if !out_begin.is_null() {
        // SAFETY: non-nul.
        unsafe {
            *out_begin = first;
        }
    }
    if !out_end.is_null() {
        // SAFETY: non-nul.
        unsafe {
            *out_end = last;
        }
    }
}

extern "win64" fn _vcomp_for_static_end() {}

extern "win64" fn _vcomp_fork(
    _ifval: i32,
    nargs: i32,
    wrapper: Option<unsafe extern "win64" fn(*mut c_void, *mut c_void)>,
    arg0: *mut c_void,
) {
    ffi_guard!(CHANNEL, "_vcomp_fork", (), {
        sb_debug!(CHANNEL, "_vcomp_fork(nargs={nargs})");
        if let Some(f) = wrapper {
            // SAFETY: exécution du corps OpenMP sur le thread courant.
            unsafe {
                f(arg0, std::ptr::null_mut());
            }
        }
    });
}
