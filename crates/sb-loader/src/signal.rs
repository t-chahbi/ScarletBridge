//! Gestionnaire de signaux Linux (`SIGSEGV` / `SIGILL` / `SIGBUS` / `SIGFPE` / `SIGTRAP`) affichant
//! l'offset exact `module+0xRVA` dans les PE32+ chargés en cas de crash invité, avec les registres,
//! les octets de code autour de RIP, le sommet de pile et la pile d'appels invitée (tables
//! `.pdata`).

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::io::Write;

use crate::pe_loader::module_manager;
use crate::trampoline::describe_trampoline;

/// `si_code` Linux pour une faute générée par le noyau (#GP, `int n` interdit, ...).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SI_KERNEL: i32 = 0x80;

/// Code de sortie Windows d'un `__fastfail`.
pub const STATUS_STACK_BUFFER_OVERRUN: u32 = 0xC000_0409;

/// Vrai si les octets désignent l'instruction `int 0x29` (`__fastfail`).
pub const fn is_fastfail_instruction(bytes: [u8; 2]) -> bool {
    bytes[0] == 0xCD && bytes[1] == 0x29
}

/// Indique si `__fastfail` (`int 0x29`) est intercepté proprement (handler SIGSEGV dédié).
pub const fn fastfail_supported() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}

/// Installe les handlers de diagnostic pour `SIGSEGV`, `SIGILL`, `SIGBUS`, `SIGFPE` et `SIGTRAP`
/// (`int3` / `__debugbreak` invité : `EXCEPTION_BREAKPOINT`).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn install_crash_handlers() {
    ensure_thread_altstack();
    // SAFETY: `sigaction` avec handler `SA_SIGINFO` conforme POSIX.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
        sa.sa_sigaction = fault_handler as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGSEGV, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGILL, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGBUS, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGFPE, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTRAP, &sa, std::ptr::null_mut());
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub fn install_crash_handlers() {}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
extern "C" fn fault_handler(
    sig: libc::c_int,
    info: *mut libc::siginfo_t,
    ucontext: *mut libc::c_void,
) {
    // SAFETY: `info` et `ucontext` sont fournis par le noyau Linux pour `SA_SIGINFO`.
    let fault_addr = if !info.is_null() {
        unsafe { (*info).si_addr() as usize }
    } else {
        0
    };

    let (rip, rsp, rax, rcx, rdx, r8, r9) = if !ucontext.is_null() {
        // SAFETY: cast standard Linux x86-64 `ucontext_t`.
        let ctx = unsafe { &*(ucontext as *const libc::ucontext_t) };
        let regs = &ctx.uc_mcontext.gregs;
        (
            regs[libc::REG_RIP as usize] as usize,
            regs[libc::REG_RSP as usize] as usize,
            regs[libc::REG_RAX as usize] as usize,
            regs[libc::REG_RCX as usize] as usize,
            regs[libc::REG_RDX as usize] as usize,
            regs[libc::REG_R8 as usize] as usize,
            regs[libc::REG_R9 as usize] as usize,
        )
    } else {
        (0, 0, 0, 0, 0, 0, 0)
    };

    // `__fastfail(code)` : `int 0x29` (CD 29) déclenche un #GP en mode utilisateur Linux
    // (SIGSEGV, `si_code == SI_KERNEL`, `si_addr == 0`). Windows termine alors le processus
    // immédiatement avec `STATUS_STACK_BUFFER_OVERRUN`, sans exécuter aucun handler.
    let si_code = if info.is_null() {
        0
    } else {
        // SAFETY: `info` fourni par le noyau pour `SA_SIGINFO`.
        unsafe { (*info).si_code }
    };
    if sig == libc::SIGSEGV && si_code == SI_KERNEL && fault_addr == 0 && rip != 0 {
        // SAFETY: un #GP sur `int` implique que RIP pointe vers du code exécutable mappé.
        let bytes = unsafe { std::ptr::read_unaligned(rip as *const [u8; 2]) };
        if is_fastfail_instruction(bytes) {
            let location = module_manager()
                .describe_address(rip)
                .unwrap_or_else(|| "<hors module PE>".to_string());
            let msg = format!(
                "\n[ScarletBridge] __fastfail(code={rcx}) à {rip:#018x} ({location}) : \
                 terminaison immédiate (STATUS_STACK_BUFFER_OVERRUN 0xC0000409)\n"
            );
            let _ = std::io::stderr().write_all(msg.as_bytes());
            let _ = std::io::stderr().flush();
            // SAFETY: terminaison immédiate, comme `__fastfail` sous Windows.
            unsafe { libc::_exit(STATUS_STACK_BUFFER_OVERRUN as i32) };
        }
    }

    // Faute dans du code invité : exception SEH (EXCEPTION_RECORD + CONTEXT) dispatchée sur la
    // pile invitée ; le rapport ci-dessous n'est produit que si personne ne la gère.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if !ucontext.is_null() && try_redirect_to_seh(sig, info, ucontext, rip, rsp, fault_addr) {
        return;
    }

    // SAFETY: rétablit l'action par défaut (équivalent de `SA_RESETHAND`) : une faute dans le
    // rapport lui-même termine le processus au lieu de boucler.
    unsafe { libc::signal(sig, libc::SIG_DFL) };
    let tail = |msg: &mut String| {
        if !ucontext.is_null() {
            append_detailed_state(msg, ucontext);
        }
    };
    fatal_fault_report(sig, fault_addr, [rip, rsp, rax, rcx, rdx, r8, r9], tail)
}

/// Rapport de crash « historique » (signal, `module+RVA`, registres, pile, backtrace) puis
/// `_exit(128 + sig)`. `regs` = `[rip, rsp, rax, rcx, rdx, r8, r9]`.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn fatal_fault_report(
    sig: i32,
    fault_addr: usize,
    regs: [usize; 7],
    detail: impl FnOnce(&mut String),
) -> ! {
    let [rip, rsp, rax, rcx, rdx, r8, r9] = regs;
    let sig_name = match sig {
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGILL => "SIGILL",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGTRAP => "SIGTRAP (int3 invité)",
        _ => "SIGNAL",
    };
    let location = module_manager()
        .describe_address(rip)
        .or_else(|| describe_trampoline(rip))
        .or_else(|| describe_host_address(rip).map(|h| format!("hôte : {h}")))
        .unwrap_or_else(|| "<hors module PE>".to_string());

    let mut msg = format!(
        "\n==================== [ScarletBridge FAULT] ====================\n\
         Signal   : {sig_name} ({sig})\n\
         RIP      : {rip:#018x} ({location})\n\
         FaultAddr: {fault_addr:#018x}\n\
         RSP      : {rsp:#018x}   RAX: {rax:#018x}\n\
         RCX (a1) : {rcx:#018x}   RDX (a2): {rdx:#018x}\n\
         R8  (a3) : {r8:#018x}   R9  (a4): {r9:#018x}\n"
    );
    if sig == libc::SIGTRAP {
        msg.push_str(
            "Note     : `int3` exécuté par le code invité (EXCEPTION_BREAKPOINT 0x80000003). \
             RIP pointe après l'instruction ; les assertions / abort de runtime C++ finissent ainsi.\n",
        );
    }
    detail(&mut msg);
    msg.push_str("===============================================================\n");
    let _ = std::io::stderr().write_all(msg.as_bytes());
    let _ = std::io::stderr().flush();

    // SAFETY: terminer proprement avec le code signal standard (128 + sig).
    unsafe {
        libc::_exit(128 + sig);
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod seh;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use seh::ensure_thread_altstack;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use seh::try_redirect_to_seh;

/// Pile alternative de signaux (sans effet hors Linux x86-64).
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub fn ensure_thread_altstack() {}

/// Dispatcher d'exceptions invité (`RtlDispatchException` de `sb-hle`), enregistré au démarrage.
///
/// Reçoit un `EXCEPTION_RECORD*` et un `CONTEXT*` ; retourne `true` si l'exécution doit reprendre
/// avec le contexte (éventuellement modifié), `false` si personne n'a géré l'exception. Si un
/// handler déroule la pile, la fonction ne revient pas.
pub type GuestExceptionDispatcher =
    unsafe extern "win64" fn(*mut u8, *mut crate::unwind::Context) -> bool;

static GUEST_DISPATCHER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Enregistre le dispatcher d'exceptions invité (appelé par `sb-hle`).
pub fn set_exception_dispatcher(f: GuestExceptionDispatcher) {
    GUEST_DISPATCHER.store(f as usize, std::sync::atomic::Ordering::Release);
}

/// Dispatcher enregistré, s'il existe.
pub fn exception_dispatcher() -> Option<GuestExceptionDispatcher> {
    let v = GUEST_DISPATCHER.load(std::sync::atomic::Ordering::Acquire);
    if v == 0 {
        None
    } else {
        // SAFETY: la valeur provient de `set_exception_dispatcher`.
        Some(unsafe { std::mem::transmute::<usize, GuestExceptionDispatcher>(v) })
    }
}

/// Termine immédiatement le processus avec `code` (tampons standard vidés).
pub fn exit_process_now(code: u32) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // SAFETY: terminaison immédiate du processus.
    unsafe { libc::_exit(code as i32) }
}

/// Écrit le rapport d'une exception SEH invitée non gérée (code, `module+RVA`, registres, pile,
/// backtrace `.pdata`) puis termine le processus avec le code de l'exception.
pub fn exit_with_guest_exception_report(
    code: u32,
    flags: u32,
    address: usize,
    info: &[usize],
    ctx: &crate::unwind::Context,
) -> ! {
    let location = module_manager()
        .describe_address(ctx.rip as usize)
        .unwrap_or_else(|| "<hors module PE>".to_string());
    let mut msg = format!(
        "\n==================== [ScarletBridge UNHANDLED EXCEPTION] ====================\n\
         Code     : {code:#010x} (flags {flags:#x})\n\
         Adresse  : {address:#018x}\n\
         RIP      : {:#018x} ({location})\n\
         RSP      : {:#018x}   RAX: {:#018x}\n",
        ctx.rip, ctx.rsp, ctx.rax
    );
    for (i, v) in info.iter().enumerate() {
        msg.push_str(&format!("Param[{i}] : {v:#018x}\n"));
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    append_detailed_regs(&mut msg, &regs_from_context(ctx));
    msg.push_str("===============================================================\n");
    let _ = std::io::stderr().write_all(msg.as_bytes());
    exit_process_now(code)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn regs_from_context(c: &crate::unwind::Context) -> Regs {
    Regs {
        rip: c.rip,
        rsp: c.rsp,
        rax: c.rax,
        rbx: c.rbx,
        rbp: c.rbp,
        rsi: c.rsi,
        rdi: c.rdi,
        r10: c.r10,
        r11: c.r11,
        r12: c.r12,
        r13: c.r13,
        r14: c.r14,
        r15: c.r15,
        efl: u64::from(c.eflags),
    }
}

/// Décrit une adresse du code **hôte** (bibliothèque partagée ou binaire) via `dladdr` :
/// `libSDL3.so.0+0x1a2b3c (SDL_Vulkan_GetInstanceExtensions+0x1e)`. `None` si l'adresse n'est dans
/// aucun objet chargé (ou si le symbole et le fichier sont inconnus).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn describe_host_address(addr: usize) -> Option<String> {
    // SAFETY: `Dl_info` est une structure de sortie POD ; `dladdr` n'écrit que dedans et ne
    // déréférence pas `addr`.
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // SAFETY: `info` est valide en écriture.
    if unsafe { libc::dladdr(addr as *const libc::c_void, &mut info) } == 0
        || info.dli_fname.is_null()
    {
        return None;
    }
    // SAFETY: `dli_fname` est une chaîne C terminée par NUL fournie par le chargeur dynamique.
    let path = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }.to_string_lossy();
    let file = path.rsplit('/').next().unwrap_or(&path);
    let offset = addr.wrapping_sub(info.dli_fbase as usize);
    let mut out = format!("{file}+{offset:#x}");
    if !info.dli_sname.is_null() {
        // SAFETY: `dli_sname` est une chaîne C terminée par NUL.
        let name = unsafe { std::ffi::CStr::from_ptr(info.dli_sname) }.to_string_lossy();
        let sym_off = addr.wrapping_sub(info.dli_saddr as usize);
        out.push_str(&format!(" ({name}+{sym_off:#x})"));
    }
    Some(out)
}

/// Ajoute au rapport : registres restants, octets de code autour de RIP, sommet de pile et pile
/// d'appels invitée. Tout est lu depuis la mémoire du processus ; les lectures ne sont tentées que
/// si l'adresse appartient à une image PE chargée ou à une pile plausible.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn append_detailed_state(msg: &mut String, ucontext: *mut libc::c_void) {
    // SAFETY: `ucontext` fourni par le noyau Linux pour `SA_SIGINFO`.
    let ctx = unsafe { &*(ucontext as *const libc::ucontext_t) };
    let g = &ctx.uc_mcontext.gregs;
    let r = |n: i32| g[n as usize] as u64;
    let regs = Regs {
        rip: r(libc::REG_RIP),
        rsp: r(libc::REG_RSP),
        rax: r(libc::REG_RAX),
        rbx: r(libc::REG_RBX),
        rbp: r(libc::REG_RBP),
        rsi: r(libc::REG_RSI),
        rdi: r(libc::REG_RDI),
        r10: r(libc::REG_R10),
        r11: r(libc::REG_R11),
        r12: r(libc::REG_R12),
        r13: r(libc::REG_R13),
        r14: r(libc::REG_R14),
        r15: r(libc::REG_R15),
        efl: r(libc::REG_EFL),
    };
    append_detailed_regs(msg, &regs);
}

/// Registres nécessaires au rapport détaillé (issus d'un `ucontext_t` ou d'un `CONTEXT`).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
struct Regs {
    rip: u64,
    rsp: u64,
    rax: u64,
    rbx: u64,
    rbp: u64,
    rsi: u64,
    rdi: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    efl: u64,
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn append_detailed_regs(msg: &mut String, regs: &Regs) {
    use std::fmt::Write as _;
    let _ = writeln!(
        msg,
        "RBX {:#018x}  RBP {:#018x}  RSI {:#018x}  RDI {:#018x}\n\
         R10 {:#018x}  R11 {:#018x}  R12 {:#018x}  R13 {:#018x}\n\
         R14 {:#018x}  R15 {:#018x}  EFL {:#010x}",
        regs.rbx,
        regs.rbp,
        regs.rsi,
        regs.rdi,
        regs.r10,
        regs.r11,
        regs.r12,
        regs.r13,
        regs.r14,
        regs.r15,
        regs.efl,
    );
    let r = regs;
    let rip = regs.rip as usize;
    let rsp = regs.rsp as usize;
    let in_image = |a: usize| crate::unwind::image_base_for_pc(a).is_some();

    if in_image(rip) && in_image(rip.saturating_sub(16)) {
        // SAFETY: 32 octets autour de RIP, dans une image PE mappée lisible.
        let code = unsafe { std::ptr::read_unaligned((rip - 16) as *const [u8; 32]) };
        let _ = write!(msg, "Code     : (RIP-16) ");
        for (i, b) in code.iter().enumerate() {
            if i == 16 {
                let _ = write!(msg, "| ");
            }
            let _ = write!(msg, "{b:02x} ");
        }
        let _ = writeln!(msg);
    }

    if rsp != 0 && rsp.is_multiple_of(8) {
        let _ = writeln!(msg, "Pile     : (RSP+0x00..)");
        for i in 0..8usize {
            // SAFETY: sommet de pile du thread fautif (le noyau l'a mappée) ; 8 qwords.
            let v = unsafe { std::ptr::read_unaligned((rsp + i * 8) as *const u64) } as usize;
            let loc = module_manager()
                .describe_address(v)
                .or_else(|| describe_host_address(v).map(|h| format!("hôte : {h}")))
                .map(|l| format!("  <- {l}"))
                .unwrap_or_default();
            let _ = writeln!(msg, "  [rsp+{:#04x}] {v:#018x}{loc}", i * 8);
        }
    }

    // Trame hôte (RIP hors image PE) : la pile `.pdata` est inutilisable ; on balaie les 96 premiers
    // mots de pile et on affiche ceux qui ressemblent à une adresse de retour dans une
    // bibliothèque hôte (heuristique : peut contenir des valeurs périmées).
    if !in_image(rip) && rsp != 0 && rsp.is_multiple_of(8) {
        let mut shown = 0;
        let _ = writeln!(
            msg,
            "Pile hôte (heuristique, adresses de retour plausibles) :"
        );
        for i in 0..96usize {
            // SAFETY: balayage de la pile du thread fautif (zone mappée par le noyau).
            let v = unsafe { std::ptr::read_unaligned((rsp + i * 8) as *const u64) } as usize;
            if let Some(h) = describe_host_address(v)
                && !h.starts_with("scarletbridge+")
            {
                let _ = writeln!(msg, "  [rsp+{:#05x}] {v:#018x}  {h}", i * 8);
                shown += 1;
                if shown >= 16 {
                    break;
                }
            }
        }
        if shown == 0 {
            let _ = writeln!(msg, "  (aucune)");
        }
    }

    // Derniers avertissements / erreurs du bridge : nomment en général la méthode non implémentée
    // ou le chemin refusé qui précède l'arrêt du titre.
    let problems = sb_common::log::recent_problems(12);
    if problems.is_empty() {
        let _ = writeln!(msg, "Avertissements récents : aucun");
    } else {
        let _ = writeln!(
            msg,
            "Derniers avertissements / erreurs du bridge ({}, le plus récent en bas) :",
            problems.len()
        );
        for p in &problems {
            let _ = writeln!(msg, "  {p}");
        }
    }

    if let Some(path) = crate::trampoline::first_fatal_path() {
        let _ = write!(msg, "{path}");
    }

    // Derniers appels d'API HLE (traceur `SB_TRACE_API`).
    let calls = crate::trampoline::recent_api_calls(420);
    if calls.is_empty() {
        let _ = writeln!(
            msg,
            "Appels API: aucun tracé (relancer avec SB_TRACE_API=1 pour voir les derniers appels)"
        );
    } else {
        let _ = writeln!(
            msg,
            "Derniers appels d'API HLE ({} ; le plus récent en bas) :",
            calls.len()
        );
        for c in &calls {
            let _ = writeln!(msg, "  {c}");
        }
    }

    // Pile d'appels invitée via les tables `.pdata` des images chargées.
    let mut uc = crate::unwind::Context {
        rip: rip as u64,
        rsp: rsp as u64,
        rbp: r.rbp,
        rbx: r.rbx,
        rsi: r.rsi,
        rdi: r.rdi,
        r12: r.r12,
        r13: r.r13,
        r14: r.r14,
        r15: r.r15,
        ..crate::unwind::Context::default()
    };
    uc.rax = r.rax;
    // SAFETY: pile du thread fautif ; le parcours s'arrête dès qu'une adresse sort des images PE.
    let frames = unsafe { crate::unwind::guest_backtrace(uc, 32) };
    if frames.is_empty() {
        let _ = writeln!(
            msg,
            "Backtrace: indisponible (RIP hors image PE : trame hôte ou trampoline)"
        );
    } else {
        let _ = writeln!(msg, "Backtrace invité ({} trames) :", frames.len());
        for (i, pc) in frames.iter().enumerate() {
            let loc = module_manager()
                .describe_address(*pc)
                .unwrap_or_else(|| "?".to_string());
            let _ = writeln!(msg, "  #{i:<2} {pc:#018x}  {loc}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_int29_fastfail() {
        assert!(is_fastfail_instruction([0xCD, 0x29]));
        assert!(!is_fastfail_instruction([0xCD, 0x2E]));
        assert!(!is_fastfail_instruction([0xCC, 0x29]));
    }

    /// Processus enfant : exécute `int3` avec les handlers installés (ne fait rien sinon).
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn trap_child() {
        if std::env::var_os("SB_TEST_TRAP_CHILD").is_none() {
            return;
        }
        install_crash_handlers();
        sb_common::sb_warn!("test", "marqueur-avertissement-avant-crash");
        // SAFETY: `int3` volontaire pour déclencher SIGTRAP dans le processus enfant.
        unsafe { core::arch::asm!("int3") };
    }

    /// Un `int3` invité produit un rapport lisible (signal, registres) et le code 128 + SIGTRAP.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn int3_prints_fault_report_and_exits_with_sigtrap_code() {
        let exe = std::env::current_exe().expect("exe de test");
        let out = std::process::Command::new(exe)
            .args(["--exact", "signal::tests::trap_child", "--nocapture"])
            .env("SB_TEST_TRAP_CHILD", "1")
            .output()
            .expect("lancement de l'enfant");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(128 + libc::SIGTRAP), "{stderr}");
        assert!(stderr.contains("[ScarletBridge FAULT]"), "{stderr}");
        assert!(stderr.contains("SIGTRAP"), "{stderr}");
        assert!(stderr.contains("EXCEPTION_BREAKPOINT"), "{stderr}");
        assert!(stderr.contains("Backtrace"), "{stderr}");
        // L'avertissement émis avant le crash est repris dans le rapport (en plus de la ligne live).
        assert!(stderr.contains("Derniers avertissements"), "{stderr}");
        assert!(
            stderr.matches("marqueur-avertissement-avant-crash").count() >= 2,
            "{stderr}"
        );
    }

    /// Processus enfant : `strlen(NULL)` dans la libc hôte (SIGSEGV hors image PE).
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn host_fault_child() {
        if std::env::var_os("SB_TEST_HOST_FAULT_CHILD").is_none() {
            return;
        }
        install_crash_handlers();
        // SAFETY: déréférencement NULL volontaire dans la libc pour provoquer SIGSEGV.
        let n = unsafe { libc::strlen(std::ptr::null()) };
        std::hint::black_box(n);
    }

    /// Une faute dans du code hôte nomme la bibliothèque (via `dladdr`) au lieu de « hors module ».
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn host_fault_report_names_the_host_library() {
        let exe = std::env::current_exe().expect("exe de test");
        let out = std::process::Command::new(exe)
            .args(["--exact", "signal::tests::host_fault_child", "--nocapture"])
            .env("SB_TEST_HOST_FAULT_CHILD", "1")
            .output()
            .expect("lancement de l'enfant");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(128 + libc::SIGSEGV), "{stderr}");
        assert!(stderr.contains("SIGSEGV"), "{stderr}");
        let rip_line = stderr
            .lines()
            .find(|l| l.starts_with("RIP"))
            .unwrap_or_default();
        assert!(rip_line.contains("hôte :"), "{stderr}");
        assert!(rip_line.contains(".so"), "{stderr}");
        assert!(stderr.contains("Pile hôte"), "{stderr}");
    }
}
