//! Couche `XTaskQueue*` de `XThreadingImpl` : files composites, ports, comptage des handles,
//! waiters, moniteurs et file « process ». Les files simples, la soumission et la distribution
//! restent celles de `gdk_xs.rs` (un seul état de files dans le processus).

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use sb_common::abi::{E_INVALIDARG, E_POINTER, HResult, S_OK};
use sb_common::sb_debug;

use crate::gdk_xs;

const CHANNEL: &str = "gdk_classes";

/// `XTaskQueuePort::Work`.
pub(crate) const PORT_WORK: u32 = 0;
/// `XTaskQueuePort::Completion`.
pub(crate) const PORT_COMPLETION: u32 = 1;

/// `E_ABORT` (file terminée).
pub(crate) const E_ABORT: HResult = 0x8000_4004_u32 as i32;

static NEXT_COMPOSITE: AtomicUsize = AtomicUsize::new(0x7000_0001);
static NEXT_PORT: AtomicUsize = AtomicUsize::new(0x6000_0001);
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct PortRef {
    queue: usize,
    kind: u32,
}

#[derive(Clone, Copy)]
struct Monitor {
    queue: usize,
    context: usize,
    callback: usize,
}

#[derive(Default)]
struct Registry {
    /// File composite -> (port de travail, port de complétion).
    composites: HashMap<usize, (PortRef, PortRef)>,
    /// Handle de port -> port.
    ports: HashMap<usize, PortRef>,
    port_by_key: HashMap<PortRef, usize>,
    /// Compteurs de références au-delà de 1 (`XTaskQueueDuplicateHandle`).
    extra_refs: HashMap<usize, u32>,
    monitors: HashMap<u64, Monitor>,
    waiters: HashMap<u64, Arc<AtomicBool>>,
    process_override: Option<usize>,
}

static REG: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));

/// Lance un thread invité (TEB + TLS initialisés comme les workers de `gdk_xs`).
pub(crate) fn spawn_guest_thread(name: &str, f: impl FnOnce() + Send + 'static) -> bool {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let _ = sb_loader::init_thread_teb(0);
            sb_loader::module_manager().attach_current_thread_tls();
            f();
        })
        .is_ok()
}

fn is_composite(queue: usize) -> bool {
    REG.lock().composites.contains_key(&queue)
}

/// Vrai si `queue` est un handle de file valide (simple ou composite).
pub(crate) fn queue_exists(queue: usize) -> bool {
    gdk_xs::task_queue_exists(queue) || is_composite(queue)
}

/// Résout (file, port) vers la file simple et le port qui portent réellement les éléments.
fn resolve(queue: usize, port: u32) -> Option<(usize, u32)> {
    if port > PORT_COMPLETION {
        return None;
    }
    let (mut q, mut p) = (queue, port);
    for _ in 0..8 {
        let next = {
            let reg = REG.lock();
            reg.composites
                .get(&q)
                .map(|(w, c)| if p == PORT_WORK { *w } else { *c })
        };
        match next {
            Some(pr) => {
                q = pr.queue;
                p = pr.kind;
            }
            None => return gdk_xs::task_queue_exists(q).then_some((q, p)),
        }
    }
    None
}

/// File « process » courante (celle posée par `SetCurrentProcessTaskQueue`, sinon celle de
/// `gdk_xs`).
pub(crate) fn process_queue() -> usize {
    if let Some(q) = REG.lock().process_override {
        return q;
    }
    let mut h = 0usize;
    let _ = gdk_xs::XTaskQueueGetCurrentProcessTaskQueue(&mut h);
    h
}

/// `XTaskQueueSubmitDelayedCallback` : valide, soumet, puis prévient les moniteurs.
pub(crate) fn submit(
    queue: usize,
    port: u32,
    delay_ms: u32,
    context: *mut c_void,
    callback: usize,
) -> HResult {
    if callback == 0 {
        return E_INVALIDARG;
    }
    let Some((rq, rk)) = resolve(queue, port) else {
        return E_INVALIDARG;
    };
    let hr = gdk_xs::XTaskQueueSubmitDelayedCallback(rq, rk, delay_ms, context, callback);
    if hr == S_OK {
        notify_monitors(queue, port);
        if rq != queue {
            notify_monitors(rq, rk);
        }
    }
    hr
}

fn notify_monitors(queue: usize, port: u32) {
    let list: Vec<Monitor> = REG
        .lock()
        .monitors
        .values()
        .filter(|m| m.queue == queue)
        .copied()
        .collect();
    for m in list {
        // SAFETY: `XTaskQueueMonitorCallback` = `void CALLBACK(void* context, XTaskQueueHandle, XTaskQueuePort)`.
        let cb: unsafe extern "win64" fn(*mut c_void, usize, u32) =
            unsafe { core::mem::transmute(m.callback) };
        // SAFETY: appel d'un callback invité Win64 enregistré par le titre.
        unsafe { cb(m.context as *mut c_void, queue, port) };
    }
}

pub(crate) fn create(work: u32, completion: u32, out: *mut usize) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }
    if work > 3 || completion > 3 {
        return E_INVALIDARG;
    }
    gdk_xs::XTaskQueueCreate(work, completion, out)
}

pub(crate) fn create_composite(
    work_port: usize,
    completion_port: usize,
    out: *mut usize,
) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }
    let mut reg = REG.lock();
    let (Some(w), Some(c)) = (
        reg.ports.get(&work_port).copied(),
        reg.ports.get(&completion_port).copied(),
    ) else {
        return E_INVALIDARG;
    };
    let h = NEXT_COMPOSITE.fetch_add(1, Ordering::AcqRel);
    reg.composites.insert(h, (w, c));
    drop(reg);
    // SAFETY: `out` vérifié non-nul.
    unsafe { *out = h };
    sb_debug!(CHANNEL, "XTaskQueueCreateComposite -> {h:#x}");
    S_OK
}

pub(crate) fn get_port(queue: usize, port: u32, out: *mut usize) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }
    if port > PORT_COMPLETION || !queue_exists(queue) {
        return E_INVALIDARG;
    }
    let key = PortRef { queue, kind: port };
    let mut reg = REG.lock();
    let h = if let Some(h) = reg.port_by_key.get(&key) {
        *h
    } else {
        let h = NEXT_PORT.fetch_add(1, Ordering::AcqRel);
        reg.ports.insert(h, key);
        reg.port_by_key.insert(key, h);
        h
    };
    drop(reg);
    // SAFETY: `out` vérifié non-nul.
    unsafe { *out = h };
    S_OK
}

pub(crate) fn duplicate(queue: usize, out: *mut usize) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }
    if !queue_exists(queue) {
        return E_INVALIDARG;
    }
    *REG.lock().extra_refs.entry(queue).or_insert(0) += 1;
    // SAFETY: `out` vérifié non-nul ; le handle dupliqué est le même (comme le runtime réel).
    unsafe { *out = queue };
    S_OK
}

pub(crate) fn close(queue: usize) {
    if queue == process_queue() {
        return; // la file process vit autant que le processus
    }
    {
        let mut reg = REG.lock();
        if let Some(n) = reg.extra_refs.get_mut(&queue) {
            *n -= 1;
            if *n == 0 {
                reg.extra_refs.remove(&queue);
            }
            return;
        }
        if reg.composites.remove(&queue).is_some() {
            reg.monitors.retain(|_, m| m.queue != queue);
            return;
        }
    }
    if gdk_xs::task_queue_exists(queue) {
        REG.lock().monitors.retain(|_, m| m.queue != queue);
        gdk_xs::XTaskQueueCloseHandle(queue);
    }
}

pub(crate) fn dispatch(queue: usize, port: u32, timeout_ms: u32) -> u8 {
    let Some((rq, rk)) = resolve(queue, port) else {
        return 0;
    };
    gdk_xs::XTaskQueueDispatch(rq, rk, timeout_ms) as u8
}

pub(crate) fn terminate(queue: usize, wait: bool, context: usize, callback: usize) -> HResult {
    if !queue_exists(queue) {
        return E_INVALIDARG;
    }
    if !is_composite(queue) {
        let hr = gdk_xs::XTaskQueueTerminate(queue, i32::from(wait), core::ptr::null_mut(), 0);
        if hr != S_OK {
            return hr;
        }
    }
    if callback != 0 {
        let run = move || {
            // SAFETY: `XTaskQueueTerminatedCallback` = `void CALLBACK(void* context)`.
            let cb: unsafe extern "win64" fn(*mut c_void) =
                unsafe { core::mem::transmute(callback) };
            // SAFETY: appel d'un callback invité Win64.
            unsafe { cb(context as *mut c_void) };
        };
        if wait || !spawn_guest_thread("sb-xtq-terminate", run) {
            run();
        }
    }
    S_OK
}

pub(crate) fn register_monitor(
    queue: usize,
    context: usize,
    callback: usize,
    out_token: *mut u64,
) -> HResult {
    if out_token.is_null() {
        return E_POINTER;
    }
    if callback == 0 || !queue_exists(queue) {
        return E_INVALIDARG;
    }
    let t = NEXT_TOKEN.fetch_add(1, Ordering::AcqRel);
    REG.lock().monitors.insert(
        t,
        Monitor {
            queue,
            context,
            callback,
        },
    );
    // SAFETY: `out_token` vérifié non-nul.
    unsafe { *out_token = t };
    S_OK
}

pub(crate) fn unregister_monitor(_queue: usize, token: u64) {
    REG.lock().monitors.remove(&token);
}

pub(crate) fn register_waiter(
    queue: usize,
    port: u32,
    wait_handle: usize,
    context: usize,
    callback: usize,
    out_token: *mut u64,
) -> HResult {
    if out_token.is_null() {
        return E_POINTER;
    }
    if callback == 0 || port > PORT_COMPLETION || !queue_exists(queue) {
        return E_INVALIDARG;
    }
    let t = NEXT_TOKEN.fetch_add(1, Ordering::AcqRel);
    let stop = Arc::new(AtomicBool::new(false));
    REG.lock().waiters.insert(t, Arc::clone(&stop));
    let started = spawn_guest_thread("sb-xtq-waiter", move || {
        while !stop.load(Ordering::Acquire) && queue_exists(queue) {
            // 0 = WAIT_OBJECT_0 ; on sonde par tranches pour pouvoir se désinscrire.
            if crate::sync::WaitForSingleObject(wait_handle, 20) == 0 {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let _ = submit(queue, port, 0, context as *mut c_void, callback);
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    });
    if !started {
        REG.lock().waiters.remove(&t);
        return 0x8007_000E_u32 as i32; // E_OUTOFMEMORY
    }
    // SAFETY: `out_token` vérifié non-nul.
    unsafe { *out_token = t };
    S_OK
}

pub(crate) fn unregister_waiter(_queue: usize, token: u64) {
    if let Some(stop) = REG.lock().waiters.remove(&token) {
        stop.store(true, Ordering::Release);
    }
}

pub(crate) fn set_process_queue(queue: usize) {
    let mut reg = REG.lock();
    if queue == 0 {
        reg.process_override = None;
    } else if gdk_xs::task_queue_exists(queue) || reg.composites.contains_key(&queue) {
        reg.process_override = Some(queue);
    }
}

pub(crate) fn get_process_queue(out: *mut usize) -> u8 {
    if out.is_null() {
        return 0;
    }
    let q = process_queue();
    if q == 0 {
        return 0;
    }
    // SAFETY: `out` vérifié non-nul.
    unsafe { *out = q };
    1
}
