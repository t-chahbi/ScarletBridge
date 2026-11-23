//! Media Foundation (`mfplat.dll`, `mfreadwrite.dll`) — signatures MSDN publiques.
//!
//! Implémenté fidèlement : `MFStartup`/`MFShutdown` (contrôle de `MF_VERSION`),
//! `MFGetSystemTime` (100 ns monotone), `IMFAttributes` complet, `IMFMediaType`, `IMFMediaBuffer`
//! mémoire, `IMFSample`. Aucun décodeur n'est disponible (pas de codecs système) :
//! `IMFSourceResolver` et les `MFCreateSourceReader*` échouent proprement avec
//! `MF_E_UNSUPPORTED_BYTESTREAM_TYPE` (ou `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)` /
//! `MF_E_UNSUPPORTED_SCHEME` / `MF_E_PLATFORM_NOT_INITIALIZED` selon le cas), sorties à NULL.
//!
//! Les allocations rendues à l'appelant (`PROPVARIANT`, `GetAllocatedString/Blob`) utilisent
//! l'allocateur de `CoTaskMemAlloc` HLE (`malloc`), libérable par `CoTaskMemFree`/`PropVariantClear`.

#![allow(non_snake_case)]

use core::ffi::c_void;
use core::mem::{offset_of, size_of};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::thread::ThreadId;

use parking_lot::{Condvar, Mutex};
use sb_common::abi::{
    E_INVALIDARG, E_NOINTERFACE, E_OUTOFMEMORY, E_POINTER, E_UNEXPECTED, HResult, S_FALSE, S_OK,
    WIN_FALSE, WIN_TRUE, WinBool,
};
use sb_common::{ffi_guard, sb_debug};

use crate::com::{Guid, IID_IUNKNOWN};

const CHANNEL: &str = "mfplat";

// ==================== Constantes publiques ====================

#[cfg_attr(not(test), allow(dead_code))]
pub const MF_VERSION: u32 = 0x0002_0070;
const MF_API_VERSION: u32 = 0x0070;

pub const MF_E_PLATFORM_NOT_INITIALIZED: HResult = 0xC00D_36B0_u32 as i32;
pub const MF_E_UNSUPPORTED_REPRESENTATION: HResult = 0xC00D_36B7_u32 as i32;
pub const MF_E_INVALIDTYPE: HResult = 0xC00D_36BD_u32 as i32;
pub const MF_E_UNSUPPORTED_SCHEME: HResult = 0xC00D_36C3_u32 as i32;
pub const MF_E_UNSUPPORTED_BYTESTREAM_TYPE: HResult = 0xC00D_36C4_u32 as i32;
pub const MF_E_NO_SAMPLE_TIMESTAMP: HResult = 0xC00D_36C8_u32 as i32;
pub const MF_E_NO_SAMPLE_DURATION: HResult = 0xC00D_36C9_u32 as i32;
pub const MF_E_BAD_STARTUP_VERSION: HResult = 0xC00D_36E3_u32 as i32;
pub const MF_E_ATTRIBUTENOTFOUND: HResult = 0xC00D_36E6_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER)` (= `STRSAFE_E_INSUFFICIENT_BUFFER`).
pub const E_NOT_SUFFICIENT_BUFFER: HResult = 0x8007_007A_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)`.
pub const E_FILE_NOT_FOUND: HResult = 0x8007_0002_u32 as i32;

const MF_OBJECT_INVALID: u32 = 2;

// VARTYPE / MF_ATTRIBUTE_TYPE
pub const VT_R8: u16 = 5;
pub const VT_UNKNOWN: u16 = 13;
pub const VT_UI4: u16 = 19;
pub const VT_UI8: u16 = 21;
pub const VT_LPWSTR: u16 = 31;
pub const VT_CLSID: u16 = 72;
pub const VT_VECTOR_UI1: u16 = 0x1011;

pub const IID_IMFATTRIBUTES: Guid = Guid::new(
    0x2CD2_D921,
    0xC447,
    0x44A7,
    [0xA1, 0x3C, 0x4A, 0xDA, 0xBF, 0xC2, 0x47, 0xE3],
);
pub const IID_IMFMEDIATYPE: Guid = Guid::new(
    0x44AE_0FA8,
    0xEA31,
    0x4109,
    [0x8D, 0x2E, 0x4C, 0xAE, 0x49, 0x97, 0xC5, 0x55],
);
pub const IID_IMFSAMPLE: Guid = Guid::new(
    0xC40A_00F2,
    0xB93A,
    0x4D80,
    [0xAE, 0x8C, 0x5A, 0x1C, 0x63, 0x4F, 0x58, 0xE4],
);
pub const IID_IMFMEDIABUFFER: Guid = Guid::new(
    0x045F_A593,
    0x8799,
    0x42B8,
    [0xBC, 0x8D, 0x89, 0x68, 0xC6, 0x45, 0x35, 0x07],
);
pub const IID_IMFSOURCERESOLVER: Guid = Guid::new(
    0xFBE5_A32D,
    0xA497,
    0x4B61,
    [0xBB, 0x85, 0x97, 0xB1, 0xA8, 0x48, 0xA6, 0xE3],
);
pub const MF_MT_MAJOR_TYPE: Guid = Guid::new(
    0x48EB_A18E,
    0xF8C9,
    0x4687,
    [0xBF, 0x11, 0x0A, 0x74, 0xC9, 0xF9, 0x6A, 0x8F],
);
pub const MF_MT_SUBTYPE: Guid = Guid::new(
    0xF7E3_4C9A,
    0x42E8,
    0x4714,
    [0xB7, 0x4B, 0xCB, 0x29, 0xD7, 0x2C, 0x35, 0xE5],
);
pub const MF_MT_ALL_SAMPLES_INDEPENDENT: Guid = Guid::new(
    0xC917_3739,
    0x5E1D,
    0x461C,
    [0xB7, 0x13, 0x46, 0xFB, 0x99, 0x5C, 0xB9, 0x5F],
);
pub const MF_MT_USER_DATA: Guid = Guid::new(
    0xB6BC_765F,
    0x4C3B,
    0x40A4,
    [0xBD, 0x51, 0x25, 0x35, 0xB6, 0x6F, 0xE0, 0x9D],
);

const MF_MEDIATYPE_EQUAL_MAJOR_TYPES: u32 = 1;
const MF_MEDIATYPE_EQUAL_FORMAT_TYPES: u32 = 2;
const MF_MEDIATYPE_EQUAL_FORMAT_DATA: u32 = 4;
const MF_MEDIATYPE_EQUAL_FORMAT_USER_DATA: u32 = 8;

// ==================== PROPVARIANT ====================

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PropVariant {
    pub vt: u16,
    pub reserved: [u16; 3],
    pub data: [u64; 2],
}

const _: () = {
    assert!(size_of::<PropVariant>() == 24);
    assert!(offset_of!(PropVariant, data) == 8);
    assert!(size_of::<Guid>() == 16);
};

// ==================== Appels de vtables COM génériques ====================

type QiFn = extern "win64" fn(*mut c_void, *const Guid, *mut *mut c_void) -> HResult;
type RefFn = extern "win64" fn(*mut c_void) -> u32;

/// # Safety
/// `obj` doit être un objet COM valide dont la vtable possède au moins `slot + 1` entrées
/// et dont l'entrée `slot` a la signature `F`.
unsafe fn vfn<F: Copy>(obj: *mut c_void, slot: usize) -> F {
    debug_assert_eq!(size_of::<F>(), size_of::<usize>());
    // SAFETY: garanti par l'appelant.
    unsafe {
        let vt = *obj.cast::<*const usize>();
        let f = *vt.add(slot);
        core::mem::transmute_copy::<usize, F>(&f)
    }
}

fn unk_add_ref(p: usize) {
    if p != 0 {
        // SAFETY: `p` est un IUnknown valide conservé par nos objets (slot 1 = AddRef).
        unsafe { vfn::<RefFn>(p as *mut c_void, 1)(p as *mut c_void) };
    }
}

fn unk_release(p: usize) {
    if p != 0 {
        // SAFETY: `p` est un IUnknown valide (slot 2 = Release).
        unsafe { vfn::<RefFn>(p as *mut c_void, 2)(p as *mut c_void) };
    }
}

fn co_alloc(bytes: usize) -> *mut u8 {
    // SAFETY: même allocateur que `CoTaskMemAlloc` HLE (`malloc`).
    unsafe { libc::malloc(bytes.max(1)) }.cast()
}

// ==================== Store d'attributs ====================

#[derive(Debug, Clone, PartialEq)]
enum AttrValue {
    U32(u32),
    U64(u64),
    F64(f64),
    Guid(Guid),
    Str(Vec<u16>),
    Blob(Vec<u8>),
    Unknown(usize),
}

impl AttrValue {
    const fn vt(&self) -> u16 {
        match self {
            Self::U32(_) => VT_UI4,
            Self::U64(_) => VT_UI8,
            Self::F64(_) => VT_R8,
            Self::Guid(_) => VT_CLSID,
            Self::Str(_) => VT_LPWSTR,
            Self::Blob(_) => VT_VECTOR_UI1,
            Self::Unknown(_) => VT_UNKNOWN,
        }
    }

    /// Lit un PROPVARIANT de l'invité (copie profonde, AddRef pour VT_UNKNOWN).
    ///
    /// # Safety
    /// `pv` doit être un PROPVARIANT valide.
    unsafe fn from_propvariant(pv: &PropVariant) -> Option<Self> {
        let d0 = pv.data[0];
        // SAFETY: les pointeurs des variantes sont valides selon le type (contrat PROPVARIANT).
        unsafe {
            Some(match pv.vt {
                VT_UI4 => Self::U32(d0 as u32),
                VT_UI8 => Self::U64(d0),
                VT_R8 => Self::F64(f64::from_bits(d0)),
                VT_CLSID => {
                    if d0 == 0 {
                        return None;
                    }
                    Self::Guid(*(d0 as *const Guid))
                }
                VT_LPWSTR => Self::Str(read_wstr(d0 as *const u16)?),
                VT_VECTOR_UI1 => {
                    let n = d0 as u32 as usize;
                    let p = pv.data[1] as *const u8;
                    if n > 0 && p.is_null() {
                        return None;
                    }
                    Self::Blob(if n == 0 {
                        Vec::new()
                    } else {
                        core::slice::from_raw_parts(p, n).to_vec()
                    })
                }
                VT_UNKNOWN => {
                    if d0 == 0 {
                        return None;
                    }
                    unk_add_ref(d0 as usize);
                    Self::Unknown(d0 as usize)
                }
                _ => return None,
            })
        }
    }

    /// PROPVARIANT *empruntant* nos données (pour SetItem/CompareItem sur un autre store).
    fn borrowed_propvariant(&self) -> PropVariant {
        let mut pv = PropVariant {
            vt: self.vt(),
            ..Default::default()
        };
        match self {
            Self::U32(v) => pv.data[0] = u64::from(*v),
            Self::U64(v) => pv.data[0] = *v,
            Self::F64(v) => pv.data[0] = v.to_bits(),
            Self::Guid(g) => pv.data[0] = g as *const Guid as u64,
            Self::Str(s) => pv.data[0] = s.as_ptr() as u64,
            Self::Blob(b) => {
                pv.data[0] = b.len() as u64;
                pv.data[1] = b.as_ptr() as u64;
            }
            Self::Unknown(p) => pv.data[0] = *p as u64,
        }
        pv
    }

    /// PROPVARIANT *possédé* par l'appelant (CoTaskMemAlloc / AddRef).
    fn owned_propvariant(&self) -> PropVariant {
        let mut pv = self.borrowed_propvariant();
        match self {
            Self::Guid(g) => {
                let p = co_alloc(16).cast::<Guid>();
                // SAFETY: allocation de 16 octets.
                unsafe { p.write_unaligned(*g) };
                pv.data[0] = p as u64;
            }
            Self::Str(s) => pv.data[0] = alloc_wstr(s) as u64,
            Self::Blob(b) => {
                let p = co_alloc(b.len());
                // SAFETY: `p` dispose de `b.len()` octets.
                unsafe { core::ptr::copy_nonoverlapping(b.as_ptr(), p, b.len()) };
                pv.data[1] = p as u64;
            }
            Self::Unknown(p) => unk_add_ref(*p),
            _ => {}
        }
        pv
    }
}

/// # Safety
/// `p` NULL ou chaîne UTF-16 terminée par NUL.
unsafe fn read_wstr(p: *const u16) -> Option<Vec<u16>> {
    if p.is_null() {
        return None;
    }
    let mut n = 0;
    // SAFETY: chaîne terminée par NUL (contrat appelant).
    unsafe {
        while *p.add(n) != 0 {
            n += 1;
        }
        Some(core::slice::from_raw_parts(p, n).to_vec())
    }
}

/// Copie terminée par NUL allouée via l'allocateur CoTaskMem.
fn alloc_wstr(s: &[u16]) -> *mut u16 {
    let p = co_alloc((s.len() + 1) * 2).cast::<u16>();
    // SAFETY: `p` dispose de `s.len() + 1` unités.
    unsafe {
        core::ptr::copy_nonoverlapping(s.as_ptr(), p, s.len());
        *p.add(s.len()) = 0;
    }
    p
}

#[derive(Default)]
struct AttrStore {
    items: Mutex<Vec<(Guid, AttrValue)>>,
    /// Verrou réentrant LockStore/UnlockStore : (propriétaire, profondeur).
    owner: Mutex<(Option<ThreadId>, u32)>,
    cv: Condvar,
}

impl AttrStore {
    fn enter(&self) {
        let me = std::thread::current().id();
        let mut o = self.owner.lock();
        while matches!(o.0, Some(t) if t != me) {
            self.cv.wait(&mut o);
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Vec<(Guid, AttrValue)>) -> R) -> R {
        self.enter();
        f(&mut self.items.lock())
    }

    fn get(&self, key: &Guid) -> Option<AttrValue> {
        self.with(|v| v.iter().find(|(k, _)| k == key).map(|(_, x)| x.clone()))
    }

    fn set(&self, key: Guid, val: AttrValue) {
        let old = self.with(|v| {
            if let Some(slot) = v.iter_mut().find(|(k, _)| *k == key) {
                Some(core::mem::replace(&mut slot.1, val))
            } else {
                v.push((key, val));
                None
            }
        });
        if let Some(AttrValue::Unknown(p)) = old {
            unk_release(p);
        }
    }

    fn remove(&self, key: &Guid) {
        let old = self.with(|v| v.iter().position(|(k, _)| k == key).map(|i| v.remove(i).1));
        if let Some(AttrValue::Unknown(p)) = old {
            unk_release(p);
        }
    }

    fn clear(&self) {
        let old = self.with(core::mem::take);
        for (_, v) in old {
            if let AttrValue::Unknown(p) = v {
                unk_release(p);
            }
        }
    }

    fn snapshot(&self) -> Vec<(Guid, AttrValue)> {
        self.with(|v| v.clone())
    }

    fn lock_store(&self) {
        let me = std::thread::current().id();
        let mut o = self.owner.lock();
        while matches!(o.0, Some(t) if t != me) {
            self.cv.wait(&mut o);
        }
        o.0 = Some(me);
        o.1 += 1;
    }

    fn unlock_store(&self) -> HResult {
        let me = std::thread::current().id();
        let mut o = self.owner.lock();
        if o.0 != Some(me) || o.1 == 0 {
            return E_UNEXPECTED;
        }
        o.1 -= 1;
        if o.1 == 0 {
            o.0 = None;
            self.cv.notify_all();
        }
        S_OK
    }
}

// ==================== Objet attributs / type / échantillon ====================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Attributes,
    MediaType,
    Sample,
}

#[derive(Default)]
struct SampleData {
    flags: u32,
    time: Option<i64>,
    duration: Option<i64>,
    buffers: Vec<usize>,
}

#[repr(C)]
struct MfObj {
    vtbl: *const c_void,
    refs: AtomicU32,
    kind: Kind,
    store: AttrStore,
    sample: Mutex<SampleData>,
}

static LIVE_OBJECTS: AtomicUsize = AtomicUsize::new(0);

fn new_obj(kind: Kind) -> *mut MfObj {
    let vtbl: *const c_void = match kind {
        Kind::Attributes => (&raw const ATTR_VTBL).cast(),
        Kind::MediaType => (&raw const MEDIATYPE_VTBL).cast(),
        Kind::Sample => (&raw const SAMPLE_VTBL).cast(),
    };
    LIVE_OBJECTS.fetch_add(1, Ordering::Relaxed);
    Box::into_raw(Box::new(MfObj {
        vtbl,
        refs: AtomicU32::new(1),
        kind,
        store: AttrStore::default(),
        sample: Mutex::new(SampleData::default()),
    }))
}

/// Retourne `Some(&MfObj)` si `p` est l'un de nos objets attributs.
fn as_ours<'a>(p: *mut c_void) -> Option<&'a MfObj> {
    if p.is_null() {
        return None;
    }
    // SAFETY: lecture du premier mot (vtable) d'un objet COM valide.
    let vt = unsafe { *p.cast::<*const c_void>() };
    let ours = core::ptr::eq(vt, (&raw const ATTR_VTBL).cast())
        || core::ptr::eq(vt, (&raw const MEDIATYPE_VTBL).cast())
        || core::ptr::eq(vt, (&raw const SAMPLE_VTBL).cast());
    // SAFETY: vtable reconnue : `p` est un `MfObj` vivant.
    ours.then(|| unsafe { &*p.cast::<MfObj>() })
}

fn obj<'a>(this: *mut MfObj) -> Option<&'a MfObj> {
    // SAFETY: `this` provient de nos vtables (premier argument COM), donc un `MfObj` vivant.
    (!this.is_null()).then(|| unsafe { &*this })
}

macro_rules! guard_hr {
    ($name:expr, $body:block) => {
        ffi_guard!(CHANNEL, $name, E_UNEXPECTED, $body)
    };
}

fn read_guid(g: *const Guid) -> Option<Guid> {
    // SAFETY: pointeur REFGUID vérifié non-nul.
    (!g.is_null()).then(|| unsafe { g.read_unaligned() })
}

extern "win64" fn mf_qi(this: *mut MfObj, riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    guard_hr!("IMFAttributes::QueryInterface", {
        if ppv.is_null() {
            return E_POINTER;
        }
        // SAFETY: `ppv` vérifié non-nul.
        unsafe { *ppv = core::ptr::null_mut() };
        let (Some(o), Some(iid)) = (obj(this), read_guid(riid)) else {
            return E_INVALIDARG;
        };
        let ok = iid == IID_IUNKNOWN
            || iid == IID_IMFATTRIBUTES
            || (o.kind == Kind::MediaType && iid == IID_IMFMEDIATYPE)
            || (o.kind == Kind::Sample && iid == IID_IMFSAMPLE);
        if !ok {
            sb_debug!(CHANNEL, "QueryInterface({{{iid}}}) -> E_NOINTERFACE");
            return E_NOINTERFACE;
        }
        o.refs.fetch_add(1, Ordering::AcqRel);
        // SAFETY: `ppv` vérifié non-nul.
        unsafe { *ppv = this.cast() };
        S_OK
    })
}

extern "win64" fn mf_add_ref(this: *mut MfObj) -> u32 {
    ffi_guard!(CHANNEL, "IMFAttributes::AddRef", 0, {
        obj(this).map_or(0, |o| o.refs.fetch_add(1, Ordering::AcqRel) + 1)
    })
}

extern "win64" fn mf_release(this: *mut MfObj) -> u32 {
    ffi_guard!(CHANNEL, "IMFAttributes::Release", 0, {
        let Some(o) = obj(this) else { return 0 };
        let left = o.refs.fetch_sub(1, Ordering::AcqRel).saturating_sub(1);
        if left == 0 {
            o.store.clear();
            for b in core::mem::take(&mut o.sample.lock().buffers) {
                unk_release(b);
            }
            LIVE_OBJECTS.fetch_sub(1, Ordering::Relaxed);
            // SAFETY: dernier référent ; l'objet provenait de `Box::into_raw`.
            drop(unsafe { Box::from_raw(this) });
        }
        left
    })
}

fn typed_get<T>(
    this: *mut MfObj,
    key: *const Guid,
    out: *mut T,
    f: impl FnOnce(&AttrValue) -> Option<T>,
) -> HResult {
    let (Some(o), Some(k)) = (obj(this), read_guid(key)) else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        return E_POINTER;
    }
    let Some(v) = o.store.get(&k) else {
        return MF_E_ATTRIBUTENOTFOUND;
    };
    let Some(x) = f(&v) else {
        return MF_E_INVALIDTYPE;
    };
    // SAFETY: `out` vérifié non-nul.
    unsafe { out.write_unaligned(x) };
    S_OK
}

fn typed_set(this: *mut MfObj, key: *const Guid, val: AttrValue) -> HResult {
    let (Some(o), Some(k)) = (obj(this), read_guid(key)) else {
        return E_INVALIDARG;
    };
    o.store.set(k, val);
    S_OK
}

extern "win64" fn mf_get_item(this: *mut MfObj, key: *const Guid, pv: *mut PropVariant) -> HResult {
    guard_hr!("IMFAttributes::GetItem", {
        let (Some(o), Some(k)) = (obj(this), read_guid(key)) else {
            return E_INVALIDARG;
        };
        let Some(v) = o.store.get(&k) else {
            return MF_E_ATTRIBUTENOTFOUND;
        };
        if !pv.is_null() {
            // SAFETY: `pv` vérifié non-nul.
            unsafe { pv.write_unaligned(v.owned_propvariant()) };
        }
        S_OK
    })
}

extern "win64" fn mf_get_item_type(this: *mut MfObj, key: *const Guid, out: *mut u32) -> HResult {
    guard_hr!("IMFAttributes::GetItemType", {
        typed_get(this, key, out, |v| Some(u32::from(v.vt())))
    })
}

extern "win64" fn mf_compare_item(
    this: *mut MfObj,
    key: *const Guid,
    pv: *const PropVariant,
    out: *mut WinBool,
) -> HResult {
    guard_hr!("IMFAttributes::CompareItem", {
        let (Some(o), Some(k)) = (obj(this), read_guid(key)) else {
            return E_INVALIDARG;
        };
        if pv.is_null() || out.is_null() {
            return E_POINTER;
        }
        // SAFETY: `pv` vérifié non-nul.
        let pvv = unsafe { pv.read_unaligned() };
        let equal = match (o.store.get(&k), pvv.vt) {
            (Some(AttrValue::Unknown(p)), VT_UNKNOWN) => p as u64 == pvv.data[0],
            (Some(mine), _) => {
                // SAFETY: PROPVARIANT fourni par l'appelant ; VT_UNKNOWN traité ci-dessus.
                let theirs = unsafe { AttrValue::from_propvariant(&pvv) };
                if let Some(AttrValue::Unknown(p)) = theirs {
                    unk_release(p);
                }
                theirs.as_ref() == Some(&mine)
            }
            (None, _) => false,
        };
        // SAFETY: `out` vérifié non-nul.
        unsafe { *out = if equal { WIN_TRUE } else { WIN_FALSE } };
        S_OK
    })
}

type GetCountFn = extern "win64" fn(*mut c_void, *mut u32) -> HResult;
type CompareItemFn =
    extern "win64" fn(*mut c_void, *const Guid, *const PropVariant, *mut WinBool) -> HResult;
type GetItemFn = extern "win64" fn(*mut c_void, *const Guid, *mut PropVariant) -> HResult;
type CompareFn = extern "win64" fn(*mut c_void, *mut c_void, u32, *mut WinBool) -> HResult;

/// Vérifie que chaque élément de `items` existe et est identique dans `other` (IMFAttributes).
/// Si `skip_missing`, les clés absentes de `other` sont ignorées (intersection).
fn items_match_in(items: &[(Guid, AttrValue)], other: *mut c_void, skip_missing: bool) -> bool {
    for (k, v) in items {
        let pv = v.borrowed_propvariant();
        if skip_missing {
            // SAFETY: `other` est un IMFAttributes valide (slot 3 = GetItem).
            let hr = unsafe { vfn::<GetItemFn>(other, 3)(other, k, core::ptr::null_mut()) };
            if hr != S_OK {
                continue;
            }
        }
        let mut eq = WIN_FALSE;
        // SAFETY: `other` est un IMFAttributes valide (slot 5 = CompareItem).
        let hr = unsafe { vfn::<CompareItemFn>(other, 5)(other, k, &pv, &mut eq) };
        if hr != S_OK || eq == WIN_FALSE {
            return false;
        }
    }
    true
}

fn count_of(other: *mut c_void) -> u32 {
    let mut n = 0;
    // SAFETY: `other` est un IMFAttributes valide (slot 30 = GetCount).
    unsafe { vfn::<GetCountFn>(other, 30)(other, &mut n) };
    n
}

extern "win64" fn mf_compare(
    this: *mut MfObj,
    theirs: *mut c_void,
    match_type: u32,
    out: *mut WinBool,
) -> HResult {
    guard_hr!("IMFAttributes::Compare", {
        let Some(o) = obj(this) else {
            return E_INVALIDARG;
        };
        if theirs.is_null() || out.is_null() {
            return E_POINTER;
        }
        let mine = o.store.snapshot();
        let their_count = count_of(theirs);
        let reverse = |t: *mut c_void| -> bool {
            let mut r = WIN_FALSE;
            // SAFETY: IMFAttributes valide (slot 6 = Compare) ; MATCH_OUR_ITEMS côté distant.
            let hr = unsafe { vfn::<CompareFn>(t, 6)(t, this.cast(), 0, &mut r) };
            hr == S_OK && r != WIN_FALSE
        };
        let result = match match_type {
            0 => items_match_in(&mine, theirs, false),
            1 => reverse(theirs),
            2 => mine.len() as u32 == their_count && items_match_in(&mine, theirs, false),
            3 => items_match_in(&mine, theirs, true),
            4 => {
                if mine.len() as u32 <= their_count {
                    items_match_in(&mine, theirs, false)
                } else {
                    reverse(theirs)
                }
            }
            _ => return E_INVALIDARG,
        };
        // SAFETY: `out` vérifié non-nul.
        unsafe { *out = if result { WIN_TRUE } else { WIN_FALSE } };
        S_OK
    })
}

extern "win64" fn mf_get_u32(this: *mut MfObj, key: *const Guid, out: *mut u32) -> HResult {
    guard_hr!("IMFAttributes::GetUINT32", {
        typed_get(this, key, out, |v| match v {
            AttrValue::U32(x) => Some(*x),
            _ => None,
        })
    })
}

extern "win64" fn mf_get_u64(this: *mut MfObj, key: *const Guid, out: *mut u64) -> HResult {
    guard_hr!("IMFAttributes::GetUINT64", {
        typed_get(this, key, out, |v| match v {
            AttrValue::U64(x) => Some(*x),
            _ => None,
        })
    })
}

extern "win64" fn mf_get_double(this: *mut MfObj, key: *const Guid, out: *mut f64) -> HResult {
    guard_hr!("IMFAttributes::GetDouble", {
        typed_get(this, key, out, |v| match v {
            AttrValue::F64(x) => Some(*x),
            _ => None,
        })
    })
}

extern "win64" fn mf_get_guid(this: *mut MfObj, key: *const Guid, out: *mut Guid) -> HResult {
    guard_hr!("IMFAttributes::GetGUID", {
        typed_get(this, key, out, |v| match v {
            AttrValue::Guid(x) => Some(*x),
            _ => None,
        })
    })
}

extern "win64" fn mf_get_string_length(
    this: *mut MfObj,
    key: *const Guid,
    out: *mut u32,
) -> HResult {
    guard_hr!("IMFAttributes::GetStringLength", {
        typed_get(this, key, out, |v| match v {
            AttrValue::Str(s) => Some(s.len() as u32),
            _ => None,
        })
    })
}

extern "win64" fn mf_get_string(
    this: *mut MfObj,
    key: *const Guid,
    buf: *mut u16,
    cch: u32,
    out_len: *mut u32,
) -> HResult {
    guard_hr!("IMFAttributes::GetString", {
        let (Some(o), Some(k)) = (obj(this), read_guid(key)) else {
            return E_INVALIDARG;
        };
        if buf.is_null() {
            return E_POINTER;
        }
        let s = match o.store.get(&k) {
            None => return MF_E_ATTRIBUTENOTFOUND,
            Some(AttrValue::Str(s)) => s,
            Some(_) => return MF_E_INVALIDTYPE,
        };
        if (cch as usize) < s.len() + 1 {
            return E_NOT_SUFFICIENT_BUFFER;
        }
        // SAFETY: `buf` dispose de `cch >= len + 1` unités.
        unsafe {
            core::ptr::copy_nonoverlapping(s.as_ptr(), buf, s.len());
            *buf.add(s.len()) = 0;
            if !out_len.is_null() {
                *out_len = s.len() as u32;
            }
        }
        S_OK
    })
}

extern "win64" fn mf_get_allocated_string(
    this: *mut MfObj,
    key: *const Guid,
    out: *mut *mut u16,
    out_len: *mut u32,
) -> HResult {
    guard_hr!("IMFAttributes::GetAllocatedString", {
        if out.is_null() || out_len.is_null() {
            return E_POINTER;
        }
        let mut len = 0;
        let hr = typed_get(this, key, out, |v| match v {
            AttrValue::Str(s) => {
                len = s.len() as u32;
                Some(alloc_wstr(s))
            }
            _ => None,
        });
        if hr == S_OK {
            // SAFETY: `out_len` vérifié non-nul.
            unsafe { *out_len = len };
        }
        hr
    })
}

extern "win64" fn mf_get_blob_size(this: *mut MfObj, key: *const Guid, out: *mut u32) -> HResult {
    guard_hr!("IMFAttributes::GetBlobSize", {
        typed_get(this, key, out, |v| match v {
            AttrValue::Blob(b) => Some(b.len() as u32),
            _ => None,
        })
    })
}

extern "win64" fn mf_get_blob(
    this: *mut MfObj,
    key: *const Guid,
    buf: *mut u8,
    cb: u32,
    out_size: *mut u32,
) -> HResult {
    guard_hr!("IMFAttributes::GetBlob", {
        let (Some(o), Some(k)) = (obj(this), read_guid(key)) else {
            return E_INVALIDARG;
        };
        let b = match o.store.get(&k) {
            None => return MF_E_ATTRIBUTENOTFOUND,
            Some(AttrValue::Blob(b)) => b,
            Some(_) => return MF_E_INVALIDTYPE,
        };
        if (cb as usize) < b.len() {
            return E_NOT_SUFFICIENT_BUFFER;
        }
        if buf.is_null() && !b.is_empty() {
            return E_POINTER;
        }
        // SAFETY: `buf` dispose de `cb >= len` octets ; `out_size` optionnel.
        unsafe {
            if !b.is_empty() {
                core::ptr::copy_nonoverlapping(b.as_ptr(), buf, b.len());
            }
            if !out_size.is_null() {
                *out_size = b.len() as u32;
            }
        }
        S_OK
    })
}

extern "win64" fn mf_get_allocated_blob(
    this: *mut MfObj,
    key: *const Guid,
    out: *mut *mut u8,
    out_size: *mut u32,
) -> HResult {
    guard_hr!("IMFAttributes::GetAllocatedBlob", {
        if out.is_null() || out_size.is_null() {
            return E_POINTER;
        }
        let mut len = 0;
        let hr = typed_get(this, key, out, |v| match v {
            AttrValue::Blob(b) => {
                len = b.len() as u32;
                let p = co_alloc(b.len());
                // SAFETY: `p` dispose de `b.len()` octets.
                unsafe { core::ptr::copy_nonoverlapping(b.as_ptr(), p, b.len()) };
                Some(p)
            }
            _ => None,
        });
        if hr == S_OK {
            // SAFETY: `out_size` vérifié non-nul.
            unsafe { *out_size = len };
        }
        hr
    })
}

extern "win64" fn mf_get_unknown(
    this: *mut MfObj,
    key: *const Guid,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    guard_hr!("IMFAttributes::GetUnknown", {
        if ppv.is_null() {
            return E_POINTER;
        }
        // SAFETY: `ppv` vérifié non-nul.
        unsafe { *ppv = core::ptr::null_mut() };
        let (Some(o), Some(k)) = (obj(this), read_guid(key)) else {
            return E_INVALIDARG;
        };
        match o.store.get(&k) {
            None => MF_E_ATTRIBUTENOTFOUND,
            Some(AttrValue::Unknown(p)) => {
                // SAFETY: IUnknown valide conservé (slot 0 = QueryInterface).
                unsafe { vfn::<QiFn>(p as *mut c_void, 0)(p as *mut c_void, riid, ppv) }
            }
            Some(_) => MF_E_INVALIDTYPE,
        }
    })
}

extern "win64" fn mf_set_item(
    this: *mut MfObj,
    key: *const Guid,
    pv: *const PropVariant,
) -> HResult {
    guard_hr!("IMFAttributes::SetItem", {
        if pv.is_null() {
            return E_POINTER;
        }
        // SAFETY: `pv` vérifié non-nul ; contenu selon `vt`.
        let v = unsafe { AttrValue::from_propvariant(&pv.read_unaligned()) };
        let Some(v) = v else {
            return MF_E_INVALIDTYPE;
        };
        if obj(this).is_none() || key.is_null() {
            if let AttrValue::Unknown(p) = v {
                unk_release(p);
            }
            return E_INVALIDARG;
        }
        typed_set(this, key, v)
    })
}

extern "win64" fn mf_delete_item(this: *mut MfObj, key: *const Guid) -> HResult {
    guard_hr!("IMFAttributes::DeleteItem", {
        let (Some(o), Some(k)) = (obj(this), read_guid(key)) else {
            return E_INVALIDARG;
        };
        o.store.remove(&k);
        S_OK
    })
}

extern "win64" fn mf_delete_all(this: *mut MfObj) -> HResult {
    guard_hr!("IMFAttributes::DeleteAllItems", {
        let Some(o) = obj(this) else {
            return E_INVALIDARG;
        };
        o.store.clear();
        S_OK
    })
}

extern "win64" fn mf_set_u32(this: *mut MfObj, key: *const Guid, v: u32) -> HResult {
    guard_hr!("IMFAttributes::SetUINT32", {
        typed_set(this, key, AttrValue::U32(v))
    })
}

extern "win64" fn mf_set_u64(this: *mut MfObj, key: *const Guid, v: u64) -> HResult {
    guard_hr!("IMFAttributes::SetUINT64", {
        typed_set(this, key, AttrValue::U64(v))
    })
}

extern "win64" fn mf_set_double(this: *mut MfObj, key: *const Guid, v: f64) -> HResult {
    guard_hr!("IMFAttributes::SetDouble", {
        typed_set(this, key, AttrValue::F64(v))
    })
}

extern "win64" fn mf_set_guid(this: *mut MfObj, key: *const Guid, v: *const Guid) -> HResult {
    guard_hr!("IMFAttributes::SetGUID", {
        let Some(g) = read_guid(v) else {
            return E_POINTER;
        };
        typed_set(this, key, AttrValue::Guid(g))
    })
}

extern "win64" fn mf_set_string(this: *mut MfObj, key: *const Guid, s: *const u16) -> HResult {
    guard_hr!("IMFAttributes::SetString", {
        // SAFETY: `s` NULL ou chaîne terminée par NUL (contrat SetString).
        let Some(w) = (unsafe { read_wstr(s) }) else {
            return E_POINTER;
        };
        typed_set(this, key, AttrValue::Str(w))
    })
}

extern "win64" fn mf_set_blob(
    this: *mut MfObj,
    key: *const Guid,
    p: *const u8,
    cb: u32,
) -> HResult {
    guard_hr!("IMFAttributes::SetBlob", {
        if p.is_null() && cb != 0 {
            return E_POINTER;
        }
        let b = if cb == 0 {
            Vec::new()
        } else {
            // SAFETY: `p` désigne `cb` octets lisibles.
            unsafe { core::slice::from_raw_parts(p, cb as usize) }.to_vec()
        };
        typed_set(this, key, AttrValue::Blob(b))
    })
}

extern "win64" fn mf_set_unknown(this: *mut MfObj, key: *const Guid, unk: *mut c_void) -> HResult {
    guard_hr!("IMFAttributes::SetUnknown", {
        if unk.is_null() {
            return E_INVALIDARG;
        }
        if obj(this).is_none() || key.is_null() {
            return E_INVALIDARG;
        }
        unk_add_ref(unk as usize);
        typed_set(this, key, AttrValue::Unknown(unk as usize))
    })
}

extern "win64" fn mf_lock_store(this: *mut MfObj) -> HResult {
    guard_hr!("IMFAttributes::LockStore", {
        let Some(o) = obj(this) else {
            return E_INVALIDARG;
        };
        o.store.lock_store();
        S_OK
    })
}

extern "win64" fn mf_unlock_store(this: *mut MfObj) -> HResult {
    guard_hr!("IMFAttributes::UnlockStore", {
        obj(this).map_or(E_INVALIDARG, |o| o.store.unlock_store())
    })
}

extern "win64" fn mf_get_count(this: *mut MfObj, out: *mut u32) -> HResult {
    guard_hr!("IMFAttributes::GetCount", {
        let Some(o) = obj(this) else {
            return E_INVALIDARG;
        };
        if out.is_null() {
            return E_POINTER;
        }
        let n = o.store.with(|v| v.len() as u32);
        // SAFETY: `out` vérifié non-nul.
        unsafe { *out = n };
        S_OK
    })
}

extern "win64" fn mf_get_item_by_index(
    this: *mut MfObj,
    index: u32,
    key: *mut Guid,
    pv: *mut PropVariant,
) -> HResult {
    guard_hr!("IMFAttributes::GetItemByIndex", {
        let Some(o) = obj(this) else {
            return E_INVALIDARG;
        };
        if key.is_null() {
            return E_POINTER;
        }
        let Some((k, v)) = o.store.with(|v| v.get(index as usize).cloned()) else {
            return E_INVALIDARG;
        };
        // SAFETY: `key` vérifié non-nul ; `pv` optionnel.
        unsafe {
            key.write_unaligned(k);
            if !pv.is_null() {
                pv.write_unaligned(v.owned_propvariant());
            }
        }
        S_OK
    })
}

type DeleteAllFn = extern "win64" fn(*mut c_void) -> HResult;
type SetItemFn = extern "win64" fn(*mut c_void, *const Guid, *const PropVariant) -> HResult;

extern "win64" fn mf_copy_all_items(this: *mut MfObj, dest: *mut c_void) -> HResult {
    guard_hr!("IMFAttributes::CopyAllItems", {
        let Some(o) = obj(this) else {
            return E_INVALIDARG;
        };
        if dest.is_null() {
            return E_POINTER;
        }
        let items = o.store.snapshot();
        // SAFETY: `dest` est un IMFAttributes (slot 20 = DeleteAllItems, 18 = SetItem).
        unsafe {
            let hr = vfn::<DeleteAllFn>(dest, 20)(dest);
            if hr < 0 {
                return hr;
            }
            for (k, v) in &items {
                let pv = v.borrowed_propvariant();
                let hr = vfn::<SetItemFn>(dest, 18)(dest, k, &pv);
                if hr < 0 {
                    return hr;
                }
            }
        }
        S_OK
    })
}

// ---------- IMFMediaType ----------

extern "win64" fn mt_get_major_type(this: *mut MfObj, out: *mut Guid) -> HResult {
    guard_hr!("IMFMediaType::GetMajorType", {
        typed_get(this, &MF_MT_MAJOR_TYPE, out, |v| match v {
            AttrValue::Guid(g) => Some(*g),
            _ => None,
        })
    })
}

extern "win64" fn mt_is_compressed(this: *mut MfObj, out: *mut WinBool) -> HResult {
    guard_hr!("IMFMediaType::IsCompressedFormat", {
        let Some(o) = obj(this) else {
            return E_INVALIDARG;
        };
        if out.is_null() {
            return E_POINTER;
        }
        // Équivalent MSDN : compressé sauf si MF_MT_ALL_SAMPLES_INDEPENDENT est TRUE.
        let independent = matches!(
            o.store.get(&MF_MT_ALL_SAMPLES_INDEPENDENT),
            Some(AttrValue::U32(v)) if v != 0
        );
        // SAFETY: `out` vérifié non-nul.
        unsafe { *out = if independent { WIN_FALSE } else { WIN_TRUE } };
        S_OK
    })
}

extern "win64" fn mt_is_equal(this: *mut MfObj, other: *mut c_void, flags: *mut u32) -> HResult {
    guard_hr!("IMFMediaType::IsEqual", {
        let Some(o) = obj(this) else {
            return E_INVALIDARG;
        };
        if flags.is_null() {
            return E_POINTER;
        }
        // SAFETY: `flags` vérifié non-nul.
        unsafe { *flags = 0 };
        let Some(t) = as_ours(other) else {
            // Type tiers : MSDN exige un IMFMediaType ; seuls les nôtres existent ici.
            return E_INVALIDARG;
        };
        let a = o.store.snapshot();
        let b = t.store.snapshot();
        let get = |v: &[(Guid, AttrValue)], k: &Guid| {
            v.iter().find(|(x, _)| x == k).map(|(_, y)| y.clone())
        };
        let mut f = 0;
        let ma = get(&a, &MF_MT_MAJOR_TYPE);
        if ma.is_some() && ma == get(&b, &MF_MT_MAJOR_TYPE) {
            f |= MF_MEDIATYPE_EQUAL_MAJOR_TYPES;
        }
        let sa = get(&a, &MF_MT_SUBTYPE);
        if sa.is_some() && sa == get(&b, &MF_MT_SUBTYPE) {
            f |= MF_MEDIATYPE_EQUAL_FORMAT_TYPES;
        }
        let strip = |v: &[(Guid, AttrValue)]| -> Vec<(Guid, AttrValue)> {
            let mut r: Vec<_> = v
                .iter()
                .filter(|(k, _)| *k != MF_MT_USER_DATA)
                .cloned()
                .collect();
            r.sort_by_key(|(k, _)| (k.data1, k.data2, k.data3, k.data4));
            r
        };
        if strip(&a) == strip(&b) {
            f |= MF_MEDIATYPE_EQUAL_FORMAT_DATA;
        }
        if get(&a, &MF_MT_USER_DATA) == get(&b, &MF_MT_USER_DATA) {
            f |= MF_MEDIATYPE_EQUAL_FORMAT_USER_DATA;
        }
        // SAFETY: `flags` vérifié non-nul.
        unsafe { *flags = f };
        if f == 0xF { S_OK } else { S_FALSE }
    })
}

extern "win64" fn mt_get_representation(
    _this: *mut MfObj,
    _guid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    guard_hr!("IMFMediaType::GetRepresentation", {
        if !out.is_null() {
            // SAFETY: `out` vérifié non-nul.
            unsafe { *out = core::ptr::null_mut() };
        }
        // Aucune conversion AM_MEDIA_TYPE/WAVEFORMATEX disponible (pas de format concret).
        MF_E_UNSUPPORTED_REPRESENTATION
    })
}

extern "win64" fn mt_free_representation(
    _this: *mut MfObj,
    _guid: *const Guid,
    _p: *mut c_void,
) -> HResult {
    guard_hr!("IMFMediaType::FreeRepresentation", {
        // Aucune représentation n'est jamais émise.
        E_INVALIDARG
    })
}

// ---------- IMFSample ----------

fn sample<'a>(this: *mut MfObj) -> Option<&'a MfObj> {
    obj(this).filter(|o| o.kind == Kind::Sample)
}

fn sample_out<T>(
    this: *mut MfObj,
    out: *mut T,
    f: impl FnOnce(&SampleData) -> Result<T, HResult>,
) -> HResult {
    let Some(o) = sample(this) else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        return E_POINTER;
    }
    match f(&o.sample.lock()) {
        Ok(v) => {
            // SAFETY: `out` vérifié non-nul.
            unsafe { out.write_unaligned(v) };
            S_OK
        }
        Err(hr) => hr,
    }
}

fn sample_mut(this: *mut MfObj, f: impl FnOnce(&mut SampleData) -> HResult) -> HResult {
    sample(this).map_or(E_INVALIDARG, |o| f(&mut o.sample.lock()))
}

extern "win64" fn smp_get_flags(this: *mut MfObj, out: *mut u32) -> HResult {
    guard_hr!("IMFSample::GetSampleFlags", {
        sample_out(this, out, |s| Ok(s.flags))
    })
}

extern "win64" fn smp_set_flags(this: *mut MfObj, v: u32) -> HResult {
    guard_hr!("IMFSample::SetSampleFlags", {
        sample_mut(this, |s| {
            s.flags = v;
            S_OK
        })
    })
}

extern "win64" fn smp_get_time(this: *mut MfObj, out: *mut i64) -> HResult {
    guard_hr!("IMFSample::GetSampleTime", {
        sample_out(this, out, |s| s.time.ok_or(MF_E_NO_SAMPLE_TIMESTAMP))
    })
}

extern "win64" fn smp_set_time(this: *mut MfObj, v: i64) -> HResult {
    guard_hr!("IMFSample::SetSampleTime", {
        sample_mut(this, |s| {
            s.time = Some(v);
            S_OK
        })
    })
}

extern "win64" fn smp_get_duration(this: *mut MfObj, out: *mut i64) -> HResult {
    guard_hr!("IMFSample::GetSampleDuration", {
        sample_out(this, out, |s| s.duration.ok_or(MF_E_NO_SAMPLE_DURATION))
    })
}

extern "win64" fn smp_set_duration(this: *mut MfObj, v: i64) -> HResult {
    guard_hr!("IMFSample::SetSampleDuration", {
        sample_mut(this, |s| {
            s.duration = Some(v);
            S_OK
        })
    })
}

extern "win64" fn smp_get_buffer_count(this: *mut MfObj, out: *mut u32) -> HResult {
    guard_hr!("IMFSample::GetBufferCount", {
        sample_out(this, out, |s| Ok(s.buffers.len() as u32))
    })
}

extern "win64" fn smp_get_buffer_by_index(
    this: *mut MfObj,
    i: u32,
    out: *mut *mut c_void,
) -> HResult {
    guard_hr!("IMFSample::GetBufferByIndex", {
        sample_out(this, out, |s| {
            let b = *s.buffers.get(i as usize).ok_or(E_INVALIDARG)?;
            unk_add_ref(b);
            Ok(b as *mut c_void)
        })
    })
}

type LockFn = extern "win64" fn(*mut c_void, *mut *mut u8, *mut u32, *mut u32) -> HResult;
type UnlockFn = extern "win64" fn(*mut c_void) -> HResult;
type GetLenFn = extern "win64" fn(*mut c_void, *mut u32) -> HResult;
type SetLenFn = extern "win64" fn(*mut c_void, u32) -> HResult;

fn buffer_len(b: usize) -> u32 {
    let mut n = 0;
    // SAFETY: IMFMediaBuffer valide (slot 5 = GetCurrentLength).
    unsafe { vfn::<GetLenFn>(b as *mut c_void, 5)(b as *mut c_void, &mut n) };
    n
}

/// Copie le contenu de `bufs` à la suite dans `dst` (capacité `cap`). Retourne la taille copiée.
fn gather(bufs: &[usize], dst: *mut u8, cap: usize) -> Result<usize, HResult> {
    let mut off = 0usize;
    for &b in bufs {
        let mut p = core::ptr::null_mut();
        let mut cur = 0u32;
        // SAFETY: IMFMediaBuffer valide (slot 3 = Lock, 4 = Unlock).
        unsafe {
            let hr = vfn::<LockFn>(b as *mut c_void, 3)(
                b as *mut c_void,
                &mut p,
                core::ptr::null_mut(),
                &mut cur,
            );
            if hr < 0 {
                return Err(hr);
            }
            let n = cur as usize;
            if off + n > cap {
                vfn::<UnlockFn>(b as *mut c_void, 4)(b as *mut c_void);
                return Err(MF_E_BUFFERTOOSMALL);
            }
            if n > 0 {
                core::ptr::copy_nonoverlapping(p, dst.add(off), n);
            }
            vfn::<UnlockFn>(b as *mut c_void, 4)(b as *mut c_void);
            off += n;
        }
    }
    Ok(off)
}

pub const MF_E_BUFFERTOOSMALL: HResult = 0xC00D_36B1_u32 as i32;

extern "win64" fn smp_convert_to_contiguous(this: *mut MfObj, out: *mut *mut c_void) -> HResult {
    guard_hr!("IMFSample::ConvertToContiguousBuffer", {
        let Some(o) = sample(this) else {
            return E_INVALIDARG;
        };
        let bufs = o.sample.lock().buffers.clone();
        if bufs.is_empty() {
            return E_UNEXPECTED;
        }
        let result = if bufs.len() == 1 {
            unk_add_ref(bufs[0]);
            bufs[0]
        } else {
            let total: usize = bufs.iter().map(|&b| buffer_len(b) as usize).sum();
            let nb = new_mem_buffer(total);
            // SAFETY: `nb` est notre buffer de `total` octets.
            let dst = unsafe { (*nb).data.lock().as_mut_ptr() };
            match gather(&bufs, dst, total) {
                Ok(n) => {
                    // SAFETY: `nb` vivant.
                    unsafe { (*nb).cur.store(n, Ordering::Release) };
                }
                Err(hr) => {
                    mb_release(nb);
                    return hr;
                }
            }
            // Le sample ne contient plus que le buffer contigu (comportement MSDN).
            let old = core::mem::replace(&mut o.sample.lock().buffers, vec![nb as usize]);
            for b in old {
                unk_release(b);
            }
            unk_add_ref(nb as usize);
            nb as usize
        };
        if !out.is_null() {
            // SAFETY: `out` vérifié non-nul.
            unsafe { *out = result as *mut c_void };
        } else {
            unk_release(result);
        }
        S_OK
    })
}

extern "win64" fn smp_add_buffer(this: *mut MfObj, b: *mut c_void) -> HResult {
    guard_hr!("IMFSample::AddBuffer", {
        if b.is_null() {
            return E_INVALIDARG;
        }
        sample_mut(this, |s| {
            unk_add_ref(b as usize);
            s.buffers.push(b as usize);
            S_OK
        })
    })
}

extern "win64" fn smp_remove_buffer_by_index(this: *mut MfObj, i: u32) -> HResult {
    guard_hr!("IMFSample::RemoveBufferByIndex", {
        let mut removed = 0;
        let hr = sample_mut(this, |s| {
            if (i as usize) < s.buffers.len() {
                removed = s.buffers.remove(i as usize);
                S_OK
            } else {
                E_INVALIDARG
            }
        });
        unk_release(removed);
        hr
    })
}

extern "win64" fn smp_remove_all(this: *mut MfObj) -> HResult {
    guard_hr!("IMFSample::RemoveAllBuffers", {
        let mut old = Vec::new();
        let hr = sample_mut(this, |s| {
            old = core::mem::take(&mut s.buffers);
            S_OK
        });
        for b in old {
            unk_release(b);
        }
        hr
    })
}

extern "win64" fn smp_get_total_length(this: *mut MfObj, out: *mut u32) -> HResult {
    guard_hr!("IMFSample::GetTotalLength", {
        let Some(o) = sample(this) else {
            return E_INVALIDARG;
        };
        if out.is_null() {
            return E_POINTER;
        }
        let bufs = o.sample.lock().buffers.clone();
        let total: u32 = bufs.iter().map(|&b| buffer_len(b)).sum();
        // SAFETY: `out` vérifié non-nul.
        unsafe { *out = total };
        S_OK
    })
}

extern "win64" fn smp_copy_to_buffer(this: *mut MfObj, dest: *mut c_void) -> HResult {
    guard_hr!("IMFSample::CopyToBuffer", {
        let Some(o) = sample(this) else {
            return E_INVALIDARG;
        };
        if dest.is_null() {
            return E_POINTER;
        }
        let bufs = o.sample.lock().buffers.clone();
        let mut p = core::ptr::null_mut();
        let mut max = 0u32;
        // SAFETY: IMFMediaBuffer destination (slots 3 = Lock, 4 = Unlock, 6 = SetCurrentLength).
        unsafe {
            let hr = vfn::<LockFn>(dest, 3)(dest, &mut p, &mut max, core::ptr::null_mut());
            if hr < 0 {
                return hr;
            }
            let r = gather(&bufs, p, max as usize);
            vfn::<UnlockFn>(dest, 4)(dest);
            match r {
                Ok(n) => vfn::<SetLenFn>(dest, 6)(dest, n as u32),
                Err(hr) => hr,
            }
        }
    })
}

// ---------- Vtables ----------

#[repr(C)]
struct AttrVtbl {
    query_interface: extern "win64" fn(*mut MfObj, *const Guid, *mut *mut c_void) -> HResult,
    add_ref: extern "win64" fn(*mut MfObj) -> u32,
    release: extern "win64" fn(*mut MfObj) -> u32,
    get_item: extern "win64" fn(*mut MfObj, *const Guid, *mut PropVariant) -> HResult,
    get_item_type: extern "win64" fn(*mut MfObj, *const Guid, *mut u32) -> HResult,
    compare_item:
        extern "win64" fn(*mut MfObj, *const Guid, *const PropVariant, *mut WinBool) -> HResult,
    compare: extern "win64" fn(*mut MfObj, *mut c_void, u32, *mut WinBool) -> HResult,
    get_u32: extern "win64" fn(*mut MfObj, *const Guid, *mut u32) -> HResult,
    get_u64: extern "win64" fn(*mut MfObj, *const Guid, *mut u64) -> HResult,
    get_double: extern "win64" fn(*mut MfObj, *const Guid, *mut f64) -> HResult,
    get_guid: extern "win64" fn(*mut MfObj, *const Guid, *mut Guid) -> HResult,
    get_string_length: extern "win64" fn(*mut MfObj, *const Guid, *mut u32) -> HResult,
    get_string: extern "win64" fn(*mut MfObj, *const Guid, *mut u16, u32, *mut u32) -> HResult,
    get_allocated_string:
        extern "win64" fn(*mut MfObj, *const Guid, *mut *mut u16, *mut u32) -> HResult,
    get_blob_size: extern "win64" fn(*mut MfObj, *const Guid, *mut u32) -> HResult,
    get_blob: extern "win64" fn(*mut MfObj, *const Guid, *mut u8, u32, *mut u32) -> HResult,
    get_allocated_blob:
        extern "win64" fn(*mut MfObj, *const Guid, *mut *mut u8, *mut u32) -> HResult,
    get_unknown:
        extern "win64" fn(*mut MfObj, *const Guid, *const Guid, *mut *mut c_void) -> HResult,
    set_item: extern "win64" fn(*mut MfObj, *const Guid, *const PropVariant) -> HResult,
    delete_item: extern "win64" fn(*mut MfObj, *const Guid) -> HResult,
    delete_all_items: extern "win64" fn(*mut MfObj) -> HResult,
    set_u32: extern "win64" fn(*mut MfObj, *const Guid, u32) -> HResult,
    set_u64: extern "win64" fn(*mut MfObj, *const Guid, u64) -> HResult,
    set_double: extern "win64" fn(*mut MfObj, *const Guid, f64) -> HResult,
    set_guid: extern "win64" fn(*mut MfObj, *const Guid, *const Guid) -> HResult,
    set_string: extern "win64" fn(*mut MfObj, *const Guid, *const u16) -> HResult,
    set_blob: extern "win64" fn(*mut MfObj, *const Guid, *const u8, u32) -> HResult,
    set_unknown: extern "win64" fn(*mut MfObj, *const Guid, *mut c_void) -> HResult,
    lock_store: extern "win64" fn(*mut MfObj) -> HResult,
    unlock_store: extern "win64" fn(*mut MfObj) -> HResult,
    get_count: extern "win64" fn(*mut MfObj, *mut u32) -> HResult,
    get_item_by_index: extern "win64" fn(*mut MfObj, u32, *mut Guid, *mut PropVariant) -> HResult,
    copy_all_items: extern "win64" fn(*mut MfObj, *mut c_void) -> HResult,
}

const ATTR_BASE: AttrVtbl = AttrVtbl {
    query_interface: mf_qi,
    add_ref: mf_add_ref,
    release: mf_release,
    get_item: mf_get_item,
    get_item_type: mf_get_item_type,
    compare_item: mf_compare_item,
    compare: mf_compare,
    get_u32: mf_get_u32,
    get_u64: mf_get_u64,
    get_double: mf_get_double,
    get_guid: mf_get_guid,
    get_string_length: mf_get_string_length,
    get_string: mf_get_string,
    get_allocated_string: mf_get_allocated_string,
    get_blob_size: mf_get_blob_size,
    get_blob: mf_get_blob,
    get_allocated_blob: mf_get_allocated_blob,
    get_unknown: mf_get_unknown,
    set_item: mf_set_item,
    delete_item: mf_delete_item,
    delete_all_items: mf_delete_all,
    set_u32: mf_set_u32,
    set_u64: mf_set_u64,
    set_double: mf_set_double,
    set_guid: mf_set_guid,
    set_string: mf_set_string,
    set_blob: mf_set_blob,
    set_unknown: mf_set_unknown,
    lock_store: mf_lock_store,
    unlock_store: mf_unlock_store,
    get_count: mf_get_count,
    get_item_by_index: mf_get_item_by_index,
    copy_all_items: mf_copy_all_items,
};

#[repr(C)]
struct MediaTypeVtbl {
    base: AttrVtbl,
    get_major_type: extern "win64" fn(*mut MfObj, *mut Guid) -> HResult,
    is_compressed_format: extern "win64" fn(*mut MfObj, *mut WinBool) -> HResult,
    is_equal: extern "win64" fn(*mut MfObj, *mut c_void, *mut u32) -> HResult,
    get_representation: extern "win64" fn(*mut MfObj, *const Guid, *mut *mut c_void) -> HResult,
    free_representation: extern "win64" fn(*mut MfObj, *const Guid, *mut c_void) -> HResult,
}

#[repr(C)]
struct SampleVtbl {
    base: AttrVtbl,
    get_sample_flags: extern "win64" fn(*mut MfObj, *mut u32) -> HResult,
    set_sample_flags: extern "win64" fn(*mut MfObj, u32) -> HResult,
    get_sample_time: extern "win64" fn(*mut MfObj, *mut i64) -> HResult,
    set_sample_time: extern "win64" fn(*mut MfObj, i64) -> HResult,
    get_sample_duration: extern "win64" fn(*mut MfObj, *mut i64) -> HResult,
    set_sample_duration: extern "win64" fn(*mut MfObj, i64) -> HResult,
    get_buffer_count: extern "win64" fn(*mut MfObj, *mut u32) -> HResult,
    get_buffer_by_index: extern "win64" fn(*mut MfObj, u32, *mut *mut c_void) -> HResult,
    convert_to_contiguous_buffer: extern "win64" fn(*mut MfObj, *mut *mut c_void) -> HResult,
    add_buffer: extern "win64" fn(*mut MfObj, *mut c_void) -> HResult,
    remove_buffer_by_index: extern "win64" fn(*mut MfObj, u32) -> HResult,
    remove_all_buffers: extern "win64" fn(*mut MfObj) -> HResult,
    get_total_length: extern "win64" fn(*mut MfObj, *mut u32) -> HResult,
    copy_to_buffer: extern "win64" fn(*mut MfObj, *mut c_void) -> HResult,
}

const _: () = {
    assert!(size_of::<AttrVtbl>() == 33 * 8);
    assert!(size_of::<MediaTypeVtbl>() == 38 * 8);
    assert!(size_of::<SampleVtbl>() == 47 * 8);
    assert!(offset_of!(AttrVtbl, set_item) == 18 * 8);
    assert!(offset_of!(AttrVtbl, get_count) == 30 * 8);
};

static ATTR_VTBL: AttrVtbl = ATTR_BASE;
static MEDIATYPE_VTBL: MediaTypeVtbl = MediaTypeVtbl {
    base: ATTR_BASE,
    get_major_type: mt_get_major_type,
    is_compressed_format: mt_is_compressed,
    is_equal: mt_is_equal,
    get_representation: mt_get_representation,
    free_representation: mt_free_representation,
};
static SAMPLE_VTBL: SampleVtbl = SampleVtbl {
    base: ATTR_BASE,
    get_sample_flags: smp_get_flags,
    set_sample_flags: smp_set_flags,
    get_sample_time: smp_get_time,
    set_sample_time: smp_set_time,
    get_sample_duration: smp_get_duration,
    set_sample_duration: smp_set_duration,
    get_buffer_count: smp_get_buffer_count,
    get_buffer_by_index: smp_get_buffer_by_index,
    convert_to_contiguous_buffer: smp_convert_to_contiguous,
    add_buffer: smp_add_buffer,
    remove_buffer_by_index: smp_remove_buffer_by_index,
    remove_all_buffers: smp_remove_all,
    get_total_length: smp_get_total_length,
    copy_to_buffer: smp_copy_to_buffer,
};

// ==================== IMFMediaBuffer (mémoire) ====================

#[repr(C)]
struct MemBuffer {
    vtbl: *const MemBufferVtbl,
    refs: AtomicU32,
    max: usize,
    cur: AtomicUsize,
    locks: AtomicU32,
    data: Mutex<Box<[u8]>>,
}

#[repr(C)]
struct MemBufferVtbl {
    query_interface: extern "win64" fn(*mut MemBuffer, *const Guid, *mut *mut c_void) -> HResult,
    add_ref: extern "win64" fn(*mut MemBuffer) -> u32,
    release: extern "win64" fn(*mut MemBuffer) -> u32,
    lock: extern "win64" fn(*mut MemBuffer, *mut *mut u8, *mut u32, *mut u32) -> HResult,
    unlock: extern "win64" fn(*mut MemBuffer) -> HResult,
    get_current_length: extern "win64" fn(*mut MemBuffer, *mut u32) -> HResult,
    set_current_length: extern "win64" fn(*mut MemBuffer, u32) -> HResult,
    get_max_length: extern "win64" fn(*mut MemBuffer, *mut u32) -> HResult,
}

static MEMBUFFER_VTBL: MemBufferVtbl = MemBufferVtbl {
    query_interface: mb_qi,
    add_ref: mb_add_ref,
    release: mb_release,
    lock: mb_lock,
    unlock: mb_unlock,
    get_current_length: mb_get_cur,
    set_current_length: mb_set_cur,
    get_max_length: mb_get_max,
};

fn new_mem_buffer(max: usize) -> *mut MemBuffer {
    LIVE_OBJECTS.fetch_add(1, Ordering::Relaxed);
    Box::into_raw(Box::new(MemBuffer {
        vtbl: &raw const MEMBUFFER_VTBL,
        refs: AtomicU32::new(1),
        max,
        cur: AtomicUsize::new(0),
        locks: AtomicU32::new(0),
        data: Mutex::new(vec![0u8; max.max(1)].into_boxed_slice()),
    }))
}

fn mb<'a>(this: *mut MemBuffer) -> Option<&'a MemBuffer> {
    // SAFETY: `this` est notre MemBuffer (premier argument COM).
    (!this.is_null()).then(|| unsafe { &*this })
}

extern "win64" fn mb_qi(this: *mut MemBuffer, riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    guard_hr!("IMFMediaBuffer::QueryInterface", {
        if ppv.is_null() {
            return E_POINTER;
        }
        // SAFETY: `ppv` vérifié non-nul.
        unsafe { *ppv = core::ptr::null_mut() };
        let (Some(b), Some(iid)) = (mb(this), read_guid(riid)) else {
            return E_INVALIDARG;
        };
        if iid != IID_IUNKNOWN && iid != IID_IMFMEDIABUFFER {
            return E_NOINTERFACE;
        }
        b.refs.fetch_add(1, Ordering::AcqRel);
        // SAFETY: `ppv` vérifié non-nul.
        unsafe { *ppv = this.cast() };
        S_OK
    })
}

extern "win64" fn mb_add_ref(this: *mut MemBuffer) -> u32 {
    ffi_guard!(CHANNEL, "IMFMediaBuffer::AddRef", 0, {
        mb(this).map_or(0, |b| b.refs.fetch_add(1, Ordering::AcqRel) + 1)
    })
}

extern "win64" fn mb_release(this: *mut MemBuffer) -> u32 {
    ffi_guard!(CHANNEL, "IMFMediaBuffer::Release", 0, {
        let Some(b) = mb(this) else { return 0 };
        let left = b.refs.fetch_sub(1, Ordering::AcqRel).saturating_sub(1);
        if left == 0 {
            LIVE_OBJECTS.fetch_sub(1, Ordering::Relaxed);
            // SAFETY: dernier référent ; alloué par `Box::into_raw`.
            drop(unsafe { Box::from_raw(this) });
        }
        left
    })
}

extern "win64" fn mb_lock(
    this: *mut MemBuffer,
    pp: *mut *mut u8,
    max: *mut u32,
    cur: *mut u32,
) -> HResult {
    guard_hr!("IMFMediaBuffer::Lock", {
        let Some(b) = mb(this) else {
            return E_INVALIDARG;
        };
        if pp.is_null() {
            return E_POINTER;
        }
        b.locks.fetch_add(1, Ordering::AcqRel);
        // SAFETY: `pp` vérifié non-nul ; sorties optionnelles ; le tampon ne se réalloue jamais.
        unsafe {
            *pp = b.data.lock().as_mut_ptr();
            if !max.is_null() {
                *max = b.max as u32;
            }
            if !cur.is_null() {
                *cur = b.cur.load(Ordering::Acquire) as u32;
            }
        }
        S_OK
    })
}

extern "win64" fn mb_unlock(this: *mut MemBuffer) -> HResult {
    guard_hr!("IMFMediaBuffer::Unlock", {
        let Some(b) = mb(this) else {
            return E_INVALIDARG;
        };
        match b
            .locks
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
        {
            Ok(_) => S_OK,
            Err(_) => E_UNEXPECTED,
        }
    })
}

extern "win64" fn mb_get_cur(this: *mut MemBuffer, out: *mut u32) -> HResult {
    guard_hr!("IMFMediaBuffer::GetCurrentLength", {
        let Some(b) = mb(this) else {
            return E_INVALIDARG;
        };
        if out.is_null() {
            return E_POINTER;
        }
        // SAFETY: `out` vérifié non-nul.
        unsafe { *out = b.cur.load(Ordering::Acquire) as u32 };
        S_OK
    })
}

extern "win64" fn mb_set_cur(this: *mut MemBuffer, n: u32) -> HResult {
    guard_hr!("IMFMediaBuffer::SetCurrentLength", {
        let Some(b) = mb(this) else {
            return E_INVALIDARG;
        };
        if n as usize > b.max {
            return E_INVALIDARG;
        }
        b.cur.store(n as usize, Ordering::Release);
        S_OK
    })
}

extern "win64" fn mb_get_max(this: *mut MemBuffer, out: *mut u32) -> HResult {
    guard_hr!("IMFMediaBuffer::GetMaxLength", {
        let Some(b) = mb(this) else {
            return E_INVALIDARG;
        };
        if out.is_null() {
            return E_POINTER;
        }
        // SAFETY: `out` vérifié non-nul.
        unsafe { *out = b.max as u32 };
        S_OK
    })
}

// ==================== IMFSourceResolver ====================

#[repr(C)]
struct Resolver {
    vtbl: *const ResolverVtbl,
    refs: AtomicU32,
}

#[repr(C)]
struct ResolverVtbl {
    query_interface: extern "win64" fn(*mut Resolver, *const Guid, *mut *mut c_void) -> HResult,
    add_ref: extern "win64" fn(*mut Resolver) -> u32,
    release: extern "win64" fn(*mut Resolver) -> u32,
    create_object_from_url: extern "win64" fn(
        *mut Resolver,
        *const u16,
        u32,
        *mut c_void,
        *mut u32,
        *mut *mut c_void,
    ) -> HResult,
    create_object_from_byte_stream: extern "win64" fn(
        *mut Resolver,
        *mut c_void,
        *const u16,
        u32,
        *mut c_void,
        *mut u32,
        *mut *mut c_void,
    ) -> HResult,
    begin_create_object_from_url: extern "win64" fn(
        *mut Resolver,
        *const u16,
        u32,
        *mut c_void,
        *mut *mut c_void,
        *mut c_void,
        *mut c_void,
    ) -> HResult,
    end_create_object_from_url:
        extern "win64" fn(*mut Resolver, *mut c_void, *mut u32, *mut *mut c_void) -> HResult,
    begin_create_object_from_byte_stream: extern "win64" fn(
        *mut Resolver,
        *mut c_void,
        *const u16,
        u32,
        *mut c_void,
        *mut *mut c_void,
        *mut c_void,
        *mut c_void,
    ) -> HResult,
    end_create_object_from_byte_stream:
        extern "win64" fn(*mut Resolver, *mut c_void, *mut u32, *mut *mut c_void) -> HResult,
    cancel_object_creation: extern "win64" fn(*mut Resolver, *mut c_void) -> HResult,
}

static RESOLVER_VTBL: ResolverVtbl = ResolverVtbl {
    query_interface: sr_qi,
    add_ref: sr_add_ref,
    release: sr_release,
    create_object_from_url: sr_create_from_url,
    create_object_from_byte_stream: sr_create_from_bs,
    begin_create_object_from_url: sr_begin_url,
    end_create_object_from_url: sr_end,
    begin_create_object_from_byte_stream: sr_begin_bs,
    end_create_object_from_byte_stream: sr_end,
    cancel_object_creation: sr_cancel,
};

extern "win64" fn sr_qi(this: *mut Resolver, riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    guard_hr!("IMFSourceResolver::QueryInterface", {
        if ppv.is_null() {
            return E_POINTER;
        }
        // SAFETY: `ppv` vérifié non-nul.
        unsafe { *ppv = core::ptr::null_mut() };
        let Some(iid) = read_guid(riid) else {
            return E_INVALIDARG;
        };
        if this.is_null() || (iid != IID_IUNKNOWN && iid != IID_IMFSOURCERESOLVER) {
            return E_NOINTERFACE;
        }
        sr_add_ref(this);
        // SAFETY: `ppv` vérifié non-nul.
        unsafe { *ppv = this.cast() };
        S_OK
    })
}

extern "win64" fn sr_add_ref(this: *mut Resolver) -> u32 {
    if this.is_null() {
        return 0;
    }
    // SAFETY: `this` est notre résolveur.
    unsafe { (*this).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

extern "win64" fn sr_release(this: *mut Resolver) -> u32 {
    if this.is_null() {
        return 0;
    }
    // SAFETY: `this` est notre résolveur alloué par `Box`.
    unsafe {
        let left = (*this)
            .refs
            .fetch_sub(1, Ordering::AcqRel)
            .saturating_sub(1);
        if left == 0 {
            LIVE_OBJECTS.fetch_sub(1, Ordering::Relaxed);
            drop(Box::from_raw(this));
        }
        left
    }
}

fn clear_outputs(obj_type: *mut u32, out: *mut *mut c_void) {
    // SAFETY: pointeurs de sortie optionnels vérifiés.
    unsafe {
        if !obj_type.is_null() {
            *obj_type = MF_OBJECT_INVALID;
        }
        if !out.is_null() {
            *out = core::ptr::null_mut();
        }
    }
}

/// Diagnostic d'une URL : schéma non géré, fichier absent du VFS, ou type de flux non supporté.
fn url_failure(url: *const u16) -> HResult {
    if url.is_null() {
        return E_INVALIDARG;
    }
    if STARTUP_COUNT.load(Ordering::Acquire) == 0 {
        return MF_E_PLATFORM_NOT_INITIALIZED;
    }
    // SAFETY: `url` chaîne UTF-16 terminée par NUL (contrat MSDN).
    let Some(s) = (unsafe { sb_common::wide::from_pcwstr(url) }) else {
        return E_INVALIDARG;
    };
    let lower = s.to_ascii_lowercase();
    let path = if let Some(rest) = lower.strip_prefix("file:///") {
        &s[s.len() - rest.len()..]
    } else if let Some(rest) = lower.strip_prefix("file://") {
        &s[s.len() - rest.len()..]
    } else if lower.contains("://") {
        // http(s), rtsp… : réseau indisponible (mode offline) et aucun handler de schéma.
        return MF_E_UNSUPPORTED_SCHEME;
    } else {
        s.as_str()
    };
    match super::resolve_guest_path(path) {
        Some(p) if p.is_file() => MF_E_UNSUPPORTED_BYTESTREAM_TYPE,
        _ => E_FILE_NOT_FOUND,
    }
}

extern "win64" fn sr_create_from_url(
    _this: *mut Resolver,
    url: *const u16,
    _flags: u32,
    _props: *mut c_void,
    obj_type: *mut u32,
    out: *mut *mut c_void,
) -> HResult {
    guard_hr!("IMFSourceResolver::CreateObjectFromURL", {
        clear_outputs(obj_type, out);
        if obj_type.is_null() || out.is_null() {
            return E_POINTER;
        }
        url_failure(url)
    })
}

extern "win64" fn sr_create_from_bs(
    _this: *mut Resolver,
    bs: *mut c_void,
    _url: *const u16,
    _flags: u32,
    _props: *mut c_void,
    obj_type: *mut u32,
    out: *mut *mut c_void,
) -> HResult {
    guard_hr!("IMFSourceResolver::CreateObjectFromByteStream", {
        clear_outputs(obj_type, out);
        if bs.is_null() {
            return E_INVALIDARG;
        }
        if obj_type.is_null() || out.is_null() {
            return E_POINTER;
        }
        MF_E_UNSUPPORTED_BYTESTREAM_TYPE
    })
}

extern "win64" fn sr_begin_url(
    _this: *mut Resolver,
    url: *const u16,
    _flags: u32,
    _props: *mut c_void,
    cancel: *mut *mut c_void,
    _cb: *mut c_void,
    _state: *mut c_void,
) -> HResult {
    guard_hr!("IMFSourceResolver::BeginCreateObjectFromURL", {
        if !cancel.is_null() {
            // SAFETY: vérifié non-nul.
            unsafe { *cancel = core::ptr::null_mut() };
        }
        // Échec synchrone : aucune opération asynchrone démarrée, callback jamais invoqué.
        url_failure(url)
    })
}

extern "win64" fn sr_begin_bs(
    _this: *mut Resolver,
    bs: *mut c_void,
    _url: *const u16,
    _flags: u32,
    _props: *mut c_void,
    cancel: *mut *mut c_void,
    _cb: *mut c_void,
    _state: *mut c_void,
) -> HResult {
    guard_hr!("IMFSourceResolver::BeginCreateObjectFromByteStream", {
        if !cancel.is_null() {
            // SAFETY: vérifié non-nul.
            unsafe { *cancel = core::ptr::null_mut() };
        }
        if bs.is_null() {
            E_INVALIDARG
        } else {
            MF_E_UNSUPPORTED_BYTESTREAM_TYPE
        }
    })
}

extern "win64" fn sr_end(
    _this: *mut Resolver,
    _result: *mut c_void,
    obj_type: *mut u32,
    out: *mut *mut c_void,
) -> HResult {
    guard_hr!("IMFSourceResolver::End*", {
        clear_outputs(obj_type, out);
        // Aucun Begin* n'aboutit : tout IMFAsyncResult présenté est invalide pour nous.
        E_INVALIDARG
    })
}

extern "win64" fn sr_cancel(_this: *mut Resolver, _cookie: *mut c_void) -> HResult {
    guard_hr!("IMFSourceResolver::CancelObjectCreation", { E_INVALIDARG })
}

// ==================== Exports mfplat ====================

static STARTUP_COUNT: AtomicU32 = AtomicU32::new(0);

/// `HRESULT MFStartup(ULONG Version, DWORD dwFlags)`.
pub(crate) extern "win64" fn MFStartup(version: u32, _flags: u32) -> HResult {
    guard_hr!("MFStartup", {
        // MF_VERSION = (MF_SDK_VERSION << 16) | MF_API_VERSION ; SDK 1 (Vista) ou 2 (Win7+).
        let sdk = version >> 16;
        if version & 0xFFFF != MF_API_VERSION || !(1..=2).contains(&sdk) {
            return MF_E_BAD_STARTUP_VERSION;
        }
        STARTUP_COUNT.fetch_add(1, Ordering::AcqRel);
        S_OK
    })
}

/// `HRESULT MFShutdown(void)`.
pub(crate) extern "win64" fn MFShutdown() -> HResult {
    guard_hr!("MFShutdown", {
        let _ = STARTUP_COUNT.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
        S_OK
    })
}

/// `MFTIME MFGetSystemTime(void)` — horloge monotone en unités de 100 ns.
pub(crate) extern "win64" fn MFGetSystemTime() -> i64 {
    ffi_guard!(CHANNEL, "MFGetSystemTime", 0, {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` est un timespec valide en écriture.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        ts.tv_sec * 10_000_000 + ts.tv_nsec / 100
    })
}

fn out_new(out: *mut *mut c_void, p: *mut c_void) -> HResult {
    // SAFETY: `out` vérifié non-nul par l'appelant.
    unsafe { *out = p };
    S_OK
}

/// `HRESULT MFCreateAttributes(IMFAttributes** ppMFAttributes, UINT32 cInitialSize)`.
pub(crate) extern "win64" fn MFCreateAttributes(out: *mut *mut c_void, _initial: u32) -> HResult {
    guard_hr!("MFCreateAttributes", {
        if out.is_null() {
            return E_POINTER;
        }
        out_new(out, new_obj(Kind::Attributes).cast())
    })
}

/// `HRESULT MFCreateMediaType(IMFMediaType** ppMFType)`.
pub(crate) extern "win64" fn MFCreateMediaType(out: *mut *mut c_void) -> HResult {
    guard_hr!("MFCreateMediaType", {
        if out.is_null() {
            return E_POINTER;
        }
        out_new(out, new_obj(Kind::MediaType).cast())
    })
}

/// `HRESULT MFCreateSample(IMFSample** ppIMFSample)`.
pub(crate) extern "win64" fn MFCreateSample(out: *mut *mut c_void) -> HResult {
    guard_hr!("MFCreateSample", {
        if out.is_null() {
            return E_POINTER;
        }
        out_new(out, new_obj(Kind::Sample).cast())
    })
}

/// `HRESULT MFCreateMemoryBuffer(DWORD cbMaxLength, IMFMediaBuffer** ppBuffer)`.
pub(crate) extern "win64" fn MFCreateMemoryBuffer(max: u32, out: *mut *mut c_void) -> HResult {
    guard_hr!("MFCreateMemoryBuffer", {
        if out.is_null() {
            return E_POINTER;
        }
        if max > 0x4000_0000 {
            // SAFETY: `out` vérifié non-nul.
            unsafe { *out = core::ptr::null_mut() };
            return E_OUTOFMEMORY;
        }
        out_new(out, new_mem_buffer(max as usize).cast())
    })
}

/// `HRESULT MFCreateSourceResolver(IMFSourceResolver** ppISourceResolver)`.
pub(crate) extern "win64" fn MFCreateSourceResolver(out: *mut *mut c_void) -> HResult {
    guard_hr!("MFCreateSourceResolver", {
        if out.is_null() {
            return E_POINTER;
        }
        LIVE_OBJECTS.fetch_add(1, Ordering::Relaxed);
        let r = Box::into_raw(Box::new(Resolver {
            vtbl: &raw const RESOLVER_VTBL,
            refs: AtomicU32::new(1),
        }));
        out_new(out, r.cast())
    })
}

pub(crate) fn mfplat_exports() -> std::collections::HashMap<&'static str, usize> {
    let mut m: std::collections::HashMap<&'static str, usize> = std::collections::HashMap::new();
    m.insert("MFStartup", MFStartup as *const () as usize);
    m.insert("MFShutdown", MFShutdown as *const () as usize);
    m.insert("MFGetSystemTime", MFGetSystemTime as *const () as usize);
    m.insert(
        "MFCreateAttributes",
        MFCreateAttributes as *const () as usize,
    );
    m.insert("MFCreateMediaType", MFCreateMediaType as *const () as usize);
    m.insert("MFCreateSample", MFCreateSample as *const () as usize);
    m.insert(
        "MFCreateMemoryBuffer",
        MFCreateMemoryBuffer as *const () as usize,
    );
    m.insert(
        "MFCreateSourceResolver",
        MFCreateSourceResolver as *const () as usize,
    );
    m
}

// ==================== Exports mfreadwrite ====================

fn clear_reader(out: *mut *mut c_void) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: vérifié non-nul.
    unsafe { *out = core::ptr::null_mut() };
    true
}

/// `HRESULT MFCreateSourceReaderFromURL(LPCWSTR, IMFAttributes*, IMFSourceReader**)`.
pub(crate) extern "win64" fn MFCreateSourceReaderFromURL(
    url: *const u16,
    _attrs: *mut c_void,
    out: *mut *mut c_void,
) -> HResult {
    guard_hr!("MFCreateSourceReaderFromURL", {
        if !clear_reader(out) {
            return E_POINTER;
        }
        url_failure(url)
    })
}

/// `HRESULT MFCreateSourceReaderFromMediaSource(IMFMediaSource*, IMFAttributes*, IMFSourceReader**)`.
/// Aucune IMFMediaSource ne peut provenir de ce MF (résolveur sans décodeur) : toute source
/// tierce est rejetée comme flux non supporté.
pub(crate) extern "win64" fn MFCreateSourceReaderFromMediaSource(
    source: *mut c_void,
    _attrs: *mut c_void,
    out: *mut *mut c_void,
) -> HResult {
    guard_hr!("MFCreateSourceReaderFromMediaSource", {
        if !clear_reader(out) {
            return E_POINTER;
        }
        if source.is_null() {
            return E_INVALIDARG;
        }
        MF_E_UNSUPPORTED_BYTESTREAM_TYPE
    })
}

/// `HRESULT MFCreateSourceReaderFromByteStream(IMFByteStream*, IMFAttributes*, IMFSourceReader**)`.
pub(crate) extern "win64" fn MFCreateSourceReaderFromByteStream(
    bs: *mut c_void,
    _attrs: *mut c_void,
    out: *mut *mut c_void,
) -> HResult {
    guard_hr!("MFCreateSourceReaderFromByteStream", {
        if !clear_reader(out) {
            return E_POINTER;
        }
        if bs.is_null() {
            return E_INVALIDARG;
        }
        MF_E_UNSUPPORTED_BYTESTREAM_TYPE
    })
}

pub(crate) fn mfreadwrite_exports() -> std::collections::HashMap<&'static str, usize> {
    let mut m: std::collections::HashMap<&'static str, usize> = std::collections::HashMap::new();
    m.insert(
        "MFCreateSourceReaderFromURL",
        MFCreateSourceReaderFromURL as *const () as usize,
    );
    m.insert(
        "MFCreateSourceReaderFromMediaSource",
        MFCreateSourceReaderFromMediaSource as *const () as usize,
    );
    m.insert(
        "MFCreateSourceReaderFromByteStream",
        MFCreateSourceReaderFromByteStream as *const () as usize,
    );
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_common::wide::to_wide_null;

    const KEY_A: Guid = Guid::new(1, 2, 3, [4, 5, 6, 7, 8, 9, 10, 11]);
    const KEY_B: Guid = Guid::new(9, 2, 3, [4, 5, 6, 7, 8, 9, 10, 11]);
    const KEY_C: Guid = Guid::new(7, 2, 3, [4, 5, 6, 7, 8, 9, 10, 11]);
    const MAJOR_VIDEO: Guid = Guid::new(
        0x7364_6976,
        0x0000,
        0x0010,
        [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
    );

    fn vt<'a>(p: *mut c_void) -> &'a AttrVtbl {
        // SAFETY: objet créé par nos fabriques ; vtable commençant par AttrVtbl.
        unsafe { &**p.cast::<*const AttrVtbl>() }
    }

    #[test]
    fn startup_version_check_and_system_time() {
        assert_eq!(MFStartup(0x0003_0070, 0), MF_E_BAD_STARTUP_VERSION);
        assert_eq!(MFStartup(0x0002_0071, 0), MF_E_BAD_STARTUP_VERSION);
        assert_eq!(MFStartup(MF_VERSION, 0), S_OK);
        let t1 = MFGetSystemTime();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let t2 = MFGetSystemTime();
        assert!(t2 - t1 >= 10_000, "{t1} -> {t2}");
        assert_eq!(MFShutdown(), S_OK);
    }

    #[test]
    fn attributes_set_get_types_and_refcount() {
        let mut p = core::ptr::null_mut();
        assert_eq!(MFCreateAttributes(&mut p, 4), S_OK);
        let a = p.cast::<MfObj>();
        let v = vt(p);
        assert_eq!((v.set_u32)(a, &KEY_A, 42), S_OK);
        assert_eq!((v.set_u64)(a, &KEY_B, 1 << 40), S_OK);
        let w = to_wide_null("bonjour");
        assert_eq!((v.set_string)(a, &KEY_C, w.as_ptr()), S_OK);
        let mut u = 0u32;
        assert_eq!((v.get_u32)(a, &KEY_A, &mut u), S_OK);
        assert_eq!(u, 42);
        let mut u64v = 0u64;
        assert_eq!((v.get_u64)(a, &KEY_A, &mut u64v), MF_E_INVALIDTYPE);
        assert_eq!((v.get_u64)(a, &KEY_B, &mut u64v), S_OK);
        assert_eq!(u64v, 1 << 40);
        let mut d = 0.0;
        assert_eq!(
            (v.get_double)(a, &MF_MT_SUBTYPE, &mut d),
            MF_E_ATTRIBUTENOTFOUND
        );
        let mut buf = [0u16; 4];
        let mut len = 0;
        assert_eq!(
            (v.get_string)(a, &KEY_C, buf.as_mut_ptr(), 4, &mut len),
            E_NOT_SUFFICIENT_BUFFER
        );
        let mut buf = [0u16; 16];
        assert_eq!(
            (v.get_string)(a, &KEY_C, buf.as_mut_ptr(), 16, &mut len),
            S_OK
        );
        assert_eq!(len, 7);
        assert_eq!(&buf[..8], &w[..]);
        let mut alloc = core::ptr::null_mut();
        assert_eq!(
            (v.get_allocated_string)(a, &KEY_C, &mut alloc, &mut len),
            S_OK
        );
        // SAFETY: chaîne allouée par malloc et terminée par NUL.
        unsafe {
            assert_eq!(*alloc.add(7), 0);
            libc::free(alloc.cast());
        }
        // Blob, GUID, double
        assert_eq!(
            (v.set_blob)(a, &MF_MT_USER_DATA, [1u8, 2, 3].as_ptr(), 3),
            S_OK
        );
        let mut bsz = 0;
        assert_eq!((v.get_blob_size)(a, &MF_MT_USER_DATA, &mut bsz), S_OK);
        assert_eq!(bsz, 3);
        let mut bb = [0u8; 3];
        assert_eq!(
            (v.get_blob)(a, &MF_MT_USER_DATA, bb.as_mut_ptr(), 3, &mut bsz),
            S_OK
        );
        assert_eq!(bb, [1, 2, 3]);
        assert_eq!((v.set_guid)(a, &MF_MT_MAJOR_TYPE, &MAJOR_VIDEO), S_OK);
        let mut g = Guid::new(0, 0, 0, [0; 8]);
        assert_eq!((v.get_guid)(a, &MF_MT_MAJOR_TYPE, &mut g), S_OK);
        assert_eq!(g, MAJOR_VIDEO);
        assert_eq!((v.set_double)(a, &MF_MT_SUBTYPE, 2.5), S_OK);
        assert_eq!((v.get_double)(a, &MF_MT_SUBTYPE, &mut d), S_OK);
        assert_eq!(d, 2.5);
        let mut ty = 0;
        assert_eq!((v.get_item_type)(a, &MF_MT_SUBTYPE, &mut ty), S_OK);
        assert_eq!(ty, u32::from(VT_R8));

        // Count / GetItemByIndex / DeleteItem
        let mut n = 0;
        assert_eq!((v.get_count)(a, &mut n), S_OK);
        assert_eq!(n, 6);
        let mut k = Guid::new(0, 0, 0, [0; 8]);
        let mut pv = PropVariant::default();
        assert_eq!((v.get_item_by_index)(a, 0, &mut k, &mut pv), S_OK);
        assert_eq!((k, pv.vt, pv.data[0]), (KEY_A, VT_UI4, 42));
        assert_eq!((v.get_item_by_index)(a, 99, &mut k, &mut pv), E_INVALIDARG);
        assert_eq!((v.delete_item)(a, &KEY_B), S_OK);
        assert_eq!((v.get_count)(a, &mut n), S_OK);
        assert_eq!(n, 5);

        // IUnknown stocké : refcount de l'objet référencé.
        let mut inner = core::ptr::null_mut();
        assert_eq!(MFCreateAttributes(&mut inner, 0), S_OK);
        assert_eq!((v.set_unknown)(a, &KEY_B, inner), S_OK);
        let mut q = core::ptr::null_mut();
        assert_eq!((v.get_unknown)(a, &KEY_B, &IID_IMFATTRIBUTES, &mut q), S_OK);
        assert_eq!(q, inner);
        let iv = vt(inner);
        assert_eq!((iv.release)(inner.cast()), 2); // q
        assert_eq!((iv.release)(inner.cast()), 1); // référence initiale ; reste celle du store

        // CopyAllItems + Compare
        let mut c = core::ptr::null_mut();
        assert_eq!(MFCreateAttributes(&mut c, 0), S_OK);
        assert_eq!((v.copy_all_items)(a, c), S_OK);
        let mut eq = WIN_FALSE;
        assert_eq!((v.compare)(a, c, 2, &mut eq), S_OK);
        assert_eq!(eq, WIN_TRUE);
        assert_eq!((vt(c).set_u32)(c.cast(), &KEY_A, 1), S_OK);
        assert_eq!((v.compare)(a, c, 2, &mut eq), S_OK);
        assert_eq!(eq, WIN_FALSE);

        // QueryInterface / Lock
        let mut qi = core::ptr::null_mut();
        assert_eq!(
            (v.query_interface)(a, &IID_IMFMEDIATYPE, &mut qi),
            E_NOINTERFACE
        );
        assert!(qi.is_null());
        assert_eq!((v.query_interface)(a, &IID_IUNKNOWN, &mut qi), S_OK);
        assert_eq!((v.release)(a), 1);
        assert_eq!((v.lock_store)(a), S_OK);
        assert_eq!((v.lock_store)(a), S_OK);
        assert_eq!((v.get_count)(a, &mut n), S_OK);
        assert_eq!((v.unlock_store)(a), S_OK);
        assert_eq!((v.unlock_store)(a), S_OK);
        assert_eq!((v.unlock_store)(a), E_UNEXPECTED);

        // Destruction complète : `c` puis `a` libèrent leurs références sur `inner` (libéré).
        assert_eq!((vt(c).release)(c.cast()), 0);
        assert_eq!((v.release)(a), 0);
    }

    #[test]
    fn media_type_major_compressed_equal() {
        let (mut a, mut b) = (core::ptr::null_mut(), core::ptr::null_mut());
        assert_eq!(MFCreateMediaType(&mut a), S_OK);
        assert_eq!(MFCreateMediaType(&mut b), S_OK);
        // SAFETY: vtables IMFMediaType.
        let (va, vb) = unsafe {
            (
                &**a.cast::<*const MediaTypeVtbl>(),
                &**b.cast::<*const MediaTypeVtbl>(),
            )
        };
        let mut g = Guid::new(0, 0, 0, [0; 8]);
        assert_eq!(
            (va.get_major_type)(a.cast(), &mut g),
            MF_E_ATTRIBUTENOTFOUND
        );
        for (o, v) in [(a, va), (b, vb)] {
            assert_eq!(
                (v.base.set_guid)(o.cast(), &MF_MT_MAJOR_TYPE, &MAJOR_VIDEO),
                S_OK
            );
            assert_eq!((v.base.set_guid)(o.cast(), &MF_MT_SUBTYPE, &KEY_A), S_OK);
        }
        assert_eq!((va.get_major_type)(a.cast(), &mut g), S_OK);
        assert_eq!(g, MAJOR_VIDEO);
        let mut c = WIN_FALSE;
        assert_eq!((va.is_compressed_format)(a.cast(), &mut c), S_OK);
        assert_eq!(c, WIN_TRUE);
        assert_eq!(
            (va.base.set_u32)(a.cast(), &MF_MT_ALL_SAMPLES_INDEPENDENT, 1),
            S_OK
        );
        assert_eq!((va.is_compressed_format)(a.cast(), &mut c), S_OK);
        assert_eq!(c, WIN_FALSE);
        let mut f = 0;
        assert_eq!((va.is_equal)(a.cast(), b, &mut f), S_FALSE);
        assert_eq!(f, 0xB);
        assert_eq!(
            (vb.base.set_u32)(b.cast(), &MF_MT_ALL_SAMPLES_INDEPENDENT, 1),
            S_OK
        );
        assert_eq!((va.is_equal)(a.cast(), b, &mut f), S_OK);
        assert_eq!(f, 0xF);
        let mut qi = core::ptr::null_mut();
        assert_eq!(
            (va.base.query_interface)(a.cast(), &IID_IMFMEDIATYPE, &mut qi),
            S_OK
        );
        assert_eq!((va.base.release)(a.cast()), 1);
        let mut rep = core::ptr::null_mut();
        assert_eq!(
            (va.get_representation)(a.cast(), &KEY_A, &mut rep),
            MF_E_UNSUPPORTED_REPRESENTATION
        );
        assert_eq!((va.base.release)(a.cast()), 0);
        assert_eq!((vb.base.release)(b.cast()), 0);
    }

    #[test]
    fn sample_and_memory_buffers() {
        let mut s = core::ptr::null_mut();
        assert_eq!(MFCreateSample(&mut s), S_OK);
        // SAFETY: vtable IMFSample.
        let sv = unsafe { &**s.cast::<*const SampleVtbl>() };
        let so = s.cast::<MfObj>();
        let mut t = 0i64;
        assert_eq!((sv.get_sample_time)(so, &mut t), MF_E_NO_SAMPLE_TIMESTAMP);
        assert_eq!((sv.set_sample_time)(so, 1234), S_OK);
        assert_eq!((sv.get_sample_time)(so, &mut t), S_OK);
        assert_eq!(t, 1234);
        let mut bufs = [core::ptr::null_mut(); 2];
        for (i, b) in bufs.iter_mut().enumerate() {
            assert_eq!(MFCreateMemoryBuffer(8, b), S_OK);
            let mb = b.cast::<MemBuffer>();
            let mut p = core::ptr::null_mut();
            let mut max = 0;
            assert_eq!(mb_lock(mb, &mut p, &mut max, core::ptr::null_mut()), S_OK);
            assert_eq!(max, 8);
            // SAFETY: tampon de 8 octets verrouillé.
            unsafe { core::ptr::write_bytes(p, i as u8 + 1, 4) };
            assert_eq!(mb_unlock(mb), S_OK);
            assert_eq!(mb_unlock(mb), E_UNEXPECTED);
            assert_eq!(mb_set_cur(mb, 9), E_INVALIDARG);
            assert_eq!(mb_set_cur(mb, 4), S_OK);
            assert_eq!((sv.add_buffer)(so, *b), S_OK);
            assert_eq!(mb_release(mb), 1);
        }
        let mut total = 0;
        assert_eq!((sv.get_total_length)(so, &mut total), S_OK);
        assert_eq!(total, 8);
        let mut cont = core::ptr::null_mut();
        assert_eq!((sv.convert_to_contiguous_buffer)(so, &mut cont), S_OK);
        let mut p = core::ptr::null_mut();
        let mut cur = 0;
        let cm = cont.cast::<MemBuffer>();
        assert_eq!(mb_lock(cm, &mut p, core::ptr::null_mut(), &mut cur), S_OK);
        // SAFETY: 8 octets copiés.
        assert_eq!(
            unsafe { core::slice::from_raw_parts(p, 8) },
            &[1, 1, 1, 1, 2, 2, 2, 2]
        );
        assert_eq!(mb_unlock(cm), S_OK);
        let mut n = 0;
        assert_eq!((sv.get_buffer_count)(so, &mut n), S_OK);
        assert_eq!(n, 1);
        assert_eq!(mb_release(cm), 1);
        assert_eq!((sv.base.release)(so), 0);
    }

    #[test]
    fn resolver_and_readers_fail_cleanly() {
        let root = super::super::tests_vfs::install("mf");
        std::fs::write(root.join("clip.mp4"), b"\0\0\0\x18ftypmp42").expect("write");
        assert_eq!(MFStartup(MF_VERSION, 0), S_OK);
        let mut r = core::ptr::null_mut();
        assert_eq!(MFCreateSourceResolver(&mut r), S_OK);
        // SAFETY: vtable IMFSourceResolver.
        let rv = unsafe { &**r.cast::<*const ResolverVtbl>() };
        let mut ty = 0u32;
        let mut o = 0x1234usize as *mut c_void;
        let url = to_wide_null("clip.mp4");
        assert_eq!(
            (rv.create_object_from_url)(
                r.cast(),
                url.as_ptr(),
                1,
                core::ptr::null_mut(),
                &mut ty,
                &mut o
            ),
            MF_E_UNSUPPORTED_BYTESTREAM_TYPE
        );
        assert!(o.is_null());
        assert_eq!(ty, MF_OBJECT_INVALID);
        let missing = to_wide_null("file:///absent.mp4");
        assert_eq!(
            (rv.create_object_from_url)(
                r.cast(),
                missing.as_ptr(),
                1,
                core::ptr::null_mut(),
                &mut ty,
                &mut o
            ),
            E_FILE_NOT_FOUND
        );
        let http = to_wide_null("https://example.invalid/a.mp4");
        let mut rd = core::ptr::null_mut();
        assert_eq!(
            MFCreateSourceReaderFromURL(http.as_ptr(), core::ptr::null_mut(), &mut rd),
            MF_E_UNSUPPORTED_SCHEME
        );
        assert_eq!(
            MFCreateSourceReaderFromURL(url.as_ptr(), core::ptr::null_mut(), &mut rd),
            MF_E_UNSUPPORTED_BYTESTREAM_TYPE
        );
        assert!(rd.is_null());
        assert_eq!(
            MFCreateSourceReaderFromMediaSource(
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                &mut rd
            ),
            E_INVALIDARG
        );
        assert_eq!((rv.release)(r.cast()), 0);
        assert_eq!(MFShutdown(), S_OK);
    }
}
