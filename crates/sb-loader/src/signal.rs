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
    // SAFETY: `sigaction` avec handler `SA_SIGINFO` conforme POSIX.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESETHAND;
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
    let sig_name = match sig {
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGILL => "SIGILL",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGTRAP => "SIGTRAP (int3 invité)",
        _ => "SIGNAL",
    };

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
    if !ucontext.is_null() {
        append_detailed_state(&mut msg, ucontext);
    }
    msg.push_str("===============================================================\n");
    let _ = std::io::stderr().write_all(msg.as_bytes());
    let _ = std::io::stderr().flush();

    // SAFETY: terminer proprement avec le code signal standard (128 + sig).
    unsafe {
        libc::_exit(128 + sig);
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
    use std::fmt::Write as _;

    // SAFETY: `ucontext` fourni par le noyau Linux pour `SA_SIGINFO`.
    let ctx = unsafe { &*(ucontext as *const libc::ucontext_t) };
    let g = &ctx.uc_mcontext.gregs;
    let r = |n: i32| g[n as usize] as usize;
    let _ = writeln!(
        msg,
        "RBX {:#018x}  RBP {:#018x}  RSI {:#018x}  RDI {:#018x}\n\
         R10 {:#018x}  R11 {:#018x}  R12 {:#018x}  R13 {:#018x}\n\
         R14 {:#018x}  R15 {:#018x}  EFL {:#010x}",
        r(libc::REG_RBX),
        r(libc::REG_RBP),
        r(libc::REG_RSI),
        r(libc::REG_RDI),
        r(libc::REG_R10),
        r(libc::REG_R11),
        r(libc::REG_R12),
        r(libc::REG_R13),
        r(libc::REG_R14),
        r(libc::REG_R15),
        r(libc::REG_EFL),
    );

    let rip = r(libc::REG_RIP);
    let rsp = r(libc::REG_RSP);
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

    if rsp != 0 && rsp % 8 == 0 {
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
    if !in_image(rip) && rsp != 0 && rsp % 8 == 0 {
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

    // Derniers appels d'API HLE (traceur `SB_TRACE_API`).
    let calls = crate::trampoline::recent_api_calls(40);
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
        rbp: r(libc::REG_RBP) as u64,
        rbx: r(libc::REG_RBX) as u64,
        rsi: r(libc::REG_RSI) as u64,
        rdi: r(libc::REG_RDI) as u64,
        r12: r(libc::REG_R12) as u64,
        r13: r(libc::REG_R13) as u64,
        r14: r(libc::REG_R14) as u64,
        r15: r(libc::REG_R15) as u64,
        ..crate::unwind::Context::default()
    };
    uc.rax = r(libc::REG_RAX) as u64;
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
