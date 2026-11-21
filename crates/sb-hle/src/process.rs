//! État global du processus invité (ligne de commande, argv/argc, VFS, synchronisation, TLS/FLS).

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::{Arc, OnceLock};

use parking_lot::{Condvar, Mutex, RwLock};
use sb_common::wide::to_wide_null;
use sb_runtime::lifecycle::LifecycleManager;
use sb_vfs::VirtualFs;

pub const HANDLE_STDIN: usize = 0x0000_0000_0000_1000;
pub const HANDLE_STDOUT: usize = 0x0000_0000_0000_1001;
pub const HANDLE_STDERR: usize = 0x0000_0000_0000_1002;
pub const HANDLE_PROCESS_HEAP: usize = 0x0000_0001_0000_0000;
pub const PSEUDO_CURRENT_PROCESS: usize = usize::MAX;
pub const PSEUDO_CURRENT_THREAD: usize = usize::MAX - 1;

pub struct ProcessState {
    pub cmdline_wide: Vec<u16>,
    pub cmdline_narrow: CString,
    pub argv_cstrings: Vec<CString>,
    pub argv_ptrs: Vec<*mut u8>,
    pub wargv_bufs: Vec<Vec<u16>>,
    pub wargv_ptrs: Vec<*mut u16>,
    pub argc: i32,
    pub env_narrow: Vec<*mut u8>,
    pub env_wide_block: Vec<u16>,
    pub commode: i32,
    pub fmode: i32,
    pub vfs: Option<VirtualFs>,
    pub lifecycle: LifecycleManager,
    pub tls_next: u32,
    pub tls_values: HashMap<u32, usize>,
    pub fls_next: u32,
    pub fls_values: HashMap<u32, usize>,
    pub events: HashMap<usize, Arc<EventObject>>,
    pub next_handle: usize,
    pub crit_sections: HashMap<usize, Arc<ReentrantCs>>,
    pub condvars: HashMap<usize, Arc<Condvar>>,
}

// SAFETY: les pointeurs `argv_ptrs`/`wargv_ptrs`/`env_narrow` pointent vers les `Vec`/`CString`
// possédés par `ProcessState` qui vit jusqu'à la fin du processus dans `OnceLock`.
unsafe impl Send for ProcessState {}
unsafe impl Sync for ProcessState {}

pub struct EventObject {
    pub manual_reset: bool,
    pub signaled: Mutex<bool>,
    pub cv: Condvar,
}

/// Section critique Windows réentrante par thread.
pub struct ReentrantCs {
    inner: Mutex<CsInner>,
    cv: Condvar,
}

struct CsInner {
    owner: Option<std::thread::ThreadId>,
    depth: u32,
}

impl Default for ReentrantCs {
    fn default() -> Self {
        Self::new()
    }
}

impl ReentrantCs {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(CsInner {
                owner: None,
                depth: 0,
            }),
            cv: Condvar::new(),
        }
    }

    pub fn lock(&self) {
        let me = std::thread::current().id();
        let mut st = self.inner.lock();
        while let Some(owner) = st.owner {
            if owner == me {
                st.depth += 1;
                return;
            }
            self.cv.wait(&mut st);
        }
        st.owner = Some(me);
        st.depth = 1;
    }

    pub fn try_lock(&self) -> bool {
        let me = std::thread::current().id();
        let mut st = self.inner.lock();
        match st.owner {
            None => {
                st.owner = Some(me);
                st.depth = 1;
                true
            }
            Some(owner) if owner == me => {
                st.depth += 1;
                true
            }
            Some(_) => false,
        }
    }

    pub fn unlock(&self) {
        let me = std::thread::current().id();
        let mut st = self.inner.lock();
        if st.owner == Some(me) {
            st.depth = st.depth.saturating_sub(1);
            if st.depth == 0 {
                st.owner = None;
                self.cv.notify_one();
            }
        }
    }
}

static PROCESS: OnceLock<RwLock<ProcessState>> = OnceLock::new();

fn build_process_state(exe_name: &str, args: &[String], vfs: Option<VirtualFs>) -> ProcessState {
    let mut all_args = Vec::with_capacity(1 + args.len());
    all_args.push(exe_name.to_string());
    all_args.extend(args.iter().cloned());

    let joined = all_args
        .iter()
        .map(|a| {
            if a.contains(' ') {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    let cmdline_wide = to_wide_null(&joined);
    let cmdline_narrow =
        CString::new(joined).unwrap_or_else(|_| CString::new("app.exe").unwrap_or_default());

    let mut argv_cstrings = Vec::with_capacity(all_args.len());
    let mut argv_ptrs = Vec::with_capacity(all_args.len() + 1);
    let mut wargv_bufs = Vec::with_capacity(all_args.len());
    let mut wargv_ptrs = Vec::with_capacity(all_args.len() + 1);

    for arg in &all_args {
        let c = CString::new(arg.as_str()).unwrap_or_default();
        argv_ptrs.push(c.as_ptr() as *mut u8);
        argv_cstrings.push(c);

        let w = to_wide_null(arg);
        wargv_ptrs.push(w.as_ptr() as *mut u16);
        wargv_bufs.push(w);
    }
    argv_ptrs.push(std::ptr::null_mut());
    wargv_ptrs.push(std::ptr::null_mut());

    // Bloc environnement UTF-16LE minimal (double nul terminal).
    let mut env_wide_block = Vec::new();
    for (k, v) in std::env::vars() {
        if k.starts_with("SB_") || k == "LANG" || k == "LC_ALL" {
            env_wide_block.extend(format!("{k}={v}").encode_utf16());
            env_wide_block.push(0);
        }
    }
    env_wide_block.push(0);
    if env_wide_block.len() == 1 {
        env_wide_block.push(0);
    }

    ProcessState {
        cmdline_wide,
        cmdline_narrow,
        argc: all_args.len() as i32,
        argv_cstrings,
        argv_ptrs,
        wargv_bufs,
        wargv_ptrs,
        env_narrow: vec![std::ptr::null_mut()],
        env_wide_block,
        commode: 0,
        fmode: 0x4000,
        vfs,
        lifecycle: LifecycleManager::from_env(),
        tls_next: 1,
        tls_values: HashMap::new(),
        fls_next: 1,
        fls_values: HashMap::new(),
        events: HashMap::new(),
        next_handle: 0x2000,
        crit_sections: HashMap::new(),
        condvars: HashMap::new(),
    }
}

/// Initialise l'état du processus invité avant l'exécution du point d'entrée PE32+.
pub fn init_process(exe_name: &str, args: &[String], vfs: Option<VirtualFs>) {
    let state = build_process_state(exe_name, args, vfs);
    if let Some(existing) = PROCESS.get() {
        *existing.write() = state;
    } else {
        let _ = PROCESS.set(RwLock::new(state));
    }
}

pub fn process_state() -> &'static RwLock<ProcessState> {
    PROCESS.get_or_init(|| RwLock::new(build_process_state("app.exe", &[], None)))
}
