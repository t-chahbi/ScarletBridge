//! Tests du noyau SEH : image synthétique en mémoire exécutable avec `.pdata`, UNWIND_INFO et
//! `C_SCOPE_TABLE` écrits à la main, exécutée nativement.
//!
//! Disposition de l'image (RVA) :
//! - `0x010` thunk `mov rax, __C_specific_handler ; jmp rax` (le handler d'UNWIND_INFO est un RVA) ;
//! - `0x100` `INNER` : `__try { RaiseException(code) } __finally { flag = 1; abn = cl }` ;
//! - `0x300` `OUTER` : `__try { INNER() } __except(code == 0xE0000001) { return 2 }`, sinon 1 ;
//! - `0x340` filtre ; `0x500` `.pdata` ; `0x800..` données (adresse de `RaiseException`, code, flags).

use super::*;
use sb_loader::unwind::register_function_table;

const SLOT_RAISE: usize = 0x800;
const SLOT_FINALLY: usize = 0x810;
const SLOT_ABNORMAL: usize = 0x818;
const SLOT_CODE: usize = 0x820;

struct TestImage {
    base: usize,
}

impl TestImage {
    fn put(&self, rva: usize, bytes: &[u8]) {
        // SAFETY: `rva + len` reste dans la page RWX allouée (0x1000 octets).
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                (self.base + rva) as *mut u8,
                bytes.len(),
            )
        };
    }
    fn put_u32(&self, rva: usize, v: u32) {
        self.put(rva, &v.to_le_bytes());
    }
    fn read_u32(&self, rva: usize) -> u32 {
        // SAFETY: lecture dans la page allouée.
        unsafe { core::ptr::read_unaligned((self.base + rva) as *const u32) }
    }
    fn func<T>(&self, rva: usize) -> T {
        // SAFETY: l'appelant choisit un type `extern "win64" fn` conforme au code écrit.
        unsafe { core::mem::transmute_copy::<usize, T>(&(self.base + rva)) }
    }
}

fn rel32(from_end: usize, target: usize) -> [u8; 4] {
    (target as i64 - from_end as i64).to_le_bytes()[..4]
        .try_into()
        .unwrap_or([0; 4])
}

/// Construit l'image de test et enregistre sa table `.pdata`.
fn build_image() -> TestImage {
    // SAFETY: mapping anonyme RWX d'une page.
    let p = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            0x1000,
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(p, libc::MAP_FAILED);
    let img = TestImage { base: p as usize };
    img.put(0, &[0xCC; 0x10]);

    // thunk vers __C_specific_handler
    let mut thunk = vec![0x48, 0xB8];
    thunk.extend_from_slice(&(__C_specific_handler as *const () as usize as u64).to_le_bytes());
    thunk.extend_from_slice(&[0xFF, 0xE0]);
    img.put(0x10, &thunk);

    // INNER
    let mut inner = vec![0x48, 0x83, 0xEC, 0x28]; // 100 sub rsp,0x28
    inner.extend_from_slice(&[0x8B, 0x0D]); // 104 mov ecx,[rip+code]
    inner.extend_from_slice(&rel32(0x10A, SLOT_CODE));
    inner.extend_from_slice(&[0x31, 0xD2, 0x45, 0x31, 0xC0, 0x45, 0x31, 0xC9]); // 10A..111
    inner.extend_from_slice(&[0xFF, 0x15]); // 112 call [rip+raise]
    inner.extend_from_slice(&rel32(0x118, SLOT_RAISE));
    inner.push(0x90); // 118 nop
    inner.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28, 0xC3]); // 119 épilogue
    img.put(0x100, &inner);
    // __finally : flag = 1 ; abn = ecx ; ret
    let mut fin = vec![0xC7, 0x05];
    fin.extend_from_slice(&rel32(0x180 + 10, SLOT_FINALLY));
    fin.extend_from_slice(&1u32.to_le_bytes());
    fin.extend_from_slice(&[0x89, 0x0D]);
    fin.extend_from_slice(&rel32(0x180 + 10 + 6, SLOT_ABNORMAL));
    fin.push(0xC3);
    img.put(0x180, &fin);
    // UNWIND_INFO INNER @0x200 : EHANDLER|UHANDLER, prologue 4, ALLOC_SMALL 0x28
    img.put(0x200, &[0x19, 4, 1, 0, 4, 0x42, 0, 0]);
    img.put_u32(0x208, 0x10); // handler RVA
    img.put_u32(0x20C, 1); // scope count
    for (i, v) in [0x104u32, 0x119, 0x180, 0].iter().enumerate() {
        img.put_u32(0x210 + i * 4, *v);
    }

    // OUTER
    let mut outer = vec![0x48, 0x83, 0xEC, 0x28]; // 300
    outer.push(0xE8); // 304 call INNER
    outer.extend_from_slice(&rel32(0x309, 0x100));
    outer.push(0x90); // 309
    outer.extend_from_slice(&[0xB8, 1, 0, 0, 0, 0x48, 0x83, 0xC4, 0x28, 0xC3]); // 30A
    img.put(0x300, &outer);
    img.put(0x320, &[0xB8, 2, 0, 0, 0, 0x48, 0x83, 0xC4, 0x28, 0xC3]); // __except
    // filtre @0x340 : return ((EXCEPTION_POINTERS*)rcx)->rec->code == 0xE0000001
    img.put(
        0x340,
        &[
            0x48, 0x8B, 0x01, 0x81, 0x38, 0x01, 0x00, 0x00, 0xE0, 0x0F, 0x94, 0xC0, 0x0F, 0xB6,
            0xC0, 0xC3,
        ],
    );
    img.put(0x400, &[0x19, 4, 1, 0, 4, 0x42, 0, 0]);
    img.put_u32(0x408, 0x10);
    img.put_u32(0x40C, 1);
    for (i, v) in [0x304u32, 0x30A, 0x340, 0x320].iter().enumerate() {
        img.put_u32(0x410 + i * 4, *v);
    }

    // .pdata
    for (i, v) in [0x100u32, 0x11E, 0x200, 0x300, 0x32A, 0x400]
        .iter()
        .enumerate()
    {
        img.put_u32(0x500 + i * 4, *v);
    }
    img.put(
        SLOT_RAISE,
        &(RaiseException as *const () as usize as u64).to_le_bytes(),
    );
    // SAFETY: table triée de 2 RUNTIME_FUNCTION dans l'image, valide tant que l'image vit.
    unsafe {
        register_function_table(
            img.base,
            0x1000,
            (img.base + 0x500) as *const RuntimeFunction,
            2,
        );
    }
    img
}

#[test]
fn try_except_with_finally_unwinds_and_runs_termination() {
    let img = build_image();
    img.put_u32(SLOT_CODE, 0xE000_0001);
    let outer: extern "win64" fn() -> i32 = img.func(0x300);
    let ret = outer();
    assert_eq!(ret, 2, "le __except doit reprendre au JumpTarget");
    assert_eq!(
        img.read_u32(SLOT_FINALLY),
        1,
        "__finally exécuté pendant le déroulage"
    );
    assert_eq!(
        img.read_u32(SLOT_ABNORMAL) & 0xFF,
        1,
        "AbnormalTermination = TRUE"
    );
}

#[test]
fn repeated_raise_and_recovery_is_stable() {
    let img = build_image();
    img.put_u32(SLOT_CODE, 0xE000_0001);
    let outer: extern "win64" fn() -> i32 = img.func(0x300);
    for _ in 0..50 {
        assert_eq!(outer(), 2);
    }
}

unsafe extern "win64" fn veh_continue(info: *mut ExceptionPointers) -> i32 {
    // SAFETY: EXCEPTION_POINTERS valide fourni par le dispatcher.
    let code = unsafe { (*(*info).exception_record).exception_code };
    if code == 0xE000_0077 {
        EXCEPTION_CONTINUE_EXECUTION
    } else {
        EXCEPTION_CONTINUE_SEARCH
    }
}

#[test]
fn vectored_handler_resumes_execution_without_unwinding() {
    let img = build_image();
    img.put_u32(SLOT_CODE, 0xE000_0077);
    let h = AddVectoredExceptionHandler(1, veh_continue as *const () as usize);
    assert_ne!(h, 0);
    let inner: extern "win64" fn() = img.func(0x100);
    inner();
    assert_eq!(
        img.read_u32(SLOT_FINALLY),
        0,
        "pas de déroulage : __finally non exécuté"
    );
    assert_eq!(RemoveVectoredExceptionHandler(h), 1);
    assert_eq!(RemoveVectoredExceptionHandler(h), 0);
}

#[test]
fn vectored_handle_semantics() {
    assert_eq!(AddVectoredContinueHandler(0, 0), 0);
    let a = AddVectoredContinueHandler(0, veh_continue as *const () as usize);
    let b = AddVectoredContinueHandler(1, veh_continue as *const () as usize);
    assert!(a != 0 && b != 0 && a != b);
    assert_eq!(RemoveVectoredContinueHandler(a), 1);
    assert_eq!(RemoveVectoredContinueHandler(b), 1);
    assert_eq!(RemoveVectoredContinueHandler(b), 0);
}

#[test]
fn raise_exception_copies_at_most_15_parameters() {
    static SEEN: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "win64" fn veh(info: *mut ExceptionPointers) -> i32 {
        // SAFETY: EXCEPTION_POINTERS valide.
        let r = unsafe { &*(*info).exception_record };
        if r.exception_code == 0xE000_0078 {
            SEEN.store(
                r.number_parameters as usize * 1000 + r.exception_information[14],
                Ordering::SeqCst,
            );
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        EXCEPTION_CONTINUE_SEARCH
    }
    let h = AddVectoredExceptionHandler(1, veh as *const () as usize);
    let args: Vec<usize> = (0..20).collect();
    RaiseException(0xE000_0078, 0, 20, args.as_ptr());
    assert_eq!(SEEN.load(Ordering::SeqCst), 15_014);
    RemoveVectoredExceptionHandler(h);
}

#[test]
fn context_restore_roundtrip_layout() {
    assert_eq!(size_of::<ExceptionRecord>(), 152);
    assert_eq!(size_of::<DispatcherContext>(), 0x50);
}

/// Processus enfant : exception non gérée (le filtre du __except refuse le code).
#[test]
fn unhandled_child() {
    if std::env::var_os("SB_TEST_UNHANDLED_CHILD").is_none() {
        return;
    }
    let img = build_image();
    img.put_u32(SLOT_CODE, 0xE000_0002);
    let outer: extern "win64" fn() -> i32 = img.func(0x300);
    std::hint::black_box(outer());
}

#[test]
fn unhandled_exception_reports_and_terminates() {
    let exe = std::env::current_exe().expect("exe de test");
    let out = std::process::Command::new(exe)
        .args([
            "--exact",
            "exceptions::tests::unhandled_child",
            "--nocapture",
        ])
        .env("SB_TEST_UNHANDLED_CHILD", "1")
        .output()
        .expect("lancement de l'enfant");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("UNHANDLED EXCEPTION"), "{stderr}");
    assert!(stderr.contains("0xe0000002"), "{stderr}");
}
