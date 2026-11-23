//! Classes du runtime GDK : `XThreadingImpl`, `XErrorImpl`, `XSystemImpl`, `XSystemAnalyticsImpl`,
//! `XGameRuntimeFeatureImpl`, `XGameImpl` et `XPersistentLocalStorageImpl` (voir `gdk_classes.rs`).
//!
//! Ordre des slots : IDL public du runtime (`IXThreadingImpl`, `IXErrorImpl`, `IXSystemImpl`..5,
//! `IXSystemAnalyticsImpl`, `IXGameRuntimeFeatureImpl`, `IXGameImpl`..3,
//! `IXPersistentLocalStorageImpl`..3). Hors ligne : file de tâches, moteur `XAsync` et espace de
//! stockage venant de `gdk_xs.rs` / `rt_core/*`, valeurs système fixes (jamais l'hôte).

mod taskq;
mod xasync;

use core::ffi::{CStr, c_char, c_void};
use std::cell::Cell;
use std::sync::OnceLock;

use parking_lot::Mutex;
use sb_common::abi::{E_INVALIDARG, E_NOTIMPL, E_POINTER, HResult, S_OK};
use sb_common::{ffi_guard, sb_debug, sb_info, sb_warn};

use super::ClassDef;
use crate::com::{
    ComHeader, Guid, VtblSlot, com_add_ref, com_query_interface_list, com_release, vslot,
};
use crate::gdk_xs::{self, XAsyncBlock, XPersistentLocalStorageSpaceInfo};

const CHANNEL: &str = "gdk_classes";

/// Construit un GUID à la compilation depuis `"xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"`.
const fn guid(s: &str) -> Guid {
    const fn hex(b: u8) -> u8 {
        match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => panic!("GUID invalide"),
        }
    }
    let b = s.as_bytes();
    assert!(b.len() == 36);
    const fn pair(b: &[u8], i: usize) -> u8 {
        (hex(b[i]) << 4) | hex(b[i + 1])
    }
    macro_rules! byte {
        ($i:expr) => {
            pair(b, $i)
        };
    }
    let d1 = u32::from_be_bytes([byte!(0), byte!(2), byte!(4), byte!(6)]);
    let d2 = u16::from_be_bytes([byte!(9), byte!(11)]);
    let d3 = u16::from_be_bytes([byte!(14), byte!(16)]);
    Guid::new(
        d1,
        d2,
        d3,
        [
            byte!(19),
            byte!(21),
            byte!(24),
            byte!(26),
            byte!(28),
            byte!(30),
            byte!(32),
            byte!(34),
        ],
    )
}

pub(crate) const CLSID_THREADING: Guid = guid("073b7dcb-1fcf-4030-94be-e3c9eb623428");
pub(crate) const CLSID_ERROR: Guid = guid("8ca467f7-22e8-4096-8456-bb8aa13f79d8");
pub(crate) const CLSID_SYSTEM: Guid = guid("e349bd1a-fc20-4e40-b99c-4178cc6b409f");
pub(crate) const CLSID_SYSTEM_ANALYTICS: Guid = guid("b884675d-b738-4a9c-815d-9a9a1e0c6c9b");
pub(crate) const CLSID_FEATURE: Guid = guid("8836fe87-edb9-4fe3-8dad-05f0d2cd5b40");
pub(crate) const CLSID_GAME: Guid = guid("973a344e-24bf-4d0f-8457-56c534892b29");
pub(crate) const CLSID_PLS: Guid = guid("f4faf4d4-2d04-4fce-b3e0-474a713a3e84");

const SYSTEM_EXTRA: [Guid; 4] = [
    guid("6fd71f09-7513-49f0-89bc-bfaf5df6f852"), // IXSystemImpl2
    guid("67ce4bfc-b1d1-4ac7-bc3a-cb9219a97a85"), // IXSystemImpl3
    guid("dadc2895-34b0-4ef5-a83e-45114d629b80"), // IXSystemImpl4
    guid("1861cf2e-e18b-4834-a9f5-b4a4e6efb4cf"), // IXSystemImpl5
];
const GAME_EXTRA: [Guid; 2] = [
    guid("50849859-0ad8-4f81-80e4-5bc78626f852"), // IXGameImpl2
    guid("2549f142-6419-4a06-97b5-931aab7c2f34"), // IXGameImpl3
];
const PLS_EXTRA: [Guid; 2] = [
    guid("d29411df-0794-4553-8b27-95fc02d0f75d"), // IXPersistentLocalStorageImpl2
    guid("41a4e10c-5a7e-41d9-8c37-37bde62a07d6"), // IXPersistentLocalStorageImpl3
];

/// `HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER)` = `E_NOT_SUFFICIENT_BUFFER`.
const E_NOT_SUFFICIENT_BUFFER: HResult = 0x8007_007A_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_DISK_FULL)`.
const E_DISK_FULL: HResult = 0x8007_0070_u32 as i32;

macro_rules! s {
    ($f:expr) => {
        vslot($f as *const c_void)
    };
}

macro_rules! define_class_plumbing {
    ($qi:ident, $label:literal, $ids:expr) => {
        extern "win64" fn $qi(
            this: *mut ComHeader,
            riid: *const Guid,
            ppv: *mut *mut c_void,
        ) -> HResult {
            com_query_interface_list($label, this, riid, ppv, $ids)
        }
    };
}

/// Objet singleton : un `ComHeader` seul (l'état est global au processus).
fn singleton(cell: &'static OnceLock<usize>, vtbl: *const VtblSlot) -> *mut ComHeader {
    *cell.get_or_init(|| Box::into_raw(Box::new(ComHeader::new(vtbl.cast()))) as usize)
        as *mut ComHeader
}

// ================= XThreadingImpl =================

const THREADING_SLOTS: usize = 3 + 28;
define_class_plumbing!(threading_qi, "XThreadingImpl", &[CLSID_THREADING]);

thread_local! {
    static TIME_SENSITIVE: Cell<bool> = const { Cell::new(false) };
}

extern "win64" fn t_get_status(_t: *mut ComHeader, b: *mut XAsyncBlock, wait: u8) -> HResult {
    ffi_guard!(CHANNEL, "XAsyncGetStatus", E_INVALIDARG, {
        xasync::get_status(b, wait != 0)
    })
}

extern "win64" fn t_get_result_size(
    _t: *mut ComHeader,
    b: *mut XAsyncBlock,
    out: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XAsyncGetResultSize", E_INVALIDARG, {
        xasync::get_result_size(b, out)
    })
}

extern "win64" fn t_cancel(_t: *mut ComHeader, b: *mut XAsyncBlock) {
    ffi_guard!(CHANNEL, "XAsyncCancel", (), { xasync::cancel(b) })
}

extern "win64" fn t_run(_t: *mut ComHeader, b: *mut XAsyncBlock, work: usize) -> HResult {
    ffi_guard!(CHANNEL, "XAsyncRun", E_INVALIDARG, { xasync::run(b, work) })
}

extern "win64" fn t_begin(
    _t: *mut ComHeader,
    b: *mut XAsyncBlock,
    context: usize,
    identity: usize,
    identity_name: *const u8,
    provider: usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XAsyncBegin", E_INVALIDARG, {
        xasync::begin(b, context, identity, identity_name, provider)
    })
}

extern "win64" fn t_schedule(_t: *mut ComHeader, b: *mut XAsyncBlock, delay: u32) -> HResult {
    ffi_guard!(CHANNEL, "XAsyncSchedule", E_INVALIDARG, {
        xasync::schedule(b, delay)
    })
}

extern "win64" fn t_complete(_t: *mut ComHeader, b: *mut XAsyncBlock, hr: HResult, size: usize) {
    ffi_guard!(CHANNEL, "XAsyncComplete", (), {
        xasync::complete(b, hr, size)
    })
}

extern "win64" fn t_get_result(
    _t: *mut ComHeader,
    b: *mut XAsyncBlock,
    identity: usize,
    size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XAsyncGetResult", E_INVALIDARG, {
        xasync::get_result(b, identity, size, buffer, used)
    })
}

extern "win64" fn t_tq_create(
    _t: *mut ComHeader,
    work: u32,
    completion: u32,
    out: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XTaskQueueCreate", E_INVALIDARG, {
        taskq::create(work, completion, out)
    })
}

extern "win64" fn t_tq_create_composite(
    _t: *mut ComHeader,
    work_port: usize,
    completion_port: usize,
    out: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XTaskQueueCreateComposite", E_INVALIDARG, {
        taskq::create_composite(work_port, completion_port, out)
    })
}

extern "win64" fn t_tq_get_port(
    _t: *mut ComHeader,
    queue: usize,
    port: u32,
    out: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XTaskQueueGetPort", E_INVALIDARG, {
        taskq::get_port(queue, port, out)
    })
}

extern "win64" fn t_tq_duplicate(_t: *mut ComHeader, queue: usize, out: *mut usize) -> HResult {
    ffi_guard!(CHANNEL, "XTaskQueueDuplicateHandle", E_INVALIDARG, {
        taskq::duplicate(queue, out)
    })
}

extern "win64" fn t_tq_dispatch(_t: *mut ComHeader, queue: usize, port: u32, timeout: u32) -> u8 {
    ffi_guard!(CHANNEL, "XTaskQueueDispatch", 0, {
        taskq::dispatch(queue, port, timeout)
    })
}

extern "win64" fn t_tq_close(_t: *mut ComHeader, queue: usize) {
    ffi_guard!(CHANNEL, "XTaskQueueCloseHandle", (), {
        taskq::close(queue)
    })
}

extern "win64" fn t_tq_submit(
    _t: *mut ComHeader,
    queue: usize,
    port: u32,
    context: *mut c_void,
    callback: usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XTaskQueueSubmitCallback", E_INVALIDARG, {
        taskq::submit(queue, port, 0, context, callback)
    })
}

extern "win64" fn t_tq_submit_delayed(
    _t: *mut ComHeader,
    queue: usize,
    port: u32,
    delay: u32,
    context: *mut c_void,
    callback: usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XTaskQueueSubmitDelayedCallback", E_INVALIDARG, {
        taskq::submit(queue, port, delay, context, callback)
    })
}

extern "win64" fn t_tq_register_waiter(
    _t: *mut ComHeader,
    queue: usize,
    port: u32,
    wait_handle: usize,
    context: usize,
    callback: usize,
    token: *mut u64,
) -> HResult {
    ffi_guard!(CHANNEL, "XTaskQueueRegisterWaiter", E_INVALIDARG, {
        taskq::register_waiter(queue, port, wait_handle, context, callback, token)
    })
}

extern "win64" fn t_tq_unregister_waiter(_t: *mut ComHeader, queue: usize, token: u64) {
    ffi_guard!(CHANNEL, "XTaskQueueUnregisterWaiter", (), {
        taskq::unregister_waiter(queue, token)
    })
}

extern "win64" fn t_tq_terminate(
    _t: *mut ComHeader,
    queue: usize,
    wait: u8,
    context: usize,
    callback: usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XTaskQueueTerminate", E_INVALIDARG, {
        taskq::terminate(queue, wait != 0, context, callback)
    })
}

extern "win64" fn t_tq_register_monitor(
    _t: *mut ComHeader,
    queue: usize,
    context: usize,
    callback: usize,
    token: *mut u64,
) -> HResult {
    ffi_guard!(CHANNEL, "XTaskQueueRegisterMonitor", E_INVALIDARG, {
        taskq::register_monitor(queue, context, callback, token)
    })
}

extern "win64" fn t_tq_unregister_monitor(_t: *mut ComHeader, queue: usize, token: u64) {
    ffi_guard!(CHANNEL, "XTaskQueueUnregisterMonitor", (), {
        taskq::unregister_monitor(queue, token)
    })
}

extern "win64" fn t_tq_get_process(_t: *mut ComHeader, out: *mut usize) -> u8 {
    ffi_guard!(CHANNEL, "XTaskQueueGetCurrentProcessTaskQueue", 0, {
        taskq::get_process_queue(out)
    })
}

extern "win64" fn t_tq_set_process(_t: *mut ComHeader, queue: usize) {
    ffi_guard!(CHANNEL, "XTaskQueueSetCurrentProcessTaskQueue", (), {
        taskq::set_process_queue(queue)
    })
}

extern "win64" fn t_set_time_sensitive(_t: *mut ComHeader, enable: u8) -> HResult {
    ffi_guard!(CHANNEL, "XThreadSetTimeSensitive", E_INVALIDARG, {
        TIME_SENSITIVE.with(|c| c.set(enable != 0));
        S_OK
    })
}

/// Slot 28 (`XTaskQueueIsEmpty`, privé) : mesuré sans argument ; la file est toujours « vide »
/// du point de vue de ce slot (réponse 0), journalisé une fois.
extern "win64" fn t_private_is_empty(_t: *mut ComHeader) -> u64 {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        sb_warn!(
            CHANNEL,
            "XThreadingImpl::slot 28 (XTaskQueueIsEmpty, privé) appelé -> 0"
        );
    });
    0
}

extern "win64" fn t_assert_not_time_sensitive(_t: *mut ComHeader) {
    ffi_guard!(CHANNEL, "XThreadAssertNotTimeSensitive", (), {
        if TIME_SENSITIVE.with(Cell::get) {
            sb_warn!(
                CHANNEL,
                "XThreadAssertNotTimeSensitive : appelé depuis un thread sensible au temps"
            );
        }
    })
}

extern "win64" fn t_is_time_sensitive(_t: *mut ComHeader) -> u8 {
    ffi_guard!(CHANNEL, "XThreadIsTimeSensitive", 0, {
        u8::from(TIME_SENSITIVE.with(Cell::get))
    })
}

static THREADING_VTBL: [VtblSlot; THREADING_SLOTS] = [
    s!(threading_qi),
    s!(com_add_ref),
    s!(com_release),
    s!(t_get_status),                // 3
    s!(t_get_result_size),           // 4
    s!(t_cancel),                    // 5
    s!(t_run),                       // 6
    s!(t_begin),                     // 7
    unimpl!(8),                      // 8  XAsyncBeginAlloc (privé, jamais appelé)
    s!(t_schedule),                  // 9
    s!(t_complete),                  // 10
    s!(t_get_result),                // 11
    s!(t_tq_create),                 // 12
    s!(t_tq_create_composite),       // 13
    s!(t_tq_get_port),               // 14
    s!(t_tq_duplicate),              // 15
    s!(t_tq_dispatch),               // 16
    s!(t_tq_close),                  // 17
    s!(t_tq_submit),                 // 18
    s!(t_tq_submit_delayed),         // 19
    s!(t_tq_register_waiter),        // 20
    s!(t_tq_unregister_waiter),      // 21
    s!(t_tq_terminate),              // 22
    s!(t_tq_register_monitor),       // 23
    s!(t_tq_unregister_monitor),     // 24
    s!(t_tq_get_process),            // 25
    s!(t_tq_set_process),            // 26
    s!(t_set_time_sensitive),        // 27
    s!(t_private_is_empty),          // 28
    s!(t_assert_not_time_sensitive), // 29
    s!(t_is_time_sensitive),         // 30
];

fn create_threading() -> *mut ComHeader {
    static OBJ: OnceLock<usize> = OnceLock::new();
    singleton(&OBJ, THREADING_VTBL.as_ptr())
}

// ================= XErrorImpl =================

const ERROR_SLOTS: usize = 3 + 3;
define_class_plumbing!(error_qi, "XErrorImpl", &[CLSID_ERROR]);

static ERROR_CALLBACK: Mutex<(usize, usize)> = Mutex::new((0, 0));
static ERROR_OPTIONS: Mutex<(u32, u32)> = Mutex::new((0, 0));

/// Slot 3 : `XErrorReport(HRESULT, const char*)` (« `_unknown_` » de la doc, `__PADDING__` de l'IDL).
extern "win64" fn e_report(_t: *mut ComHeader, hr: HResult, msg: *const c_char) -> HResult {
    ffi_guard!(CHANNEL, "XErrorReport", E_INVALIDARG, {
        let text = if msg.is_null() {
            String::new()
        } else {
            // SAFETY: `msg` est une chaîne C terminée par NUL fournie par l'appelant.
            unsafe { CStr::from_ptr(msg) }
                .to_string_lossy()
                .into_owned()
        };
        sb_warn!(CHANNEL, "XErrorReport(0x{:08X}, \"{text}\")", hr as u32);
        let (cb, ctx) = *ERROR_CALLBACK.lock();
        if cb != 0 {
            // SAFETY: `XErrorCallback` = `void CALLBACK(HRESULT, const char*, void*)`.
            let f: unsafe extern "win64" fn(HResult, *const c_char, *mut c_void) =
                unsafe { core::mem::transmute(cb) };
            // SAFETY: appel d'un callback invité ; `msg` reste valide pendant l'appel.
            unsafe { f(hr, msg, ctx as *mut c_void) };
        }
        S_OK
    })
}

extern "win64" fn e_set_callback(_t: *mut ComHeader, callback: usize, context: usize) {
    *ERROR_CALLBACK.lock() = (callback, context);
}

extern "win64" fn e_set_options(_t: *mut ComHeader, debugger_present: u32, not_present: u32) {
    *ERROR_OPTIONS.lock() = (debugger_present, not_present);
    sb_debug!(
        CHANNEL,
        "XErrorSetOptions({debugger_present:#x}, {not_present:#x}) (informatif : aucun arrêt forcé)"
    );
}

static ERROR_VTBL: [VtblSlot; ERROR_SLOTS] = [
    s!(error_qi),
    s!(com_add_ref),
    s!(com_release),
    s!(e_report),
    s!(e_set_callback),
    s!(e_set_options),
];

fn create_error() -> *mut ComHeader {
    static OBJ: OnceLock<usize> = OnceLock::new();
    singleton(&OBJ, ERROR_VTBL.as_ptr())
}

// ================= XSystemImpl =================

/// Slots 3-8 : l'IDL public (IXSystemImpl..5) ne déclare rien au-delà de `AllowFullDownloadBandwidth`.
const SYSTEM_SLOTS: usize = 3 + 6;
define_class_plumbing!(system_qi, "XSystemImpl", &SYSTEM_QI_IDS);
const SYSTEM_QI_IDS: [Guid; 5] = [
    CLSID_SYSTEM,
    SYSTEM_EXTRA[0],
    SYSTEM_EXTRA[1],
    SYSTEM_EXTRA[2],
    SYSTEM_EXTRA[3],
];

/// `XSystemConsoleIdBytes` = 40 (NUL compris) ; identifiant fixe, sans rapport avec l'hôte.
const CONSOLE_ID: &str = "SB0000000000000000000000000000000000001";
/// `XSystemXboxLiveSandboxIdMaxBytes` = 16.
const SANDBOX_ID: &str = "RETAIL";
/// `XSystemAppSpecificDeviceIdBytes` = 45.
const APP_DEVICE_ID: &str = "SB-APP-DEVICE-0000-0000-0000-000000000001";
const _: () = {
    assert!(CONSOLE_ID.len() < 40);
    assert!(SANDBOX_ID.len() < 16);
    assert!(APP_DEVICE_ID.len() < 45);
};

/// Écrit `text` (NUL compris) dans le tampon `(size, buf)` ; `used` reçoit la taille requise.
fn write_id_string(text: &str, size: i32, buf: *mut u8, used: *mut usize) -> HResult {
    let needed = text.len() + 1;
    if buf.is_null() || size < 0 {
        return E_INVALIDARG;
    }
    if !used.is_null() {
        // SAFETY: `used` vérifié non-nul.
        unsafe { *used = needed };
    }
    if (size as usize) < needed {
        return E_NOT_SUFFICIENT_BUFFER;
    }
    // SAFETY: `buf` dispose d'au moins `needed` octets (`size >= needed`).
    unsafe {
        core::ptr::copy_nonoverlapping(text.as_ptr(), buf, text.len());
        *buf.add(text.len()) = 0;
    }
    S_OK
}

extern "win64" fn sys_get_console_id(
    _t: *mut ComHeader,
    size: i32,
    buf: *mut u8,
    used: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XSystemGetConsoleId", E_INVALIDARG, {
        write_id_string(CONSOLE_ID, size, buf, used)
    })
}

extern "win64" fn sys_get_sandbox_id(
    _t: *mut ComHeader,
    size: i32,
    buf: *mut u8,
    used: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XSystemGetXboxLiveSandboxId", E_INVALIDARG, {
        write_id_string(SANDBOX_ID, size, buf, used)
    })
}

extern "win64" fn sys_get_app_device_id(
    _t: *mut ComHeader,
    size: i32,
    buf: *mut u8,
    used: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XSystemGetAppSpecificDeviceId", E_INVALIDARG, {
        write_id_string(APP_DEVICE_ID, size, buf, used)
    })
}

/// Callback posé par `XSystemHandleTrack` (`callback`, `context`) ; les classes qui créent des
/// handles peuvent l'appeler via [`notify_handle`].
static HANDLE_TRACKER: Mutex<(usize, usize)> = Mutex::new((0, 0));

/// Notifie le suivi de handles (`XSystemHandleCallbackReason` : 0 créé, 1 détruit).
#[allow(dead_code)]
pub(crate) fn notify_handle(handle: usize, handle_type: u32, reason: u32) {
    let (cb, ctx) = *HANDLE_TRACKER.lock();
    if cb != 0 {
        // SAFETY: `XSystemHandleCallback` = `void CALLBACK(XSystemHandle, XSystemHandleType, reason, void*)`.
        let f: unsafe extern "win64" fn(usize, u32, u32, *mut c_void) =
            unsafe { core::mem::transmute(cb) };
        // SAFETY: appel d'un callback invité enregistré par le titre.
        unsafe { f(handle, handle_type, reason, ctx as *mut c_void) };
    }
}

extern "win64" fn sys_handle_track(_t: *mut ComHeader, callback: usize, context: usize) -> HResult {
    ffi_guard!(CHANNEL, "XSystemHandleTrack", E_INVALIDARG, {
        if callback == 0 {
            return E_INVALIDARG;
        }
        *HANDLE_TRACKER.lock() = (callback, context);
        S_OK
    })
}

/// Un handle non-NUL est considéré valide : les classes ne partagent pas de registre commun
/// (les pointeurs ne sont jamais déréférencés).
extern "win64" fn sys_is_handle_valid(_t: *mut ComHeader, handle: usize) -> u8 {
    ffi_guard!(CHANNEL, "XSystemIsHandleValid", 0, {
        u8::from(handle != 0)
    })
}

extern "win64" fn sys_allow_full_bandwidth(_t: *mut ComHeader, enable: u8) {
    sb_debug!(
        CHANNEL,
        "XSystemAllowFullDownloadBandwidth({}) (hors ligne : sans effet)",
        enable != 0
    );
}

static SYSTEM_VTBL: [VtblSlot; SYSTEM_SLOTS] = [
    s!(system_qi),
    s!(com_add_ref),
    s!(com_release),
    s!(sys_get_console_id),
    s!(sys_get_sandbox_id),
    s!(sys_get_app_device_id),
    s!(sys_handle_track),
    s!(sys_is_handle_valid),
    s!(sys_allow_full_bandwidth),
];

fn create_system() -> *mut ComHeader {
    static OBJ: OnceLock<usize> = OnceLock::new();
    singleton(&OBJ, SYSTEM_VTBL.as_ptr())
}

// ================= XSystemAnalyticsImpl =================

const ANALYTICS_SLOTS: usize = 3 + 1;
define_class_plumbing!(
    analytics_qi,
    "XSystemAnalyticsImpl",
    &[CLSID_SYSTEM_ANALYTICS]
);

/// `XVersion` du GDK public.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct XVersion {
    pub major: u16,
    pub minor: u16,
    pub build: u16,
    pub revision: u16,
}

/// `XSystemAnalyticsInfo` du GDK public.
#[repr(C)]
pub(crate) struct XSystemAnalyticsInfo {
    pub os_version: XVersion,
    pub hosting_os_version: XVersion,
    pub family: [u8; 64],
    pub form: [u8; 64],
}

const _: () = {
    assert!(size_of::<XVersion>() == 8);
    assert!(size_of::<XSystemAnalyticsInfo>() == 144);
    assert!(core::mem::offset_of!(XSystemAnalyticsInfo, family) == 16);
    assert!(core::mem::offset_of!(XSystemAnalyticsInfo, form) == 80);
};

fn fixed_cstr<const N: usize>(s: &str) -> [u8; N] {
    let mut a = [0u8; N];
    a[..s.len()].copy_from_slice(s.as_bytes());
    a
}

/// `XSystemGetAnalyticsInfo` : la struct de retour est passée par adresse cachée (arg 1 après
/// `this`) et renvoyée aussi en `rax` (convention MSVC pour une méthode membre).
extern "win64" fn an_get_info(
    _t: *mut ComHeader,
    out: *mut XSystemAnalyticsInfo,
) -> *mut XSystemAnalyticsInfo {
    ffi_guard!(CHANNEL, "XSystemGetAnalyticsInfo", core::ptr::null_mut(), {
        if out.is_null() {
            return core::ptr::null_mut();
        }
        // Console émulée : Xbox Series X (cohérent avec `XSystemGetDeviceType`).
        let info = XSystemAnalyticsInfo {
            os_version: XVersion {
                major: 10,
                minor: 0,
                build: 25398,
                revision: 0,
            },
            hosting_os_version: XVersion::default(),
            family: fixed_cstr("Xbox"),
            form: fixed_cstr("XboxSeriesX"),
        };
        // SAFETY: `out` non-nul, pointe vers un `XSystemAnalyticsInfo` de l'appelant.
        unsafe { out.write_unaligned(info) };
        out
    })
}

static ANALYTICS_VTBL: [VtblSlot; ANALYTICS_SLOTS] = [
    s!(analytics_qi),
    s!(com_add_ref),
    s!(com_release),
    s!(an_get_info),
];

fn create_analytics() -> *mut ComHeader {
    static OBJ: OnceLock<usize> = OnceLock::new();
    singleton(&OBJ, ANALYTICS_VTBL.as_ptr())
}

// ================= XGameRuntimeFeatureImpl =================

const FEATURE_SLOTS: usize = 3 + 1;
define_class_plumbing!(feature_qi, "XGameRuntimeFeatureImpl", &[CLSID_FEATURE]);

const CLSID_ACCESSIBILITY: Guid = guid("3e241977-6237-41e9-8559-102c2d9983f1");
const CLSID_APP_CAPTURE: Guid = guid("a4f1aee2-4bf1-4485-b008-a7c26d52ac27");
const CLSID_DISPLAY: Guid = guid("03f0fe74-fdd9-4e5c-b630-f9339c47acc5");
const CLSID_GAME_INVITE: Guid = guid("0651aae2-4012-4077-bf84-8b9097090e2c");
const CLSID_GAME_SAVE: Guid = guid("704c3f58-e629-4cc2-b197-30511b996fe2");
const CLSID_GAME_UI: Guid = guid("dfcd4649-4ff8-4043-ba07-35d607df98b0");
const CLSID_LAUNCHER: Guid = guid("1b339674-328d-4283-a200-3171f18d3639");
const CLSID_NETWORKING: Guid = guid("37e56907-2f10-41e8-b72f-36edb185331a");
const CLSID_PACKAGE: Guid = guid("af406016-e850-4aa8-a88d-2f3dcb9dac7e");
const CLSID_STORE: Guid = guid("0dd112ac-7c24-448c-b92b-3960fb5bd30c");
const CLSID_USER: Guid = guid("01acd177-91f9-4763-a38e-ccbb55ce32e0");
const CLSID_GAME_EVENT: Guid = guid("bbfbdcc7-bfe7-409b-a5ca-edf054960b4d");
const CLSID_GAME_STREAMING: Guid = guid("0a2192aa-b2d5-4d58-83be-383b6d80799e");

/// `XGameRuntimeFeature` (ordre de l'énumération publique) -> classe qui sert ses fonctions.
/// `XSpeechSynthesizer` passe par `XAccessibilityImpl` ; `XAsync`, `XAsyncProvider`, `XTaskQueue`
/// et `XThread` par `XThreadingImpl`.
const FEATURE_CLASSES: [Guid; 22] = [
    CLSID_ACCESSIBILITY,  // XAccessibility
    CLSID_APP_CAPTURE,    // XAppCapture
    CLSID_THREADING,      // XAsync
    CLSID_THREADING,      // XAsyncProvider
    CLSID_DISPLAY,        // XDisplay
    CLSID_GAME,           // XGame
    CLSID_GAME_INVITE,    // XGameInvite
    CLSID_GAME_SAVE,      // XGameSave
    CLSID_GAME_UI,        // XGameUI
    CLSID_LAUNCHER,       // XLauncher
    CLSID_NETWORKING,     // XNetworking
    CLSID_PACKAGE,        // XPackage
    CLSID_PLS,            // XPersistentLocalStorage
    CLSID_ACCESSIBILITY,  // XSpeechSynthesizer
    CLSID_STORE,          // XStore
    CLSID_SYSTEM,         // XSystem
    CLSID_THREADING,      // XTaskQueue
    CLSID_THREADING,      // XThread
    CLSID_USER,           // XUser
    CLSID_ERROR,          // XError
    CLSID_GAME_EVENT,     // XGameEvent
    CLSID_GAME_STREAMING, // XGameStreaming
];

/// `XGameRuntimeIsFeatureAvailable` : vrai exactement quand `QueryApiImpl` sert la classe du
/// groupe de fonctions. `XDisplay` suit la même règle : la documentation le réserve aux
/// consoles, or l'appareil émulé se déclare Xbox Series X (`XSystemGetDeviceType`).
fn feature_available(feature: u32) -> bool {
    FEATURE_CLASSES
        .get(feature as usize)
        .is_some_and(super::is_class_served)
}

extern "win64" fn feat_is_available(_t: *mut ComHeader, feature: u32) -> u8 {
    ffi_guard!(CHANNEL, "XGameRuntimeIsFeatureAvailable", 0, {
        u8::from(feature_available(feature))
    })
}

static FEATURE_VTBL: [VtblSlot; FEATURE_SLOTS] = [
    s!(feature_qi),
    s!(com_add_ref),
    s!(com_release),
    s!(feat_is_available),
];

fn create_feature() -> *mut ComHeader {
    static OBJ: OnceLock<usize> = OnceLock::new();
    singleton(&OBJ, FEATURE_VTBL.as_ptr())
}

// ================= XGameImpl =================

const GAME_SLOTS: usize = 3 + 3;
define_class_plumbing!(game_qi, "XGameImpl", &GAME_QI_IDS);
const GAME_QI_IDS: [Guid; 3] = [CLSID_GAME, GAME_EXTRA[0], GAME_EXTRA[1]];

/// Identifiant de titre quand `MicrosoftGame.Config` est introuvable (valeur fixe, « SBRG »).
pub(crate) const FALLBACK_TITLE_ID: u32 = 0x5342_5247;

/// Extrait `<TitleId>` (hexadécimal, comme dans `MicrosoftGame.Config`).
fn parse_title_id(xml: &str) -> Option<u32> {
    let m = sb_vfs::AppManifest::parse_microsoft_game_config(xml).ok()?;
    let raw = m.title_id?;
    let t = raw.trim();
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u32::from_str_radix(t, 16).ok()
}

fn title_id() -> u32 {
    for name in ["game:/MicrosoftGame.Config", "game:/microsoftgame.config"] {
        if let Some(path) = crate::file_io::resolve_vfs_path(name)
            && let Ok(xml) = std::fs::read_to_string(&path)
            && let Some(id) = parse_title_id(&xml)
        {
            return id;
        }
    }
    FALLBACK_TITLE_ID
}

extern "win64" fn game_get_title_id(_t: *mut ComHeader, out: *mut u32) -> HResult {
    ffi_guard!(CHANNEL, "XGameGetXboxTitleId", E_INVALIDARG, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: `out` vérifié non-nul.
        unsafe { *out = title_id() };
        S_OK
    })
}

fn cstr_or_empty(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        // SAFETY: chaîne C terminée par NUL fournie par l'appelant.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// `XLaunchNewGame` (void) : aucun processus n'est lancé hors ligne ; la demande est journalisée.
extern "win64" fn game_launch_new(
    _t: *mut ComHeader,
    exe: *const c_char,
    args: *const c_char,
    _user: usize,
) {
    ffi_guard!(CHANNEL, "XLaunchNewGame", (), {
        sb_info!(
            CHANNEL,
            "XLaunchNewGame(exe=\"{}\", args=\"{}\") ignoré (aucun lancement hors ligne)",
            cstr_or_empty(exe),
            cstr_or_empty(args)
        );
    })
}

/// `XLaunchRestartOnCrash` : `reserved` doit valoir 0 ; rien n'est enregistré.
extern "win64" fn game_restart_on_crash(
    _t: *mut ComHeader,
    args: *const c_char,
    reserved: u32,
) -> HResult {
    ffi_guard!(CHANNEL, "XLaunchRestartOnCrash", E_INVALIDARG, {
        if reserved != 0 {
            return E_INVALIDARG;
        }
        sb_debug!(
            CHANNEL,
            "XLaunchRestartOnCrash(args=\"{}\") accepté, aucun redémarrage enregistré",
            cstr_or_empty(args)
        );
        S_OK
    })
}

static GAME_VTBL: [VtblSlot; GAME_SLOTS] = [
    s!(game_qi),
    s!(com_add_ref),
    s!(com_release),
    s!(game_get_title_id),
    s!(game_launch_new),
    s!(game_restart_on_crash),
];

fn create_game() -> *mut ComHeader {
    static OBJ: OnceLock<usize> = OnceLock::new();
    singleton(&OBJ, GAME_VTBL.as_ptr())
}

// ================= XPersistentLocalStorageImpl =================

const PLS_SLOTS: usize = 3 + 6;
define_class_plumbing!(pls_qi, "XPersistentLocalStorageImpl", &PLS_QI_IDS);
const PLS_QI_IDS: [Guid; 3] = [CLSID_PLS, PLS_EXTRA[0], PLS_EXTRA[1]];

extern "win64" fn pls_get_path_size(_t: *mut ComHeader, out: *mut usize) -> HResult {
    ffi_guard!(CHANNEL, "XPersistentLocalStorageGetPathSize", E_POINTER, {
        gdk_xs::XPersistentLocalStorageGetPathSize(out)
    })
}

extern "win64" fn pls_get_path(
    _t: *mut ComHeader,
    size: usize,
    out: *mut u8,
    used: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XPersistentLocalStorageGetPath", E_INVALIDARG, {
        gdk_xs::XPersistentLocalStorageGetPath(size, out, used)
    })
}

extern "win64" fn pls_get_space_info(
    _t: *mut ComHeader,
    out: *mut XPersistentLocalStorageSpaceInfo,
) -> HResult {
    ffi_guard!(CHANNEL, "XPersistentLocalStorageGetSpaceInfo", E_POINTER, {
        gdk_xs::XPersistentLocalStorageGetSpaceInfo(out)
    })
}

/// Fournisseur de `XPersistentLocalStoragePromptUserForSpaceAsync` : `context` = octets demandés.
/// Aucune interface hors ligne : la demande réussit si le quota la contient, sinon disque plein.
unsafe extern "win64" fn prompt_provider(
    op: u32,
    data: *const xasync::XAsyncProviderData,
) -> HResult {
    if op != 1 || data.is_null() {
        return S_OK;
    }
    // SAFETY: `data` fourni par le moteur XAsync, valide pendant l'appel.
    let requested = unsafe { (*data).context } as usize as u64;
    let mut info = XPersistentLocalStorageSpaceInfo {
        available_free_bytes: 0,
        total_free_bytes: 0,
        used_bytes: 0,
        total_bytes: 0,
    };
    if gdk_xs::XPersistentLocalStorageGetSpaceInfo(&mut info) != S_OK {
        return E_DISK_FULL;
    }
    if requested <= info.total_bytes {
        S_OK
    } else {
        E_DISK_FULL
    }
}

extern "win64" fn pls_prompt_async(
    _t: *mut ComHeader,
    requested: u64,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "XPersistentLocalStoragePromptUserForSpaceAsync",
        E_INVALIDARG,
        {
            if block.is_null() {
                return E_INVALIDARG;
            }
            xasync::begin_and_schedule(
                block,
                requested as usize,
                pls_prompt_async as *const () as usize,
                prompt_provider,
            )
        }
    )
}

extern "win64" fn pls_prompt_result(_t: *mut ComHeader, block: *mut XAsyncBlock) -> HResult {
    ffi_guard!(
        CHANNEL,
        "XPersistentLocalStoragePromptUserForSpaceResult",
        E_INVALIDARG,
        {
            xasync::get_result(
                block,
                pls_prompt_async as *const () as usize,
                0,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        }
    )
}

/// `XPersistentLocalStorageMountForPackage` : non pris en charge (`E_NOTIMPL`, handle mis à 0).
extern "win64" fn pls_mount_for_package(
    _t: *mut ComHeader,
    _package_id: *const c_char,
    mount: *mut usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "XPersistentLocalStorageMountForPackage",
        E_NOTIMPL,
        {
            if !mount.is_null() {
                // SAFETY: `mount` vérifié non-nul.
                unsafe { *mount = 0 };
            }
            E_NOTIMPL
        }
    )
}

static PLS_VTBL: [VtblSlot; PLS_SLOTS] = [
    s!(pls_qi),
    s!(com_add_ref),
    s!(com_release),
    s!(pls_get_path_size),
    s!(pls_get_path),
    s!(pls_get_space_info),
    s!(pls_prompt_async),
    s!(pls_prompt_result),
    s!(pls_mount_for_package),
];

fn create_pls() -> *mut ComHeader {
    static OBJ: OnceLock<usize> = OnceLock::new();
    singleton(&OBJ, PLS_VTBL.as_ptr())
}

// ================= Enregistrement =================

/// Classes fournies par ce module (enregistrées dans `gdk_classes::registry`).
pub fn classes() -> Vec<ClassDef> {
    vec![
        ClassDef {
            name: "XThreadingImpl",
            clsid: CLSID_THREADING,
            extra_iids: &[],
            create: create_threading,
        },
        ClassDef {
            name: "XErrorImpl",
            clsid: CLSID_ERROR,
            extra_iids: &[],
            create: create_error,
        },
        ClassDef {
            name: "XSystemImpl",
            clsid: CLSID_SYSTEM,
            extra_iids: &SYSTEM_EXTRA,
            create: create_system,
        },
        ClassDef {
            name: "XSystemAnalyticsImpl",
            clsid: CLSID_SYSTEM_ANALYTICS,
            extra_iids: &[],
            create: create_analytics,
        },
        ClassDef {
            name: "XGameRuntimeFeatureImpl",
            clsid: CLSID_FEATURE,
            extra_iids: &[],
            create: create_feature,
        },
        ClassDef {
            name: "XGameImpl",
            clsid: CLSID_GAME,
            extra_iids: &GAME_EXTRA,
            create: create_game,
        },
        ClassDef {
            name: "XPersistentLocalStorageImpl",
            clsid: CLSID_PLS,
            extra_iids: &PLS_EXTRA,
            create: create_pls,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::IID_IUNKNOWN;
    use crate::gdk_classes::query;
    use core::sync::atomic::{AtomicU32, Ordering};

    fn obj(clsid: &Guid) -> *mut ComHeader {
        let mut o: *mut c_void = core::ptr::null_mut();
        assert_eq!(query(clsid, clsid, &mut o), 0);
        assert!(!o.is_null());
        o.cast()
    }

    /// Appelle le slot `$n` de l'objet `$o` : `call!(o, 3, fn(A, B) -> R, a, b)`.
    macro_rules! call {
        ($o:expr, $n:expr, fn($($t:ty),*) -> $r:ty $(, $a:expr)*) => {{
            let o: *mut ComHeader = $o;
            // SAFETY: vtable statique de la classe ; signature choisie par le test.
            unsafe {
                let vt = (*o).vtbl.cast::<*const c_void>();
                let f: extern "win64" fn(*mut ComHeader $(, $t)*) -> $r =
                    core::mem::transmute(*vt.add($n));
                f(o $(, $a)*)
            }
        }};
    }

    fn check_iunknown(clsid: &Guid, extra: &[Guid]) {
        let o = obj(clsid);
        let before = unsafe { (*o).ref_count.load(Ordering::SeqCst) };
        let n = call!(o, 1, fn() -> u32);
        assert_eq!(n, before + 1);
        let _ = call!(o, 2, fn() -> u32);
        for iid in extra.iter().chain([clsid, &IID_IUNKNOWN]) {
            let mut out: *mut c_void = core::ptr::null_mut();
            let hr = call!(
                o,
                0,
                fn(*const Guid, *mut *mut c_void) -> HResult,
                iid,
                &mut out
            );
            assert_eq!(hr, S_OK);
            assert_eq!(out.cast::<ComHeader>(), o);
            let _ = call!(o, 2, fn() -> u32);
        }
        let bad = guid("00000000-dead-beef-0000-000000000000");
        let mut out: *mut c_void = core::ptr::null_mut();
        let hr = call!(
            o,
            0,
            fn(*const Guid, *mut *mut c_void) -> HResult,
            &bad,
            &mut out
        );
        assert_eq!(hr, sb_common::abi::E_NOINTERFACE);
        assert!(out.is_null());
        // QueryApiImpl refuse aussi un IID inconnu.
        let mut o2: *mut c_void = core::ptr::null_mut();
        assert_eq!(query(clsid, &bad, &mut o2), sb_common::abi::E_NOINTERFACE);
    }

    #[test]
    fn guid_parser_and_registry() {
        assert_eq!(CLSID_THREADING.data1, 0x073b_7dcb);
        assert_eq!(CLSID_THREADING.data4[0], 0x94);
        assert_eq!(CLSID_THREADING.data4[7], 0x28);
        let names: Vec<_> = classes().iter().map(|c| c.name).collect();
        assert_eq!(names.len(), 7);
        for c in classes() {
            assert!(crate::gdk_classes::is_class_served(&c.clsid), "{}", c.name);
        }
        assert_eq!(THREADING_VTBL.len(), 31);
        assert_eq!(ERROR_VTBL.len(), 6);
        assert_eq!(SYSTEM_VTBL.len(), 9);
        assert_eq!(ANALYTICS_VTBL.len(), 4);
        assert_eq!(FEATURE_VTBL.len(), 4);
        assert_eq!(GAME_VTBL.len(), 6);
        assert_eq!(PLS_VTBL.len(), 9);
    }

    #[test]
    fn iunknown_on_every_class() {
        check_iunknown(&CLSID_THREADING, &[]);
        check_iunknown(&CLSID_ERROR, &[]);
        check_iunknown(&CLSID_SYSTEM, &SYSTEM_EXTRA);
        check_iunknown(&CLSID_SYSTEM_ANALYTICS, &[]);
        check_iunknown(&CLSID_FEATURE, &[]);
        check_iunknown(&CLSID_GAME, &GAME_EXTRA);
        check_iunknown(&CLSID_PLS, &PLS_EXTRA);
    }

    static TEST_CB: AtomicU32 = AtomicU32::new(0);
    /// Les tests de files de tâches partagent `TEST_CB` : on les exécute l'un après l'autre.
    static TEST_SERIAL: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    unsafe extern "win64" fn on_task(ctx: *mut c_void, canceled: u8) {
        assert_eq!(canceled, 0);
        TEST_CB.fetch_add(ctx as usize as u32, Ordering::SeqCst);
    }

    #[test]
    fn threading_task_queue_slots() {
        let _serial = TEST_SERIAL.lock();
        let o = obj(&CLSID_THREADING);
        let mut q = 0usize;
        assert_eq!(
            call!(o, 12, fn(u32, u32, *mut usize) -> HResult, 2, 2, &mut q),
            S_OK
        );
        assert_eq!(
            call!(o, 12, fn(u32, u32, *mut usize) -> HResult, 9, 0, &mut q),
            E_INVALIDARG
        );
        assert_eq!(
            call!(
                o,
                12,
                fn(u32, u32, *mut usize) -> HResult,
                0,
                0,
                core::ptr::null_mut()
            ),
            E_POINTER
        );
        let before = TEST_CB.load(Ordering::SeqCst);
        let cb = on_task as *const () as usize;
        assert_eq!(
            call!(
                o,
                18,
                fn(usize, u32, *mut c_void, usize) -> HResult,
                q,
                0,
                5usize as *mut c_void,
                cb
            ),
            S_OK
        );
        assert_eq!(
            call!(
                o,
                18,
                fn(usize, u32, *mut c_void, usize) -> HResult,
                q,
                0,
                core::ptr::null_mut(),
                0
            ),
            E_INVALIDARG
        );
        assert_eq!(
            call!(
                o,
                18,
                fn(usize, u32, *mut c_void, usize) -> HResult,
                q,
                7,
                core::ptr::null_mut(),
                cb
            ),
            E_INVALIDARG
        );
        assert_eq!(
            call!(
                o,
                19,
                fn(usize, u32, u32, *mut c_void, usize) -> HResult,
                q,
                1,
                0,
                7usize as *mut c_void,
                cb
            ),
            S_OK
        );
        assert_eq!(call!(o, 16, fn(usize, u32, u32) -> u8, q, 0, 0), 1);
        assert_eq!(call!(o, 16, fn(usize, u32, u32) -> u8, q, 0, 0), 0);
        assert_eq!(call!(o, 16, fn(usize, u32, u32) -> u8, q, 1, 0), 1);
        assert_eq!(TEST_CB.load(Ordering::SeqCst), before + 12);

        // Ports et file composite : le travail du composite va sur la file `q`.
        let (mut wp, mut cp) = (0usize, 0usize);
        assert_eq!(
            call!(o, 14, fn(usize, u32, *mut usize) -> HResult, q, 0, &mut wp),
            S_OK
        );
        assert_eq!(
            call!(o, 14, fn(usize, u32, *mut usize) -> HResult, q, 1, &mut cp),
            S_OK
        );
        assert_eq!(
            call!(o, 14, fn(usize, u32, *mut usize) -> HResult, q, 2, &mut cp),
            E_INVALIDARG
        );
        let mut comp = 0usize;
        assert_eq!(
            call!(
                o,
                13,
                fn(usize, usize, *mut usize) -> HResult,
                wp,
                cp,
                &mut comp
            ),
            S_OK
        );
        assert_eq!(
            call!(
                o,
                13,
                fn(usize, usize, *mut usize) -> HResult,
                1,
                cp,
                &mut comp
            ),
            E_INVALIDARG
        );
        assert_eq!(
            call!(
                o,
                18,
                fn(usize, u32, *mut c_void, usize) -> HResult,
                comp,
                0,
                3usize as *mut c_void,
                cb
            ),
            S_OK
        );
        assert_eq!(call!(o, 16, fn(usize, u32, u32) -> u8, comp, 0, 0), 1);
        assert_eq!(TEST_CB.load(Ordering::SeqCst), before + 15);

        // Doublon : la fermeture ne détruit la file qu'au dernier handle.
        let mut dup = 0usize;
        assert_eq!(
            call!(o, 15, fn(usize, *mut usize) -> HResult, q, &mut dup),
            S_OK
        );
        assert_eq!(dup, q);
        call!(o, 17, fn(usize) -> (), q);
        assert!(taskq::queue_exists(q));
        // Terminate avec callback de fin.
        static TERMINATED: AtomicU32 = AtomicU32::new(0);
        unsafe extern "win64" fn on_term(_c: *mut c_void) {
            TERMINATED.fetch_add(1, Ordering::SeqCst);
        }
        assert_eq!(
            call!(
                o,
                22,
                fn(usize, u8, usize, usize) -> HResult,
                q,
                1,
                0,
                on_term as *const () as usize
            ),
            S_OK
        );
        assert_eq!(TERMINATED.load(Ordering::SeqCst), 1);
        assert_eq!(
            call!(
                o,
                18,
                fn(usize, u32, *mut c_void, usize) -> HResult,
                q,
                0,
                core::ptr::null_mut(),
                cb
            ),
            E_INVALIDARG,
            "file terminée"
        );
        assert_eq!(
            call!(
                o,
                22,
                fn(usize, u8, usize, usize) -> HResult,
                0xBAD,
                0,
                0,
                0
            ),
            E_INVALIDARG
        );
        call!(o, 17, fn(usize) -> (), q);
        call!(o, 17, fn(usize) -> (), comp);
        assert!(!taskq::queue_exists(q));
    }

    static MON: AtomicU32 = AtomicU32::new(0);
    unsafe extern "win64" fn on_monitor(_c: *mut c_void, _q: usize, port: u32) {
        MON.fetch_add(1 + port * 10, Ordering::SeqCst);
    }

    #[test]
    fn threading_monitor_waiter_process_queue_and_time_flag() {
        let _serial = TEST_SERIAL.lock();
        let o = obj(&CLSID_THREADING);
        let mut q = 0usize;
        assert_eq!(
            call!(o, 12, fn(u32, u32, *mut usize) -> HResult, 2, 2, &mut q),
            S_OK
        );
        let mut tok = 0u64;
        let mon = on_monitor as *const () as usize;
        assert_eq!(
            call!(
                o,
                23,
                fn(usize, usize, usize, *mut u64) -> HResult,
                q,
                0,
                0,
                &mut tok
            ),
            E_INVALIDARG
        );
        assert_eq!(
            call!(
                o,
                23,
                fn(usize, usize, usize, *mut u64) -> HResult,
                q,
                0,
                mon,
                &mut tok
            ),
            S_OK
        );
        let m0 = MON.load(Ordering::SeqCst);
        let cb = on_task as *const () as usize;
        assert_eq!(
            call!(
                o,
                18,
                fn(usize, u32, *mut c_void, usize) -> HResult,
                q,
                1,
                3usize as *mut c_void,
                cb
            ),
            S_OK
        );
        assert_eq!(MON.load(Ordering::SeqCst), m0 + 11);
        call!(o, 24, fn(usize, u64) -> (), q, tok);
        assert_eq!(
            call!(
                o,
                18,
                fn(usize, u32, *mut c_void, usize) -> HResult,
                q,
                0,
                3usize as *mut c_void,
                cb
            ),
            S_OK
        );
        assert_eq!(MON.load(Ordering::SeqCst), m0 + 11, "moniteur retiré");

        // Waiter sur un événement.
        let ev = crate::sync::create_event_handle(false, false);
        let c0 = TEST_CB.load(Ordering::SeqCst);
        let mut wtok = 0u64;
        assert_eq!(
            call!(
                o,
                20,
                fn(usize, u32, usize, usize, usize, *mut u64) -> HResult,
                q,
                0,
                ev,
                100,
                0,
                &mut wtok
            ),
            E_INVALIDARG
        );
        assert_eq!(
            call!(
                o,
                20,
                fn(usize, u32, usize, usize, usize, *mut u64) -> HResult,
                q,
                0,
                ev,
                100,
                cb,
                &mut wtok
            ),
            S_OK
        );
        // Draine l'élément déjà soumis plus haut, puis signale l'événement.
        while call!(o, 16, fn(usize, u32, u32) -> u8, q, 0, 0) == 1 {}
        while call!(o, 16, fn(usize, u32, u32) -> u8, q, 1, 0) == 1 {}
        let base = TEST_CB.load(Ordering::SeqCst);
        assert!(base >= c0);
        // SetEvent via le module sync.
        crate::sync::SetEvent(ev);
        assert_eq!(call!(o, 16, fn(usize, u32, u32) -> u8, q, 0, 2000), 1);
        assert_eq!(TEST_CB.load(Ordering::SeqCst), base + 100);
        call!(o, 21, fn(usize, u64) -> (), q, wtok);

        // File process : pose / lit.
        let mut cur = 0usize;
        assert_eq!(call!(o, 25, fn(*mut usize) -> u8, &mut cur), 1);
        assert_ne!(cur, 0);
        assert_eq!(call!(o, 25, fn(*mut usize) -> u8, core::ptr::null_mut()), 0);
        call!(o, 26, fn(usize) -> (), q);
        let mut got = 0usize;
        assert_eq!(call!(o, 25, fn(*mut usize) -> u8, &mut got), 1);
        assert_eq!(got, q);
        call!(o, 26, fn(usize) -> (), 0);
        assert_eq!(call!(o, 25, fn(*mut usize) -> u8, &mut got), 1);
        assert_eq!(got, cur);
        call!(o, 17, fn(usize) -> (), q);

        // Drapeau « sensible au temps » par thread.
        assert_eq!(call!(o, 30, fn() -> u8), 0);
        assert_eq!(call!(o, 27, fn(u8) -> HResult, 1), S_OK);
        assert_eq!(call!(o, 30, fn() -> u8), 1);
        let other = std::thread::spawn(|| {
            let o = obj(&CLSID_THREADING);
            call!(o, 30, fn() -> u8)
        })
        .join()
        .expect("thread");
        assert_eq!(other, 0);
        call!(o, 29, fn() -> ());
        assert_eq!(call!(o, 27, fn(u8) -> HResult, 0), S_OK);
        assert_eq!(call!(o, 30, fn() -> u8), 0);
        // Slots privés.
        assert_eq!(call!(o, 28, fn() -> u64), 0);
        assert_eq!(call!(o, 8, fn(usize) -> HResult, 0), E_NOTIMPL);
    }

    unsafe extern "win64" fn done_cb(_b: *mut XAsyncBlock) {}

    unsafe extern "win64" fn prov(op: u32, d: *const xasync::XAsyncProviderData) -> HResult {
        // SAFETY: données valides.
        let d = unsafe { &*d };
        match op {
            0 => S_OK,
            1 => {
                xasync::complete(d.async_block, S_OK, 0);
                S_OK
            }
            _ => S_OK,
        }
    }

    #[test]
    fn threading_xasync_slots() {
        let o = obj(&CLSID_THREADING);
        let mut q = 0usize;
        assert_eq!(
            call!(o, 12, fn(u32, u32, *mut usize) -> HResult, 3, 3, &mut q),
            S_OK
        );
        let mut b = Box::new(XAsyncBlock {
            queue: q,
            context: core::ptr::null_mut(),
            callback: Some(done_cb),
            internal: [0; 32],
        });
        let bp: *mut XAsyncBlock = &mut *b;
        assert_eq!(
            call!(
                o,
                7,
                fn(*mut XAsyncBlock, usize, usize, *const u8, usize) -> HResult,
                core::ptr::null_mut(),
                0,
                0,
                core::ptr::null(),
                prov as *const () as usize
            ),
            E_INVALIDARG
        );
        assert_eq!(
            call!(
                o,
                7,
                fn(*mut XAsyncBlock, usize, usize, *const u8, usize) -> HResult,
                bp,
                0,
                0,
                core::ptr::null(),
                prov as *const () as usize
            ),
            S_OK
        );
        assert_eq!(
            call!(o, 3, fn(*mut XAsyncBlock, u8) -> HResult, bp, 0),
            E_PENDING_T
        );
        assert_eq!(
            call!(o, 9, fn(*mut XAsyncBlock, u32) -> HResult, bp, 0),
            S_OK
        );
        // File immédiate (mode 3) : DoWork et complétion se sont exécutés en ligne.
        assert_eq!(
            call!(o, 3, fn(*mut XAsyncBlock, u8) -> HResult, bp, 1),
            S_OK
        );
        let mut size = 9usize;
        assert_eq!(
            call!(
                o,
                4,
                fn(*mut XAsyncBlock, *mut usize) -> HResult,
                bp,
                &mut size
            ),
            S_OK
        );
        assert_eq!(size, 0);
        assert_eq!(
            call!(
                o,
                4,
                fn(*mut XAsyncBlock, *mut usize) -> HResult,
                bp,
                core::ptr::null_mut()
            ),
            E_INVALIDARG
        );
        let mut used = 5usize;
        assert_eq!(
            call!(
                o,
                11,
                fn(*mut XAsyncBlock, usize, usize, *mut c_void, *mut usize) -> HResult,
                bp,
                0,
                0,
                core::ptr::null_mut(),
                &mut used
            ),
            S_OK
        );
        assert_eq!(used, 0);
        // Complete/Cancel sur un bloc terminé : sans effet.
        call!(
            o,
            10,
            fn(*mut XAsyncBlock, HResult, usize) -> (),
            bp,
            E_INVALIDARG,
            0
        );
        call!(o, 5, fn(*mut XAsyncBlock) -> (), bp);
        assert_eq!(
            call!(o, 3, fn(*mut XAsyncBlock, u8) -> HResult, bp, 0),
            S_OK
        );
        // XAsyncRun avec une fonction de travail.
        unsafe extern "win64" fn work(_b: *mut XAsyncBlock) -> HResult {
            0x8007_0005_u32 as i32
        }
        let mut b2 = Box::new(XAsyncBlock {
            queue: q,
            context: core::ptr::null_mut(),
            callback: None,
            internal: [0; 32],
        });
        let b2p: *mut XAsyncBlock = &mut *b2;
        assert_eq!(
            call!(
                o,
                6,
                fn(*mut XAsyncBlock, usize) -> HResult,
                b2p,
                work as *const () as usize
            ),
            S_OK
        );
        assert_eq!(
            call!(o, 3, fn(*mut XAsyncBlock, u8) -> HResult, b2p, 1),
            0x8007_0005_u32 as i32
        );
        call!(o, 17, fn(usize) -> (), q);
    }

    const E_PENDING_T: HResult = 0x8000_000A_u32 as i32;

    static REPORTED: Mutex<Vec<(HResult, String, usize)>> = Mutex::new(Vec::new());
    unsafe extern "win64" fn on_error(hr: HResult, msg: *const c_char, ctx: *mut c_void) {
        REPORTED.lock().push((hr, cstr_or_empty(msg), ctx as usize));
    }

    #[test]
    fn error_report_calls_callback() {
        let o = obj(&CLSID_ERROR);
        assert_eq!(
            call!(
                o,
                3,
                fn(HResult, *const c_char) -> HResult,
                E_INVALIDARG,
                c"sans callback".as_ptr()
            ),
            S_OK
        );
        call!(
            o,
            4,
            fn(usize, usize) -> (),
            on_error as *const () as usize,
            0x77
        );
        call!(o, 5, fn(u32, u32) -> (), 1, 2);
        assert_eq!(*ERROR_OPTIONS.lock(), (1, 2));
        assert_eq!(
            call!(
                o,
                3,
                fn(HResult, *const c_char) -> HResult,
                E_NOTIMPL,
                c"boom".as_ptr()
            ),
            S_OK
        );
        assert_eq!(
            call!(
                o,
                3,
                fn(HResult, *const c_char) -> HResult,
                E_FAIL_T,
                core::ptr::null()
            ),
            S_OK
        );
        let r = REPORTED.lock().clone();
        assert_eq!(r[0], (E_NOTIMPL, "boom".to_string(), 0x77));
        assert_eq!(r[1], (E_FAIL_T, String::new(), 0x77));
        call!(o, 4, fn(usize, usize) -> (), 0, 0);
        assert_eq!(
            call!(
                o,
                3,
                fn(HResult, *const c_char) -> HResult,
                E_NOTIMPL,
                c"x".as_ptr()
            ),
            S_OK
        );
        assert_eq!(REPORTED.lock().len(), 2);
    }

    const E_FAIL_T: HResult = 0x8000_4005_u32 as i32;

    unsafe extern "win64" fn on_handle(_h: usize, _t: u32, _r: u32, _c: *mut c_void) {}

    #[test]
    fn system_slots() {
        let o = obj(&CLSID_SYSTEM);
        let mut buf = [0xAAu8; 64];
        let mut used = 0usize;
        for (slot, text) in [(3usize, CONSOLE_ID), (4, SANDBOX_ID), (5, APP_DEVICE_ID)] {
            buf.fill(0xAA);
            assert_eq!(
                call!(
                    o,
                    slot,
                    fn(i32, *mut u8, *mut usize) -> HResult,
                    64,
                    buf.as_mut_ptr(),
                    &mut used
                ),
                S_OK
            );
            assert_eq!(used, text.len() + 1);
            assert_eq!(&buf[..text.len()], text.as_bytes());
            assert_eq!(buf[text.len()], 0);
            assert_eq!(
                call!(
                    o,
                    slot,
                    fn(i32, *mut u8, *mut usize) -> HResult,
                    2,
                    buf.as_mut_ptr(),
                    &mut used
                ),
                E_NOT_SUFFICIENT_BUFFER
            );
            assert_eq!(used, text.len() + 1);
            assert_eq!(
                call!(
                    o,
                    slot,
                    fn(i32, *mut u8, *mut usize) -> HResult,
                    64,
                    core::ptr::null_mut(),
                    &mut used
                ),
                E_INVALIDARG
            );
            assert_eq!(
                call!(
                    o,
                    slot,
                    fn(i32, *mut u8, *mut usize) -> HResult,
                    -1,
                    buf.as_mut_ptr(),
                    core::ptr::null_mut()
                ),
                E_INVALIDARG
            );
            assert_eq!(
                call!(
                    o,
                    slot,
                    fn(i32, *mut u8, *mut usize) -> HResult,
                    64,
                    buf.as_mut_ptr(),
                    core::ptr::null_mut()
                ),
                S_OK
            );
        }
        assert_eq!(call!(o, 6, fn(usize, usize) -> HResult, 0, 0), E_INVALIDARG);
        assert_eq!(
            call!(
                o,
                6,
                fn(usize, usize) -> HResult,
                on_handle as *const () as usize,
                0
            ),
            S_OK
        );
        assert_eq!(call!(o, 7, fn(usize) -> u8, 0), 0);
        assert_eq!(call!(o, 7, fn(usize) -> u8, 0x1000), 1);
        call!(o, 8, fn(u8) -> (), 1);
        notify_handle(0x1000, 0, 0);
    }

    #[test]
    fn analytics_returns_struct_through_hidden_pointer() {
        let o = obj(&CLSID_SYSTEM_ANALYTICS);
        let mut info = core::mem::MaybeUninit::<XSystemAnalyticsInfo>::zeroed();
        let ret = call!(
            o,
            3,
            fn(*mut XSystemAnalyticsInfo) -> *mut XSystemAnalyticsInfo,
            info.as_mut_ptr()
        );
        assert_eq!(ret, info.as_mut_ptr());
        // SAFETY: rempli par le slot.
        let info = unsafe { info.assume_init() };
        assert_eq!(info.os_version.major, 10);
        assert_eq!(&info.family[..5], b"Xbox\0");
        assert_eq!(&info.form[..12], b"XboxSeriesX\0");
        assert!(
            call!(
                o,
                3,
                fn(*mut XSystemAnalyticsInfo) -> *mut XSystemAnalyticsInfo,
                core::ptr::null_mut()
            )
            .is_null()
        );
    }

    #[test]
    fn feature_availability_matches_served_classes() {
        let o = obj(&CLSID_FEATURE);
        for (i, clsid) in FEATURE_CLASSES.iter().enumerate() {
            let expected = u8::from(crate::gdk_classes::is_class_served(clsid));
            assert_eq!(
                call!(o, 3, fn(u32) -> u8, i as u32),
                expected,
                "feature {i}"
            );
        }
        // Toujours servies par ce module : XAsync(2), XAsyncProvider(3), XGame(5), XPLS(12),
        // XSystem(15), XTaskQueue(16), XThread(17), XError(19).
        for f in [2u32, 3, 5, 12, 15, 16, 17, 19] {
            assert_eq!(call!(o, 3, fn(u32) -> u8, f), 1, "feature {f}");
        }
        assert_eq!(call!(o, 3, fn(u32) -> u8, 22), 0);
        assert_eq!(call!(o, 3, fn(u32) -> u8, u32::MAX), 0);
    }

    #[test]
    fn game_slots_and_title_id() {
        let o = obj(&CLSID_GAME);
        let mut id = 0u32;
        assert_eq!(call!(o, 3, fn(*mut u32) -> HResult, &mut id), S_OK);
        assert_ne!(id, 0);
        assert_eq!(
            call!(o, 3, fn(*mut u32) -> HResult, core::ptr::null_mut()),
            E_INVALIDARG
        );
        call!(
            o,
            4,
            fn(*const c_char, *const c_char, usize) -> (),
            c"game.exe".as_ptr(),
            core::ptr::null(),
            0
        );
        assert_eq!(
            call!(o, 5, fn(*const c_char, u32) -> HResult, c"-x".as_ptr(), 0),
            S_OK
        );
        assert_eq!(
            call!(
                o,
                5,
                fn(*const c_char, u32) -> HResult,
                core::ptr::null(),
                1
            ),
            E_INVALIDARG
        );
        assert_eq!(
            parse_title_id("<Game><TitleId>6C0B14D3</TitleId></Game>"),
            Some(0x6C0B_14D3)
        );
        assert_eq!(parse_title_id("<Game><TitleId>zz</TitleId></Game>"), None);
        assert_eq!(parse_title_id("<Game></Game>"), None);
        assert_eq!(parse_title_id("pas du xml <<"), None);
    }

    #[test]
    fn persistent_local_storage_slots() {
        let o = obj(&CLSID_PLS);
        let mut size = 0usize;
        assert_eq!(call!(o, 3, fn(*mut usize) -> HResult, &mut size), S_OK);
        assert_eq!(
            call!(o, 3, fn(*mut usize) -> HResult, core::ptr::null_mut()),
            E_POINTER
        );
        let mut buf = vec![0u8; size];
        let mut used = 0usize;
        assert_eq!(
            call!(
                o,
                4,
                fn(usize, *mut u8, *mut usize) -> HResult,
                size,
                buf.as_mut_ptr(),
                &mut used
            ),
            S_OK
        );
        assert_eq!(used, size);
        assert_eq!(&buf[..2], b"S:");
        assert_eq!(
            call!(
                o,
                4,
                fn(usize, *mut u8, *mut usize) -> HResult,
                1,
                buf.as_mut_ptr(),
                &mut used
            ),
            E_INVALIDARG
        );
        let mut info = XPersistentLocalStorageSpaceInfo {
            available_free_bytes: 0,
            total_free_bytes: 0,
            used_bytes: 1,
            total_bytes: 0,
        };
        assert_eq!(
            call!(
                o,
                5,
                fn(*mut XPersistentLocalStorageSpaceInfo) -> HResult,
                &mut info
            ),
            S_OK
        );
        assert!(info.total_bytes > 0 && info.used_bytes == 0);
        assert_eq!(
            call!(
                o,
                5,
                fn(*mut XPersistentLocalStorageSpaceInfo) -> HResult,
                core::ptr::null_mut()
            ),
            E_POINTER
        );

        // Prompt : succès si le quota contient la demande, sinon disque plein.
        let mut q = 0usize;
        assert_eq!(taskq::create(3, 3, &mut q), S_OK);
        for (req, expected) in [(1024u64, S_OK), (u64::MAX, E_DISK_FULL)] {
            let mut b = Box::new(XAsyncBlock {
                queue: q,
                context: core::ptr::null_mut(),
                callback: None,
                internal: [0; 32],
            });
            let bp: *mut XAsyncBlock = &mut *b;
            assert_eq!(
                call!(o, 6, fn(u64, *mut XAsyncBlock) -> HResult, req, bp),
                S_OK
            );
            assert_eq!(call!(o, 7, fn(*mut XAsyncBlock) -> HResult, bp), expected);
        }
        assert_eq!(
            call!(
                o,
                6,
                fn(u64, *mut XAsyncBlock) -> HResult,
                1,
                core::ptr::null_mut()
            ),
            E_INVALIDARG
        );
        assert_eq!(
            call!(o, 7, fn(*mut XAsyncBlock) -> HResult, core::ptr::null_mut()),
            E_INVALIDARG
        );
        taskq::close(q);

        let mut mount = 0xFFusize;
        assert_eq!(
            call!(
                o,
                8,
                fn(*const c_char, *mut usize) -> HResult,
                c"pkg".as_ptr(),
                &mut mount
            ),
            E_NOTIMPL
        );
        assert_eq!(mount, 0);
    }
}
