//! Propagation des exceptions Windows x64 (SEH) pour le code invité PE32+.
//!
//! Ce module fournit le noyau de l'« exception dispatching » de ntdll :
//!
//! - [`ExceptionRecord`] / [`DispatcherContext`] (layouts Win64 vérifiés à la compilation) ;
//! - `RaiseException`, `RtlRaiseException`, `RtlRaiseStatus`, `RtlDispatchException` : phase 1,
//!   gestionnaires vectoriels puis remontée des trames via les tables `.pdata`
//!   ([`sb_loader::unwind`]) en appelant chaque `EXCEPTION_ROUTINE` ;
//! - `RtlUnwindEx` / `RtlUnwind` : phase 2 (appel des handlers avec `EXCEPTION_UNWINDING`, gestion
//!   de `EXCEPTION_TARGET_UNWIND` et des déroulages « collisionnés »), puis restauration du contexte
//!   cible ;
//! - `RtlRestoreContext` (naked asm) ;
//! - `__C_specific_handler` (`__try` / `__except` / `__finally`) ;
//! - `Add/RemoveVectoredExceptionHandler`, `Add/RemoveVectoredContinueHandler`.
//!
//! Les exceptions C++ MSVC (`_CxxThrowException`, `__CxxFrameHandler3/4`) vivent dans [`cxx`].
//!
//! # Modèle d'exécution
//!
//! Le code invité s'exécute sur la pile native du thread hôte : les fonctions Rust du dispatcher
//! se trouvent *sous* les trames invitées. Un déroulage qui traverse ces trames hôtes (par
//! exemple quand un handler invité appelle `RtlUnwindEx`) ne dispose d'aucune métadonnée
//! `.pdata` : on enregistre donc, pour chaque appel d'une routine invitée depuis le dispatcher
//! ([`with_host_call`]), le contexte à partir duquel la marche doit continuer. Quand la remontée
//! atteint une adresse hors image, elle « saute » vers ce contexte, ce qui reproduit le chemin
//! que ntdll emprunte via `KiUserExceptionDispatcher`.
//!
//! La restauration finale d'un contexte (`RtlRestoreContext`) abandonne les trames Rust situées
//! sous la cible sans exécuter leurs destructeurs : aucun verrou ni allocation Rust ne doit donc
//! être détenu pendant l'appel d'une routine invitée.
//!
//! Aucun panic Rust ne traverse la frontière FFI : chaque point d'entrée est enveloppé dans
//! `ffi_guard!`, et une panique dans un chemin « sans retour » termine le processus.

#![allow(non_snake_case, clippy::missing_safety_doc)]

pub(crate) mod cxx;

use core::ffi::c_void;
use core::mem::{offset_of, size_of};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::RwLock;
use sb_common::{ffi_guard, sb_err, sb_warn};
use sb_loader::unwind::{
    Context, RuntimeFunction, UNW_FLAG_EHANDLER, UNW_FLAG_NHANDLER, UNW_FLAG_UHANDLER,
    image_base_for_pc, lookup_function_entry, virtual_unwind,
};

use crate::kernel_ext::rtl::{CONTEXT_CAPTURE_FLAGS, RtlCaptureContext};

pub(crate) const CHANNEL: &str = "seh";

// ------------------------------------------------------------------------------------------
// Constantes
// ------------------------------------------------------------------------------------------

pub const EXCEPTION_NONCONTINUABLE: u32 = 0x01;
pub const EXCEPTION_UNWINDING: u32 = 0x02;
pub const EXCEPTION_EXIT_UNWIND: u32 = 0x04;
pub const EXCEPTION_STACK_INVALID: u32 = 0x08;
pub const EXCEPTION_NESTED_CALL: u32 = 0x10;
pub const EXCEPTION_TARGET_UNWIND: u32 = 0x20;
pub const EXCEPTION_COLLIDED_UNWIND: u32 = 0x40;
/// `EXCEPTION_UNWIND` : `UNWINDING | EXIT_UNWIND | TARGET_UNWIND | COLLIDED_UNWIND`.
pub const EXCEPTION_UNWIND: u32 = 0x66;

pub const EXCEPTION_MAXIMUM_PARAMETERS: usize = 15;

pub const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
pub const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
pub const EXCEPTION_EXECUTE_HANDLER: i32 = 1;

pub const EXCEPTION_DISPOSITION_CONTINUE_EXECUTION: i32 = 0;
pub const EXCEPTION_DISPOSITION_CONTINUE_SEARCH: i32 = 1;
pub const EXCEPTION_DISPOSITION_NESTED_EXCEPTION: i32 = 2;
pub const EXCEPTION_DISPOSITION_COLLIDED_UNWIND: i32 = 3;

pub const STATUS_ACCESS_VIOLATION: u32 = 0xC000_0005;
pub const STATUS_NONCONTINUABLE_EXCEPTION: u32 = 0xC000_0025;
pub const STATUS_INVALID_DISPOSITION: u32 = 0xC000_0026;
pub const STATUS_UNWIND: u32 = 0xC000_0027;
pub const STATUS_BAD_STACK: u32 = 0xC000_0028;
pub const STATUS_INVALID_UNWIND_TARGET: u32 = 0xC000_0029;
pub const STATUS_BREAKPOINT: u32 = 0x8000_0003;
pub const STATUS_GCC_THROW: u32 = 0x2047_4343;

/// Nombre maximal de trames parcourues par une remontée (garde-fou contre une pile corrompue).
const MAX_FRAMES: usize = 1 << 17;

// ------------------------------------------------------------------------------------------
// Structures ABI
// ------------------------------------------------------------------------------------------

/// `EXCEPTION_RECORD` x64 (152 octets).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ExceptionRecord {
    pub exception_code: u32,
    pub exception_flags: u32,
    pub exception_record: *mut ExceptionRecord,
    pub exception_address: usize,
    pub number_parameters: u32,
    pub _pad: u32,
    pub exception_information: [usize; EXCEPTION_MAXIMUM_PARAMETERS],
}

impl ExceptionRecord {
    pub const fn new(code: u32, flags: u32) -> Self {
        Self {
            exception_code: code,
            exception_flags: flags,
            exception_record: core::ptr::null_mut(),
            exception_address: 0,
            number_parameters: 0,
            _pad: 0,
            exception_information: [0; EXCEPTION_MAXIMUM_PARAMETERS],
        }
    }
}

/// `EXCEPTION_POINTERS`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ExceptionPointers {
    pub exception_record: *mut ExceptionRecord,
    pub context_record: *mut Context,
}

/// `DISPATCHER_CONTEXT` x64 (0x50 octets), transmis aux `EXCEPTION_ROUTINE`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DispatcherContext {
    pub control_pc: u64,
    pub image_base: u64,
    pub function_entry: *const RuntimeFunction,
    pub establisher_frame: u64,
    pub target_ip: u64,
    pub context_record: *mut Context,
    pub language_handler: usize,
    pub handler_data: usize,
    pub history_table: *mut c_void,
    pub scope_index: u32,
    pub fill0: u32,
}

const _: () = {
    assert!(size_of::<ExceptionRecord>() == 152);
    assert!(offset_of!(ExceptionRecord, exception_flags) == 4);
    assert!(offset_of!(ExceptionRecord, exception_record) == 8);
    assert!(offset_of!(ExceptionRecord, exception_address) == 16);
    assert!(offset_of!(ExceptionRecord, number_parameters) == 24);
    assert!(offset_of!(ExceptionRecord, exception_information) == 32);
    assert!(size_of::<ExceptionPointers>() == 16);
    assert!(size_of::<DispatcherContext>() == 0x50);
    assert!(offset_of!(DispatcherContext, image_base) == 0x08);
    assert!(offset_of!(DispatcherContext, function_entry) == 0x10);
    assert!(offset_of!(DispatcherContext, establisher_frame) == 0x18);
    assert!(offset_of!(DispatcherContext, target_ip) == 0x20);
    assert!(offset_of!(DispatcherContext, context_record) == 0x28);
    assert!(offset_of!(DispatcherContext, language_handler) == 0x30);
    assert!(offset_of!(DispatcherContext, handler_data) == 0x38);
    assert!(offset_of!(DispatcherContext, history_table) == 0x40);
    assert!(offset_of!(DispatcherContext, scope_index) == 0x48);
};

/// `EXCEPTION_ROUTINE` : `(record, establisher_frame, context, dispatcher_context) -> disposition`.
pub type ExceptionRoutine = unsafe extern "win64" fn(
    *mut ExceptionRecord,
    u64,
    *mut Context,
    *mut DispatcherContext,
) -> i32;

/// Gestionnaire vectoriel : `LONG (*)(EXCEPTION_POINTERS *)`.
type VectoredHandler = unsafe extern "win64" fn(*mut ExceptionPointers) -> i32;

// ------------------------------------------------------------------------------------------
// Traçage optionnel (SB_TRACE_SEH=1)
// ------------------------------------------------------------------------------------------

pub(crate) fn trace_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SB_TRACE_SEH").is_some_and(|v| v != "0"))
}

macro_rules! seh_trace {
    ($($arg:tt)+) => {
        if $crate::exceptions::trace_enabled() {
            sb_common::sb_info!($crate::exceptions::CHANNEL, $($arg)+);
        }
    };
}
#[allow(unused_imports)]
pub(crate) use seh_trace;

// ------------------------------------------------------------------------------------------
// Appels hôte → invité et passage par les trames hôtes
// ------------------------------------------------------------------------------------------

/// Appel d'une routine invitée depuis le dispatcher : permet à un déroulage lancé *depuis* cette
/// routine de traverser la trame hôte et de poursuivre depuis `resume`.
#[derive(Clone, Copy)]
struct HostCall {
    /// Adresse d'une variable locale de la trame hôte (borne haute des trames de la routine).
    marker: usize,
    /// Contexte à partir duquel la marche reprend quand elle atteint cette trame hôte.
    resume: *const Context,
}

thread_local! {
    static HOST_CALLS: RefCell<Vec<HostCall>> = const { RefCell::new(Vec::new()) };
}

/// Exécute `f` (qui appelle une routine invitée) en enregistrant le point de reprise `resume`.
///
/// `resume` doit rester valide pendant toute la durée de `f`.
#[inline(never)]
fn with_host_call<R>(resume: &Context, f: impl FnOnce() -> R) -> R {
    let local = 0u8;
    let marker = core::hint::black_box(&local) as *const u8 as usize;
    HOST_CALLS.with(|h| {
        let mut h = h.borrow_mut();
        // Les entrées plus profondes (adresses plus basses) sont mortes : trames abandonnées.
        h.retain(|c| c.marker > marker);
        h.push(HostCall {
            marker,
            resume: core::ptr::from_ref(resume),
        });
    });
    let r = f();
    HOST_CALLS.with(|h| {
        let mut h = h.borrow_mut();
        if let Some(pos) = h.iter().rposition(|c| c.marker == marker) {
            h.truncate(pos);
        }
    });
    r
}

/// Contexte de reprise de l'appel hôte le plus interne dont la trame se situe au-dessus de `rsp`.
fn host_resume_for(rsp: u64) -> Option<Context> {
    HOST_CALLS.with(|h| {
        h.borrow()
            .iter()
            .filter(|c| c.marker as u64 >= rsp)
            .min_by_key(|c| c.marker)
            // SAFETY: `resume` est valide tant que l'appel hôte correspondant est actif ;
            // les entrées mortes sont élaguées avant tout saut (`prune_dead_host_calls`).
            .map(|c| unsafe { *c.resume })
    })
}

/// Supprime les appels hôte dont la trame se trouve sous `new_rsp` (abandonnée par un saut).
fn prune_dead_host_calls(new_rsp: u64) {
    HOST_CALLS.with(|h| h.borrow_mut().retain(|c| c.marker as u64 >= new_rsp));
    cxx::prune_dead_catches(new_rsp);
}

/// Appelle une `EXCEPTION_ROUTINE` invitée.
///
/// # Safety
/// `handler` doit être l'adresse d'une routine Win64 de signature [`ExceptionRoutine`].
unsafe fn call_routine(
    handler: usize,
    rec: *mut ExceptionRecord,
    establisher: u64,
    ctx: *mut Context,
    disp: *mut DispatcherContext,
) -> i32 {
    // SAFETY: contrat de l'appelant ; `handler` est l'adresse d'une EXCEPTION_ROUTINE.
    let f: ExceptionRoutine = unsafe { core::mem::transmute::<usize, ExceptionRoutine>(handler) };
    // SAFETY: arguments valides construits par le dispatcher.
    unsafe { f(rec, establisher, ctx, disp) }
}

// ------------------------------------------------------------------------------------------
// Remontée d'une trame
// ------------------------------------------------------------------------------------------

/// Trame invitée dépilée par [`unwind_frame`].
#[derive(Clone, Copy)]
struct FrameInfo {
    control_pc: u64,
    image_base: usize,
    function_entry: *const RuntimeFunction,
    handler: usize,
    handler_data: usize,
    establisher: u64,
    /// Passage par une trame hôte (aucun handler, pas de test de cadre).
    host_hop: bool,
}

/// Dépile une trame de `cur` dans `new`.
///
/// Retourne `None` quand la pile ne peut plus être remontée (adresse hors image sans point de
/// reprise hôte, `UNWIND_INFO` non supportée).
///
/// # Safety
/// La pile décrite par `cur` et le code des images doivent être lisibles.
unsafe fn unwind_frame(kind: u32, cur: &Context, new: &mut Context) -> Option<FrameInfo> {
    *new = *cur;
    let pc = cur.rip as usize;
    if let Some((base, func)) = lookup_function_entry(pc) {
        // SAFETY: `func` provient de la table `.pdata` de l'image `base`.
        let r = unsafe { virtual_unwind(kind, base, pc, func, new, None) }?;
        Some(FrameInfo {
            control_pc: pc as u64,
            image_base: base,
            function_entry: func,
            handler: r.handler.unwrap_or(0),
            handler_data: r.handler_data,
            establisher: r.establisher_frame,
            host_hop: false,
        })
    } else if let Some(base) = image_base_for_pc(pc) {
        // Fonction feuille d'une image connue : adresse de retour au sommet de pile.
        if cur.rsp == 0 {
            return None;
        }
        // SAFETY: sommet de pile invité lisible.
        new.rip = unsafe { core::ptr::read_unaligned(cur.rsp as usize as *const u64) };
        new.rsp = cur.rsp.wrapping_add(8);
        Some(FrameInfo {
            control_pc: pc as u64,
            image_base: base,
            function_entry: core::ptr::null(),
            handler: 0,
            handler_data: 0,
            establisher: cur.rsp,
            host_hop: false,
        })
    } else if let Some(resume) = host_resume_for(cur.rsp) {
        *new = resume;
        Some(FrameInfo {
            control_pc: pc as u64,
            image_base: 0,
            function_entry: core::ptr::null(),
            handler: 0,
            handler_data: 0,
            establisher: cur.rsp,
            host_hop: true,
        })
    } else {
        None
    }
}

// ------------------------------------------------------------------------------------------
// Gestionnaires vectoriels
// ------------------------------------------------------------------------------------------

struct VectoredEntry {
    id: usize,
    handler: usize,
}

static VEH_LIST: RwLock<Vec<VectoredEntry>> = RwLock::new(Vec::new());
static VCH_LIST: RwLock<Vec<VectoredEntry>> = RwLock::new(Vec::new());
static NEXT_VECTORED_ID: AtomicUsize = AtomicUsize::new(0x1000);

fn vectored_add(list: &RwLock<Vec<VectoredEntry>>, first: u32, handler: usize) -> usize {
    if handler == 0 {
        return 0;
    }
    let id = NEXT_VECTORED_ID.fetch_add(0x10, Ordering::Relaxed);
    let mut l = list.write();
    let entry = VectoredEntry { id, handler };
    if first != 0 {
        l.insert(0, entry);
    } else {
        l.push(entry);
    }
    id
}

fn vectored_remove(list: &RwLock<Vec<VectoredEntry>>, handle: usize) -> u32 {
    let mut l = list.write();
    let before = l.len();
    l.retain(|e| e.id != handle);
    u32::from(l.len() != before)
}

/// Appelle les gestionnaires de `list` ; `true` dès que l'un renvoie `EXCEPTION_CONTINUE_EXECUTION`.
///
/// # Safety
/// `rec` et `ctx` doivent être valides ; les handlers sont des routines invitées.
unsafe fn vectored_call(
    list: &RwLock<Vec<VectoredEntry>>,
    rec: *mut ExceptionRecord,
    ctx: *mut Context,
) -> bool {
    // Copie de la liste : aucun verrou ne doit être détenu pendant l'appel invité.
    let handlers: Vec<usize> = list.read().iter().map(|e| e.handler).collect();
    let mut ptrs = ExceptionPointers {
        exception_record: rec,
        context_record: ctx,
    };
    for h in handlers {
        // SAFETY: `h` est un `PVECTORED_EXCEPTION_HANDLER` Win64 enregistré par l'invité.
        let f: VectoredHandler = unsafe { core::mem::transmute::<usize, VectoredHandler>(h) };
        // SAFETY: `ptrs` est valide pendant l'appel.
        if unsafe { f(&mut ptrs) } == EXCEPTION_CONTINUE_EXECUTION {
            return true;
        }
    }
    false
}

pub(crate) extern "win64" fn AddVectoredExceptionHandler(first: u32, handler: usize) -> usize {
    ffi_guard!(CHANNEL, "AddVectoredExceptionHandler", 0, {
        vectored_add(&VEH_LIST, first, handler)
    })
}

pub(crate) extern "win64" fn RemoveVectoredExceptionHandler(handle: usize) -> u32 {
    ffi_guard!(CHANNEL, "RemoveVectoredExceptionHandler", 0, {
        vectored_remove(&VEH_LIST, handle)
    })
}

pub(crate) extern "win64" fn AddVectoredContinueHandler(first: u32, handler: usize) -> usize {
    ffi_guard!(CHANNEL, "AddVectoredContinueHandler", 0, {
        vectored_add(&VCH_LIST, first, handler)
    })
}

pub(crate) extern "win64" fn RemoveVectoredContinueHandler(handle: usize) -> u32 {
    ffi_guard!(CHANNEL, "RemoveVectoredContinueHandler", 0, {
        vectored_remove(&VCH_LIST, handle)
    })
}

// ------------------------------------------------------------------------------------------
// Restauration de contexte
// ------------------------------------------------------------------------------------------

/// Restaure `ctx` selon ses `ContextFlags` (CONTROL : RSP/RIP/EFLAGS ; INTEGER : RAX..R15 ;
/// FLOATING_POINT : `fxrstor64` + MXCSR) et saute à `ctx.rip`. Ne revient jamais.
///
/// L'adresse de retour et EFLAGS sont poussés sous la nouvelle pile (région morte).
/// `GS` (TEB) et `FS` ne sont jamais touchés.
#[unsafe(naked)]
unsafe extern "win64" fn restore_context_raw(ctx: *const Context) -> ! {
    core::arch::naked_asm!(
        "test dword ptr [rcx + 0x30], 8",
        "jz 2f",
        "fxrstor64 [rcx + 0x100]",
        "ldmxcsr dword ptr [rcx + 0x34]",
        "2:",
        "mov r11, [rcx + 0x98]",
        "mov rax, [rcx + 0xF8]",
        "mov [r11 - 8], rax",
        "mov eax, dword ptr [rcx + 0x44]",
        "and eax, 0xCD5",
        "or eax, 2",
        "mov [r11 - 16], rax",
        "test dword ptr [rcx + 0x30], 2",
        "jz 3f",
        "mov rax, [rcx + 0x78]",
        "mov rdx, [rcx + 0x88]",
        "mov rbx, [rcx + 0x90]",
        "mov rbp, [rcx + 0xA0]",
        "mov rsi, [rcx + 0xA8]",
        "mov rdi, [rcx + 0xB0]",
        "mov r8, [rcx + 0xB8]",
        "mov r9, [rcx + 0xC0]",
        "mov r10, [rcx + 0xC8]",
        "mov r12, [rcx + 0xD8]",
        "mov r13, [rcx + 0xE0]",
        "mov r14, [rcx + 0xE8]",
        "mov r15, [rcx + 0xF0]",
        "3:",
        "lea rsp, [r11 - 16]",
        "test dword ptr [rcx + 0x30], 2",
        "jz 4f",
        "mov r11, [rcx + 0xD0]",
        "mov rcx, [rcx + 0x80]",
        "4:",
        "popfq",
        "ret",
    )
}

/// Restaure `ctx` et reprend l'exécution invitée à `ctx.rip`.
///
/// Le contexte est d'abord copié dans la trame courante : l'original (souvent dans une trame
/// morte) ne risque pas d'être écrasé par les deux mots poussés sous la pile cible.
///
/// # Safety
/// `ctx` doit décrire un état d'exécution invité valide (RSP aligné, RIP exécutable).
pub(crate) unsafe fn restore_context(ctx: &Context) -> ! {
    let local: Context = *ctx;
    prune_dead_host_calls(local.rsp);
    seh_trace!(
        "restore_context rip={:#x} rsp={:#x} flags={:#x}",
        local.rip,
        local.rsp,
        local.context_flags
    );
    // SAFETY: contrat de l'appelant.
    unsafe { restore_context_raw(&local) }
}

/// Termine le processus après une erreur fatale du dispatcher (panique interne, pile invalide).
fn fatal(msg: &str) -> ! {
    sb_err!(CHANNEL, "{msg}");
    sb_loader::signal::exit_process_now(0xC000_0409);
}

/// `RtlRestoreContext(ContextRecord, ExceptionRecord)`.
pub(crate) extern "win64" fn RtlRestoreContext(ctx: *const Context, _rec: *const ExceptionRecord) {
    ffi_guard!(CHANNEL, "RtlRestoreContext", (), {
        if ctx.is_null() {
            return;
        }
        // SAFETY: `ctx` est un CONTEXT invité valide ; la reprise abandonne les trames Rust
        // sous la cible (aucun verrou détenu ici).
        unsafe { restore_context(&*ctx) };
    })
}

// ------------------------------------------------------------------------------------------
// Dispatch (phase 1)
// ------------------------------------------------------------------------------------------

/// Termine un traitement d'exception « géré » : appelle les gestionnaires de continuation.
///
/// # Safety
/// `rec` et `ctx` valides.
unsafe fn call_continue_handlers(rec: *mut ExceptionRecord, ctx: *mut Context) {
    // SAFETY: contrat de l'appelant.
    unsafe { vectored_call(&VCH_LIST, rec, ctx) };
}

/// Phase 1 : gestionnaires vectoriels, puis remontée des trames avec appel des handlers.
///
/// Retourne `true` si l'exécution doit reprendre (`ctx` éventuellement modifié) ; `false` si
/// aucun handler n'a traité l'exception. Si un handler décide de dérouler (`RtlUnwindEx`), la
/// fonction ne revient pas : le contrôle passe directement au code invité cible.
///
/// # Safety
/// `rec` et `ctx` doivent être valides et décrire l'exception courante du thread invité.
pub(crate) unsafe fn dispatch_exception(rec: *mut ExceptionRecord, ctx: *mut Context) -> bool {
    // SAFETY: contrat de l'appelant.
    unsafe {
        // Les premières exceptions livrées au titre sont toujours journalisées : un gestionnaire de
        // plantage du titre (vectoriel ou filtre) les consomme sans autre trace.
        static LOGGED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        if LOGGED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 8 {
            let where_ = sb_loader::module_manager()
                .describe_address((*ctx).rip as usize)
                .unwrap_or_else(|| "<hors module PE>".to_string());
            sb_warn!(
                CHANNEL,
                "exception livrée au titre : code={:#010x} flags={:#x} rip={:#x} ({where_}) params={:x?}",
                (*rec).exception_code,
                (*rec).exception_flags,
                (*ctx).rip,
                &(&(*rec).exception_information)[..((*rec).number_parameters as usize).min(4)]
            );
            seh_trace!(
                "  thread : {}",
                sb_loader::teb::describe_current_teb().trim_end()
            );
        }
        seh_trace!(
            "dispatch code={:#010x} flags={:#x} rip={:#x} rsp={:#x}",
            (*rec).exception_code,
            (*rec).exception_flags,
            (*ctx).rip,
            (*ctx).rsp
        );
        if vectored_call(&VEH_LIST, rec, ctx) {
            call_continue_handlers(rec, ctx);
            return true;
        }
        let root: Context = *ctx;
        let mut cur: Context = root;
        let mut new: Context = root;
        for _ in 0..MAX_FRAMES {
            let Some(info) = unwind_frame(UNW_FLAG_EHANDLER, &cur, &mut new) else {
                seh_trace!(
                    "  remontée interrompue : pc={:#x} rsp={:#x} (aucune trame .pdata/hôte) hôte={}",
                    cur.rip,
                    cur.rsp,
                    sb_loader::signal::describe_host_address(cur.rip as usize)
                        .unwrap_or_else(|| "?".to_string())
                );
                break;
            };
            seh_trace!(
                "  trame pc={:#x} rsp={:#x} handler={:#x} hop_hote={}",
                info.control_pc,
                cur.rsp,
                info.handler,
                info.host_hop
            );
            if info.handler != 0 {
                let mut disp = DispatcherContext {
                    control_pc: info.control_pc,
                    image_base: info.image_base as u64,
                    function_entry: info.function_entry,
                    establisher_frame: info.establisher,
                    target_ip: 0,
                    context_record: &raw mut cur,
                    language_handler: info.handler,
                    handler_data: info.handler_data,
                    history_table: core::ptr::null_mut(),
                    scope_index: 0,
                    fill0: 0,
                };
                seh_trace!(
                    "  frame pc={:#x} establisher={:#x} handler={:#x}",
                    info.control_pc,
                    info.establisher,
                    info.handler
                );
                let disposition = with_host_call(&root, || {
                    call_routine(
                        info.handler,
                        rec,
                        info.establisher,
                        &raw mut cur,
                        &raw mut disp,
                    )
                });
                seh_trace!("  -> disposition {disposition}");
                match disposition {
                    EXCEPTION_DISPOSITION_CONTINUE_EXECUTION => {
                        if (*rec).exception_flags & EXCEPTION_NONCONTINUABLE != 0 {
                            raise_noncontinuable(rec, ctx);
                        }
                        call_continue_handlers(rec, ctx);
                        return true;
                    }
                    EXCEPTION_DISPOSITION_CONTINUE_SEARCH
                    | EXCEPTION_DISPOSITION_NESTED_EXCEPTION
                    | EXCEPTION_DISPOSITION_COLLIDED_UNWIND => {}
                    _ => {
                        let mut bad = ExceptionRecord::new(
                            STATUS_INVALID_DISPOSITION,
                            EXCEPTION_NONCONTINUABLE,
                        );
                        bad.exception_record = rec;
                        bad.exception_address = info.handler;
                        raise_internal(&mut bad, ctx);
                    }
                }
            }
            cur = new;
            if cur.rip == 0 {
                break;
            }
        }
        false
    }
}

/// Lève `STATUS_NONCONTINUABLE_EXCEPTION` (un handler a demandé de reprendre une exception
/// non continuable).
///
/// # Safety
/// `rec` et `ctx` valides.
unsafe fn raise_noncontinuable(rec: *mut ExceptionRecord, ctx: *mut Context) -> ! {
    let mut nc = ExceptionRecord::new(STATUS_NONCONTINUABLE_EXCEPTION, EXCEPTION_NONCONTINUABLE);
    nc.exception_record = rec;
    // SAFETY: contrat de l'appelant.
    unsafe {
        nc.exception_address = (*rec).exception_address;
        raise_internal(&mut nc, ctx)
    }
}

/// Chemin complet d'une exception : dispatch, reprise ou traitement « non géré ».
///
/// # Safety
/// `rec` et `ctx` valides ; `ctx` est le contexte de l'appelant de `RaiseException`.
pub(crate) unsafe fn raise_internal(rec: *mut ExceptionRecord, ctx: *mut Context) -> ! {
    // SAFETY: contrat de l'appelant.
    unsafe {
        if dispatch_exception(rec, ctx) {
            restore_context(&*ctx);
        }
        unhandled_exception(rec, ctx)
    }
}

/// Consulte le filtre posé par `SetUnhandledExceptionFilter` ; reprend l'exécution si celui-ci
/// renvoie `EXCEPTION_CONTINUE_EXECUTION` (exception continuable). Sinon retourne.
///
/// # Safety
/// `rec` et `ctx` valides.
unsafe fn run_top_level_filter(rec: *mut ExceptionRecord, ctx: *mut Context) {
    let filter = crate::kernel32::top_level_exception_filter();
    if filter == 0 {
        return;
    }
    let mut ptrs = ExceptionPointers {
        exception_record: rec,
        context_record: ctx,
    };
    // SAFETY: `filter` est un `LPTOP_LEVEL_EXCEPTION_FILTER` Win64 posé par l'invité.
    unsafe {
        let f: unsafe extern "win64" fn(*mut ExceptionPointers) -> i32 =
            core::mem::transmute::<usize, _>(filter);
        let resume = with_host_call(&*ctx, || f(&mut ptrs));
        if resume == EXCEPTION_CONTINUE_EXECUTION
            && (*rec).exception_flags & EXCEPTION_NONCONTINUABLE == 0
        {
            restore_context(&*ctx);
        }
    }
}

/// Exception non gérée : filtre de niveau supérieur (`SetUnhandledExceptionFilter`), sinon
/// rapport de crash puis fin du processus avec le code de l'exception.
///
/// # Safety
/// `rec` et `ctx` valides.
pub(crate) unsafe fn unhandled_exception(rec: *mut ExceptionRecord, ctx: *mut Context) -> ! {
    // SAFETY: contrat de l'appelant.
    unsafe {
        let r = &*rec;
        let where_ = sb_loader::module_manager()
            .describe_address((*ctx).rip as usize)
            .unwrap_or_else(|| "<hors module PE>".to_string());
        sb_err!(
            CHANNEL,
            "exception non gérée code={:#010x} flags={:#x} adresse={:#x} ({where_})",
            r.exception_code,
            r.exception_flags,
            r.exception_address
        );
        run_top_level_filter(rec, ctx);
        if r.exception_code == cxx::CXX_EXCEPTION_CODE {
            // Exception C++ sans catch : `std::terminate()` (gestionnaire invité éventuel, puis abort).
            cxx::terminate_now();
        }
        let n = (r.number_parameters as usize).min(EXCEPTION_MAXIMUM_PARAMETERS);
        sb_loader::signal::exit_with_guest_exception_report(
            r.exception_code,
            r.exception_flags,
            r.exception_address,
            &r.exception_information[..n],
            &*ctx,
        )
    }
}

// ------------------------------------------------------------------------------------------
// Points d'entrée « raise » (captures de contexte en asm)
// ------------------------------------------------------------------------------------------

/// Génère un point d'entrée `naked` à ≤ 4 arguments registre qui capture le contexte de
/// l'appelant (RSP = pile après retour, RIP = adresse de retour) puis appelle
/// `$imp(a1, a2, a3, a4, context)` (le contexte capturé est le 5ᵉ argument, sur la pile).
///
/// Disposition (RSP après `sub`) : `0x00..0x20` home space, `0x20` 5ᵉ argument, `0x30..0x500`
/// CONTEXT (aligné 16), `0x500` remplissage, `0x508` adresse de retour, `0x510..` home space de
/// l'appelant (où les arguments registre sont sauvegardés).
macro_rules! capture_thunk {
    ($(#[$m:meta])* $vis:vis fn $name:ident($($arg:ident : $ty:ty),*) => $imp:path) => {
        $(#[$m])*
        #[unsafe(naked)]
        $vis extern "win64" fn $name($($arg: $ty),*) {
            core::arch::naked_asm!(
                "sub rsp, 0x508",
                "mov [rsp + 0x510], rcx",
                "mov [rsp + 0x518], rdx",
                "mov [rsp + 0x520], r8",
                "mov [rsp + 0x528], r9",
                "lea rcx, [rsp + 0x30]",
                "call {capture}",
                "lea rax, [rsp + 0x510]",
                "mov [rsp + 0x30 + 0x98], rax",
                "mov rax, [rsp + 0x508]",
                "mov [rsp + 0x30 + 0xF8], rax",
                "lea rax, [rsp + 0x30]",
                "mov [rsp + 0x20], rax",
                "mov rcx, [rsp + 0x510]",
                "mov rdx, [rsp + 0x518]",
                "mov r8, [rsp + 0x520]",
                "mov r9, [rsp + 0x528]",
                "call {imp}",
                "int3",
                capture = sym $crate::kernel_ext::rtl::RtlCaptureContext,
                imp = sym $imp,
            )
        }
    };
}

pub(crate) use capture_thunk;

capture_thunk! {
    /// `RaiseException(code, flags, nargs, args)`.
    pub(crate) fn RaiseException(code: u32, flags: u32, nargs: u32, args: *const usize)
        => raise_exception_impl
}

capture_thunk! {
    /// `RtlRaiseException(record)`.
    pub(crate) fn RtlRaiseException(rec: *mut ExceptionRecord)
        => rtl_raise_exception_impl
}

capture_thunk! {
    /// `RtlRaiseStatus(status)` : exception non continuable de code `status`.
    pub(crate) fn RtlRaiseStatus(status: u32)
        => rtl_raise_status_impl
}

/// Corps de `RaiseException` (appelé par le thunk avec le contexte capturé).
extern "win64" fn raise_exception_impl(
    code: u32,
    flags: u32,
    nargs: u32,
    args: *const usize,
    ctx: *mut Context,
) {
    ffi_guard!(CHANNEL, "RaiseException", (), {
        let mut rec = ExceptionRecord::new(code, flags & EXCEPTION_NONCONTINUABLE);
        let n = if args.is_null() {
            0
        } else {
            (nargs as usize).min(EXCEPTION_MAXIMUM_PARAMETERS)
        };
        rec.number_parameters = n as u32;
        if n > 0 {
            // SAFETY: l'appelant fournit `nargs` valeurs lisibles (borné à 15).
            unsafe {
                core::ptr::copy_nonoverlapping(args, rec.exception_information.as_mut_ptr(), n)
            };
        }
        // SAFETY: `ctx` est le contexte capturé par le thunk ; `rec` vit pendant tout le
        // dispatch (la trame courante n'est abandonnée qu'après la reprise).
        unsafe {
            rec.exception_address = (*ctx).rip as usize;
            raise_internal(&raw mut rec, ctx)
        }
    });
    fatal("RaiseException : panique interne du dispatcher");
}

extern "win64" fn rtl_raise_exception_impl(
    rec: *mut ExceptionRecord,
    _b: u64,
    _c: u64,
    _d: u64,
    ctx: *mut Context,
) {
    ffi_guard!(CHANNEL, "RtlRaiseException", (), {
        // SAFETY: `rec` est un EXCEPTION_RECORD invité valide ; `ctx` capturé par le thunk.
        unsafe {
            (*rec).exception_address = (*ctx).rip as usize;
            raise_internal(rec, ctx)
        }
    });
    fatal("RtlRaiseException : panique interne du dispatcher");
}

extern "win64" fn rtl_raise_status_impl(status: u32, _b: u64, _c: u64, _d: u64, ctx: *mut Context) {
    ffi_guard!(CHANNEL, "RtlRaiseStatus", (), {
        let mut rec = ExceptionRecord::new(status, EXCEPTION_NONCONTINUABLE);
        // SAFETY: `ctx` capturé par le thunk.
        unsafe {
            rec.exception_address = (*ctx).rip as usize;
            raise_internal(&raw mut rec, ctx)
        }
    });
    fatal("RtlRaiseStatus : panique interne du dispatcher");
}

/// `RtlDispatchException(ExceptionRecord, ContextRecord)` : retourne `TRUE` si l'exécution doit
/// reprendre.
pub(crate) extern "win64" fn RtlDispatchException(
    rec: *mut ExceptionRecord,
    ctx: *mut Context,
) -> u8 {
    ffi_guard!(CHANNEL, "RtlDispatchException", 0, {
        if rec.is_null() || ctx.is_null() {
            return 0;
        }
        // SAFETY: pointeurs invités valides (contrat de l'API).
        u8::from(unsafe { dispatch_exception(rec, ctx) })
    })
}

// ------------------------------------------------------------------------------------------
// Phase 2 : RtlUnwindEx / RtlUnwind
// ------------------------------------------------------------------------------------------

/// Marche de phase 2 depuis `start` jusqu'à la trame `target_frame` (handlers appelés avec
/// `EXCEPTION_UNWINDING`). À la sortie, `*context` est le contexte de la trame cible (registres
/// non volatils restaurés, RSP/RIP de la trame cible).
///
/// # Safety
/// `rec` (si non nul) et `out_ctx` (si non nul) valides ; la pile décrite par `start` lisible.
unsafe fn unwind_walk(
    target_frame: u64,
    target_ip: u64,
    rec: *mut ExceptionRecord,
    context: *mut Context,
    history: *mut c_void,
    start: &Context,
) {
    // SAFETY: pointeurs valides (contrat de l'appelant et variables locales).
    unsafe {
        (*rec).exception_flags |= EXCEPTION_UNWINDING;
        if target_frame == 0 {
            (*rec).exception_flags |= EXCEPTION_EXIT_UNWIND;
        }
        // `start` peut vivre dans la mémoire de `context` (`ContextRecord` de l'appelant) : on
        // garde une copie locale, point de reprise des appels hôte, avant de l'écraser.
        let anchor: Context = *start;
        *context = anchor;
        let mut new: Context = anchor;
        seh_trace!(
            "unwind target_frame={:#x} target_ip={:#x} rip={:#x} rsp={:#x} code={:#010x}",
            target_frame,
            target_ip,
            anchor.rip,
            anchor.rsp,
            (*rec).exception_code
        );
        let mut pending: Option<FrameInfo> = None;
        let mut iterations = 0usize;
        loop {
            iterations += 1;
            if iterations > MAX_FRAMES {
                fatal("RtlUnwindEx : pile invalide (trop de trames)");
            }
            let info = match pending.take() {
                Some(i) => i,
                None => match unwind_frame(UNW_FLAG_UHANDLER, &*context, &mut new) {
                    Some(i) => i,
                    None if target_frame == 0 => break,
                    None => {
                        let mut bad =
                            ExceptionRecord::new(STATUS_BAD_STACK, EXCEPTION_NONCONTINUABLE);
                        bad.exception_record = rec;
                        bad.exception_address = (*context).rip as usize;
                        sb_err!(
                            CHANNEL,
                            "RtlUnwindEx : trame cible {target_frame:#x} introuvable (rip={:#x})",
                            (*context).rip
                        );
                        unhandled_exception(&raw mut bad, context)
                    }
                },
            };
            if info.host_hop {
                *context = new;
                continue;
            }
            if info.establisher == 0 {
                break;
            }
            if info.handler != 0 {
                if target_frame != 0 && info.establisher > target_frame {
                    sb_err!(
                        CHANNEL,
                        "RtlUnwindEx : cible invalide (cadre {:#x} > {target_frame:#x})",
                        info.establisher
                    );
                    let mut bad = ExceptionRecord::new(
                        STATUS_INVALID_UNWIND_TARGET,
                        EXCEPTION_NONCONTINUABLE,
                    );
                    bad.exception_record = rec;
                    unhandled_exception(&raw mut bad, context);
                }
                if info.establisher == target_frame {
                    (*rec).exception_flags |= EXCEPTION_TARGET_UNWIND;
                }
                let mut disp = DispatcherContext {
                    control_pc: info.control_pc,
                    image_base: info.image_base as u64,
                    function_entry: info.function_entry,
                    establisher_frame: info.establisher,
                    target_ip,
                    context_record: context,
                    language_handler: info.handler,
                    handler_data: info.handler_data,
                    history_table: history,
                    scope_index: 0,
                    fill0: 0,
                };
                seh_trace!(
                    "  unwind frame pc={:#x} establisher={:#x} handler={:#x}",
                    info.control_pc,
                    info.establisher,
                    info.handler
                );
                let disposition = with_host_call(&anchor, || {
                    call_routine(info.handler, rec, info.establisher, context, &raw mut disp)
                });
                if disposition == EXCEPTION_DISPOSITION_COLLIDED_UNWIND {
                    // Le handler a remplacé le contexte : on repart de celui-ci, la trame du
                    // handler est dépilée sans le rappeler (collision), `ScopeIndex` conservé.
                    *context = *disp.context_record;
                    new = *context;
                    let _ = virtual_unwind(
                        UNW_FLAG_NHANDLER,
                        info.image_base,
                        info.control_pc as usize,
                        info.function_entry,
                        &mut new,
                        None,
                    );
                    (*rec).exception_flags |= EXCEPTION_COLLIDED_UNWIND;
                    pending = Some(FrameInfo { handler: 0, ..info });
                    continue;
                }
                (*rec).exception_flags &= !EXCEPTION_COLLIDED_UNWIND;
            }
            if info.establisher == target_frame {
                break;
            }
            *context = new;
        }
    }
}

/// Déroulage de phase 2 depuis le contexte `start`, jusqu'à la trame `target_frame`, puis
/// reprise à `target_ip` avec `RAX = retval`.
///
/// `out_ctx` (facultatif) sert de CONTEXT de travail (comme le paramètre `ContextRecord` de
/// `RtlUnwindEx`).
///
/// # Safety
/// `rec` (si non nul) et `out_ctx` (si non nul) valides ; la pile décrite par `start` lisible.
unsafe fn unwind_to(
    target_frame: u64,
    target_ip: u64,
    rec: *mut ExceptionRecord,
    retval: u64,
    out_ctx: *mut Context,
    history: *mut c_void,
    start: &Context,
) -> ! {
    let mut local_rec = ExceptionRecord::new(STATUS_UNWIND, 0);
    let mut local_ctx = *start;
    let rec: *mut ExceptionRecord = if rec.is_null() {
        local_rec.exception_address = start.rip as usize;
        &raw mut local_rec
    } else {
        rec
    };
    let context: *mut Context = if out_ctx.is_null() {
        &raw mut local_ctx
    } else {
        out_ctx
    };
    // SAFETY: pointeurs valides (contrat de l'appelant et variables locales).
    unsafe {
        unwind_walk(target_frame, target_ip, rec, context, history, start);
        (*context).rax = retval;
        (*context).rip = target_ip;
        restore_context(&*context)
    }
}

/// Génère le point d'entrée `naked` de `RtlUnwindEx` : capture le contexte de l'appelant puis
/// appelle `unwind_ex_impl(a1, a2, a3, a4, ContextRecord, HistoryTable, start_context)`.
///
/// Disposition (RSP après `sub 0x518`) : `0x20..0x38` arguments 5 à 7, `0x40..0x510` CONTEXT,
/// `0x518` adresse de retour, `0x520..0x540` home space de l'appelant, `0x540`/`0x548`
/// arguments 5 et 6 de `RtlUnwindEx`.
#[unsafe(naked)]
pub(crate) extern "win64" fn RtlUnwindEx(
    target_frame: u64,
    target_ip: u64,
    rec: *mut ExceptionRecord,
    retval: u64,
    ctx: *mut Context,
    history: *mut c_void,
) {
    core::arch::naked_asm!(
        "sub rsp, 0x518",
        "mov [rsp + 0x520], rcx",
        "mov [rsp + 0x528], rdx",
        "mov [rsp + 0x530], r8",
        "mov [rsp + 0x538], r9",
        "lea rcx, [rsp + 0x40]",
        "call {capture}",
        "lea rax, [rsp + 0x520]",
        "mov [rsp + 0x40 + 0x98], rax",
        "mov rax, [rsp + 0x518]",
        "mov [rsp + 0x40 + 0xF8], rax",
        "mov rax, [rsp + 0x540]",
        "mov [rsp + 0x20], rax",
        "mov rax, [rsp + 0x548]",
        "mov [rsp + 0x28], rax",
        "lea rax, [rsp + 0x40]",
        "mov [rsp + 0x30], rax",
        "mov rcx, [rsp + 0x520]",
        "mov rdx, [rsp + 0x528]",
        "mov r8, [rsp + 0x530]",
        "mov r9, [rsp + 0x538]",
        "call {imp}",
        "int3",
        capture = sym RtlCaptureContext,
        imp = sym unwind_ex_impl,
    )
}

extern "win64" fn unwind_ex_impl(
    target_frame: u64,
    target_ip: u64,
    rec: *mut ExceptionRecord,
    retval: u64,
    out_ctx: *mut Context,
    history: *mut c_void,
    start: *mut Context,
) {
    ffi_guard!(CHANNEL, "RtlUnwindEx", (), {
        // SAFETY: `start` est le contexte capturé par le thunk ; les autres pointeurs viennent
        // de l'invité (contrat de `RtlUnwindEx`).
        unsafe {
            unwind_to(
                target_frame,
                target_ip,
                rec,
                retval,
                out_ctx,
                history,
                &*start,
            )
        }
    });
    fatal("RtlUnwindEx : panique interne du dispatcher");
}

capture_thunk! {
    /// `RtlUnwind(TargetFrame, TargetIp, ExceptionRecord, ReturnValue)` (ancienne forme).
    pub(crate) fn RtlUnwind(target_frame: u64, target_ip: u64, rec: *mut ExceptionRecord, retval: u64)
        => rtl_unwind_impl
}

extern "win64" fn rtl_unwind_impl(
    target_frame: u64,
    target_ip: u64,
    rec: *mut ExceptionRecord,
    retval: u64,
    start: *mut Context,
) {
    ffi_guard!(CHANNEL, "RtlUnwind", (), {
        // SAFETY: idem `unwind_ex_impl`.
        unsafe {
            unwind_to(
                target_frame,
                target_ip,
                rec,
                retval,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                &*start,
            )
        }
    });
    fatal("RtlUnwind : panique interne du dispatcher");
}

/// `RtlCaptureContext2(ContextRecord)` : capture complète (même contrat que `RtlCaptureContext`).
pub(crate) extern "win64" fn RtlCaptureContext2(ctx: *mut Context) {
    ffi_guard!(CHANNEL, "RtlCaptureContext2", (), {
        if ctx.is_null() {
            return;
        }
        // `RtlCaptureContext` capture l'appelant direct ; ici l'appelant est cette fonction, on
        // copie donc le contexte puis on rétablit RSP/RIP de l'appelant d'origine.
        // SAFETY: `ctx` est un CONTEXT invité valide en écriture.
        unsafe {
            RtlCaptureContext(ctx);
            (*ctx).context_flags = CONTEXT_CAPTURE_FLAGS;
        }
    });
}

// ------------------------------------------------------------------------------------------
// __C_specific_handler (__try / __except / __finally)
// ------------------------------------------------------------------------------------------

/// `SCOPE_TABLE` : `Count` suivi de `ScopeRecord[Count]` (16 octets chacun).
#[repr(C)]
#[derive(Clone, Copy)]
struct ScopeRecord {
    begin_address: u32,
    end_address: u32,
    handler_address: u32,
    jump_target: u32,
}

const _: () = assert!(size_of::<ScopeRecord>() == 16);

/// `__C_specific_handler` : interprète la `C_SCOPE_TABLE` d'une fonction (HandlerData).
///
/// - phase 1 : évalue les filtres des scopes `__try/__except` contenant `ControlPc` ;
///   `EXCEPTION_EXECUTE_HANDLER` → `RtlUnwindEx` vers le `JumpTarget` ; `CONTINUE_EXECUTION` →
///   reprise ;
/// - phase 2 : appelle les terminaisons `__finally` (`HandlerAddress(TRUE, frame)`) des scopes
///   traversés, sauf si la cible du déroulage est le `__except` du scope.
pub(crate) extern "win64" fn __C_specific_handler(
    rec: *mut ExceptionRecord,
    frame: u64,
    ctx: *mut Context,
    disp: *mut DispatcherContext,
) -> i32 {
    ffi_guard!(
        CHANNEL,
        "__C_specific_handler",
        EXCEPTION_DISPOSITION_CONTINUE_SEARCH,
        {
            // SAFETY: arguments fournis par le dispatcher (ou par un appel invité conforme).
            unsafe { c_specific_handler(rec, frame, ctx, disp) }
        }
    )
}

unsafe fn c_specific_handler(
    rec: *mut ExceptionRecord,
    frame: u64,
    ctx: *mut Context,
    disp: *mut DispatcherContext,
) -> i32 {
    // SAFETY: structures valides (contrat de l'appelant) ; `handler_data` pointe sur la
    // SCOPE_TABLE de l'image.
    unsafe {
        let d = &mut *disp;
        let table = d.handler_data;
        if table == 0 {
            return EXCEPTION_DISPOSITION_CONTINUE_SEARCH;
        }
        let count = core::ptr::read_unaligned(table as *const u32) as usize;
        let records = (table + 4) as *const ScopeRecord;
        let image = d.image_base as usize;
        let pc = d.control_pc as usize;
        let flags = (*rec).exception_flags;
        let in_scope = |s: &ScopeRecord| {
            pc >= image + s.begin_address as usize && pc < image + s.end_address as usize
        };

        if flags & (EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND) != 0 {
            let mut i = d.scope_index as usize;
            while i < count {
                let s = core::ptr::read_unaligned(records.add(i));
                if in_scope(&s) {
                    if s.jump_target != 0 {
                        if flags & EXCEPTION_TARGET_UNWIND != 0
                            && d.target_ip as usize == image + s.jump_target as usize
                        {
                            break;
                        }
                    } else {
                        d.scope_index = (i + 1) as u32;
                        seh_trace!(
                            "  __finally {:#x} frame={frame:#x}",
                            image + s.handler_address as usize
                        );
                        let f: unsafe extern "win64" fn(u8, u64) =
                            core::mem::transmute::<usize, _>(image + s.handler_address as usize);
                        let resume = *d.context_record;
                        with_host_call(&resume, || f(1, frame));
                    }
                }
                i += 1;
            }
            return EXCEPTION_DISPOSITION_CONTINUE_SEARCH;
        }

        let mut i = d.scope_index as usize;
        while i < count {
            let s = core::ptr::read_unaligned(records.add(i));
            if in_scope(&s) && s.jump_target != 0 {
                let verdict = if s.handler_address == 1 {
                    EXCEPTION_EXECUTE_HANDLER
                } else {
                    let mut ptrs = ExceptionPointers {
                        exception_record: rec,
                        context_record: ctx,
                    };
                    let f: unsafe extern "win64" fn(*mut ExceptionPointers, u64) -> i32 =
                        core::mem::transmute::<usize, _>(image + s.handler_address as usize);
                    let resume = *ctx;
                    with_host_call(&resume, || f(&mut ptrs, frame))
                };
                seh_trace!("  __except filtre scope {i} -> {verdict}");
                if verdict == EXCEPTION_CONTINUE_EXECUTION {
                    return EXCEPTION_DISPOSITION_CONTINUE_EXECUTION;
                }
                if verdict == EXCEPTION_EXECUTE_HANDLER {
                    // Le déroulage repart du contexte d'origine de l'exception : c'est le point
                    // de reprise enregistré par le dispatcher pour cet appel.
                    let start =
                        host_resume_for(&raw const i as u64).unwrap_or_else(|| *d.context_record);
                    unwind_to(
                        frame,
                        (image + s.jump_target as usize) as u64,
                        rec,
                        0,
                        d.context_record,
                        d.history_table,
                        &start,
                    );
                }
            }
            i += 1;
        }
        EXCEPTION_DISPOSITION_CONTINUE_SEARCH
    }
}

// ------------------------------------------------------------------------------------------
// Exports
// ------------------------------------------------------------------------------------------

/// Exports de la famille kernel/ntdll (`kernel32`, `kernelbase`, `ntdll`, ...).
pub(crate) fn kernel_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:literal, $f:path) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("RaiseException", RaiseException);
    reg!("RtlRaiseException", RtlRaiseException);
    reg!("RtlRaiseStatus", RtlRaiseStatus);
    reg!("RtlDispatchException", RtlDispatchException);
    reg!("RtlUnwindEx", RtlUnwindEx);
    reg!("RtlUnwind", RtlUnwind);
    reg!("RtlRestoreContext", RtlRestoreContext);
    reg!("RtlCaptureContext2", RtlCaptureContext2);
    reg!("AddVectoredExceptionHandler", AddVectoredExceptionHandler);
    reg!(
        "RemoveVectoredExceptionHandler",
        RemoveVectoredExceptionHandler
    );
    reg!("AddVectoredContinueHandler", AddVectoredContinueHandler);
    reg!(
        "RemoveVectoredContinueHandler",
        RemoveVectoredContinueHandler
    );
    reg!("__C_specific_handler", __C_specific_handler);
    m
}

/// Exports de la famille CRT (`vcruntime140*`, `ucrtbase`, `msvcrt`).
pub(crate) fn crt_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:literal, $f:path) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("__C_specific_handler", __C_specific_handler);
    cxx::register(&mut m);
    m
}

/// Point d'entrée appelé par `fault_entry` (sb-loader) pour une faute matérielle invitée.
///
/// Dispatche l'exception ; si un handler la traite, le contexte est restauré (pas de retour).
/// Sinon, le filtre de niveau supérieur (`SetUnhandledExceptionFilter`) est consulté (reprise si
/// `EXCEPTION_CONTINUE_EXECUTION`) puis la fonction retourne `false` : l'appelant produit le
/// rapport de crash historique.
pub(crate) unsafe extern "win64" fn signal_dispatch(rec: *mut u8, ctx: *mut Context) -> bool {
    ffi_guard!(CHANNEL, "signal_dispatch", false, {
        let rec = rec.cast::<ExceptionRecord>();
        // SAFETY: `fault_entry` fournit un EXCEPTION_RECORD et un CONTEXT valides (pile invitée).
        unsafe {
            if dispatch_exception(rec, ctx) {
                restore_context(&*ctx);
            }
            run_top_level_filter(rec, ctx);
        }
        false
    })
}

#[cfg(test)]
mod tests;
