//! `std::locale`, `_Locimp`, `_Lockit`, `_Locinfo`, `_Yarn`, facettes `ctype<wchar_t>` et
//! `codecvt<char, char, _Mbstatet>` (locale « C » classique, émulée sans toucher au `setlocale` hôte).

use core::ffi::{CStr, c_void};
use core::ptr;
use std::collections::HashMap;
use std::sync::Once;
use std::sync::atomic::{AtomicI32, AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use parking_lot::{ReentrantMutex, const_reentrant_mutex};
use sb_common::{ffi_guard, sb_debug};

use super::{
    CHANNEL, CodecvtC, CtypeW, Ctypevec, Cvtvec, Facet, Locale, Locimp, Locinfo, Lockit, Mbstatet,
    VTable, Yarn, fatal_throw, vslot,
};

// ---------- `_Lockit` ----------

const MAX_LOCK: usize = 8;
pub const LOCK_LOCALE: i32 = 0;
#[cfg_attr(not(test), allow(dead_code))]
pub const LOCK_STREAM: i32 = 2;

static LOCKS: [ReentrantMutex<()>; MAX_LOCK] = [
    const_reentrant_mutex(()),
    const_reentrant_mutex(()),
    const_reentrant_mutex(()),
    const_reentrant_mutex(()),
    const_reentrant_mutex(()),
    const_reentrant_mutex(()),
    const_reentrant_mutex(()),
    const_reentrant_mutex(()),
];

pub fn lock_kind(kind: i32) {
    if let Some(m) = usize::try_from(kind).ok().and_then(|k| LOCKS.get(k)) {
        core::mem::forget(m.lock());
    }
}

pub fn unlock_kind(kind: i32) {
    if let Some(m) = usize::try_from(kind).ok().and_then(|k| LOCKS.get(k)) {
        // SAFETY: appairé avec `lock_kind` sur le même thread (sémantique `_Lockit`).
        unsafe { m.force_unlock() };
    }
}

/// Garde RAII Rust équivalent à `_Lockit`.
pub struct LockGuard(i32);

impl LockGuard {
    pub fn new(kind: i32) -> Self {
        lock_kind(kind);
        Self(kind)
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        unlock_kind(self.0);
    }
}

extern "win64" fn lockit_ctor(this: *mut Lockit, kind: i32) -> *mut Lockit {
    ffi_guard!(CHANNEL, "_Lockit::_Lockit", this, {
        if !this.is_null() {
            // SAFETY: `this` pointe vers un `_Lockit` (4 octets) fourni par l'invité.
            unsafe { (*this).locktype = kind };
        }
        lock_kind(kind);
        this
    })
}

extern "win64" fn lockit_dtor(this: *mut Lockit) {
    ffi_guard!(CHANNEL, "_Lockit::~_Lockit", (), {
        if !this.is_null() {
            // SAFETY: `_Lockit` construit par `lockit_ctor`.
            unlock_kind(unsafe { (*this).locktype });
        }
    })
}

// ---------- `_Yarn` ----------

/// Libère la chaîne d'un `_Yarn` (`_Tidy`).
///
/// # Safety
/// `y` doit pointer vers un `_Yarn` valide dont `myptr` provient de `malloc` (ou est nul).
pub unsafe fn yarn_tidy<E>(y: *mut Yarn<E>) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        if !(*y).myptr.is_null() {
            libc::free((*y).myptr.cast());
        }
        (*y).myptr = ptr::null_mut();
    }
}

/// `_Yarn<char>::operator=(const char*)`.
///
/// # Safety
/// `y` valide ; `src` nul ou chaîne C terminée.
pub unsafe fn yarn_set(y: *mut Yarn<u8>, src: *const u8) {
    // SAFETY: contrat de l'appelant ; copie bornée par `strlen + 1`.
    unsafe {
        if (*y).myptr.cast_const() == src {
            return;
        }
        yarn_tidy(y);
        if !src.is_null() {
            let n = libc::strlen(src.cast()) + 1;
            let p = libc::malloc(n).cast::<u8>();
            if !p.is_null() {
                ptr::copy_nonoverlapping(src, p, n);
            }
            (*y).myptr = p;
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
/// `_Yarn<char>::_C_str()`.
///
/// # Safety
/// `y` valide.
pub unsafe fn yarn_cstr(y: *const Yarn<u8>) -> *const u8 {
    // SAFETY: contrat de l'appelant.
    unsafe {
        if (*y).myptr.is_null() {
            &raw const (*y).nul
        } else {
            (*y).myptr
        }
    }
}

extern "win64" fn yarn_assign(this: *mut Yarn<u8>, src: *const u8) -> *mut Yarn<u8> {
    ffi_guard!(CHANNEL, "_Yarn<char>::operator=", this, {
        if !this.is_null() {
            // SAFETY: `this` est un `_Yarn<char>` de l'invité, `src` une chaîne C ou nul.
            unsafe { yarn_set(this, src) };
        }
        this
    })
}

// ---------- facettes : comptage de références ----------

/// Compteur initial des objets de la locale classique (immortels en pratique, comme MSVC).
const IMMORTAL_REFS: u32 = 0x4000_0000;

extern "win64" fn facet_incref(this: *mut Facet) {
    // SAFETY: `this` est une facette valide.
    unsafe { (*this).refs.fetch_add(1, Ordering::AcqRel) };
}

extern "win64" fn facet_decref(this: *mut Facet) -> *mut Facet {
    // SAFETY: `this` est une facette valide.
    let prev = unsafe { (*this).refs.fetch_sub(1, Ordering::AcqRel) };
    if prev == 1 { this } else { ptr::null_mut() }
}

/// Appel virtuel `_Incref`.
///
/// # Safety
/// `f` facette MSVC valide.
pub unsafe fn v_incref(f: *mut Facet) {
    // SAFETY: slot 1 de toute vtable de facette.
    unsafe {
        let p: unsafe extern "win64" fn(*mut Facet) = core::mem::transmute(vslot(f.cast(), 1));
        p(f);
    }
}

/// Appel virtuel `_Decref` puis `delete` si le compteur atteint zéro.
///
/// # Safety
/// `f` facette MSVC valide.
pub unsafe fn v_release(f: *mut Facet) {
    // SAFETY: slots 2 (`_Decref`) et 0 (destructeur de suppression scalaire).
    unsafe {
        let dec: unsafe extern "win64" fn(*mut Facet) -> *mut Facet =
            core::mem::transmute(vslot(f.cast(), 2));
        let dead = dec(f);
        if !dead.is_null() {
            let dtor: unsafe extern "win64" fn(*mut Facet, u32) -> *mut c_void =
                core::mem::transmute(vslot(dead.cast(), 0));
            dtor(dead, 1);
        }
    }
}

extern "win64" fn facet_plain_dtor(this: *mut Facet, flags: u32) -> *mut c_void {
    if flags & 1 != 0 {
        // SAFETY: facette allouée par `malloc` dans ce module.
        unsafe { libc::free(this.cast()) };
    }
    this.cast()
}

// ---------- `_Locimp` ----------

extern "win64" fn locimp_dtor(this: *mut Locimp, flags: u32) -> *mut c_void {
    ffi_guard!(CHANNEL, "_Locimp::~_Locimp", this.cast(), {
        // SAFETY: `this` est un `_Locimp` créé par `new_locimp`.
        unsafe {
            let imp = &mut *this;
            for i in 0..imp.facetcount {
                let f = *imp.facetvec.add(i);
                if !f.is_null() {
                    v_release(f);
                }
            }
            if !imp.facetvec.is_null() {
                libc::free(imp.facetvec.cast());
            }
            imp.facetvec = ptr::null_mut();
            imp.facetcount = 0;
            yarn_tidy(&raw mut imp.name);
            if flags & 1 != 0 {
                libc::free(this.cast());
            }
        }
        this.cast()
    })
}

static LOCIMP_VT: VTable<3> = VTable::new([
    locimp_dtor as *const (),
    facet_incref as *const (),
    facet_decref as *const (),
]);

fn new_locimp(transparent: bool, refs: u32) -> *mut Locimp {
    // SAFETY: allocation brute puis initialisation complète de tous les champs.
    unsafe {
        let p = libc::malloc(size_of::<Locimp>()).cast::<Locimp>();
        if p.is_null() {
            fatal_throw("std::bad_alloc", "bad allocation");
        }
        p.write(Locimp {
            facet: Facet {
                vptr: LOCIMP_VT.ptr(),
                refs: AtomicU32::new(refs),
            },
            facetvec: ptr::null_mut(),
            facetcount: 0,
            catmask: 0,
            xparent: transparent,
            name: Yarn {
                myptr: ptr::null_mut(),
                nul: 0,
            },
        });
        yarn_set(&raw mut (*p).name, c"*".as_ptr().cast());
        p
    }
}

/// `_Locimp::_Addfac(facet*, size_t id)`.
///
/// # Safety
/// `imp` et `fac` valides.
pub unsafe fn locimp_addfac(imp: *mut Locimp, fac: *mut Facet, id: usize) {
    let _g = LockGuard::new(LOCK_LOCALE);
    // SAFETY: contrat de l'appelant ; réallocation du vecteur de facettes.
    unsafe {
        let imp = &mut *imp;
        if id >= imp.facetcount {
            let count = (id + 1).max(40);
            let nv = libc::realloc(imp.facetvec.cast(), count * size_of::<*mut Facet>())
                .cast::<*mut Facet>();
            if nv.is_null() {
                fatal_throw("std::bad_alloc", "bad allocation");
            }
            for i in imp.facetcount..count {
                *nv.add(i) = ptr::null_mut();
            }
            imp.facetvec = nv;
            imp.facetcount = count;
        }
        v_incref(fac);
        let old = *imp.facetvec.add(id);
        *imp.facetvec.add(id) = fac;
        if !old.is_null() {
            v_release(old);
        }
    }
}

/// `_Locimp::_Getfacet` (inline côté STL) : recherche locale puis globale si transparente.
///
/// # Safety
/// `imp` valide.
pub unsafe fn locimp_getfacet(imp: *const Locimp, id: usize) -> *mut Facet {
    // SAFETY: contrat de l'appelant.
    unsafe {
        let f = if id < (*imp).facetcount {
            *(*imp).facetvec.add(id)
        } else {
            ptr::null_mut()
        };
        if !f.is_null() || !(*imp).xparent {
            return f;
        }
        let g = global_locimp();
        if id < (*g).facetcount {
            *(*g).facetvec.add(id)
        } else {
            ptr::null_mut()
        }
    }
}

// ---------- `locale` ----------

/// `std::locale::id::_Id_cnt` (export de données `int`).
pub static ID_CNT: AtomicI32 = AtomicI32::new(0);
/// `std::codecvt<char,char,_Mbstatet>::id` (export de données `locale::id`).
pub static CODECVT_ID: AtomicUsize = AtomicUsize::new(0);
/// `std::ctype<wchar_t>::id` (export de données `locale::id`).
pub static CTYPEW_ID: AtomicUsize = AtomicUsize::new(0);

static GLOBAL_LOCIMP: AtomicPtr<Locimp> = AtomicPtr::new(ptr::null_mut());
/// Objet `locale` statique renvoyé par `locale::classic()` (layout identique : un pointeur).
static CLASSIC_LOCALE: AtomicPtr<Locimp> = AtomicPtr::new(ptr::null_mut());
static CLASSIC_ONCE: Once = Once::new();

/// `locale::id::operator size_t()`.
pub fn id_value(id: &AtomicUsize) -> usize {
    let v = id.load(Ordering::Acquire);
    if v != 0 {
        return v;
    }
    let _g = LockGuard::new(LOCK_LOCALE);
    let v = id.load(Ordering::Acquire);
    if v != 0 {
        return v;
    }
    let n = (ID_CNT.fetch_add(1, Ordering::AcqRel) + 1) as usize;
    id.store(n, Ordering::Release);
    n
}

extern "win64" fn locale_id_value(this: *mut AtomicUsize) -> usize {
    ffi_guard!(CHANNEL, "locale::id::operator size_t", 0, {
        if this.is_null() {
            return 0;
        }
        // SAFETY: `locale::id` = `size_t` aligné ; même layout qu'`AtomicUsize`.
        id_value(unsafe { &*this })
    })
}

fn init_classic() {
    CLASSIC_ONCE.call_once(|| {
        let imp = new_locimp(false, IMMORTAL_REFS);
        // SAFETY: `imp` fraîchement créé ; facettes initialisées avant insertion.
        unsafe {
            (*imp).catmask = 0x3F; // locale::all
            yarn_set(&raw mut (*imp).name, c"C".as_ptr().cast());
            locimp_addfac(imp, new_ctype_w(IMMORTAL_REFS).cast(), id_value(&CTYPEW_ID));
            locimp_addfac(
                imp,
                new_codecvt(IMMORTAL_REFS).cast(),
                id_value(&CODECVT_ID),
            );
        }
        CLASSIC_LOCALE.store(imp, Ordering::Release);
        GLOBAL_LOCIMP.store(imp, Ordering::Release);
    });
}

/// `_Locimp*` global courant (construit la locale classique au besoin).
pub fn global_locimp() -> *mut Locimp {
    init_classic();
    GLOBAL_LOCIMP.load(Ordering::Acquire)
}

/// `locale::_Init(bool _Do_incref)`.
pub fn locale_init_imp(incref: bool) -> *mut Locimp {
    let p = global_locimp();
    if incref {
        // SAFETY: `p` est le `_Locimp` global valide.
        unsafe { v_incref(p.cast()) };
    }
    p
}

extern "win64" fn locale_init(do_incref: u32) -> *mut Locimp {
    ffi_guard!(CHANNEL, "locale::_Init", ptr::null_mut(), {
        locale_init_imp(do_incref as u8 != 0)
    })
}

extern "win64" fn locale_getgloballocale() -> *mut Locimp {
    ffi_guard!(CHANNEL, "locale::_Getgloballocale", ptr::null_mut(), {
        global_locimp()
    })
}

extern "win64" fn locale_classic() -> *const Locale {
    ffi_guard!(CHANNEL, "locale::classic", ptr::null(), {
        init_classic();
        (&raw const CLASSIC_LOCALE).cast::<Locale>()
    })
}

/// Alloue un `locale*` (8 octets, `new locale`) initialisé sur la locale globale.
pub fn new_locale_obj() -> *mut Locale {
    // SAFETY: allocation de 8 octets puis écriture du pointeur `_Locimp` référencé.
    unsafe {
        let p = libc::malloc(size_of::<Locale>()).cast::<Locale>();
        if p.is_null() {
            fatal_throw("std::bad_alloc", "bad allocation");
        }
        p.write(Locale {
            ptr: locale_init_imp(true),
        });
        p
    }
}

/// Destructeur de `locale` (`delete _Ptr->_Decref()`), sans libérer l'objet lui-même.
///
/// # Safety
/// `l` pointe vers un `locale` valide.
pub unsafe fn locale_release(l: *mut Locale) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        let imp = (*l).ptr;
        if !imp.is_null() {
            v_release(imp.cast());
        }
        (*l).ptr = ptr::null_mut();
    }
}

/// `delete pLocale`.
///
/// # Safety
/// `l` nul ou issu de `new_locale_obj`.
pub unsafe fn delete_locale_obj(l: *mut Locale) {
    if l.is_null() {
        return;
    }
    // SAFETY: contrat de l'appelant.
    unsafe {
        locale_release(l);
        libc::free(l.cast());
    }
}

/// Constructeur de copie `locale(const locale&)` dans une zone non initialisée.
///
/// # Safety
/// `dst` inscriptible, `src` valide.
pub unsafe fn locale_copy_into(dst: *mut Locale, src: *const Locale) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        let imp = (*src).ptr;
        if !imp.is_null() {
            v_incref(imp.cast());
        }
        dst.write(Locale { ptr: imp });
    }
}

/// `locale::operator=`.
///
/// # Safety
/// `dst` et `src` valides.
pub unsafe fn locale_assign(dst: *mut Locale, src: *const Locale) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        if (*dst).ptr == (*src).ptr {
            return;
        }
        let imp = (*src).ptr;
        if !imp.is_null() {
            v_incref(imp.cast());
        }
        locale_release(dst);
        (*dst).ptr = imp;
    }
}

extern "win64" fn locimp_new_locimp(transparent: u32) -> *mut Locimp {
    ffi_guard!(CHANNEL, "_Locimp::_New_Locimp", ptr::null_mut(), {
        new_locimp(transparent as u8 != 0, 1)
    })
}

/// Paire (`locale::id` statique, fabrique de facette) utilisée par `_Makeloc`.
type FacetMaker = (&'static AtomicUsize, fn() -> *mut Facet);

/// `_CATMASK(_X_CTYPE)` : catégorie `locale::ctype`.
const CAT_CTYPE: i32 = 0x2;
/// Valeur renvoyée par `_Getcat` pour les facettes de la catégorie ctype (`_X_CTYPE`).
const X_CTYPE: usize = 2;

extern "win64" fn locimp_makeloc(
    lobj: *const Locinfo,
    cat: i32,
    imp: *mut Locimp,
    ploc: *const Locale,
) -> *mut Locimp {
    ffi_guard!(CHANNEL, "_Locimp::_Makeloc", imp, {
        if imp.is_null() {
            return imp;
        }
        if cat & CAT_CTYPE != 0 {
            let makers: [FacetMaker; 2] = [
                (&CTYPEW_ID, || new_ctype_w(0).cast()),
                (&CODECVT_ID, || new_codecvt(0).cast()),
            ];
            for (id_static, maker) in makers {
                let id = id_value(id_static);
                // SAFETY: `ploc` nul ou locale valide ; `imp` valide.
                unsafe {
                    let from_loc = if ploc.is_null() || (*ploc).ptr.is_null() {
                        ptr::null_mut()
                    } else {
                        locimp_getfacet((*ploc).ptr, id)
                    };
                    let fac = if from_loc.is_null() {
                        maker()
                    } else {
                        from_loc
                    };
                    locimp_addfac(imp, fac, id);
                }
            }
        }
        sb_debug!(
            CHANNEL,
            "_Locimp::_Makeloc(cat={cat:#x}) — facettes ctype<wchar_t>/codecvt<char> installées"
        );
        imp
    })
}

// ---------- `_Locinfo` ----------

/// Nom de locale accepté par l'émulation « C » (les autres échouent comme un nom invalide).
fn resolve_locale_name(name: &str) -> Option<&str> {
    let lower = name.to_ascii_lowercase();
    if name.is_empty() || name == "C" || name == "POSIX" {
        Some("C")
    } else if lower.ends_with(".utf-8") || lower.ends_with(".utf8") {
        Some(name)
    } else {
        None
    }
}

extern "win64" fn locinfo_ctor(this: *mut Locinfo, cat: i32, name: *const i8) -> *mut Locinfo {
    ffi_guard!(CHANNEL, "_Locinfo::_Locinfo", this, {
        if this.is_null() {
            return this;
        }
        if name.is_null() {
            fatal_throw("std::runtime_error", "bad locale name");
        }
        lock_kind(LOCK_LOCALE);
        let empty8 = || Yarn::<u8> {
            myptr: ptr::null_mut(),
            nul: 0,
        };
        let empty16 = || Yarn::<u16> {
            myptr: ptr::null_mut(),
            nul: 0,
        };
        // SAFETY: `this` pointe vers 0x68 octets inscriptibles ; `name` chaîne C valide.
        unsafe {
            this.write(Locinfo {
                lock: Lockit {
                    locktype: LOCK_LOCALE,
                },
                days: empty8(),
                months: empty8(),
                w_days: empty16(),
                w_months: empty16(),
                oldlocname: empty8(),
                newlocname: empty8(),
            });
            yarn_set(&raw mut (*this).oldlocname, c"C".as_ptr().cast());
            let req = CStr::from_ptr(name).to_string_lossy();
            match resolve_locale_name(&req) {
                Some(n) => {
                    let c = std::ffi::CString::new(n).unwrap_or_default();
                    yarn_set(&raw mut (*this).newlocname, c.as_ptr().cast());
                }
                None => {
                    sb_debug!(
                        CHANNEL,
                        "_Locinfo(cat={cat}, '{req}') : locale inconnue -> '*'"
                    );
                    yarn_set(&raw mut (*this).newlocname, c"*".as_ptr().cast());
                }
            }
        }
        this
    })
}

extern "win64" fn locinfo_dtor(this: *mut Locinfo) {
    ffi_guard!(CHANNEL, "_Locinfo::~_Locinfo", (), {
        if this.is_null() {
            return;
        }
        // SAFETY: `_Locinfo` construit par `locinfo_ctor`.
        unsafe {
            yarn_tidy(&raw mut (*this).days);
            yarn_tidy(&raw mut (*this).months);
            yarn_tidy(&raw mut (*this).w_days);
            yarn_tidy(&raw mut (*this).w_months);
            yarn_tidy(&raw mut (*this).oldlocname);
            yarn_tidy(&raw mut (*this).newlocname);
            unlock_kind((*this).lock.locktype);
        }
    })
}

// ---------- `ctype<wchar_t>` ----------

/// Classification UCRT locale « C » (`_UPPER`=1, `_LOWER`=2, `_DIGIT`=4, `_SPACE`=8,
/// `_PUNCT`=0x10, `_CONTROL`=0x20, `_BLANK`=0x40, `_HEX`=0x80, `_ALPHA`=0x100).
pub const fn classify_ascii(c: u32) -> u16 {
    match c {
        0x09 => 0x68,
        0x0A..=0x0D => 0x28,
        0x20 => 0x48,
        0x00..=0x1F | 0x7F => 0x20,
        0x30..=0x39 => 0x84,
        0x41..=0x46 => 0x181,
        0x47..=0x5A => 0x101,
        0x61..=0x66 => 0x182,
        0x67..=0x7A => 0x102,
        0x21..=0x7E => 0x10,
        _ => 0,
    }
}

pub fn classify_wide(c: u32) -> u16 {
    if c < 0x80 {
        return classify_ascii(c);
    }
    let Some(ch) = char::from_u32(c) else {
        return 0;
    };
    if ch.is_alphabetic() {
        0x100 | if ch.is_uppercase() { 1 } else { 0 } | if ch.is_lowercase() { 2 } else { 0 }
    } else if ch.is_whitespace() {
        0x08 | if c == 0xA0 { 0x40 } else { 0 }
    } else if ch.is_control() {
        0x20
    } else if ch.is_numeric() {
        0x04
    } else {
        0x10
    }
}

const fn build_ctype_table() -> [i16; 256] {
    let mut t = [0i16; 256];
    let mut i = 0;
    while i < 128 {
        t[i] = classify_ascii(i as u32) as i16;
        i += 1;
    }
    t
}

/// Table `_Ctypevec::_Table` de la locale « C ».
static CTYPE_TABLE: [i16; 256] = build_ctype_table();

extern "win64" fn ctw_is_range(
    this: *const CtypeW,
    first: *const u16,
    last: *const u16,
    dest: *mut i16,
) -> *const u16 {
    let mut p = first;
    let mut d = dest;
    while p < last {
        // SAFETY: plage `[first, last)` et `dest` fournies par l'invité.
        unsafe {
            *d = classify_wide(u32::from(*p)) as i16;
            p = p.add(1);
            d = d.add(1);
        }
    }
    p
}

extern "win64" fn ctw_is_ch(this: *const CtypeW, mask: u32, ch: u32) -> bool {
    classify_wide(ch & 0xFFFF) & (mask as u16) != 0
}

extern "win64" fn ctw_scan_is(
    this: *const CtypeW,
    mask: u32,
    first: *const u16,
    last: *const u16,
) -> *const u16 {
    let mut p = first;
    // SAFETY: plage invité.
    while p < last && classify_wide(u32::from(unsafe { *p })) & (mask as u16) == 0 {
        p = p.wrapping_add(1);
    }
    p
}

extern "win64" fn ctw_scan_not(
    this: *const CtypeW,
    mask: u32,
    first: *const u16,
    last: *const u16,
) -> *const u16 {
    let mut p = first;
    // SAFETY: plage invité.
    while p < last && classify_wide(u32::from(unsafe { *p })) & (mask as u16) != 0 {
        p = p.wrapping_add(1);
    }
    p
}

fn lower_w(c: u16) -> u16 {
    if (0x41..=0x5A).contains(&c) {
        c + 0x20
    } else {
        c
    }
}

fn upper_w(c: u16) -> u16 {
    if (0x61..=0x7A).contains(&c) {
        c - 0x20
    } else {
        c
    }
}

extern "win64" fn ctw_tolower_range(
    this: *const CtypeW,
    first: *mut u16,
    last: *const u16,
) -> *const u16 {
    let mut p = first;
    while p.cast_const() < last {
        // SAFETY: plage invité modifiable.
        unsafe {
            *p = lower_w(*p);
            p = p.add(1);
        }
    }
    p
}

extern "win64" fn ctw_tolower_ch(this: *const CtypeW, ch: u32) -> u16 {
    lower_w(ch as u16)
}

extern "win64" fn ctw_toupper_range(
    this: *const CtypeW,
    first: *mut u16,
    last: *const u16,
) -> *const u16 {
    let mut p = first;
    while p.cast_const() < last {
        // SAFETY: plage invité modifiable.
        unsafe {
            *p = upper_w(*p);
            p = p.add(1);
        }
    }
    p
}

extern "win64" fn ctw_toupper_ch(this: *const CtypeW, ch: u32) -> u16 {
    upper_w(ch as u16)
}

extern "win64" fn ctw_widen_range(
    this: *const CtypeW,
    first: *const u8,
    last: *const u8,
    dest: *mut u16,
) -> *const u8 {
    let mut p = first;
    let mut d = dest;
    while p < last {
        // SAFETY: plages invité.
        unsafe {
            *d = u16::from(*p);
            p = p.add(1);
            d = d.add(1);
        }
    }
    p
}

extern "win64" fn ctw_widen_ch(this: *const CtypeW, byte: u32) -> u16 {
    u16::from(byte as u8)
}

extern "win64" fn ctw_narrow_range(
    this: *const CtypeW,
    first: *const u16,
    last: *const u16,
    dflt: u32,
    dest: *mut u8,
) -> *const u16 {
    let mut p = first;
    let mut d = dest;
    while p < last {
        // SAFETY: plages invité.
        unsafe {
            let c = *p;
            *d = if c < 0x100 { c as u8 } else { dflt as u8 };
            p = p.add(1);
            d = d.add(1);
        }
    }
    p
}

extern "win64" fn ctw_narrow_ch(this: *const CtypeW, ch: u32, dflt: u32) -> u8 {
    let c = ch as u16;
    if c < 0x100 { c as u8 } else { dflt as u8 }
}

/// Vtable `ctype<wchar_t>` : facette puis virtuelles `do_*` (surcharges groupées en ordre
/// inverse de déclaration, convention MSVC).
static CTYPEW_VT: VTable<15> = VTable::new([
    facet_plain_dtor as *const (),
    facet_incref as *const (),
    facet_decref as *const (),
    ctw_is_range as *const (),
    ctw_is_ch as *const (),
    ctw_scan_is as *const (),
    ctw_scan_not as *const (),
    ctw_tolower_range as *const (),
    ctw_tolower_ch as *const (),
    ctw_toupper_range as *const (),
    ctw_toupper_ch as *const (),
    ctw_widen_range as *const (),
    ctw_widen_ch as *const (),
    ctw_narrow_range as *const (),
    ctw_narrow_ch as *const (),
]);
const CTYPEW_SLOT_WIDEN_CH: usize = 12;

fn new_ctype_w(refs: u32) -> *mut CtypeW {
    // SAFETY: allocation puis initialisation complète.
    unsafe {
        let p = libc::malloc(size_of::<CtypeW>()).cast::<CtypeW>();
        if p.is_null() {
            fatal_throw("std::bad_alloc", "bad allocation");
        }
        p.write(CtypeW {
            facet: Facet {
                vptr: CTYPEW_VT.ptr(),
                refs: AtomicU32::new(refs),
            },
            ctype: Ctypevec {
                page: 0,
                table: CTYPE_TABLE.as_ptr(),
                delfl: 0,
                locale_name: ptr::null_mut(),
            },
            cvt: Cvtvec {
                page: 0,
                mbcurmax: 1,
                isclocale: 1,
                isleadbyte: [0; 32],
            },
        });
        p
    }
}

extern "win64" fn ctypew_getcat(ppf: *mut *const Facet, ploc: *const Locale) -> usize {
    ffi_guard!(CHANNEL, "ctype<wchar_t>::_Getcat", X_CTYPE, {
        // SAFETY: `ppf` nul ou pointeur de facette invité.
        unsafe {
            if !ppf.is_null() && (*ppf).is_null() {
                *ppf = new_ctype_w(0).cast::<Facet>();
            }
        }
        X_CTYPE
    })
}

extern "win64" fn ctypew_widen(this: *const CtypeW, byte: u32) -> u16 {
    ffi_guard!(CHANNEL, "ctype<wchar_t>::widen", 0, {
        // SAFETY: appel virtuel `do_widen(char)` sur une facette `ctype<wchar_t>` valide.
        unsafe {
            let f: unsafe extern "win64" fn(*const CtypeW, u32) -> u16 =
                core::mem::transmute(vslot(this.cast(), CTYPEW_SLOT_WIDEN_CH));
            f(this, byte & 0xFF)
        }
    })
}

// ---------- `codecvt<char, char, _Mbstatet>` ----------

/// `codecvt_base::result`.
const CVT_NOCONV: i32 = 3;

extern "win64" fn cvt_always_noconv(this: *const CodecvtC) -> bool {
    true
}

extern "win64" fn cvt_max_length(this: *const CodecvtC) -> i32 {
    1
}

extern "win64" fn cvt_encoding(this: *const CodecvtC) -> i32 {
    1
}

#[allow(clippy::too_many_arguments)]
extern "win64" fn cvt_do_in(
    this: *const CodecvtC,
    state: *mut Mbstatet,
    from: *const u8,
    from_end: *const u8,
    from_next: *mut *const u8,
    to: *mut u8,
    to_end: *mut u8,
    to_next: *mut *mut u8,
) -> i32 {
    // SAFETY: références de sortie fournies par l'invité (non nulles par contrat C++).
    unsafe {
        if !from_next.is_null() {
            *from_next = from;
        }
        if !to_next.is_null() {
            *to_next = to;
        }
    }
    CVT_NOCONV
}

extern "win64" fn cvt_do_unshift(
    this: *const CodecvtC,
    state: *mut Mbstatet,
    to: *mut u8,
    to_end: *mut u8,
    to_next: *mut *mut u8,
) -> i32 {
    if !to_next.is_null() {
        // SAFETY: référence de sortie invité.
        unsafe { *to_next = to };
    }
    CVT_NOCONV
}

extern "win64" fn cvt_do_length(
    this: *const CodecvtC,
    state: *mut Mbstatet,
    first: *const u8,
    last: *const u8,
    count: usize,
) -> i32 {
    let avail = (last as usize).saturating_sub(first as usize);
    avail.min(count).min(i32::MAX as usize) as i32
}

static CODECVT_VT: VTable<10> = VTable::new([
    facet_plain_dtor as *const (),
    facet_incref as *const (),
    facet_decref as *const (),
    cvt_always_noconv as *const (),
    cvt_max_length as *const (),
    cvt_encoding as *const (),
    cvt_do_in as *const (),
    cvt_do_in as *const (), // do_out : même sémantique « noconv »
    cvt_do_unshift as *const (),
    cvt_do_length as *const (),
]);
const CVT_SLOT_NOCONV: usize = 3;
const CVT_SLOT_IN: usize = 6;
const CVT_SLOT_OUT: usize = 7;
const CVT_SLOT_UNSHIFT: usize = 8;

fn new_codecvt(refs: u32) -> *mut CodecvtC {
    // SAFETY: allocation puis initialisation complète.
    unsafe {
        let p = libc::malloc(size_of::<CodecvtC>()).cast::<CodecvtC>();
        if p.is_null() {
            fatal_throw("std::bad_alloc", "bad allocation");
        }
        p.write(CodecvtC {
            facet: Facet {
                vptr: CODECVT_VT.ptr(),
                refs: AtomicU32::new(refs),
            },
        });
        p
    }
}

extern "win64" fn codecvt_getcat(ppf: *mut *const Facet, ploc: *const Locale) -> usize {
    ffi_guard!(CHANNEL, "codecvt<char>::_Getcat", X_CTYPE, {
        // SAFETY: `ppf` nul ou pointeur de facette invité.
        unsafe {
            if !ppf.is_null() && (*ppf).is_null() {
                *ppf = new_codecvt(0).cast::<Facet>();
            }
        }
        X_CTYPE
    })
}

extern "win64" fn codecvt_base_always_noconv(this: *const c_void) -> bool {
    ffi_guard!(CHANNEL, "codecvt_base::always_noconv", true, {
        // SAFETY: appel virtuel `do_always_noconv` (slot 3 de `codecvt_base`).
        unsafe {
            let f: unsafe extern "win64" fn(*const c_void) -> bool =
                core::mem::transmute(vslot(this, CVT_SLOT_NOCONV));
            f(this)
        }
    })
}

type CvtFn = unsafe extern "win64" fn(
    *const c_void,
    *mut Mbstatet,
    *const u8,
    *const u8,
    *mut *const u8,
    *mut u8,
    *mut u8,
    *mut *mut u8,
) -> i32;

#[allow(clippy::too_many_arguments)]
extern "win64" fn codecvt_in(
    this: *const c_void,
    state: *mut Mbstatet,
    from: *const u8,
    from_end: *const u8,
    from_next: *mut *const u8,
    to: *mut u8,
    to_end: *mut u8,
    to_next: *mut *mut u8,
) -> i32 {
    ffi_guard!(CHANNEL, "codecvt<char>::in", 2, {
        // SAFETY: appel virtuel `do_in`.
        unsafe {
            let f: CvtFn = core::mem::transmute(vslot(this, CVT_SLOT_IN));
            f(this, state, from, from_end, from_next, to, to_end, to_next)
        }
    })
}

#[allow(clippy::too_many_arguments)]
extern "win64" fn codecvt_out(
    this: *const c_void,
    state: *mut Mbstatet,
    from: *const u8,
    from_end: *const u8,
    from_next: *mut *const u8,
    to: *mut u8,
    to_end: *mut u8,
    to_next: *mut *mut u8,
) -> i32 {
    ffi_guard!(CHANNEL, "codecvt<char>::out", 2, {
        // SAFETY: appel virtuel `do_out`.
        unsafe {
            let f: CvtFn = core::mem::transmute(vslot(this, CVT_SLOT_OUT));
            f(this, state, from, from_end, from_next, to, to_end, to_next)
        }
    })
}

extern "win64" fn codecvt_unshift(
    this: *const c_void,
    state: *mut Mbstatet,
    to: *mut u8,
    to_end: *mut u8,
    to_next: *mut *mut u8,
) -> i32 {
    ffi_guard!(CHANNEL, "codecvt<char>::unshift", 2, {
        // SAFETY: appel virtuel `do_unshift`.
        unsafe {
            let f: unsafe extern "win64" fn(
                *const c_void,
                *mut Mbstatet,
                *mut u8,
                *mut u8,
                *mut *mut u8,
            ) -> i32 = core::mem::transmute(vslot(this, CVT_SLOT_UNSHIFT));
            f(this, state, to, to_end, to_next)
        }
    })
}

pub fn register(m: &mut HashMap<&'static str, usize>) {
    macro_rules! reg {
        ($name:expr, $f:expr) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("??0_Lockit@std@@QEAA@H@Z", lockit_ctor);
    reg!("??1_Lockit@std@@QEAA@XZ", lockit_dtor);
    reg!("??4?$_Yarn@D@std@@QEAAAEAV01@PEBD@Z", yarn_assign);
    reg!("??0_Locinfo@std@@QEAA@HPEBD@Z", locinfo_ctor);
    reg!("??1_Locinfo@std@@QEAA@XZ", locinfo_dtor);
    reg!("??Bid@locale@std@@QEAA_KXZ", locale_id_value);
    reg!("?_Init@locale@std@@CAPEAV_Locimp@12@_N@Z", locale_init);
    reg!(
        "?_Getgloballocale@locale@std@@CAPEAV_Locimp@12@XZ",
        locale_getgloballocale
    );
    reg!("?classic@locale@std@@SAAEBV12@XZ", locale_classic);
    reg!(
        "?_New_Locimp@_Locimp@locale@std@@CAPEAV123@_N@Z",
        locimp_new_locimp
    );
    reg!(
        "?_Makeloc@_Locimp@locale@std@@CAPEAV123@AEBV_Locinfo@3@HPEAV123@PEBV23@@Z",
        locimp_makeloc
    );
    reg!(
        "?_Getcat@?$ctype@_W@std@@SA_KPEAPEBVfacet@locale@2@PEBV42@@Z",
        ctypew_getcat
    );
    reg!(
        "?_Getcat@?$codecvt@DDU_Mbstatet@@@std@@SA_KPEAPEBVfacet@locale@2@PEBV42@@Z",
        codecvt_getcat
    );
    reg!("?widen@?$ctype@_W@std@@QEBA_WD@Z", ctypew_widen);
    reg!(
        "?always_noconv@codecvt_base@std@@QEBA_NXZ",
        codecvt_base_always_noconv
    );
    reg!(
        "?in@?$codecvt@DDU_Mbstatet@@@std@@QEBAHAEAU_Mbstatet@@PEBD1AEAPEBDPEAD3AEAPEAD@Z",
        codecvt_in
    );
    reg!(
        "?out@?$codecvt@DDU_Mbstatet@@@std@@QEBAHAEAU_Mbstatet@@PEBD1AEAPEBDPEAD3AEAPEAD@Z",
        codecvt_out
    );
    reg!(
        "?unshift@?$codecvt@DDU_Mbstatet@@@std@@QEBAHAEAU_Mbstatet@@PEAD1AEAPEAD@Z",
        codecvt_unshift
    );
    // Exports de données : adresses des objets statiques.
    m.insert("?_Id_cnt@id@locale@std@@0HA", (&raw const ID_CNT) as usize);
    m.insert(
        "?id@?$codecvt@DDU_Mbstatet@@@std@@2V0locale@2@A",
        (&raw const CODECVT_ID) as usize,
    );
    m.insert(
        "?id@?$ctype@_W@std@@2V0locale@2@A",
        (&raw const CTYPEW_ID) as usize,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_id_assigns_unique_increasing_ids() {
        let a = AtomicUsize::new(0);
        let b = AtomicUsize::new(0);
        let ia = locale_id_value(&raw const a as *mut _);
        let ib = locale_id_value(&raw const b as *mut _);
        assert!(ia > 0 && ib > 0 && ia != ib);
        assert_eq!(locale_id_value(&raw const a as *mut _), ia);
        assert!(ID_CNT.load(Ordering::Acquire) as usize >= ia.max(ib));
    }

    #[test]
    fn classic_locale_has_working_facets() {
        let loc = locale_classic();
        assert!(!loc.is_null());
        // SAFETY: locale classique valide.
        let imp = unsafe { (*loc).ptr };
        assert_eq!(imp, locale_getgloballocale());
        // SAFETY: `_Locimp` classique.
        unsafe {
            assert_eq!(
                CStr::from_ptr(yarn_cstr(&raw const (*imp).name).cast()),
                c"C"
            );
            let ct = locimp_getfacet(imp, id_value(&CTYPEW_ID)).cast::<CtypeW>();
            assert!(!ct.is_null());
            assert_eq!(ctypew_widen(ct, u32::from(b'A')), u16::from(b'A'));
            assert_eq!(ctypew_widen(ct, 0xE9), 0xE9);
            assert!(ctw_is_ch(ct, 0x8, u32::from(b' ')));
            assert!(!ctw_is_ch(ct, 0x4, u32::from(b'a')));
            let cv = locimp_getfacet(imp, id_value(&CODECVT_ID));
            assert!(codecvt_base_always_noconv(cv.cast()));
            let src = *b"abc";
            let mut dst = [0u8; 3];
            let mut st = Mbstatet::default();
            let mut fnext = ptr::null::<u8>();
            let mut tnext = ptr::null_mut::<u8>();
            let r = codecvt_in(
                cv.cast(),
                &mut st,
                src.as_ptr(),
                src.as_ptr().add(3),
                &mut fnext,
                dst.as_mut_ptr(),
                dst.as_mut_ptr().add(3),
                &mut tnext,
            );
            assert_eq!(r, CVT_NOCONV);
            assert_eq!(fnext, src.as_ptr());
            assert_eq!(tnext, dst.as_mut_ptr());
            let mut un = ptr::null_mut::<u8>();
            assert_eq!(
                codecvt_unshift(
                    cv.cast(),
                    &mut st,
                    dst.as_mut_ptr(),
                    dst.as_mut_ptr(),
                    &mut un
                ),
                CVT_NOCONV
            );
            assert_eq!(un, dst.as_mut_ptr());
        }
    }

    #[test]
    fn getcat_creates_facets_and_refcount_deletes() {
        let mut pf: *const Facet = ptr::null();
        assert_eq!(ctypew_getcat(&mut pf, ptr::null()), X_CTYPE);
        assert!(!pf.is_null());
        let f = pf.cast_mut();
        // SAFETY: facette fraîchement créée (refs 0).
        unsafe {
            v_incref(f);
            assert_eq!((*f).refs.load(Ordering::Acquire), 1);
            v_release(f); // compteur 0 -> suppression
        }
        let mut pc: *const Facet = ptr::null();
        assert_eq!(codecvt_getcat(&mut pc, ptr::null()), X_CTYPE);
        assert!(!pc.is_null());
        // SAFETY: facette valide.
        unsafe {
            v_incref(pc.cast_mut());
            v_release(pc.cast_mut());
        }
    }

    #[test]
    fn locinfo_makeloc_and_yarn() {
        let mut li = core::mem::MaybeUninit::<Locinfo>::uninit();
        let p = locinfo_ctor(li.as_mut_ptr(), 0x3F, c"C".as_ptr());
        // SAFETY: construit ci-dessus.
        unsafe {
            assert_eq!(
                CStr::from_ptr(yarn_cstr(&raw const (*p).newlocname).cast()),
                c"C"
            );
            let imp = locimp_new_locimp(0);
            assert_eq!(locimp_makeloc(p, 0x3F, imp, ptr::null()), imp);
            assert!(!locimp_getfacet(imp, id_value(&CTYPEW_ID)).is_null());
            assert!(!locimp_getfacet(imp, id_value(&CODECVT_ID)).is_null());
            yarn_assign(&raw mut (*imp).name, c"C".as_ptr().cast());
            assert_eq!(
                CStr::from_ptr(yarn_cstr(&raw const (*imp).name).cast()),
                c"C"
            );
            locinfo_dtor(p);
            v_release(imp.cast()); // refs 1 -> 0 : destruction complète
        }
        let mut bad = core::mem::MaybeUninit::<Locinfo>::uninit();
        let p = locinfo_ctor(bad.as_mut_ptr(), 0x3F, c"Klingon_Qo'noS.1252".as_ptr());
        // SAFETY: construit ci-dessus.
        unsafe {
            assert_eq!(
                CStr::from_ptr(yarn_cstr(&raw const (*p).newlocname).cast()),
                c"*"
            );
            locinfo_dtor(p);
        }
    }

    #[test]
    fn lockit_is_recursive() {
        let mut a = Lockit { locktype: 0 };
        let mut b = Lockit { locktype: 0 };
        lockit_ctor(&mut a, LOCK_STREAM);
        lockit_ctor(&mut b, LOCK_STREAM);
        lockit_dtor(&mut b);
        lockit_dtor(&mut a);
        assert_eq!(a.locktype, LOCK_STREAM);
    }

    #[test]
    fn ctype_wide_classification_ranges() {
        let s: Vec<u16> = "a1 Z".encode_utf16().collect();
        let mut masks = [0i16; 4];
        let end = ctw_is_range(
            ptr::null(),
            s.as_ptr(),
            s.as_ptr().wrapping_add(4),
            masks.as_mut_ptr(),
        );
        assert_eq!(end, s.as_ptr().wrapping_add(4));
        assert_eq!(masks, [0x182, 0x84, 0x48, 0x101]);
        let mut up = s.clone();
        ctw_toupper_range(ptr::null(), up.as_mut_ptr(), up.as_ptr().wrapping_add(4));
        assert_eq!(String::from_utf16_lossy(&up), "A1 Z");
        assert_eq!(ctw_narrow_ch(ptr::null(), 0x263A, u32::from(b'?')), b'?');
        assert_eq!(
            ctw_scan_is(ptr::null(), 0x4, s.as_ptr(), s.as_ptr().wrapping_add(4)),
            s.as_ptr().wrapping_add(1)
        );
    }
}
