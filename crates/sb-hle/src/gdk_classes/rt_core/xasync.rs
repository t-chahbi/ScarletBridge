//! Moteur `XAsync*` / `XAsyncProvider` de `XThreadingImpl`.
//!
//! Un `XAsyncBlock` démarré (`XAsyncBegin`) reçoit dans `internal[0..8]` l'identifiant d'une
//! opération ; l'état (statut, taille de résultat, fournisseur) vit côté hôte. Le travail
//! (`XAsyncOp::DoWork`) passe par le port de travail de la file du bloc, la complétion
//! (callback du bloc) par son port de complétion ; une file NULL désigne la file « process ».

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use parking_lot::{Condvar, Mutex};
use sb_common::abi::{E_INVALIDARG, HResult, S_OK};
use sb_common::sb_debug;

use super::taskq::{self, E_ABORT, PORT_COMPLETION, PORT_WORK};
use crate::gdk_xs::XAsyncBlock;

const CHANNEL: &str = "gdk_classes";

/// `E_PENDING` : opération en cours.
pub(crate) const E_PENDING: HResult = 0x8000_000A_u32 as i32;
/// `E_ILLEGAL_METHOD_CALL`.
pub(crate) const E_ILLEGAL_METHOD_CALL: HResult = 0x8000_0010_u32 as i32;

/// `XAsyncOp`.
const OP_BEGIN: u32 = 0;
const OP_DO_WORK: u32 = 1;
const OP_GET_RESULT: u32 = 2;
const OP_CANCEL: u32 = 3;
const OP_CLEANUP: u32 = 4;

/// `XAsyncProviderData` (public GDK).
#[repr(C)]
pub(crate) struct XAsyncProviderData {
    pub async_block: *mut XAsyncBlock,
    pub buffer_size: usize,
    pub buffer: *mut c_void,
    pub context: *mut c_void,
}

const _: () = {
    assert!(size_of::<XAsyncProviderData>() == 32);
    assert!(core::mem::offset_of!(XAsyncProviderData, context) == 24);
    assert!(size_of::<XAsyncBlock>() == 56);
    assert!(core::mem::offset_of!(XAsyncBlock, internal) == 24);
};

/// `HRESULT XAsyncProvider(XAsyncOp, const XAsyncProviderData*)`.
pub(crate) type Provider = unsafe extern "win64" fn(u32, *const XAsyncProviderData) -> HResult;
/// `HRESULT XAsyncWork(XAsyncBlock*)`.
type Work = unsafe extern "win64" fn(*mut XAsyncBlock) -> HResult;

struct OpState {
    status: HResult,
    done: bool,
    cleaned: bool,
    required: usize,
}

struct AsyncOp {
    id: u64,
    block: usize,
    provider: usize,
    context: usize,
    identity: usize,
    state: Mutex<OpState>,
    cv: Condvar,
}

/// Les identifiants commencent haut : `internal[0] == 1` reste le marqueur « terminé » des
/// anciens stubs (`XUserAddAsync` de `gdk_xs`).
static NEXT_ID: AtomicU64 = AtomicU64::new(0x1_0000_0000);
static OPS: LazyLock<Mutex<HashMap<u64, Arc<AsyncOp>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn block_id(block: *mut XAsyncBlock) -> u64 {
    // SAFETY: `block` non-nul (vérifié par les appelants) ; lecture non alignée de internal[0..8].
    unsafe {
        let p = core::ptr::addr_of!((*block).internal).cast::<u8>();
        core::ptr::read_unaligned(p.cast::<u64>())
    }
}

fn set_block_id(block: *mut XAsyncBlock, id: u64) {
    // SAFETY: `block` non-nul ; écriture non alignée de internal[0..8].
    unsafe {
        let p = core::ptr::addr_of_mut!((*block).internal).cast::<u8>();
        core::ptr::write_unaligned(p.cast::<u64>(), id);
    }
}

fn op_of(block: *mut XAsyncBlock) -> Option<Arc<AsyncOp>> {
    if block.is_null() {
        return None;
    }
    OPS.lock().get(&block_id(block)).cloned()
}

/// Bloc terminé par un autre module (stubs `gdk_xs`, GameSave, XUser…) sans passer par
/// `XAsyncBegin`. Convention commune : `internal[0..4] == 1` (terminé), statut `HRESULT` en
/// `internal[4..8]` (0 = `S_OK` si non renseigné), taille du résultat en `internal[8..16]`.
fn legacy_state(block: *mut XAsyncBlock) -> Option<(HResult, u64)> {
    if block.is_null() {
        return None;
    }
    // SAFETY: `block` non-nul ; lectures non alignées dans `internal` (24 octets).
    unsafe {
        let p = core::ptr::addr_of!((*block).internal).cast::<u8>();
        if core::ptr::read_unaligned(p.cast::<u32>()) != 1 {
            return None;
        }
        let status = core::ptr::read_unaligned(p.add(4).cast::<i32>());
        let size = core::ptr::read_unaligned(p.add(8).cast::<u64>());
        Some((status, size))
    }
}

fn call_provider(op: &AsyncOp, which: u32, size: usize, buffer: *mut c_void) -> HResult {
    if op.provider == 0 {
        return S_OK;
    }
    let data = XAsyncProviderData {
        async_block: op.block as *mut XAsyncBlock,
        buffer_size: size,
        buffer,
        context: op.context as *mut c_void,
    };
    // SAFETY: `provider` est un `XAsyncProvider` Win64 fourni à `XAsyncBegin`.
    let p: Provider = unsafe { core::mem::transmute(op.provider) };
    // SAFETY: `data` vit pendant l'appel.
    unsafe { p(which, &data) }
}

fn queue_of(block: *mut XAsyncBlock) -> usize {
    // SAFETY: `block` non-nul.
    let q = unsafe { (*block).queue };
    if q == 0 { taskq::process_queue() } else { q }
}

fn cleanup(op: &AsyncOp) {
    {
        let mut st = op.state.lock();
        if st.cleaned {
            return;
        }
        st.cleaned = true;
    }
    let _ = call_provider(op, OP_CLEANUP, 0, core::ptr::null_mut());
}

/// `XAsyncBegin`.
pub(crate) fn begin(
    block: *mut XAsyncBlock,
    context: usize,
    identity: usize,
    identity_name: *const u8,
    provider: usize,
) -> HResult {
    if block.is_null() || provider == 0 {
        return E_INVALIDARG;
    }
    {
        // Un bloc réutilisé : l'ancienne opération (terminée ou non) est oubliée.
        OPS.lock().remove(&block_id(block));
    }
    let id = NEXT_ID.fetch_add(1, Ordering::AcqRel);
    let op = Arc::new(AsyncOp {
        id,
        block: block as usize,
        provider,
        context,
        identity,
        state: Mutex::new(OpState {
            status: E_PENDING,
            done: false,
            cleaned: false,
            required: 0,
        }),
        cv: Condvar::new(),
    });
    set_block_id(block, id);
    OPS.lock().insert(id, Arc::clone(&op));
    let hr = call_provider(&op, OP_BEGIN, 0, core::ptr::null_mut());
    if hr < 0 {
        OPS.lock().remove(&id);
        set_block_id(block, 0);
        return hr;
    }
    let _ = identity_name;
    sb_debug!(CHANNEL, "XAsyncBegin(block={block:p}) id={id:#x}");
    S_OK
}

/// `XAsyncSchedule`.
pub(crate) fn schedule(block: *mut XAsyncBlock, delay_ms: u32) -> HResult {
    let Some(op) = op_of(block) else {
        return E_INVALIDARG;
    };
    if op.state.lock().done {
        return E_ILLEGAL_METHOD_CALL;
    }
    taskq::submit(
        queue_of(block),
        PORT_WORK,
        delay_ms,
        op.id as usize as *mut c_void,
        work_trampoline as *const () as usize,
    )
}

extern "win64" fn work_trampoline(ctx: *mut c_void, canceled: u8) {
    let Some(op) = OPS.lock().get(&(ctx as usize as u64)).cloned() else {
        return;
    };
    if canceled != 0 || op.state.lock().done {
        return;
    }
    let hr = call_provider(&op, OP_DO_WORK, 0, core::ptr::null_mut());
    if hr != E_PENDING {
        complete(op.block as *mut XAsyncBlock, hr, 0);
    }
}

extern "win64" fn completion_trampoline(ctx: *mut c_void, _canceled: u8) {
    let Some(op) = OPS.lock().get(&(ctx as usize as u64)).cloned() else {
        return;
    };
    let block = op.block as *mut XAsyncBlock;
    // SAFETY: le bloc doit rester valide jusqu'à la complétion (contrat XAsync).
    let cb = unsafe { (*block).callback };
    if let Some(cb) = cb {
        // SAFETY: callback de complétion invité Win64.
        unsafe { cb(block) };
    }
    finish_if_no_result(&op);
}

fn finish_if_no_result(op: &AsyncOp) {
    let no_result = {
        let st = op.state.lock();
        st.status < 0 || st.required == 0
    };
    if no_result {
        cleanup(op);
    }
}

/// `XAsyncComplete`.
pub(crate) fn complete(block: *mut XAsyncBlock, result: HResult, required: usize) {
    let Some(op) = op_of(block) else {
        return;
    };
    {
        let mut st = op.state.lock();
        if st.done {
            return;
        }
        st.status = result;
        st.required = if result < 0 { 0 } else { required };
        st.done = true;
    }
    op.cv.notify_all();
    // SAFETY: bloc valide jusqu'à la complétion.
    let has_cb = unsafe { (*block).callback.is_some() };
    if has_cb {
        let hr = taskq::submit(
            queue_of(block),
            PORT_COMPLETION,
            0,
            op.id as usize as *mut c_void,
            completion_trampoline as *const () as usize,
        );
        if hr != S_OK {
            // File terminée ou invalide : le callback tourne en ligne.
            completion_trampoline(op.id as usize as *mut c_void, 1);
        }
    } else {
        finish_if_no_result(&op);
    }
}

/// `XAsyncGetStatus`.
pub(crate) fn get_status(block: *mut XAsyncBlock, wait: bool) -> HResult {
    if block.is_null() {
        return E_INVALIDARG;
    }
    let Some(op) = op_of(block) else {
        return legacy_state(block).map_or(E_INVALIDARG, |(status, _)| status);
    };
    let mut st = op.state.lock();
    if wait {
        while !st.done {
            op.cv.wait(&mut st);
        }
    }
    if st.done { st.status } else { E_PENDING }
}

/// `XAsyncGetResultSize`.
pub(crate) fn get_result_size(block: *mut XAsyncBlock, out: *mut usize) -> HResult {
    if block.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let Some(op) = op_of(block) else {
        let Some((status, size)) = legacy_state(block) else {
            return E_INVALIDARG;
        };
        // SAFETY: `out` vérifié non-nul.
        unsafe { *out = size as usize };
        return status.min(S_OK);
    };
    let st = op.state.lock();
    if !st.done {
        return E_ILLEGAL_METHOD_CALL;
    }
    // SAFETY: `out` vérifié non-nul.
    unsafe { *out = st.required };
    st.status.min(S_OK)
}

/// `XAsyncGetResult`.
pub(crate) fn get_result(
    block: *mut XAsyncBlock,
    identity: usize,
    size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> HResult {
    if block.is_null() {
        return E_INVALIDARG;
    }
    if !used.is_null() {
        // SAFETY: `used` vérifié non-nul.
        unsafe { *used = 0 };
    }
    let Some(op) = op_of(block) else {
        return legacy_state(block).map_or(E_INVALIDARG, |(status, _)| status.min(S_OK));
    };
    let (done, status, required) = {
        let st = op.state.lock();
        (st.done, st.status, st.required)
    };
    if !done {
        return E_ILLEGAL_METHOD_CALL;
    }
    if identity != 0 && op.identity != 0 && identity != op.identity {
        return E_INVALIDARG;
    }
    if status < 0 {
        return status;
    }
    if required == 0 {
        return S_OK;
    }
    if buffer.is_null() || size < required {
        return E_INVALIDARG;
    }
    let hr = call_provider(&op, OP_GET_RESULT, size, buffer);
    if hr >= 0 {
        if !used.is_null() {
            // SAFETY: `used` vérifié non-nul.
            unsafe { *used = required };
        }
        cleanup(&op);
    }
    hr
}

/// `XAsyncCancel` : prévient le fournisseur puis complète avec `E_ABORT`.
pub(crate) fn cancel(block: *mut XAsyncBlock) {
    let Some(op) = op_of(block) else {
        return;
    };
    if op.state.lock().done {
        return;
    }
    let _ = call_provider(&op, OP_CANCEL, 0, core::ptr::null_mut());
    complete(block, E_ABORT, 0);
}

/// `XAsyncBegin` + `XAsyncSchedule(0)` avec un fournisseur interne.
pub(crate) fn begin_and_schedule(
    block: *mut XAsyncBlock,
    context: usize,
    identity: usize,
    provider: Provider,
) -> HResult {
    let hr = begin(
        block,
        context,
        identity,
        core::ptr::null(),
        provider as usize,
    );
    if hr < 0 {
        return hr;
    }
    schedule(block, 0)
}

/// Fournisseur de `XAsyncRun` : `context` = la fonction de travail.
unsafe extern "win64" fn run_provider(op: u32, data: *const XAsyncProviderData) -> HResult {
    if op != OP_DO_WORK || data.is_null() {
        return S_OK;
    }
    // SAFETY: `data` fourni par le moteur, valide pendant l'appel.
    let d = unsafe { &*data };
    // SAFETY: `context` est le `XAsyncWork` donné à `XAsyncRun`.
    let work: Work = unsafe { core::mem::transmute(d.context) };
    // SAFETY: appel de la fonction de travail invitée.
    unsafe { work(d.async_block) }
}

/// `XAsyncRun`.
pub(crate) fn run(block: *mut XAsyncBlock, work: usize) -> HResult {
    if block.is_null() || work == 0 {
        return E_INVALIDARG;
    }
    begin_and_schedule(block, work, run as *const () as usize, run_provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU32, AtomicUsize};

    fn new_block(queue: usize) -> Box<XAsyncBlock> {
        Box::new(XAsyncBlock {
            queue,
            context: core::ptr::null_mut(),
            callback: None,
            internal: [0; 32],
        })
    }

    #[test]
    fn blocks_completed_outside_the_engine_expose_status_and_size() {
        // Convention commune (GameSave, XUser, stubs gdk_xs) : internal[0]=1, statut [4..8], taille [8..16].
        let mut b = new_block(0);
        assert_eq!(
            get_status(&mut *b, false),
            E_INVALIDARG,
            "bloc jamais démarré"
        );
        b.internal[0] = 1;
        let mut size = 99usize;
        assert_eq!(get_status(&mut *b, false), S_OK);
        assert_eq!(get_result_size(&mut *b, &mut size), S_OK);
        assert_eq!(size, 0);

        b.internal[8..16].copy_from_slice(&4096u64.to_le_bytes());
        assert_eq!(get_result_size(&mut *b, &mut size), S_OK);
        assert_eq!(size, 4096);

        let failure = 0x8924_5400_u32 as i32;
        b.internal[4..8].copy_from_slice(&failure.to_le_bytes());
        assert_eq!(get_status(&mut *b, false), failure);
        assert_eq!(get_result_size(&mut *b, &mut size), failure);
        assert_eq!(
            get_result(&mut *b, 0, 0, core::ptr::null_mut(), core::ptr::null_mut()),
            failure
        );
    }

    fn manual_queue() -> usize {
        let mut q = 0usize;
        assert_eq!(taskq::create(2, 2, &mut q), S_OK);
        q
    }

    static CB_COUNT: AtomicU32 = AtomicU32::new(0);
    unsafe extern "win64" fn on_complete(_b: *mut XAsyncBlock) {
        CB_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    static GOT_CLEANUP: AtomicU32 = AtomicU32::new(0);
    static WORK_RESULT_SIZE: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "win64" fn sized_provider(op: u32, d: *const XAsyncProviderData) -> HResult {
        // SAFETY: données valides fournies par le moteur.
        let d = unsafe { &*d };
        match op {
            OP_BEGIN => {
                let _ = schedule(d.async_block, 0);
                S_OK
            }
            OP_DO_WORK => {
                complete(d.async_block, S_OK, 4);
                S_OK
            }
            OP_GET_RESULT => {
                WORK_RESULT_SIZE.store(d.buffer_size, Ordering::SeqCst);
                // SAFETY: tampon d'au moins 4 octets garanti par le moteur.
                unsafe { d.buffer.cast::<u32>().write_unaligned(0xC0FF_EE00) };
                S_OK
            }
            OP_CLEANUP => {
                GOT_CLEANUP.fetch_add(1, Ordering::SeqCst);
                S_OK
            }
            _ => S_OK,
        }
    }

    #[test]
    fn begin_schedule_complete_get_result_cycle() {
        let q = manual_queue();
        let mut b = new_block(q);
        b.callback = Some(on_complete);
        let bp: *mut XAsyncBlock = &mut *b;
        assert_eq!(get_status(bp, false), E_INVALIDARG, "bloc non démarré");
        assert_eq!(
            begin(core::ptr::null_mut(), 0, 0, core::ptr::null(), 1),
            E_INVALIDARG
        );
        assert_eq!(begin(bp, 0, 0, core::ptr::null(), 0), E_INVALIDARG);
        let before = CB_COUNT.load(Ordering::SeqCst);
        assert_eq!(
            begin(
                bp,
                0,
                0x1234,
                core::ptr::null(),
                sized_provider as *const () as usize
            ),
            S_OK
        );
        assert_eq!(get_status(bp, false), E_PENDING);
        assert_eq!(taskq::dispatch(q, PORT_WORK, 0), 1);
        assert_eq!(get_status(bp, true), S_OK);
        assert_eq!(
            CB_COUNT.load(Ordering::SeqCst),
            before,
            "callback via la file"
        );
        assert_eq!(taskq::dispatch(q, PORT_COMPLETION, 0), 1);
        assert_eq!(CB_COUNT.load(Ordering::SeqCst), before + 1);
        let mut size = 0usize;
        assert_eq!(get_result_size(bp, &mut size), S_OK);
        assert_eq!(size, 4);
        let mut buf = [0u8; 8];
        let mut used = 0usize;
        assert_eq!(
            get_result(bp, 0x9999, 8, buf.as_mut_ptr().cast(), &mut used),
            E_INVALIDARG,
            "identité différente"
        );
        assert_eq!(
            get_result(bp, 0x1234, 2, buf.as_mut_ptr().cast(), &mut used),
            E_INVALIDARG,
            "tampon trop petit"
        );
        let c0 = GOT_CLEANUP.load(Ordering::SeqCst);
        assert_eq!(
            get_result(bp, 0x1234, 8, buf.as_mut_ptr().cast(), &mut used),
            S_OK
        );
        assert_eq!(used, 4);
        assert_eq!(
            u32::from_le_bytes(buf[..4].try_into().expect("4 octets")),
            0xC0FF_EE00
        );
        assert_eq!(GOT_CLEANUP.load(Ordering::SeqCst), c0 + 1);
        taskq::close(q);
    }

    unsafe extern "win64" fn work_ok(_b: *mut XAsyncBlock) -> HResult {
        0x8000_4005_u32 as i32 // E_FAIL
    }

    #[test]
    fn run_completes_with_work_result_and_wait_blocks() {
        let mut q = 0usize;
        assert_eq!(taskq::create(0, 0, &mut q), S_OK);
        let mut b = new_block(q);
        let bp: *mut XAsyncBlock = &mut *b;
        assert_eq!(run(bp, work_ok as *const () as usize), S_OK);
        assert_eq!(get_status(bp, true), 0x8000_4005_u32 as i32);
        let mut sz = 7usize;
        assert_eq!(get_result_size(bp, &mut sz), 0x8000_4005_u32 as i32);
        assert_eq!(run(bp, 0), E_INVALIDARG);
        taskq::close(q);
    }

    unsafe extern "win64" fn cancelable(op: u32, d: *const XAsyncProviderData) -> HResult {
        // SAFETY: données valides.
        let d = unsafe { &*d };
        match op {
            OP_DO_WORK => E_PENDING,
            OP_BEGIN => {
                let _ = schedule(d.async_block, 10_000);
                S_OK
            }
            _ => S_OK,
        }
    }

    #[test]
    fn cancel_completes_with_abort_and_is_idempotent() {
        let q = manual_queue();
        let mut b = new_block(q);
        let bp: *mut XAsyncBlock = &mut *b;
        assert_eq!(
            begin(
                bp,
                0,
                0,
                core::ptr::null(),
                cancelable as *const () as usize
            ),
            S_OK
        );
        assert_eq!(get_status(bp, false), E_PENDING);
        cancel(bp);
        assert_eq!(get_status(bp, false), E_ABORT);
        complete(bp, S_OK, 0);
        assert_eq!(get_status(bp, false), E_ABORT, "déjà complété");
        assert_eq!(get_status(core::ptr::null_mut(), false), E_INVALIDARG);
        taskq::close(q);
    }

    #[test]
    fn legacy_done_marker_is_accepted() {
        let mut b = new_block(0);
        b.internal[0] = 1;
        let bp: *mut XAsyncBlock = &mut *b;
        assert_eq!(get_status(bp, true), S_OK);
        let mut sz = 5usize;
        assert_eq!(get_result_size(bp, &mut sz), S_OK);
        assert_eq!(sz, 0);
    }
}
