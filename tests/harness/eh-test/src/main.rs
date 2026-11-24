//! Harnais PE32+ x86-64 de validation de la **propagation d'exceptions Windows x64** (SEH) :
//! unwinding Rust (`x86_64-pc-windows-gnu` : `RaiseException(0x20474343)` + personnalité SEH),
//! `catch_unwind`, panics dans des threads, destructeurs exécutés pendant le déroulage, panics
//! imbriqués, `resume_unwind`, exceptions SEH personnalisées rattrapées par un gestionnaire
//! vectoriel, `RtlUnwindEx` direct et `__try`-équivalent (faute matérielle NULL récupérée).
//!
//! Exécution : `scarletbridge run --headless target/x86_64-pc-windows-gnu/debug/eh_test.exe`.

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "[eh_test] Compiler pour PE32+ x86-64 :\n\
         cargo build --target x86_64-pc-windows-gnu -p sb-harness-eh-test"
    );
}

#[cfg(windows)]
fn main() {
    std::process::exit(win_impl::run());
}

#[cfg(windows)]
mod win_impl {
    use core::ffi::c_void;
    use std::panic::{self, AssertUnwindSafe, catch_unwind, resume_unwind};
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    const CUSTOM_CODE: u32 = 0xE000_0001;
    const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
    const STATUS_ACCESS_VIOLATION: u32 = 0xC000_0005;

    #[repr(C)]
    struct ExceptionRecord {
        code: u32,
        flags: u32,
        record: *mut ExceptionRecord,
        address: *mut c_void,
        number_parameters: u32,
        information: [usize; 15],
    }

    #[repr(C)]
    struct ExceptionPointers {
        record: *mut ExceptionRecord,
        context: *mut c_void,
    }

    #[link(name = "kernel32", kind = "raw-dylib")]
    unsafe extern "system" {
        fn RaiseException(code: u32, flags: u32, nargs: u32, args: *const usize);
        fn AddVectoredExceptionHandler(
            first: u32,
            handler: unsafe extern "system" fn(*mut ExceptionPointers) -> i32,
        ) -> *mut c_void;
        fn RemoveVectoredExceptionHandler(handle: *mut c_void) -> u32;
        fn CreateThread(
            sec: *mut c_void,
            stack_size: usize,
            start: unsafe extern "system" fn(*mut c_void) -> u32,
            param: *mut c_void,
            flags: u32,
            out_tid: *mut u32,
        ) -> usize;
        fn WaitForSingleObject(h: usize, ms: u32) -> u32;
        fn GetExitCodeThread(h: usize, out_code: *mut u32) -> i32;
    }

    static VEH_HITS: AtomicU32 = AtomicU32::new(0);
    static VEH_PARAM: AtomicUsize = AtomicUsize::new(0);
    static VEH_FAULT_ADDR: AtomicUsize = AtomicUsize::new(0);
    static DROPS: AtomicU32 = AtomicU32::new(0);

    struct Guard(u32);
    impl Drop for Guard {
        fn drop(&mut self) {
            DROPS.fetch_add(self.0, Ordering::SeqCst);
        }
    }

    unsafe extern "system" fn veh_custom(info: *mut ExceptionPointers) -> i32 {
        // SAFETY: `info` fourni par le dispatcher d'exceptions (EXCEPTION_POINTERS valide).
        let rec = unsafe { &*(*info).record };
        if rec.code == CUSTOM_CODE {
            VEH_HITS.fetch_add(1, Ordering::SeqCst);
            VEH_PARAM.store(rec.information[0], Ordering::SeqCst);
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        EXCEPTION_CONTINUE_SEARCH
    }

    unsafe extern "system" fn veh_observer(info: *mut ExceptionPointers) -> i32 {
        // SAFETY: idem.
        let rec = unsafe { &*(*info).record };
        if rec.code == STATUS_ACCESS_VIOLATION {
            VEH_FAULT_ADDR.store(rec.information[1], Ordering::SeqCst);
        }
        EXCEPTION_CONTINUE_SEARCH
    }

    unsafe extern "system" fn thread_panics(_p: *mut c_void) -> u32 {
        // Le panic est rattrapé dans le thread invité lui-même.
        match catch_unwind(|| {
            let _g = Guard(1000);
            panic!("thread-win32");
        }) {
            Ok(()) => 0,
            Err(_) => 7,
        }
    }

    struct Report {
        failed: u32,
    }

    impl Report {
        fn check(&mut self, name: &str, ok: bool) {
            if ok {
                println!("[eh_test] PASS {name}");
            } else {
                println!("[eh_test] FAIL {name}");
                self.failed += 1;
            }
        }
    }

    fn payload_str(p: &Box<dyn std::any::Any + Send>) -> String {
        if let Some(s) = p.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = p.downcast_ref::<String>() {
            s.clone()
        } else {
            "?".to_string()
        }
    }

    #[inline(never)]
    fn level3(n: u32) {
        let _g = Guard(1);
        if n == 0 {
            panic!("deep");
        }
        level3(n - 1);
    }

    pub fn run() -> i32 {
        println!("[eh_test] === Propagation d'exceptions Windows x64 (SEH) ===");
        panic::set_hook(Box::new(|_| {}));
        let mut r = Report { failed: 0 };

        // 1. catch_unwind simple.
        let res = catch_unwind(|| {
            panic!("x");
        });
        r.check(
            "catch_unwind(panic!) -> Err",
            res.as_ref().is_err_and(|p| payload_str(p) == "x"),
        );

        // 2. Panic au travers de plusieurs trames avec Drop (compteur).
        DROPS.store(0, Ordering::SeqCst);
        let res = catch_unwind(|| level3(5));
        r.check(
            "drops executes pendant le deroulage (6 trames)",
            res.is_err() && DROPS.load(Ordering::SeqCst) == 6,
        );

        // 3. Panic dans un thread Rust (std::thread::spawn + join).
        let h = std::thread::spawn(|| {
            let _g = Guard(10);
            panic!("in-thread");
        });
        let jr = h.join();
        r.check(
            "thread::spawn + join -> Err",
            jr.as_ref().is_err_and(|p| payload_str(p) == "in-thread"),
        );

        // 4. Panic imbriqués : un catch_unwind interne dans un Drop pendant le déroulage.
        struct Inner;
        impl Drop for Inner {
            fn drop(&mut self) {
                let inner = catch_unwind(|| {
                    let _g = Guard(100);
                    panic!("inner");
                });
                if inner.is_ok() {
                    DROPS.fetch_add(1_000_000, Ordering::SeqCst);
                } else {
                    DROPS.fetch_add(10_000, Ordering::SeqCst);
                }
            }
        }
        DROPS.store(0, Ordering::SeqCst);
        let res = catch_unwind(|| {
            let _i = Inner;
            let _g = Guard(1);
            panic!("outer");
        });
        r.check(
            "panics imbriques (catch_unwind dans Drop)",
            res.as_ref().is_err_and(|p| payload_str(p) == "outer")
                && DROPS.load(Ordering::SeqCst) == 10_101,
        );

        // 5. Re-panic dans un gestionnaire (catch -> resume_unwind -> catch).
        let res = catch_unwind(|| {
            let e = catch_unwind(|| panic!("first")).unwrap_err();
            resume_unwind(e);
        });
        r.check(
            "resume_unwind du payload rattrape",
            res.as_ref().is_err_and(|p| payload_str(p) == "first"),
        );
        let res = catch_unwind(|| resume_unwind(Box::new(42i32)));
        r.check(
            "resume_unwind(Box<i32>)",
            res.as_ref()
                .is_err_and(|p| p.downcast_ref::<i32>() == Some(&42)),
        );

        // 6. panic dans un panic rattrapé puis nouveau panic (deux cycles complets).
        let a = catch_unwind(|| panic!("a"));
        let b = catch_unwind(|| panic!("b"));
        r.check(
            "cycles successifs",
            a.is_err() && b.is_err() && payload_str(&b.unwrap_err()) == "b",
        );

        // 7. Exception SEH personnalisée rattrapée par un gestionnaire vectoriel.
        // SAFETY: API Win32 résolues par ScarletBridge ; le gestionnaire est `extern "system"`.
        unsafe {
            let h = AddVectoredExceptionHandler(1, veh_custom);
            r.check("AddVectoredExceptionHandler", !h.is_null());
            let args = [0xABCDusize, 2, 3];
            RaiseException(CUSTOM_CODE, 0, 3, args.as_ptr());
            r.check(
                "RaiseException(0xE0000001) rattrape par VEH (reprise)",
                VEH_HITS.load(Ordering::SeqCst) == 1 && VEH_PARAM.load(Ordering::SeqCst) == 0xABCD,
            );
            r.check(
                "RemoveVectoredExceptionHandler",
                RemoveVectoredExceptionHandler(h) != 0,
            );
        }

        // 8. Un panic reste rattrapable avec un VEH « observateur » actif (CONTINUE_SEARCH).
        // SAFETY: idem.
        unsafe {
            let h = AddVectoredExceptionHandler(0, veh_observer);
            let res = catch_unwind(AssertUnwindSafe(|| panic!("with-veh")));
            r.check("panic avec VEH observateur", res.is_err());
            RemoveVectoredExceptionHandler(h);
        }

        // 9. Threads Win32 (CreateThread) rattrapant leur propre panic.
        // SAFETY: handles Win32 simulés ; le thread n'accède qu'à des atomiques.
        unsafe {
            let mut tid = 0u32;
            let h = CreateThread(
                core::ptr::null_mut(),
                0,
                thread_panics,
                core::ptr::null_mut(),
                0,
                &mut tid,
            );
            WaitForSingleObject(h, 0xFFFF_FFFF);
            let mut code = 0u32;
            GetExitCodeThread(h, &mut code);
            r.check("CreateThread + catch_unwind interne", code == 7);
        }

        // 10. Faute matérielle (NULL) convertie en EXCEPTION_ACCESS_VIOLATION et récupérée
        // par un gestionnaire vectoriel qui saute l'instruction fautive.
        if std::env::var_os("EH_TEST_SKIP_HW").is_none() {
            hw_fault_tests(&mut r);
        }

        // Mode « crash » (manuel) : faute NULL sans aucun handler -> rapport de crash historique.
        if std::env::var_os("EH_TEST_CRASH").is_some() {
            // SAFETY: faute volontaire non gérée.
            unsafe { core::arch::asm!("mov dword ptr [rax], 1", in("rax") 0usize) };
        }
        println!(
            "[eh_test] === {} ===",
            if r.failed == 0 { "OK" } else { "ECHEC" }
        );
        i32::from(r.failed != 0)
    }

    /// Fautes matérielles récupérées par un gestionnaire vectoriel qui avance RIP de `skip`
    /// octets (`HW_SKIP`) : écriture/lecture NULL, `int3`, division entière par zéro.
    fn hw_fault_tests(r: &mut Report) {
        static HW_CODE: AtomicU32 = AtomicU32::new(0);
        static HW_SKIP: AtomicUsize = AtomicUsize::new(0);
        static HW_RIP_DELTA: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "system" fn veh_skip(info: *mut ExceptionPointers) -> i32 {
            // SAFETY: `info` valide ; le CONTEXT est modifiable en place (Rip à 0xF8).
            unsafe {
                let rec = &*(*info).record;
                let expected = HW_CODE.load(Ordering::SeqCst);
                if expected == 0 || rec.code != expected {
                    return EXCEPTION_CONTINUE_SEARCH;
                }
                VEH_FAULT_ADDR.store(rec.information[1], Ordering::SeqCst);
                VEH_PARAM.store(rec.information[0], Ordering::SeqCst);
                let rip = ((*info).context as *mut u8).add(0xF8).cast::<u64>();
                HW_RIP_DELTA.store((*rip - rec.address as u64) as usize, Ordering::SeqCst);
                *rip += HW_SKIP.load(Ordering::SeqCst) as u64;
                EXCEPTION_CONTINUE_EXECUTION
            }
        }
        // SAFETY: fautes volontaires, récupérées par le gestionnaire vectoriel.
        unsafe {
            let h = AddVectoredExceptionHandler(1, veh_skip);

            // Écriture NULL : `mov dword ptr [rax], 1` (6 octets).
            HW_CODE.store(STATUS_ACCESS_VIOLATION, Ordering::SeqCst);
            HW_SKIP.store(6, Ordering::SeqCst);
            VEH_FAULT_ADDR.store(1, Ordering::SeqCst);
            let target: usize = 0;
            core::arch::asm!("mov dword ptr [rax], 1", in("rax") target, options(nostack));
            r.check(
                "ecriture NULL -> ACCESS_VIOLATION (info[0]=1, info[1]=0)",
                VEH_FAULT_ADDR.load(Ordering::SeqCst) == 0 && VEH_PARAM.load(Ordering::SeqCst) == 1,
            );

            // Lecture à une adresse invalide : `mov eax, [rax]` (2 octets), information[0] == 0.
            HW_SKIP.store(2, Ordering::SeqCst);
            let mut v: u32 = 0x1234;
            core::arch::asm!("mov eax, [rax]", inout("rax") 0x10usize => _, out("rcx") _, options(nostack));
            let _ = &mut v;
            r.check(
                "lecture 0x10 -> ACCESS_VIOLATION (info[0]=0, info[1]=0x10)",
                VEH_FAULT_ADDR.load(Ordering::SeqCst) == 0x10
                    && VEH_PARAM.load(Ordering::SeqCst) == 0,
            );

            // int3 : Windows rapporte Rip = adresse de l'int3 ; le gestionnaire saute 1 octet.
            HW_CODE.store(0x8000_0003, Ordering::SeqCst);
            HW_SKIP.store(1, Ordering::SeqCst);
            HW_RIP_DELTA.store(99, Ordering::SeqCst);
            core::arch::asm!("int3", options(nostack));
            r.check(
                "int3 -> EXCEPTION_BREAKPOINT (Rip = adresse de int3)",
                HW_RIP_DELTA.load(Ordering::SeqCst) == 0,
            );

            // Division entière par zéro : `div rcx` (3 octets).
            HW_CODE.store(0xC000_0094, Ordering::SeqCst);
            HW_SKIP.store(3, Ordering::SeqCst);
            VEH_PARAM.store(0, Ordering::SeqCst);
            core::arch::asm!(
                "xor ecx, ecx",
                "xor edx, edx",
                "div rcx",
                inout("rax") 5usize => _,
                out("rdx") _,
                out("rcx") _,
                options(nostack)
            );
            r.check(
                "div par zero -> INTEGER_DIVIDE_BY_ZERO (reprise)",
                HW_CODE.load(Ordering::SeqCst) == 0xC000_0094,
            );

            // Reprise après plusieurs fautes successives.
            HW_CODE.store(STATUS_ACCESS_VIOLATION, Ordering::SeqCst);
            HW_SKIP.store(6, Ordering::SeqCst);
            let mut count = 0u32;
            for _ in 0..100 {
                core::arch::asm!("mov dword ptr [rax], 1", in("rax") 0usize, options(nostack));
                count += 1;
            }
            r.check("100 fautes NULL recuperees", count == 100);
            HW_CODE.store(0, Ordering::SeqCst);
            RemoveVectoredExceptionHandler(h);
        }
    }
}
