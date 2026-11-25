//! `ios_base`, `basic_ios<E>`, `basic_istream<char>`, `basic_ostream<E>`, `basic_iostream<E>`.
//!
//! Héritage virtuel MSVC x64 : `basic_istream`/`basic_ostream` héritent virtuellement de
//! `basic_ios`. Chaque sous-objet de flux commence par un vbptr vers une vbtable `int[2]`
//! (`{0, offset du sous-objet basic_ios}`), et le vptr unique vit dans le sous-objet
//! `basic_ios` (aucune nouvelle virtuelle n'est introduite, pas de vtordisp — cf. layouts Wine).
//! Les constructeurs prennent un dernier argument caché « most-derived » (non nul : construire
//! la base virtuelle et poser les vbptr) ; les `??1` reçoivent `this` = sous-objet `basic_ios`.

use core::ffi::c_void;
use core::ptr;
use std::collections::HashMap;

use parking_lot::Mutex;
use sb_common::ffi_guard;

use super::locale::{
    delete_locale_obj, locale_assign, locale_copy_into, locale_init_imp, new_locale_obj,
};
use super::streambuf::{
    SbVtable, sgetc, snextc, sputc, v_imbue, v_lock, v_seekoff, v_seekpos, v_sync, v_unlock,
    v_xsgetn, v_xsputn,
};
use super::*;

// ---------- vtables / vbtables ----------

static IOS_BASE_VT: VTable<1> = VTable::new([ios_base_vdtor as *const ()]);

/// vbtables (`??_8...@7B@`) : `{offset vbptr -> objet, offset vbptr -> basic_ios}`.
static ISTREAM_VBT: [i32; 2] = [0, ISTREAM_VBASE as i32];
static OSTREAM_VBT: [i32; 2] = [0, OSTREAM_VBASE as i32];
static IOSTREAM_VBT_I: [i32; 2] = [0, IOSTREAM_VBASE as i32];
static IOSTREAM_VBT_O: [i32; 2] = [0, (IOSTREAM_VBASE - IOSTREAM_OSTREAM_OFF) as i32];

pub trait IosVtables: Elem + SbVtable {
    fn ios_vt() -> &'static VTable<1>;
    fn istream_vt() -> &'static VTable<1>;
    fn ostream_vt() -> &'static VTable<1>;
    fn iostream_vt() -> &'static VTable<1>;
}

macro_rules! ios_vtables {
    ($e:ty, $ios:ident, $is:ident, $os:ident, $ios2:ident) => {
        static $ios: VTable<1> = VTable::new([basic_ios_vdtor::<$e> as *const ()]);
        static $is: VTable<1> = VTable::new([istream_vdtor::<$e> as *const ()]);
        static $os: VTable<1> = VTable::new([ostream_vdtor::<$e> as *const ()]);
        static $ios2: VTable<1> = VTable::new([iostream_vdtor::<$e> as *const ()]);
        impl IosVtables for $e {
            fn ios_vt() -> &'static VTable<1> {
                &$ios
            }
            fn istream_vt() -> &'static VTable<1> {
                &$is
            }
            fn ostream_vt() -> &'static VTable<1> {
                &$os
            }
            fn iostream_vt() -> &'static VTable<1> {
                &$ios2
            }
        }
    };
}

ios_vtables!(u8, IOS_VT_C, ISTREAM_VT_C, OSTREAM_VT_C, IOSTREAM_VT_C);
ios_vtables!(u16, IOS_VT_W, ISTREAM_VT_W, OSTREAM_VT_W, IOSTREAM_VT_W);

// ---------- ios_base ----------

/// Table des flux standard (`_Addstd`) : `stdstr[8]`, `stdopens[8]`.
static STDSTR: Mutex<([usize; 8], [i32; 8])> = Mutex::new(([0; 8], [0; 8]));

/// `ios_base::clear(state, reraise)`.
///
/// # Safety
/// `ios` valide.
pub unsafe fn ios_clear(ios: *mut IosBase, state: i32, reraise: bool) {
    // SAFETY: contrat de l'appelant.
    let hit = unsafe {
        (*ios).mystate = state & STATMASK;
        (*ios).mystate & (*ios).except
    };
    if hit == 0 {
        return;
    }
    if reraise {
        fatal_throw("<rethrow>", "exception relancée par ios_base::clear");
    }
    let msg = if hit & BADBIT != 0 {
        "ios_base::badbit set"
    } else if hit & FAILBIT != 0 {
        "ios_base::failbit set"
    } else {
        "ios_base::eofbit set"
    };
    fatal_throw("std::ios_base::failure", msg);
}

/// # Safety
/// `ios` valide.
unsafe fn ios_callfns(ios: *mut IosBase, ev: i32) {
    // SAFETY: liste de callbacks enregistrée par `register_callback` (invité).
    unsafe {
        let mut p = (*ios).calls;
        while !p.is_null() {
            if let Some(f) = (*p).pfn {
                f(ev, ios, (*p).index);
            }
            p = (*p).next;
        }
    }
}

/// `ios_base::_Tidy()`.
///
/// # Safety
/// `ios` valide.
unsafe fn ios_tidy(ios: *mut IosBase) {
    // SAFETY: contrat de l'appelant ; nœuds alloués par l'allocateur CRT (`malloc`).
    unsafe {
        ios_callfns(ios, ERASE_EVENT);
        let mut a = (*ios).arr;
        while !a.is_null() {
            let n = (*a).next;
            libc::free(a.cast());
            a = n;
        }
        (*ios).arr = ptr::null_mut();
        let mut c = (*ios).calls;
        while !c.is_null() {
            let n = (*c).next;
            libc::free(c.cast());
            c = n;
        }
        (*ios).calls = ptr::null_mut();
    }
}

/// `ios_base::_Init()`.
///
/// # Safety
/// `ios` valide.
unsafe fn ios_init(ios: *mut IosBase) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        (*ios).ploc = ptr::null_mut();
        (*ios).stdstr = 0;
        (*ios).except = GOODBIT;
        (*ios).fmtfl = SKIPWS | DEC;
        (*ios).prec = 6;
        (*ios).wide = 0;
        (*ios).arr = ptr::null_mut();
        (*ios).calls = ptr::null_mut();
        ios_clear(ios, GOODBIT, false);
        (*ios).ploc = new_locale_obj();
    }
}

/// `ios_base::_Addstd(ios_base*)`.
///
/// # Safety
/// `ios` valide.
unsafe fn ios_addstd(ios: *mut IosBase) {
    let mut t = STDSTR.lock();
    let me = ios as usize;
    let mut i = 1;
    while i < 8 && t.0[i] != 0 && t.0[i] != me {
        i += 1;
    }
    if i >= 8 {
        return;
    }
    // SAFETY: contrat de l'appelant.
    unsafe { (*ios).stdstr = i };
    t.0[i] = me;
    t.1[i] += 1;
}

extern "win64" fn ios_base_dtor_static(ios: *mut IosBase) {
    ffi_guard!(CHANNEL, "ios_base::_Ios_base_dtor", (), {
        if ios.is_null() {
            return;
        }
        // SAFETY: `ios` objet `ios_base` valide.
        unsafe {
            let idx = (*ios).stdstr;
            if idx > 0 && idx < 8 {
                let mut t = STDSTR.lock();
                t.1[idx] -= 1;
                if t.1[idx] > 0 {
                    return;
                }
            }
            ios_tidy(ios);
            delete_locale_obj((*ios).ploc);
            (*ios).ploc = ptr::null_mut();
        }
    })
}

extern "win64" fn ios_base_ctor(this: *mut IosBase) -> *mut IosBase {
    ffi_guard!(CHANNEL, "ios_base::ios_base", this, {
        if this.is_null() {
            return this;
        }
        // SAFETY: `this` pointe vers 0x48 octets inscriptibles.
        unsafe {
            this.write(IosBase {
                vptr: IOS_BASE_VT.ptr(),
                stdstr: 0,
                mystate: 0,
                except: GOODBIT,
                fmtfl: 0,
                prec: 0,
                wide: 0,
                arr: ptr::null_mut(),
                calls: ptr::null_mut(),
                ploc: ptr::null_mut(),
            });
        }
        this
    })
}

extern "win64" fn ios_base_dtor(this: *mut IosBase) {
    ffi_guard!(CHANNEL, "ios_base::~ios_base", (), {
        if this.is_null() {
            return;
        }
        // SAFETY: objet valide.
        unsafe { (*this).vptr = IOS_BASE_VT.ptr() };
        ios_base_dtor_static(this);
    })
}

extern "win64" fn ios_base_vdtor(this: *mut IosBase, flags: u32) -> *mut c_void {
    ios_base_dtor(this);
    if flags & 1 != 0 {
        // SAFETY: objet alloué par `operator new` (malloc).
        unsafe { libc::free(this.cast()) };
    }
    this.cast()
}

extern "win64" fn ios_base_good(this: *const IosBase) -> bool {
    // SAFETY: `this` objet valide.
    !this.is_null() && unsafe { (*this).mystate } == GOODBIT
}

extern "win64" fn ios_base_getloc(this: *const IosBase, ret: *mut Locale) -> *mut Locale {
    ffi_guard!(CHANNEL, "ios_base::getloc", ret, {
        // SAFETY: `ret` tampon caché ; `_Ploc` valide ou nul (flux non initialisé).
        unsafe {
            let ploc = (*this).ploc;
            if ploc.is_null() {
                ret.write(Locale::new(locale_init_imp(true)));
            } else {
                locale_copy_into(ret, ploc);
            }
        }
        ret
    })
}

// ---------- basic_ios<E> ----------

/// `basic_ios::clear(state, reraise)` (badbit forcé sans streambuf).
///
/// # Safety
/// `ios` valide.
pub unsafe fn bios_clear<E>(ios: *mut BasicIos<E>, state: i32, reraise: bool) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        let extra = if (*ios).mystrbuf.is_null() { BADBIT } else { 0 };
        ios_clear(ios.cast(), state | extra, reraise);
    }
}

/// `basic_ios::setstate(state, reraise)`.
///
/// # Safety
/// `ios` valide.
pub unsafe fn bios_setstate<E>(ios: *mut BasicIos<E>, state: i32, reraise: bool) {
    // SAFETY: contrat de l'appelant.
    unsafe { bios_clear(ios, (*ios).base.mystate | state, reraise) };
}

/// `basic_ios::init(sb, isstd)`.
///
/// # Safety
/// `ios` valide.
pub unsafe fn bios_init<E: Elem>(ios: *mut BasicIos<E>, sb: *mut StreamBuf<E>, isstd: bool) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        ios_init(ios.cast());
        (*ios).mystrbuf = sb;
        (*ios).tiestr = ptr::null_mut();
        (*ios).fillch = E::from_ascii(b' ');
        if sb.is_null() {
            bios_setstate(ios, BADBIT, false);
        }
        if isstd {
            ios_addstd(ios.cast());
        }
    }
}

/// # Safety
/// `ios` pointe vers 0x60 octets inscriptibles.
unsafe fn bios_ctor_impl<E: IosVtables>(ios: *mut BasicIos<E>) {
    ios_base_ctor(ios.cast());
    // SAFETY: contrat de l'appelant.
    unsafe {
        (*ios).mystrbuf = ptr::null_mut();
        (*ios).tiestr = ptr::null_mut();
        (*ios).fillch = E::default();
        (*ios).base.vptr = E::ios_vt().ptr();
    }
}

extern "win64" fn bios_ctor<E: IosVtables>(this: *mut BasicIos<E>) -> *mut BasicIos<E> {
    ffi_guard!(CHANNEL, "basic_ios::basic_ios", this, {
        if !this.is_null() {
            // SAFETY: objet en construction.
            unsafe { bios_ctor_impl(this) };
        }
        this
    })
}

extern "win64" fn bios_dtor<E: IosVtables>(this: *mut BasicIos<E>) {
    ffi_guard!(CHANNEL, "basic_ios::~basic_ios", (), {
        if this.is_null() {
            return;
        }
        // SAFETY: objet valide.
        unsafe { (*this).base.vptr = E::ios_vt().ptr() };
        ios_base_dtor(this.cast());
    })
}

extern "win64" fn basic_ios_vdtor<E: IosVtables>(
    this: *mut BasicIos<E>,
    flags: u32,
) -> *mut c_void {
    bios_dtor(this);
    if flags & 1 != 0 {
        // SAFETY: objet alloué par `operator new` (malloc).
        unsafe { libc::free(this.cast()) };
    }
    this.cast()
}

extern "win64" fn bios_clear_export<E: Elem>(this: *mut BasicIos<E>, state: i32, reraise: u32) {
    ffi_guard!(CHANNEL, "basic_ios::clear", (), {
        // SAFETY: objet valide.
        unsafe { bios_clear(this, state, reraise as u8 != 0) };
    })
}

extern "win64" fn bios_setstate_export<E: Elem>(this: *mut BasicIos<E>, state: i32, reraise: u32) {
    ffi_guard!(CHANNEL, "basic_ios::setstate", (), {
        // SAFETY: objet valide.
        unsafe { bios_setstate(this, state, reraise as u8 != 0) };
    })
}

extern "win64" fn bios_init_export<E: Elem>(
    this: *mut BasicIos<E>,
    sb: *mut StreamBuf<E>,
    isstd: u32,
) {
    ffi_guard!(CHANNEL, "basic_ios::init", (), {
        // SAFETY: objet valide.
        unsafe { bios_init(this, sb, isstd as u8 != 0) };
    })
}

extern "win64" fn bios_imbue<E: Elem>(
    this: *mut BasicIos<E>,
    ret: *mut Locale,
    loc: *const Locale,
) -> *mut Locale {
    ffi_guard!(CHANNEL, "basic_ios::imbue", ret, {
        // SAFETY: objet valide, `ret` tampon caché, `loc` locale valide.
        unsafe {
            let ploc = (*this).base.ploc;
            if ploc.is_null() {
                (*this).base.ploc = new_locale_obj();
            }
            let ploc = (*this).base.ploc;
            locale_copy_into(ret, ploc);
            locale_assign(ploc, loc);
            ios_callfns(this.cast(), IMBUE_EVENT);
            let sb = (*this).mystrbuf;
            if !sb.is_null() {
                v_imbue(sb, loc);
                if !(*sb).plocale.is_null() {
                    locale_assign((*sb).plocale, loc);
                }
            }
        }
        ret
    })
}

// ---------- accès flux -> basic_ios via vbptr ----------

/// Sous-objet `basic_ios` d'un `basic_istream`/`basic_ostream` (lecture de la vbtable).
///
/// # Safety
/// `s` pointe vers un sous-objet de flux dont le vbptr est initialisé.
pub unsafe fn ios_of<E>(s: *mut c_void) -> *mut BasicIos<E> {
    // SAFETY: contrat de l'appelant ; vbtable `{0, off}`.
    unsafe {
        let vbt = *(s as *const *const i32);
        s.cast::<u8>().offset(*vbt.add(1) as isize).cast()
    }
}

// ---------- basic_istream ----------

/// `basic_istream::_Ipfx(noskip)`.
///
/// # Safety
/// `this` flux d'entrée valide.
pub unsafe fn ipfx<E: Elem + IosVtables>(this: *mut c_void, noskip: bool) -> bool {
    // SAFETY: contrat de l'appelant.
    unsafe {
        let ios = ios_of::<E>(this);
        if (*ios).base.mystate == GOODBIT {
            let tie = (*ios).tiestr;
            if !tie.is_null() {
                ostream_flush_impl::<E>(tie);
            }
            if !noskip && (*ios).base.fmtfl & SKIPWS != 0 {
                let sb = (*ios).mystrbuf;
                let mut st = GOODBIT;
                let mut meta = sgetc(sb);
                loop {
                    if E::is_eof(meta) {
                        st |= EOFBIT;
                        break;
                    }
                    let c = E::from_int(meta).code();
                    if super::locale::classify_wide(c) & 0x8 == 0 {
                        break;
                    }
                    meta = snextc(sb);
                }
                bios_setstate(ios, st, false);
            }
            if (*ios).base.mystate == GOODBIT {
                return true;
            }
        }
        bios_setstate(ios, FAILBIT, false);
        false
    }
}

/// Exécute `f(ok)` sous un `basic_istream::sentry`.
///
/// # Safety
/// `this` flux d'entrée valide.
unsafe fn with_isentry<E: IosVtables, R>(
    this: *mut c_void,
    noskip: bool,
    f: impl FnOnce(bool) -> R,
) -> R {
    // SAFETY: contrat de l'appelant.
    unsafe {
        let sb = (*ios_of::<E>(this)).mystrbuf;
        if !sb.is_null() {
            v_lock(sb);
        }
        let ok = ipfx::<E>(this, noskip);
        let r = f(ok);
        if !sb.is_null() {
            v_unlock(sb);
        }
        r
    }
}

/// # Safety
/// `this` pointe vers 0x70 octets (ou un sous-objet istream d'iostream).
unsafe fn istream_ctor_impl<E: IosVtables>(
    this: *mut c_void,
    sb: *mut StreamBuf<E>,
    isstd: bool,
    virt: bool,
) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        if virt {
            *(this as *mut *const i32) = ISTREAM_VBT.as_ptr();
            bios_ctor_impl::<E>(this.cast::<u8>().add(ISTREAM_VBASE).cast());
        }
        let ios = ios_of::<E>(this);
        (*ios).base.vptr = E::istream_vt().ptr();
        *(this.cast::<u8>().add(8) as *mut i64) = 0; // _Chcount
        bios_init(ios, sb, isstd);
    }
}

extern "win64" fn istream_ctor<E: IosVtables>(
    this: *mut c_void,
    sb: *mut StreamBuf<E>,
    isstd: u32,
    virt: u32,
) -> *mut c_void {
    ffi_guard!(CHANNEL, "basic_istream::basic_istream", this, {
        if !this.is_null() {
            // SAFETY: objet en construction.
            unsafe { istream_ctor_impl(this, sb, isstd as u8 != 0, virt != 0) };
        }
        this
    })
}

extern "win64" fn istream_dtor<E: IosVtables>(ios: *mut BasicIos<E>) {
    // `this` = sous-objet basic_ios ; la base virtuelle n'est pas détruite ici.
    if !ios.is_null() {
        // SAFETY: objet valide.
        unsafe { (*ios).base.vptr = E::istream_vt().ptr() };
    }
}

extern "win64" fn istream_vdtor<E: IosVtables>(ios: *mut BasicIos<E>, flags: u32) -> *mut c_void {
    let full = ios.cast::<u8>().wrapping_sub(ISTREAM_VBASE);
    istream_dtor(ios);
    bios_dtor(ios);
    if flags & 1 != 0 {
        // SAFETY: objet complet alloué par `operator new`.
        unsafe { libc::free(full.cast()) };
    }
    full.cast()
}

extern "win64" fn istream_ipfx(this: *mut c_void, noskip: u32) -> bool {
    ffi_guard!(CHANNEL, "basic_istream::_Ipfx", false, {
        // SAFETY: flux valide.
        unsafe { ipfx::<u8>(this, noskip as u8 != 0) }
    })
}

/// Extraction d'entier signé 32 bits (`num_get::do_get(long&)`, `long` = 32 bits sous Windows).
///
/// # Safety
/// `sb` streambuf valide.
unsafe fn get_long<E: Elem>(sb: *mut StreamBuf<E>, fmtfl: i32, st: &mut i32) -> i32 {
    let base_f = fmtfl & BASEFIELD;
    let mut base: u32 = if base_f == OCT {
        8
    } else if base_f == HEX {
        16
    } else if base_f == 0 {
        0
    } else {
        10
    };
    // SAFETY: contrat de l'appelant.
    unsafe {
        let peek = |m: E::Int| -> Option<u8> {
            if E::is_eof(m) {
                None
            } else {
                u8::try_from(E::from_int(m).code()).ok()
            }
        };
        let mut meta = sgetc(sb);
        let mut neg = false;
        if let Some(c @ (b'+' | b'-')) = peek(meta) {
            neg = c == b'-';
            meta = snextc(sb);
        }
        let mut any = false;
        if (base == 0 || base == 16) && peek(meta) == Some(b'0') {
            any = true;
            meta = snextc(sb);
            if matches!(peek(meta), Some(b'x' | b'X')) {
                base = 16;
                any = false;
                meta = snextc(sb);
            } else if base == 0 {
                base = 8;
            }
        }
        if base == 0 {
            base = 10;
        }
        let mut val: u64 = 0;
        let mut overflow = false;
        while let Some(d) = peek(meta).and_then(|c| (c as char).to_digit(base)) {
            any = true;
            val = val * u64::from(base) + u64::from(d);
            if val > 0x1_0000_0000 {
                overflow = true;
                val = 0x1_0000_0000;
            }
            meta = snextc(sb);
        }
        if E::is_eof(meta) {
            *st |= EOFBIT;
        }
        if !any {
            *st |= FAILBIT;
            return 0;
        }
        let limit_pos = i32::MAX as u64;
        let limit_neg = (i32::MAX as u64) + 1;
        if neg {
            if overflow || val > limit_neg {
                *st |= FAILBIT;
                i32::MIN
            } else {
                (val as i64).wrapping_neg() as i32
            }
        } else if overflow || val > limit_pos {
            *st |= FAILBIT;
            i32::MAX
        } else {
            val as i32
        }
    }
}

extern "win64" fn istream_extract_int(this: *mut c_void, out: *mut i32) -> *mut c_void {
    ffi_guard!(CHANNEL, "basic_istream::operator>>(int&)", this, {
        // SAFETY: flux valide, `out` référence invité.
        unsafe {
            let ios = ios_of::<u8>(this);
            let mut st = GOODBIT;
            with_isentry::<u8, _>(this, false, |ok| {
                if ok {
                    let v = get_long((*ios).mystrbuf, (*ios).base.fmtfl, &mut st);
                    if !out.is_null() {
                        *out = v;
                    }
                }
            });
            bios_setstate(ios, st, false);
        }
        this
    })
}

extern "win64" fn istream_read(this: *mut c_void, buf: *mut u8, count: i64) -> *mut c_void {
    ffi_guard!(CHANNEL, "basic_istream::read", this, {
        // SAFETY: flux valide ; `buf` contient `count` octets.
        unsafe {
            let ios = ios_of::<u8>(this);
            let chcount = this.cast::<u8>().add(8) as *mut i64;
            *chcount = 0;
            let mut st = GOODBIT;
            with_isentry::<u8, _>(this, true, |ok| {
                if ok && count > 0 {
                    let n = v_xsgetn((*ios).mystrbuf, buf, count);
                    *chcount += n;
                    if n != count {
                        st |= EOFBIT | FAILBIT;
                    }
                }
            });
            bios_setstate(ios, st, false);
        }
        this
    })
}

const fn failed(state: i32) -> bool {
    state & (BADBIT | FAILBIT) != 0
}

extern "win64" fn istream_seekg_off(this: *mut c_void, off: i64, way: i32) -> *mut c_void {
    ffi_guard!(CHANNEL, "basic_istream::seekg(off, dir)", this, {
        // SAFETY: flux valide.
        unsafe {
            let ios = ios_of::<u8>(this);
            bios_clear(ios, (*ios).base.mystate & !EOFBIT, false);
            let mut st = GOODBIT;
            with_isentry::<u8, _>(this, true, |_| {
                if !failed((*ios).base.mystate)
                    && v_seekoff((*ios).mystrbuf, off, way, MODE_IN).off() == -1
                {
                    st |= FAILBIT;
                }
            });
            bios_setstate(ios, st, false);
        }
        this
    })
}

extern "win64" fn istream_seekg_pos(this: *mut c_void, pos: *const Fpos) -> *mut c_void {
    ffi_guard!(CHANNEL, "basic_istream::seekg(pos)", this, {
        // SAFETY: flux valide ; `pos` copie temporaire de l'appelant.
        unsafe {
            let ios = ios_of::<u8>(this);
            let p = if pos.is_null() {
                Fpos::from_off(-1)
            } else {
                *pos
            };
            bios_clear(ios, (*ios).base.mystate & !EOFBIT, false);
            let mut st = GOODBIT;
            with_isentry::<u8, _>(this, true, |_| {
                if !failed((*ios).base.mystate)
                    && v_seekpos((*ios).mystrbuf, p, MODE_IN).off() == -1
                {
                    st |= FAILBIT;
                }
            });
            bios_setstate(ios, st, false);
        }
        this
    })
}

extern "win64" fn istream_tellg(this: *mut c_void, ret: *mut Fpos) -> *mut Fpos {
    ffi_guard!(CHANNEL, "basic_istream::tellg", ret, {
        // SAFETY: flux valide ; `ret` tampon caché.
        unsafe {
            let ios = ios_of::<u8>(this);
            let pos = with_isentry::<u8, _>(this, true, |_| {
                if failed((*ios).base.mystate) {
                    Fpos::from_off(-1)
                } else {
                    v_seekoff((*ios).mystrbuf, 0, SEEK_CUR, MODE_IN)
                }
            });
            ret.write(pos);
        }
        ret
    })
}

// ---------- basic_ostream ----------

/// `basic_ostream::_Osfx()` (noexcept : jamais d'exception propagée).
///
/// # Safety
/// `this` flux de sortie valide.
unsafe fn osfx<E: Elem>(this: *mut c_void) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        let ios = ios_of::<E>(this);
        if (*ios).base.mystate == GOODBIT
            && (*ios).base.fmtfl & UNITBUF != 0
            && !(*ios).mystrbuf.is_null()
            && v_sync((*ios).mystrbuf) == -1
        {
            (*ios).base.mystate |= BADBIT;
        }
    }
}

/// Exécute `f(ok)` sous un `basic_ostream::sentry`.
///
/// # Safety
/// `this` flux de sortie valide.
unsafe fn with_osentry<E: IosVtables, R>(this: *mut c_void, f: impl FnOnce(bool) -> R) -> R {
    // SAFETY: contrat de l'appelant.
    unsafe {
        let ios = ios_of::<E>(this);
        let sb = (*ios).mystrbuf;
        if !sb.is_null() {
            v_lock(sb);
        }
        let tie = (*ios).tiestr;
        if (*ios).base.mystate == GOODBIT && !tie.is_null() && tie != this {
            ostream_flush_impl::<E>(tie);
        }
        let ok = (*ios).base.mystate == GOODBIT;
        let r = f(ok);
        // `uncaught_exceptions()` est toujours 0 (pas de propagation C++) : _Osfx systématique.
        osfx::<E>(this);
        if !sb.is_null() {
            v_unlock(sb);
        }
        r
    }
}

/// `basic_ostream::flush()`.
///
/// # Safety
/// `this` flux de sortie valide.
unsafe fn ostream_flush_impl<E: IosVtables>(this: *mut c_void) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        let ios = ios_of::<E>(this);
        let sb = (*ios).mystrbuf;
        if sb.is_null() {
            return;
        }
        with_osentry::<E, _>(this, |ok| {
            if ok {
                let st = if v_sync(sb) == -1 { BADBIT } else { GOODBIT };
                bios_setstate(ios, st, false);
            }
        });
    }
}

/// # Safety
/// `this` pointe vers un objet ostream (ou sous-objet) inscriptible.
unsafe fn ostream_ctor_impl<E: IosVtables>(
    this: *mut c_void,
    sb: *mut StreamBuf<E>,
    isstd: bool,
    virt: bool,
) {
    // SAFETY: contrat de l'appelant.
    unsafe {
        if virt {
            *(this as *mut *const i32) = OSTREAM_VBT.as_ptr();
            bios_ctor_impl::<E>(this.cast::<u8>().add(OSTREAM_VBASE).cast());
        }
        let ios = ios_of::<E>(this);
        (*ios).base.vptr = E::ostream_vt().ptr();
        bios_init(ios, sb, isstd);
    }
}

extern "win64" fn ostream_ctor<E: IosVtables>(
    this: *mut c_void,
    sb: *mut StreamBuf<E>,
    isstd: u32,
    virt: u32,
) -> *mut c_void {
    ffi_guard!(CHANNEL, "basic_ostream::basic_ostream", this, {
        if !this.is_null() {
            // SAFETY: objet en construction.
            unsafe { ostream_ctor_impl(this, sb, isstd as u8 != 0, virt != 0) };
        }
        this
    })
}

extern "win64" fn ostream_dtor<E: IosVtables>(ios: *mut BasicIos<E>) {
    if !ios.is_null() {
        // SAFETY: objet valide.
        unsafe { (*ios).base.vptr = E::ostream_vt().ptr() };
    }
}

extern "win64" fn ostream_vdtor<E: IosVtables>(ios: *mut BasicIos<E>, flags: u32) -> *mut c_void {
    let full = ios.cast::<u8>().wrapping_sub(OSTREAM_VBASE);
    ostream_dtor(ios);
    bios_dtor(ios);
    if flags & 1 != 0 {
        // SAFETY: objet complet alloué par `operator new`.
        unsafe { libc::free(full.cast()) };
    }
    full.cast()
}

extern "win64" fn ostream_flush<E: IosVtables>(this: *mut c_void) -> *mut c_void {
    ffi_guard!(CHANNEL, "basic_ostream::flush", this, {
        // SAFETY: flux valide.
        unsafe { ostream_flush_impl::<E>(this) };
        this
    })
}

extern "win64" fn ostream_osfx<E: IosVtables>(this: *mut c_void) {
    ffi_guard!(CHANNEL, "basic_ostream::_Osfx", (), {
        // SAFETY: flux valide.
        unsafe { osfx::<E>(this) };
    })
}

extern "win64" fn ostream_write(this: *mut c_void, s: *const u8, count: i64) -> *mut c_void {
    ffi_guard!(CHANNEL, "basic_ostream::write", this, {
        // SAFETY: flux valide ; `s` contient `count` octets.
        unsafe {
            let ios = ios_of::<u8>(this);
            let st = with_osentry::<u8, _>(this, |ok| {
                if !ok || (count > 0 && v_xsputn((*ios).mystrbuf, s, count) != count) {
                    BADBIT
                } else {
                    GOODBIT
                }
            });
            bios_setstate(ios, st, false);
        }
        this
    })
}

extern "win64" fn ostream_tellp(this: *mut c_void, ret: *mut Fpos) -> *mut Fpos {
    ffi_guard!(CHANNEL, "basic_ostream::tellp", ret, {
        // SAFETY: flux valide ; `ret` tampon caché.
        unsafe {
            let ios = ios_of::<u8>(this);
            let pos = with_osentry::<u8, _>(this, |_| {
                if failed((*ios).base.mystate) {
                    Fpos::from_off(-1)
                } else {
                    v_seekoff((*ios).mystrbuf, 0, SEEK_CUR, MODE_OUT)
                }
            });
            ret.write(pos);
        }
        ret
    })
}

// ---------- formatage numérique (`num_put`, locale « C ») ----------

/// Formate un entier selon `basefield`/`showbase`/`showpos`/`uppercase` (`%d`/`%u`/`%o`/`%x`).
fn fmt_integer(neg: bool, mag: u64, signed: bool, fl: i32) -> String {
    let base = fl & BASEFIELD;
    if base == OCT {
        let s = format!("{mag:o}");
        if fl & SHOWBASE != 0 && mag != 0 {
            format!("0{s}")
        } else {
            s
        }
    } else if base == HEX {
        let s = if fl & UPPERCASE != 0 {
            format!("{mag:X}")
        } else {
            format!("{mag:x}")
        };
        if fl & SHOWBASE != 0 && mag != 0 {
            let p = if fl & UPPERCASE != 0 { "0X" } else { "0x" };
            format!("{p}{s}")
        } else {
            s
        }
    } else if neg {
        format!("-{mag}")
    } else if signed && fl & SHOWPOS != 0 {
        format!("+{mag}")
    } else {
        mag.to_string()
    }
}

/// `long` (32 bits) : en octal/hexa, valeur réinterprétée non signée (`%lx`).
fn fmt_long(v: i32, fl: i32) -> String {
    let b = fl & BASEFIELD;
    if b == OCT || b == HEX {
        fmt_integer(false, u64::from(v as u32), true, fl)
    } else {
        fmt_integer(v < 0, u64::from(v.unsigned_abs()), true, fl)
    }
}

/// Formatage `double` via `snprintf` (`%g`/`%f`/`%e`/`%a` + `+`/`#`, précision `_Prec`).
fn fmt_double(v: f64, fl: i32, prec: i64) -> String {
    let ff = fl & FLOATFIELD;
    let mut f = String::from("%");
    if fl & SHOWPOS != 0 {
        f.push('+');
    }
    if fl & SHOWPOINT != 0 {
        f.push('#');
    }
    let up = fl & UPPERCASE != 0;
    let hexf = ff == FIXED | SCIENTIFIC;
    if !hexf {
        f.push_str(".*");
    }
    f.push(match (ff, up) {
        (FIXED, _) => 'f',
        (SCIENTIFIC, false) => 'e',
        (SCIENTIFIC, true) => 'E',
        (x, false) if x == FIXED | SCIENTIFIC => 'a',
        (x, true) if x == FIXED | SCIENTIFIC => 'A',
        (_, false) => 'g',
        (_, true) => 'G',
    });
    let Ok(cf) = std::ffi::CString::new(f) else {
        return String::new();
    };
    let p = prec.clamp(0, 400) as libc::c_int;
    let mut buf = vec![0u8; 512];
    // SAFETY: format construit ci-dessus (un `int` de précision optionnel + un `double`), tampon borné.
    let n = unsafe {
        if hexf {
            libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), cf.as_ptr(), v)
        } else {
            libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), cf.as_ptr(), p, v)
        }
    };
    let n = usize::try_from(n).unwrap_or(0).min(buf.len() - 1);
    buf.truncate(n);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Écrit `text` avec padding (`width`, `fill`, `adjustfield`) via `sputc`, puis `width(0)`.
///
/// # Safety
/// `ios` valide avec streambuf non nul.
unsafe fn put_padded<E: Elem>(ios: *mut BasicIos<E>, text: &str) -> bool {
    // SAFETY: contrat de l'appelant.
    unsafe {
        let sb = (*ios).mystrbuf;
        let fl = (*ios).base.fmtfl;
        let fill = (*ios).fillch;
        let bytes = text.as_bytes();
        let pad = usize::try_from((*ios).base.wide)
            .unwrap_or(0)
            .saturating_sub(bytes.len());
        (*ios).base.wide = 0;
        let adj = fl & ADJUSTFIELD;
        // Longueur du préfixe (signe et/ou 0x) pour `internal`.
        let mut pre = 0;
        if matches!(bytes.first(), Some(b'+' | b'-')) {
            pre = 1;
        }
        if bytes.len() >= pre + 2 && bytes[pre] == b'0' && matches!(bytes[pre + 1], b'x' | b'X') {
            pre += 2;
        }
        let mut ok = true;
        let mut put = |c: E| {
            if ok && E::is_eof(sputc(sb, c)) {
                ok = false;
            }
        };
        let fill_n = |put: &mut dyn FnMut(E), n: usize| {
            for _ in 0..n {
                put(fill);
            }
        };
        if adj == INTERNAL {
            bytes[..pre].iter().for_each(|&b| put(E::from_ascii(b)));
            fill_n(&mut put, pad);
            bytes[pre..].iter().for_each(|&b| put(E::from_ascii(b)));
        } else if adj == LEFT {
            bytes.iter().for_each(|&b| put(E::from_ascii(b)));
            fill_n(&mut put, pad);
        } else {
            fill_n(&mut put, pad);
            bytes.iter().for_each(|&b| put(E::from_ascii(b)));
        }
        ok
    }
}

/// Corps commun des `operator<<` numériques.
///
/// # Safety
/// `this` flux de sortie `char` valide.
unsafe fn ostream_put_num(this: *mut c_void, make: impl FnOnce(i32, i64) -> String) -> *mut c_void {
    // SAFETY: contrat de l'appelant.
    unsafe {
        let ios = ios_of::<u8>(this);
        let st = with_osentry::<u8, _>(this, |ok| {
            if ok {
                let text = make((*ios).base.fmtfl, (*ios).base.prec);
                if put_padded(ios, &text) {
                    GOODBIT
                } else {
                    BADBIT
                }
            } else {
                GOODBIT
            }
        });
        bios_setstate(ios, st, false);
        this
    }
}

extern "win64" fn ostream_put_int(this: *mut c_void, v: i32) -> *mut c_void {
    ffi_guard!(CHANNEL, "basic_ostream::operator<<(int)", this, {
        // SAFETY: flux valide.
        unsafe { ostream_put_num(this, |fl, _| fmt_long(v, fl)) }
    })
}

extern "win64" fn ostream_put_long(this: *mut c_void, v: i32) -> *mut c_void {
    ffi_guard!(CHANNEL, "basic_ostream::operator<<(long)", this, {
        // SAFETY: flux valide.
        unsafe { ostream_put_num(this, |fl, _| fmt_long(v, fl)) }
    })
}

extern "win64" fn ostream_put_uint(this: *mut c_void, v: u32) -> *mut c_void {
    ffi_guard!(CHANNEL, "basic_ostream::operator<<(unsigned int)", this, {
        // SAFETY: flux valide.
        unsafe { ostream_put_num(this, |fl, _| fmt_integer(false, u64::from(v), false, fl)) }
    })
}

extern "win64" fn ostream_put_u64(this: *mut c_void, v: u64) -> *mut c_void {
    ffi_guard!(
        CHANNEL,
        "basic_ostream::operator<<(unsigned __int64)",
        this,
        {
            // SAFETY: flux valide.
            unsafe { ostream_put_num(this, |fl, _| fmt_integer(false, v, false, fl)) }
        }
    )
}

extern "win64" fn ostream_put_float(this: *mut c_void, v: f32) -> *mut c_void {
    ffi_guard!(CHANNEL, "basic_ostream::operator<<(float)", this, {
        // SAFETY: flux valide.
        unsafe { ostream_put_num(this, |fl, prec| fmt_double(f64::from(v), fl, prec)) }
    })
}

extern "win64" fn ostream_put_ptr(this: *mut c_void, v: *const c_void) -> *mut c_void {
    ffi_guard!(CHANNEL, "basic_ostream::operator<<(const void*)", this, {
        // `%p` UCRT : 16 chiffres hexadécimaux majuscules, sans préfixe.
        // SAFETY: flux valide.
        unsafe { ostream_put_num(this, |_, _| format!("{:016X}", v as usize)) }
    })
}

// ---------- basic_iostream ----------

extern "win64" fn iostream_ctor<E: IosVtables>(
    this: *mut c_void,
    sb: *mut StreamBuf<E>,
    virt: u32,
) -> *mut c_void {
    ffi_guard!(CHANNEL, "basic_iostream::basic_iostream", this, {
        if this.is_null() {
            return this;
        }
        // SAFETY: `this` pointe vers 0x78 octets en construction.
        unsafe {
            let os = this.cast::<u8>().add(IOSTREAM_OSTREAM_OFF).cast::<c_void>();
            if virt != 0 {
                *(this as *mut *const i32) = IOSTREAM_VBT_I.as_ptr();
                *(os as *mut *const i32) = IOSTREAM_VBT_O.as_ptr();
                bios_ctor_impl::<E>(this.cast::<u8>().add(IOSTREAM_VBASE).cast());
            }
            istream_ctor_impl::<E>(this, sb, false, false);
            // basic_ostream(_Noinit, false) : rien à initialiser.
            let ios = ios_of::<E>(this);
            (*ios).base.vptr = E::iostream_vt().ptr();
        }
        this
    })
}

extern "win64" fn iostream_dtor<E: IosVtables>(ios: *mut BasicIos<E>) {
    if !ios.is_null() {
        // ~basic_iostream puis ~basic_ostream / ~basic_istream (corps vides).
        // SAFETY: objet valide.
        unsafe { (*ios).base.vptr = E::iostream_vt().ptr() };
        ostream_dtor(ios);
        istream_dtor(ios);
    }
}

extern "win64" fn iostream_vdtor<E: IosVtables>(ios: *mut BasicIos<E>, flags: u32) -> *mut c_void {
    let full = ios.cast::<u8>().wrapping_sub(IOSTREAM_VBASE);
    iostream_dtor(ios);
    bios_dtor(ios);
    if flags & 1 != 0 {
        // SAFETY: objet complet alloué par `operator new`.
        unsafe { libc::free(full.cast()) };
    }
    full.cast()
}

pub fn register(m: &mut HashMap<&'static str, usize>) {
    macro_rules! reg {
        ($name:expr, $f:expr) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    // ios_base
    reg!("??0ios_base@std@@IEAA@XZ", ios_base_ctor);
    reg!("??1ios_base@std@@UEAA@XZ", ios_base_dtor);
    reg!(
        "?_Ios_base_dtor@ios_base@std@@CAXPEAV12@@Z",
        ios_base_dtor_static
    );
    reg!("?good@ios_base@std@@QEBA_NXZ", ios_base_good);
    reg!("?getloc@ios_base@std@@QEBA?AVlocale@2@XZ", ios_base_getloc);
    m.insert("??_7ios_base@std@@6B@", IOS_BASE_VT.addr());

    // basic_ios
    reg!(
        "??0?$basic_ios@DU?$char_traits@D@std@@@std@@IEAA@XZ",
        bios_ctor::<u8>
    );
    reg!(
        "??0?$basic_ios@_WU?$char_traits@_W@std@@@std@@IEAA@XZ",
        bios_ctor::<u16>
    );
    reg!(
        "??1?$basic_ios@DU?$char_traits@D@std@@@std@@UEAA@XZ",
        bios_dtor::<u8>
    );
    reg!(
        "??1?$basic_ios@_WU?$char_traits@_W@std@@@std@@UEAA@XZ",
        bios_dtor::<u16>
    );
    reg!(
        "?clear@?$basic_ios@DU?$char_traits@D@std@@@std@@QEAAXH_N@Z",
        bios_clear_export::<u8>
    );
    reg!(
        "?clear@?$basic_ios@_WU?$char_traits@_W@std@@@std@@QEAAXH_N@Z",
        bios_clear_export::<u16>
    );
    reg!(
        "?setstate@?$basic_ios@DU?$char_traits@D@std@@@std@@QEAAXH_N@Z",
        bios_setstate_export::<u8>
    );
    reg!(
        "?setstate@?$basic_ios@_WU?$char_traits@_W@std@@@std@@QEAAXH_N@Z",
        bios_setstate_export::<u16>
    );
    reg!(
        "?init@?$basic_ios@DU?$char_traits@D@std@@@std@@IEAAXPEAV?$basic_streambuf@DU?$char_traits@D@std@@@2@_N@Z",
        bios_init_export::<u8>
    );
    reg!(
        "?imbue@?$basic_ios@DU?$char_traits@D@std@@@std@@QEAA?AVlocale@2@AEBV32@@Z",
        bios_imbue::<u8>
    );
    m.insert(
        "??_7?$basic_ios@DU?$char_traits@D@std@@@std@@6B@",
        IOS_VT_C.addr(),
    );
    m.insert(
        "??_7?$basic_ios@_WU?$char_traits@_W@std@@@std@@6B@",
        IOS_VT_W.addr(),
    );

    // basic_istream<char>
    reg!(
        "??0?$basic_istream@DU?$char_traits@D@std@@@std@@QEAA@PEAV?$basic_streambuf@DU?$char_traits@D@std@@@1@_N@Z",
        istream_ctor::<u8>
    );
    reg!(
        "??1?$basic_istream@DU?$char_traits@D@std@@@std@@UEAA@XZ",
        istream_dtor::<u8>
    );
    reg!(
        "?_Ipfx@?$basic_istream@DU?$char_traits@D@std@@@std@@QEAA_N_N@Z",
        istream_ipfx
    );
    reg!(
        "??5?$basic_istream@DU?$char_traits@D@std@@@std@@QEAAAEAV01@AEAH@Z",
        istream_extract_int
    );
    reg!(
        "?read@?$basic_istream@DU?$char_traits@D@std@@@std@@QEAAAEAV12@PEAD_J@Z",
        istream_read
    );
    reg!(
        "?seekg@?$basic_istream@DU?$char_traits@D@std@@@std@@QEAAAEAV12@_JH@Z",
        istream_seekg_off
    );
    reg!(
        "?seekg@?$basic_istream@DU?$char_traits@D@std@@@std@@QEAAAEAV12@V?$fpos@U_Mbstatet@@@2@@Z",
        istream_seekg_pos
    );
    reg!(
        "?tellg@?$basic_istream@DU?$char_traits@D@std@@@std@@QEAA?AV?$fpos@U_Mbstatet@@@2@XZ",
        istream_tellg
    );
    m.insert(
        "??_7?$basic_istream@DU?$char_traits@D@std@@@std@@6B@",
        ISTREAM_VT_C.addr(),
    );
    m.insert(
        "??_8?$basic_istream@DU?$char_traits@D@std@@@std@@7B@",
        ISTREAM_VBT.as_ptr() as usize,
    );

    // basic_ostream<char> / <wchar_t>
    reg!(
        "??0?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAA@PEAV?$basic_streambuf@DU?$char_traits@D@std@@@1@_N@Z",
        ostream_ctor::<u8>
    );
    reg!(
        "??1?$basic_ostream@DU?$char_traits@D@std@@@std@@UEAA@XZ",
        ostream_dtor::<u8>
    );
    reg!(
        "??6?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAAEAV01@H@Z",
        ostream_put_int
    );
    reg!(
        "??6?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAAEAV01@I@Z",
        ostream_put_uint
    );
    reg!(
        "??6?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAAEAV01@J@Z",
        ostream_put_long
    );
    reg!(
        "??6?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAAEAV01@M@Z",
        ostream_put_float
    );
    reg!(
        "??6?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAAEAV01@PEBX@Z",
        ostream_put_ptr
    );
    reg!(
        "??6?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAAEAV01@_K@Z",
        ostream_put_u64
    );
    reg!(
        "?write@?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAAEAV12@PEBD_J@Z",
        ostream_write
    );
    reg!(
        "?flush@?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAAEAV12@XZ",
        ostream_flush::<u8>
    );
    reg!(
        "?flush@?$basic_ostream@_WU?$char_traits@_W@std@@@std@@QEAAAEAV12@XZ",
        ostream_flush::<u16>
    );
    reg!(
        "?_Osfx@?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAXXZ",
        ostream_osfx::<u8>
    );
    reg!(
        "?_Osfx@?$basic_ostream@_WU?$char_traits@_W@std@@@std@@QEAAXXZ",
        ostream_osfx::<u16>
    );
    reg!(
        "?tellp@?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAA?AV?$fpos@U_Mbstatet@@@2@XZ",
        ostream_tellp
    );
    m.insert(
        "??_7?$basic_ostream@DU?$char_traits@D@std@@@std@@6B@",
        OSTREAM_VT_C.addr(),
    );
    m.insert(
        "??_8?$basic_ostream@DU?$char_traits@D@std@@@std@@7B@",
        OSTREAM_VBT.as_ptr() as usize,
    );

    // basic_iostream<char> / <wchar_t>
    reg!(
        "??0?$basic_iostream@DU?$char_traits@D@std@@@std@@QEAA@PEAV?$basic_streambuf@DU?$char_traits@D@std@@@1@@Z",
        iostream_ctor::<u8>
    );
    reg!(
        "??0?$basic_iostream@_WU?$char_traits@_W@std@@@std@@QEAA@PEAV?$basic_streambuf@_WU?$char_traits@_W@std@@@1@@Z",
        iostream_ctor::<u16>
    );
    reg!(
        "??1?$basic_iostream@DU?$char_traits@D@std@@@std@@UEAA@XZ",
        iostream_dtor::<u8>
    );
    reg!(
        "??1?$basic_iostream@_WU?$char_traits@_W@std@@@std@@UEAA@XZ",
        iostream_dtor::<u16>
    );
    m.insert(
        "??_7?$basic_iostream@DU?$char_traits@D@std@@@std@@6B@",
        IOSTREAM_VT_C.addr(),
    );
    m.insert(
        "??_7?$basic_iostream@_WU?$char_traits@_W@std@@@std@@6B@",
        IOSTREAM_VT_W.addr(),
    );
}

#[cfg(test)]
mod tests {
    use super::super::streambuf::tests::{free_sb, new_sb};
    use super::super::streambuf::{setg, setp};
    use super::*;
    use core::ffi::CStr;

    /// Objet invité alloué (zone non initialisée de `n` octets).
    fn raw(n: usize) -> *mut c_void {
        // SAFETY: allocation brute.
        unsafe { libc::calloc(1, n) }
    }

    fn delete_stream(ios: *mut c_void) {
        // SAFETY: appel virtuel du destructeur de suppression via le vptr du sous-objet basic_ios.
        unsafe {
            let d: unsafe extern "win64" fn(*mut c_void, u32) -> *mut c_void =
                core::mem::transmute(vslot(ios, 0));
            d(ios, 1);
        }
    }

    #[test]
    fn ostream_inserts_integers_into_memory_streambuf() {
        let sb = new_sb::<u8>();
        let mut buf = [0u8; 256];
        // SAFETY: tampon local vivant pendant tout le test.
        unsafe { setp(sb, buf.as_mut_ptr(), buf.as_mut_ptr().add(buf.len())) };
        let os = raw(0x68);
        ostream_ctor::<u8>(os, sb, 0, 1);
        // SAFETY: flux construit ci-dessus.
        let ios = unsafe { ios_of::<u8>(os) };
        assert_eq!(ios as usize, os as usize + OSTREAM_VBASE);
        // SAFETY: flux valide.
        unsafe {
            assert_eq!((*ios).base.fmtfl, SKIPWS | DEC);
            assert_eq!((*ios).base.prec, 6);
            assert_eq!((*ios).fillch, b' ');
        }
        assert!(ios_base_good(ios.cast()));
        assert_eq!(ostream_put_int(os, 42), os);
        ostream_write(os, b" ".as_ptr(), 1);
        ostream_put_int(os, -7);
        ostream_write(os, b" ".as_ptr(), 1);
        ostream_put_uint(os, 4_000_000_000);
        ostream_write(os, b" ".as_ptr(), 1);
        ostream_put_u64(os, u64::MAX);
        ostream_write(os, b" ".as_ptr(), 1);
        // SAFETY: modification des drapeaux comme `std::hex << std::showbase << std::setw(8)`.
        unsafe {
            (*ios).base.fmtfl = ((*ios).base.fmtfl & !BASEFIELD) | HEX | SHOWBASE;
            (*ios).base.wide = 8;
            (*ios).fillch = b'*';
        }
        ostream_put_long(os, 255);
        ostream_write(os, b" ".as_ptr(), 1);
        ostream_put_int(os, -1);
        // SAFETY: idem (`internal`, `dec`, `showpos`).
        unsafe {
            (*ios).base.fmtfl = DEC | SHOWPOS | INTERNAL;
            (*ios).base.wide = 6;
            (*ios).fillch = b'0';
        }
        ostream_write(os, b" ".as_ptr(), 1);
        ostream_put_int(os, 12);
        // SAFETY: idem (`left`).
        unsafe {
            (*ios).base.fmtfl = DEC | LEFT;
            (*ios).base.wide = 4;
            (*ios).fillch = b'.';
        }
        ostream_write(os, b" ".as_ptr(), 1);
        ostream_put_int(os, 5);
        // SAFETY: flottant par défaut (%g, précision 6).
        unsafe { (*ios).base.fmtfl = DEC };
        ostream_write(os, b" ".as_ptr(), 1);
        ostream_put_float(os, 1.5);
        ostream_write(os, b" ".as_ptr(), 1);
        ostream_put_float(os, 1.0 / 3.0);
        // SAFETY: fixed, précision 2.
        unsafe {
            (*ios).base.fmtfl = DEC | FIXED;
            (*ios).base.prec = 2;
        }
        ostream_write(os, b" ".as_ptr(), 1);
        ostream_put_float(os, 2.0);
        ostream_write(os, b" ".as_ptr(), 1);
        ostream_put_ptr(os, 0x1234usize as *const c_void);
        ostream_flush::<u8>(os);
        assert!(ios_base_good(ios.cast()));
        let s = CStr::from_bytes_until_nul(&buf)
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();
        assert_eq!(
            s,
            "42 -7 4000000000 18446744073709551615 ****0xff 0xffffffff +00012 5... 1.5 0.333333 2.00 0000000000001234"
        );
        let mut pos = Fpos::default();
        ostream_tellp(os, &mut pos);
        assert_eq!(pos.off(), -1); // seekoff par défaut
        delete_stream(ios.cast());
        free_sb(sb);
    }

    #[test]
    fn ostream_without_room_sets_badbit() {
        let sb = new_sb::<u8>();
        let os = raw(0x68);
        ostream_ctor::<u8>(os, sb, 0, 1);
        ostream_put_int(os, 1); // aucun tampon : overflow par défaut -> eof
        // SAFETY: flux valide.
        let ios = unsafe { ios_of::<u8>(os) };
        // SAFETY: flux valide.
        assert_eq!(unsafe { (*ios).base.mystate } & BADBIT, BADBIT);
        assert!(!ios_base_good(ios.cast()));
        delete_stream(ios.cast());
        free_sb(sb);
    }

    #[test]
    fn istream_extracts_ints_and_reads() {
        let sb = new_sb::<u8>();
        let mut data = *b"  123\n-45 0x1F zz";
        // SAFETY: tampon local.
        unsafe {
            setg(
                sb,
                data.as_mut_ptr(),
                data.as_mut_ptr(),
                data.as_mut_ptr().add(data.len()),
            )
        };
        let is = raw(0x70);
        istream_ctor::<u8>(is, sb, 0, 1);
        // SAFETY: flux construit.
        let ios = unsafe { ios_of::<u8>(is) };
        assert_eq!(ios as usize, is as usize + ISTREAM_VBASE);
        let mut v = 0i32;
        istream_extract_int(is, &mut v);
        assert_eq!(v, 123);
        istream_extract_int(is, &mut v);
        assert_eq!(v, -45);
        // `0x1F` en décimal : lit `0` puis s'arrête sur `x`.
        istream_extract_int(is, &mut v);
        assert_eq!(v, 0);
        let mut two = [0u8; 2];
        istream_read(is, two.as_mut_ptr(), 2);
        assert_eq!(&two, b"x1");
        // SAFETY: flux valide (`_Chcount` à l'offset 8).
        assert_eq!(unsafe { *(is.cast::<u8>().add(8) as *const i64) }, 2);
        // SAFETY: passage en base 16 (`std::hex`).
        unsafe { (*ios).base.fmtfl = SKIPWS | HEX };
        istream_extract_int(is, &mut v); // "F"
        assert_eq!(v, 15);
        istream_extract_int(is, &mut v); // "zz" -> échec
        assert_eq!(v, 0);
        // SAFETY: flux valide.
        assert_eq!(unsafe { (*ios).base.mystate } & FAILBIT, FAILBIT);
        assert!(!istream_ipfx(is, 0));
        // seekg remet eofbit à zéro mais le streambuf de base échoue (-1) -> failbit.
        bios_clear_export::<u8>(ios, GOODBIT, 0);
        istream_seekg_off(is, 0, SEEK_BEG);
        // SAFETY: flux valide.
        assert_eq!(unsafe { (*ios).base.mystate }, FAILBIT);
        let mut pos = Fpos::default();
        istream_tellg(is, &mut pos);
        assert_eq!(pos.off(), -1);
        // lecture courte -> eof|fail
        bios_clear_export::<u8>(ios, GOODBIT, 0);
        let mut big = [0u8; 16];
        istream_read(is, big.as_mut_ptr(), 16);
        // SAFETY: flux valide.
        assert_eq!(unsafe { (*ios).base.mystate }, EOFBIT | FAILBIT);
        delete_stream(ios.cast());
        free_sb(sb);
    }

    #[test]
    fn iostream_layout_and_roundtrip() {
        let sb = new_sb::<u8>();
        let mut store = [0u8; 32];
        // SAFETY: tampons locaux.
        unsafe {
            setp(sb, store.as_mut_ptr(), store.as_mut_ptr().add(32));
            setg(
                sb,
                store.as_mut_ptr(),
                store.as_mut_ptr(),
                store.as_mut_ptr(),
            );
        }
        let io = raw(0x78);
        iostream_ctor::<u8>(io, sb, 1);
        let os = io
            .cast::<u8>()
            .wrapping_add(IOSTREAM_OSTREAM_OFF)
            .cast::<c_void>();
        // SAFETY: flux construit.
        unsafe {
            assert_eq!(ios_of::<u8>(io) as usize, io as usize + IOSTREAM_VBASE);
            assert_eq!(ios_of::<u8>(os) as usize, io as usize + IOSTREAM_VBASE);
        }
        ostream_put_int(os, 987);
        // SAFETY: rend les octets écrits lisibles.
        unsafe {
            setg(
                sb,
                store.as_mut_ptr(),
                store.as_mut_ptr(),
                store.as_mut_ptr().add(3),
            )
        };
        let mut v = 0;
        istream_extract_int(io, &mut v);
        assert_eq!(v, 987);
        // SAFETY: flux valide.
        delete_stream(unsafe { ios_of::<u8>(io) }.cast());

        // Variante wchar_t.
        let wsb = new_sb::<u16>();
        let wio = raw(0x78);
        iostream_ctor::<u16>(wio, wsb, 1);
        // SAFETY: flux construit.
        let wios = unsafe { ios_of::<u16>(wio) };
        // SAFETY: flux valide.
        unsafe { assert_eq!((*wios).fillch, u16::from(b' ')) };
        let wos = wio
            .cast::<u8>()
            .wrapping_add(IOSTREAM_OSTREAM_OFF)
            .cast::<c_void>();
        ostream_flush::<u16>(wos);
        ostream_osfx::<u16>(wos);
        bios_setstate_export::<u16>(wios, EOFBIT, 0);
        // SAFETY: flux valide.
        assert_eq!(unsafe { (*wios).base.mystate }, EOFBIT);
        iostream_dtor::<u16>(wios);
        bios_dtor::<u16>(wios);
        // SAFETY: objet détruit.
        unsafe { libc::free(wio) };
        free_sb(wsb);
        free_sb(sb);
    }

    #[test]
    fn basic_ios_init_imbue_and_stdstr() {
        let ios = raw(0x60).cast::<BasicIos<u8>>();
        bios_ctor::<u8>(ios);
        bios_init_export::<u8>(ios, ptr::null_mut(), 1);
        // SAFETY: objet initialisé.
        unsafe {
            assert_eq!((*ios).base.mystate, BADBIT); // pas de streambuf
            assert!((*ios).base.stdstr >= 1);
        }
        let mut loc = core::mem::MaybeUninit::<Locale>::uninit();
        ios_base_getloc(ios.cast(), loc.as_mut_ptr());
        let mut old = core::mem::MaybeUninit::<Locale>::uninit();
        bios_imbue::<u8>(ios, old.as_mut_ptr(), loc.as_ptr());
        // SAFETY: locales construites ci-dessus.
        unsafe {
            assert_eq!(old.assume_init_ref().ptr, loc.assume_init_ref().ptr);
            super::super::locale::locale_release(old.as_mut_ptr());
            super::super::locale::locale_release(loc.as_mut_ptr());
        }
        bios_dtor::<u8>(ios);
        // SAFETY: objet détruit.
        unsafe {
            assert!((*ios).base.ploc.is_null());
            libc::free(ios.cast());
        }
        // ios_base seul.
        let b = raw(0x48).cast::<IosBase>();
        ios_base_ctor(b);
        assert!(ios_base_good(b));
        ios_base_vdtor(b, 1);
    }
}
