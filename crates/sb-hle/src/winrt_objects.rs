//! Implémentation HLE COM/WinRT des classes système Xbox/UWP :
//! - `Windows.ApplicationModel.Core.CoreApplication` & `CoreApplicationView`
//! - `Windows.UI.Core.CoreWindow` & `CoreDispatcher` (fenêtre SDL3 Wayland)
//! - `Windows.UI.Core.SystemNavigationManager`
//! - `Windows.Storage.ApplicationData` & `StorageFolder` (connectés à `sb-vfs`)
//! - `Windows.System.UserProfile.GlobalizationPreferences`
//! - `Windows.Gaming.Input.Gamepad` (connecté à SDL3 Gamepad)

#![allow(non_snake_case, unused_variables)]

use core::ffi::c_void;
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;
use sb_common::abi::{E_NOTIMPL, E_POINTER, E_UNEXPECTED, HResult, S_OK};
use sb_common::{ffi_guard, sb_debug, sb_info, sb_warn};
use sb_gpu::{Sdl3Lib, Sdl3Window};
use sb_vfs::VfsMount;

use crate::com::{
    ComHeader, Guid, IID_IAPPLICATION_DATA_STATICS, IID_ICOREAPPLICATION,
    IID_ICOREAPPLICATION_EXIT, IID_ICOREAPPLICATION_VIEW, IID_ICOREDISPATCHER, IID_ICOREWINDOW,
    IID_IGAMEPAD_STATICS, IID_IGLOBALIZATION_PREFS_STATICS, IID_ISYSTEM_NAV_STATICS, VtblSlot,
    com_add_ref, com_get_iids, com_get_runtime_class_name, com_get_trust_level,
    com_query_interface_list, com_release, format_guid_opt, vslot,
};
use crate::winrt::create_hstring;

const CHANNEL: &str = "winrt_obj";

static WINDOW_CLOSED: AtomicBool = AtomicBool::new(false);
static DISPATCH_TICKS: AtomicU64 = AtomicU64::new(0);
static ACTIVATED_HANDLER: Mutex<Option<usize>> = Mutex::new(None);
static CLOSED_HANDLER: Mutex<Option<usize>> = Mutex::new(None);
static SDL_WINDOW: OnceLock<Mutex<Option<Sdl3Window>>> = OnceLock::new();

pub fn is_headless() -> bool {
    std::env::var("SB_HEADLESS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

pub fn request_window_close() {
    WINDOW_CLOSED.store(true, Ordering::Release);
}

fn ensure_sdl_window() -> &'static Mutex<Option<Sdl3Window>> {
    SDL_WINDOW.get_or_init(|| {
        if is_headless() {
            sb_info!(CHANNEL, "mode SB_HEADLESS=1 : pas de fenêtre SDL3 ouverte");
            Mutex::new(None)
        } else {
            Mutex::new(Sdl3Window::new("ScarletBridge — Xbox Runtime", 1280, 720))
        }
    })
}

pub fn create_vulkan_surface_for_window(
    instance: sb_gpu::ash::vk::Instance,
) -> Option<sb_gpu::ash::vk::SurfaceKHR> {
    let lock = ensure_sdl_window();
    let guard = lock.lock();
    guard.as_ref()?.create_vulkan_surface(instance)
}

/// Résout une factory d'activation WinRT par nom de classe complet.
pub(crate) fn resolve_activation_factory(
    class_name: &str,
    iid: *const Guid,
    out_factory: *mut *mut c_void,
) -> HResult {
    if out_factory.is_null() {
        return E_POINTER;
    }

    let factory_ptr: Option<*mut c_void> = match class_name {
        "Windows.ApplicationModel.Core.CoreApplication" => Some(
            Box::into_raw(Box::new(CoreApplicationObject {
                header: ComHeader::new((&raw const CORE_APPLICATION_VTBL).cast()),
            }))
            .cast(),
        ),
        "Windows.UI.Core.SystemNavigationManager" => Some(
            Box::into_raw(Box::new(SysNavStaticsObject {
                header: ComHeader::new((&raw const SYS_NAV_STATICS_VTBL).cast()),
            }))
            .cast(),
        ),
        "Windows.Storage.ApplicationData" => Some(
            Box::into_raw(Box::new(AppDataStaticsObject {
                header: ComHeader::new((&raw const APP_DATA_STATICS_VTBL).cast()),
            }))
            .cast(),
        ),
        "Windows.System.UserProfile.GlobalizationPreferences" => Some(
            Box::into_raw(Box::new(GlobPrefsStaticsObject {
                header: ComHeader::new((&raw const GLOB_PREFS_STATICS_VTBL).cast()),
            }))
            .cast(),
        ),
        "Windows.Gaming.Input.Gamepad" => Some(
            Box::into_raw(Box::new(GamepadStaticsObject {
                header: ComHeader::new((&raw const GAMEPAD_STATICS_VTBL).cast()),
            }))
            .cast(),
        ),
        _ => None,
    };

    if let Some(ptr_val) = factory_ptr {
        sb_info!(
            CHANNEL,
            "ActivationFactory('{class_name}', iid={}) -> {ptr_val:p}",
            format_guid_opt(iid)
        );
        // SAFETY: `out_factory` vérifié non-nul.
        unsafe {
            *out_factory = ptr_val;
        }
        S_OK
    } else {
        sb_warn!(
            CHANNEL,
            "ActivationFactory inconnue : '{class_name}' (iid={})",
            format_guid_opt(iid)
        );
        // SAFETY: `out_factory` vérifié non-nul.
        unsafe {
            *out_factory = ptr::null_mut();
        }
        E_NOTIMPL
    }
}

// ==================== CoreApplication (`ICoreApplication`) ====================

#[repr(C)]
struct CoreApplicationVtbl {
    slots: [VtblSlot; 16],
}

#[repr(C)]
struct CoreApplicationObject {
    header: ComHeader,
}

extern "win64" fn core_app_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "CoreApplication",
        this,
        riid,
        ppv,
        &[IID_ICOREAPPLICATION, IID_ICOREAPPLICATION_EXIT],
    )
}

extern "win64" fn core_app_get_id(_this: *mut CoreApplicationObject, out_id: *mut u64) -> HResult {
    if !out_id.is_null() {
        // SAFETY: `out_id` non-nul.
        unsafe {
            *out_id = create_hstring("App");
        }
    }
    S_OK
}

extern "win64" fn core_app_add_event(
    _this: *mut CoreApplicationObject,
    _handler: *mut c_void,
    out_token: *mut i64,
) -> HResult {
    if !out_token.is_null() {
        // SAFETY: `out_token` non-nul.
        unsafe {
            *out_token = 1;
        }
    }
    S_OK
}

extern "win64" fn core_app_remove_event(_this: *mut CoreApplicationObject, _token: i64) -> HResult {
    S_OK
}

extern "win64" fn core_app_get_current_view(
    _this: *mut CoreApplicationObject,
    out_view: *mut *mut c_void,
) -> HResult {
    if out_view.is_null() {
        return E_POINTER;
    }
    let window = make_core_window();
    let view = make_core_app_view(window);
    // SAFETY: `out_view` non-nul.
    unsafe {
        *out_view = view;
    }
    S_OK
}

extern "win64" fn core_app_run(
    _this: *mut CoreApplicationObject,
    view_source: *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CoreApplication::Run", E_UNEXPECTED, {
        if view_source.is_null() {
            return E_POINTER;
        }
        sb_info!(
            CHANNEL,
            "CoreApplication::Run(view_source={view_source:p}) — démarrage cycle de vie UWP/Xbox !"
        );

        // Initialise la fenêtre SDL3 Wayland (si non headless).
        let _ = ensure_sdl_window();

        // 1. `IFrameworkViewSource::CreateView(&view)` (slot 6).
        let mut view_ptr: *mut c_void = ptr::null_mut();
        // SAFETY: `view_source` implémente `IFrameworkViewSource` (slot 6 = `CreateView`).
        let hr = unsafe {
            let vtbl = *(view_source.cast::<*const usize>());
            let create_view: unsafe extern "win64" fn(*mut c_void, *mut *mut c_void) -> HResult =
                core::mem::transmute(*vtbl.add(6));
            create_view(view_source, &mut view_ptr)
        };
        if hr < 0 || view_ptr.is_null() {
            sb_warn!(CHANNEL, "CreateView a échoué : hr={hr:#x}");
            return hr;
        }
        sb_info!(CHANNEL, "IFrameworkView créé : {view_ptr:p}");

        let core_window = make_core_window();
        let app_view = make_core_app_view(core_window);

        // SAFETY: `view_ptr` implémente `IFrameworkView` :
        // slot 6 = Initialize(CoreApplicationView)
        // slot 7 = SetWindow(CoreWindow)
        // slot 8 = Load(HSTRING entryPoint)
        // slot 9 = Run()
        // slot 10 = Uninitialize()
        unsafe {
            let vtbl = *(view_ptr.cast::<*const usize>());
            let initialize: unsafe extern "win64" fn(*mut c_void, *mut c_void) -> HResult =
                core::mem::transmute(*vtbl.add(6));
            let set_window: unsafe extern "win64" fn(*mut c_void, *mut c_void) -> HResult =
                core::mem::transmute(*vtbl.add(7));
            let load: unsafe extern "win64" fn(*mut c_void, u64) -> HResult =
                core::mem::transmute(*vtbl.add(8));
            let run: unsafe extern "win64" fn(*mut c_void) -> HResult =
                core::mem::transmute(*vtbl.add(9));
            let uninitialize: unsafe extern "win64" fn(*mut c_void) -> HResult =
                core::mem::transmute(*vtbl.add(10));

            sb_info!(
                CHANNEL,
                "-> IFrameworkView::Initialize(CoreApplicationView)"
            );
            let hr1 = initialize(view_ptr, app_view);
            sb_debug!(CHANNEL, "   Initialize -> {hr1:#x}");

            sb_info!(CHANNEL, "-> IFrameworkView::SetWindow(CoreWindow)");
            let hr2 = set_window(view_ptr, core_window);
            sb_debug!(CHANNEL, "   SetWindow -> {hr2:#x}");

            if ACTIVATED_HANDLER.lock().is_some() {
                sb_debug!(
                    CHANNEL,
                    "handler CoreApplicationView::Activated enregistré — fenêtre CoreWindow prête"
                );
            }

            sb_info!(CHANNEL, "-> IFrameworkView::Load(\"App\")");
            let entry_hstr = create_hstring("App");
            let hr3 = load(view_ptr, entry_hstr);
            sb_debug!(CHANNEL, "   Load -> {hr3:#x}");

            sb_info!(
                CHANNEL,
                "-> IFrameworkView::Run() — entrée dans la boucle principale du jeu !"
            );
            let hr4 = run(view_ptr);
            sb_info!(CHANNEL, "<- IFrameworkView::Run() terminé (hr={hr4:#x})");

            let _ = uninitialize(view_ptr);
        }

        S_OK
    })
}

extern "win64" fn core_app_noop() -> HResult {
    S_OK
}

static CORE_APPLICATION_VTBL: CoreApplicationVtbl = CoreApplicationVtbl {
    slots: [
        vslot(core_app_qi as *const c_void),                // 0: QI
        vslot(com_add_ref as *const c_void),                // 1: AddRef
        vslot(com_release as *const c_void),                // 2: Release
        vslot(com_get_iids as *const c_void),               // 3: GetIids
        vslot(com_get_runtime_class_name as *const c_void), // 4: GetRuntimeClassName
        vslot(com_get_trust_level as *const c_void),        // 5: GetTrustLevel
        vslot(core_app_get_id as *const c_void),            // 6: get_Id
        vslot(core_app_add_event as *const c_void),         // 7: add_Suspending
        vslot(core_app_remove_event as *const c_void),      // 8: remove_Suspending
        vslot(core_app_add_event as *const c_void),         // 9: add_Resuming
        vslot(core_app_remove_event as *const c_void),      // 10: remove_Resuming
        vslot(core_app_noop as *const c_void),              // 11: get_Properties
        vslot(core_app_get_current_view as *const c_void),  // 12: GetCurrentView
        vslot(core_app_run as *const c_void),               // 13: Run
        vslot(core_app_noop as *const c_void),              // 14: RunWithActivationFactories
        vslot(core_app_noop as *const c_void), // 15: Exit (si QI vers ICoreApplicationExit)
    ],
};

// ==================== CoreApplicationView ====================

#[repr(C)]
struct CoreAppViewVtbl {
    slots: [VtblSlot; 12],
}

#[repr(C)]
struct CoreAppViewObject {
    header: ComHeader,
    window: *mut c_void,
}

extern "win64" fn app_view_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "CoreApplicationView",
        this,
        riid,
        ppv,
        &[IID_ICOREAPPLICATION_VIEW],
    )
}

extern "win64" fn app_view_get_window(
    this: *mut CoreAppViewObject,
    out_win: *mut *mut c_void,
) -> HResult {
    if this.is_null() || out_win.is_null() {
        return E_POINTER;
    }
    // SAFETY: `this` et `out_win` non-nuls.
    unsafe {
        *out_win = (*this).window;
    }
    S_OK
}

extern "win64" fn app_view_add_activated(
    _this: *mut CoreAppViewObject,
    handler: *mut c_void,
    out_token: *mut i64,
) -> HResult {
    if !handler.is_null() {
        *ACTIVATED_HANDLER.lock() = Some(handler as usize);
    }
    if !out_token.is_null() {
        // SAFETY: `out_token` non-nul.
        unsafe {
            *out_token = 1;
        }
    }
    S_OK
}

extern "win64" fn app_view_get_bool_true(_this: *mut CoreAppViewObject, out: *mut u8) -> HResult {
    if !out.is_null() {
        // SAFETY: `out` non-nul.
        unsafe {
            *out = 1;
        }
    }
    S_OK
}

static CORE_APP_VIEW_VTBL: CoreAppViewVtbl = CoreAppViewVtbl {
    slots: [
        vslot(app_view_qi as *const c_void),
        vslot(com_add_ref as *const c_void),
        vslot(com_release as *const c_void),
        vslot(com_get_iids as *const c_void),
        vslot(com_get_runtime_class_name as *const c_void),
        vslot(com_get_trust_level as *const c_void),
        vslot(app_view_get_window as *const c_void), // 6: get_CoreWindow
        vslot(app_view_add_activated as *const c_void), // 7: add_Activated
        vslot(core_app_noop as *const c_void),       // 8: remove_Activated
        vslot(app_view_get_bool_true as *const c_void), // 9: get_IsMain
        vslot(app_view_get_bool_true as *const c_void), // 10: get_IsHosted
        vslot(core_app_noop as *const c_void),
    ],
};

fn make_core_app_view(window: *mut c_void) -> *mut c_void {
    Box::into_raw(Box::new(CoreAppViewObject {
        header: ComHeader::new((&raw const CORE_APP_VIEW_VTBL).cast()),
        window,
    }))
    .cast()
}

// ==================== CoreWindow & CoreDispatcher ====================

#[repr(C)]
struct WinRtRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[repr(C)]
struct CoreDispatcherVtbl {
    slots: [VtblSlot; 10],
}

#[repr(C)]
struct CoreDispatcherObject {
    header: ComHeader,
    window: *mut c_void,
}

extern "win64" fn disp_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("CoreDispatcher", this, riid, ppv, &[IID_ICOREDISPATCHER])
}

extern "win64" fn disp_has_thread_access(
    _this: *mut CoreDispatcherObject,
    out_val: *mut u8,
) -> HResult {
    if !out_val.is_null() {
        // SAFETY: `out_val` non-nul.
        unsafe {
            *out_val = 1;
        }
    }
    S_OK
}

extern "win64" fn disp_process_events(this: *mut CoreDispatcherObject, _options: i32) -> HResult {
    ffi_guard!(CHANNEL, "CoreDispatcher::ProcessEvents", E_UNEXPECTED, {
        let tick = DISPATCH_TICKS.fetch_add(1, Ordering::AcqRel) + 1;
        if let Some(lock) = SDL_WINDOW.get()
            && let Some(ref mut win) = *lock.lock()
            && !win.poll_events()
        {
            WINDOW_CLOSED.store(true, Ordering::Release);
        }

        if let Some(limit) = crate::d3d11::max_frames_limit()
            && tick >= limit.saturating_mul(2).max(10)
        {
            WINDOW_CLOSED.store(true, Ordering::Release);
        }

        if WINDOW_CLOSED.load(Ordering::Acquire) {
            sb_info!(
                CHANNEL,
                "CoreWindow fermée — arrêt propre de la boucle principale"
            );
            std::process::exit(0);
        }
        S_OK
    })
}

static CORE_DISPATCHER_VTBL: CoreDispatcherVtbl = CoreDispatcherVtbl {
    slots: [
        vslot(disp_qi as *const c_void),
        vslot(com_add_ref as *const c_void),
        vslot(com_release as *const c_void),
        vslot(com_get_iids as *const c_void),
        vslot(com_get_runtime_class_name as *const c_void),
        vslot(com_get_trust_level as *const c_void),
        vslot(disp_has_thread_access as *const c_void), // 6: get_HasThreadAccess
        vslot(disp_process_events as *const c_void),    // 7: ProcessEvents
        vslot(core_app_noop as *const c_void),          // 8: RunAsync
        vslot(core_app_noop as *const c_void),          // 9: RunIdleAsync
    ],
};

#[repr(C)]
struct CoreWindowVtbl {
    slots: [VtblSlot; 57],
}

#[repr(C)]
struct CoreWindowObject {
    header: ComHeader,
    dispatcher: *mut c_void,
}

extern "win64" fn cw_qi(this: *mut ComHeader, riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    com_query_interface_list("CoreWindow", this, riid, ppv, &[IID_ICOREWINDOW])
}

extern "win64" fn cw_get_bounds(_this: *mut CoreWindowObject, out_rect: *mut WinRtRect) -> HResult {
    if !out_rect.is_null() {
        // SAFETY: `out_rect` non-nul.
        unsafe {
            *out_rect = WinRtRect {
                x: 0.0,
                y: 0.0,
                width: 1280.0,
                height: 720.0,
            };
        }
    }
    S_OK
}

extern "win64" fn cw_get_dispatcher(
    this: *mut CoreWindowObject,
    out_disp: *mut *mut c_void,
) -> HResult {
    if this.is_null() || out_disp.is_null() {
        return E_POINTER;
    }
    // SAFETY: `this` et `out_disp` non-nuls.
    unsafe {
        *out_disp = (*this).dispatcher;
    }
    S_OK
}

extern "win64" fn cw_get_visible(_this: *mut CoreWindowObject, out_vis: *mut u8) -> HResult {
    if !out_vis.is_null() {
        // SAFETY: `out_vis` non-nul.
        unsafe {
            *out_vis = u8::from(!WINDOW_CLOSED.load(Ordering::Acquire));
        }
    }
    S_OK
}

extern "win64" fn cw_activate(_this: *mut CoreWindowObject) -> HResult {
    sb_info!(CHANNEL, "CoreWindow::Activate()");
    S_OK
}

extern "win64" fn cw_close(_this: *mut CoreWindowObject) -> HResult {
    WINDOW_CLOSED.store(true, Ordering::Release);
    S_OK
}

extern "win64" fn cw_get_key_state(
    _this: *mut CoreWindowObject,
    _vkey: i32,
    out_state: *mut u32,
) -> HResult {
    if !out_state.is_null() {
        // SAFETY: `out_state` non-nul.
        unsafe {
            *out_state = 0;
        }
    }
    S_OK
}

extern "win64" fn cw_add_closed(
    _this: *mut CoreWindowObject,
    handler: *mut c_void,
    out_token: *mut i64,
) -> HResult {
    if !handler.is_null() {
        *CLOSED_HANDLER.lock() = Some(handler as usize);
    }
    if !out_token.is_null() {
        // SAFETY: `out_token` non-nul.
        unsafe {
            *out_token = 1;
        }
    }
    S_OK
}

extern "win64" fn cw_add_event_noop(
    _this: *mut CoreWindowObject,
    _handler: *mut c_void,
    out_token: *mut i64,
) -> HResult {
    if !out_token.is_null() {
        // SAFETY: `out_token` non-nul.
        unsafe {
            *out_token = 1;
        }
    }
    S_OK
}

const fn build_core_window_vtbl() -> CoreWindowVtbl {
    let mut slots = [vslot(core_app_noop as *const c_void); 57];
    slots[0] = vslot(cw_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[3] = vslot(com_get_iids as *const c_void);
    slots[4] = vslot(com_get_runtime_class_name as *const c_void);
    slots[5] = vslot(com_get_trust_level as *const c_void);
    slots[7] = vslot(cw_get_bounds as *const c_void);
    slots[9] = vslot(cw_get_dispatcher as *const c_void);
    slots[16] = vslot(cw_get_visible as *const c_void);
    slots[17] = vslot(cw_activate as *const c_void);
    slots[18] = vslot(cw_close as *const c_void);
    slots[19] = vslot(cw_get_key_state as *const c_void);
    slots[20] = vslot(cw_get_key_state as *const c_void);
    slots[23] = vslot(cw_add_event_noop as *const c_void);
    slots[27] = vslot(cw_add_event_noop as *const c_void);
    slots[29] = vslot(cw_add_closed as *const c_void);
    slots[33] = vslot(cw_add_event_noop as *const c_void);
    slots[35] = vslot(cw_add_event_noop as *const c_void);
    slots[43] = vslot(cw_add_event_noop as *const c_void);
    slots[45] = vslot(cw_add_event_noop as *const c_void);
    slots[47] = vslot(cw_add_event_noop as *const c_void);
    slots[51] = vslot(cw_add_event_noop as *const c_void);
    slots[53] = vslot(cw_add_event_noop as *const c_void);
    slots[55] = vslot(cw_add_event_noop as *const c_void);
    CoreWindowVtbl { slots }
}

static CORE_WINDOW_VTBL: CoreWindowVtbl = build_core_window_vtbl();

fn make_core_window() -> *mut c_void {
    let win = Box::into_raw(Box::new(CoreWindowObject {
        header: ComHeader::new((&raw const CORE_WINDOW_VTBL).cast()),
        dispatcher: ptr::null_mut(),
    }));
    let disp = Box::into_raw(Box::new(CoreDispatcherObject {
        header: ComHeader::new((&raw const CORE_DISPATCHER_VTBL).cast()),
        window: win.cast(),
    }))
    .cast();
    // SAFETY: `win` vient d'être alloué.
    unsafe {
        (*win).dispatcher = disp;
    }
    win.cast()
}

// ==================== SystemNavigationManager ====================

#[repr(C)]
struct SysNavStaticsVtbl {
    slots: [VtblSlot; 7],
}

#[repr(C)]
struct SysNavStaticsObject {
    header: ComHeader,
}

#[repr(C)]
struct SysNavInstanceVtbl {
    slots: [VtblSlot; 10],
}

#[repr(C)]
struct SysNavInstanceObject {
    header: ComHeader,
}

extern "win64" fn sys_nav_statics_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "SystemNavigationManagerStatics",
        this,
        riid,
        ppv,
        &[IID_ISYSTEM_NAV_STATICS],
    )
}

extern "win64" fn sys_nav_get_for_view(
    _this: *mut SysNavStaticsObject,
    out_mgr: *mut *mut c_void,
) -> HResult {
    if out_mgr.is_null() {
        return E_POINTER;
    }
    let inst = Box::into_raw(Box::new(SysNavInstanceObject {
        header: ComHeader::new((&raw const SYS_NAV_INSTANCE_VTBL).cast()),
    }))
    .cast();
    // SAFETY: `out_mgr` non-nul.
    unsafe {
        *out_mgr = inst;
    }
    S_OK
}

extern "win64" fn sys_nav_inst_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("SystemNavigationManager", this, riid, ppv, &[])
}

static SYS_NAV_STATICS_VTBL: SysNavStaticsVtbl = SysNavStaticsVtbl {
    slots: [
        vslot(sys_nav_statics_qi as *const c_void),
        vslot(com_add_ref as *const c_void),
        vslot(com_release as *const c_void),
        vslot(com_get_iids as *const c_void),
        vslot(com_get_runtime_class_name as *const c_void),
        vslot(com_get_trust_level as *const c_void),
        vslot(sys_nav_get_for_view as *const c_void), // 6: GetForCurrentView
    ],
};

static SYS_NAV_INSTANCE_VTBL: SysNavInstanceVtbl = SysNavInstanceVtbl {
    slots: [
        vslot(sys_nav_inst_qi as *const c_void),
        vslot(com_add_ref as *const c_void),
        vslot(com_release as *const c_void),
        vslot(com_get_iids as *const c_void),
        vslot(com_get_runtime_class_name as *const c_void),
        vslot(com_get_trust_level as *const c_void),
        vslot(cw_add_event_noop as *const c_void), // 6: add_BackRequested
        vslot(core_app_noop as *const c_void),     // 7: remove_BackRequested
        vslot(core_app_noop as *const c_void),     // 8: get_AppViewBackButtonVisibility
        vslot(core_app_noop as *const c_void),     // 9: put_AppViewBackButtonVisibility
    ],
};

// ==================== ApplicationData & StorageFolder (VFS `sb-vfs`) ====================

#[repr(C)]
struct StorageFolderVtbl {
    slots: [VtblSlot; 16],
}

#[repr(C)]
struct StorageFolderObject {
    header: ComHeader,
    path: String,
}

extern "win64" fn folder_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    // `IStorageFolder` et `IStorageItem` partagent cet objet (slot 12 = `IStorageItem::get_Path`).
    if ppv.is_null() || this.is_null() {
        return E_POINTER;
    }
    // SAFETY: accepte `IStorageItem` et `IStorageFolder` sur cet objet.
    unsafe {
        (*this).add_ref();
        *ppv = this.cast();
    }
    S_OK
}

extern "win64" fn folder_get_name(this: *mut StorageFolderObject, out_hstr: *mut u64) -> HResult {
    if !this.is_null() && !out_hstr.is_null() {
        // SAFETY: `this` et `out_hstr` non-nuls.
        unsafe {
            *out_hstr = create_hstring("LocalState");
        }
    }
    S_OK
}

extern "win64" fn folder_get_path(this: *mut StorageFolderObject, out_hstr: *mut u64) -> HResult {
    if !this.is_null() && !out_hstr.is_null() {
        // SAFETY: `this` et `out_hstr` non-nuls.
        let p = unsafe { &(*this).path };
        sb_debug!(CHANNEL, "StorageFolder::get_Path() -> '{p}'");
        unsafe {
            *out_hstr = create_hstring(p);
        }
    }
    S_OK
}

static STORAGE_FOLDER_VTBL: StorageFolderVtbl = StorageFolderVtbl {
    slots: [
        vslot(folder_qi as *const c_void),
        vslot(com_add_ref as *const c_void),
        vslot(com_release as *const c_void),
        vslot(com_get_iids as *const c_void),
        vslot(com_get_runtime_class_name as *const c_void),
        vslot(com_get_trust_level as *const c_void),
        vslot(core_app_noop as *const c_void),
        vslot(core_app_noop as *const c_void),
        vslot(core_app_noop as *const c_void),
        vslot(core_app_noop as *const c_void),
        vslot(core_app_noop as *const c_void),
        vslot(folder_get_name as *const c_void), // 11: IStorageItem::get_Name
        vslot(folder_get_path as *const c_void), // 12: IStorageItem::get_Path
        vslot(core_app_noop as *const c_void),
        vslot(core_app_noop as *const c_void),
        vslot(core_app_noop as *const c_void),
    ],
};

#[repr(C)]
struct AppDataStaticsVtbl {
    slots: [VtblSlot; 7],
}

#[repr(C)]
struct AppDataStaticsObject {
    header: ComHeader,
}

#[repr(C)]
struct AppDataInstanceVtbl {
    slots: [VtblSlot; 19],
}

#[repr(C)]
struct AppDataInstanceObject {
    header: ComHeader,
}

extern "win64" fn app_data_statics_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "ApplicationDataStatics",
        this,
        riid,
        ppv,
        &[IID_IAPPLICATION_DATA_STATICS],
    )
}

extern "win64" fn app_data_get_current(
    _this: *mut AppDataStaticsObject,
    out_data: *mut *mut c_void,
) -> HResult {
    if out_data.is_null() {
        return E_POINTER;
    }
    let inst = Box::into_raw(Box::new(AppDataInstanceObject {
        header: ComHeader::new((&raw const APP_DATA_INSTANCE_VTBL).cast()),
    }))
    .cast();
    // SAFETY: `out_data` non-nul.
    unsafe {
        *out_data = inst;
    }
    S_OK
}

extern "win64" fn app_data_inst_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    if ppv.is_null() || this.is_null() {
        return E_POINTER;
    }
    // SAFETY: `this` et `ppv` non-nuls.
    unsafe {
        (*this).add_ref();
        *ppv = this.cast();
    }
    S_OK
}

fn make_vfs_folder(mount: VfsMount) -> *mut c_void {
    // Expose uniquement des lecteurs virtuels sandboxés (jamais de chemin hôte `/home/...`).
    let path = match mount {
        VfsMount::Save => "S:".to_string(),
        VfsMount::Temp => "T:".to_string(),
        VfsMount::Game => "G:".to_string(),
    };
    Box::into_raw(Box::new(StorageFolderObject {
        header: ComHeader::new((&raw const STORAGE_FOLDER_VTBL).cast()),
        path,
    }))
    .cast()
}

extern "win64" fn app_data_get_local_folder(
    _this: *mut AppDataInstanceObject,
    out_folder: *mut *mut c_void,
) -> HResult {
    if out_folder.is_null() {
        return E_POINTER;
    }
    // SAFETY: `out_folder` non-nul.
    unsafe {
        *out_folder = make_vfs_folder(VfsMount::Save);
    }
    S_OK
}

extern "win64" fn app_data_get_temp_folder(
    _this: *mut AppDataInstanceObject,
    out_folder: *mut *mut c_void,
) -> HResult {
    if out_folder.is_null() {
        return E_POINTER;
    }
    // SAFETY: `out_folder` non-nul.
    unsafe {
        *out_folder = make_vfs_folder(VfsMount::Temp);
    }
    S_OK
}

static APP_DATA_STATICS_VTBL: AppDataStaticsVtbl = AppDataStaticsVtbl {
    slots: [
        vslot(app_data_statics_qi as *const c_void),
        vslot(com_add_ref as *const c_void),
        vslot(com_release as *const c_void),
        vslot(com_get_iids as *const c_void),
        vslot(com_get_runtime_class_name as *const c_void),
        vslot(com_get_trust_level as *const c_void),
        vslot(app_data_get_current as *const c_void), // 6: get_Current
    ],
};

const fn build_app_data_inst_vtbl() -> AppDataInstanceVtbl {
    let mut slots = [vslot(core_app_noop as *const c_void); 19];
    slots[0] = vslot(app_data_inst_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[3] = vslot(com_get_iids as *const c_void);
    slots[4] = vslot(com_get_runtime_class_name as *const c_void);
    slots[5] = vslot(com_get_trust_level as *const c_void);
    slots[12] = vslot(app_data_get_local_folder as *const c_void);
    slots[13] = vslot(app_data_get_local_folder as *const c_void);
    slots[14] = vslot(app_data_get_temp_folder as *const c_void);
    AppDataInstanceVtbl { slots }
}

static APP_DATA_INSTANCE_VTBL: AppDataInstanceVtbl = build_app_data_inst_vtbl();

// ==================== GlobalizationPreferences & IVectorView<HSTRING> ====================

#[repr(C)]
struct StringVectorViewVtbl {
    slots: [VtblSlot; 10],
}

#[repr(C)]
struct StringVectorViewObject {
    header: ComHeader,
    items: Vec<String>,
}

extern "win64" fn vec_view_qi(
    this: *mut ComHeader,
    _riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    if ppv.is_null() || this.is_null() {
        return E_POINTER;
    }
    // SAFETY: `this` et `ppv` non-nuls.
    unsafe {
        (*this).add_ref();
        *ppv = this.cast();
    }
    S_OK
}

extern "win64" fn str_vec_get_at(
    this: *mut StringVectorViewObject,
    index: u32,
    out_hstr: *mut u64,
) -> HResult {
    if this.is_null() || out_hstr.is_null() {
        return E_POINTER;
    }
    // SAFETY: `this` et `out_hstr` non-nuls.
    let obj = unsafe { &*this };
    let item = obj
        .items
        .get(index as usize)
        .map(String::as_str)
        .unwrap_or("en-US");
    unsafe {
        *out_hstr = create_hstring(item);
    }
    S_OK
}

extern "win64" fn str_vec_get_size(
    this: *mut StringVectorViewObject,
    out_size: *mut u32,
) -> HResult {
    if !this.is_null() && !out_size.is_null() {
        // SAFETY: `this` et `out_size` non-nuls.
        unsafe {
            *out_size = (*this).items.len() as u32;
        }
    }
    S_OK
}

static STRING_VECTOR_VIEW_VTBL: StringVectorViewVtbl = StringVectorViewVtbl {
    slots: [
        vslot(vec_view_qi as *const c_void),
        vslot(com_add_ref as *const c_void),
        vslot(com_release as *const c_void),
        vslot(com_get_iids as *const c_void),
        vslot(com_get_runtime_class_name as *const c_void),
        vslot(com_get_trust_level as *const c_void),
        vslot(str_vec_get_at as *const c_void),   // 6: GetAt
        vslot(str_vec_get_size as *const c_void), // 7: get_Size
        vslot(core_app_noop as *const c_void),    // 8: IndexOf
        vslot(core_app_noop as *const c_void),    // 9: GetMany
    ],
};

#[repr(C)]
struct GlobPrefsStaticsVtbl {
    slots: [VtblSlot; 12],
}

#[repr(C)]
struct GlobPrefsStaticsObject {
    header: ComHeader,
}

extern "win64" fn glob_prefs_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "GlobalizationPreferences",
        this,
        riid,
        ppv,
        &[IID_IGLOBALIZATION_PREFS_STATICS],
    )
}

extern "win64" fn glob_prefs_get_languages(
    _this: *mut GlobPrefsStaticsObject,
    out_vec: *mut *mut c_void,
) -> HResult {
    if out_vec.is_null() {
        return E_POINTER;
    }
    let vec = Box::into_raw(Box::new(StringVectorViewObject {
        header: ComHeader::new((&raw const STRING_VECTOR_VIEW_VTBL).cast()),
        items: vec!["en-US".to_string(), "fr-FR".to_string()],
    }))
    .cast();
    // SAFETY: `out_vec` non-nul.
    unsafe {
        *out_vec = vec;
    }
    S_OK
}

static GLOB_PREFS_STATICS_VTBL: GlobPrefsStaticsVtbl = GlobPrefsStaticsVtbl {
    slots: [
        vslot(glob_prefs_qi as *const c_void),
        vslot(com_add_ref as *const c_void),
        vslot(com_release as *const c_void),
        vslot(com_get_iids as *const c_void),
        vslot(com_get_runtime_class_name as *const c_void),
        vslot(com_get_trust_level as *const c_void),
        vslot(glob_prefs_get_languages as *const c_void), // 6: Calendars
        vslot(glob_prefs_get_languages as *const c_void), // 7: Clocks
        vslot(glob_prefs_get_languages as *const c_void), // 8: Currencies
        vslot(glob_prefs_get_languages as *const c_void), // 9: Languages
        vslot(core_app_get_id as *const c_void),          // 10: HomeGeographicRegion
        vslot(core_app_noop as *const c_void),            // 11: WeekStartsOn
    ],
};

// ==================== Windows.Gaming.Input.Gamepad (SDL3) ====================

#[repr(C)]
struct GamepadReading {
    timestamp: u64,
    buttons: u32,
    _pad: u32,
    left_trigger: f64,
    right_trigger: f64,
    left_thumbstick_x: f64,
    left_thumbstick_y: f64,
    right_thumbstick_x: f64,
    right_thumbstick_y: f64,
}

#[repr(C)]
struct GamepadVibration {
    left_motor: f64,
    right_motor: f64,
    left_trigger: f64,
    right_trigger: f64,
}

#[repr(C)]
struct GamepadVtbl {
    slots: [VtblSlot; 10],
}

#[repr(C)]
struct GamepadObject {
    header: ComHeader,
}

extern "win64" fn gamepad_put_vibration(
    _this: *mut GamepadObject,
    vib: GamepadVibration,
) -> HResult {
    if let Some(sdl) = Sdl3Lib::load() {
        let low = (vib.left_motor.clamp(0.0, 1.0) * 65535.0) as u16;
        let high = (vib.right_motor.clamp(0.0, 1.0) * 65535.0) as u16;
        sdl.rumble_primary_gamepad(low, high, 100);
    }
    S_OK
}

extern "win64" fn gamepad_get_reading(
    _this: *mut GamepadObject,
    out_reading: *mut GamepadReading,
) -> HResult {
    if out_reading.is_null() {
        return E_POINTER;
    }
    let snap = Sdl3Lib::load()
        .map(Sdl3Lib::poll_primary_gamepad)
        .unwrap_or_default();
    // SAFETY: `out_reading` non-nul.
    unsafe {
        *out_reading = GamepadReading {
            timestamp: DISPATCH_TICKS.load(Ordering::Relaxed),
            buttons: snap.buttons_mask,
            _pad: 0,
            left_trigger: snap.left_trigger,
            right_trigger: snap.right_trigger,
            left_thumbstick_x: snap.left_thumb_x,
            left_thumbstick_y: snap.left_thumb_y,
            right_thumbstick_x: snap.right_thumb_x,
            right_thumbstick_y: snap.right_thumb_y,
        };
    }
    S_OK
}

static GAMEPAD_VTBL: GamepadVtbl = GamepadVtbl {
    slots: [
        vslot(vec_view_qi as *const c_void),
        vslot(com_add_ref as *const c_void),
        vslot(com_release as *const c_void),
        vslot(com_get_iids as *const c_void),
        vslot(com_get_runtime_class_name as *const c_void),
        vslot(com_get_trust_level as *const c_void),
        vslot(core_app_noop as *const c_void), // 6: get_Vibration
        vslot(gamepad_put_vibration as *const c_void), // 7: put_Vibration
        vslot(gamepad_get_reading as *const c_void), // 8: GetCurrentReading
        vslot(core_app_noop as *const c_void),
    ],
};

#[repr(C)]
struct GamepadVecViewVtbl {
    slots: [VtblSlot; 10],
}

#[repr(C)]
struct GamepadVecViewObject {
    header: ComHeader,
}

extern "win64" fn gp_vec_get_at(
    _this: *mut GamepadVecViewObject,
    _index: u32,
    out_gp: *mut *mut c_void,
) -> HResult {
    if out_gp.is_null() {
        return E_POINTER;
    }
    let gp = Box::into_raw(Box::new(GamepadObject {
        header: ComHeader::new((&raw const GAMEPAD_VTBL).cast()),
    }))
    .cast();
    // SAFETY: `out_gp` non-nul.
    unsafe {
        *out_gp = gp;
    }
    S_OK
}

extern "win64" fn gp_vec_get_size(_this: *mut GamepadVecViewObject, out_size: *mut u32) -> HResult {
    if !out_size.is_null() {
        let count = Sdl3Lib::load()
            .map(|s| u32::from(s.poll_primary_gamepad().connected))
            .unwrap_or(0);
        // SAFETY: `out_size` non-nul.
        unsafe {
            *out_size = count;
        }
    }
    S_OK
}

static GAMEPAD_VEC_VTBL: GamepadVecViewVtbl = GamepadVecViewVtbl {
    slots: [
        vslot(vec_view_qi as *const c_void),
        vslot(com_add_ref as *const c_void),
        vslot(com_release as *const c_void),
        vslot(com_get_iids as *const c_void),
        vslot(com_get_runtime_class_name as *const c_void),
        vslot(com_get_trust_level as *const c_void),
        vslot(gp_vec_get_at as *const c_void),
        vslot(gp_vec_get_size as *const c_void),
        vslot(core_app_noop as *const c_void),
        vslot(core_app_noop as *const c_void),
    ],
};

#[repr(C)]
struct GamepadStaticsVtbl {
    slots: [VtblSlot; 11],
}

#[repr(C)]
struct GamepadStaticsObject {
    header: ComHeader,
}

extern "win64" fn gp_statics_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("GamepadStatics", this, riid, ppv, &[IID_IGAMEPAD_STATICS])
}

extern "win64" fn gp_get_gamepads(
    _this: *mut GamepadStaticsObject,
    out_vec: *mut *mut c_void,
) -> HResult {
    if out_vec.is_null() {
        return E_POINTER;
    }
    let vec = Box::into_raw(Box::new(GamepadVecViewObject {
        header: ComHeader::new((&raw const GAMEPAD_VEC_VTBL).cast()),
    }))
    .cast();
    // SAFETY: `out_vec` non-nul.
    unsafe {
        *out_vec = vec;
    }
    S_OK
}

static GAMEPAD_STATICS_VTBL: GamepadStaticsVtbl = GamepadStaticsVtbl {
    slots: [
        vslot(gp_statics_qi as *const c_void),
        vslot(com_add_ref as *const c_void),
        vslot(com_release as *const c_void),
        vslot(com_get_iids as *const c_void),
        vslot(com_get_runtime_class_name as *const c_void),
        vslot(com_get_trust_level as *const c_void),
        vslot(cw_add_event_noop as *const c_void), // 6: add_GamepadAdded
        vslot(core_app_noop as *const c_void),     // 7: remove_GamepadAdded
        vslot(cw_add_event_noop as *const c_void), // 8: add_GamepadRemoved
        vslot(core_app_noop as *const c_void),     // 9: remove_GamepadRemoved
        vslot(gp_get_gamepads as *const c_void),   // 10: get_Gamepads
    ],
};
