//! `std::basic_streambuf<char>` / `<wchar_t>` : membres non virtuels, implémentations par défaut
//! des virtuelles et dispatch virtuel via le vptr de l'objet (les classes dérivées de l'invité —
//! `basic_filebuf`, `basic_stringbuf`… — surchargent ces entrées).
//!
//! Ordre de la vtable MSVC v143 (`<streambuf>`) : 0 `~basic_streambuf` (suppression scalaire),
//! 1 `_Lock`, 2 `_Unlock`, 3 `overflow`, 4 `pbackfail`, 5 `showmanyc`, 6 `underflow`, 7 `uflow`,
//! 8 `xsgetn`, 9 `xsputn`, 10 `seekoff`, 11 `seekpos`, 12 `setbuf`, 13 `sync`, 14 `imbue`.

use core::ffi::c_void;
use core::ptr;
use std::collections::HashMap;

use sb_common::ffi_guard;

use super::locale::{delete_locale_obj, locale_copy_into, new_locale_obj};
use super::{CHANNEL, Elem, Fpos, Locale, StreamBuf, VTable, vslot};

#[cfg_attr(not(test), allow(dead_code))]
pub const SLOT_DTOR: usize = 0;
pub const SLOT_LOCK: usize = 1;
pub const SLOT_UNLOCK: usize = 2;
pub const SLOT_OVERFLOW: usize = 3;
pub const SLOT_UNDERFLOW: usize = 6;
pub const SLOT_UFLOW: usize = 7;
pub const SLOT_XSGETN: usize = 8;
pub const SLOT_XSPUTN: usize = 9;
pub const SLOT_SEEKOFF: usize = 10;
pub const SLOT_SEEKPOS: usize = 11;
pub const SLOT_SYNC: usize = 13;
pub const SLOT_IMBUE: usize = 14;

type Sb<E> = *mut StreamBuf<E>;

// ---------- accès aux pointeurs de tampon (via les indirections `_IG*` / `_IP*`) ----------

/// # Safety
/// `sb` doit être un `basic_streambuf` initialisé (`_Init` appelé).
pub unsafe fn gptr<E>(sb: Sb<E>) -> *mut E {
    // SAFETY: contrat de l'appelant.
    unsafe { *(*sb).ignext }
}

/// # Safety
/// Voir [`gptr`].
pub unsafe fn pptr<E>(sb: Sb<E>) -> *mut E {
    // SAFETY: contrat de l'appelant.
    unsafe { *(*sb).ipnext }
}

/// # Safety
/// Voir [`gptr`].
pub unsafe fn gnavail<E>(sb: Sb<E>) -> i64 {
    // SAFETY: contrat de l'appelant.
    unsafe {
        if (*(*sb).ignext).is_null() {
            0
        } else {
            i64::from(*(*sb).igcount)
        }
    }
}

/// # Safety
/// Voir [`gptr`].
pub unsafe fn pnavail<E>(sb: Sb<E>) -> i64 {
    // SAFETY: contrat de l'appelant.
    unsafe {
        if (*(*sb).ipnext).is_null() {
            0
        } else {
            i64::from(*(*sb).ipcount)
        }
    }
}

/// `_Gninc` : renvoie `gptr()` puis l'avance.
///
/// # Safety
/// Voir [`gptr`].
pub unsafe fn gninc<E>(sb: Sb<E>) -> *mut E {
    // SAFETY: contrat de l'appelant.
    unsafe {
        *(*sb).igcount -= 1;
        let p = *(*sb).ignext;
        *(*sb).ignext = p.wrapping_add(1);
        p
    }
}

/// `_Gnpreinc` : avance `gptr()` puis le renvoie.
///
/// # Safety
/// Voir [`gptr`].
pub unsafe fn gnpreinc<E>(sb: Sb<E>) -> *mut E {
    // SAFETY: contrat de l'appelant.
    unsafe {
        *(*sb).igcount -= 1;
        let p = (*(*sb).ignext).wrapping_add(1);
        *(*sb).ignext = p;
        p
    }
}

/// `_Pninc` : renvoie `pptr()` puis l'avance.
///
/// # Safety
/// Voir [`gptr`].
pub unsafe fn pninc<E>(sb: Sb<E>) -> *mut E {
    // SAFETY: contrat de l'appelant.
    unsafe {
        *(*sb).ipcount -= 1;
        let p = *(*sb).ipnext;
        *(*sb).ipnext = p.wrapping_add(1);
        p
    }
}

/// # Safety
/// Voir [`gptr`].
pub unsafe fn gbump<E>(sb: Sb<E>, n: i32) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        *(*sb).igcount -= n;
        *(*sb).ignext = (*(*sb).ignext).wrapping_offset(n as isize);
    }
}

/// # Safety
/// Voir [`gptr`].
pub unsafe fn pbump<E>(sb: Sb<E>, n: i32) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        *(*sb).ipcount -= n;
        *(*sb).ipnext = (*(*sb).ipnext).wrapping_offset(n as isize);
    }
}

/// `setg(first, next, last)`.
///
/// # Safety
/// Voir [`gptr`].
pub unsafe fn setg<E>(sb: Sb<E>, first: *mut E, next: *mut E, last: *mut E) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        *(*sb).igfirst = first;
        *(*sb).ignext = next;
        *(*sb).igcount = ((last as usize).wrapping_sub(next as usize) / size_of::<E>()) as i32;
    }
}

/// `setp(first, last)`.
///
/// # Safety
/// Voir [`gptr`].
pub unsafe fn setp<E>(sb: Sb<E>, first: *mut E, last: *mut E) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        *(*sb).ipfirst = first;
        *(*sb).ipnext = first;
        *(*sb).ipcount = ((last as usize).wrapping_sub(first as usize) / size_of::<E>()) as i32;
    }
}

/// `basic_streambuf::_Init()` : relie les indirections aux membres internes et vide les tampons.
///
/// # Safety
/// `sb` pointe vers au moins 0x68 octets inscriptibles.
pub unsafe fn sb_init<E>(sb: Sb<E>) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        (*sb).igfirst = &raw mut (*sb).gfirst;
        (*sb).ipfirst = &raw mut (*sb).pfirst;
        (*sb).ignext = &raw mut (*sb).gnext;
        (*sb).ipnext = &raw mut (*sb).pnext;
        (*sb).igcount = &raw mut (*sb).gcount;
        (*sb).ipcount = &raw mut (*sb).pcount;
        setp(sb, ptr::null_mut(), ptr::null_mut());
        setg(sb, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
    }
}

// ---------- dispatch virtuel ----------

macro_rules! vfn {
    ($sb:expr, $slot:expr, $ty:ty) => {{
        // SAFETY: slot de la vtable `basic_streambuf` de l'objet (contrat de l'appelant).
        let f: $ty = unsafe { core::mem::transmute(vslot($sb.cast::<c_void>(), $slot)) };
        f
    }};
}

/// # Safety
/// `sb` est un `basic_streambuf<E>` valide (vptr compris).
pub unsafe fn v_overflow<E: Elem>(sb: Sb<E>, c: E::Int) -> E::Int {
    let f = vfn!(
        sb,
        SLOT_OVERFLOW,
        unsafe extern "win64" fn(Sb<E>, E::Int) -> E::Int
    );
    // SAFETY: contrat de l'appelant.
    unsafe { f(sb, c) }
}

/// # Safety
/// Voir [`v_overflow`].
pub unsafe fn v_underflow<E: Elem>(sb: Sb<E>) -> E::Int {
    let f = vfn!(
        sb,
        SLOT_UNDERFLOW,
        unsafe extern "win64" fn(Sb<E>) -> E::Int
    );
    // SAFETY: contrat de l'appelant.
    unsafe { f(sb) }
}

/// # Safety
/// Voir [`v_overflow`].
pub unsafe fn v_uflow<E: Elem>(sb: Sb<E>) -> E::Int {
    let f = vfn!(sb, SLOT_UFLOW, unsafe extern "win64" fn(Sb<E>) -> E::Int);
    // SAFETY: contrat de l'appelant.
    unsafe { f(sb) }
}

/// # Safety
/// Voir [`v_overflow`].
pub unsafe fn v_xsgetn<E: Elem>(sb: Sb<E>, p: *mut E, n: i64) -> i64 {
    let f = vfn!(
        sb,
        SLOT_XSGETN,
        unsafe extern "win64" fn(Sb<E>, *mut E, i64) -> i64
    );
    // SAFETY: contrat de l'appelant.
    unsafe { f(sb, p, n) }
}

/// # Safety
/// Voir [`v_overflow`].
pub unsafe fn v_xsputn<E: Elem>(sb: Sb<E>, p: *const E, n: i64) -> i64 {
    let f = vfn!(
        sb,
        SLOT_XSPUTN,
        unsafe extern "win64" fn(Sb<E>, *const E, i64) -> i64
    );
    // SAFETY: contrat de l'appelant.
    unsafe { f(sb, p, n) }
}

/// `pubseekoff` : `seekoff(ret*, off, way, mode)` (retour `fpos` par pointeur caché).
///
/// # Safety
/// Voir [`v_overflow`].
pub unsafe fn v_seekoff<E: Elem>(sb: Sb<E>, off: i64, way: i32, mode: i32) -> Fpos {
    let f = vfn!(
        sb,
        SLOT_SEEKOFF,
        unsafe extern "win64" fn(Sb<E>, *mut Fpos, i64, i32, i32) -> *mut Fpos
    );
    let mut ret = Fpos::from_off(-1);
    // SAFETY: contrat de l'appelant.
    unsafe { f(sb, &mut ret, off, way, mode) };
    ret
}

/// `pubseekpos` : `seekpos(ret*, &pos, mode)` (fpos passé par copie → pointeur).
///
/// # Safety
/// Voir [`v_overflow`].
pub unsafe fn v_seekpos<E: Elem>(sb: Sb<E>, pos: Fpos, mode: i32) -> Fpos {
    let f = vfn!(
        sb,
        SLOT_SEEKPOS,
        unsafe extern "win64" fn(Sb<E>, *mut Fpos, *const Fpos, i32) -> *mut Fpos
    );
    let mut ret = Fpos::from_off(-1);
    let arg = pos;
    // SAFETY: contrat de l'appelant.
    unsafe { f(sb, &mut ret, &arg, mode) };
    ret
}

/// `pubsync`.
///
/// # Safety
/// Voir [`v_overflow`].
pub unsafe fn v_sync<E: Elem>(sb: Sb<E>) -> i32 {
    let f = vfn!(sb, SLOT_SYNC, unsafe extern "win64" fn(Sb<E>) -> i32);
    // SAFETY: contrat de l'appelant.
    unsafe { f(sb) }
}

/// # Safety
/// Voir [`v_overflow`].
pub unsafe fn v_imbue<E: Elem>(sb: Sb<E>, loc: *const Locale) {
    let f = vfn!(
        sb,
        SLOT_IMBUE,
        unsafe extern "win64" fn(Sb<E>, *const Locale)
    );
    // SAFETY: contrat de l'appelant.
    unsafe { f(sb, loc) }
}

/// # Safety
/// Voir [`v_overflow`].
pub unsafe fn v_lock<E: Elem>(sb: Sb<E>) {
    let f = vfn!(sb, SLOT_LOCK, unsafe extern "win64" fn(Sb<E>));
    // SAFETY: contrat de l'appelant.
    unsafe { f(sb) }
}

/// # Safety
/// Voir [`v_overflow`].
pub unsafe fn v_unlock<E: Elem>(sb: Sb<E>) {
    let f = vfn!(sb, SLOT_UNLOCK, unsafe extern "win64" fn(Sb<E>));
    // SAFETY: contrat de l'appelant.
    unsafe { f(sb) }
}

// ---------- membres publics non virtuels (sémantique `<streambuf>`) ----------

/// `sputc`.
///
/// # Safety
/// Voir [`v_overflow`].
pub unsafe fn sputc<E: Elem>(sb: Sb<E>, ch: E) -> E::Int {
    // SAFETY: contrat de l'appelant.
    unsafe {
        if pnavail(sb) > 0 {
            let p = pninc(sb);
            *p = ch;
            ch.to_int()
        } else {
            v_overflow(sb, ch.to_int())
        }
    }
}

/// `sgetc`.
///
/// # Safety
/// Voir [`v_overflow`].
pub unsafe fn sgetc<E: Elem>(sb: Sb<E>) -> E::Int {
    // SAFETY: contrat de l'appelant.
    unsafe {
        if gnavail(sb) > 0 {
            (*gptr(sb)).to_int()
        } else {
            v_underflow(sb)
        }
    }
}

/// `sbumpc`.
///
/// # Safety
/// Voir [`v_overflow`].
pub unsafe fn sbumpc<E: Elem>(sb: Sb<E>) -> E::Int {
    // SAFETY: contrat de l'appelant.
    unsafe {
        if gnavail(sb) > 0 {
            (*gninc(sb)).to_int()
        } else {
            v_uflow(sb)
        }
    }
}

/// `snextc`.
///
/// # Safety
/// Voir [`v_overflow`].
pub unsafe fn snextc<E: Elem>(sb: Sb<E>) -> E::Int {
    // SAFETY: contrat de l'appelant.
    unsafe {
        if gnavail(sb) > 1 {
            (*gnpreinc(sb)).to_int()
        } else if E::is_eof(sbumpc(sb)) {
            E::eof()
        } else {
            sgetc(sb)
        }
    }
}

// ---------- implémentations par défaut des virtuelles ----------

/// Corps du destructeur `~basic_streambuf` (`delete _Plocale`).
///
/// # Safety
/// `sb` construit par le constructeur exporté (ou équivalent invité).
pub unsafe fn sb_dtor_body<E: Elem + SbVtable>(sb: Sb<E>) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        (*sb).vptr = E::sb_vtable().ptr();
        delete_locale_obj((*sb).plocale);
        (*sb).plocale = ptr::null_mut();
    }
}

extern "win64" fn sb_vdtor<E: Elem + SbVtable>(this: Sb<E>, flags: u32) -> *mut c_void {
    ffi_guard!(
        CHANNEL,
        "basic_streambuf::`scalar deleting dtor'",
        this.cast(),
        {
            // SAFETY: objet construit par `sb_ctor` (ou dérivé invité).
            unsafe {
                sb_dtor_body(this);
                if flags & 1 != 0 {
                    libc::free(this.cast());
                }
            }
            this.cast()
        }
    )
}

extern "win64" fn sb_lock<E>(this: Sb<E>) {}
extern "win64" fn sb_unlock<E>(this: Sb<E>) {}

extern "win64" fn sb_overflow<E: Elem>(this: Sb<E>, c: E::Int) -> E::Int {
    E::eof()
}

extern "win64" fn sb_pbackfail<E: Elem>(this: Sb<E>, c: E::Int) -> E::Int {
    E::eof()
}

extern "win64" fn sb_showmanyc<E>(this: Sb<E>) -> i64 {
    0
}

extern "win64" fn sb_underflow<E: Elem>(this: Sb<E>) -> E::Int {
    E::eof()
}

extern "win64" fn sb_uflow<E: Elem>(this: Sb<E>) -> E::Int {
    ffi_guard!(CHANNEL, "basic_streambuf::uflow", E::eof(), {
        // SAFETY: `this` streambuf valide ; `underflow` virtuel.
        unsafe {
            if E::is_eof(v_underflow(this)) {
                E::eof()
            } else {
                (*gninc(this)).to_int()
            }
        }
    })
}

extern "win64" fn sb_xsgetn<E: Elem>(this: Sb<E>, mut dst: *mut E, count: i64) -> i64 {
    ffi_guard!(CHANNEL, "basic_streambuf::xsgetn", 0, {
        let mut left = count;
        // SAFETY: `dst` contient `count` éléments ; streambuf valide.
        unsafe {
            while left > 0 {
                let avail = gnavail(this);
                if avail > 0 {
                    let n = avail.min(left);
                    ptr::copy(gptr(this), dst, n as usize);
                    dst = dst.add(n as usize);
                    left -= n;
                    gbump(this, n as i32);
                } else {
                    let meta = v_uflow(this);
                    if E::is_eof(meta) {
                        break;
                    }
                    *dst = E::from_int(meta);
                    dst = dst.add(1);
                    left -= 1;
                }
            }
        }
        count - left.max(0)
    })
}

extern "win64" fn sb_xsputn<E: Elem>(this: Sb<E>, mut src: *const E, count: i64) -> i64 {
    ffi_guard!(CHANNEL, "basic_streambuf::xsputn", 0, {
        let mut left = count;
        // SAFETY: `src` contient `count` éléments ; streambuf valide.
        unsafe {
            while left > 0 {
                let avail = pnavail(this);
                if avail > 0 {
                    let n = avail.min(left);
                    ptr::copy(src, pptr(this), n as usize);
                    src = src.add(n as usize);
                    left -= n;
                    pbump(this, n as i32);
                } else {
                    let c = *src;
                    if E::is_eof(v_overflow(this, c.to_int())) {
                        break;
                    }
                    src = src.add(1);
                    left -= 1;
                }
            }
        }
        count - left.max(0)
    })
}

extern "win64" fn sb_seekoff<E>(
    this: Sb<E>,
    ret: *mut Fpos,
    off: i64,
    way: i32,
    mode: i32,
) -> *mut Fpos {
    if !ret.is_null() {
        // SAFETY: tampon de retour caché fourni par l'appelant.
        unsafe { ret.write(Fpos::from_off(-1)) };
    }
    ret
}

extern "win64" fn sb_seekpos<E>(
    this: Sb<E>,
    ret: *mut Fpos,
    pos: *const Fpos,
    mode: i32,
) -> *mut Fpos {
    if !ret.is_null() {
        // SAFETY: tampon de retour caché fourni par l'appelant.
        unsafe { ret.write(Fpos::from_off(-1)) };
    }
    ret
}

extern "win64" fn sb_setbuf<E>(this: Sb<E>, buf: *mut E, n: i64) -> Sb<E> {
    this
}

extern "win64" fn sb_sync<E>(this: Sb<E>) -> i32 {
    0
}

extern "win64" fn sb_imbue<E>(this: Sb<E>, loc: *const Locale) {}

/// Fournit la vtable `basic_streambuf<E>` de base.
pub trait SbVtable {
    fn sb_vtable() -> &'static VTable<15>;
}

macro_rules! sb_vtable {
    ($e:ty) => {
        VTable::new([
            sb_vdtor::<$e> as *const (),
            sb_lock::<$e> as *const (),
            sb_unlock::<$e> as *const (),
            sb_overflow::<$e> as *const (),
            sb_pbackfail::<$e> as *const (),
            sb_showmanyc::<$e> as *const (),
            sb_underflow::<$e> as *const (),
            sb_uflow::<$e> as *const (),
            sb_xsgetn::<$e> as *const (),
            sb_xsputn::<$e> as *const (),
            sb_seekoff::<$e> as *const (),
            sb_seekpos::<$e> as *const (),
            sb_setbuf::<$e> as *const (),
            sb_sync::<$e> as *const (),
            sb_imbue::<$e> as *const (),
        ])
    };
}

static SB_VT_C: VTable<15> = sb_vtable!(u8);
static SB_VT_W: VTable<15> = sb_vtable!(u16);

impl SbVtable for u8 {
    fn sb_vtable() -> &'static VTable<15> {
        &SB_VT_C
    }
}

impl SbVtable for u16 {
    fn sb_vtable() -> &'static VTable<15> {
        &SB_VT_W
    }
}

// ---------- exports ----------

extern "win64" fn sb_ctor<E: Elem + SbVtable>(this: Sb<E>) -> Sb<E> {
    ffi_guard!(CHANNEL, "basic_streambuf::basic_streambuf", this, {
        if this.is_null() {
            return this;
        }
        // SAFETY: `this` pointe vers 0x68 octets inscriptibles (objet en construction).
        unsafe {
            (*this).vptr = E::sb_vtable().ptr();
            (*this).plocale = new_locale_obj();
            sb_init(this);
        }
        this
    })
}

extern "win64" fn sb_dtor<E: Elem + SbVtable>(this: Sb<E>) {
    ffi_guard!(CHANNEL, "basic_streambuf::~basic_streambuf", (), {
        if !this.is_null() {
            // SAFETY: objet construit.
            unsafe { sb_dtor_body(this) };
        }
    })
}

extern "win64" fn sb_init_export<E: Elem>(this: Sb<E>) {
    ffi_guard!(CHANNEL, "basic_streambuf::_Init", (), {
        if !this.is_null() {
            // SAFETY: objet invité en construction.
            unsafe { sb_init(this) };
        }
    })
}

extern "win64" fn sb_gninc_export<E: Elem>(this: Sb<E>) -> *mut E {
    ffi_guard!(CHANNEL, "basic_streambuf::_Gninc", ptr::null_mut(), {
        // SAFETY: streambuf initialisé.
        unsafe { gninc(this) }
    })
}

extern "win64" fn sb_pninc_export<E: Elem>(this: Sb<E>) -> *mut E {
    ffi_guard!(CHANNEL, "basic_streambuf::_Pninc", ptr::null_mut(), {
        // SAFETY: streambuf initialisé.
        unsafe { pninc(this) }
    })
}

extern "win64" fn sb_sbumpc_export<E: Elem>(this: Sb<E>) -> E::Int {
    ffi_guard!(CHANNEL, "basic_streambuf::sbumpc", E::eof(), {
        // SAFETY: streambuf initialisé.
        unsafe { sbumpc(this) }
    })
}

extern "win64" fn sb_sputc_export<E: Elem>(this: Sb<E>, ch: u32) -> E::Int {
    ffi_guard!(CHANNEL, "basic_streambuf::sputc", E::eof(), {
        // SAFETY: streambuf initialisé.
        unsafe { sputc(this, E::from_reg(ch)) }
    })
}

extern "win64" fn sb_getloc<E: Elem>(this: Sb<E>, ret: *mut Locale) -> *mut Locale {
    ffi_guard!(CHANNEL, "basic_streambuf::getloc", ret, {
        // SAFETY: `ret` tampon de retour caché ; `_Plocale` valide.
        unsafe { locale_copy_into(ret, (*this).plocale) };
        ret
    })
}

pub fn register(m: &mut HashMap<&'static str, usize>) {
    macro_rules! reg {
        ($name:expr, $f:expr) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    // char
    reg!(
        "??0?$basic_streambuf@DU?$char_traits@D@std@@@std@@IEAA@XZ",
        sb_ctor::<u8>
    );
    reg!(
        "??1?$basic_streambuf@DU?$char_traits@D@std@@@std@@UEAA@XZ",
        sb_dtor::<u8>
    );
    reg!(
        "?_Init@?$basic_streambuf@DU?$char_traits@D@std@@@std@@IEAAXXZ",
        sb_init_export::<u8>
    );
    reg!(
        "?_Gninc@?$basic_streambuf@DU?$char_traits@D@std@@@std@@IEAAPEADXZ",
        sb_gninc_export::<u8>
    );
    reg!(
        "?_Pninc@?$basic_streambuf@DU?$char_traits@D@std@@@std@@IEAAPEADXZ",
        sb_pninc_export::<u8>
    );
    reg!(
        "?sbumpc@?$basic_streambuf@DU?$char_traits@D@std@@@std@@QEAAHXZ",
        sb_sbumpc_export::<u8>
    );
    reg!(
        "?sputc@?$basic_streambuf@DU?$char_traits@D@std@@@std@@QEAAHD@Z",
        sb_sputc_export::<u8>
    );
    reg!(
        "?getloc@?$basic_streambuf@DU?$char_traits@D@std@@@std@@QEBA?AVlocale@2@XZ",
        sb_getloc::<u8>
    );
    reg!(
        "?_Lock@?$basic_streambuf@DU?$char_traits@D@std@@@std@@UEAAXXZ",
        sb_lock::<u8>
    );
    reg!(
        "?_Unlock@?$basic_streambuf@DU?$char_traits@D@std@@@std@@UEAAXXZ",
        sb_unlock::<u8>
    );
    reg!(
        "?imbue@?$basic_streambuf@DU?$char_traits@D@std@@@std@@MEAAXAEBVlocale@2@@Z",
        sb_imbue::<u8>
    );
    reg!(
        "?setbuf@?$basic_streambuf@DU?$char_traits@D@std@@@std@@MEAAPEAV12@PEAD_J@Z",
        sb_setbuf::<u8>
    );
    reg!(
        "?showmanyc@?$basic_streambuf@DU?$char_traits@D@std@@@std@@MEAA_JXZ",
        sb_showmanyc::<u8>
    );
    reg!(
        "?sync@?$basic_streambuf@DU?$char_traits@D@std@@@std@@MEAAHXZ",
        sb_sync::<u8>
    );
    reg!(
        "?uflow@?$basic_streambuf@DU?$char_traits@D@std@@@std@@MEAAHXZ",
        sb_uflow::<u8>
    );
    reg!(
        "?xsgetn@?$basic_streambuf@DU?$char_traits@D@std@@@std@@MEAA_JPEAD_J@Z",
        sb_xsgetn::<u8>
    );
    reg!(
        "?xsputn@?$basic_streambuf@DU?$char_traits@D@std@@@std@@MEAA_JPEBD_J@Z",
        sb_xsputn::<u8>
    );
    reg!(
        "?overflow@?$basic_streambuf@DU?$char_traits@D@std@@@std@@MEAAHH@Z",
        sb_overflow::<u8>
    );
    reg!(
        "?underflow@?$basic_streambuf@DU?$char_traits@D@std@@@std@@MEAAHXZ",
        sb_underflow::<u8>
    );
    reg!(
        "?pbackfail@?$basic_streambuf@DU?$char_traits@D@std@@@std@@MEAAHH@Z",
        sb_pbackfail::<u8>
    );
    m.insert(
        "??_7?$basic_streambuf@DU?$char_traits@D@std@@@std@@6B@",
        SB_VT_C.addr(),
    );
    // wchar_t
    reg!(
        "??0?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@IEAA@XZ",
        sb_ctor::<u16>
    );
    reg!(
        "??1?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@UEAA@XZ",
        sb_dtor::<u16>
    );
    reg!(
        "?_Pninc@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@IEAAPEA_WXZ",
        sb_pninc_export::<u16>
    );
    reg!(
        "?sputc@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@QEAAG_W@Z",
        sb_sputc_export::<u16>
    );
    reg!(
        "?_Lock@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@UEAAXXZ",
        sb_lock::<u16>
    );
    reg!(
        "?_Unlock@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@UEAAXXZ",
        sb_unlock::<u16>
    );
    reg!(
        "?imbue@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@MEAAXAEBVlocale@2@@Z",
        sb_imbue::<u16>
    );
    reg!(
        "?setbuf@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@MEAAPEAV12@PEA_W_J@Z",
        sb_setbuf::<u16>
    );
    reg!(
        "?showmanyc@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@MEAA_JXZ",
        sb_showmanyc::<u16>
    );
    reg!(
        "?sync@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@MEAAHXZ",
        sb_sync::<u16>
    );
    reg!(
        "?uflow@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@MEAAGXZ",
        sb_uflow::<u16>
    );
    reg!(
        "?xsgetn@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@MEAA_JPEA_W_J@Z",
        sb_xsgetn::<u16>
    );
    reg!(
        "?xsputn@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@MEAA_JPEB_W_J@Z",
        sb_xsputn::<u16>
    );
    m.insert(
        "??_7?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@6B@",
        SB_VT_W.addr(),
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Construit un `basic_streambuf<E>` alloué sur le tas via le constructeur exporté.
    pub fn new_sb<E: Elem + SbVtable>() -> Sb<E> {
        // SAFETY: zone de 0x68 octets alignée (`malloc`).
        let p = unsafe { libc::calloc(1, size_of::<StreamBuf<E>>()) }.cast::<StreamBuf<E>>();
        sb_ctor::<E>(p)
    }

    pub fn free_sb<E: Elem + SbVtable>(sb: Sb<E>) {
        // SAFETY: appel virtuel du destructeur de suppression (slot 0).
        unsafe {
            let d: unsafe extern "win64" fn(Sb<E>, u32) -> *mut c_void =
                core::mem::transmute(vslot(sb.cast(), SLOT_DTOR));
            d(sb, 1);
        }
    }

    thread_local! {
        static SINK: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    extern "win64" fn capture_overflow(this: Sb<u8>, c: i32) -> i32 {
        if c == -1 {
            return 0;
        }
        SINK.with(|s| s.borrow_mut().push(c as u8));
        c
    }

    /// Vtable « dérivée » (comme une classe de l'invité) qui surcharge `overflow`.
    pub fn derived_vtable() -> &'static VTable<15> {
        let mut slots = SB_VT_C.slots;
        slots[SLOT_OVERFLOW] = capture_overflow as *const ();
        Box::leak(Box::new(VTable::new(slots)))
    }

    #[test]
    fn streambuf_with_memory_buffer() {
        let sb = new_sb::<u8>();
        let mut out = [0u8; 4];
        let mut input = *b"xyz";
        // SAFETY: tampons locaux vivants pendant le test.
        unsafe {
            assert!(!(*sb).plocale.is_null());
            assert_eq!(sputc(sb, b'a'), -1); // pas de tampon : overflow par défaut -> eof
            setp(sb, out.as_mut_ptr(), out.as_mut_ptr().add(4));
            assert_eq!(sb_sputc_export::<u8>(sb, u32::from(b'h')), i32::from(b'h'));
            assert_eq!(sb_xsputn::<u8>(sb, b"ello".as_ptr(), 4), 3);
            assert_eq!(&out, b"hell");
            assert_eq!(pnavail(sb), 0);
            setg(
                sb,
                input.as_mut_ptr(),
                input.as_mut_ptr(),
                input.as_mut_ptr().add(3),
            );
            assert_eq!(sb_sbumpc_export::<u8>(sb), i32::from(b'x'));
            assert_eq!(snextc(sb), i32::from(b'z'));
            let mut dst = [0u8; 4];
            assert_eq!(sb_xsgetn::<u8>(sb, dst.as_mut_ptr(), 4), 1);
            assert_eq!(dst[0], b'z');
            assert_eq!(sb_uflow::<u8>(sb), -1);
            let pos = v_seekoff(sb, 0, 1, 1);
            assert_eq!(pos.off(), -1);
            let mut loc = core::mem::MaybeUninit::<Locale>::uninit();
            sb_getloc(sb, loc.as_mut_ptr());
            assert_eq!(loc.assume_init_ref().ptr, (*(*sb).plocale).ptr);
            super::super::locale::locale_release(loc.as_mut_ptr());
        }
        free_sb(sb);
    }

    #[test]
    fn virtual_dispatch_reaches_derived_overflow() {
        SINK.with(|s| s.borrow_mut().clear());
        let sb = new_sb::<u8>();
        // SAFETY: remplacement du vptr comme le ferait le constructeur d'une classe dérivée.
        unsafe {
            (*sb).vptr = derived_vtable().ptr();
            assert_eq!(sputc(sb, b'Q'), i32::from(b'Q'));
            // xsputn par défaut (sans tampon) -> overflow dérivé pour chaque caractère
            assert_eq!(sb_xsputn::<u8>(sb, b"rst".as_ptr(), 3), 3);
        }
        SINK.with(|s| assert_eq!(s.borrow().as_slice(), b"Qrst"));
        sb_dtor::<u8>(sb);
        // SAFETY: objet détruit, libération brute.
        unsafe { libc::free(sb.cast()) };
    }

    #[test]
    fn wide_streambuf_put_and_eof() {
        let sb = new_sb::<u16>();
        let mut out = [0u16; 2];
        // SAFETY: tampon local.
        unsafe {
            assert_eq!(sb_sputc_export::<u16>(sb, 0x263A), 0xFFFF);
            setp(sb, out.as_mut_ptr(), out.as_mut_ptr().add(2));
            assert_eq!(sb_sputc_export::<u16>(sb, 0x263A), 0x263A);
            let p = sb_pninc_export::<u16>(sb);
            assert_eq!(p, out.as_mut_ptr().add(1));
            assert_eq!(sb_uflow::<u16>(sb), 0xFFFF);
        }
        assert_eq!(out[0], 0x263A);
        free_sb(sb);
    }
}
