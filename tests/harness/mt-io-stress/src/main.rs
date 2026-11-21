//! Harnais PE32+ x86-64 de stress **Multi-Threading, Synchronisation, I/O 64 bits & Runtimes MSVC/GDK** :
//! Exécuté nativement sous Linux par ScarletBridge (sans Wine) :
//! 1. 8 threads Windows concurrents (`CreateThread`, `CREATE_SUSPENDED` + `ResumeThread`, `TEB`/`GS`,
//!    `Tls*` + `Fls*` + destructeurs FLS + `.tls` natif par thread, `WaitForMultipleObjects`).
//! 2. `CRITICAL_SECTION`, `SRWLOCK` (Exclusive + Shared), `CONDITION_VARIABLE` (producteur/consommateur
//!    200 items, zéro lost wakeup), `WaitOnAddress` + `msvcp140_atomic_wait`, `Semaphore`, `Mutex`,
//!    `Event`, `InitOnceExecuteOnce` (8 threads en course -> exactement 1 init).
//! 3. I/O 64 bits VFS (`CreateFileW`, `WriteFile`, `GetFileSizeEx`, `SetFilePointerEx`, 4 threads en
//!    `ReadFile` `OVERLAPPED` simultanés sur le même handle + `IOCP`, `CreateFileMappingW` + `MapViewOfFile`,
//!    `FindFirstFileW` + blocage path traversal).
//! 4. `xgameruntime` (`XTaskQueue` multi-threadé réel, `XPersistentLocalStorageGetPath` = `S:\`, `XUser*`),
//!    `XCurl` offline strict, `amd_fidelityfx_loader_xbox`, `concrt140`, `vcomp140`, `msvcp140`, `vcruntime140_1`.

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "[mt_io_stress] Compiler pour PE32+ x86-64 :\n\
         cargo build --target x86_64-pc-windows-gnu -p sb-harness-mt-io-stress"
    );
}

#[cfg(windows)]
fn main() {
    std::process::exit(win_impl::run());
}

#[cfg(windows)]
mod win_impl {
    use core::ffi::c_void;
    use std::cell::Cell;
    use std::ptr;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    const INFINITE: u32 = 0xFFFF_FFFF;
    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const INVALID_HANDLE_VALUE: usize = usize::MAX;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        h_event: usize,
    }

    #[repr(C)]
    struct Win32FindDataW {
        file_attributes: u32,
        creation_time: [u32; 2],
        last_access_time: [u32; 2],
        last_write_time: [u32; 2],
        file_size_high: u32,
        file_size_low: u32,
        reserved0: u32,
        reserved1: u32,
        file_name: [u16; 260],
        alternate_file_name: [u16; 14],
    }

    #[link(name = "kernel32", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn CreateThread(
            sec: *mut c_void,
            stack_size: usize,
            start: unsafe extern "win64" fn(*mut c_void) -> u32,
            param: *mut c_void,
            flags: u32,
            out_tid: *mut u32,
        ) -> usize;
        fn ResumeThread(h: usize) -> u32;
        fn GetExitCodeThread(h: usize, out_code: *mut u32) -> i32;
        fn WaitForSingleObject(h: usize, ms: u32) -> u32;
        fn WaitForMultipleObjects(count: u32, hs: *const usize, wait_all: i32, ms: u32) -> u32;
        fn CloseHandle(h: usize) -> i32;

        fn TlsAlloc() -> u32;
        fn TlsGetValue(idx: u32) -> *mut c_void;
        fn TlsSetValue(idx: u32, val: *mut c_void) -> i32;
        fn TlsFree(idx: u32) -> i32;
        fn FlsAlloc(cb: Option<unsafe extern "win64" fn(*mut c_void)>) -> u32;
        fn FlsGetValue(idx: u32) -> *mut c_void;
        fn FlsSetValue(idx: u32, val: *mut c_void) -> i32;
        fn FlsFree(idx: u32) -> i32;

        fn InitializeCriticalSection(cs: *mut c_void);
        fn DeleteCriticalSection(cs: *mut c_void);
        fn EnterCriticalSection(cs: *mut c_void);
        fn TryEnterCriticalSection(cs: *mut c_void) -> i32;
        fn LeaveCriticalSection(cs: *mut c_void);

        fn InitializeSRWLock(lock: *mut c_void);
        fn AcquireSRWLockExclusive(lock: *mut c_void);
        fn ReleaseSRWLockExclusive(lock: *mut c_void);
        fn AcquireSRWLockShared(lock: *mut c_void);
        fn ReleaseSRWLockShared(lock: *mut c_void);
        fn InitializeConditionVariable(cv: *mut c_void);
        fn SleepConditionVariableSRW(cv: *mut c_void, srw: *mut c_void, ms: u32, flags: u32)
        -> i32;
        fn WakeConditionVariable(cv: *mut c_void);

        fn CreateEventW(sec: *mut c_void, manual: i32, init: i32, name: *const u16) -> usize;
        fn SetEvent(h: usize) -> i32;
        fn ResetEvent(h: usize) -> i32;
        fn CreateSemaphoreW(sec: *mut c_void, init: i32, max: i32, name: *const u16) -> usize;
        fn ReleaseSemaphore(h: usize, count: i32, prev: *mut i32) -> i32;
        fn CreateMutexW(sec: *mut c_void, owner: i32, name: *const u16) -> usize;
        fn ReleaseMutex(h: usize) -> i32;

        fn InitOnceExecuteOnce(
            once: *mut usize,
            cb: unsafe extern "win64" fn(*mut usize, *mut c_void, *mut *mut c_void) -> i32,
            param: *mut c_void,
            out_ctx: *mut *mut c_void,
        ) -> i32;

        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            sec: *mut c_void,
            disp: u32,
            flags: u32,
            tpl: usize,
        ) -> usize;
        fn WriteFile(
            h: usize,
            buf: *const u8,
            len: u32,
            written: *mut u32,
            ov: *mut Overlapped,
        ) -> i32;
        fn ReadFile(h: usize, buf: *mut u8, len: u32, read: *mut u32, ov: *mut Overlapped) -> i32;
        fn GetFileSizeEx(h: usize, out_size: *mut i64) -> i32;
        fn SetFilePointerEx(h: usize, dist: i64, out_pos: *mut i64, method: u32) -> i32;
        fn CreateFileMappingW(
            h: usize,
            sec: *mut c_void,
            protect: u32,
            high: u32,
            low: u32,
            name: *const u16,
        ) -> usize;
        fn MapViewOfFile(h: usize, access: u32, high: u32, low: u32, len: usize) -> *mut c_void;
        fn UnmapViewOfFile(ptr: *const c_void) -> i32;
        fn FindFirstFileW(pat: *const u16, data: *mut Win32FindDataW) -> usize;
        fn FindNextFileW(h: usize, data: *mut Win32FindDataW) -> i32;
        fn FindClose(h: usize) -> i32;
        fn CreateIoCompletionPort(fh: usize, port: usize, key: usize, threads: u32) -> usize;
        fn GetQueuedCompletionStatus(
            port: usize,
            bytes: *mut u32,
            key: *mut usize,
            ov: *mut *mut Overlapped,
            ms: u32,
        ) -> i32;
    }

    #[link(name = "msvcp140_atomic_wait", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn __std_atomic_wait_direct(
            storage: *const u8,
            cmp: *const u8,
            size: usize,
            ms: u32,
        ) -> i32;
        fn __std_atomic_notify_all_direct(storage: *const c_void);
    }

    #[link(name = "msvcp140", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn _Thrd_hardware_concurrency() -> u32;
        fn _Mtx_init_in_situ(mtx: *mut c_void, flags: i32);
        fn _Mtx_lock(mtx: *mut c_void) -> i32;
        fn _Mtx_unlock(mtx: *mut c_void) -> i32;
    }

    #[link(name = "concrt140", kind = "raw-dylib")]
    unsafe extern "win64" {
        #[link_name = "?Alloc@Concurrency@@YAPEAX_K@Z"]
        fn concrt_alloc(size: usize) -> *mut c_void;
        #[link_name = "?Free@Concurrency@@YAXPEAX@Z"]
        fn concrt_free(ptr: *mut c_void);
        #[link_name = "?GetNumberOfVirtualProcessors@CurrentScheduler@Concurrency@@SAIXZ"]
        fn concrt_vprocs() -> u32;
    }

    #[link(name = "vcomp140", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn omp_get_num_procs() -> i32;
        fn omp_init_lock(lock: *mut c_void);
        fn omp_set_lock(lock: *mut c_void);
        fn omp_unset_lock(lock: *mut c_void);
        fn omp_destroy_lock(lock: *mut c_void);
    }

    #[link(name = "xgameruntime", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn XGameRuntimeInitialize() -> i32;
        fn XPersistentLocalStorageGetPath(size: usize, buf: *mut u8, used: *mut usize) -> i32;
        fn XTaskQueueCreate(work_mode: u32, comp_mode: u32, out_q: *mut usize) -> i32;
        fn XTaskQueueSubmitCallback(
            q: usize,
            port: u32,
            ctx: *mut c_void,
            cb: unsafe extern "win64" fn(*mut c_void, u8),
        ) -> i32;
        fn XTaskQueueCloseHandle(q: usize);
    }

    #[link(name = "XCurl", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn curl_global_init(flags: i64) -> i32;
        fn curl_easy_init() -> *mut c_void;
        fn curl_easy_perform(h: *mut c_void) -> i32;
        fn curl_easy_cleanup(h: *mut c_void);
        fn curl_global_cleanup();
    }

    #[link(name = "amd_fidelityfx_loader_xbox", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn ffxCreateContext(ctx: *mut *mut c_void, desc: *const c_void, mem: *const c_void) -> i32;
        fn ffxDispatch(ctx: *mut *mut c_void, desc: *const c_void) -> i32;
        fn ffxDestroyContext(ctx: *mut *mut c_void, mem: *const c_void) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(core::iter::once(0)).collect()
    }

    // --- État partagé pour le stress multi-thread ---

    thread_local! {
        static NATIVE_TLS_SLOT: Cell<u32> = const { Cell::new(0xA5A5) };
    }

    static mut CS_STORAGE: [u64; 6] = [0; 6];
    static mut CS_COUNTER: u64 = 0;
    static TLS_INDEX: AtomicU32 = AtomicU32::new(0);
    static FLS_INDEX: AtomicU32 = AtomicU32::new(0);
    static FLS_DTOR_COUNT: AtomicU32 = AtomicU32::new(0);
    static INIT_ONCE_SLOT: AtomicU64 = AtomicU64::new(0);
    static INIT_ONCE_CALLS: AtomicU32 = AtomicU32::new(0);
    static FUTEX_FLAG: AtomicU64 = AtomicU64::new(0);
    static XTASK_SUM: AtomicU64 = AtomicU64::new(0);

    unsafe extern "win64" fn fls_dtor(val: *mut c_void) {
        if !val.is_null() {
            FLS_DTOR_COUNT.fetch_add(1, Ordering::AcqRel);
        }
    }

    unsafe extern "win64" fn init_once_cb(
        _once: *mut usize,
        _param: *mut c_void,
        out_ctx: *mut *mut c_void,
    ) -> i32 {
        INIT_ONCE_CALLS.fetch_add(1, Ordering::AcqRel);
        // SAFETY: `out_ctx` non-nul.
        unsafe {
            *out_ctx = 0x1234_5678usize as *mut c_void;
        }
        1
    }

    unsafe extern "win64" fn worker_main(param: *mut c_void) -> u32 {
        let id = param as usize;
        // 1. Vérifie `.tls` natif (cloné par thread dans `sb-loader`) + `Tls*` + `Fls*`
        NATIVE_TLS_SLOT.with(|c| {
            assert_eq!(c.get(), 0xA5A5, "template .tls initial corrompu");
            c.set((id as u32) + 100);
        });
        let tls_i = TLS_INDEX.load(Ordering::Acquire);
        let fls_i = FLS_INDEX.load(Ordering::Acquire);
        // SAFETY: appels Win32 `Tls*` / `Fls*` / `InitOnce*` / `CRITICAL_SECTION`.
        unsafe {
            assert_eq!(TlsSetValue(tls_i, (0x1000 + id) as *mut c_void), 1);
            assert_eq!(FlsSetValue(fls_i, (0x2000 + id) as *mut c_void), 1);

            // 2. InitOnce en concurrence entre 8 threads (doit s'exécuter 1 seule fois).
            let mut ctx: *mut c_void = ptr::null_mut();
            let once_ptr = (&raw const INIT_ONCE_SLOT).cast::<usize>().cast_mut();
            assert_eq!(
                InitOnceExecuteOnce(once_ptr, init_once_cb, ptr::null_mut(), &mut ctx),
                1
            );
            assert_eq!(ctx as usize, 0x1234_5678);

            // 3. 1000 incréments protégés par `CRITICAL_SECTION`.
            let cs_ptr = (&raw mut CS_STORAGE).cast::<c_void>();
            for _ in 0..1000 {
                EnterCriticalSection(cs_ptr);
                CS_COUNTER += 1;
                LeaveCriticalSection(cs_ptr);
            }

            // 4. Vérifie que nos valeurs TLS/FLS/.tls n'ont pas été écrasées par un autre thread !
            assert_eq!(TlsGetValue(tls_i) as usize, 0x1000 + id);
            assert_eq!(FlsGetValue(fls_i) as usize, 0x2000 + id);
        }
        NATIVE_TLS_SLOT.with(|c| assert_eq!(c.get(), (id as u32) + 100));

        // 5. Attente futex `msvcp140_atomic_wait` jusqu'à signalement global.
        let zero = 0u64;
        while FUTEX_FLAG.load(Ordering::Acquire) == 0 {
            // SAFETY: futex sur `FUTEX_FLAG`.
            unsafe {
                let _ = __std_atomic_wait_direct(
                    FUTEX_FLAG.as_ptr().cast::<u8>(),
                    (&raw const zero).cast::<u8>(),
                    8,
                    2000,
                );
            }
        }
        (id as u32) + 42
    }

    // Producteur SRWLOCK + CONDITION_VARIABLE
    struct PcState {
        srw: usize,
        cv: usize,
        items: std::sync::Mutex<Vec<u32>>,
    }

    unsafe extern "win64" fn pc_producer(param: *mut c_void) -> u32 {
        // SAFETY: `param` est un `&PcState` vivant jusqu'à `WaitForSingleObject`.
        let st = unsafe { &*(param.cast::<PcState>()) };
        for i in 1..=200u32 {
            // SAFETY: appels `SRWLOCK` / `CONDITION_VARIABLE`.
            unsafe {
                AcquireSRWLockExclusive((&raw const st.srw).cast_mut().cast());
                st.items.lock().expect("lock").push(i);
                ReleaseSRWLockExclusive((&raw const st.srw).cast_mut().cast());
                WakeConditionVariable((&raw const st.cv).cast_mut().cast());
            }
        }
        0
    }

    // Lecteur concurrent OVERLAPPED sur même handle de fichier 64 KiB
    struct IoWorkerArgs {
        file_handle: usize,
        slice_idx: usize,
    }

    unsafe extern "win64" fn io_overlapped_reader(param: *mut c_void) -> u32 {
        // SAFETY: `param` pointe vers un `IoWorkerArgs` valide.
        let args = unsafe { &*(param.cast::<IoWorkerArgs>()) };
        let mut buf = vec![0u8; 16384];
        // SAFETY: appels Win32 `CreateEventW` / `ReadFile` / `WaitForSingleObject`.
        unsafe {
            let ev = CreateEventW(ptr::null_mut(), 1, 0, ptr::null());
            let mut ov = Overlapped {
                offset: (args.slice_idx * 16384) as u32,
                offset_high: 0,
                h_event: ev,
                ..Default::default()
            };
            let mut nread = 0u32;
            assert_eq!(
                ReadFile(
                    args.file_handle,
                    buf.as_mut_ptr(),
                    16384,
                    &mut nread,
                    &mut ov
                ),
                1
            );
            assert_eq!(nread, 16384);
            assert_eq!(WaitForSingleObject(ev, 2000), 0);
            CloseHandle(ev);
        }
        let expected = ((args.slice_idx as u8).wrapping_mul(37)).wrapping_add(11);
        assert!(
            buf.iter().all(|&b| b == expected),
            "corruption lecture OVERLAPPED slice {}",
            args.slice_idx
        );
        0
    }

    unsafe extern "win64" fn xtask_cb(ctx: *mut c_void, canceled: u8) {
        assert_eq!(canceled, 0);
        XTASK_SUM.fetch_add(ctx as u64, Ordering::AcqRel);
    }

    pub fn run() -> i32 {
        println!("[mt_io_stress] === Stress Test PE32+ Multi-Threading, I/O 64 bits & GDK ===");

        // SAFETY: appels Win64 HLE résolus par ScarletBridge (`sb-hle`).
        unsafe {
            // 1. 8 worker threads (`CREATE_SUSPENDED` + `ResumeThread` + TLS/FLS/.tls + CS + InitOnce + Futex)
            let cs_ptr = (&raw mut CS_STORAGE).cast::<c_void>();
            InitializeCriticalSection(cs_ptr);
            assert_eq!(TryEnterCriticalSection(cs_ptr), 1);
            LeaveCriticalSection(cs_ptr);

            let tls_idx = TlsAlloc();
            let fls_idx = FlsAlloc(Some(fls_dtor));
            TLS_INDEX.store(tls_idx, Ordering::Release);
            FLS_INDEX.store(fls_idx, Ordering::Release);

            let mut handles = [0usize; 8];
            for (i, slot) in handles.iter_mut().enumerate() {
                let mut tid = 0u32;
                let h = CreateThread(
                    ptr::null_mut(),
                    0,
                    worker_main,
                    i as *mut c_void,
                    CREATE_SUSPENDED,
                    &mut tid,
                );
                assert!(h != 0 && tid != 0);
                *slot = h;
            }
            for &h in &handles {
                assert_eq!(ResumeThread(h), 1);
            }

            // Débloque la barrière futex après que les 8 threads ont démarré.
            std::thread::sleep(std::time::Duration::from_millis(20));
            FUTEX_FLAG.store(1, Ordering::Release);
            __std_atomic_notify_all_direct(FUTEX_FLAG.as_ptr().cast());

            assert_eq!(
                WaitForMultipleObjects(8, handles.as_ptr(), 1, INFINITE),
                0,
                "WaitForMultipleObjects(bWaitAll=TRUE) sur 8 threads"
            );
            for (i, &h) in handles.iter().enumerate() {
                let mut code = 0u32;
                assert_eq!(GetExitCodeThread(h, &mut code), 1);
                assert_eq!(code, (i as u32) + 42);
                CloseHandle(h);
            }
            assert_eq!(
                ptr::read(&raw const CS_COUNTER),
                8000,
                "CRITICAL_SECTION exactement 8000"
            );
            assert_eq!(
                INIT_ONCE_CALLS.load(Ordering::Acquire),
                1,
                "InitOnce exactement 1"
            );
            assert_eq!(
                FLS_DTOR_COUNT.load(Ordering::Acquire),
                8,
                "8 destructeurs FLS appelés"
            );
            TlsFree(tls_idx);
            FlsFree(fls_idx);
            DeleteCriticalSection(cs_ptr);
            println!(
                "[mt_io_stress] [1/5] 8 threads OK (CS=8000, InitOnce=1, FLS dtors=8, TLS/.tls/Futex OK)"
            );

            // 2. SRWLOCK (Shared + Exclusive) + CONDITION_VARIABLE + Semaphore + Mutex + Event
            let pc = Box::new(PcState {
                srw: 0,
                cv: 0,
                items: std::sync::Mutex::new(Vec::new()),
            });
            let srw_p = (&raw const pc.srw).cast_mut().cast::<c_void>();
            let cv_p = (&raw const pc.cv).cast_mut().cast::<c_void>();
            InitializeSRWLock(srw_p);
            InitializeConditionVariable(cv_p);
            AcquireSRWLockShared(srw_p);
            ReleaseSRWLockShared(srw_p);

            let prod_h = CreateThread(
                ptr::null_mut(),
                0,
                pc_producer,
                (&raw const *pc).cast_mut().cast(),
                0,
                ptr::null_mut(),
            );
            let mut consumed = 0usize;
            let mut sum = 0u64;
            AcquireSRWLockExclusive(srw_p);
            while consumed < 200 {
                while pc.items.lock().expect("lock").is_empty() {
                    assert_eq!(SleepConditionVariableSRW(cv_p, srw_p, 5000, 0), 1);
                }
                for v in pc.items.lock().expect("lock").drain(..) {
                    sum += u64::from(v);
                    consumed += 1;
                }
            }
            ReleaseSRWLockExclusive(srw_p);
            assert_eq!(WaitForSingleObject(prod_h, 5000), 0);
            CloseHandle(prod_h);
            assert_eq!(sum, 20100, "somme 1..=200 via SRWLOCK+CV");

            let sem = CreateSemaphoreW(ptr::null_mut(), 0, 2, ptr::null());
            let mtx = CreateMutexW(ptr::null_mut(), 0, ptr::null());
            let ev = CreateEventW(ptr::null_mut(), 1, 0, ptr::null());
            SetEvent(ev);
            ResetEvent(ev);
            ReleaseSemaphore(sem, 1, ptr::null_mut());
            let any_hs = [ev, sem];
            assert_eq!(WaitForMultipleObjects(2, any_hs.as_ptr(), 0, 1000), 1);
            assert_eq!(WaitForSingleObject(mtx, 1000), 0);
            assert_eq!(ReleaseMutex(mtx), 1);
            CloseHandle(sem);
            CloseHandle(mtx);
            CloseHandle(ev);
            println!(
                "[mt_io_stress] [2/5] Sync OK (SRWLOCK+CV 200 items sum=20100, Semaphore/Mutex/Event/WaitAny)"
            );

            // 3. I/O 64 bits VFS + 4 lectures OVERLAPPED concurrentes + IOCP + FileMapping + FindFirstFileW
            let archive_path = wide("S:\\mt_archive.bin");
            let fh = CreateFileW(
                archive_path.as_ptr(),
                0xC000_0000,
                3,
                ptr::null_mut(),
                2,
                0,
                0,
            );
            assert_ne!(fh, INVALID_HANDLE_VALUE, "CreateFileW S:\\mt_archive.bin");

            let mut payload = vec![0u8; 65536];
            for s in 0..4usize {
                let val = ((s as u8).wrapping_mul(37)).wrapping_add(11);
                payload[s * 16384..(s + 1) * 16384].fill(val);
            }
            let mut written = 0u32;
            assert_eq!(
                WriteFile(fh, payload.as_ptr(), 65536, &mut written, ptr::null_mut()),
                1
            );
            assert_eq!(written, 65536);

            let mut fsize = 0i64;
            assert_eq!(GetFileSizeEx(fh, &mut fsize), 1);
            assert_eq!(fsize, 65536);
            let mut new_pos = 0i64;
            assert_eq!(SetFilePointerEx(fh, 16384, &mut new_pos, 0), 1);
            assert_eq!(new_pos, 16384);

            // IOCP + 4 threads lisant 4 tranches simultanément via `OVERLAPPED` (`pread`)
            let iocp = CreateIoCompletionPort(fh, 0, 0xCAFE, 4);
            assert!(iocp != 0);
            let args: Vec<IoWorkerArgs> = (0..4)
                .map(|s| IoWorkerArgs {
                    file_handle: fh,
                    slice_idx: s,
                })
                .collect();
            let mut io_hs = [0usize; 4];
            for i in 0..4 {
                io_hs[i] = CreateThread(
                    ptr::null_mut(),
                    0,
                    io_overlapped_reader,
                    (&raw const args[i]).cast_mut().cast(),
                    0,
                    ptr::null_mut(),
                );
            }
            assert_eq!(WaitForMultipleObjects(4, io_hs.as_ptr(), 1, 5000), 0);
            for h in io_hs {
                CloseHandle(h);
            }
            for _ in 0..4 {
                let mut bytes = 0u32;
                let mut key = 0usize;
                let mut ov_p: *mut Overlapped = ptr::null_mut();
                assert_eq!(
                    GetQueuedCompletionStatus(iocp, &mut bytes, &mut key, &mut ov_p, 2000),
                    1
                );
                assert_eq!(bytes, 16384);
                assert_eq!(key, 0xCAFE);
            }
            CloseHandle(iocp);

            // Memory-mapped file (`CreateFileMappingW` + `MapViewOfFile`)
            let mh = CreateFileMappingW(fh, ptr::null_mut(), 0x02, 0, 0, ptr::null());
            assert!(mh != 0);
            let view = MapViewOfFile(mh, 0x04, 0, 0, 65536);
            assert!(!view.is_null());
            let mapped = std::slice::from_raw_parts(view.cast::<u8>(), 65536);
            assert_eq!(mapped[0], 11);
            assert_eq!(mapped[16384], 48);
            assert_eq!(UnmapViewOfFile(view), 1);
            CloseHandle(mh);
            CloseHandle(fh);

            // FindFirstFileW + blocage traversal VFS
            let pat = wide("S:\\*.bin");
            let mut fdata: Win32FindDataW = core::mem::zeroed();
            let find_h = FindFirstFileW(pat.as_ptr(), &mut fdata);
            assert_ne!(find_h, INVALID_HANDLE_VALUE);
            let mut found = false;
            loop {
                let end = fdata.file_name.iter().position(|&c| c == 0).unwrap_or(260);
                let name = String::from_utf16_lossy(&fdata.file_name[..end]);
                if name.eq_ignore_ascii_case("mt_archive.bin") {
                    found = true;
                }
                if FindNextFileW(find_h, &mut fdata) == 0 {
                    break;
                }
            }
            FindClose(find_h);
            assert!(found, "mt_archive.bin doit apparaître via FindFirstFileW");

            let bad_path = wide("S:\\..\\..\\etc\\passwd");
            assert_eq!(
                CreateFileW(bad_path.as_ptr(), 0x8000_0000, 1, ptr::null_mut(), 3, 0, 0),
                INVALID_HANDLE_VALUE,
                "le path traversal VFS doit être bloqué"
            );
            println!(
                "[mt_io_stress] [3/5] I/O 64 bits OK (OVERLAPPED 4 threads + IOCP + MapViewOfFile + FindFirstFileW + sandbox VFS)"
            );

            // 4. Xbox Series X GDK (`XTaskQueue` multi-threadé + `XPersistentLocalStorageGetPath` + `XCurl` offline + FidelityFX)
            assert_eq!(XGameRuntimeInitialize(), 0);
            let mut pls = [0u8; 32];
            let mut pls_used = 0usize;
            assert_eq!(
                XPersistentLocalStorageGetPath(pls.len(), pls.as_mut_ptr(), &mut pls_used),
                0
            );
            assert_eq!(&pls[..pls_used - 1], b"S:\\");

            let mut tq = 0usize;
            assert_eq!(XTaskQueueCreate(0, 0, &mut tq), 0);
            for v in [10usize, 20, 30, 40] {
                assert_eq!(
                    XTaskQueueSubmitCallback(tq, 0, v as *mut c_void, xtask_cb),
                    0
                );
            }
            for _ in 0..100 {
                if XTASK_SUM.load(Ordering::Acquire) == 100 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert_eq!(
                XTASK_SUM.load(Ordering::Acquire),
                100,
                "XTaskQueue async callbacks"
            );
            XTaskQueueCloseHandle(tq);

            assert_eq!(curl_global_init(0), 0);
            let ch = curl_easy_init();
            assert!(!ch.is_null());
            assert_eq!(
                curl_easy_perform(ch),
                7,
                "XCurl offline -> CURLE_COULDNT_CONNECT"
            );
            curl_easy_cleanup(ch);
            curl_global_cleanup();

            let mut ffx_ctx: *mut c_void = ptr::null_mut();
            assert_eq!(ffxCreateContext(&mut ffx_ctx, ptr::null(), ptr::null()), 0);
            assert_eq!(ffxDispatch(&mut ffx_ctx, ptr::null()), 0);
            assert_eq!(ffxDestroyContext(&mut ffx_ctx, ptr::null()), 0);
            println!(
                "[mt_io_stress] [4/5] GDK Series X OK (XTaskQueue async=100, PLS='S:\\', XCurl offline=7, FidelityFX)"
            );

            // 5. Runtimes MSVC (`msvcp140`, `concrt140`, `vcomp140`)
            assert!(_Thrd_hardware_concurrency() >= 1);
            let mut mtx_buf = [0u64; 8];
            _Mtx_init_in_situ(mtx_buf.as_mut_ptr().cast(), 0);
            assert_eq!(_Mtx_lock(mtx_buf.as_mut_ptr().cast()), 0);
            assert_eq!(_Mtx_unlock(mtx_buf.as_mut_ptr().cast()), 0);

            assert!(concrt_vprocs() >= 1);
            let cp = concrt_alloc(256);
            assert!(!cp.is_null());
            concrt_free(cp);

            assert!(omp_get_num_procs() >= 1);
            let mut omp_lock = 0usize;
            omp_init_lock((&raw mut omp_lock).cast());
            omp_set_lock((&raw mut omp_lock).cast());
            omp_unset_lock((&raw mut omp_lock).cast());
            omp_destroy_lock((&raw mut omp_lock).cast());
            println!(
                "[mt_io_stress] [5/5] Runtimes MSVC OK (msvcp140, concrt140, vcomp140, atomic_wait)"
            );
        }

        println!("[mt_io_stress] === TOUS LES TESTS MULTI-THREADING, I/O & GDK RÉUSSIS ===");
        0
    }
}
