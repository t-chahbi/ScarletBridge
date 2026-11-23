//! `user32` / `ext-ms-win-rtcore-ntuser-*` : classes et fenêtres **logiques** (HWND opaques, pas
//! de surface native : la présentation passe par la `CoreWindow` SDL3 de `winrt_objects`),
//! procédures de fenêtre et files de messages par thread (`PeekMessage`, `GetMessage`,
//! `DispatchMessage`, `PostMessage`, `PostQuitMessage`, ...).

use core::ffi::c_void;
use core::mem::{offset_of, size_of};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};
use sb_common::abi::{WIN_FALSE, WIN_TRUE, WinBool};
use sb_common::ffi_guard;
use sb_common::wide::from_pcwstr;
use sb_loader::set_last_error;

use super::{CHANNEL, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ansi_to_string, write_out};

const ERROR_INVALID_WINDOW_HANDLE: u32 = 1400;
const ERROR_CANNOT_FIND_WND_CLASS: u32 = 1407;
const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;
const ERROR_CLASS_DOES_NOT_EXIST: u32 = 1411;
const ERROR_INVALID_INDEX: u32 = 1413;

pub(crate) const WM_CREATE: u32 = 0x0001;
pub(crate) const WM_DESTROY: u32 = 0x0002;
pub(crate) const WM_SETTEXT: u32 = 0x000C;
pub(crate) const WM_GETTEXT: u32 = 0x000D;
pub(crate) const WM_GETTEXTLENGTH: u32 = 0x000E;
pub(crate) const WM_CLOSE: u32 = 0x0010;
pub(crate) const WM_QUIT: u32 = 0x0012;
pub(crate) const WM_SHOWWINDOW: u32 = 0x0018;
pub(crate) const WM_NCCREATE: u32 = 0x0081;
pub(crate) const WM_NCDESTROY: u32 = 0x0082;
pub(crate) const WM_KEYDOWN: u32 = 0x0100;
pub(crate) const WM_KEYUP: u32 = 0x0101;
pub(crate) const WM_CHAR: u32 = 0x0102;
pub(crate) const WM_SYSKEYDOWN: u32 = 0x0104;
pub(crate) const WM_SYSKEYUP: u32 = 0x0105;
pub(crate) const WM_SYSCHAR: u32 = 0x0106;
pub(crate) const WM_TIMER: u32 = 0x0113;
pub(crate) const WM_SYSCOMMAND: u32 = 0x0112;
const SC_CLOSE: usize = 0xF060;
const PM_REMOVE: u32 = 0x0001;
const WS_VISIBLE: u32 = 0x1000_0000;
const CW_USEDEFAULT: i32 = 0x8000_0000_u32 as i32;
const HWND_BROADCAST: usize = 0xFFFF;
const GWLP_WNDPROC: i32 = -4;
const GWLP_HINSTANCE: i32 = -6;
const GWLP_HWNDPARENT: i32 = -8;
const GWLP_ID: i32 = -12;
const GWL_STYLE: i32 = -16;
const GWL_EXSTYLE: i32 = -20;
const GWLP_USERDATA: i32 = -21;

/// `WNDCLASSEXW` / `WNDCLASSEXA` (80 octets).
#[repr(C)]
pub(crate) struct WndClassEx {
    pub cb_size: u32,
    pub style: u32,
    pub wnd_proc: usize,
    pub cls_extra: i32,
    pub wnd_extra: i32,
    pub instance: usize,
    pub icon: usize,
    pub cursor: usize,
    pub background: usize,
    pub menu_name: usize,
    pub class_name: usize,
    pub icon_sm: usize,
}

/// `WNDCLASSW` / `WNDCLASSA` (72 octets).
#[repr(C)]
pub(crate) struct WndClass {
    pub style: u32,
    pub wnd_proc: usize,
    pub cls_extra: i32,
    pub wnd_extra: i32,
    pub instance: usize,
    pub icon: usize,
    pub cursor: usize,
    pub background: usize,
    pub menu_name: usize,
    pub class_name: usize,
}

/// `MSG` (48 octets).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Msg {
    pub hwnd: usize,
    pub message: u32,
    pub wparam: usize,
    pub lparam: isize,
    pub time: u32,
    pub pt: [i32; 2],
    pub private: u32,
}

/// `CREATESTRUCTW` / `CREATESTRUCTA` (80 octets).
#[repr(C)]
pub(crate) struct CreateStruct {
    pub create_params: usize,
    pub instance: usize,
    pub menu: usize,
    pub parent: usize,
    pub cy: i32,
    pub cx: i32,
    pub y: i32,
    pub x: i32,
    pub style: i32,
    pub name: usize,
    pub class: usize,
    pub ex_style: u32,
}

const _: () = {
    assert!(size_of::<WndClassEx>() == 80);
    assert!(offset_of!(WndClassEx, class_name) == 64);
    assert!(size_of::<WndClass>() == 72);
    assert!(offset_of!(WndClass, class_name) == 64);
    assert!(size_of::<Msg>() == 48);
    assert!(offset_of!(Msg, time) == 32);
    assert!(offset_of!(Msg, pt) == 36);
    assert!(size_of::<CreateStruct>() == 80);
    assert!(offset_of!(CreateStruct, cy) == 32);
    assert!(offset_of!(CreateStruct, name) == 56);
    assert!(offset_of!(CreateStruct, ex_style) == 72);
};

#[derive(Clone)]
struct ClassInfo {
    name: String,
    atom: u16,
    wnd_proc: usize,
    wnd_extra: usize,
    instance: usize,
}

#[derive(Clone)]
struct WindowInfo {
    atom: u16,
    wnd_proc: usize,
    tid: u32,
    style: u32,
    ex_style: u32,
    parent: usize,
    id: usize,
    instance: usize,
    text: String,
    size: (i32, i32),
    visible: bool,
    user_data: usize,
    extra: Vec<u8>,
    unicode: bool,
}

struct Queue {
    msgs: Mutex<VecDeque<Msg>>,
    quit: Mutex<Option<i32>>,
    cv: Condvar,
}

static CLASSES: LazyLock<Mutex<Vec<ClassInfo>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static WINDOWS: LazyLock<Mutex<HashMap<usize, WindowInfo>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static QUEUES: LazyLock<Mutex<HashMap<u32, Arc<Queue>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_ATOM: AtomicU16 = AtomicU16::new(0xC000);
static NEXT_HWND: AtomicUsize = AtomicUsize::new(0x0001_0010);

fn current_tid() -> u32 {
    // SAFETY: syscall `gettid` sans précondition.
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

fn queue_for(tid: u32) -> Arc<Queue> {
    Arc::clone(QUEUES.lock().entry(tid).or_insert_with(|| {
        Arc::new(Queue {
            msgs: Mutex::new(VecDeque::new()),
            quit: Mutex::new(None),
            cv: Condvar::new(),
        })
    }))
}

fn window(hwnd: usize) -> Option<WindowInfo> {
    WINDOWS.lock().get(&hwnd).cloned()
}

fn call_wndproc(proc_: usize, hwnd: usize, msg: u32, w: usize, l: isize) -> isize {
    if proc_ == 0 {
        return 0;
    }
    // SAFETY: `proc_` est une `WNDPROC` Win64 enregistrée par l'invité (ou une fn HLE).
    let f: unsafe extern "win64" fn(usize, u32, usize, isize) -> isize =
        unsafe { core::mem::transmute(proc_) };
    // SAFETY: appel de la procédure de fenêtre ; aucun verrou n'est tenu ici.
    unsafe { f(hwnd, msg, w, l) }
}

/// Envoi synchrone (`SendMessage`) : appel direct de la procédure, quel que soit le thread.
fn send(hwnd: usize, msg: u32, w: usize, l: isize) -> isize {
    match window(hwnd) {
        Some(win) => call_wndproc(win.wnd_proc, hwnd, msg, w, l),
        None => {
            set_last_error(ERROR_INVALID_WINDOW_HANDLE);
            0
        }
    }
}

fn read_name(p: usize, unicode: bool) -> Option<String> {
    if p == 0 {
        None
    } else if unicode {
        // SAFETY: `PCWSTR` fourni par l'invité.
        unsafe { from_pcwstr(p as *const u16) }
    } else {
        ansi_to_string(p as *const i8)
    }
}

/// Clé de classe : nom (insensible à la casse) ou atome `MAKEINTATOM`.
enum ClassKey {
    Atom(u16),
    Name(String),
}

fn class_key(p: usize, unicode: bool) -> Option<ClassKey> {
    if p == 0 {
        None
    } else if p <= 0xFFFF {
        Some(ClassKey::Atom(p as u16))
    } else {
        read_name(p, unicode).map(|n| ClassKey::Name(n.to_lowercase()))
    }
}

fn find_class(key: &ClassKey) -> Option<ClassInfo> {
    CLASSES
        .lock()
        .iter()
        .find(|c| match key {
            ClassKey::Atom(a) => c.atom == *a,
            ClassKey::Name(n) => c.name == *n,
        })
        .cloned()
}

fn register_class(
    wnd_proc: usize,
    wnd_extra: i32,
    instance: usize,
    class_name: usize,
    unicode: bool,
) -> u16 {
    let Some(ClassKey::Name(name)) = class_key(class_name, unicode) else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    if wnd_extra < 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let mut classes = CLASSES.lock();
    if classes.iter().any(|c| c.name == name) {
        set_last_error(ERROR_CLASS_ALREADY_EXISTS);
        return 0;
    }
    let atom = NEXT_ATOM.fetch_add(1, Ordering::AcqRel);
    classes.push(ClassInfo {
        name,
        atom,
        wnd_proc,
        wnd_extra: wnd_extra as usize,
        instance,
    });
    atom
}

fn register_class_ex(wc: *const WndClassEx, unicode: bool) -> u16 {
    if wc.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: `wc` non-nul pointe vers un `WNDCLASSEX` de l'invité.
    let wc = unsafe { &*wc };
    if wc.cb_size as usize != size_of::<WndClassEx>() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    register_class(
        wc.wnd_proc,
        wc.wnd_extra,
        wc.instance,
        wc.class_name,
        unicode,
    )
}

pub(super) extern "win64" fn RegisterClassExW(wc: *const WndClassEx) -> u16 {
    ffi_guard!(CHANNEL, "RegisterClassExW", 0, {
        register_class_ex(wc, true)
    })
}

pub(super) extern "win64" fn RegisterClassExA(wc: *const WndClassEx) -> u16 {
    ffi_guard!(CHANNEL, "RegisterClassExA", 0, {
        register_class_ex(wc, false)
    })
}

fn register_class_plain(wc: *const WndClass, unicode: bool) -> u16 {
    if wc.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: `wc` non-nul pointe vers un `WNDCLASS` de l'invité.
    let wc = unsafe { &*wc };
    register_class(
        wc.wnd_proc,
        wc.wnd_extra,
        wc.instance,
        wc.class_name,
        unicode,
    )
}

pub(super) extern "win64" fn RegisterClassW(wc: *const WndClass) -> u16 {
    ffi_guard!(CHANNEL, "RegisterClassW", 0, {
        register_class_plain(wc, true)
    })
}

pub(super) extern "win64" fn RegisterClassA(wc: *const WndClass) -> u16 {
    ffi_guard!(CHANNEL, "RegisterClassA", 0, {
        register_class_plain(wc, false)
    })
}

pub(super) extern "win64" fn UnregisterClassW(name: *const u16, instance: usize) -> WinBool {
    ffi_guard!(CHANNEL, "UnregisterClassW", WIN_FALSE, {
        let Some(key) = class_key(name as usize, true) else {
            set_last_error(ERROR_CLASS_DOES_NOT_EXIST);
            return WIN_FALSE;
        };
        let Some(cls) = find_class(&key) else {
            set_last_error(ERROR_CLASS_DOES_NOT_EXIST);
            return WIN_FALSE;
        };
        if WINDOWS.lock().values().any(|w| w.atom == cls.atom) {
            set_last_error(1412); // ERROR_CLASS_HAS_WINDOWS
            return WIN_FALSE;
        }
        CLASSES.lock().retain(|c| c.atom != cls.atom);
        WIN_TRUE
    })
}

#[allow(clippy::too_many_arguments)]
fn create_window(
    ex_style: u32,
    class: usize,
    title: usize,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    parent: usize,
    menu: usize,
    instance: usize,
    param: usize,
    unicode: bool,
) -> usize {
    let Some(cls) = class_key(class, unicode).and_then(|k| find_class(&k)) else {
        set_last_error(ERROR_CANNOT_FIND_WND_CLASS);
        return 0;
    };
    if parent != 0 && parent != usize::MAX - 2 && window(parent).is_none() {
        set_last_error(ERROR_INVALID_WINDOW_HANDLE);
        return 0;
    }
    let (cx, cy) = if w == CW_USEDEFAULT {
        (1280, 720)
    } else {
        (w, h)
    };
    let (px, py) = if x == CW_USEDEFAULT { (0, 0) } else { (x, y) };
    let hwnd = NEXT_HWND.fetch_add(2, Ordering::AcqRel);
    WINDOWS.lock().insert(
        hwnd,
        WindowInfo {
            atom: cls.atom,
            wnd_proc: cls.wnd_proc,
            tid: current_tid(),
            style: style & !WS_VISIBLE,
            ex_style,
            parent,
            id: menu,
            instance: if instance != 0 {
                instance
            } else {
                cls.instance
            },
            text: String::new(),
            size: (cx, cy),
            visible: false,
            user_data: 0,
            extra: vec![0; cls.wnd_extra],
            unicode,
        },
    );
    let cs = CreateStruct {
        create_params: param,
        instance,
        menu,
        parent,
        cy,
        cx,
        y: py,
        x: px,
        style: style as i32,
        name: title,
        class,
        ex_style,
    };
    let lp = &raw const cs as isize;
    if send(hwnd, WM_NCCREATE, 0, lp) == 0 {
        destroy_window(hwnd);
        return 0;
    }
    if send(hwnd, WM_CREATE, 0, lp) == -1 {
        destroy_window(hwnd);
        return 0;
    }
    if (style & WS_VISIBLE) != 0 {
        show_window(hwnd, 1);
    }
    set_last_error(0);
    hwnd
}

#[allow(clippy::too_many_arguments)]
pub(super) extern "win64" fn CreateWindowExW(
    ex_style: u32,
    class: *const u16,
    title: *const u16,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    parent: usize,
    menu: usize,
    instance: usize,
    param: *mut c_void,
) -> usize {
    ffi_guard!(CHANNEL, "CreateWindowExW", 0, {
        create_window(
            ex_style,
            class as usize,
            title as usize,
            style,
            x,
            y,
            w,
            h,
            parent,
            menu,
            instance,
            param as usize,
            true,
        )
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) extern "win64" fn CreateWindowExA(
    ex_style: u32,
    class: *const i8,
    title: *const i8,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    parent: usize,
    menu: usize,
    instance: usize,
    param: *mut c_void,
) -> usize {
    ffi_guard!(CHANNEL, "CreateWindowExA", 0, {
        create_window(
            ex_style,
            class as usize,
            title as usize,
            style,
            x,
            y,
            w,
            h,
            parent,
            menu,
            instance,
            param as usize,
            false,
        )
    })
}

fn destroy_window(hwnd: usize) -> bool {
    let Some(win) = window(hwnd) else {
        set_last_error(ERROR_INVALID_WINDOW_HANDLE);
        return false;
    };
    if win.tid != current_tid() {
        set_last_error(ERROR_ACCESS_DENIED);
        return false;
    }
    if win.visible {
        call_wndproc(win.wnd_proc, hwnd, WM_SHOWWINDOW, 0, 0);
    }
    call_wndproc(win.wnd_proc, hwnd, WM_DESTROY, 0, 0);
    let children: Vec<usize> = WINDOWS
        .lock()
        .iter()
        .filter(|(_, w)| w.parent == hwnd)
        .map(|(h, _)| *h)
        .collect();
    for c in children {
        destroy_window(c);
    }
    call_wndproc(win.wnd_proc, hwnd, WM_NCDESTROY, 0, 0);
    WINDOWS.lock().remove(&hwnd);
    queue_for(win.tid).msgs.lock().retain(|m| m.hwnd != hwnd);
    true
}

pub(super) extern "win64" fn DestroyWindow(hwnd: usize) -> WinBool {
    ffi_guard!(CHANNEL, "DestroyWindow", WIN_FALSE, {
        WinBool::from(destroy_window(hwnd))
    })
}

pub(super) extern "win64" fn IsWindow(hwnd: usize) -> WinBool {
    WinBool::from(WINDOWS.lock().contains_key(&hwnd))
}

fn show_window(hwnd: usize, cmd: i32) -> WinBool {
    let Some(win) = window(hwnd) else {
        set_last_error(ERROR_INVALID_WINDOW_HANDLE);
        return WIN_FALSE;
    };
    let visible = cmd != 0; // SW_HIDE = 0
    if win.visible != visible {
        if let Some(w) = WINDOWS.lock().get_mut(&hwnd) {
            w.visible = visible;
        }
        call_wndproc(win.wnd_proc, hwnd, WM_SHOWWINDOW, usize::from(visible), 0);
    }
    WinBool::from(win.visible)
}

pub(super) extern "win64" fn ShowWindow(hwnd: usize, cmd: i32) -> WinBool {
    ffi_guard!(CHANNEL, "ShowWindow", WIN_FALSE, { show_window(hwnd, cmd) })
}

pub(super) extern "win64" fn UpdateWindow(hwnd: usize) -> WinBool {
    WinBool::from(WINDOWS.lock().contains_key(&hwnd))
}

fn def_window_proc(hwnd: usize, msg: u32, w: usize, l: isize) -> isize {
    match msg {
        WM_NCCREATE => {
            if l != 0 {
                // SAFETY: `lParam` de WM_NCCREATE est un `CREATESTRUCT*`.
                let name = unsafe { (*(l as *const CreateStruct)).name };
                let unicode = window(hwnd).is_none_or(|w| w.unicode);
                if let Some(t) = read_name(name, unicode)
                    && let Some(win) = WINDOWS.lock().get_mut(&hwnd)
                {
                    win.text = t;
                }
            }
            1
        }
        WM_SETTEXT => {
            let unicode = window(hwnd).is_none_or(|w| w.unicode);
            let t = read_name(l as usize, unicode).unwrap_or_default();
            if let Some(win) = WINDOWS.lock().get_mut(&hwnd) {
                win.text = t;
            }
            1
        }
        WM_GETTEXTLENGTH => window(hwnd).map_or(0, |w| w.text.encode_utf16().count() as isize),
        WM_GETTEXT => {
            let Some(win) = window(hwnd) else { return 0 };
            if w == 0 || l == 0 {
                return 0;
            }
            let units: Vec<u16> = win.text.encode_utf16().collect();
            let n = units.len().min(w - 1);
            // SAFETY: tampon de `wParam` caractères fourni par l'invité.
            unsafe {
                core::ptr::copy_nonoverlapping(units.as_ptr(), l as *mut u16, n);
                *(l as *mut u16).add(n) = 0;
            }
            n as isize
        }
        WM_CLOSE => {
            destroy_window(hwnd);
            0
        }
        WM_SYSCOMMAND if (w & 0xFFF0) == SC_CLOSE => {
            send(hwnd, WM_CLOSE, 0, 0);
            0
        }
        _ => 0,
    }
}

pub(super) extern "win64" fn DefWindowProcW(hwnd: usize, msg: u32, w: usize, l: isize) -> isize {
    ffi_guard!(CHANNEL, "DefWindowProcW", 0, {
        def_window_proc(hwnd, msg, w, l)
    })
}

pub(super) extern "win64" fn DefWindowProcA(hwnd: usize, msg: u32, w: usize, l: isize) -> isize {
    ffi_guard!(CHANNEL, "DefWindowProcA", 0, {
        def_window_proc(hwnd, msg, w, l)
    })
}

pub(super) extern "win64" fn SendMessageW(hwnd: usize, msg: u32, w: usize, l: isize) -> isize {
    ffi_guard!(CHANNEL, "SendMessageW", 0, { send(hwnd, msg, w, l) })
}

fn post_to(tid: u32, hwnd: usize, msg: u32, w: usize, l: isize) {
    let q = queue_for(tid);
    q.msgs.lock().push_back(Msg {
        hwnd,
        message: msg,
        wparam: w,
        lparam: l,
        time: super::sysinfo::tick_count_ms() as u32,
        pt: [0, 0],
        private: 0,
    });
    q.cv.notify_all();
}

fn post_message(hwnd: usize, msg: u32, w: usize, l: isize) -> WinBool {
    if hwnd == 0 {
        post_to(current_tid(), 0, msg, w, l);
        return WIN_TRUE;
    }
    if hwnd == HWND_BROADCAST {
        let targets: Vec<(usize, u32)> = WINDOWS
            .lock()
            .iter()
            .filter(|(_, w)| w.parent == 0)
            .map(|(h, w)| (*h, w.tid))
            .collect();
        for (h, tid) in targets {
            post_to(tid, h, msg, w, l);
        }
        return WIN_TRUE;
    }
    match window(hwnd) {
        Some(win) => {
            post_to(win.tid, hwnd, msg, w, l);
            WIN_TRUE
        }
        None => {
            set_last_error(ERROR_INVALID_WINDOW_HANDLE);
            WIN_FALSE
        }
    }
}

pub(super) extern "win64" fn PostMessageW(hwnd: usize, msg: u32, w: usize, l: isize) -> WinBool {
    ffi_guard!(CHANNEL, "PostMessageW", WIN_FALSE, {
        post_message(hwnd, msg, w, l)
    })
}

pub(super) extern "win64" fn PostThreadMessageW(tid: u32, msg: u32, w: usize, l: isize) -> WinBool {
    ffi_guard!(CHANNEL, "PostThreadMessageW", WIN_FALSE, {
        if !QUEUES.lock().contains_key(&tid) && tid != current_tid() {
            set_last_error(ERROR_INVALID_PARAMETER); // ERROR_INVALID_THREAD_ID non distingué
            return WIN_FALSE;
        }
        post_to(tid, 0, msg, w, l);
        WIN_TRUE
    })
}

pub(super) extern "win64" fn PostQuitMessage(code: i32) {
    ffi_guard!(CHANNEL, "PostQuitMessage", (), {
        let q = queue_for(current_tid());
        *q.quit.lock() = Some(code);
        q.cv.notify_all();
    })
}

fn matches_filter(m: &Msg, hwnd: usize, min: u32, max: u32) -> bool {
    let hwnd_ok = match hwnd {
        0 => true,
        usize::MAX => m.hwnd == 0,
        h => m.hwnd == h,
    };
    let range_ok = (min == 0 && max == 0) || (m.message >= min && m.message <= max);
    hwnd_ok && range_ok
}

/// Retire (ou lit) le prochain message correspondant ; `WM_QUIT` après les messages postés.
fn take_message(hwnd: usize, min: u32, max: u32, remove: bool) -> Option<Msg> {
    let q = queue_for(current_tid());
    {
        let mut msgs = q.msgs.lock();
        if let Some(idx) = msgs.iter().position(|m| matches_filter(m, hwnd, min, max)) {
            return if remove {
                msgs.remove(idx)
            } else {
                msgs.get(idx).copied()
            };
        }
    }
    let mut quit = q.quit.lock();
    let code = (*quit)?;
    if remove {
        *quit = None;
    }
    Some(Msg {
        hwnd: 0,
        message: WM_QUIT,
        wparam: code as isize as usize,
        lparam: 0,
        time: super::sysinfo::tick_count_ms() as u32,
        pt: [0, 0],
        private: 0,
    })
}

fn peek_message(out: *mut Msg, hwnd: usize, min: u32, max: u32, flags: u32) -> WinBool {
    if out.is_null() {
        set_last_error(998); // ERROR_NOACCESS
        return WIN_FALSE;
    }
    if hwnd != 0 && hwnd != usize::MAX && window(hwnd).is_none() {
        set_last_error(ERROR_INVALID_WINDOW_HANDLE);
        return WIN_FALSE;
    }
    match take_message(hwnd, min, max, (flags & PM_REMOVE) != 0) {
        Some(m) => {
            write_out(out, m);
            WIN_TRUE
        }
        None => WIN_FALSE,
    }
}

pub(super) extern "win64" fn PeekMessageW(
    out: *mut Msg,
    hwnd: usize,
    min: u32,
    max: u32,
    flags: u32,
) -> WinBool {
    ffi_guard!(CHANNEL, "PeekMessageW", WIN_FALSE, {
        peek_message(out, hwnd, min, max, flags)
    })
}

fn get_message(out: *mut Msg, hwnd: usize, min: u32, max: u32) -> WinBool {
    if out.is_null() {
        set_last_error(998);
        return -1;
    }
    if hwnd != 0 && hwnd != usize::MAX && window(hwnd).is_none() {
        set_last_error(ERROR_INVALID_WINDOW_HANDLE);
        return -1;
    }
    let q = queue_for(current_tid());
    loop {
        if let Some(m) = take_message(hwnd, min, max, true) {
            write_out(out, m);
            return WinBool::from(m.message != WM_QUIT);
        }
        let mut g = q.msgs.lock();
        if !g.iter().any(|m| matches_filter(m, hwnd, min, max)) && q.quit.lock().is_none() {
            let _ = q.cv.wait_for(&mut g, Duration::from_millis(50));
        }
    }
}

pub(super) extern "win64" fn GetMessageW(
    out: *mut Msg,
    hwnd: usize,
    min: u32,
    max: u32,
) -> WinBool {
    ffi_guard!(CHANNEL, "GetMessageW", -1, {
        get_message(out, hwnd, min, max)
    })
}

pub(super) extern "win64" fn TranslateMessage(msg: *const Msg) -> WinBool {
    ffi_guard!(CHANNEL, "TranslateMessage", WIN_FALSE, {
        if msg.is_null() {
            return WIN_FALSE;
        }
        // SAFETY: `msg` non-nul.
        let m = unsafe { *msg };
        let char_msg = match m.message {
            WM_KEYDOWN => WM_CHAR,
            WM_SYSKEYDOWN => WM_SYSCHAR,
            WM_KEYUP | WM_SYSKEYUP => return WIN_TRUE,
            _ => return WIN_FALSE,
        };
        // Disposition US sans modificateurs : lettres en minuscules, chiffres et touches de contrôle.
        let vk = m.wparam as u32;
        let ch = match vk {
            0x41..=0x5A => Some(vk + 0x20),
            0x30..=0x39 | 0x20 | 0x0D | 0x08 | 0x09 | 0x1B => Some(vk),
            0x60..=0x69 => Some(vk - 0x30), // pavé numérique
            _ => None,
        };
        if let Some(c) = ch {
            post_message(m.hwnd, char_msg, c as usize, m.lparam);
        }
        WIN_TRUE
    })
}

fn dispatch(msg: *const Msg) -> isize {
    if msg.is_null() {
        return 0;
    }
    // SAFETY: `msg` non-nul.
    let m = unsafe { *msg };
    if m.message == WM_TIMER && m.lparam != 0 {
        // TIMERPROC(hwnd, WM_TIMER, idEvent, time)
        // SAFETY: `lParam` est une `TIMERPROC` Win64 fournie par l'invité.
        let f: unsafe extern "win64" fn(usize, u32, usize, u32) =
            unsafe { core::mem::transmute(m.lparam as usize) };
        // SAFETY: appel du callback invité.
        unsafe { f(m.hwnd, WM_TIMER, m.wparam, m.time) };
        return 0;
    }
    if m.hwnd == 0 {
        return 0;
    }
    send(m.hwnd, m.message, m.wparam, m.lparam)
}

pub(super) extern "win64" fn DispatchMessageW(msg: *const Msg) -> isize {
    ffi_guard!(CHANNEL, "DispatchMessageW", 0, { dispatch(msg) })
}

pub(super) extern "win64" fn GetClientRect(hwnd: usize, rect: *mut [i32; 4]) -> WinBool {
    ffi_guard!(CHANNEL, "GetClientRect", WIN_FALSE, {
        let Some(win) = window(hwnd) else {
            set_last_error(ERROR_INVALID_WINDOW_HANDLE);
            return WIN_FALSE;
        };
        write_out(rect, [0, 0, win.size.0, win.size.1]);
        WIN_TRUE
    })
}

fn window_long(hwnd: usize, index: i32, new: Option<usize>) -> usize {
    let mut wins = WINDOWS.lock();
    let Some(w) = wins.get_mut(&hwnd) else {
        set_last_error(ERROR_INVALID_WINDOW_HANDLE);
        return 0;
    };
    let slot: &mut usize = match index {
        GWLP_WNDPROC => &mut w.wnd_proc,
        GWLP_HINSTANCE => &mut w.instance,
        GWLP_HWNDPARENT => &mut w.parent,
        GWLP_ID => &mut w.id,
        GWLP_USERDATA => &mut w.user_data,
        GWL_STYLE | GWL_EXSTYLE => {
            let field = if index == GWL_STYLE {
                &mut w.style
            } else {
                &mut w.ex_style
            };
            let prev = *field as usize;
            if let Some(v) = new {
                *field = v as u32;
            }
            set_last_error(0);
            return prev;
        }
        i if i >= 0 && (i as usize) + 8 <= w.extra.len() => {
            let i = i as usize;
            let mut b = [0u8; 8];
            b.copy_from_slice(&w.extra[i..i + 8]);
            if let Some(v) = new {
                w.extra[i..i + 8].copy_from_slice(&v.to_le_bytes());
            }
            set_last_error(0);
            return usize::from_le_bytes(b);
        }
        _ => {
            set_last_error(ERROR_INVALID_INDEX);
            return 0;
        }
    };
    let prev = *slot;
    if let Some(v) = new {
        *slot = v;
    }
    set_last_error(0);
    prev
}

pub(super) extern "win64" fn GetWindowLongPtrW(hwnd: usize, index: i32) -> usize {
    ffi_guard!(CHANNEL, "GetWindowLongPtrW", 0, {
        window_long(hwnd, index, None)
    })
}

pub(super) extern "win64" fn SetWindowLongPtrW(hwnd: usize, index: i32, value: usize) -> usize {
    ffi_guard!(CHANNEL, "SetWindowLongPtrW", 0, {
        window_long(hwnd, index, Some(value))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_common::wide::to_wide_null;
    use std::sync::atomic::AtomicU32;

    static LOG: LazyLock<Mutex<Vec<(usize, u32, usize)>>> =
        LazyLock::new(|| Mutex::new(Vec::new()));
    static CREATED: AtomicU32 = AtomicU32::new(0);

    extern "win64" fn test_proc(hwnd: usize, msg: u32, w: usize, l: isize) -> isize {
        LOG.lock().push((hwnd, msg, w));
        match msg {
            WM_CREATE => {
                CREATED.fetch_add(1, Ordering::SeqCst);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(7);
                0
            }
            0x0400 => 0x1234, // WM_USER
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }

    fn wc(name: &[u16]) -> WndClassEx {
        WndClassEx {
            cb_size: 80,
            style: 0,
            wnd_proc: test_proc as *const () as usize,
            cls_extra: 0,
            wnd_extra: 16,
            instance: 0,
            icon: 0,
            cursor: 0,
            background: 0,
            menu_name: 0,
            class_name: name.as_ptr() as usize,
            icon_sm: 0,
        }
    }

    #[test]
    fn window_lifecycle_and_message_loop() {
        super::super::init_test_thread();
        let name = to_wide_null("SbAgentDTestClass");
        let atom = RegisterClassExW(&wc(&name));
        assert!(atom >= 0xC000);
        assert_eq!(RegisterClassExW(&wc(&name)), 0);
        assert_eq!(sb_loader::last_error(), ERROR_CLASS_ALREADY_EXISTS);
        let mut bad = wc(&name);
        bad.cb_size = 10;
        assert_eq!(RegisterClassExW(&bad), 0);

        let title = to_wide_null("Hello");
        let hwnd = CreateWindowExW(
            0,
            name.as_ptr(),
            title.as_ptr(),
            WS_VISIBLE,
            CW_USEDEFAULT,
            0,
            CW_USEDEFAULT,
            0,
            0,
            0,
            0,
            core::ptr::null_mut(),
        );
        assert_ne!(hwnd, 0);
        assert_eq!(CREATED.load(Ordering::SeqCst), 1);
        {
            let log = LOG.lock();
            let msgs: Vec<u32> = log.iter().filter(|e| e.0 == hwnd).map(|e| e.1).collect();
            assert_eq!(&msgs[..3], &[WM_NCCREATE, WM_CREATE, WM_SHOWWINDOW]);
        }
        assert_eq!(SendMessageW(hwnd, WM_GETTEXTLENGTH, 0, 0), 5);
        let mut rect = [0i32; 4];
        assert_eq!(GetClientRect(hwnd, &mut rect), WIN_TRUE);
        assert_eq!(rect, [0, 0, 1280, 720]);
        assert_eq!(SetWindowLongPtrW(hwnd, GWLP_USERDATA, 99), 0);
        assert_eq!(GetWindowLongPtrW(hwnd, GWLP_USERDATA), 99);
        assert_eq!(SetWindowLongPtrW(hwnd, 8, 0xABCD), 0);
        assert_eq!(GetWindowLongPtrW(hwnd, 8), 0xABCD);
        assert_eq!(GetWindowLongPtrW(hwnd, 9), 0);
        assert_eq!(sb_loader::last_error(), ERROR_INVALID_INDEX);
        assert_eq!(ShowWindow(hwnd, 0), WIN_TRUE);
        assert_eq!(ShowWindow(hwnd, 0), WIN_FALSE);

        // Messages postés : filtrage, PM_NOREMOVE, dispatch, traduction clavier.
        let mut m = Msg::default();
        assert_eq!(PeekMessageW(&mut m, 0, 0, 0, PM_REMOVE), WIN_FALSE);
        assert_eq!(PostMessageW(hwnd, 0x0400, 1, 2), WIN_TRUE);
        assert_eq!(PostMessageW(hwnd, WM_KEYDOWN, 0x41, 0), WIN_TRUE);
        assert_eq!(PeekMessageW(&mut m, 0, WM_KEYDOWN, WM_KEYDOWN, 0), WIN_TRUE);
        assert_eq!(m.message, WM_KEYDOWN);
        assert_eq!(PeekMessageW(&mut m, 0, 0, 0, PM_REMOVE), WIN_TRUE);
        assert_eq!(
            (m.hwnd, m.message, m.wparam, m.lparam),
            (hwnd, 0x0400, 1, 2)
        );
        assert_eq!(DispatchMessageW(&m), 0x1234);
        assert_eq!(GetMessageW(&mut m, 0, 0, 0), WIN_TRUE);
        assert_eq!(m.message, WM_KEYDOWN);
        assert_eq!(TranslateMessage(&m), WIN_TRUE);
        assert_eq!(GetMessageW(&mut m, hwnd, 0, 0), WIN_TRUE);
        assert_eq!((m.message, m.wparam), (WM_CHAR, usize::from(b'a')));
        assert_eq!(PostMessageW(0x7777_7770, 1, 0, 0), WIN_FALSE);
        assert_eq!(sb_loader::last_error(), ERROR_INVALID_WINDOW_HANDLE);

        // Message de thread + WM_CLOSE -> DestroyWindow -> WM_DESTROY -> PostQuitMessage(7).
        assert_eq!(PostMessageW(0, 0x8001, 5, 0), WIN_TRUE);
        assert_eq!(PostMessageW(hwnd, WM_CLOSE, 0, 0), WIN_TRUE);
        let mut seen = Vec::new();
        loop {
            let r = GetMessageW(&mut m, 0, 0, 0);
            if r == 0 {
                break;
            }
            assert_eq!(r, WIN_TRUE);
            seen.push(m.message);
            TranslateMessage(&m);
            DispatchMessageW(&m);
        }
        assert_eq!(seen, vec![0x8001, WM_CLOSE]);
        assert_eq!((m.message, m.wparam), (WM_QUIT, 7));
        assert_eq!(IsWindow(hwnd), WIN_FALSE);
        assert!(
            LOG.lock()
                .iter()
                .any(|e| e.0 == hwnd && e.1 == WM_NCDESTROY)
        );
        assert_eq!(DestroyWindow(hwnd), WIN_FALSE);
        assert_eq!(sb_loader::last_error(), ERROR_INVALID_WINDOW_HANDLE);
        assert_eq!(UnregisterClassW(name.as_ptr(), 0), WIN_TRUE);
        let missing = to_wide_null("NoSuchClass");
        assert_eq!(
            CreateWindowExW(
                0,
                missing.as_ptr(),
                core::ptr::null(),
                0,
                0,
                0,
                1,
                1,
                0,
                0,
                0,
                core::ptr::null_mut()
            ),
            0
        );
        assert_eq!(sb_loader::last_error(), ERROR_CANNOT_FIND_WND_CLASS);
    }
}
