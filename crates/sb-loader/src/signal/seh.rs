//! Conversion des fautes matérielles du code invité en exceptions SEH Windows.
//!
//! Le gestionnaire de signaux construit, **sur la pile invitée** (sous RSP, comme le noyau
//! Windows), un `EXCEPTION_RECORD` (152 octets) et un `CONTEXT` complet, puis modifie le
//! `ucontext` pour que le retour de signal reprenne dans [`fault_entry`] (pile invitée, hors
//! contexte de signal). `fault_entry` appelle le dispatcher SEH de `sb-hle` ; s'il revient,
//! personne n'a géré l'exception et le rapport de crash historique est produit.
//!
//! Le gestionnaire s'exécute sur une pile alternative par thread (`sigaltstack`) : un
//! débordement de pile invité produit ainsi un rapport au lieu d'un arrêt silencieux. Un tel
//! débordement n'est pas converti en SEH (`STATUS_STACK_OVERFLOW` non émis).

use std::cell::RefCell;

use super::{
    Regs, append_detailed_regs, exception_dispatcher, fatal_fault_report, regs_from_context,
};
use crate::unwind::Context;

/// Taille de la pile alternative de signaux (suffisante pour le rapport de crash).
const ALTSTACK_SIZE: usize = 256 * 1024;

const STATUS_ACCESS_VIOLATION: u32 = 0xC000_0005;
const STATUS_IN_PAGE_ERROR: u32 = 0xC000_0006;
const STATUS_BREAKPOINT: u32 = 0x8000_0003;
const STATUS_SINGLE_STEP: u32 = 0x8000_0004;
const STATUS_ILLEGAL_INSTRUCTION: u32 = 0xC000_001D;
const STATUS_INTEGER_DIVIDE_BY_ZERO: u32 = 0xC000_0094;
const STATUS_INTEGER_OVERFLOW: u32 = 0xC000_0095;
const STATUS_FLOAT_DIVIDE_BY_ZERO: u32 = 0xC000_008E;
const STATUS_FLOAT_INEXACT_RESULT: u32 = 0xC000_008F;
const STATUS_FLOAT_INVALID_OPERATION: u32 = 0xC000_0090;
const STATUS_FLOAT_OVERFLOW: u32 = 0xC000_0091;
const STATUS_FLOAT_UNDERFLOW: u32 = 0xC000_0093;

// `si_code` de SIGFPE (asm-generic/siginfo.h).
const FPE_INTDIV: i32 = 1;
const FPE_INTOVF: i32 = 2;
const FPE_FLTDIV: i32 = 3;
const FPE_FLTOVF: i32 = 4;
const FPE_FLTUND: i32 = 5;
const FPE_FLTRES: i32 = 6;
const FPE_FLTINV: i32 = 7;
/// `si_code` de SIGTRAP pour un pas-à-pas.
const TRAP_TRACE: i32 = 2;

/// Taille d'un `EXCEPTION_RECORD` x64.
const RECORD_SIZE: usize = 152;
/// Zone des métadonnées de signal placée juste après l'enregistrement : `sig` (i32) puis
/// `fault_addr` (u64).
const META_SIZE: usize = 16;

/// Pile alternative du thread courant (libérée à la fin du thread).
struct AltStack {
    base: *mut libc::c_void,
}

impl Drop for AltStack {
    fn drop(&mut self) {
        let ss = libc::stack_t {
            ss_sp: core::ptr::null_mut(),
            ss_flags: libc::SS_DISABLE,
            ss_size: 0,
        };
        // SAFETY: désactive la pile alternative avant de libérer sa mémoire.
        unsafe {
            libc::sigaltstack(&ss, core::ptr::null_mut());
            libc::munmap(self.base, ALTSTACK_SIZE);
        }
    }
}

thread_local! {
    static ALT_STACK: RefCell<Option<AltStack>> = const { RefCell::new(None) };
}

/// Installe (une fois par thread) la pile alternative utilisée par les gestionnaires de signaux
/// de diagnostic.
pub fn ensure_thread_altstack() {
    let _ = ALT_STACK.try_with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_some() {
            return;
        }
        // SAFETY: mapping anonyme privé de `ALTSTACK_SIZE` octets.
        let base = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                ALTSTACK_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return;
        }
        let ss = libc::stack_t {
            ss_sp: base,
            ss_flags: 0,
            ss_size: ALTSTACK_SIZE,
        };
        // SAFETY: `ss` décrit un mapping valide appartenant à ce thread.
        if unsafe { libc::sigaltstack(&ss, core::ptr::null_mut()) } == 0 {
            *slot = Some(AltStack { base });
        } else {
            // SAFETY: libère le mapping inutilisé.
            unsafe { libc::munmap(base, ALTSTACK_SIZE) };
        }
    });
}

/// Statut SEH correspondant à un signal. Retourne `(code, rip_delta, information)`.
fn status_for_signal(
    sig: i32,
    si_code: i32,
    fault_addr: usize,
    err: u64,
) -> Option<(u32, i64, Vec<usize>)> {
    Some(match sig {
        libc::SIGSEGV => {
            let kind = if err & 0x10 != 0 {
                8 // exécution (DEP)
            } else {
                usize::from(err & 0x2 != 0) // 0 lecture, 1 écriture
            };
            (STATUS_ACCESS_VIOLATION, 0, vec![kind, fault_addr])
        }
        libc::SIGBUS => (STATUS_IN_PAGE_ERROR, 0, vec![0, fault_addr, 0]),
        libc::SIGILL => (STATUS_ILLEGAL_INSTRUCTION, 0, vec![]),
        libc::SIGFPE => {
            let code = match si_code {
                FPE_INTDIV => STATUS_INTEGER_DIVIDE_BY_ZERO,
                FPE_INTOVF => STATUS_INTEGER_OVERFLOW,
                FPE_FLTDIV => STATUS_FLOAT_DIVIDE_BY_ZERO,
                FPE_FLTOVF => STATUS_FLOAT_OVERFLOW,
                FPE_FLTUND => STATUS_FLOAT_UNDERFLOW,
                FPE_FLTRES => STATUS_FLOAT_INEXACT_RESULT,
                FPE_FLTINV => STATUS_FLOAT_INVALID_OPERATION,
                _ => STATUS_INTEGER_DIVIDE_BY_ZERO,
            };
            (code, 0, vec![])
        }
        libc::SIGTRAP => {
            if si_code == TRAP_TRACE {
                (STATUS_SINGLE_STEP, 0, vec![])
            } else {
                // `int3` : RIP pointe après l'instruction, Windows rapporte l'adresse de `int3`.
                (STATUS_BREAKPOINT, -1, vec![])
            }
        }
        _ => return None,
    })
}

/// Tente de rediriger l'exécution vers [`fault_entry`] sur la pile invitée.
///
/// Retourne `true` si le `ucontext` a été modifié (le gestionnaire doit simplement revenir).
pub(super) fn try_redirect_to_seh(
    sig: i32,
    info: *mut libc::siginfo_t,
    ucontext: *mut libc::c_void,
    rip: usize,
    rsp: usize,
    fault_addr: usize,
) -> bool {
    if exception_dispatcher().is_none() || rip == 0 {
        return false;
    }
    // Uniquement le code d'une image PE invitée ; pile plausible (alignée, non triviale).
    if crate::unwind::image_base_for_pc(rip).is_none() || rsp < 0x10000 || !rsp.is_multiple_of(8) {
        return false;
    }
    // Débordement de pile (faute juste sous RSP) : pas de pile pour dispatcher.
    if sig == libc::SIGSEGV && fault_addr >= rsp.saturating_sub(0x10000) && fault_addr < rsp {
        return false;
    }
    // SAFETY: `info` fourni par le noyau pour `SA_SIGINFO` (vérifié non nul).
    let si_code = if info.is_null() {
        0
    } else {
        unsafe { (*info).si_code }
    };
    // SAFETY: `ucontext` est un `ucontext_t` Linux x86-64 modifiable.
    let uc = unsafe { &mut *(ucontext as *mut libc::ucontext_t) };
    let err = uc.uc_mcontext.gregs[libc::REG_ERR as usize] as u64;
    let Some((code, rip_delta, params)) = status_for_signal(sig, si_code, fault_addr, err) else {
        return false;
    };

    // Disposition sur la pile invitée (de haut en bas) : [marge][CONTEXT][record + meta][retour].
    // Si le gestionnaire tourne sur la pile invitée (pas de pile alternative), le cadre de signal
    // du noyau occupe la zone sous RSP : on place les blocs sous le cadre courant du gestionnaire.
    let here = 0u8;
    let handler_sp = core::hint::black_box(&here) as *const u8 as usize;
    let top = (rsp.min(handler_sp) - 0x200) & !0xF;
    let ctx_addr = (top - 0x20 - core::mem::size_of::<Context>()) & !0xF;
    let rec_addr = (ctx_addr - RECORD_SIZE - META_SIZE) & !0xF;
    let entry_rsp = rec_addr - 0x28;

    // SAFETY: les zones écrites sont sous le RSP invité, dans la pile du thread (mappée) ; les
    // structures sont POD et alignées 16.
    unsafe {
        let g = &uc.uc_mcontext.gregs;
        let r = |n: i32| g[n as usize] as u64;
        let mut c = Context {
            context_flags: 0x0010_000F,
            seg_cs: 0x33,
            seg_ds: 0x2B,
            seg_es: 0x2B,
            seg_fs: 0x53,
            seg_gs: 0x2B,
            seg_ss: 0x2B,
            eflags: r(libc::REG_EFL) as u32,
            rax: r(libc::REG_RAX),
            rcx: r(libc::REG_RCX),
            rdx: r(libc::REG_RDX),
            rbx: r(libc::REG_RBX),
            rsp: r(libc::REG_RSP),
            rbp: r(libc::REG_RBP),
            rsi: r(libc::REG_RSI),
            rdi: r(libc::REG_RDI),
            r8: r(libc::REG_R8),
            r9: r(libc::REG_R9),
            r10: r(libc::REG_R10),
            r11: r(libc::REG_R11),
            r12: r(libc::REG_R12),
            r13: r(libc::REG_R13),
            r14: r(libc::REG_R14),
            r15: r(libc::REG_R15),
            rip: r(libc::REG_RIP).wrapping_add_signed(rip_delta),
            ..Context::default()
        };
        if !uc.uc_mcontext.fpregs.is_null() {
            // `_libc_fpstate` a la disposition de `fxsave` (512 octets).
            core::ptr::copy_nonoverlapping(
                uc.uc_mcontext.fpregs as *const u8,
                (&raw mut c.flt_save).cast::<u8>(),
                512,
            );
            c.mx_csr = c.flt_save.mx_csr;
        } else {
            c.context_flags &= !0x8;
        }
        core::ptr::write(ctx_addr as *mut Context, c);

        let mut rec = [0u8; RECORD_SIZE + META_SIZE];
        rec[0..4].copy_from_slice(&code.to_le_bytes());
        // flags = 0 ; record chaîné = NULL
        let exc_addr = c.rip;
        rec[16..24].copy_from_slice(&exc_addr.to_le_bytes());
        let n = params.len().min(15);
        rec[24..28].copy_from_slice(&(n as u32).to_le_bytes());
        for (i, p) in params.iter().take(n).enumerate() {
            rec[32 + i * 8..40 + i * 8].copy_from_slice(&(*p as u64).to_le_bytes());
        }
        rec[RECORD_SIZE..RECORD_SIZE + 4].copy_from_slice(&sig.to_le_bytes());
        rec[RECORD_SIZE + 8..RECORD_SIZE + 16].copy_from_slice(&(fault_addr as u64).to_le_bytes());
        core::ptr::copy_nonoverlapping(rec.as_ptr(), rec_addr as *mut u8, rec.len());
        // Adresse de retour factice (0) : arrête les remontées de pile.
        core::ptr::write(entry_rsp as *mut u64, 0);

        let gm = &mut uc.uc_mcontext.gregs;
        gm[libc::REG_RIP as usize] = fault_entry as *const () as usize as i64;
        gm[libc::REG_RSP as usize] = entry_rsp as i64;
        gm[libc::REG_RCX as usize] = rec_addr as i64;
        gm[libc::REG_RDX as usize] = ctx_addr as i64;
        // Win64 : DF et TF effacés à l'entrée d'une fonction.
        gm[libc::REG_EFL as usize] &= !(0x400 | 0x100);
    }
    true
}

/// Point d'entrée Win64 (exécuté sur la pile invitée) après une faute convertie en SEH.
extern "win64" fn fault_entry(rec: *mut u8, ctx: *mut Context) -> ! {
    // Capture les derniers appels d'API HLE *avant* le passage dans les handlers SEH du titre
    // (ex. Breakpad/minidump qui noie sinon le tampon circulaire avec des centaines de memcpy/HTTP).
    let pre_seh_calls = crate::trampoline::recent_api_calls(40);
    let pre_seh_sig_calls = crate::trampoline::recent_significant_api_calls(120);
    if let Some(d) = exception_dispatcher() {
        // SAFETY: `rec` et `ctx` ont été construits par `try_redirect_to_seh` ; le dispatcher
        // ne revient que si l'exception n'est gérée par personne.
        unsafe { d(rec, ctx) };
    }
    // SAFETY: mêmes pointeurs, valides sur la pile invitée ; les métadonnées suivent le record.
    let (sig, fault_addr, c) = unsafe {
        (
            core::ptr::read_unaligned(rec.add(RECORD_SIZE).cast::<i32>()),
            core::ptr::read_unaligned(rec.add(RECORD_SIZE + 8).cast::<u64>()) as usize,
            *ctx,
        )
    };
    // SAFETY: rétablit l'action par défaut avant le rapport.
    unsafe { libc::signal(sig, libc::SIG_DFL) };
    let rip = if sig == libc::SIGTRAP {
        c.rip.wrapping_add(1)
    } else {
        c.rip
    } as usize;
    let regs: Regs = regs_from_context(&c);
    fatal_fault_report(
        sig,
        fault_addr,
        [
            rip,
            c.rsp as usize,
            c.rax as usize,
            c.rcx as usize,
            c.rdx as usize,
            c.r8 as usize,
            c.r9 as usize,
        ],
        |msg| {
            append_detailed_regs(msg, &regs);
            if !pre_seh_sig_calls.is_empty() {
                use std::fmt::Write as _;
                let _ = writeln!(
                    msg,
                    "Appels d'API significatifs avant la faute (avant SEH, {} ; le plus récent en bas) :",
                    pre_seh_sig_calls.len()
                );
                for call in &pre_seh_sig_calls {
                    let _ = writeln!(msg, "  {call}");
                }
            }
            if !pre_seh_calls.is_empty() {
                use std::fmt::Write as _;
                let _ = writeln!(
                    msg,
                    "Appels d'API HLE juste avant la faute (brut avant SEH, {} ; le plus récent en bas) :",
                    pre_seh_calls.len()
                );
                for call in &pre_seh_calls {
                    let _ = writeln!(msg, "  {call}");
                }
            }
        },
    )
}
