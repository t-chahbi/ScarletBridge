//! Primitives multi-threading et synchronisation natives Win64 pour ScarletBridge :
//! - `CreateThread` (`CREATE_SUSPENDED`), `ResumeThread`, `ExitThread`, `GetExitCodeThread`,
//!   `SetThreadPriority`, `SetThreadAffinityMask`, `SetThreadDescription`, `TEB`/`GS` + `.tls` par thread
//! - Table unifiée d'objets waitables (`Event`, `Semaphore`, `Mutex`, `Thread`) pour
//!   `WaitForSingleObject[Ex]` et `WaitForMultipleObjects[Ex]` (`bWaitAll = TRUE/FALSE`)
//! - `CRITICAL_SECTION` réentrante (`TryEnterCriticalSection`), `SRWLOCK` lecteur/rédacteur réel
//!   (Exclusive + Shared + `TryAcquire`), `CONDITION_VARIABLE` sans perte de réveil
//!   (`SleepConditionVariableCS`, `SleepConditionVariableSRW`, `Wake*`)
//! - Futex `WaitOnAddress` / `WakeByAddressSingle` / `WakeByAddressAll` (table 256 buckets, zéro polling)
//! - `InitOnceExecuteOnce` / `InitOnceBeginInitialize` / `InitOnceComplete` thread-safe
//! - TLS (`Tls*`) et FLS (`Fls*`) strictement **par thread** (avec destructeurs FLS à la sortie du thread).

#![allow(non_snake_case, unused_variables)]

use core::ffi::c_void;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, RwLock};
use sb_common::abi::{WIN_FALSE, WIN_TRUE, WinBool};
use sb_common::{ffi_guard, sb_debug, sb_warn};
use sb_loader::{
    get_teb_tls_slot, init_thread_teb, module_manager, set_last_error, set_teb_tls_slot,
};

use crate::process::ReentrantCs;

const CHANNEL: &str = "sync";

pub const WAIT_OBJECT_0: u32 = 0x0000_0000;
pub const WAIT_TIMEOUT: u32 = 0x0000_0102;
pub const WAIT_FAILED: u32 = 0xFFFF_FFFF;
pub const INFINITE: u32 = 0xFFFF_FFFF;
const ERROR_TIMEOUT: u32 = 1460;
const CREATE_SUSPENDED: u32 = 0x0000_0004;
const STILL_ACTIVE: u32 = 259;
const TLS_OUT_OF_INDEXES: u32 = 0xFFFF_FFFF;
const CONDITION_VARIABLE_LOCKMODE_SHARED: u32 = 0x1;

// ==================== TLS & FLS strictement par thread ====================

static NEXT_TLS_SLOT: AtomicU32 = AtomicU32::new(1);
static NEXT_FLS_SLOT: AtomicU32 = AtomicU32::new(1);
static FLS_CALLBACKS: LazyLock<RwLock<HashMap<u32, Option<usize>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

thread_local! {
    static THREAD_TLS: RefCell<HashMap<u32, usize>> = RefCell::new(HashMap::new());
    static THREAD_FLS: RefCell<HashMap<u32, usize>> = RefCell::new(HashMap::new());
}

fn run_fls_destructors_for_current_thread() {
    let entries: Vec<(u32, usize)> = THREAD_FLS.with(|m| m.borrow_mut().drain().collect());
    let callbacks = FLS_CALLBACKS.read().clone();
    for (slot, val) in entries {
        if val != 0
            && let Some(Some(cb_addr)) = callbacks.get(&slot)
        {
            // SAFETY: `cb_addr` est un `PFLS_CALLBACK_FUNCTION` Win64 enregistré via `FlsAlloc`.
            let cb: unsafe extern "win64" fn(*mut c_void) =
                unsafe { core::mem::transmute(*cb_addr) };
            unsafe {
                cb(val as *mut c_void);
            }
        }
    }
}

pub(crate) extern "win64" fn TlsAlloc() -> u32 {
    let slot = NEXT_TLS_SLOT.fetch_add(1, Ordering::AcqRel);
    if slot >= 4096 {
        return TLS_OUT_OF_INDEXES;
    }
    set_last_error(0);
    slot
}

pub(crate) extern "win64" fn TlsGetValue(index: u32) -> *mut c_void {
    set_last_error(0);
    if let Some(v) = get_teb_tls_slot(index) {
        return v as *mut c_void;
    }
    THREAD_TLS.with(|m| m.borrow().get(&index).copied().unwrap_or(0)) as *mut c_void
}

pub(crate) extern "win64" fn TlsSetValue(index: u32, value: *mut c_void) -> WinBool {
    let v = value as usize;
    set_teb_tls_slot(index, v);
    THREAD_TLS.with(|m| {
        m.borrow_mut().insert(index, v);
    });
    WIN_TRUE
}

pub(crate) extern "win64" fn TlsFree(index: u32) -> WinBool {
    set_teb_tls_slot(index, 0);
    THREAD_TLS.with(|m| {
        m.borrow_mut().remove(&index);
    });
    WIN_TRUE
}

pub(crate) extern "win64" fn FlsAlloc(callback: *const c_void) -> u32 {
    let slot = NEXT_FLS_SLOT.fetch_add(1, Ordering::AcqRel);
    let cb = if callback.is_null() {
        None
    } else {
        Some(callback as usize)
    };
    FLS_CALLBACKS.write().insert(slot, cb);
    slot
}

pub(crate) extern "win64" fn FlsGetValue(index: u32) -> *mut c_void {
    set_last_error(0);
    THREAD_FLS.with(|m| m.borrow().get(&index).copied().unwrap_or(0)) as *mut c_void
}

pub(crate) extern "win64" fn FlsSetValue(index: u32, value: *mut c_void) -> WinBool {
    THREAD_FLS.with(|m| {
        m.borrow_mut().insert(index, value as usize);
    });
    WIN_TRUE
}

pub(crate) extern "win64" fn FlsFree(index: u32) -> WinBool {
    FLS_CALLBACKS.write().remove(&index);
    THREAD_FLS.with(|m| {
        m.borrow_mut().remove(&index);
    });
    WIN_TRUE
}

// ==================== Table unifiée d'objets waitables ====================

struct WinMutexInner {
    owner: Option<std::thread::ThreadId>,
    depth: u32,
}

enum WaitableObject {
    Event {
        manual_reset: bool,
        signaled: Mutex<bool>,
    },
    Semaphore {
        state: Mutex<(i32, i32)>, // (current, max)
    },
    WinMutex {
        state: Mutex<WinMutexInner>,
    },
    Thread {
        done: AtomicBool,
        exit_code: AtomicU32,
        tid: AtomicU32,
        suspend_count: AtomicU32,
        start_gate: Arc<(Mutex<bool>, Condvar)>,
    },
    Timer {
        manual_reset: bool,
        due: Mutex<Option<Instant>>,
    },
}

impl WaitableObject {
    fn is_signaled(&self) -> bool {
        match self {
            Self::Event { signaled, .. } => *signaled.lock(),
            Self::Semaphore { state } => state.lock().0 > 0,
            Self::WinMutex { state } => {
                let st = state.lock();
                st.owner.is_none() || st.owner == Some(std::thread::current().id())
            }
            Self::Thread { done, .. } => done.load(Ordering::Acquire),
            Self::Timer { due, .. } => due.lock().is_some_and(|d| Instant::now() >= d),
        }
    }

    fn next_timer_due(&self) -> Option<Instant> {
        if let Self::Timer { due, .. } = self {
            *due.lock()
        } else {
            None
        }
    }

    fn try_acquire(&self) -> bool {
        match self {
            Self::Event {
                manual_reset,
                signaled,
            } => {
                let mut sig = signaled.lock();
                if *sig {
                    if !*manual_reset {
                        *sig = false;
                    }
                    true
                } else {
                    false
                }
            }
            Self::Semaphore { state } => {
                let mut st = state.lock();
                if st.0 > 0 {
                    st.0 -= 1;
                    true
                } else {
                    false
                }
            }
            Self::WinMutex { state } => {
                let me = std::thread::current().id();
                let mut st = state.lock();
                if st.owner.is_none() {
                    st.owner = Some(me);
                    st.depth = 1;
                    true
                } else if st.owner == Some(me) {
                    st.depth += 1;
                    true
                } else {
                    false
                }
            }
            Self::Thread { done, .. } => done.load(Ordering::Acquire),
            Self::Timer { manual_reset, due } => {
                let mut g = due.lock();
                if g.is_some_and(|d| Instant::now() >= d) {
                    if !*manual_reset {
                        *g = None;
                    }
                    true
                } else {
                    false
                }
            }
        }
    }
}

struct WaitHub {
    epoch: Mutex<u64>,
    cv: Condvar,
}

impl WaitHub {
    fn notify_all(&self) {
        let mut g = self.epoch.lock();
        *g = g.wrapping_add(1);
        self.cv.notify_all();
    }
}

static WAIT_HUB: LazyLock<WaitHub> = LazyLock::new(|| WaitHub {
    epoch: Mutex::new(0),
    cv: Condvar::new(),
});

static NEXT_WAIT_HANDLE: AtomicUsize = AtomicUsize::new(0x0002_0000);
static WAITABLES: LazyLock<RwLock<HashMap<usize, Arc<WaitableObject>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

fn register_waitable(obj: WaitableObject) -> usize {
    let handle = NEXT_WAIT_HANDLE.fetch_add(1, Ordering::AcqRel);
    WAITABLES.write().insert(handle, Arc::new(obj));
    handle
}

pub(crate) fn close_waitable_handle(handle: usize) -> bool {
    WAITABLES.write().remove(&handle).is_some()
}

pub(crate) fn create_event_handle(manual_reset: bool, initial_state: bool) -> usize {
    register_waitable(WaitableObject::Event {
        manual_reset,
        signaled: Mutex::new(initial_state),
    })
}

pub(crate) extern "win64" fn CreateEventA(
    _sec: *mut c_void,
    manual_reset: WinBool,
    initial_state: WinBool,
    _name: *const u8,
) -> usize {
    create_event_handle(manual_reset != 0, initial_state != 0)
}

pub(crate) extern "win64" fn CreateEventW(
    _sec: *mut c_void,
    manual_reset: WinBool,
    initial_state: WinBool,
    _name: *const u16,
) -> usize {
    create_event_handle(manual_reset != 0, initial_state != 0)
}

pub(crate) extern "win64" fn CreateEventExA(
    _sec: *mut c_void,
    _name: *const u8,
    flags: u32,
    _access: u32,
) -> usize {
    create_event_handle((flags & 0x1) != 0, (flags & 0x2) != 0)
}

pub(crate) extern "win64" fn CreateEventExW(
    _sec: *mut c_void,
    _name: *const u16,
    flags: u32,
    _access: u32,
) -> usize {
    create_event_handle((flags & 0x1) != 0, (flags & 0x2) != 0)
}

pub(crate) extern "win64" fn SetEvent(handle: usize) -> WinBool {
    let Some(obj) = WAITABLES.read().get(&handle).cloned() else {
        return WIN_FALSE;
    };
    if let WaitableObject::Event { signaled, .. } = obj.as_ref() {
        *signaled.lock() = true;
        WAIT_HUB.notify_all();
        WIN_TRUE
    } else {
        WIN_FALSE
    }
}

pub(crate) extern "win64" fn ResetEvent(handle: usize) -> WinBool {
    let Some(obj) = WAITABLES.read().get(&handle).cloned() else {
        return WIN_FALSE;
    };
    if let WaitableObject::Event { signaled, .. } = obj.as_ref() {
        *signaled.lock() = false;
        WIN_TRUE
    } else {
        WIN_FALSE
    }
}

pub(crate) extern "win64" fn PulseEvent(handle: usize) -> WinBool {
    if SetEvent(handle) == WIN_TRUE {
        std::thread::yield_now();
        ResetEvent(handle)
    } else {
        WIN_FALSE
    }
}

pub(crate) extern "win64" fn CreateSemaphoreA(
    _sec: *mut c_void,
    initial_count: i32,
    max_count: i32,
    _name: *const u8,
) -> usize {
    create_semaphore_impl(initial_count, max_count)
}

pub(crate) extern "win64" fn CreateSemaphoreW(
    _sec: *mut c_void,
    initial_count: i32,
    max_count: i32,
    _name: *const u16,
) -> usize {
    create_semaphore_impl(initial_count, max_count)
}

pub(crate) extern "win64" fn CreateSemaphoreExW(
    _sec: *mut c_void,
    initial_count: i32,
    max_count: i32,
    _name: *const u16,
    _flags: u32,
    _access: u32,
) -> usize {
    create_semaphore_impl(initial_count, max_count)
}

fn create_semaphore_impl(initial_count: i32, max_count: i32) -> usize {
    let max_c = max_count.max(1);
    let init_c = initial_count.clamp(0, max_c);
    register_waitable(WaitableObject::Semaphore {
        state: Mutex::new((init_c, max_c)),
    })
}

pub(crate) extern "win64" fn ReleaseSemaphore(
    handle: usize,
    release_count: i32,
    out_prev: *mut i32,
) -> WinBool {
    if release_count <= 0 {
        return WIN_FALSE;
    }
    let Some(obj) = WAITABLES.read().get(&handle).cloned() else {
        return WIN_FALSE;
    };
    if let WaitableObject::Semaphore { state } = obj.as_ref() {
        let mut st = state.lock();
        let prev = st.0;
        let next = prev.saturating_add(release_count);
        if next > st.1 {
            return WIN_FALSE;
        }
        st.0 = next;
        drop(st);
        if !out_prev.is_null() {
            // SAFETY: `out_prev` vérifié non-nul.
            unsafe {
                *out_prev = prev;
            }
        }
        WAIT_HUB.notify_all();
        WIN_TRUE
    } else {
        WIN_FALSE
    }
}

pub(crate) extern "win64" fn CreateMutexA(
    _sec: *mut c_void,
    initial_owner: WinBool,
    _name: *const u8,
) -> usize {
    create_mutex_impl(initial_owner != 0)
}

pub(crate) extern "win64" fn CreateMutexW(
    _sec: *mut c_void,
    initial_owner: WinBool,
    _name: *const u16,
) -> usize {
    create_mutex_impl(initial_owner != 0)
}

pub(crate) extern "win64" fn CreateMutexExW(
    _sec: *mut c_void,
    _name: *const u16,
    flags: u32,
    _access: u32,
) -> usize {
    create_mutex_impl((flags & 0x1) != 0)
}

fn create_mutex_impl(initial_owner: bool) -> usize {
    let (owner, depth) = if initial_owner {
        (Some(std::thread::current().id()), 1)
    } else {
        (None, 0)
    };
    register_waitable(WaitableObject::WinMutex {
        state: Mutex::new(WinMutexInner { owner, depth }),
    })
}

pub(crate) extern "win64" fn ReleaseMutex(handle: usize) -> WinBool {
    let Some(obj) = WAITABLES.read().get(&handle).cloned() else {
        return WIN_FALSE;
    };
    if let WaitableObject::WinMutex { state } = obj.as_ref() {
        let me = std::thread::current().id();
        let mut st = state.lock();
        if st.owner != Some(me) {
            return WIN_FALSE;
        }
        st.depth = st.depth.saturating_sub(1);
        if st.depth == 0 {
            st.owner = None;
            drop(st);
            WAIT_HUB.notify_all();
        }
        WIN_TRUE
    } else {
        WIN_FALSE
    }
}

pub(crate) extern "win64" fn CreateWaitableTimerW(
    _sec: *mut c_void,
    manual_reset: WinBool,
    _name: *const u16,
) -> usize {
    register_waitable(WaitableObject::Timer {
        manual_reset: manual_reset != 0,
        due: Mutex::new(None),
    })
}

pub(crate) extern "win64" fn CreateWaitableTimerExW(
    _sec: *mut c_void,
    _name: *const u16,
    flags: u32,
    _access: u32,
) -> usize {
    register_waitable(WaitableObject::Timer {
        manual_reset: (flags & 0x1) != 0,
        due: Mutex::new(None),
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) extern "win64" fn SetWaitableTimer(
    handle: usize,
    due_time: *const i64,
    _period_ms: i32,
    _completion: *const c_void,
    _arg: *mut c_void,
    _resume: WinBool,
) -> WinBool {
    if due_time.is_null() {
        return WIN_FALSE;
    }
    let Some(obj) = WAITABLES.read().get(&handle).cloned() else {
        return WIN_FALSE;
    };
    if let WaitableObject::Timer { due, .. } = obj.as_ref() {
        // SAFETY: `due_time` non-nul (valeur négative = délai relatif en unités de 100 ns).
        let raw = unsafe { *due_time };
        let target = if raw <= 0 {
            let ns = raw.unsigned_abs().saturating_mul(100);
            Instant::now() + Duration::from_nanos(ns)
        } else {
            Instant::now()
        };
        *due.lock() = Some(target);
        WAIT_HUB.notify_all();
        WIN_TRUE
    } else {
        WIN_FALSE
    }
}

pub(crate) extern "win64" fn SetWaitableTimerEx(
    handle: usize,
    due_time: *const i64,
    period_ms: i32,
    completion: *const c_void,
    arg: *mut c_void,
    _wake_ctx: *mut c_void,
    _tolerable_delay_ms: u32,
) -> WinBool {
    SetWaitableTimer(handle, due_time, period_ms, completion, arg, WIN_FALSE)
}

pub(crate) extern "win64" fn CancelWaitableTimer(handle: usize) -> WinBool {
    if let Some(obj) = WAITABLES.read().get(&handle).cloned()
        && let WaitableObject::Timer { due, .. } = obj.as_ref()
    {
        *due.lock() = None;
        return WIN_TRUE;
    }
    WIN_FALSE
}

pub(crate) extern "win64" fn WaitForSingleObject(handle: usize, timeout_ms: u32) -> u32 {
    ffi_guard!(CHANNEL, "WaitForSingleObject", WAIT_FAILED, {
        let Some(obj) = WAITABLES.read().get(&handle).cloned() else {
            return WAIT_OBJECT_0;
        };
        let deadline = if timeout_ms == INFINITE {
            None
        } else {
            Some(Instant::now() + Duration::from_millis(u64::from(timeout_ms)))
        };
        loop {
            let mut epoch_guard = WAIT_HUB.epoch.lock();
            if obj.try_acquire() {
                return WAIT_OBJECT_0;
            }
            let timer_due = obj.next_timer_due();
            let wake_at = match (deadline, timer_due) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            if let Some(dl) = deadline
                && Instant::now() >= dl
            {
                return WAIT_TIMEOUT;
            }
            if let Some(at) = wake_at {
                WAIT_HUB.cv.wait_until(&mut epoch_guard, at);
            } else {
                WAIT_HUB.cv.wait(&mut epoch_guard);
            }
        }
    })
}

pub(crate) extern "win64" fn WaitForSingleObjectEx(
    handle: usize,
    timeout_ms: u32,
    _alertable: WinBool,
) -> u32 {
    WaitForSingleObject(handle, timeout_ms)
}

pub(crate) extern "win64" fn WaitForMultipleObjects(
    count: u32,
    handles: *const usize,
    wait_all: WinBool,
    timeout_ms: u32,
) -> u32 {
    ffi_guard!(CHANNEL, "WaitForMultipleObjects", WAIT_FAILED, {
        if count == 0 || handles.is_null() {
            return WAIT_FAILED;
        }
        // SAFETY: `handles` pointe vers `count` handles valides.
        let slice = unsafe { std::slice::from_raw_parts(handles, count as usize) };
        let objs: Vec<Option<Arc<WaitableObject>>> = {
            let tbl = WAITABLES.read();
            slice.iter().map(|h| tbl.get(h).cloned()).collect()
        };

        let deadline = if timeout_ms == INFINITE {
            None
        } else {
            Some(Instant::now() + Duration::from_millis(u64::from(timeout_ms)))
        };

        loop {
            let mut epoch_guard = WAIT_HUB.epoch.lock();
            if wait_all != 0 {
                let all_ready = objs
                    .iter()
                    .all(|opt| opt.as_ref().is_none_or(|o| o.is_signaled()));
                if all_ready {
                    for o in objs.iter().flatten() {
                        let _ = o.try_acquire();
                    }
                    return WAIT_OBJECT_0;
                }
            } else {
                for (i, opt) in objs.iter().enumerate() {
                    if let Some(o) = opt {
                        if o.try_acquire() {
                            return WAIT_OBJECT_0 + (i as u32);
                        }
                    } else {
                        return WAIT_OBJECT_0 + (i as u32);
                    }
                }
            }

            if let Some(dl) = deadline {
                let now = Instant::now();
                if now >= dl {
                    return WAIT_TIMEOUT;
                }
                WAIT_HUB.cv.wait_until(&mut epoch_guard, dl);
            } else {
                WAIT_HUB.cv.wait(&mut epoch_guard);
            }
        }
    })
}

pub(crate) extern "win64" fn WaitForMultipleObjectsEx(
    count: u32,
    handles: *const usize,
    wait_all: WinBool,
    timeout_ms: u32,
    _alertable: WinBool,
) -> u32 {
    WaitForMultipleObjects(count, handles, wait_all, timeout_ms)
}

// ==================== CRITICAL_SECTION, SRWLOCK & CONDITION_VARIABLE ====================

static CRIT_SECTIONS: LazyLock<RwLock<HashMap<usize, Arc<ReentrantCs>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

fn get_or_create_cs(addr: usize) -> Arc<ReentrantCs> {
    if let Some(cs) = CRIT_SECTIONS.read().get(&addr).cloned() {
        return cs;
    }
    let mut map = CRIT_SECTIONS.write();
    Arc::clone(
        map.entry(addr)
            .or_insert_with(|| Arc::new(ReentrantCs::new())),
    )
}

pub(crate) extern "win64" fn InitializeCriticalSection(cs: *mut c_void) {
    if !cs.is_null() {
        let _ = get_or_create_cs(cs as usize);
    }
}

pub(crate) extern "win64" fn InitializeCriticalSectionAndSpinCount(
    cs: *mut c_void,
    _spin: u32,
) -> WinBool {
    InitializeCriticalSection(cs);
    WIN_TRUE
}

pub(crate) extern "win64" fn InitializeCriticalSectionEx(
    cs: *mut c_void,
    _spin: u32,
    _flags: u32,
) -> WinBool {
    InitializeCriticalSection(cs);
    WIN_TRUE
}

pub(crate) extern "win64" fn DeleteCriticalSection(cs: *mut c_void) {
    if !cs.is_null() {
        CRIT_SECTIONS.write().remove(&(cs as usize));
    }
}

pub(crate) extern "win64" fn EnterCriticalSection(cs: *mut c_void) {
    if !cs.is_null() {
        get_or_create_cs(cs as usize).lock();
    }
}

pub(crate) extern "win64" fn TryEnterCriticalSection(cs: *mut c_void) -> WinBool {
    if cs.is_null() {
        return WIN_FALSE;
    }
    if get_or_create_cs(cs as usize).try_lock() {
        WIN_TRUE
    } else {
        WIN_FALSE
    }
}

pub(crate) extern "win64" fn LeaveCriticalSection(cs: *mut c_void) {
    if !cs.is_null() {
        get_or_create_cs(cs as usize).unlock();
    }
}

// --- SRWLOCK lecteur/rédacteur réel ---

struct SrwState {
    readers: u32,
    writer: bool,
}

struct SrwLockObj {
    state: Mutex<SrwState>,
    cv: Condvar,
}

impl SrwLockObj {
    fn new() -> Self {
        Self {
            state: Mutex::new(SrwState {
                readers: 0,
                writer: false,
            }),
            cv: Condvar::new(),
        }
    }

    fn lock_exclusive(&self) {
        let mut st = self.state.lock();
        while st.writer || st.readers > 0 {
            self.cv.wait(&mut st);
        }
        st.writer = true;
    }

    fn try_lock_exclusive(&self) -> bool {
        let mut st = self.state.lock();
        if !st.writer && st.readers == 0 {
            st.writer = true;
            true
        } else {
            false
        }
    }

    fn unlock_exclusive(&self) {
        let mut st = self.state.lock();
        st.writer = false;
        self.cv.notify_all();
    }

    fn lock_shared(&self) {
        let mut st = self.state.lock();
        while st.writer {
            self.cv.wait(&mut st);
        }
        st.readers += 1;
    }

    fn try_lock_shared(&self) -> bool {
        let mut st = self.state.lock();
        if !st.writer {
            st.readers += 1;
            true
        } else {
            false
        }
    }

    fn unlock_shared(&self) {
        let mut st = self.state.lock();
        st.readers = st.readers.saturating_sub(1);
        if st.readers == 0 {
            self.cv.notify_all();
        }
    }
}

static SRW_LOCKS: LazyLock<RwLock<HashMap<usize, Arc<SrwLockObj>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

fn get_or_create_srw(addr: usize) -> Arc<SrwLockObj> {
    if let Some(l) = SRW_LOCKS.read().get(&addr).cloned() {
        return l;
    }
    let mut map = SRW_LOCKS.write();
    Arc::clone(
        map.entry(addr)
            .or_insert_with(|| Arc::new(SrwLockObj::new())),
    )
}

pub(crate) extern "win64" fn InitializeSRWLock(lock: *mut c_void) {
    if !lock.is_null() {
        let _ = get_or_create_srw(lock as usize);
    }
}

pub(crate) extern "win64" fn AcquireSRWLockExclusive(lock: *mut c_void) {
    if !lock.is_null() {
        get_or_create_srw(lock as usize).lock_exclusive();
    }
}

pub(crate) extern "win64" fn TryAcquireSRWLockExclusive(lock: *mut c_void) -> u8 {
    if !lock.is_null() && get_or_create_srw(lock as usize).try_lock_exclusive() {
        1
    } else {
        0
    }
}

pub(crate) extern "win64" fn ReleaseSRWLockExclusive(lock: *mut c_void) {
    if !lock.is_null() {
        get_or_create_srw(lock as usize).unlock_exclusive();
    }
}

pub(crate) extern "win64" fn AcquireSRWLockShared(lock: *mut c_void) {
    if !lock.is_null() {
        get_or_create_srw(lock as usize).lock_shared();
    }
}

pub(crate) extern "win64" fn TryAcquireSRWLockShared(lock: *mut c_void) -> u8 {
    if !lock.is_null() && get_or_create_srw(lock as usize).try_lock_shared() {
        1
    } else {
        0
    }
}

pub(crate) extern "win64" fn ReleaseSRWLockShared(lock: *mut c_void) {
    if !lock.is_null() {
        get_or_create_srw(lock as usize).unlock_shared();
    }
}

// --- CONDITION_VARIABLE sans perte de réveil (compteur de génération) ---

struct CondVarObj {
    seq: Mutex<u64>,
    cv: Condvar,
}

static CONDVARS: LazyLock<RwLock<HashMap<usize, Arc<CondVarObj>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

fn get_or_create_cv(addr: usize) -> Arc<CondVarObj> {
    if let Some(c) = CONDVARS.read().get(&addr).cloned() {
        return c;
    }
    let mut map = CONDVARS.write();
    Arc::clone(map.entry(addr).or_insert_with(|| {
        Arc::new(CondVarObj {
            seq: Mutex::new(0),
            cv: Condvar::new(),
        })
    }))
}

pub(crate) extern "win64" fn InitializeConditionVariable(cv: *mut c_void) {
    if !cv.is_null() {
        let _ = get_or_create_cv(cv as usize);
    }
}

pub(crate) extern "win64" fn SleepConditionVariableCS(
    cv: *mut c_void,
    cs: *mut c_void,
    timeout_ms: u32,
) -> WinBool {
    ffi_guard!(CHANNEL, "SleepConditionVariableCS", WIN_FALSE, {
        if cv.is_null() || cs.is_null() {
            return WIN_FALSE;
        }
        let cond = get_or_create_cv(cv as usize);
        let cs_obj = get_or_create_cs(cs as usize);
        // Capture le compteur de génération AVANT de relâcher la section critique (zéro lost wakeup).
        let seen = *cond.seq.lock();
        cs_obj.unlock();

        let timed_out = {
            let mut g = cond.seq.lock();
            if *g == seen {
                if timeout_ms == INFINITE {
                    cond.cv.wait(&mut g);
                    false
                } else {
                    let res = cond
                        .cv
                        .wait_for(&mut g, Duration::from_millis(u64::from(timeout_ms)));
                    res.timed_out() && *g == seen
                }
            } else {
                false
            }
        };

        cs_obj.lock();
        if timed_out {
            set_last_error(ERROR_TIMEOUT);
            WIN_FALSE
        } else {
            WIN_TRUE
        }
    })
}

pub(crate) extern "win64" fn SleepConditionVariableSRW(
    cv: *mut c_void,
    srw: *mut c_void,
    timeout_ms: u32,
    flags: u32,
) -> WinBool {
    ffi_guard!(CHANNEL, "SleepConditionVariableSRW", WIN_FALSE, {
        if cv.is_null() || srw.is_null() {
            return WIN_FALSE;
        }
        let shared = (flags & CONDITION_VARIABLE_LOCKMODE_SHARED) != 0;
        let cond = get_or_create_cv(cv as usize);
        let lock = get_or_create_srw(srw as usize);
        let seen = *cond.seq.lock();
        if shared {
            lock.unlock_shared();
        } else {
            lock.unlock_exclusive();
        }

        let timed_out = {
            let mut g = cond.seq.lock();
            if *g == seen {
                if timeout_ms == INFINITE {
                    cond.cv.wait(&mut g);
                    false
                } else {
                    let res = cond
                        .cv
                        .wait_for(&mut g, Duration::from_millis(u64::from(timeout_ms)));
                    res.timed_out() && *g == seen
                }
            } else {
                false
            }
        };

        if shared {
            lock.lock_shared();
        } else {
            lock.lock_exclusive();
        }
        if timed_out {
            set_last_error(ERROR_TIMEOUT);
            WIN_FALSE
        } else {
            WIN_TRUE
        }
    })
}

pub(crate) extern "win64" fn WakeConditionVariable(cv: *mut c_void) {
    if !cv.is_null() {
        let cond = get_or_create_cv(cv as usize);
        let mut g = cond.seq.lock();
        *g = g.wrapping_add(1);
        cond.cv.notify_one();
    }
}

pub(crate) extern "win64" fn WakeAllConditionVariable(cv: *mut c_void) {
    if !cv.is_null() {
        let cond = get_or_create_cv(cv as usize);
        let mut g = cond.seq.lock();
        *g = g.wrapping_add(1);
        cond.cv.notify_all();
    }
}

// ==================== Futex `WaitOnAddress` / `WakeByAddress*` (256 buckets) ====================

struct FutexBucket {
    mu: Mutex<u64>,
    cv: Condvar,
}

static FUTEX_TABLE: LazyLock<Vec<FutexBucket>> = LazyLock::new(|| {
    (0..256)
        .map(|_| FutexBucket {
            mu: Mutex::new(0),
            cv: Condvar::new(),
        })
        .collect()
});

#[inline]
fn futex_bucket(addr: usize) -> &'static FutexBucket {
    let idx = (addr >> 3) ^ (addr >> 11);
    &FUTEX_TABLE[idx & 0xFF]
}

pub(crate) extern "win64" fn WaitOnAddress(
    addr: *const u8,
    compare_addr: *const u8,
    size: usize,
    timeout_ms: u32,
) -> WinBool {
    ffi_guard!(CHANNEL, "WaitOnAddress", WIN_FALSE, {
        if addr.is_null() || compare_addr.is_null() || !matches!(size, 1 | 2 | 4 | 8) {
            return WIN_FALSE;
        }
        let bucket = futex_bucket(addr as usize);
        let mut guard = bucket.mu.lock();
        // SAFETY: `addr` et `compare_addr` valides sur `size` octets (1/2/4/8).
        let equal = unsafe {
            std::slice::from_raw_parts(addr, size) == std::slice::from_raw_parts(compare_addr, size)
        };
        if !equal {
            return WIN_TRUE;
        }
        if timeout_ms == INFINITE {
            bucket.cv.wait(&mut guard);
            WIN_TRUE
        } else {
            let res = bucket
                .cv
                .wait_for(&mut guard, Duration::from_millis(u64::from(timeout_ms)));
            if res.timed_out() {
                set_last_error(ERROR_TIMEOUT);
                WIN_FALSE
            } else {
                WIN_TRUE
            }
        }
    })
}

pub(crate) extern "win64" fn WakeByAddressSingle(addr: *const c_void) {
    let bucket = futex_bucket(addr as usize);
    let mut g = bucket.mu.lock();
    *g = g.wrapping_add(1);
    bucket.cv.notify_one();
}

pub(crate) extern "win64" fn WakeByAddressAll(addr: *const c_void) {
    let bucket = futex_bucket(addr as usize);
    let mut g = bucket.mu.lock();
    *g = g.wrapping_add(1);
    bucket.cv.notify_all();
}

// ==================== `InitOnce*` thread-safe ====================

struct InitOnceEntry {
    state: Mutex<u8>, // 0 = uninit, 1 = in_progress, 2 = done
    cv: Condvar,
}

static INIT_ONCE_MAP: LazyLock<RwLock<HashMap<usize, Arc<InitOnceEntry>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

fn get_init_once(addr: usize) -> Arc<InitOnceEntry> {
    if let Some(e) = INIT_ONCE_MAP.read().get(&addr).cloned() {
        return e;
    }
    let mut map = INIT_ONCE_MAP.write();
    Arc::clone(map.entry(addr).or_insert_with(|| {
        Arc::new(InitOnceEntry {
            state: Mutex::new(0),
            cv: Condvar::new(),
        })
    }))
}

pub(crate) extern "win64" fn InitOnceInitialize(init_once: *mut usize) {
    if !init_once.is_null() {
        // SAFETY: `init_once` non-nul.
        unsafe {
            *init_once = 0;
        }
        INIT_ONCE_MAP.write().remove(&(init_once as usize));
    }
}

pub(crate) extern "win64" fn InitOnceBeginInitialize(
    init_once: *mut usize,
    flags: u32,
    pending: *mut WinBool,
    context: *mut usize,
) -> WinBool {
    ffi_guard!(CHANNEL, "InitOnceBeginInitialize", WIN_FALSE, {
        if init_once.is_null() || pending.is_null() {
            return WIN_FALSE;
        }
        let entry = get_init_once(init_once as usize);
        let mut st = entry.state.lock();
        while *st == 1 {
            entry.cv.wait(&mut st);
        }
        // SAFETY: `init_once` non-nul.
        let cur = unsafe { *init_once };
        if *st == 2 || (cur & 1) != 0 {
            *st = 2;
            unsafe {
                *pending = WIN_FALSE;
                if !context.is_null() {
                    *context = cur & !1;
                }
            }
            return WIN_TRUE;
        }
        if (flags & 1) != 0 {
            // INIT_ONCE_CHECK_ONLY
            return WIN_FALSE;
        }
        *st = 1;
        // SAFETY: `pending` non-nul.
        unsafe {
            *pending = WIN_TRUE;
        }
        WIN_TRUE
    })
}

pub(crate) extern "win64" fn InitOnceComplete(
    init_once: *mut usize,
    flags: u32,
    context: usize,
) -> WinBool {
    if init_once.is_null() {
        return WIN_FALSE;
    }
    let entry = get_init_once(init_once as usize);
    let mut st = entry.state.lock();
    if (flags & 4) != 0 {
        // INIT_ONCE_INIT_FAILED
        *st = 0;
    } else {
        // SAFETY: `init_once` valide en écriture.
        unsafe {
            *init_once = (context & !1) | 1;
        }
        *st = 2;
    }
    entry.cv.notify_all();
    WIN_TRUE
}

pub(crate) extern "win64" fn InitOnceExecuteOnce(
    init_once: *mut usize,
    init_fn: Option<unsafe extern "win64" fn(*mut usize, *mut c_void, *mut *mut c_void) -> WinBool>,
    param: *mut c_void,
    out_ctx: *mut *mut c_void,
) -> WinBool {
    ffi_guard!(CHANNEL, "InitOnceExecuteOnce", WIN_FALSE, {
        let mut pending: WinBool = WIN_FALSE;
        let mut ctx_val: usize = 0;
        if InitOnceBeginInitialize(init_once, 0, &mut pending, &mut ctx_val) == WIN_FALSE {
            return WIN_FALSE;
        }
        if pending != 0 {
            let mut new_ctx: *mut c_void = std::ptr::null_mut();
            let ok = if let Some(f) = init_fn {
                // SAFETY: `init_fn` est un `PINIT_ONCE_FN` Win64 valide.
                unsafe { f(init_once, param, &mut new_ctx) }
            } else {
                WIN_TRUE
            };
            if ok != 0 {
                InitOnceComplete(init_once, 0, new_ctx as usize);
                ctx_val = new_ctx as usize;
            } else {
                InitOnceComplete(init_once, 4, 0);
                return WIN_FALSE;
            }
        }
        if !out_ctx.is_null() {
            // SAFETY: `out_ctx` non-nul.
            unsafe {
                *out_ctx = ctx_val as *mut c_void;
            }
        }
        WIN_TRUE
    })
}

// ==================== Threads Win64 (`CreateThread`, `ResumeThread`, `ExitThread`, etc.) ====================

struct ThreadExitSignal(u32);

pub(crate) extern "win64" fn CreateThread(
    _sec: *mut c_void,
    stack_size: usize,
    start_address: usize,
    parameter: *mut c_void,
    creation_flags: u32,
    out_tid: *mut u32,
) -> usize {
    ffi_guard!(CHANNEL, "CreateThread", 0, {
        let param_val = parameter as usize;
        let suspended = (creation_flags & CREATE_SUSPENDED) != 0;
        let start_gate = Arc::new((Mutex::new(!suspended), Condvar::new()));
        let handle = register_waitable(WaitableObject::Thread {
            done: AtomicBool::new(false),
            exit_code: AtomicU32::new(STILL_ACTIVE),
            tid: AtomicU32::new(0),
            suspend_count: AtomicU32::new(u32::from(suspended)),
            start_gate: Arc::clone(&start_gate),
        });
        let thread_obj = WAITABLES.read().get(&handle).cloned();
        let tid_ready = Arc::new((Mutex::new(0u32), Condvar::new()));
        let tid_ready_worker = Arc::clone(&tid_ready);

        let mut builder = std::thread::Builder::new().name(format!("sb-guest-{handle:#x}"));
        if stack_size >= 65536 {
            builder = builder.stack_size(stack_size.min(16 * 1024 * 1024));
        }

        let spawned = builder.spawn(move || {
            // SAFETY: `SYS_gettid` sans précondition.
            let linux_tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
            if let Some(ref obj) = thread_obj
                && let WaitableObject::Thread { tid, .. } = obj.as_ref()
            {
                tid.store(linux_tid, Ordering::Release);
            }
            {
                let mut g = tid_ready_worker.0.lock();
                *g = linux_tid;
                tid_ready_worker.1.notify_all();
            }

            if let Err(err) = init_thread_teb(0) {
                sb_warn!(CHANNEL, "init_thread_teb thread invité échoué : {err}");
            }
            module_manager().attach_current_thread_tls();

            // Attendre `ResumeThread` si `CREATE_SUSPENDED`.
            {
                let mut go = start_gate.0.lock();
                while !*go {
                    start_gate.1.wait(&mut go);
                }
            }

            let code = if start_address != 0 {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // SAFETY: `start_address` est une `LPTHREAD_START_ROUTINE` Win64.
                    let entry: unsafe extern "win64" fn(*mut c_void) -> u32 =
                        unsafe { core::mem::transmute(start_address) };
                    unsafe { entry(param_val as *mut c_void) }
                }));
                match res {
                    Ok(c) => c,
                    Err(payload) => {
                        if let Some(sig) = payload.downcast_ref::<ThreadExitSignal>() {
                            sig.0
                        } else {
                            sb_warn!(CHANNEL, "panic dans thread invité {handle:#x}");
                            1
                        }
                    }
                }
            } else {
                0
            };

            run_fls_destructors_for_current_thread();
            module_manager().detach_current_thread_tls();

            if let Some(ref obj) = thread_obj
                && let WaitableObject::Thread {
                    done, exit_code, ..
                } = obj.as_ref()
            {
                exit_code.store(code, Ordering::Release);
                done.store(true, Ordering::Release);
            }
            WAIT_HUB.notify_all();
        });

        if let Err(err) = spawned {
            sb_warn!(CHANNEL, "CreateThread spawn échoué: {err}");
            WAITABLES.write().remove(&handle);
            return 0;
        }

        // Attend brièvement le TID réel Linux du thread créé.
        let assigned_tid = {
            let mut g = tid_ready.0.lock();
            if *g == 0 {
                let _ = tid_ready.1.wait_for(&mut g, Duration::from_millis(50));
            }
            if *g != 0 { *g } else { handle as u32 }
        };

        if !out_tid.is_null() {
            // SAFETY: `out_tid` vérifié non-nul.
            unsafe {
                *out_tid = assigned_tid;
            }
        }
        sb_debug!(
            CHANNEL,
            "CreateThread(start={start_address:#x}, suspended={suspended}) -> handle={handle:#x}, tid={assigned_tid}"
        );
        handle
    })
}

pub(crate) extern "win64" fn ResumeThread(handle: usize) -> u32 {
    let Some(obj) = WAITABLES.read().get(&handle).cloned() else {
        return 0;
    };
    if let WaitableObject::Thread {
        suspend_count,
        start_gate,
        ..
    } = obj.as_ref()
    {
        let prev = suspend_count.load(Ordering::Acquire);
        if prev > 0 {
            let new_val = prev - 1;
            suspend_count.store(new_val, Ordering::Release);
            if new_val == 0 {
                *start_gate.0.lock() = true;
                start_gate.1.notify_all();
            }
        }
        prev
    } else {
        0
    }
}

pub(crate) extern "win64" fn SuspendThread(handle: usize) -> u32 {
    let Some(obj) = WAITABLES.read().get(&handle).cloned() else {
        return 0;
    };
    if let WaitableObject::Thread { suspend_count, .. } = obj.as_ref() {
        suspend_count.fetch_add(1, Ordering::AcqRel)
    } else {
        0
    }
}

pub(crate) extern "win64" fn ExitThread(exit_code: u32) -> ! {
    std::panic::panic_any(ThreadExitSignal(exit_code))
}

pub(crate) extern "win64" fn GetExitCodeThread(handle: usize, out_code: *mut u32) -> WinBool {
    if out_code.is_null() {
        return WIN_FALSE;
    }
    let Some(obj) = WAITABLES.read().get(&handle).cloned() else {
        return WIN_FALSE;
    };
    if let WaitableObject::Thread { exit_code, .. } = obj.as_ref() {
        // SAFETY: `out_code` non-nul.
        unsafe {
            *out_code = exit_code.load(Ordering::Acquire);
        }
        WIN_TRUE
    } else {
        WIN_FALSE
    }
}

/// TID Linux du thread désigné par `handle` (pseudo-handle courant inclus), si c'est un thread.
pub(crate) fn thread_tid_of_handle(handle: usize) -> Option<u32> {
    if handle == crate::process::PSEUDO_CURRENT_THREAD {
        // SAFETY: syscall `gettid` sans précondition.
        return Some(unsafe { libc::syscall(libc::SYS_gettid) } as u32);
    }
    let obj = WAITABLES.read().get(&handle).cloned()?;
    if let WaitableObject::Thread { tid, .. } = obj.as_ref() {
        Some(tid.load(Ordering::Acquire))
    } else {
        None
    }
}

/// `OpenThread` : nouveau handle vers le thread `tid` (thread invité connu, ou thread courant
/// non créé via `CreateThread`, par ex. le thread principal). `None` si le TID est inconnu.
pub(crate) fn open_thread_handle(tid: u32) -> Option<usize> {
    if tid == 0 {
        return None;
    }
    let existing = WAITABLES
        .read()
        .values()
        .find_map(|obj| match obj.as_ref() {
            WaitableObject::Thread { tid: t, .. } if t.load(Ordering::Acquire) == tid => {
                Some(Arc::clone(obj))
            }
            _ => None,
        });
    let obj = match existing {
        Some(obj) => obj,
        None => {
            // SAFETY: syscall `gettid` sans précondition.
            let me = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
            if tid != me {
                return None;
            }
            Arc::new(WaitableObject::Thread {
                done: AtomicBool::new(false),
                exit_code: AtomicU32::new(STILL_ACTIVE),
                tid: AtomicU32::new(tid),
                suspend_count: AtomicU32::new(0),
                start_gate: Arc::new((Mutex::new(true), Condvar::new())),
            })
        }
    };
    let handle = NEXT_WAIT_HANDLE.fetch_add(1, Ordering::AcqRel);
    WAITABLES.write().insert(handle, obj);
    Some(handle)
}

pub(crate) extern "win64" fn GetThreadId(handle: usize) -> u32 {
    if handle == crate::process::PSEUDO_CURRENT_THREAD {
        return thread_tid_of_handle(handle).unwrap_or(0);
    }
    if let Some(obj) = WAITABLES.read().get(&handle).cloned()
        && let WaitableObject::Thread { tid, .. } = obj.as_ref()
    {
        return tid.load(Ordering::Acquire);
    }
    0
}

pub(crate) extern "win64" fn SetThreadPriority(_handle: usize, _priority: i32) -> WinBool {
    WIN_TRUE
}

pub(crate) extern "win64" fn GetThreadPriority(_handle: usize) -> i32 {
    0
}

pub(crate) extern "win64" fn SetThreadAffinityMask(_handle: usize, mask: usize) -> usize {
    if mask == 0 { 1 } else { mask }
}

pub(crate) extern "win64" fn SetThreadIdealProcessor(_handle: usize, ideal: u32) -> u32 {
    ideal
}

pub(crate) extern "win64" fn SetThreadDescription(_handle: usize, _desc: *const u16) -> i32 {
    0 // S_OK
}

pub(crate) extern "win64" fn GetActiveProcessorCount(_group: u16) -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(8)
}

pub(crate) extern "win64" fn GetCurrentProcessorNumber() -> u32 {
    // SAFETY: `sched_getcpu` sans précondition sous Linux.
    let cpu = unsafe { libc::sched_getcpu() };
    cpu.max(0) as u32
}

pub(crate) extern "win64" fn GetProcessAffinityMask(
    _proc: usize,
    out_proc_mask: *mut usize,
    out_sys_mask: *mut usize,
) -> WinBool {
    let n = GetActiveProcessorCount(0).min(63);
    let mask = (1usize << n) - 1;
    if !out_proc_mask.is_null() {
        // SAFETY: non-nul.
        unsafe {
            *out_proc_mask = mask;
        }
    }
    if !out_sys_mask.is_null() {
        // SAFETY: non-nul.
        unsafe {
            *out_sys_mask = mask;
        }
    }
    WIN_TRUE
}

pub(crate) extern "win64" fn DuplicateHandle(
    _src_proc: usize,
    src_handle: usize,
    _dst_proc: usize,
    out_handle: *mut usize,
    _access: u32,
    _inherit: WinBool,
    _options: u32,
) -> WinBool {
    if out_handle.is_null() {
        return WIN_FALSE;
    }
    if let Some(obj) = WAITABLES.read().get(&src_handle).cloned() {
        let new_h = NEXT_WAIT_HANDLE.fetch_add(1, Ordering::AcqRel);
        WAITABLES.write().insert(new_h, obj);
        // SAFETY: `out_handle` non-nul.
        unsafe {
            *out_handle = new_h;
        }
        return WIN_TRUE;
    }
    // SAFETY: `out_handle` non-nul (pseudo-handle ou autre handle).
    unsafe {
        *out_handle = src_handle;
    }
    WIN_TRUE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn srwlock_condvar_waitonaddress_semaphore_and_wait_multiple_work_concurrently() {
        // 1. Producteur/Consommateur sur SRWLOCK + CONDITION_VARIABLE (100 items, zéro lost wakeup)
        let mut srw_slot = 0usize;
        let mut cv_slot = 0usize;
        let srw_ptr = (&raw mut srw_slot).cast::<c_void>();
        let cv_ptr = (&raw mut cv_slot).cast::<c_void>();
        InitializeSRWLock(srw_ptr);
        InitializeConditionVariable(cv_ptr);

        let queue = Arc::new(Mutex::new(Vec::<u32>::new()));
        let q_prod = Arc::clone(&queue);
        let srw_addr = srw_ptr as usize;
        let cv_addr = cv_ptr as usize;

        let producer = std::thread::spawn(move || {
            for i in 1..=100u32 {
                AcquireSRWLockExclusive(srw_addr as *mut c_void);
                q_prod.lock().push(i);
                ReleaseSRWLockExclusive(srw_addr as *mut c_void);
                WakeConditionVariable(cv_addr as *mut c_void);
            }
        });

        let mut sum = 0u64;
        let mut consumed = 0usize;
        AcquireSRWLockExclusive(srw_ptr);
        while consumed < 100 {
            while queue.lock().is_empty() {
                assert_eq!(
                    SleepConditionVariableSRW(cv_ptr, srw_ptr, 2000, 0),
                    WIN_TRUE
                );
            }
            for v in queue.lock().drain(..) {
                sum += u64::from(v);
                consumed += 1;
            }
        }
        ReleaseSRWLockExclusive(srw_ptr);
        producer.join().expect("producer join");
        assert_eq!(sum, 5050);

        // 2. Futex `WaitOnAddress` + `WakeByAddressAll`
        let flag = Arc::new(AtomicU64::new(0));
        let flag_w = Arc::clone(&flag);
        let waker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            flag_w.store(42, Ordering::Release);
            WakeByAddressAll(flag_w.as_ptr().cast());
        });
        let expected = 0u64;
        while flag.load(Ordering::Acquire) == 0 {
            assert_eq!(
                WaitOnAddress(
                    flag.as_ptr().cast::<u8>(),
                    (&raw const expected).cast::<u8>(),
                    8,
                    2000
                ),
                WIN_TRUE
            );
        }
        waker.join().expect("waker join");
        assert_eq!(flag.load(Ordering::Acquire), 42);

        // 3. Semaphore + WaitForMultipleObjects (bWaitAll = TRUE)
        let ev = create_event_handle(true, false);
        let sem = create_semaphore_impl(0, 4);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            SetEvent(ev);
            ReleaseSemaphore(sem, 1, std::ptr::null_mut());
        });
        let hs = [ev, sem];
        assert_eq!(
            WaitForMultipleObjects(2, hs.as_ptr(), WIN_TRUE, 2000),
            WAIT_OBJECT_0
        );
        t.join().expect("wait_multiple join");
        assert!(close_waitable_handle(ev));
        assert!(close_waitable_handle(sem));
    }
}
