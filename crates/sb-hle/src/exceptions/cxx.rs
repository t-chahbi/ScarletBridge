//! Exceptions C++ MSVC x64 : `_CxxThrowException`, `__CxxFrameHandler3/4`, `std::terminate`,
//! `__current_exception[_context]`, `__CxxUnhandledExceptionFilter`.
//!
//! # Protocole
//!
//! `_CxxThrowException(object, ThrowInfo*)` lève `0xE06D7363` (non continuable) avec
//! `ExceptionInformation = { magic 0x19930520, objet, ThrowInfo*, ImageBase }` ; les RVA de
//! `ThrowInfo` / `CatchableTypeArray` / `CatchableType` / `TypeDescriptor` sont relatives à
//! l'ImageBase du 4ᵉ paramètre. `throw;` (`object == NULL && ThrowInfo == NULL`) relance
//! l'exception en cours (`__current_exception`).
//!
//! Le handler de langage (`__CxxFrameHandler3/4`, `HandlerData` = RVA de `FuncInfo`) :
//!
//! - phase 2 (`EXCEPTION_UNWINDING`) : exécute les funclets de destruction de la `UnwindMap`
//!   depuis l'état courant (déduit de `IPtoStateMap`) jusqu'à -1, sauf pour la trame cible ;
//! - phase 1 : cherche, dans les `TryBlockMap` couvrant l'état, un `HandlerType` compatible
//!   (`catch (...)`, ou type correspondant via `ThrowInfo` → `CatchableTypeArray`). Sur
//!   correspondance, **il exécute lui-même le catch** (comme MSVC) : déroulage des trames
//!   inférieures (`unwind_walk`, destructeurs), destruction des objets locaux jusqu'à
//!   `tryLow`, copie de l'objet dans la variable d'exception, appel du funclet catch
//!   `funclet(rcx = 0, rdx = EstablisherFrame)` — *avant* de libérer la pile du lanceur, ce qui
//!   garde valide l'objet lancé —, puis reprise à l'adresse de continuation renvoyée (FH3 :
//!   adresse absolue ; FH4 : voir `desc::Fh4`) avec le contexte de la trame cible.
//! - un `throw` depuis un funclet catch : la remontée traverse la trame hôte du funclet et
//!   reprend dans la trame parente ; la table [`ACTIVE`] indique alors que l'état effectif de
//!   cette trame est `tryLow` (les objets du corps du `try` sont déjà détruits) et que ce bloc
//!   `try` (et ceux qu'il contient) ne doit plus être considéré. *Inférence* : MSVC obtient le
//!   même résultat via `UnwindHelp`.
//!
//! Limites : spécifications d'exception (`throw(...)`), `noexcept` violé (→ `terminate`),
//! `catch` sur types de pointeurs avec conversions complexes, et `_set_se_translator` ne sont pas
//! gérés.

pub(crate) mod desc;
mod eptr;
pub(crate) mod synth;
#[cfg(test)]
mod tests;

use core::ffi::c_void;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use sb_common::{ffi_guard, sb_err};
use sb_loader::unwind::{Context, image_base_for_pc};

use self::desc::{FuncDesc, HT_REFERENCE, Handler, TryBlock, rd_i32, rd_u32};
use super::{
    CHANNEL, DispatcherContext, EXCEPTION_DISPOSITION_CONTINUE_SEARCH, EXCEPTION_EXIT_UNWIND,
    EXCEPTION_NONCONTINUABLE, EXCEPTION_TARGET_UNWIND, EXCEPTION_UNWIND, EXCEPTION_UNWINDING,
    ExceptionPointers, ExceptionRecord, capture_thunk, host_resume_for, raise_internal,
    restore_context, seh_trace, unwind_walk, with_host_call,
};

/// Code d'exception C++ MSVC (`'msc' | 0xE0000000`).
pub const CXX_EXCEPTION_CODE: u32 = 0xE06D_7363;
const CXX_MAGIC_BASE: usize = 0x1993_0520;

const CT_SIMPLE_TYPE: u32 = 1;
const CT_BY_REFERENCE_ONLY: u32 = 2;
const CT_HAS_VIRTUAL_BASE: u32 = 4;
const TI_IS_CONST: u32 = 1;
const TI_IS_VOLATILE: u32 = 2;

// ------------------------------------------------------------------------------------------
// État par thread : exception courante et catch actifs
// ------------------------------------------------------------------------------------------

thread_local! {
    /// `EXCEPTION_RECORD*` de l'exception en cours de traitement dans un bloc catch.
    static CUR_EXC: Cell<*mut c_void> = const { Cell::new(core::ptr::null_mut()) };
    /// `CONTEXT*` associé.
    static CUR_CTX: Cell<*mut c_void> = const { Cell::new(core::ptr::null_mut()) };
    static ACTIVE: RefCell<Vec<ActiveCatch>> = const { RefCell::new(Vec::new()) };
}

/// Cellule de `__current_exception()`.
pub(crate) fn current_exception_cell() -> *mut *mut c_void {
    CUR_EXC.with(Cell::as_ptr)
}

/// Cellule de `__current_exception_context()`.
pub(crate) fn current_exception_context_cell() -> *mut *mut c_void {
    CUR_CTX.with(Cell::as_ptr)
}

/// Bloc catch en cours d'exécution dans une trame.
#[derive(Clone, Copy)]
struct ActiveCatch {
    frame: u64,
    low: i32,
    high: i32,
    /// Adresse locale de la trame hôte qui exécute le funclet (élagage après un saut).
    marker: usize,
}

/// Supprime les catch dont la trame hôte a été abandonnée par un saut vers `new_rsp`.
pub(super) fn prune_dead_catches(new_rsp: u64) {
    let _ = ACTIVE.try_with(|a| a.borrow_mut().retain(|c| c.marker as u64 >= new_rsp));
}

fn active_for(frame: u64) -> Option<ActiveCatch> {
    ACTIVE
        .try_with(|a| a.borrow().iter().rev().find(|c| c.frame == frame).copied())
        .ok()
        .flatten()
}

fn active_covers(frame: u64, tb: &TryBlock) -> bool {
    ACTIVE
        .try_with(|a| {
            a.borrow()
                .iter()
                .any(|c| c.frame == frame && tb.low >= c.low && tb.high <= c.high)
        })
        .unwrap_or(false)
}

// ------------------------------------------------------------------------------------------
// Lecture de ThrowInfo / CatchableType
// ------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub(super) struct CatchableType {
    pub props: u32,
    pub type_rva: i32,
    pub mdisp: i32,
    pub pdisp: i32,
    pub vdisp: i32,
    pub size: i32,
    pub copy_fn: i32,
}

/// # Safety
/// `addr` doit désigner un `CatchableType` valide (28 octets).
unsafe fn read_ct(addr: usize) -> CatchableType {
    // SAFETY: contrat de l'appelant.
    unsafe {
        CatchableType {
            props: rd_u32(addr),
            type_rva: rd_i32(addr + 4),
            mdisp: rd_i32(addr + 8),
            pdisp: rd_i32(addr + 12),
            vdisp: rd_i32(addr + 16),
            size: rd_i32(addr + 20),
            copy_fn: rd_i32(addr + 24),
        }
    }
}

/// Paramètres décodés d'une exception C++ (`None` si l'enregistrement n'en est pas une).
#[derive(Clone, Copy)]
pub(super) struct CxxParams {
    pub object: usize,
    pub throw_info: usize,
    pub base: usize,
}

pub(super) fn cxx_params(rec: &ExceptionRecord) -> Option<CxxParams> {
    if rec.exception_code != CXX_EXCEPTION_CODE || rec.number_parameters < 3 {
        return None;
    }
    let magic = rec.exception_information[0];
    if !(CXX_MAGIC_BASE..=CXX_MAGIC_BASE + 2).contains(&(magic & 0x1FFF_FFFF)) {
        return None;
    }
    let throw_info = rec.exception_information[2];
    let base = if rec.number_parameters >= 4 {
        rec.exception_information[3]
    } else {
        image_base_for_pc(throw_info).unwrap_or(0)
    };
    Some(CxxParams {
        object: rec.exception_information[1],
        throw_info,
        base,
    })
}

/// Comparaison de deux `TypeDescriptor` : même adresse ou même nom décoré.
///
/// # Safety
/// Les deux adresses désignent des `TypeDescriptor` valides.
unsafe fn same_type(a: usize, b: usize) -> bool {
    if a == b {
        return true;
    }
    // SAFETY: noms C terminés par NUL à l'offset 16.
    unsafe {
        let (mut pa, mut pb) = (a + 16, b + 16);
        loop {
            let (ca, cb) = (*(pa as *const u8), *(pb as *const u8));
            if ca != cb {
                return false;
            }
            if ca == 0 {
                return true;
            }
            pa += 1;
            pb += 1;
        }
    }
}

/// Résultat de la mise en correspondance d'un handler.
#[derive(Clone, Copy)]
enum Matched {
    CatchAll,
    Type(CatchableType),
}

/// Le handler `h` attrape-t-il l'exception `p` ?
///
/// # Safety
/// Les tables désignées par `p` et `h` doivent être valides.
unsafe fn match_handler(func_base: usize, p: Option<&CxxParams>, h: &Handler) -> Option<Matched> {
    if h.type_rva == 0 {
        return Some(Matched::CatchAll);
    }
    let p = p?;
    if p.throw_info == 0 {
        return None;
    }
    // SAFETY: tables `ThrowInfo` / `CatchableTypeArray` de l'image de base `p.base`.
    unsafe {
        let attributes = rd_u32(p.throw_info);
        let array = p.base + rd_i32(p.throw_info + 12) as usize;
        let n = rd_i32(array);
        let catch_type = func_base + h.type_rva as usize;
        for i in 0..n.max(0) as usize {
            let ct = read_ct(p.base + rd_i32(array + 4 + i * 4) as usize);
            if ct.props & CT_BY_REFERENCE_ONLY != 0 && h.adjectives & HT_REFERENCE == 0 {
                continue;
            }
            if !same_type(catch_type, p.base + ct.type_rva as usize) {
                continue;
            }
            if attributes & TI_IS_CONST != 0 && h.adjectives & desc::HT_CONST == 0 {
                continue;
            }
            if attributes & TI_IS_VOLATILE != 0 && h.adjectives & desc::HT_VOLATILE == 0 {
                continue;
            }
            return Some(Matched::Type(ct));
        }
    }
    None
}

/// Ajuste `obj` vers le sous-objet décrit par le `PMD` du `CatchableType`.
///
/// # Safety
/// `obj` désigne un objet valide ; en cas de base virtuelle, sa vbtable est lisible.
unsafe fn adjust_this(obj: usize, ct: &CatchableType) -> usize {
    // SAFETY: contrat de l'appelant.
    unsafe {
        let mut p = obj.wrapping_add_signed(ct.mdisp as isize);
        if ct.pdisp >= 0 {
            let vbtable = *(obj.wrapping_add_signed(ct.pdisp as isize) as *const usize);
            let off = rd_i32(vbtable.wrapping_add_signed(ct.vdisp as isize));
            p = p.wrapping_add_signed(off as isize);
        }
        p
    }
}

/// Initialise la variable d'exception du bloc catch (`frame + catch_obj`).
///
/// # Safety
/// `frame` et les tables désignées valides ; la variable est inscriptible.
unsafe fn init_catch_object(p: &CxxParams, ct: &CatchableType, h: &Handler, frame: u64) {
    if h.catch_obj == 0 {
        return;
    }
    let dest = (frame as usize).wrapping_add_signed(h.catch_obj as isize);
    // SAFETY: contrat de l'appelant ; l'objet lancé est valide jusqu'à la fin du catch.
    unsafe {
        let src = adjust_this(p.object, ct);
        if h.adjectives & HT_REFERENCE != 0 {
            *(dest as *mut usize) = src;
        } else if ct.props & CT_SIMPLE_TYPE != 0 {
            let n = ct.size.max(0) as usize;
            if n == 8 && ct.mdisp != 0 {
                // pointeur converti vers une base : valeur ajustée
                *(dest as *mut usize) = adjust_this(*(p.object as *const usize), ct);
            } else {
                core::ptr::copy_nonoverlapping(p.object as *const u8, dest as *mut u8, n);
            }
        } else if ct.copy_fn != 0 {
            let f: unsafe extern "win64" fn(usize, usize, u32) =
                core::mem::transmute::<usize, _>(p.base + ct.copy_fn as usize);
            f(dest, src, u32::from(ct.props & CT_HAS_VIRTUAL_BASE != 0));
        } else {
            core::ptr::copy_nonoverlapping(
                src as *const u8,
                dest as *mut u8,
                ct.size.max(0) as usize,
            );
        }
    }
}

/// Détruit l'objet lancé (`ThrowInfo.pmfnUnwind`), à la fin d'un catch.
///
/// # Safety
/// `p` désigne une exception C++ dont l'objet est encore vivant.
pub(super) unsafe fn destroy_thrown_object(p: &CxxParams) {
    if p.throw_info == 0 || p.object == 0 {
        return;
    }
    // SAFETY: `ThrowInfo` valide ; `pmfnUnwind` est un `void (*)(void *)` Win64 (RVA / base).
    unsafe {
        let d = rd_i32(p.throw_info + 4);
        if d != 0 {
            let f: unsafe extern "win64" fn(usize) =
                core::mem::transmute::<usize, _>(p.base + d as usize);
            f(p.object);
        }
    }
}

// ------------------------------------------------------------------------------------------
// _CxxThrowException
// ------------------------------------------------------------------------------------------

capture_thunk! {
    /// `_CxxThrowException(object, throw_info)`.
    pub(crate) fn _CxxThrowException(object: *mut c_void, throw_info: *const c_void)
        => cxx_throw_impl
}

extern "win64" fn cxx_throw_impl(
    object: *mut c_void,
    throw_info: *const c_void,
    _c: u64,
    _d: u64,
    ctx: *mut Context,
) {
    ffi_guard!(CHANNEL, "_CxxThrowException", (), {
        // SAFETY: `ctx` capturé par le thunk ; `rec` vit pendant le dispatch.
        unsafe { throw_from(object as usize, throw_info as usize, &mut *ctx) }
    });
    terminate_now();
}

/// Lève (ou relance) une exception C++ depuis le contexte invité `ctx`.
///
/// # Safety
/// `ctx` est le contexte de l'appelant invité ; `throw_info` est nul ou un `ThrowInfo` valide.
pub(super) unsafe fn throw_from(object: usize, throw_info: usize, ctx: &mut Context) -> ! {
    let mut rec = ExceptionRecord::new(CXX_EXCEPTION_CODE, EXCEPTION_NONCONTINUABLE);
    if object == 0 && throw_info == 0 {
        // `throw;` : relance l'exception courante.
        let cur = CUR_EXC.with(Cell::get) as *const ExceptionRecord;
        if cur.is_null() {
            sb_err!(CHANNEL, "throw; sans exception active : std::terminate()");
            terminate_now();
        }
        // SAFETY: l'enregistrement courant est vivant pendant l'exécution du catch.
        rec = unsafe { *cur };
        rec.exception_flags = EXCEPTION_NONCONTINUABLE;
    } else {
        let base = image_base_for_pc(throw_info)
            .or_else(|| synth::base_containing(throw_info))
            .unwrap_or(0);
        rec.number_parameters = 4;
        rec.exception_information[..4].copy_from_slice(&[CXX_MAGIC_BASE, object, throw_info, base]);
    }
    rec.exception_record = core::ptr::null_mut();
    rec.exception_address = ctx.rip as usize;
    // SAFETY: contrat de l'appelant.
    unsafe { raise_internal(&raw mut rec, ctx) }
}

// ------------------------------------------------------------------------------------------
// Handlers de langage
// ------------------------------------------------------------------------------------------

/// `__CxxFrameHandler3` (également utilisé pour `__CxxFrameHandler2`).
pub(crate) extern "win64" fn __CxxFrameHandler3(
    rec: *mut ExceptionRecord,
    frame: u64,
    ctx: *mut Context,
    disp: *mut DispatcherContext,
) -> i32 {
    ffi_guard!(
        CHANNEL,
        "__CxxFrameHandler3",
        EXCEPTION_DISPOSITION_CONTINUE_SEARCH,
        {
            // SAFETY: arguments fournis par le dispatcher SEH.
            unsafe { frame_handler(rec, frame, ctx, disp, false) }
        }
    )
}

/// `__CxxFrameHandler4` (tables `FuncInfo4` compressées).
pub(crate) extern "win64" fn __CxxFrameHandler4(
    rec: *mut ExceptionRecord,
    frame: u64,
    ctx: *mut Context,
    disp: *mut DispatcherContext,
) -> i32 {
    ffi_guard!(
        CHANNEL,
        "__CxxFrameHandler4",
        EXCEPTION_DISPOSITION_CONTINUE_SEARCH,
        {
            // SAFETY: idem.
            unsafe { frame_handler(rec, frame, ctx, disp, true) }
        }
    )
}

/// Appelle un funclet `void f(rcx = 0, rdx = frame)`.
///
/// # Safety
/// `addr` est un funclet Win64.
unsafe fn call_funclet(addr: usize, frame: u64) -> usize {
    // SAFETY: contrat de l'appelant.
    unsafe {
        let f: unsafe extern "win64" fn(usize, u64) -> usize =
            core::mem::transmute::<usize, _>(addr);
        f(0, frame)
    }
}

/// Exécute les actions de la `UnwindMap` de `from` jusqu'à `to` (exclu).
///
/// # Safety
/// Métadonnées et trame valides.
unsafe fn unwind_to_state(desc: &FuncDesc, base: usize, frame: u64, from: i32, to: i32) {
    let mut s = from;
    // SAFETY: contrat de l'appelant.
    unsafe {
        for _ in 0..4096 {
            if s == to || s < 0 {
                return;
            }
            let Some(e) = desc.unwind_entry(s) else {
                return;
            };
            if e.action != 0 {
                seh_trace!("  dtor state {s} action {:#x}", base + e.action as usize);
                call_funclet(base + e.action as usize, frame);
            }
            s = e.to_state;
        }
    }
}

unsafe fn frame_handler(
    rec: *mut ExceptionRecord,
    establisher: u64,
    _ctx: *mut Context,
    disp: *mut DispatcherContext,
    v4: bool,
) -> i32 {
    // SAFETY: structures valides fournies par le dispatcher ; tables de l'image.
    unsafe {
        let d = &*disp;
        if d.handler_data == 0 || d.function_entry.is_null() {
            return EXCEPTION_DISPOSITION_CONTINUE_SEARCH;
        }
        let base = d.image_base as usize;
        let info = base + rd_u32(d.handler_data) as usize;
        let desc = if v4 {
            FuncDesc::v4(base, info)
        } else {
            FuncDesc::v3(base, info)
        };
        let flags = (*rec).exception_flags;
        let pc_rva = (d.control_pc as usize).wrapping_sub(base) as u32;
        let begin_rva = (*d.function_entry).begin_address;

        // Funclet catch : la trame logique est celle de la fonction parente.
        let mut frame = establisher;
        'find: for i in 0..desc.try_count() {
            let tb = desc.try_block(i);
            for j in 0..tb.handler_count {
                let h = desc.handler(&tb, j);
                if h.handler_rva == begin_rva {
                    let disp_off = desc.funclet_frame_disp(&h);
                    if disp_off != 0 {
                        frame = *((establisher as usize).wrapping_add_signed(disp_off as isize)
                            as *const u64);
                    }
                    break 'find;
                }
            }
        }
        let state = match active_for(frame) {
            Some(a) => a.low,
            None => desc.ip_to_state(pc_rva),
        };

        if flags & (EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND) != 0 {
            if desc.max_state() != 0 && flags & EXCEPTION_TARGET_UNWIND == 0 {
                unwind_to_state(&desc, base, frame, state, -1);
            }
            return EXCEPTION_DISPOSITION_CONTINUE_SEARCH;
        }

        let params = cxx_params(&*rec);
        if params.is_none() && desc.synchronous_only() {
            return EXCEPTION_DISPOSITION_CONTINUE_SEARCH;
        }
        for i in 0..desc.try_count() {
            let tb = desc.try_block(i);
            if tb.low > state || state > tb.high || active_covers(frame, &tb) {
                continue;
            }
            for j in 0..tb.handler_count {
                let h = desc.handler(&tb, j);
                if let Some(m) = match_handler(base, params.as_ref(), &h) {
                    seh_trace!("  catch trouvé : try {i} handler {j} état {state}");
                    execute_catch(rec, d, frame, base, &desc, &tb, &h, m, params);
                }
            }
        }
        EXCEPTION_DISPOSITION_CONTINUE_SEARCH
    }
}

/// Exécute un bloc catch puis reprend à la continuation (ne revient jamais).
///
/// # Safety
/// Arguments issus de `frame_handler` ; appelé depuis le dispatcher (appel hôte actif).
#[allow(clippy::too_many_arguments)]
unsafe fn execute_catch(
    rec: *mut ExceptionRecord,
    d: &DispatcherContext,
    frame: u64,
    base: usize,
    desc: &FuncDesc,
    tb: &TryBlock,
    h: &Handler,
    m: Matched,
    params: Option<CxxParams>,
) -> ! {
    let marker_local = 0u8;
    let marker = core::hint::black_box(&marker_local) as *const u8 as usize;
    // SAFETY: contrat de l'appelant.
    unsafe {
        let mut saved: ExceptionRecord = *rec;
        saved.exception_flags &= !EXCEPTION_UNWIND;
        let anchor: Context = host_resume_for(marker as u64).unwrap_or(*d.context_record);
        let mut saved_ctx: Context = anchor;

        // 1. Déroulage des trames inférieures (destructeurs, __finally).
        let mut target_ctx: Context = anchor;
        let begin_rva = (*d.function_entry).begin_address;
        let pc_rva = (d.control_pc as usize).wrapping_sub(base) as u32;
        let state = match active_for(frame) {
            Some(a) => a.low,
            None => desc.ip_to_state(pc_rva),
        };
        unwind_walk(
            d.establisher_frame,
            0,
            rec,
            &raw mut target_ctx,
            d.history_table,
            &anchor,
        );
        // 2. Objets locaux de la trame jusqu'au niveau du `try`.
        unwind_to_state(desc, base, frame, state, tb.low);
        // 3. Variable d'exception.
        if let (Matched::Type(ct), Some(p)) = (m, params.as_ref()) {
            init_catch_object(p, &ct, h, frame);
        }
        // 4. Funclet catch, avec exception courante publiée.
        ACTIVE.with(|a| {
            a.borrow_mut().push(ActiveCatch {
                frame,
                low: tb.low,
                high: tb.high,
                marker,
            })
        });
        let prev_exc = CUR_EXC.with(|c| c.replace(&raw mut saved as *mut c_void));
        let prev_ctx = CUR_CTX.with(|c| c.replace(&raw mut saved_ctx as *mut c_void));
        let funclet_addr = base + h.handler_rva as usize;
        let ret = with_host_call(&target_ctx, || call_funclet(funclet_addr, frame));
        CUR_EXC.with(|c| c.set(prev_exc));
        CUR_CTX.with(|c| c.set(prev_ctx));
        ACTIVE.with(|a| {
            let mut a = a.borrow_mut();
            if let Some(pos) = a.iter().rposition(|c| c.marker == marker) {
                a.truncate(pos);
            }
        });
        // 5. Fin normale du catch : l'exception est détruite.
        if let Some(p) = params.as_ref() {
            destroy_thrown_object(p);
        }
        // 6. Continuation.
        let cont = if desc.is_v4() && h.cont_count > 0 {
            let idx = usize::from(ret != 0 && h.cont_count > 1).min(1);
            let c = h.cont[idx];
            if c & 0x8000_0000 != 0 {
                base + begin_rva as usize + (c & 0x7FFF_FFFF) as usize
            } else {
                base + c as usize
            }
        } else {
            ret
        };
        seh_trace!("  catch terminé, continuation {cont:#x}");
        target_ctx.rip = cont as u64;
        restore_context(&target_ctx)
    }
}

// ------------------------------------------------------------------------------------------
// terminate & filtre
// ------------------------------------------------------------------------------------------

static TERMINATE_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Appelle le gestionnaire `terminate` de l'invité, puis termine comme `abort()`.
pub(crate) fn terminate_now() -> ! {
    let h = TERMINATE_HANDLER.load(Ordering::Acquire);
    if h != 0 {
        // SAFETY: gestionnaire `void (*)()` Win64 posé par `set_terminate`.
        unsafe {
            let f: extern "win64" fn() = core::mem::transmute::<usize, _>(h);
            f();
        }
    }
    sb_err!(CHANNEL, "std::terminate() : abort()");
    crate::ucrt::crt_abort()
}

extern "win64" fn terminate() {
    ffi_guard!(CHANNEL, "terminate", (), { terminate_now() })
}

extern "win64" fn set_terminate(handler: usize) -> usize {
    ffi_guard!(CHANNEL, "set_terminate", 0, {
        TERMINATE_HANDLER.swap(handler, Ordering::AcqRel)
    })
}

extern "win64" fn get_terminate() -> usize {
    TERMINATE_HANDLER.load(Ordering::Acquire)
}

/// `__CxxUnhandledExceptionFilter(EXCEPTION_POINTERS*)` : `terminate()` pour une exception C++,
/// sinon `EXCEPTION_CONTINUE_SEARCH`.
extern "win64" fn cxx_unhandled_exception_filter(ptrs: *const ExceptionPointers) -> i32 {
    ffi_guard!(CHANNEL, "__CxxUnhandledExceptionFilter", 0, {
        if ptrs.is_null() {
            return 0;
        }
        // SAFETY: `EXCEPTION_POINTERS` fourni par l'appelant.
        let rec = unsafe { (*ptrs).exception_record };
        // SAFETY: enregistrement valide si non nul.
        if !rec.is_null() && unsafe { (*rec).exception_code } == CXX_EXCEPTION_CODE {
            terminate_now();
        }
        0
    })
}

// ------------------------------------------------------------------------------------------
// Exports
// ------------------------------------------------------------------------------------------

pub(crate) fn register(m: &mut HashMap<&'static str, usize>) {
    macro_rules! reg {
        ($name:expr, $f:expr) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("_CxxThrowException", _CxxThrowException);
    reg!("__CxxFrameHandler3", __CxxFrameHandler3);
    reg!("__CxxFrameHandler2", __CxxFrameHandler3);
    reg!("__CxxFrameHandler4", __CxxFrameHandler4);
    reg!(
        "__CxxUnhandledExceptionFilter",
        cxx_unhandled_exception_filter
    );
    reg!("terminate", terminate);
    reg!("__std_terminate", terminate);
    reg!("?terminate@std@@YAXXZ", terminate);
    reg!("set_terminate", set_terminate);
    reg!("?set_terminate@std@@YAP6AXXZP6AXXZ@Z", set_terminate);
    reg!("?get_terminate@std@@YAP6AXXZXZ", get_terminate);
    reg!("_get_terminate", get_terminate);
    synth::register_crt(m);
}

/// Exports `msvcp140` (`std::exception_ptr`, `std::_X*`).
pub(crate) fn register_msvcp(m: &mut HashMap<&'static str, usize>) {
    eptr::register(m);
    synth::register_msvcp(m);
}
