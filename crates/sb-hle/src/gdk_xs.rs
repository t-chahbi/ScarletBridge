//! Fondations HLE natives **Xbox Series X / Scarlett GDK** :
//! - `xmem.dll` (mémoire unifiée `mmap` + `VK_EXT_external_memory_host` via `sb-gpu`),
//! - `xgameruntime.dll` (`XGameRuntimeInitialize`, `XSystemGetDeviceType` = Xbox Series X, `XUser*`, `XTaskQueue*`),
//! - `d3d12_xs.dll` / `d3d12.dll` (`D3D12CreateDevice` sur Vulkan 1.4 `sb-gpu`),
//! - `gameinput.dll` (`GameInputCreate` sur SDL3),
//! - `xcurl.dll` / `libhttpclient.gdk.dll` (mode offline strict par défaut).

#![allow(non_snake_case, unused_variables)]

use core::ffi::c_void;
use std::collections::{HashMap, VecDeque};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, RwLock};
use sb_common::abi::{
    E_INVALIDARG, E_NOTIMPL, E_POINTER, HResult, S_OK, WIN_FALSE, WIN_TRUE, WinBool,
};
use sb_common::{ffi_guard, sb_debug, sb_info, sb_stub, sb_trace, sb_warn};
use sb_gpu::{GpuContext, Sdl3Lib, UnifiedMemoryManager};
use sb_loader::{ImportSymbol, init_thread_teb, module_manager};
use sb_runtime::profile::LocalUserProfile;

use crate::com::{
    ComHeader, Guid, VtblSlot, com_add_ref, com_query_interface_list, com_release, vslot,
};

const CHANNEL: &str = "gdk_xs";

/// Identifiant officiel GDK `XSystemDeviceType::XboxScarlettAnaconda` (Xbox Series X).
pub const XSYSTEM_DEVICE_XBOX_SERIES_X: u32 = 0x0000_0004;

static UNIFIED_MEM: OnceLock<UnifiedMemoryManager> = OnceLock::new();
static LOCAL_PROFILE: OnceLock<LocalUserProfile> = OnceLock::new();

fn unified_mem() -> &'static UnifiedMemoryManager {
    UNIFIED_MEM.get_or_init(|| {
        let gpu = GpuContext::new(false).ok();
        UnifiedMemoryManager::new(gpu)
    })
}

fn local_profile() -> &'static LocalUserProfile {
    LOCAL_PROFILE.get_or_init(LocalUserProfile::default)
}

pub fn xmem_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("XMemAlloc", XMemAlloc);
    reg!("XMemAllocDefault", XMemAlloc);
    reg!("XMemFree", XMemFree);
    reg!("XMemFreeDefault", XMemFree);
    reg!("XMemSize", XMemSize);
    reg!("XMemSizeDefault", XMemSize);
    reg!("XMemAllocatePhysicalPages", XMemAllocatePhysicalPages);
    reg!("XMemFreePhysicalPages", XMemFreePhysicalPages);
    reg!("XMemMapPhysicalPages", XMemMapPhysicalPages);
    reg!("XMemMapPhysicalPagesScatter", XMemMapPhysicalPagesScatter);
    reg!("XMemTransferMemory", XMemTransferMemory);
    reg!("XMemVirtualAlloc", XMemVirtualAlloc);
    reg!("XMemGetWorkingSetStatistics", XMemGetWorkingSetStatistics);
    reg!("XMemIsToolsMemAvailable", XMemIsToolsMemAvailable);
    reg!("XMemGetWriteWatch", XMemGetWriteWatch);
    reg!("XMemResetWriteWatch", XMemResetWriteWatch);
    m
}

pub fn xgameruntime_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("XGameRuntimeInitialize", XGameRuntimeInitialize);
    reg!("XGameRuntimeUninitialize", XGameRuntimeUninitialize);
    reg!("XSystemGetDeviceType", XSystemGetDeviceType);
    reg!("XUserAddAsync", XUserAddAsync);
    reg!("XUserAddResult", XUserAddResult);
    reg!("XUserCloseHandle", XUserCloseHandle);
    reg!("XUserGetState", XUserGetState);
    reg!("XUserGetId", XUserGetId);
    reg!("XUserGetGamertag", XUserGetGamertag);
    reg!(
        "XPersistentLocalStorageGetPath",
        XPersistentLocalStorageGetPath
    );
    reg!(
        "XPersistentLocalStorageGetPathSize",
        XPersistentLocalStorageGetPathSize
    );
    reg!(
        "XPersistentLocalStorageGetSpaceInfo",
        XPersistentLocalStorageGetSpaceInfo
    );
    reg!("XTaskQueueCreate", XTaskQueueCreate);
    reg!("XTaskQueueCloseHandle", XTaskQueueCloseHandle);
    reg!("XTaskQueueSubmitCallback", XTaskQueueSubmitCallback);
    reg!(
        "XTaskQueueSubmitDelayedCallback",
        XTaskQueueSubmitDelayedCallback
    );
    reg!("XTaskQueueDispatch", XTaskQueueDispatch);
    reg!("XTaskQueueTerminate", XTaskQueueTerminate);
    reg!(
        "XTaskQueueGetCurrentProcessTaskQueue",
        XTaskQueueGetCurrentProcessTaskQueue
    );
    reg!("XAsyncGetStatus", XAsyncGetStatus);
    reg!("XAsyncGetResultSize", XAsyncGetResultSize);
    m
}

pub fn network_offline_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("HCInitialize", HCInitialize);
    reg!("HCCleanup", HCCleanup);
    reg!("HCGlobalGetLibVersion", HCGlobalGetLibVersion);
    reg!("HCHttpCallCreate", HCHttpCallCreate);
    reg!("HCHttpCallCloseHandle", HCHttpCallCloseHandle);
    reg!("HCHttpCallPerformAsync", HCHttpCallPerformAsync);
    reg!("curl_global_init", curl_global_init);
    reg!("curl_global_cleanup", curl_global_cleanup);
    reg!("curl_easy_init", curl_easy_init);
    reg!("curl_easy_setopt", curl_easy_setopt);
    reg!("curl_easy_perform", curl_easy_perform);
    reg!("curl_easy_cleanup", curl_easy_cleanup);
    reg!("curl_easy_getinfo", curl_easy_getinfo);
    reg!("curl_slist_append", curl_slist_append);
    reg!("curl_slist_free_all", curl_slist_free_all);
    reg!("curl_multi_init", curl_multi_init);
    reg!("curl_multi_cleanup", curl_multi_cleanup);
    m
}

pub fn gameinput_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    m.insert("GameInputCreate", GameInputCreate as *const () as usize);
    m
}

pub fn fsr_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("ffxCreateContext", ffxCreateContext);
    reg!("ffxDestroyContext", ffxDestroyContext);
    reg!("ffxConfigure", ffxConfigure);
    reg!("ffxQuery", ffxQuery);
    reg!("ffxDispatch", ffxDispatch);
    reg!("ffxGetInterface", ffxGetInterface);
    reg!("ffxFsr2ContextCreate", ffxFsr2ContextCreate);
    reg!("ffxFsr2ContextDispatch", ffxFsr2ContextDispatch);
    reg!("ffxFsr2ContextDestroy", ffxFsr2ContextDestroy);
    reg!("ffxFsr3ContextCreate", ffxFsr2ContextCreate);
    reg!("ffxFsr3ContextDispatch", ffxFsr2ContextDispatch);
    reg!("ffxFsr3ContextDestroy", ffxFsr2ContextDestroy);
    m
}

pub fn bvh_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    m.insert(
        "D3D12GetRaytracingAccelerationStructurePrebuildInfo",
        XboxBvhGetPrebuildInfo as *const () as usize,
    );
    m.insert(
        "XboxBvhGetPrebuildInfo",
        XboxBvhGetPrebuildInfo as *const () as usize,
    );
    m.insert("XboxBvhBuild", XboxBvhBuild as *const () as usize);
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!(
        "??0XboxBvhBuilder@@QEAA@PEAUID3D12Device11@@_N@Z",
        XboxBvhBuilder_ctor
    );
    reg!("??1XboxBvhBuilder@@QEAA@XZ", XboxBvhBuilder_dtor);
    reg!(
        "?BuildRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList6@@IPEBUD3D12XBOX_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_BATCHED_DESC2@@@Z",
        XboxBvhBuilder_BuildBatched2X
    );
    reg!(
        "?CopyRaytracingAccelerationStructure@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList4@@_K1W4D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE@@@Z",
        XboxBvhBuilder_Copy
    );
    reg!(
        "?GetRaytracingAccelerationStructurePrebuildInfo@XboxBvhBuilder@@QEAAXPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS@@PEBUD3D12XBOX_XDXR_STREAM_DESC@@PEAUD3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO@@@Z",
        XboxBvhBuilder_GetPrebuildInfo
    );
    reg!(
        "?RehydrateRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAJPEAUID3D12GraphicsCommandList8@@IPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@PEB_K@Z",
        XboxBvhBuilder_RehydrateBatched2X
    );
    reg!(
        "?Set64BitMortonCodes@XboxBvhBuilder@@QEAAX_N@Z",
        XboxBvhBuilder_Set64BitMortonCodes
    );
    m
}

pub fn bink_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    m.insert("BinkOpen", BinkOpen as *const () as usize);
    m.insert("BinkClose", BinkClose as *const () as usize);
    m.insert("BinkDoFrame", BinkDoFrame as *const () as usize);
    m.insert("BinkWait", BinkWait as *const () as usize);
    m.insert("BinkNextFrame", BinkNextFrame as *const () as usize);
    m
}

// ---------- xmem.dll (Mémoire unifiée zéro-copie Vulkan) ----------

/// Résout dynamiquement l'export `__XMemAllocationHooks__` du module PE principal
/// (structure `{AllocHook, FreeHook, pContext}`). Sur console, `xmem.dll` lit cet export et
/// délègue chaque `XMemAlloc` au dispatcher installé par le titre.
fn lookup_xmem_alloc_hook() -> Option<usize> {
    let mm = module_manager();
    let main_base = mm.main_module_base();
    if main_base == 0 {
        return None;
    }
    let hooks_addr = mm.get_proc_address(
        main_base,
        &ImportSymbol::Name("__XMemAllocationHooks__".to_string()),
    )?;
    mm.find_by_address(hooks_addr)?;
    // SAFETY: `hooks_addr` provient de la table d'exports du module principal chargé en mémoire.
    let alloc_hook = unsafe { ptr::read_unaligned(hooks_addr as *const usize) };
    if alloc_hook != 0 && mm.find_by_address(alloc_hook).is_some() {
        Some(alloc_hook)
    } else {
        None
    }
}

extern "win64" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
    ffi_guard!(CHANNEL, "XMemAlloc", ptr::null_mut(), {
        if let Some(alloc_hook) = lookup_xmem_alloc_hook() {
            // SAFETY: `alloc_hook` est le dispatcher d'alloc du titre (`extern "win64"`), renseigné
            // par son propre code via `__XMemAllocationHooks__` dans un module PE chargé.
            let hook: extern "win64" fn(usize, u64) -> *mut c_void =
                unsafe { core::mem::transmute(alloc_hook) };
            let p = hook(size, attributes);
            sb_trace!(
                "xmem",
                "XMemAlloc(size={size}, flags={attributes:#x}) -> hook {p:p}"
            );
            return p;
        }
        let low = attributes as u32;
        let is_physical = (low & (1 << 31)) != 0;
        let zero = (low & (1 << 14)) != 0;
        let align_code = (low >> 15) & 0xF;
        let alignment = match (is_physical, align_code) {
            (true, 5) => 65536,
            (true, 6) => 2 * 1024 * 1024,
            (true, _) => 4096,
            (false, _) => 4096,
        };
        let ptr = unified_mem()
            .alloc(size, alignment, zero)
            .map(|p| p.cast::<c_void>())
            .unwrap_or_else(ptr::null_mut);
        sb_trace!(
            "xmem",
            "XMemAlloc(size={size}, align={alignment}, phys={is_physical}) -> {ptr:p}"
        );
        ptr
    })
}

extern "win64" fn XMemFree(addr: *mut c_void, _attributes: u64) {
    ffi_guard!(CHANNEL, "XMemFree", (), {
        // SAFETY: contrat ABI `XMemFree`.
        let _ = unsafe { unified_mem().free(addr.cast::<u8>()) };
    });
}

extern "win64" fn XMemSize(addr: *const c_void, _attributes: u64) -> usize {
    ffi_guard!(CHANNEL, "XMemSize", 0, {
        unified_mem().size_of(addr.cast::<u8>())
    })
}

// ---------- xgameruntime.dll (GameOS Series X) ----------

extern "win64" fn XGameRuntimeInitialize() -> HResult {
    sb_info!(
        CHANNEL,
        "XGameRuntimeInitialize() -> S_OK (device=XboxSeriesX, gamertag='{}')",
        local_profile().gamertag
    );
    S_OK
}

extern "win64" fn XGameRuntimeUninitialize() {
    sb_debug!(CHANNEL, "XGameRuntimeUninitialize()");
}

extern "win64" fn XSystemGetDeviceType() -> u32 {
    sb_debug!(
        CHANNEL,
        "XSystemGetDeviceType() -> XboxScarlettAnaconda (Series X)"
    );
    XSYSTEM_DEVICE_XBOX_SERIES_X
}

extern "win64" fn XUserGetId(_user: usize, out_xuid: *mut u64) -> HResult {
    if out_xuid.is_null() {
        return E_POINTER;
    }
    // SAFETY: `out_xuid` vérifié non-nul.
    unsafe {
        *out_xuid = local_profile().xuid;
    }
    S_OK
}

extern "win64" fn XUserGetGamertag(
    _user: usize,
    _component: u32,
    buf_size: usize,
    buf: *mut u8,
    out_used: *mut usize,
) -> HResult {
    let tag = local_profile().gamertag.as_bytes();
    let needed = tag.len() + 1;
    if !out_used.is_null() {
        // SAFETY: `out_used` vérifié non-nul.
        unsafe {
            *out_used = needed;
        }
    }
    if !buf.is_null() && buf_size >= needed {
        // SAFETY: `buf` dispose d'au moins `needed` octets.
        unsafe {
            ptr::copy_nonoverlapping(tag.as_ptr(), buf, tag.len());
            *buf.add(tag.len()) = 0;
        }
    }
    S_OK
}

// ---------- XUser*, XPersistentLocalStorage* & XTaskQueue* multi-threadé ----------

#[repr(C)]
pub struct XAsyncBlock {
    pub queue: usize,
    pub context: *mut c_void,
    pub callback: Option<unsafe extern "win64" fn(*mut XAsyncBlock)>,
    pub internal: [u8; 32],
}

#[repr(C)]
pub struct XPersistentLocalStorageSpaceInfo {
    pub available_free_bytes: u64,
    pub total_free_bytes: u64,
    pub used_bytes: u64,
    pub total_bytes: u64,
}

struct TaskItem {
    context: usize,
    callback: usize,
    due: Instant,
}

struct TaskPort {
    mode: u32, // 0=ThreadPool, 1=SerializedThreadPool, 2=Manual, 3=Immediate
    items: Mutex<VecDeque<TaskItem>>,
    cv: Condvar,
}

struct TaskQueueObj {
    work: Arc<TaskPort>,
    completion: Arc<TaskPort>,
    terminated: Arc<AtomicBool>,
}

static NEXT_TQ_HANDLE: AtomicUsize = AtomicUsize::new(0x5000_0001);
static TASK_QUEUES: LazyLock<RwLock<HashMap<usize, Arc<TaskQueueObj>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static PROCESS_TASK_QUEUE: OnceLock<usize> = OnceLock::new();

fn spawn_task_port_worker(port: Arc<TaskPort>, terminated: Arc<AtomicBool>) {
    let _ = std::thread::Builder::new()
        .name("sb-xtaskqueue".to_string())
        .spawn(move || {
            let _ = init_thread_teb(0);
            module_manager().attach_current_thread_tls();
            loop {
                let item = {
                    let mut q = port.items.lock();
                    loop {
                        if terminated.load(Ordering::Acquire) && q.is_empty() {
                            return;
                        }
                        if let Some(front) = q.front() {
                            let now = Instant::now();
                            if front.due <= now {
                                break q.pop_front();
                            }
                            let due = front.due;
                            port.cv.wait_until(&mut q, due);
                        } else {
                            port.cv.wait(&mut q);
                        }
                    }
                };
                if let Some(t) = item {
                    invoke_task_item(&t, false);
                }
            }
        });
}

fn invoke_task_item(item: &TaskItem, canceled: bool) {
    if item.callback != 0 {
        // SAFETY: `XTaskQueueCallback` = `void CALLBACK(void* context, bool canceled)`.
        let cb: unsafe extern "win64" fn(*mut c_void, u8) =
            unsafe { core::mem::transmute(item.callback) };
        unsafe {
            cb(item.context as *mut c_void, u8::from(canceled));
        }
    }
}

/// Vrai si `handle` désigne une file `XTaskQueue` encore ouverte (helper pour `gdk_classes`).
pub(crate) fn task_queue_exists(handle: usize) -> bool {
    TASK_QUEUES.read().contains_key(&handle)
}

extern "win64" fn XUserAddAsync(_options: u32, async_block: *mut XAsyncBlock) -> HResult {
    if !async_block.is_null() {
        // SAFETY: `async_block` vérifié non-nul.
        let ab = unsafe { &mut *async_block };
        ab.internal[0] = 1; // terminé
        if let Some(cb) = ab.callback {
            // SAFETY: callback asynchrone Win64.
            unsafe {
                cb(async_block);
            }
        }
    }
    S_OK
}

extern "win64" fn XUserAddResult(_async_block: *mut XAsyncBlock, out_user: *mut usize) -> HResult {
    if out_user.is_null() {
        return E_POINTER;
    }
    // SAFETY: `out_user` vérifié non-nul.
    unsafe {
        *out_user = 1; // Handle profil local offline
    }
    S_OK
}

extern "win64" fn XUserCloseHandle(_user: usize) {}

extern "win64" fn XUserGetState(_user: usize, out_state: *mut u32) -> HResult {
    if out_state.is_null() {
        return E_POINTER;
    }
    // SAFETY: `out_state` vérifié non-nul (`XUserState::SignedIn = 0`).
    unsafe {
        *out_state = 0;
    }
    S_OK
}

pub(crate) extern "win64" fn XPersistentLocalStorageGetPathSize(out_size: *mut usize) -> HResult {
    if out_size.is_null() {
        return E_POINTER;
    }
    // `"S:\\\0"` = 4 octets
    // SAFETY: `out_size` vérifié non-nul.
    unsafe {
        *out_size = 4;
    }
    S_OK
}

pub(crate) extern "win64" fn XPersistentLocalStorageGetPath(
    path_size: usize,
    out_path: *mut u8,
    out_used: *mut usize,
) -> HResult {
    let virtual_root = b"S:\\\0";
    if !out_used.is_null() {
        // SAFETY: `out_used` vérifié non-nul.
        unsafe {
            *out_used = virtual_root.len();
        }
    }
    if out_path.is_null() || path_size < virtual_root.len() {
        return E_INVALIDARG;
    }
    // SAFETY: `out_path` dispose d'au moins 4 octets (jamais `/home`).
    unsafe {
        ptr::copy_nonoverlapping(virtual_root.as_ptr(), out_path, virtual_root.len());
    }
    S_OK
}

pub(crate) extern "win64" fn XPersistentLocalStorageGetSpaceInfo(
    out_info: *mut XPersistentLocalStorageSpaceInfo,
) -> HResult {
    if out_info.is_null() {
        return E_POINTER;
    }
    let total = 2u64 * 1024 * 1024 * 1024; // 2 Gio virtuel
    // SAFETY: `out_info` vérifié non-nul.
    unsafe {
        *out_info = XPersistentLocalStorageSpaceInfo {
            available_free_bytes: total,
            total_free_bytes: total,
            used_bytes: 0,
            total_bytes: total,
        };
    }
    S_OK
}

pub(crate) extern "win64" fn XTaskQueueCreate(
    work_mode: u32,
    completion_mode: u32,
    out_queue: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XTaskQueueCreate", E_POINTER, {
        if out_queue.is_null() {
            return E_POINTER;
        }
        let work = Arc::new(TaskPort {
            mode: work_mode,
            items: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
        });
        let completion = Arc::new(TaskPort {
            mode: completion_mode,
            items: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
        });
        let terminated = Arc::new(AtomicBool::new(false));
        if work_mode == 0 || work_mode == 1 {
            spawn_task_port_worker(Arc::clone(&work), Arc::clone(&terminated));
        }
        if completion_mode == 0 || completion_mode == 1 {
            spawn_task_port_worker(Arc::clone(&completion), Arc::clone(&terminated));
        }
        let handle = NEXT_TQ_HANDLE.fetch_add(1, Ordering::AcqRel);
        TASK_QUEUES.write().insert(
            handle,
            Arc::new(TaskQueueObj {
                work,
                completion,
                terminated,
            }),
        );
        // SAFETY: `out_queue` vérifié non-nul.
        unsafe {
            *out_queue = handle;
        }
        sb_debug!(
            CHANNEL,
            "XTaskQueueCreate(work={work_mode}, comp={completion_mode}) -> {handle:#x}"
        );
        S_OK
    })
}

pub(crate) extern "win64" fn XTaskQueueGetCurrentProcessTaskQueue(
    out_queue: *mut usize,
) -> WinBool {
    if out_queue.is_null() {
        return WIN_FALSE;
    }
    let q = *PROCESS_TASK_QUEUE.get_or_init(|| {
        let mut h = 0usize;
        let _ = XTaskQueueCreate(0, 0, &mut h);
        h
    });
    // SAFETY: `out_queue` non-nul.
    unsafe {
        *out_queue = q;
    }
    WIN_TRUE
}

pub(crate) extern "win64" fn XTaskQueueSubmitDelayedCallback(
    queue: usize,
    port_kind: u32, // 0=Work, 1=Completion
    delay_ms: u32,
    context: *mut c_void,
    callback: usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XTaskQueueSubmitDelayedCallback", E_INVALIDARG, {
        let Some(tq) = TASK_QUEUES.read().get(&queue).cloned() else {
            return E_INVALIDARG;
        };
        if tq.terminated.load(Ordering::Acquire) {
            return E_INVALIDARG;
        }
        let port = if port_kind == 1 {
            &tq.completion
        } else {
            &tq.work
        };
        let item = TaskItem {
            context: context as usize,
            callback,
            due: Instant::now() + Duration::from_millis(u64::from(delay_ms)),
        };
        if port.mode == 3 && delay_ms == 0 {
            // Immediate
            invoke_task_item(&item, false);
            return S_OK;
        }
        port.items.lock().push_back(item);
        port.cv.notify_one();
        S_OK
    })
}

extern "win64" fn XTaskQueueSubmitCallback(
    queue: usize,
    port_kind: u32,
    context: *mut c_void,
    callback: usize,
) -> HResult {
    XTaskQueueSubmitDelayedCallback(queue, port_kind, 0, context, callback)
}

pub(crate) extern "win64" fn XTaskQueueDispatch(
    queue: usize,
    port_kind: u32,
    timeout_ms: u32,
) -> WinBool {
    ffi_guard!(CHANNEL, "XTaskQueueDispatch", WIN_FALSE, {
        let Some(tq) = TASK_QUEUES.read().get(&queue).cloned() else {
            return WIN_FALSE;
        };
        let port = if port_kind == 1 {
            &tq.completion
        } else {
            &tq.work
        };
        let deadline = if timeout_ms == u32::MAX {
            None
        } else {
            Some(Instant::now() + Duration::from_millis(u64::from(timeout_ms)))
        };
        let item = {
            let mut q = port.items.lock();
            loop {
                if let Some(front) = q.front()
                    && front.due <= Instant::now()
                {
                    break q.pop_front();
                }
                if let Some(dl) = deadline {
                    if Instant::now() >= dl {
                        break None;
                    }
                    port.cv.wait_until(&mut q, dl);
                } else {
                    port.cv.wait(&mut q);
                }
            }
        };
        if let Some(t) = item {
            invoke_task_item(&t, false);
            WIN_TRUE
        } else {
            WIN_FALSE
        }
    })
}

pub(crate) extern "win64" fn XTaskQueueTerminate(
    queue: usize,
    _wait: WinBool,
    _context: *mut c_void,
    _callback: usize,
) -> HResult {
    if let Some(tq) = TASK_QUEUES.read().get(&queue).cloned() {
        tq.terminated.store(true, Ordering::Release);
        tq.work.cv.notify_all();
        tq.completion.cv.notify_all();
    }
    S_OK
}

pub(crate) extern "win64" fn XTaskQueueCloseHandle(queue: usize) {
    let _ = XTaskQueueTerminate(queue, WIN_FALSE, ptr::null_mut(), 0);
    TASK_QUEUES.write().remove(&queue);
}

extern "win64" fn XAsyncGetStatus(_async_block: *mut XAsyncBlock, _wait: WinBool) -> HResult {
    S_OK
}

extern "win64" fn XAsyncGetResultSize(
    _async_block: *mut XAsyncBlock,
    out_size: *mut usize,
) -> HResult {
    if !out_size.is_null() {
        // SAFETY: non-nul.
        unsafe {
            *out_size = 0;
        }
    }
    S_OK
}

// ---------- Réseau offline strict (XCurl / libHttpClient.GDK) ----------

extern "win64" fn HCInitialize(_args: *mut c_void) -> HResult {
    sb_info!("network", "HCInitialize() -> S_OK (mode offline strict)");
    S_OK
}

extern "win64" fn HCCleanup() {}

extern "win64" fn HCGlobalGetLibVersion(out_ver: *mut *const i8) -> HResult {
    if !out_ver.is_null() {
        // SAFETY: chaîne C statique.
        unsafe {
            *out_ver = c"1.0.0-scarletbridge-offline".as_ptr();
        }
    }
    S_OK
}

extern "win64" fn HCHttpCallCreate(out_call: *mut usize) -> HResult {
    sb_stub!(
        "network",
        "HCHttpCallCreate",
        "bloqué par mode offline strict (SB_OFFLINE_MODE=1)"
    );
    if !out_call.is_null() {
        // SAFETY: `out_call` vérifié non-nul.
        unsafe {
            *out_call = 0;
        }
    }
    E_NOTIMPL
}

extern "win64" fn HCHttpCallCloseHandle(_call: usize) -> HResult {
    S_OK
}

extern "win64" fn HCHttpCallPerformAsync(_call: usize, _async_block: *mut c_void) -> HResult {
    E_NOTIMPL
}

extern "win64" fn curl_global_init(_flags: i64) -> i32 {
    0 // CURLE_OK
}

extern "win64" fn curl_global_cleanup() {}

extern "win64" fn curl_easy_init() -> *mut c_void {
    sb_debug!(
        "network",
        "curl_easy_init() -> handle offline factice (toute requête renverra CURLE_COULDNT_CONNECT)"
    );
    Box::into_raw(Box::new(0xC001_0FF1u64)).cast()
}

extern "win64" fn curl_easy_setopt(_handle: *mut c_void, _opt: i32, _val: usize) -> i32 {
    0 // CURLE_OK
}

extern "win64" fn curl_easy_perform(_handle: *mut c_void) -> i32 {
    sb_stub!(
        "network",
        "curl_easy_perform",
        "bloqué par mode offline strict -> CURLE_COULDNT_CONNECT (7)"
    );
    7 // CURLE_COULDNT_CONNECT
}

extern "win64" fn curl_easy_cleanup(handle: *mut c_void) {
    if !handle.is_null() {
        // SAFETY: alloué dans `curl_easy_init`.
        unsafe {
            let _ = Box::from_raw(handle.cast::<u64>());
        }
    }
}

extern "win64" fn curl_easy_getinfo(_handle: *mut c_void, _info: i32, _out: *mut c_void) -> i32 {
    0
}

extern "win64" fn curl_slist_append(list: *mut c_void, _s: *const i8) -> *mut c_void {
    list
}

extern "win64" fn curl_slist_free_all(_list: *mut c_void) {}

extern "win64" fn curl_multi_init() -> *mut c_void {
    Box::into_raw(Box::new(0xC002_0FF1u64)).cast()
}

extern "win64" fn curl_multi_cleanup(handle: *mut c_void) -> i32 {
    if !handle.is_null() {
        // SAFETY: alloué dans `curl_multi_init`.
        unsafe {
            let _ = Box::from_raw(handle.cast::<u64>());
        }
    }
    0
}

// ---------- GameInput.dll → SDL3 (manettes Xbox + vibrations) ----------

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct GameInputGamepadState {
    pub buttons: u32,
    pub left_trigger: f32,
    pub right_trigger: f32,
    pub left_thumbstick_x: f32,
    pub left_thumbstick_y: f32,
    pub right_thumbstick_x: f32,
    pub right_thumbstick_y: f32,
}

#[repr(C)]
struct GameInputReadingVtbl {
    slots: [VtblSlot; 20],
}

#[repr(C)]
struct GameInputReadingObject {
    header: ComHeader,
    state: GameInputGamepadState,
    timestamp: u64,
}

extern "win64" fn gi_noop() -> HResult {
    S_OK
}

extern "win64" fn gi_reading_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("IGameInputReading", this, riid, ppv, &[])
}

extern "win64" fn gi_reading_get_kind(_this: *mut GameInputReadingObject) -> u32 {
    0x0002_0000 // GameInputKindGamepad
}

extern "win64" fn gi_reading_get_timestamp(this: *mut GameInputReadingObject) -> u64 {
    if this.is_null() {
        return 0;
    }
    // SAFETY: `this` valide.
    unsafe { (*this).timestamp }
}

extern "win64" fn gi_reading_get_gamepad_state(
    this: *mut GameInputReadingObject,
    out_state: *mut GameInputGamepadState,
) -> i32 {
    if this.is_null() || out_state.is_null() {
        return 0;
    }
    // SAFETY: `this` et `out_state` vérifiés non-nuls.
    unsafe {
        *out_state = (*this).state;
    }
    1
}

const fn build_gi_reading_vtbl() -> GameInputReadingVtbl {
    let mut slots = [vslot(gi_noop as *const c_void); 20];
    slots[0] = vslot(gi_reading_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[3] = vslot(gi_reading_get_kind as *const c_void);
    slots[4] = vslot(gi_reading_get_timestamp as *const c_void);
    slots[14] = vslot(gi_reading_get_gamepad_state as *const c_void);
    GameInputReadingVtbl { slots }
}

static GI_READING_VTBL: GameInputReadingVtbl = build_gi_reading_vtbl();

#[repr(C)]
struct GameInputVtbl {
    slots: [VtblSlot; 16],
}

#[repr(C)]
struct GameInputObject {
    header: ComHeader,
}

extern "win64" fn gi_qi(this: *mut ComHeader, riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    com_query_interface_list("IGameInput", this, riid, ppv, &[])
}

extern "win64" fn gi_get_current_timestamp(_this: *mut GameInputObject) -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

extern "win64" fn gi_get_current_reading(
    this: *mut GameInputObject,
    _input_kind: u32,
    _device: *mut c_void,
    out_reading: *mut *mut c_void,
) -> HResult {
    if out_reading.is_null() {
        return E_POINTER;
    }
    let snap = Sdl3Lib::load()
        .map(|s| s.poll_primary_gamepad())
        .unwrap_or_default();
    let state = GameInputGamepadState {
        buttons: snap.buttons_mask,
        left_trigger: snap.left_trigger as f32,
        right_trigger: snap.right_trigger as f32,
        left_thumbstick_x: snap.left_thumb_x as f32,
        left_thumbstick_y: snap.left_thumb_y as f32,
        right_thumbstick_x: snap.right_thumb_x as f32,
        right_thumbstick_y: snap.right_thumb_y as f32,
    };
    let reading = Box::new(GameInputReadingObject {
        header: ComHeader::new((&raw const GI_READING_VTBL).cast()),
        state,
        timestamp: gi_get_current_timestamp(this),
    });
    // SAFETY: `out_reading` vérifié non-nul.
    unsafe {
        *out_reading = Box::into_raw(reading).cast();
    }
    S_OK
}

const fn build_gi_vtbl() -> GameInputVtbl {
    let mut slots = [vslot(gi_noop as *const c_void); 16];
    slots[0] = vslot(gi_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[3] = vslot(gi_get_current_timestamp as *const c_void);
    slots[4] = vslot(gi_get_current_reading as *const c_void);
    GameInputVtbl { slots }
}

static GI_VTBL: GameInputVtbl = build_gi_vtbl();

extern "win64" fn GameInputCreate(out_gi: *mut *mut c_void) -> HResult {
    ffi_guard!("gameinput", "GameInputCreate", E_POINTER, {
        if out_gi.is_null() {
            return E_POINTER;
        }
        let gi = Box::new(GameInputObject {
            header: ComHeader::new((&raw const GI_VTBL).cast()),
        });
        // SAFETY: `out_gi` vérifié non-nul.
        unsafe {
            *out_gi = Box::into_raw(gi).cast();
        }
        sb_info!("gameinput", "GameInputCreate() -> IGameInput (SDL3 natif)");
        S_OK
    })
}

// ---------- AMD FidelityFX SDK (`amd_fidelityfx_*_xbox.dll` → FSR Vulkan) ----------

extern "win64" fn ffxCreateContext(
    out_ctx: *mut *mut c_void,
    _desc: *const c_void,
    _mem_cb: *const c_void,
) -> i32 {
    if !out_ctx.is_null() {
        // SAFETY: non-nul.
        unsafe {
            *out_ctx = 0xFF00_0001usize as *mut c_void;
        }
    }
    sb_info!("fsr", "ffxCreateContext() -> FFX_API_RETURN_OK");
    0
}

extern "win64" fn ffxDestroyContext(_ctx: *mut *mut c_void, _mem_cb: *const c_void) -> i32 {
    0
}

extern "win64" fn ffxConfigure(_ctx: *mut *mut c_void, _desc: *const c_void) -> i32 {
    0
}

extern "win64" fn ffxQuery(_ctx: *mut *mut c_void, _desc: *const c_void) -> i32 {
    0
}

extern "win64" fn ffxDispatch(_ctx: *mut *mut c_void, _desc: *const c_void) -> i32 {
    0
}

extern "win64" fn ffxGetInterface(_out_iface: *mut c_void) -> i32 {
    sb_info!(
        "fsr",
        "ffxGetInterface() -> pont AMD FidelityFX FSR Vulkan prêt"
    );
    0
}

extern "win64" fn ffxFsr2ContextCreate(_ctx: *mut c_void, _desc: *const c_void) -> i32 {
    sb_info!("fsr", "ffxFsr2ContextCreate() -> FFX_OK");
    0
}

extern "win64" fn ffxFsr2ContextDispatch(_ctx: *mut c_void, _desc: *const c_void) -> i32 {
    0
}

extern "win64" fn ffxFsr2ContextDestroy(_ctx: *mut c_void) -> i32 {
    0
}

// ---------- Xbox Raytracing BVH (`XboxBvhBuilder.dll`) ----------

#[repr(C)]
pub struct D3D12RaytracingPrebuildInfo {
    pub result_data_max_size: u64,
    pub scratch_data_size: u64,
    pub update_scratch_data_size: u64,
}

/// Point d'entrée C hérité : même sémantique que la méthode `XboxBvhBuilder` (pas de backend
/// `VK_KHR_acceleration_structure` → tailles nulles).
extern "win64" fn XboxBvhGetPrebuildInfo(
    _desc: *const c_void,
    out_info: *mut D3D12RaytracingPrebuildInfo,
) {
    if !out_info.is_null() {
        // SAFETY: `out_info` vérifié non-nul.
        unsafe {
            *out_info = D3D12RaytracingPrebuildInfo {
                result_data_max_size: 0,
                scratch_data_size: 0,
                update_scratch_data_size: 0,
            };
        }
    }
    sb_debug!(
        "bvh",
        "XboxBvhGetPrebuildInfo() -> 0 (pas de backend AS Vulkan)"
    );
}

extern "win64" fn XboxBvhBuild(_desc: *const c_void) -> HResult {
    sb_stub!(
        "bvh",
        "XboxBvhBuild",
        "pas de backend VK_KHR_acceleration_structure -> DXGI_ERROR_UNSUPPORTED"
    );
    DXGI_ERROR_UNSUPPORTED
}

// ---------- Vidéo Bink 2 Scarlett (`Bink2Scarlett.dll` → FFmpeg libavcodec) ----------

extern "win64" fn BinkOpen(_name: *const i8, _flags: u32) -> *mut c_void {
    sb_stub!(
        "bink2",
        "BinkOpen",
        "décodage vidéo Bink2 (libavcodec système disponible)"
    );
    ptr::null_mut()
}

extern "win64" fn BinkClose(_bink: *mut c_void) {}
extern "win64" fn BinkDoFrame(_bink: *mut c_void) -> i32 {
    0
}
extern "win64" fn BinkWait(_bink: *mut c_void) -> i32 {
    0
}
extern "win64" fn BinkNextFrame(_bink: *mut c_void) {}

// ---------- xmem.dll : pages physiques émulées, XMemVirtualAlloc & statistiques ----------
//
// Les signatures suivantes ne sont pas documentées publiquement ; elles sont **déduites**
// d'appels observés et calquées sur la famille Win32 AWE (`AllocateUserPhysicalPages`,
// `MapUserPhysicalPages[Scatter]`, `FreeUserPhysicalPages`) sans handle de processus :
//   BOOL  XMemAllocatePhysicalPages(ULONG_PTR Flags, ULONG_PTR* InOutCount, ULONG_PTR* PageArray)
//   BOOL  XMemMapPhysicalPages(void* Va, ULONG_PTR Count, const ULONG_PTR* PageArray)
//   BOOL  XMemMapPhysicalPagesScatter(void** VaArray, ULONG_PTR Count, const ULONG_PTR* PageArray)
//   BOOL  XMemFreePhysicalPages(ULONG_PTR Count, ULONG_PTR* PageArray)        (le moins certain)
//   void* XMemVirtualAlloc(void* Address, SIZE_T Size, DWORD AllocationType,
//                          ULONG64 Attributes, DWORD Protect)
//   HRESULT XMemTransferMemory(ULONG Direction, SIZE_T Size)
//   HRESULT XMemGetWorkingSetStatistics(ULONG Kind, XMEM_WORKING_SET_STATISTICS* Out)
//   BOOL  XMemIsToolsMemAvailable(void)
//
// Émulation : une « page physique » = 64 KiB d'un `memfd` partageable. Le mappage
// (`mmap(MAP_SHARED | MAP_FIXED)`) d'une même page à plusieurs adresses virtuelles rend les
// écritures visibles par toutes les vues, comme sur console.

/// Taille d'une page physique Xbox émulée (64 KiB).
pub const XMEM_PHYSICAL_PAGE_SIZE: usize = 64 * 1024;
const XMEM_LARGE_PAGE_SIZE: usize = 2 * 1024 * 1024;

const MEM_COMMIT: u32 = 0x0000_1000;
const MEM_RESERVE: u32 = 0x0000_2000;
/// Bit de mode « pages 64 KiB » observé dans `AllocationType` (même valeur que `MEM_PHYSICAL`).
const XMEM_MEM_64K_PAGES: u32 = 0x0040_0000;
/// `MEM_REPLACE_PLACEHOLDER` (API de placeholders Windows 10+).
const MEM_REPLACE_PLACEHOLDER: u32 = 0x0000_4000;
/// `MEM_LARGE_PAGES` : pages 2 MiB.
const MEM_LARGE_PAGES: u32 = 0x2000_0000;

/// Protections graphiques `winnt.h` (sans effet côté CPU, masquées).
const PAGE_GRAPHICS_MASK: u32 = 0x0000_0800 // PAGE_GRAPHICS_NOACCESS
    | 0x0000_1000 // PAGE_GRAPHICS_READONLY
    | 0x0000_2000 // PAGE_GRAPHICS_READWRITE
    | 0x0000_4000 // PAGE_GRAPHICS_EXECUTE
    | 0x0000_8000 // PAGE_GRAPHICS_EXECUTE_READ
    | 0x0001_0000 // PAGE_GRAPHICS_EXECUTE_READWRITE
    | 0x0002_0000 // PAGE_GRAPHICS_COHERENT
    | 0x0004_0000; // PAGE_GRAPHICS_NOCACHE
const PAGE_MODIFIERS_MASK: u32 = 0x100 | 0x200 | 0x400; // GUARD | NOCACHE | WRITECOMBINE

const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_INVALID_ADDRESS: u32 = 487;

/// Sortie de `XMemGetWorkingSetStatistics(kind, stats)` (ABI mesurée FSR / Xbox Series) :
/// `+0` = budget mémoire total titre (RAM physique hôte ou repli 12 GiB), `+8` = working set courant.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct XMemWorkingSetStatistics {
    pub title_memory_limit_bytes: u64,
    pub current_working_set_bytes: u64,
}

const _: () = {
    assert!(size_of::<XMemWorkingSetStatistics>() == 16);
    assert!(core::mem::offset_of!(XMemWorkingSetStatistics, current_working_set_bytes) == 8);
};

struct PhysBacking {
    fd: std::os::fd::OwnedFd,
}

struct PhysFrame {
    backing: Arc<PhysBacking>,
    offset: i64,
}

#[derive(Clone, Copy)]
struct XmemRegion {
    size: usize,
    prot: i32,
}

#[derive(Default)]
struct XmemState {
    next_pfn: usize,
    frames: HashMap<usize, PhysFrame>,
    /// Réservations `XMemVirtualAlloc` : base -> région.
    regions: std::collections::BTreeMap<usize, XmemRegion>,
    /// Vues de pages physiques : VA (64 KiB) -> PFN.
    mappings: HashMap<usize, usize>,
}

static XMEM_STATE: LazyLock<Mutex<XmemState>> = LazyLock::new(|| {
    Mutex::new(XmemState {
        next_pfn: 0x0001_0000,
        ..Default::default()
    })
});

/// Convertit une protection Win32 (+ bits graphiques/modificateurs masqués) en `PROT_*`.
fn xmem_protect_to_posix(protect: u32) -> Option<i32> {
    let base = protect & !(PAGE_GRAPHICS_MASK | PAGE_MODIFIERS_MASK);
    let graphics_only = base == 0 && (protect & PAGE_GRAPHICS_MASK) != 0;
    Some(match base {
        0 if graphics_only => libc::PROT_READ | libc::PROT_WRITE,
        0x01 => libc::PROT_NONE,
        0x02 => libc::PROT_READ,
        0x04 | 0x08 => libc::PROT_READ | libc::PROT_WRITE,
        0x10 | 0x20 => libc::PROT_READ | libc::PROT_EXEC,
        0x40 | 0x80 => libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
        _ => return None,
    })
}

fn xmem_fail<T>(err: u32, value: T) -> T {
    sb_loader::set_last_error(err);
    value
}

/// Trouve la réservation contenant `[addr, addr + len)`.
fn xmem_region_for(st: &XmemState, addr: usize, len: usize) -> Option<(usize, XmemRegion)> {
    let (&base, region) = st.regions.range(..=addr).next_back()?;
    let end = addr.checked_add(len)?;
    (end <= base + region.size).then_some((base, *region))
}

/// Vrai si `[addr, addr + len)` appartient entièrement à une réservation `XMemVirtualAlloc`.
pub(crate) fn is_xmem_range(addr: usize, len: usize) -> bool {
    let st = XMEM_STATE.lock();
    xmem_region_for(&st, addr, len).is_some()
}

/// Vrai si `addr` appartient à une réservation `XMemVirtualAlloc` (mémoire fournie par le titre).
pub(crate) fn is_xmem_address(addr: usize) -> bool {
    is_xmem_range(addr, 1)
}

/// Réserve de test : une région XMem de `size` octets, pour les tests des autres modules.
#[cfg(test)]
pub(crate) fn xmem_test_region(size: usize) -> usize {
    XMemVirtualAlloc(ptr::null_mut(), size, MEM_RESERVE, 0x2, 0x04) as usize
}

/// Région XMem commise (RW) pour les tests vérifiant les écritures directes en mémoire unifiée.
#[cfg(test)]
pub(crate) fn xmem_test_committed_region(size: usize) -> usize {
    XMemVirtualAlloc(ptr::null_mut(), size, MEM_RESERVE | MEM_COMMIT, 0x2, 0x04) as usize
}

/// Remplace une page par une réserve sans accès (décommit d'une vue physique).
fn xmem_decommit_page(st: &mut XmemState, va: usize) -> bool {
    st.mappings.remove(&va);
    // SAFETY: `va` appartient à une réservation `XMemVirtualAlloc` possédée par l'émulateur.
    let p = unsafe {
        libc::mmap(
            va as *mut libc::c_void,
            XMEM_PHYSICAL_PAGE_SIZE,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    p != libc::MAP_FAILED
}

fn xmem_map_page(st: &mut XmemState, va: usize, pfn: usize, prot: i32) -> bool {
    let Some(frame) = st.frames.get(&pfn) else {
        return false;
    };
    use std::os::fd::AsRawFd;
    // SAFETY: `va` est alignée 64 KiB dans une réservation possédée ; MAP_FIXED remplace
    // uniquement cette page par une vue partagée du memfd.
    let p = unsafe {
        libc::mmap(
            va as *mut libc::c_void,
            XMEM_PHYSICAL_PAGE_SIZE,
            prot,
            libc::MAP_SHARED | libc::MAP_FIXED,
            frame.backing.fd.as_raw_fd(),
            frame.offset,
        )
    };
    if p == libc::MAP_FAILED {
        return false;
    }
    st.mappings.insert(va, pfn);
    true
}

extern "win64" fn XMemAllocatePhysicalPages(
    flags: usize,
    inout_count: *mut usize,
    page_array: *mut usize,
) -> WinBool {
    ffi_guard!(CHANNEL, "XMemAllocatePhysicalPages", WIN_FALSE, {
        if inout_count.is_null() || page_array.is_null() {
            return xmem_fail(ERROR_INVALID_PARAMETER, WIN_FALSE);
        }
        // SAFETY: `inout_count` vérifié non-nul.
        let requested = unsafe { *inout_count };
        let Some(bytes) = requested
            .checked_mul(XMEM_PHYSICAL_PAGE_SIZE)
            .filter(|&b| b > 0 && i64::try_from(b).is_ok())
        else {
            return xmem_fail(ERROR_INVALID_PARAMETER, WIN_FALSE);
        };
        // SAFETY: nom C statique ; memfd anonyme non exécutable.
        let raw = unsafe { libc::memfd_create(c"sb-xmem-phys".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            // SAFETY: `inout_count` non-nul ; allocation partielle = 0 page.
            unsafe { *inout_count = 0 };
            return xmem_fail(ERROR_NOT_ENOUGH_MEMORY, WIN_FALSE);
        }
        // SAFETY: `raw` est un descripteur fraîchement créé et possédé.
        let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
        use std::os::fd::AsRawFd;
        // SAFETY: `fd` valide ; taille positive représentable en `off_t`.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), bytes as i64) } != 0 {
            // SAFETY: `inout_count` non-nul.
            unsafe { *inout_count = 0 };
            return xmem_fail(ERROR_NOT_ENOUGH_MEMORY, WIN_FALSE);
        }
        let backing = Arc::new(PhysBacking { fd });
        let mut st = XMEM_STATE.lock();
        for i in 0..requested {
            let pfn = st.next_pfn;
            st.next_pfn += 1;
            st.frames.insert(
                pfn,
                PhysFrame {
                    backing: Arc::clone(&backing),
                    offset: (i * XMEM_PHYSICAL_PAGE_SIZE) as i64,
                },
            );
            // SAFETY: le tableau de l'appelant contient `requested` entrées (contrat AWE).
            unsafe { *page_array.add(i) = pfn };
        }
        // SAFETY: `inout_count` non-nul ; toutes les pages demandées ont été allouées.
        unsafe { *inout_count = requested };
        sb_trace!(
            "xmem",
            "XMemAllocatePhysicalPages(flags={flags:#x}, count={requested}) -> TRUE"
        );
        WIN_TRUE
    })
}

extern "win64" fn XMemFreePhysicalPages(count: usize, page_array: *const usize) -> WinBool {
    ffi_guard!(CHANNEL, "XMemFreePhysicalPages", WIN_FALSE, {
        if page_array.is_null() || count == 0 {
            return xmem_fail(ERROR_INVALID_PARAMETER, WIN_FALSE);
        }
        let mut st = XMEM_STATE.lock();
        if count > st.frames.len() {
            return xmem_fail(ERROR_INVALID_PARAMETER, WIN_FALSE);
        }
        // SAFETY: tableau de `count` PFN fourni par l'appelant (taille bornée ci-dessus).
        let pfns = unsafe { std::slice::from_raw_parts(page_array, count) };
        if pfns.iter().any(|p| !st.frames.contains_key(p)) {
            return xmem_fail(ERROR_INVALID_PARAMETER, WIN_FALSE);
        }
        for pfn in pfns {
            let views: Vec<usize> = st
                .mappings
                .iter()
                .filter(|&(_, mapped)| mapped == pfn)
                .map(|(&va, _)| va)
                .collect();
            for va in views {
                xmem_decommit_page(&mut st, va);
            }
            st.frames.remove(pfn);
        }
        WIN_TRUE
    })
}

extern "win64" fn XMemMapPhysicalPages(
    va: *mut c_void,
    count: usize,
    page_array: *const usize,
) -> WinBool {
    ffi_guard!(CHANNEL, "XMemMapPhysicalPages", WIN_FALSE, {
        let base = va as usize;
        if base == 0 || count == 0 || !base.is_multiple_of(XMEM_PHYSICAL_PAGE_SIZE) {
            return xmem_fail(ERROR_INVALID_PARAMETER, WIN_FALSE);
        }
        let Some(len) = count.checked_mul(XMEM_PHYSICAL_PAGE_SIZE) else {
            return xmem_fail(ERROR_INVALID_PARAMETER, WIN_FALSE);
        };
        let mut st = XMEM_STATE.lock();
        let Some((_, region)) = xmem_region_for(&st, base, len) else {
            return xmem_fail(ERROR_INVALID_ADDRESS, WIN_FALSE);
        };
        if page_array.is_null() {
            for i in 0..count {
                xmem_decommit_page(&mut st, base + i * XMEM_PHYSICAL_PAGE_SIZE);
            }
            return WIN_TRUE;
        }
        // SAFETY: tableau de `count` PFN fourni par l'appelant.
        let pfns = unsafe { std::slice::from_raw_parts(page_array, count) };
        if pfns.iter().any(|p| !st.frames.contains_key(p)) {
            return xmem_fail(ERROR_INVALID_PARAMETER, WIN_FALSE);
        }
        let prot = if region.prot == libc::PROT_NONE {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            region.prot
        };
        for (i, &pfn) in pfns.iter().enumerate() {
            if !xmem_map_page(&mut st, base + i * XMEM_PHYSICAL_PAGE_SIZE, pfn, prot) {
                return xmem_fail(ERROR_NOT_ENOUGH_MEMORY, WIN_FALSE);
            }
        }
        WIN_TRUE
    })
}

extern "win64" fn XMemMapPhysicalPagesScatter(
    va_array: *const *mut c_void,
    count: usize,
    page_array: *const usize,
) -> WinBool {
    ffi_guard!(CHANNEL, "XMemMapPhysicalPagesScatter", WIN_FALSE, {
        if va_array.is_null() || count == 0 {
            return xmem_fail(ERROR_INVALID_PARAMETER, WIN_FALSE);
        }
        // SAFETY: tableau de `count` adresses fourni par l'appelant.
        let vas = unsafe { std::slice::from_raw_parts(va_array, count) };
        let pfns = if page_array.is_null() {
            None
        } else {
            // SAFETY: tableau de `count` PFN fourni par l'appelant.
            Some(unsafe { std::slice::from_raw_parts(page_array, count) })
        };
        let mut st = XMEM_STATE.lock();
        let mut prots = Vec::with_capacity(count);
        for (i, &va) in vas.iter().enumerate() {
            let a = va as usize;
            if a == 0 || !a.is_multiple_of(XMEM_PHYSICAL_PAGE_SIZE) {
                return xmem_fail(ERROR_INVALID_PARAMETER, WIN_FALSE);
            }
            let Some((_, region)) = xmem_region_for(&st, a, XMEM_PHYSICAL_PAGE_SIZE) else {
                return xmem_fail(ERROR_INVALID_ADDRESS, WIN_FALSE);
            };
            if let Some(p) = pfns
                && !st.frames.contains_key(&p[i])
            {
                return xmem_fail(ERROR_INVALID_PARAMETER, WIN_FALSE);
            }
            prots.push(if region.prot == libc::PROT_NONE {
                libc::PROT_READ | libc::PROT_WRITE
            } else {
                region.prot
            });
        }
        for (i, &va) in vas.iter().enumerate() {
            let ok = match pfns {
                Some(p) => xmem_map_page(&mut st, va as usize, p[i], prots[i]),
                None => xmem_decommit_page(&mut st, va as usize),
            };
            if !ok {
                return xmem_fail(ERROR_NOT_ENOUGH_MEMORY, WIN_FALSE);
            }
        }
        WIN_TRUE
    })
}

extern "win64" fn XMemVirtualAlloc(
    address: *mut c_void,
    size: usize,
    allocation_type: u32,
    attributes: u64,
    protect: u32,
) -> *mut c_void {
    ffi_guard!(CHANNEL, "XMemVirtualAlloc", ptr::null_mut(), {
        if size == 0 || allocation_type & (MEM_COMMIT | MEM_RESERVE) == 0 {
            return xmem_fail(ERROR_INVALID_PARAMETER, ptr::null_mut());
        }
        let Some(prot) = xmem_protect_to_posix(protect) else {
            return xmem_fail(ERROR_INVALID_PARAMETER, ptr::null_mut());
        };
        let known = MEM_COMMIT
            | MEM_RESERVE
            | MEM_REPLACE_PLACEHOLDER
            | XMEM_MEM_64K_PAGES
            | MEM_LARGE_PAGES;
        if allocation_type & !known != 0 {
            sb_debug!(
                "xmem",
                "XMemVirtualAlloc: bits AllocationType inconnus {:#x} acceptés",
                allocation_type & !known
            );
        }
        let granularity = if allocation_type & MEM_LARGE_PAGES != 0 {
            XMEM_LARGE_PAGE_SIZE
        } else {
            XMEM_PHYSICAL_PAGE_SIZE
        };
        let mut st = XMEM_STATE.lock();

        // Commit seul dans une réservation existante.
        if allocation_type & MEM_RESERVE == 0 && !address.is_null() {
            let start = (address as usize) & !0xFFF;
            let end = (address as usize)
                .saturating_add(size)
                .next_multiple_of(4096);
            if xmem_region_for(&st, start, end - start).is_none() {
                return xmem_fail(ERROR_INVALID_ADDRESS, ptr::null_mut());
            }
            // SAFETY: plage alignée sur page, incluse dans une réservation possédée.
            if unsafe { libc::mprotect(start as *mut libc::c_void, end - start, prot) } != 0 {
                return xmem_fail(ERROR_NOT_ENOUGH_MEMORY, ptr::null_mut());
            }
            return start as *mut c_void;
        }

        // `MEM_REPLACE_PLACEHOLDER` : la plage est un placeholder issu d'une réservation précédente
        // (scindée par `VirtualFree(.., MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER)`) ; on la remappe
        // à l'adresse exacte, sans réaligner sur la granularité des grandes pages.
        if allocation_type & MEM_REPLACE_PLACEHOLDER != 0 && !address.is_null() {
            let start = (address as usize) & !0xFFF;
            let Some(end) = (address as usize)
                .checked_add(size)
                .map(|e| e.next_multiple_of(4096))
            else {
                return xmem_fail(ERROR_INVALID_PARAMETER, ptr::null_mut());
            };
            if xmem_region_for(&st, start, end - start).is_none() {
                return xmem_fail(ERROR_INVALID_ADDRESS, ptr::null_mut());
            }
            let map_prot = if allocation_type & MEM_COMMIT != 0 {
                prot
            } else {
                libc::PROT_NONE
            };
            // SAFETY: plage incluse dans une réservation `XMemVirtualAlloc` possédée par l'émulateur.
            let p = unsafe {
                libc::mmap(
                    start as *mut libc::c_void,
                    end - start,
                    map_prot,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | libc::MAP_FIXED,
                    -1,
                    0,
                )
            };
            if p == libc::MAP_FAILED {
                return xmem_fail(ERROR_NOT_ENOUGH_MEMORY, ptr::null_mut());
            }
            sb_trace!(
                "xmem",
                "XMemVirtualAlloc(placeholder {start:#x}+{size:#x}, type={allocation_type:#x}) -> {start:#x}"
            );
            return start as *mut c_void;
        }

        let Some(len) = size.checked_next_multiple_of(granularity) else {
            return xmem_fail(ERROR_INVALID_PARAMETER, ptr::null_mut());
        };
        let map_prot = if allocation_type & MEM_COMMIT != 0 {
            prot
        } else {
            libc::PROT_NONE
        };
        let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE;
        let base = if !address.is_null() {
            let want = (address as usize) & !(granularity - 1);
            // SAFETY: MAP_FIXED_NOREPLACE n'écrase jamais un mapping existant.
            let p = unsafe {
                libc::mmap(
                    want as *mut libc::c_void,
                    len,
                    map_prot,
                    flags | libc::MAP_FIXED_NOREPLACE,
                    -1,
                    0,
                )
            };
            if p == libc::MAP_FAILED || p as usize != want {
                if p != libc::MAP_FAILED {
                    // SAFETY: plage obtenue ci-dessus, non utilisée.
                    unsafe { libc::munmap(p, len) };
                }
                return xmem_fail(ERROR_INVALID_ADDRESS, ptr::null_mut());
            }
            want
        } else {
            // Sur-réserve puis rogne pour aligner sur la granularité (64 KiB / 2 MiB).
            let total = len + granularity;
            // SAFETY: réservation anonyme fraîche.
            let p = unsafe { libc::mmap(ptr::null_mut(), total, map_prot, flags, -1, 0) };
            if p == libc::MAP_FAILED {
                return xmem_fail(ERROR_NOT_ENOUGH_MEMORY, ptr::null_mut());
            }
            let raw = p as usize;
            let aligned = raw.next_multiple_of(granularity);
            // SAFETY: rognage des bords non alignés de la réservation qui vient d'être créée.
            unsafe {
                if aligned > raw {
                    libc::munmap(raw as *mut libc::c_void, aligned - raw);
                }
                let tail = raw + total - (aligned + len);
                if tail > 0 {
                    libc::munmap((aligned + len) as *mut libc::c_void, tail);
                }
            }
            aligned
        };
        st.regions.insert(base, XmemRegion { size: len, prot });
        sb_trace!(
            "xmem",
            "XMemVirtualAlloc(size={size}, type={allocation_type:#x}, attrs={attributes:#x}, protect={protect:#x}) -> {base:#x}"
        );
        base as *mut c_void
    })
}

/// `BOOL XMemTransferMemory(XMEM_TRANSFER_TYPE direction, SIZE_T size)` : renvoie `TRUE` (1) en
/// cas de succès, `FALSE` (0) si `size == 0` (ABI GDK : les appelants testent `if (!XMemTransferMemory(...))`).
static XMEM_TRANSFERRED: AtomicUsize = AtomicUsize::new(0);

extern "win64" fn XMemTransferMemory(direction: u32, size: usize) -> WinBool {
    ffi_guard!(CHANNEL, "XMemTransferMemory", WIN_FALSE, {
        if size == 0 {
            return xmem_fail(ERROR_INVALID_PARAMETER, WIN_FALSE);
        }
        XMEM_TRANSFERRED.fetch_add(size, Ordering::AcqRel);
        sb_info!(
            "xmem",
            "XMemTransferMemory(direction={direction}, size={size}) -> TRUE (partition unique, budget non borné)"
        );
        WIN_TRUE
    })
}

fn proc_status_kib(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(field))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
        })
        .unwrap_or(0)
}

fn host_total_phys_bytes() -> u64 {
    let fallback = 12u64 * 1024 * 1024 * 1024;
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
        })
        .map(|kib| kib.saturating_mul(1024))
        .filter(|&b| b > 0)
        .unwrap_or(fallback)
}

/// `BOOL XMemGetWorkingSetStatistics(UINT32 kind, XMEM_WORKING_SET_STATISTICS* out)` :
/// renvoie `TRUE` (1) si rempli, `FALSE` (0) sinon (un retour `0` signale « pas de données »).
extern "win64" fn XMemGetWorkingSetStatistics(
    kind: u32,
    out: *mut XMemWorkingSetStatistics,
) -> WinBool {
    ffi_guard!(CHANNEL, "XMemGetWorkingSetStatistics", WIN_FALSE, {
        if out.is_null() {
            return xmem_fail(ERROR_INVALID_PARAMETER, WIN_FALSE);
        }
        let stats = XMemWorkingSetStatistics {
            title_memory_limit_bytes: host_total_phys_bytes(),
            current_working_set_bytes: proc_status_kib("VmRSS:") * 1024,
        };
        // SAFETY: `out` vérifié non-nul.
        unsafe { ptr::write_unaligned(out, stats) };
        sb_trace!(
            "xmem",
            "XMemGetWorkingSetStatistics(kind={kind:#x}) -> TRUE ({stats:?})"
        );
        WIN_TRUE
    })
}

extern "win64" fn XMemIsToolsMemAvailable() -> WinBool {
    WIN_FALSE
}

/// `UINT XMemGetWriteWatch(DWORD flags, void* base, SIZE_T size, void** addrs, ULONG_PTR* count, ULONG* gran)`.
/// Renvoie `0` en cas de succès (`UINT::MAX` en cas d'erreur), avec 0 page sale et granularité 4 KiB.
extern "win64" fn XMemGetWriteWatch(
    _flags: u32,
    base_address: *mut c_void,
    region_size: usize,
    _addresses: *mut *mut c_void,
    count: *mut usize,
    granularity: *mut u32,
) -> u32 {
    ffi_guard!(CHANNEL, "XMemGetWriteWatch", u32::MAX, {
        if base_address.is_null() || region_size == 0 || count.is_null() {
            return xmem_fail(ERROR_INVALID_PARAMETER, u32::MAX);
        }
        // SAFETY: `count` vérifié non-nul.
        unsafe { ptr::write_unaligned(count, 0) };
        if !granularity.is_null() {
            // SAFETY: `granularity` vérifié non-nul.
            unsafe { ptr::write_unaligned(granularity, 4096) };
        }
        0
    })
}

/// `UINT XMemResetWriteWatch(void* base, SIZE_T size)` : renvoie `0` en cas de succès.
extern "win64" fn XMemResetWriteWatch(base_address: *mut c_void, region_size: usize) -> u32 {
    ffi_guard!(CHANNEL, "XMemResetWriteWatch", u32::MAX, {
        if base_address.is_null() || region_size == 0 {
            return xmem_fail(ERROR_INVALID_PARAMETER, u32::MAX);
        }
        0
    })
}

// ---------- XboxBvhBuilder.dll : classe C++ `XboxBvhBuilder` (thiscall, rcx = this) ----------
//
// L'objet est alloué par l'appelant (taille inconnue) : aucun octet n'y est écrit, l'état est
// conservé dans une table annexe indexée par `this`. Le backend Vulkan n'active pas
// `VK_KHR_acceleration_structure` : sans raytracing (rt=false), les tailles de prébuild sont
// nulles et les constructions/copies sont des no-op journalisés.

/// `DXGI_ERROR_UNSUPPORTED`.
const DXGI_ERROR_UNSUPPORTED: HResult = 0x887A_0004_u32 as i32;

#[derive(Debug, Clone, Copy)]
struct BvhBuilderState {
    rt: bool,
    morton64: bool,
}

static BVH_BUILDERS: LazyLock<Mutex<HashMap<usize, BvhBuilderState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn bvh_rt(this: *mut c_void) -> bool {
    BVH_BUILDERS
        .lock()
        .get(&(this as usize))
        .is_some_and(|s| s.rt)
}

/// `XboxBvhBuilder::XboxBvhBuilder(ID3D12Device11*, bool)`.
extern "win64" fn XboxBvhBuilder_ctor(
    this: *mut c_void,
    device: *mut c_void,
    flag: u8,
) -> *mut c_void {
    ffi_guard!("bvh", "XboxBvhBuilder::XboxBvhBuilder", this, {
        let rt = crate::d3d12_xs::device_raytracing_available(device).unwrap_or(false);
        if !this.is_null() {
            BVH_BUILDERS.lock().insert(
                this as usize,
                BvhBuilderState {
                    rt,
                    morton64: false,
                },
            );
        }
        sb_info!(
            "bvh",
            "XboxBvhBuilder({this:p}, device={device:p}, flag={flag}) -> rt={rt}"
        );
        this
    })
}

/// `XboxBvhBuilder::~XboxBvhBuilder()`.
extern "win64" fn XboxBvhBuilder_dtor(this: *mut c_void) {
    ffi_guard!("bvh", "XboxBvhBuilder::~XboxBvhBuilder", (), {
        BVH_BUILDERS.lock().remove(&(this as usize));
    })
}

extern "win64" fn XboxBvhBuilder_BuildBatched2X(
    this: *mut c_void,
    _cmd_list: *mut c_void,
    count: u32,
    _descs: *const c_void,
) {
    ffi_guard!(
        "bvh",
        "BuildRaytracingAccelerationStructureBatched2X",
        (),
        {
            sb_debug!(
                "bvh",
                "BuildRaytracingAccelerationStructureBatched2X(count={count}) -> no-op (rt={}, backend AS Vulkan absent)",
                bvh_rt(this)
            );
        }
    )
}

extern "win64" fn XboxBvhBuilder_Copy(
    this: *mut c_void,
    _cmd_list: *mut c_void,
    dest: u64,
    src: u64,
    mode: u32,
) {
    ffi_guard!("bvh", "CopyRaytracingAccelerationStructure", (), {
        sb_debug!(
            "bvh",
            "CopyRaytracingAccelerationStructure({src:#x} -> {dest:#x}, mode={mode}) -> no-op (rt={})",
            bvh_rt(this)
        );
    })
}

extern "win64" fn XboxBvhBuilder_GetPrebuildInfo(
    this: *mut c_void,
    _inputs: *const c_void,
    _stream_desc: *const c_void,
    out_info: *mut D3D12RaytracingPrebuildInfo,
) {
    ffi_guard!(
        "bvh",
        "GetRaytracingAccelerationStructurePrebuildInfo",
        (),
        {
            if out_info.is_null() {
                return;
            }
            // Sans backend AS, aucune mémoire BVH n'est requise : tailles nulles.
            // SAFETY: `out_info` vérifié non-nul.
            unsafe {
                *out_info = D3D12RaytracingPrebuildInfo {
                    result_data_max_size: 0,
                    scratch_data_size: 0,
                    update_scratch_data_size: 0,
                };
            }
            sb_debug!(
                "bvh",
                "GetRaytracingAccelerationStructurePrebuildInfo -> 0 (rt={})",
                bvh_rt(this)
            );
        }
    )
}

extern "win64" fn XboxBvhBuilder_RehydrateBatched2X(
    this: *mut c_void,
    _cmd_list: *mut c_void,
    count: u32,
    _descs: *const c_void,
    _sizes: *const u64,
) -> HResult {
    ffi_guard!(
        "bvh",
        "RehydrateRaytracingAccelerationStructureBatched2X",
        E_INVALIDARG,
        {
            sb_warn!(
                "bvh",
                "RehydrateRaytracingAccelerationStructureBatched2X(count={count}) -> DXGI_ERROR_UNSUPPORTED (rt={})",
                bvh_rt(this)
            );
            DXGI_ERROR_UNSUPPORTED
        }
    )
}

extern "win64" fn XboxBvhBuilder_Set64BitMortonCodes(this: *mut c_void, enable: u8) {
    ffi_guard!("bvh", "Set64BitMortonCodes", (), {
        if let Some(s) = BVH_BUILDERS.lock().get_mut(&(this as usize)) {
            s.morton64 = enable != 0;
        }
    })
}

#[cfg(test)]
mod xmem_bvh_tests {
    use super::*;

    #[test]
    fn physical_pages_double_mapping_is_coherent_and_unmappable() {
        let teb = sb_loader::init_thread_teb(0).is_ok();
        let mut count = 2usize;
        let mut pfns = [0usize; 2];
        assert_eq!(
            XMemAllocatePhysicalPages(0x380, &mut count, pfns.as_mut_ptr()),
            WIN_TRUE
        );
        assert_eq!(count, 2);
        assert_ne!(pfns[0], pfns[1]);

        let size = 4 * XMEM_PHYSICAL_PAGE_SIZE;
        let a = XMemVirtualAlloc(
            ptr::null_mut(),
            size,
            MEM_RESERVE | XMEM_MEM_64K_PAGES,
            0x1,
            0x04,
        );
        let b = XMemVirtualAlloc(
            ptr::null_mut(),
            size,
            MEM_RESERVE | XMEM_MEM_64K_PAGES,
            0x20,
            0x2004,
        );
        assert!(!a.is_null() && !b.is_null());
        assert_eq!(a as usize % XMEM_PHYSICAL_PAGE_SIZE, 0);

        assert_eq!(XMemMapPhysicalPages(a, 2, pfns.as_ptr()), WIN_TRUE);
        // Même page physique mappée à deux adresses (scatter, ordre inversé).
        let vas = [(b as usize + XMEM_PHYSICAL_PAGE_SIZE) as *mut c_void, b];
        assert_eq!(
            XMemMapPhysicalPagesScatter(vas.as_ptr(), 2, pfns.as_ptr()),
            WIN_TRUE
        );
        // SAFETY: pages mappées en RW ci-dessus.
        unsafe {
            *(a as *mut u32) = 0xDEAD_BEEF;
            *((a as usize + XMEM_PHYSICAL_PAGE_SIZE + 8) as *mut u32) = 0x1234_5678;
            assert_eq!(
                *((b as usize + XMEM_PHYSICAL_PAGE_SIZE) as *const u32),
                0xDEAD_BEEF
            );
            assert_eq!(*((b as usize + 8) as *const u32), 0x1234_5678);
        }

        // Démappage (page_array NULL) puis erreurs propres.
        assert_eq!(XMemMapPhysicalPages(a, 2, ptr::null()), WIN_TRUE);
        let bogus = [0xFFFF_FFFFusize];
        assert_eq!(XMemMapPhysicalPages(a, 1, bogus.as_ptr()), WIN_FALSE);
        if teb {
            assert_eq!(sb_loader::last_error(), ERROR_INVALID_PARAMETER);
        }
        let mut outside = 0u64;
        assert_eq!(
            XMemMapPhysicalPages((&raw mut outside).cast(), 1, pfns.as_ptr()),
            WIN_FALSE
        );

        assert_eq!(XMemFreePhysicalPages(2, pfns.as_ptr()), WIN_TRUE);
        assert_eq!(XMemFreePhysicalPages(2, pfns.as_ptr()), WIN_FALSE);
        assert!(
            XMEM_STATE
                .lock()
                .mappings
                .values()
                .all(|p| !pfns.contains(p))
        );
    }

    #[test]
    fn virtual_alloc_commit_large_pages_and_virtual_query() {
        let teb = sb_loader::init_thread_teb(0).is_ok();
        let p = XMemVirtualAlloc(
            ptr::null_mut(),
            100,
            MEM_COMMIT | MEM_RESERVE | MEM_LARGE_PAGES | 0x4000,
            0x100,
            0x04 | 0x2000 | 0x400,
        );
        assert!(!p.is_null());
        assert_eq!(p as usize % XMEM_LARGE_PAGE_SIZE, 0);
        // SAFETY: mémoire engagée RW.
        unsafe {
            *(p as *mut u64) = 42;
            assert_eq!(*(p as *const u64), 42);
        }
        // Cohérence avec kernel32!VirtualQuery (via la table d'exports réelle).
        let vq = crate::kernel32::exports()["VirtualQuery"];
        // SAFETY: adresse d'une fonction `extern "win64"` (PCVOID, PMEMORY_BASIC_INFORMATION, SIZE_T).
        let vq: extern "win64" fn(*const c_void, *mut u8, usize) -> usize =
            unsafe { core::mem::transmute(vq) };
        let mut mbi = [0u8; 48];
        assert_eq!(vq(p, mbi.as_mut_ptr(), mbi.len()), 48);
        let base = u64::from_le_bytes(mbi[0..8].try_into().expect("8 octets"));
        if base != 0 {
            assert_eq!(base, p as u64);
            let state = u32::from_le_bytes(mbi[32..36].try_into().expect("4 octets"));
            assert_eq!(state, MEM_COMMIT);
        }

        // Placeholder : réserve large, scission, puis remplacement à l'adresse exacte (grandes pages).
        let big = XMemVirtualAlloc(
            ptr::null_mut(),
            8 << 20,
            MEM_RESERVE | 0x0040_0000,
            0x7,
            0x04,
        );
        assert!(!big.is_null());
        let part = (big as usize + 0x10000) as *mut c_void;
        let repl = XMemVirtualAlloc(
            part,
            0x20000,
            MEM_RESERVE | MEM_REPLACE_PLACEHOLDER | MEM_LARGE_PAGES | 0x0040_0000,
            0x27,
            0x04,
        );
        assert_eq!(repl, part);
        // Hors de toute réservation : refusé.
        assert!(
            XMemVirtualAlloc(
                0x10_0000_0000usize as *mut c_void,
                0x10000,
                MEM_RESERVE | MEM_REPLACE_PLACEHOLDER,
                0x7,
                0x04
            )
            .is_null()
        );

        // Réserve puis commit partiel.
        let r = XMemVirtualAlloc(ptr::null_mut(), 1 << 20, MEM_RESERVE, 0x2, 0x01);
        assert!(!r.is_null());
        let c = XMemVirtualAlloc(r, 4096, MEM_COMMIT, 0x2, 0x04);
        assert_eq!(c, r);
        // SAFETY: page engagée RW.
        unsafe { *(c as *mut u8) = 7 };

        // Paramètres invalides.
        assert!(XMemVirtualAlloc(ptr::null_mut(), 0, MEM_COMMIT, 1, 0x04).is_null());
        assert!(XMemVirtualAlloc(ptr::null_mut(), 4096, MEM_COMMIT, 1, 0x03).is_null());
        let mut local = 0u64;
        assert!(XMemVirtualAlloc((&raw mut local).cast(), 8, MEM_COMMIT, 1, 0x04).is_null());
        if teb {
            assert_eq!(sb_loader::last_error(), ERROR_INVALID_ADDRESS);
        }
    }

    #[test]
    fn stats_tools_and_transfer() {
        let mut s = XMemWorkingSetStatistics::default();
        assert_eq!(XMemGetWorkingSetStatistics(0x2000_0001, &mut s), WIN_TRUE);
        assert!(s.title_memory_limit_bytes >= 1024 * 1024 * 1024);
        assert!(s.current_working_set_bytes > 0);
        assert_eq!(
            XMemGetWorkingSetStatistics(0x2000_0001, ptr::null_mut()),
            WIN_FALSE
        );
        assert_eq!(XMemIsToolsMemAvailable(), WIN_FALSE);
        assert_eq!(XMemTransferMemory(1, 16 << 20), WIN_TRUE);
        assert_eq!(XMemTransferMemory(1, 0), WIN_FALSE);
        let mut dummy = 0u64;
        let mut count = 4usize;
        let mut gran = 0u32;
        assert_eq!(
            XMemGetWriteWatch(
                0,
                (&raw mut dummy).cast(),
                4096,
                ptr::null_mut(),
                &mut count,
                &mut gran
            ),
            0
        );
        assert_eq!(count, 0);
        assert_eq!(gran, 4096);
        assert_eq!(XMemResetWriteWatch((&raw mut dummy).cast(), 4096), 0);
        let ex = xmem_exports();
        for n in [
            "XMemAllocatePhysicalPages",
            "XMemFreePhysicalPages",
            "XMemMapPhysicalPages",
            "XMemMapPhysicalPagesScatter",
            "XMemTransferMemory",
            "XMemVirtualAlloc",
            "XMemGetWorkingSetStatistics",
            "XMemIsToolsMemAvailable",
            "XMemGetWriteWatch",
            "XMemResetWriteWatch",
        ] {
            assert!(ex.contains_key(n), "{n}");
        }
    }

    #[test]
    fn bvh_builder_without_rt_reports_zero_sizes() {
        let mut storage = [0u64; 16];
        let this = storage.as_mut_ptr().cast::<c_void>();
        let mut fake_device = 0usize;
        assert_eq!(
            XboxBvhBuilder_ctor(this, (&raw mut fake_device).cast(), 1),
            this
        );
        assert_eq!(storage, [0u64; 16], "l'objet de l'appelant n'est pas écrit");
        let mut info = D3D12RaytracingPrebuildInfo {
            result_data_max_size: 1,
            scratch_data_size: 1,
            update_scratch_data_size: 1,
        };
        XboxBvhBuilder_GetPrebuildInfo(this, ptr::null(), ptr::null(), &mut info);
        assert_eq!(
            (
                info.result_data_max_size,
                info.scratch_data_size,
                info.update_scratch_data_size
            ),
            (0, 0, 0)
        );
        XboxBvhBuilder_Set64BitMortonCodes(this, 1);
        assert!(BVH_BUILDERS.lock()[&(this as usize)].morton64);
        XboxBvhBuilder_BuildBatched2X(this, ptr::null_mut(), 1, ptr::null());
        XboxBvhBuilder_Copy(this, ptr::null_mut(), 0x1000, 0x2000, 0);
        assert_eq!(
            XboxBvhBuilder_RehydrateBatched2X(this, ptr::null_mut(), 1, ptr::null(), ptr::null()),
            DXGI_ERROR_UNSUPPORTED
        );
        XboxBvhBuilder_dtor(this);
        assert!(!BVH_BUILDERS.lock().contains_key(&(this as usize)));
        let ex = bvh_exports();
        assert_eq!(ex.len(), 10);
        assert!(ex.contains_key("??0XboxBvhBuilder@@QEAA@PEAUID3D12Device11@@_N@Z"));
    }
}
