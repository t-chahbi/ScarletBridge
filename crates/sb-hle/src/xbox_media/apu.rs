//! Audio matériel Xbox (Scarlett) émulé en logiciel : `acphal` (HAL ACP + tas APU),
//! `xapu` (file de commandes APU) et `chad` (DSP audio `XDsp*`).
//!
//! Aucune puce APU/SHAPE n'existe sur l'hôte. L'émulation fournit :
//! - un tas APU en mémoire hôte (mmap `MAP_32BIT` quand possible) avec mappage **identité**
//!   adresse APU (32 bits) ↔ adresse virtuelle ; à défaut un mappage linéaire bijectif ;
//! - une table de traduction bidirectionnelle APU 32 bits ↔ pointeur CPU pour les buffers
//!   hors tas validés par `ApuIsVirtualAddressValid` (plage APU `0xC000_0000..`) ;
//! - un bloc de contextes SHAPE zéro-initialisé (le matériel ne le traitera jamais) ;
//! - des files de commandes complétées immédiatement par un résultat « succès sans traitement ».
//!
//! Signatures : `acphal.h`/`apu.h` GDK ne sont pas publics dans le détail ; les signatures
//! ci-dessous sont **déduites** des sites d'appel observés (faits d'ABI mesurés, aucun code
//! tiers repris) :
//! - `HRESULT AcpHalCreate(void** out, UINT32 /*0*/)` — `*out` relu par l'appelant ; l'objet
//!   retourné expose IUnknown, ses autres méthodes renvoient `E_NOTIMPL`.
//! - `HRESULT AcpHalAllocateShapeContexts(void** out)` — même schéma : `*out` = bloc de
//!   contextes zéro-initialisé ([`SHAPE_BLOCK_BYTES`] octets, aligné 128).
//! - `HRESULT AcpHalReleaseShapeContexts(void* block)` — résultat ignoré par l'appelant.
//! - `HRESULT ApuCreateHeap(UINT32 cachedSize?, UINT32 size)` — taille vue : `0x226e00`.
//! - `HRESULT ApuAlloc(void** va, APU_ADDRESS* pa /*NULL vu*/, UINT32 size, UINT32 align)`.
//! - `HRESULT ApuFree(void* va)`.
//! - `BOOL ApuIsVirtualAddressValid(const void* va, UINT32 size)` (octet booléen lu).
//! - `APU_ADDRESS ApuMapVirtualAddress(const void* va, UINT32 size)` / `void* ApuMapApuAddress(APU_ADDRESS)`.
//! - `HANDLE XApuConnect(void)`, `HRESULT XApuDisconnect(HANDLE, UINT32)`,
//!   `HRESULT XApuEnqueueCommand(HANDLE, const void* cmd, UINT32 flags)`,
//!   `HRESULT XApuDequeueResult(HANDLE, XAPU_RESULT* out)` (négatif = aucun résultat),
//!   `UINT32 XApuGetResultCount(HANDLE)`.
//! - `HRESULT XDspActivate(...)`, `HRESULT XDspDeactivate(...)`,
//!   `HRESULT XDspConnectWithMaximumStreamLimit(?, ?, ?, UINT32 maxStreams)`,
//!   `HRESULT XDspDisconnect(HANDLE)`, `HRESULT XDspSubmitCommand(HANDLE, const void* cmd)`.

#![allow(non_snake_case)]

use core::ffi::c_void;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use parking_lot::Mutex;
use sb_common::abi::{
    E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, HResult, S_OK, WIN_FALSE,
    WIN_TRUE, WinBool,
};
use sb_common::{ffi_guard, sb_debug, sb_fixme};

use super::plausible_ptr;
use crate::com::{Guid, IID_IUNKNOWN};

const CHANNEL: &str = "xbox_apu";

/// `HRESULT_FROM_WIN32(ERROR_INVALID_STATE)` — appel hors séquence (tas absent, DSP inactif).
pub const E_NOT_VALID_STATE: HResult = 0x8007_139F_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_ALREADY_INITIALIZED)`.
pub const E_ALREADY_INITIALIZED: HResult = 0x8007_04DF_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_INVALID_HANDLE)`.
pub const E_HANDLE: HResult = 0x8007_0006_u32 as i32;
/// `E_PENDING` — file de résultats vide.
pub const E_PENDING: HResult = 0x8000_000A_u32 as i32;

// ==================== Tas APU + table de traduction ====================

const MAX_HEAP: u64 = 0x8000_0000;
const XLATE_APU_BASE: u32 = 0xC000_0000;
const PAGE: usize = 0x1000;

struct ApuHeap {
    base: usize,
    size: usize,
    apu_base: u32,
    /// offset -> taille.
    allocs: BTreeMap<usize, usize>,
}

impl ApuHeap {
    fn to_apu(&self, va: usize) -> Option<u32> {
        (va >= self.base && va < self.base + self.size)
            .then(|| self.apu_base.wrapping_add((va - self.base) as u32))
    }

    fn to_va(&self, apu: u32) -> Option<usize> {
        let off = apu.checked_sub(self.apu_base)? as usize;
        (off < self.size).then_some(self.base + off)
    }

    fn block_of(&self, va: usize) -> Option<(usize, usize)> {
        let off = va.checked_sub(self.base)?;
        self.allocs
            .range(..=off)
            .next_back()
            .filter(|&(&o, &l)| off < o + l)
            .map(|(&o, &l)| (self.base + o, l))
    }

    fn alloc(&mut self, size: usize, align: usize) -> Option<usize> {
        let mut cursor = 0usize;
        let mut found = None;
        for (&off, &len) in &self.allocs {
            let start = (self.base + cursor).next_multiple_of(align) - self.base;
            if start + size <= off {
                found = Some(start);
                break;
            }
            cursor = cursor.max(off + len);
        }
        let off = match found {
            Some(o) => o,
            None => {
                let start = (self.base + cursor).next_multiple_of(align) - self.base;
                if start.checked_add(size)? > self.size {
                    return None;
                }
                start
            }
        };
        self.allocs.insert(off, size);
        // SAFETY: `[base+off, base+off+size)` est inclus dans la région mmap du tas.
        unsafe { core::ptr::write_bytes((self.base + off) as *mut u8, 0, size) };
        Some(self.base + off)
    }
}

/// Traductions hors tas : va_page_start -> (pages_len, apu_page_start).
#[derive(Default)]
struct Xlate {
    by_va: BTreeMap<usize, (usize, u32)>,
    by_apu: BTreeMap<u32, (usize, usize)>,
    cursor: u32,
}

impl Xlate {
    fn lookup_va(&self, va: usize) -> Option<u32> {
        let (&start, &(len, apu)) = self.by_va.range(..=va).next_back()?;
        (va < start + len).then(|| apu + (va - start) as u32)
    }

    fn lookup_apu(&self, apu: u32) -> Option<usize> {
        let (&start, &(len, va)) = self.by_apu.range(..=apu).next_back()?;
        ((apu - start) as usize)
            .lt(&len)
            .then(|| va + (apu - start) as usize)
    }

    fn map(&mut self, va: usize, size: usize) -> Option<u32> {
        if let Some(apu) = self.lookup_va(va)
            && self.lookup_va(va + size.max(1) - 1) == Some(apu + (size.max(1) - 1) as u32)
        {
            return Some(apu);
        }
        let start = va & !(PAGE - 1);
        let len = (va + size.max(1)).next_multiple_of(PAGE) - start;
        let cursor = if self.cursor == 0 {
            XLATE_APU_BASE
        } else {
            self.cursor
        };
        let end = u64::from(cursor) + len as u64;
        if end > u64::from(u32::MAX) {
            return None;
        }
        self.cursor = end as u32;
        self.by_va.insert(start, (len, cursor));
        self.by_apu.insert(cursor, (len, start));
        Some(cursor + (va - start) as u32)
    }
}

static HEAP: Mutex<Option<ApuHeap>> = Mutex::new(None);
static XLATE: LazyLock<Mutex<Xlate>> = LazyLock::new(|| Mutex::new(Xlate::default()));

fn map_heap(size: usize) -> Option<(usize, u32)> {
    let prot = libc::PROT_READ | libc::PROT_WRITE;
    let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
    // SAFETY: mmap anonyme sans adresse imposée.
    let p = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            size,
            prot,
            flags | libc::MAP_32BIT,
            -1,
            0,
        )
    };
    if p != libc::MAP_FAILED && (p as usize) + size < XLATE_APU_BASE as usize {
        return Some((p as usize, p as usize as u32));
    }
    if p != libc::MAP_FAILED {
        // SAFETY: région mmap ci-dessus, non utilisée.
        unsafe { libc::munmap(p, size) };
    }
    // SAFETY: mmap anonyme sans adresse imposée.
    let p = unsafe { libc::mmap(core::ptr::null_mut(), size, prot, flags, -1, 0) };
    if p == libc::MAP_FAILED {
        return None;
    }
    // Mappage linéaire bijectif (identité impossible au-delà de 4 Gio).
    Some((p as usize, 0x1000_0000))
}

/// Vérifie que `[va, va+size)` est intégralement mappé dans l'espace hôte (`mincore`).
fn host_range_mapped(va: usize, size: usize) -> bool {
    if !plausible_ptr(va as *const u8) {
        return false;
    }
    let start = va & !(PAGE - 1);
    let Some(end) = va.checked_add(size.max(1)) else {
        return false;
    };
    let len = end.next_multiple_of(PAGE) - start;
    let mut vec = vec![0u8; len / PAGE];
    // SAFETY: `mincore` ne lit ni n'écrit la plage ; `vec` couvre len/PAGE pages.
    unsafe { libc::mincore(start as *mut c_void, len, vec.as_mut_ptr()) == 0 }
}

pub(crate) extern "win64" fn ApuCreateHeap(first: u32, size: u32) -> HResult {
    ffi_guard!(CHANNEL, "ApuCreateHeap", E_OUTOFMEMORY, {
        // Premier argument de sémantique incertaine (taille « cached » probable) : compté
        // seulement s'il a l'allure d'une taille raisonnable.
        let extra = if u64::from(first) < MAX_HEAP / 2 {
            u64::from(first)
        } else {
            0
        };
        let total = u64::from(size) + extra;
        if size == 0 || total > MAX_HEAP {
            return E_INVALIDARG;
        }
        let mut heap = HEAP.lock();
        if heap.is_some() {
            return E_ALREADY_INITIALIZED;
        }
        let size = (total as usize).next_multiple_of(0x1_0000);
        let Some((base, apu_base)) = map_heap(size) else {
            return E_OUTOFMEMORY;
        };
        sb_debug!(
            CHANNEL,
            "ApuCreateHeap {size:#x} octets @ {base:#x} (APU {apu_base:#x})"
        );
        *heap = Some(ApuHeap {
            base,
            size,
            apu_base,
            allocs: BTreeMap::new(),
        });
        S_OK
    })
}

pub(crate) extern "win64" fn ApuAlloc(
    va: *mut *mut c_void,
    pa: *mut u32,
    size: u32,
    alignment: u32,
) -> HResult {
    ffi_guard!(CHANNEL, "ApuAlloc", E_OUTOFMEMORY, {
        if va.is_null() {
            return E_POINTER;
        }
        // SAFETY: `va` vérifié non-nul.
        unsafe { *va = core::ptr::null_mut() };
        let align = if alignment == 0 { 4 } else { alignment } as usize;
        if size == 0 || !align.is_power_of_two() {
            return E_INVALIDARG;
        }
        let mut guard = HEAP.lock();
        let Some(heap) = guard.as_mut() else {
            return E_NOT_VALID_STATE;
        };
        let Some(addr) = heap.alloc(size as usize, align) else {
            return E_OUTOFMEMORY;
        };
        let apu = heap.to_apu(addr).unwrap_or(0);
        // SAFETY: `va` vérifié non-nul ; `pa` optionnel vérifié.
        unsafe {
            *va = addr as *mut c_void;
            if !pa.is_null() {
                *pa = apu;
            }
        }
        S_OK
    })
}

pub(crate) extern "win64" fn ApuFree(va: *mut c_void) -> HResult {
    ffi_guard!(CHANNEL, "ApuFree", E_INVALIDARG, {
        let mut guard = HEAP.lock();
        let Some(heap) = guard.as_mut() else {
            return E_NOT_VALID_STATE;
        };
        let addr = va as usize;
        if addr < heap.base || heap.allocs.remove(&(addr - heap.base)).is_none() {
            return E_INVALIDARG;
        }
        S_OK
    })
}

/// Valide une plage : bloc du tas APU contenant toute la plage, ou mémoire hôte mappée
/// (rendue adressable par l'APU émulée via la table de traduction).
fn range_valid(va: usize, size: usize) -> bool {
    if let Some(heap) = HEAP.lock().as_ref()
        && heap.to_apu(va).is_some()
    {
        return heap
            .block_of(va)
            .is_some_and(|(s, l)| va + size.max(1) <= s + l);
    }
    host_range_mapped(va, size)
}

pub(crate) extern "win64" fn ApuIsVirtualAddressValid(va: *const c_void, size: u32) -> WinBool {
    ffi_guard!(CHANNEL, "ApuIsVirtualAddressValid", WIN_FALSE, {
        if range_valid(va as usize, size as usize) {
            WIN_TRUE
        } else {
            WIN_FALSE
        }
    })
}

pub(crate) extern "win64" fn ApuMapVirtualAddress(va: *const c_void, size: u32) -> u32 {
    ffi_guard!(CHANNEL, "ApuMapVirtualAddress", 0, {
        let addr = va as usize;
        if let Some(apu) = HEAP.lock().as_ref().and_then(|h| h.to_apu(addr)) {
            return apu;
        }
        if !host_range_mapped(addr, size as usize) {
            return 0;
        }
        XLATE.lock().map(addr, size as usize).unwrap_or(0)
    })
}

pub(crate) extern "win64" fn ApuMapApuAddress(apu: u32) -> *mut c_void {
    ffi_guard!(CHANNEL, "ApuMapApuAddress", core::ptr::null_mut(), {
        let va = if apu >= XLATE_APU_BASE {
            XLATE.lock().lookup_apu(apu)
        } else {
            HEAP.lock().as_ref().and_then(|h| h.to_va(apu))
        };
        va.map_or(core::ptr::null_mut(), |v| v as *mut c_void)
    })
}

// ==================== AcpHal ====================

/// Taille du bloc de contextes SHAPE (disposition non publique : généreuse et zéro-initialisée).
pub const SHAPE_BLOCK_BYTES: usize = 256 * 1024;

/// Ligne de cache de 128 octets (alignement des contextes SHAPE).
#[repr(C, align(128))]
#[derive(Clone, Copy)]
struct CacheLine([u8; 128]);

const _: () = assert!(size_of::<CacheLine>() == 128 && align_of::<CacheLine>() == 128);

/// Blocs de contextes SHAPE vivants (adresse -> stockage aligné sur 128 octets).
static SHAPE_BLOCKS: LazyLock<Mutex<HashMap<usize, Box<[CacheLine]>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) extern "win64" fn AcpHalAllocateShapeContexts(out: *mut *mut c_void) -> HResult {
    ffi_guard!(CHANNEL, "AcpHalAllocateShapeContexts", E_OUTOFMEMORY, {
        if !plausible_ptr(out) {
            return E_POINTER;
        }
        let mut block = vec![CacheLine([0; 128]); SHAPE_BLOCK_BYTES / 128].into_boxed_slice();
        let p = block.as_mut_ptr().cast::<c_void>();
        SHAPE_BLOCKS.lock().insert(p as usize, block);
        // SAFETY: pointeur de sortie validé.
        unsafe { *out = p };
        S_OK
    })
}

pub(crate) extern "win64" fn AcpHalReleaseShapeContexts(block: *mut c_void) -> HResult {
    ffi_guard!(CHANNEL, "AcpHalReleaseShapeContexts", E_INVALIDARG, {
        if SHAPE_BLOCKS.lock().remove(&(block as usize)).is_some() {
            S_OK
        } else {
            E_INVALIDARG
        }
    })
}

const ACP_SLOTS: usize = 19;

#[repr(C)]
struct AcpHalObject {
    vtbl: *const [usize; ACP_SLOTS],
    refs: AtomicU32,
}

extern "win64" fn acp_qi(
    this: *mut AcpHalObject,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "IAcpHal::QueryInterface", E_NOINTERFACE, {
        if ppv.is_null() {
            return E_POINTER;
        }
        // SAFETY: `ppv` non-nul ; `riid` vérifié avant lecture ; `this` est notre objet.
        unsafe {
            if !riid.is_null() && *riid == IID_IUNKNOWN && !this.is_null() {
                (*this).refs.fetch_add(1, Ordering::AcqRel);
                *ppv = this.cast();
                return S_OK;
            }
            *ppv = core::ptr::null_mut();
        }
        E_NOINTERFACE
    })
}

extern "win64" fn acp_add_ref(this: *mut AcpHalObject) -> u32 {
    if this.is_null() {
        return 0;
    }
    // SAFETY: `this` est notre objet.
    unsafe { (*this).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

extern "win64" fn acp_release(this: *mut AcpHalObject) -> u32 {
    if this.is_null() {
        return 0;
    }
    // SAFETY: `this` est notre objet, alloué par `Box` dans AcpHalCreate.
    unsafe {
        let left = (*this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            drop(Box::from_raw(this));
        }
        left
    }
}

extern "win64" fn acp_not_impl(_this: *mut AcpHalObject) -> HResult {
    sb_fixme!(CHANNEL, "méthode IAcpHal non documentée -> E_NOTIMPL");
    E_NOTIMPL
}

static ACP_VTBL: LazyLock<[usize; ACP_SLOTS]> = LazyLock::new(|| {
    let mut v = [acp_not_impl as *const () as usize; ACP_SLOTS];
    v[0] = acp_qi as *const () as usize;
    v[1] = acp_add_ref as *const () as usize;
    v[2] = acp_release as *const () as usize;
    v
});

pub(crate) extern "win64" fn AcpHalCreate(out: *mut *mut c_void, _reserved: u32) -> HResult {
    ffi_guard!(CHANNEL, "AcpHalCreate", E_OUTOFMEMORY, {
        if !plausible_ptr(out) {
            return E_POINTER;
        }
        let obj = Box::new(AcpHalObject {
            vtbl: &raw const *ACP_VTBL,
            refs: AtomicU32::new(1),
        });
        // SAFETY: pointeur de sortie validé.
        unsafe { *out = Box::into_raw(obj).cast() };
        S_OK
    })
}

pub(crate) fn acphal_exports() -> HashMap<&'static str, usize> {
    let mut m: HashMap<&'static str, usize> = HashMap::new();
    m.insert("AcpHalCreate", AcpHalCreate as *const () as usize);
    m.insert(
        "AcpHalAllocateShapeContexts",
        AcpHalAllocateShapeContexts as *const () as usize,
    );
    m.insert(
        "AcpHalReleaseShapeContexts",
        AcpHalReleaseShapeContexts as *const () as usize,
    );
    m.insert("ApuCreateHeap", ApuCreateHeap as *const () as usize);
    m.insert("ApuAlloc", ApuAlloc as *const () as usize);
    m.insert("ApuFree", ApuFree as *const () as usize);
    m.insert(
        "ApuIsVirtualAddressValid",
        ApuIsVirtualAddressValid as *const () as usize,
    );
    m.insert("ApuMapApuAddress", ApuMapApuAddress as *const () as usize);
    m.insert(
        "ApuMapVirtualAddress",
        ApuMapVirtualAddress as *const () as usize,
    );
    m
}

// ==================== xapu (file de commandes + file de résultats) ====================

/// Résultat de commande (*déduit*) : jeton = n° de séquence de la commande (1-based),
/// `status` = HRESULT de complétion, `command_type` = premier UINT32 de la commande.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct XApuResult {
    pub token: u64,
    pub status: i32,
    pub command_type: u32,
}

const _: () = assert!(size_of::<XApuResult>() == 16);

#[derive(Default)]
struct XApuConn {
    next_token: u64,
    results: VecDeque<XApuResult>,
}

static NEXT_HANDLE: AtomicUsize = AtomicUsize::new(0x5A90_0000);
static XAPU: LazyLock<Mutex<HashMap<usize, XApuConn>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn new_handle() -> usize {
    NEXT_HANDLE.fetch_add(4, Ordering::Relaxed)
}

/// `HANDLE XApuConnect(void)` — retourne le handle de connexion (jamais NULL).
pub(crate) extern "win64" fn XApuConnect() -> usize {
    ffi_guard!(CHANNEL, "XApuConnect", 0, {
        let h = new_handle();
        XAPU.lock().insert(h, XApuConn::default());
        h
    })
}

/// `HRESULT XApuDisconnect(HANDLE, UINT32 /*0x20 vu, ignoré*/)`.
pub(crate) extern "win64" fn XApuDisconnect(h: usize, _flags: u32) -> HResult {
    ffi_guard!(CHANNEL, "XApuDisconnect", E_HANDLE, {
        if XAPU.lock().remove(&h).is_some() {
            S_OK
        } else {
            E_HANDLE
        }
    })
}

/// `HRESULT XApuEnqueueCommand(HANDLE, const void* command, UINT32 flags /*0 vu*/)`.
/// Complétée immédiatement (aucune APU matérielle) : résultat `S_OK` « sans traitement ».
pub(crate) extern "win64" fn XApuEnqueueCommand(
    h: usize,
    cmd: *const c_void,
    _flags: u32,
) -> HResult {
    ffi_guard!(CHANNEL, "XApuEnqueueCommand", E_OUTOFMEMORY, {
        if !plausible_ptr(cmd) {
            return E_INVALIDARG;
        }
        let mut map = XAPU.lock();
        let Some(conn) = map.get_mut(&h) else {
            return E_HANDLE;
        };
        // SAFETY: `cmd` validé ; toute commande APU débute par un en-tête d'au moins 4 octets.
        let ty = unsafe { cmd.cast::<u32>().read_unaligned() };
        conn.next_token += 1;
        conn.results.push_back(XApuResult {
            token: conn.next_token,
            status: S_OK,
            command_type: ty,
        });
        S_OK
    })
}

/// `HRESULT XApuDequeueResult(HANDLE, XAPU_RESULT* out)` — `E_PENDING` (négatif) si la file
/// est vide, sortie alors non modifiée.
pub(crate) extern "win64" fn XApuDequeueResult(h: usize, out: *mut XApuResult) -> HResult {
    ffi_guard!(CHANNEL, "XApuDequeueResult", E_INVALIDARG, {
        if !plausible_ptr(out) {
            return E_INVALIDARG;
        }
        let mut map = XAPU.lock();
        let Some(conn) = map.get_mut(&h) else {
            return E_HANDLE;
        };
        let Some(r) = conn.results.pop_front() else {
            return E_PENDING;
        };
        // SAFETY: pointeur de sortie validé.
        unsafe { out.write_unaligned(r) };
        S_OK
    })
}

/// `UINT32 XApuGetResultCount(HANDLE)` — 0 pour un handle inconnu.
pub(crate) extern "win64" fn XApuGetResultCount(h: usize) -> u32 {
    ffi_guard!(CHANNEL, "XApuGetResultCount", 0, {
        XAPU.lock().get(&h).map_or(0, |c| c.results.len() as u32)
    })
}

pub(crate) fn xapu_exports() -> HashMap<&'static str, usize> {
    let mut m: HashMap<&'static str, usize> = HashMap::new();
    m.insert("XApuConnect", XApuConnect as *const () as usize);
    m.insert("XApuDisconnect", XApuDisconnect as *const () as usize);
    m.insert(
        "XApuEnqueueCommand",
        XApuEnqueueCommand as *const () as usize,
    );
    m.insert("XApuDequeueResult", XApuDequeueResult as *const () as usize);
    m.insert(
        "XApuGetResultCount",
        XApuGetResultCount as *const () as usize,
    );
    m
}

// ==================== chad (XDsp*) ====================

static DSP_ACTIVE: AtomicU32 = AtomicU32::new(0);

/// `HRESULT XDspActivate(...)` — arguments non lus ; active le DSP logiciel.
pub(crate) extern "win64" fn XDspActivate() -> HResult {
    ffi_guard!(CHANNEL, "XDspActivate", E_OUTOFMEMORY, {
        DSP_ACTIVE.store(1, Ordering::Release);
        S_OK
    })
}

/// `HRESULT XDspDeactivate(...)`.
pub(crate) extern "win64" fn XDspDeactivate() -> HResult {
    ffi_guard!(CHANNEL, "XDspDeactivate", E_OUTOFMEMORY, {
        if DSP_ACTIVE.swap(0, Ordering::AcqRel) == 0 {
            E_NOT_VALID_STATE
        } else {
            S_OK
        }
    })
}

/// `HRESULT XDspConnectWithMaximumStreamLimit(?, ?, ?, UINT32 maxStreams /*0x80 vu*/)`.
///
/// Échec documenté : la position du paramètre de sortie (handle) parmi les trois premiers
/// arguments n'est pas connue publiquement ; plutôt que d'écrire à l'aveugle ou de renvoyer
/// un faux succès sans handle, on renvoie `E_NOTIMPL` (l'appelant dispose d'un chemin
/// d'échec et retombe sur le mixage logiciel XAudio2).
pub(crate) extern "win64" fn XDspConnectWithMaximumStreamLimit(
    _a: usize,
    _b: usize,
    _c: usize,
    max_streams: u32,
) -> HResult {
    ffi_guard!(CHANNEL, "XDspConnectWithMaximumStreamLimit", E_NOTIMPL, {
        if DSP_ACTIVE.load(Ordering::Acquire) == 0 {
            return E_NOT_VALID_STATE;
        }
        sb_fixme!(
            CHANNEL,
            "XDspConnectWithMaximumStreamLimit(max {max_streams}) : DSP matériel indisponible -> E_NOTIMPL"
        );
        E_NOTIMPL
    })
}

/// `HRESULT XDspDisconnect(HANDLE)` — aucun handle n'étant jamais émis : `E_HANDLE`.
pub(crate) extern "win64" fn XDspDisconnect(_h: usize) -> HResult {
    ffi_guard!(CHANNEL, "XDspDisconnect", E_HANDLE, { E_HANDLE })
}

/// `HRESULT XDspSubmitCommand(HANDLE, const void* cmd)` — aucun handle valide : `E_HANDLE`.
pub(crate) extern "win64" fn XDspSubmitCommand(_h: usize, cmd: *const c_void) -> HResult {
    ffi_guard!(CHANNEL, "XDspSubmitCommand", E_HANDLE, {
        if !plausible_ptr(cmd) {
            return E_INVALIDARG;
        }
        E_HANDLE
    })
}

pub(crate) fn chad_exports() -> HashMap<&'static str, usize> {
    let mut m: HashMap<&'static str, usize> = HashMap::new();
    m.insert("XDspActivate", XDspActivate as *const () as usize);
    m.insert("XDspDeactivate", XDspDeactivate as *const () as usize);
    m.insert(
        "XDspConnectWithMaximumStreamLimit",
        XDspConnectWithMaximumStreamLimit as *const () as usize,
    );
    m.insert("XDspDisconnect", XDspDisconnect as *const () as usize);
    m.insert("XDspSubmitCommand", XDspSubmitCommand as *const () as usize);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apu_heap_alloc_map_free() {
        let mut va: *mut c_void = core::ptr::null_mut();
        let mut pa = 0u32;
        // Le tas est global au processus : on tolère qu'il existe déjà.
        let hr = ApuCreateHeap(0, 0x22_6E00);
        assert!(hr == S_OK || hr == E_ALREADY_INITIALIZED);
        assert_eq!(ApuCreateHeap(0, 16), E_ALREADY_INITIALIZED);
        assert_eq!(ApuAlloc(&mut va, &mut pa, 0, 0), E_INVALIDARG);
        assert_eq!(ApuAlloc(&mut va, &mut pa, 100, 3), E_INVALIDARG);
        assert_eq!(ApuAlloc(&mut va, &mut pa, 4096, 0x800), S_OK);
        assert!(!va.is_null());
        assert_eq!(pa % 0x800, 0);
        assert_eq!(ApuMapApuAddress(pa), va);
        assert_eq!(ApuMapVirtualAddress(va, 0x800), pa);
        assert_eq!(ApuIsVirtualAddressValid(va, 0x800), WIN_TRUE);
        assert_eq!(ApuIsVirtualAddressValid(va, 0x2000), WIN_FALSE);
        // SAFETY: bloc de 4096 octets alloué ci-dessus, zéro-initialisé.
        unsafe {
            assert_eq!(*va.cast::<u8>().add(4095), 0);
            *va.cast::<u8>() = 0x5A;
        }
        let mut va2: *mut c_void = core::ptr::null_mut();
        assert_eq!(ApuAlloc(&mut va2, core::ptr::null_mut(), 64, 0), S_OK);
        assert_ne!(va, va2);
        assert_eq!(ApuFree(va), S_OK);
        assert_eq!(ApuFree(va), E_INVALIDARG);
        assert_eq!(ApuIsVirtualAddressValid(va, 0x40), WIN_FALSE);
        assert_eq!(ApuFree(va2), S_OK);
        let mut huge: *mut c_void = core::ptr::null_mut();
        assert_eq!(ApuAlloc(&mut huge, &mut pa, u32::MAX, 0), E_OUTOFMEMORY);
    }

    #[test]
    fn apu_translation_for_host_buffers() {
        let buf = vec![0u8; 0x3000];
        let p = buf.as_ptr().cast::<c_void>();
        assert_eq!(ApuIsVirtualAddressValid(p, 0x800), WIN_TRUE);
        let apu = ApuMapVirtualAddress(p, 0x800);
        assert!(apu >= XLATE_APU_BASE);
        assert_eq!(ApuMapApuAddress(apu), p.cast_mut());
        // Même buffer : même traduction ; décalage interne préservé.
        assert_eq!(ApuMapVirtualAddress(p, 0x800), apu);
        // SAFETY: décalage dans `buf`.
        let p2 = unsafe { p.byte_add(0x100) };
        assert_eq!(ApuMapVirtualAddress(p2, 0x40), apu + 0x100);
        assert_eq!(ApuIsVirtualAddressValid(core::ptr::null(), 0x40), WIN_FALSE);
        assert_eq!(ApuMapVirtualAddress(core::ptr::null(), 0x40), 0);
        assert!(ApuMapApuAddress(0xFFFF_FFF0).is_null());
    }

    #[test]
    fn acphal_shape_contexts_and_interface() {
        let mut obj: *mut c_void = core::ptr::null_mut();
        assert_eq!(AcpHalCreate(core::ptr::null_mut(), 0), E_POINTER);
        assert_eq!(AcpHalCreate(&mut obj, 0), S_OK);
        // SAFETY: objet COM retourné ci-dessus ; slot 2 = Release, slot 5 = méthode privée.
        unsafe {
            let vt = *obj.cast::<*const usize>();
            let release: extern "win64" fn(*mut c_void) -> u32 = core::mem::transmute(*vt.add(2));
            let other: extern "win64" fn(*mut c_void) -> HResult = core::mem::transmute(*vt.add(5));
            assert_eq!(other(obj), E_NOTIMPL);
            assert_eq!(release(obj), 0);
        }
        let mut block: *mut c_void = core::ptr::null_mut();
        assert_eq!(AcpHalAllocateShapeContexts(&mut block), S_OK);
        assert!(!block.is_null());
        assert_eq!(block as usize % 128, 0);
        // SAFETY: bloc de SHAPE_BLOCK_BYTES octets alloué ci-dessus.
        assert!(
            unsafe { core::slice::from_raw_parts(block.cast::<u8>(), SHAPE_BLOCK_BYTES) }
                .iter()
                .all(|&b| b == 0)
        );
        assert_eq!(AcpHalReleaseShapeContexts(block), S_OK);
        assert_eq!(AcpHalReleaseShapeContexts(block), E_INVALIDARG);
    }

    #[test]
    fn xapu_enqueue_dequeue_counts() {
        let h = XApuConnect();
        assert_ne!(h, 0);
        let cmd = [7u32, 1, 2, 3];
        for _ in 0..3 {
            assert_eq!(XApuEnqueueCommand(h, cmd.as_ptr().cast(), 0), S_OK);
        }
        assert_eq!(XApuGetResultCount(h), 3);
        let mut r = XApuResult::default();
        assert_eq!(XApuDequeueResult(h, &mut r), S_OK);
        assert_eq!(
            r,
            XApuResult {
                token: 1,
                status: S_OK,
                command_type: 7
            }
        );
        assert_eq!(XApuDequeueResult(h, &mut r), S_OK);
        assert_eq!(XApuDequeueResult(h, &mut r), S_OK);
        assert_eq!(r.token, 3);
        assert_eq!(XApuDequeueResult(h, &mut r), E_PENDING);
        assert_eq!(XApuGetResultCount(h), 0);
        assert_eq!(XApuDisconnect(h, 0x20), S_OK);
        assert_eq!(XApuEnqueueCommand(h, cmd.as_ptr().cast(), 0), E_HANDLE);
        assert_eq!(XApuDisconnect(h, 0x20), E_HANDLE);
    }

    #[test]
    fn chad_dsp_fails_cleanly() {
        let cmd = [1u8; 8];
        assert_eq!(XDspActivate(), S_OK);
        assert_eq!(XDspConnectWithMaximumStreamLimit(0, 0, 0, 0x80), E_NOTIMPL);
        assert_eq!(
            XDspSubmitCommand(0x1234_5678, cmd.as_ptr().cast()),
            E_HANDLE
        );
        assert_eq!(XDspDisconnect(0x1234_5678), E_HANDLE);
        assert_eq!(XDspDeactivate(), S_OK);
    }
}
