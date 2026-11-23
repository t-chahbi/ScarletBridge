//! Structures `#[repr(C)]` partagées avec l'invité (layouts STL MSVC v143 x64) et constantes.

use core::ffi::c_void;
use core::mem::{offset_of, size_of};
use std::sync::atomic::AtomicU32;

// ---------- Constantes `ios_base` (`<xiosbase>`, `_Iosb<int>`) ----------

pub const GOODBIT: i32 = 0x0;
pub const EOFBIT: i32 = 0x1;
pub const FAILBIT: i32 = 0x2;
pub const BADBIT: i32 = 0x4;
pub const STATMASK: i32 = 0x17;

pub const SKIPWS: i32 = 0x0001;
pub const UNITBUF: i32 = 0x0002;
pub const UPPERCASE: i32 = 0x0004;
pub const SHOWBASE: i32 = 0x0008;
pub const SHOWPOINT: i32 = 0x0010;
pub const SHOWPOS: i32 = 0x0020;
pub const LEFT: i32 = 0x0040;
pub const RIGHT: i32 = 0x0080;
pub const INTERNAL: i32 = 0x0100;
pub const DEC: i32 = 0x0200;
pub const OCT: i32 = 0x0400;
pub const HEX: i32 = 0x0800;
pub const SCIENTIFIC: i32 = 0x1000;
pub const FIXED: i32 = 0x2000;
pub const ADJUSTFIELD: i32 = LEFT | RIGHT | INTERNAL;
pub const BASEFIELD: i32 = DEC | OCT | HEX;
pub const FLOATFIELD: i32 = SCIENTIFIC | FIXED;

#[cfg_attr(not(test), allow(dead_code))]
pub const SEEK_BEG: i32 = 0;
pub const SEEK_CUR: i32 = 1;
#[allow(dead_code)] // constante ABI de référence (`ios_base::end`)
pub const SEEK_END: i32 = 2;

pub const MODE_IN: i32 = 0x01;
pub const MODE_OUT: i32 = 0x02;
pub const MODE_ATE: i32 = 0x04;
pub const MODE_APP: i32 = 0x08;
pub const MODE_TRUNC: i32 = 0x10;
pub const MODE_BINARY: i32 = 0x20;
pub const MODE_NOCREATE: i32 = 0x40;
pub const MODE_NOREPLACE: i32 = 0x80;

/// `ios_base::event` (`erase_event`, `imbue_event`, `copyfmt_event`).
pub const ERASE_EVENT: i32 = 0;
pub const IMBUE_EVENT: i32 = 1;

// ---------- Type d'élément (`char` / `wchar_t`) ----------

/// Abstraction `char_traits<E>` : `int_type` = `int` (eof -1) pour `char`, `wint_t`
/// (`unsigned short`, eof 0xFFFF) pour `wchar_t`.
pub trait Elem: Copy + Default + Eq + 'static {
    type Int: Copy + Eq + core::fmt::Debug;
    fn eof() -> Self::Int;
    fn to_int(self) -> Self::Int;
    fn from_int(i: Self::Int) -> Self;
    fn from_ascii(b: u8) -> Self;
    /// Troncature d'un registre d'argument win64 vers l'élément.
    fn from_reg(r: u32) -> Self;
    fn is_eof(i: Self::Int) -> bool {
        i == Self::eof()
    }
    fn code(self) -> u32;
}

impl Elem for u8 {
    type Int = i32;
    fn eof() -> i32 {
        -1
    }
    fn to_int(self) -> i32 {
        i32::from(self)
    }
    fn from_int(i: i32) -> Self {
        i as u8
    }
    fn from_ascii(b: u8) -> Self {
        b
    }
    fn from_reg(r: u32) -> Self {
        r as u8
    }
    fn code(self) -> u32 {
        u32::from(self)
    }
}

impl Elem for u16 {
    type Int = u16;
    fn eof() -> u16 {
        0xFFFF
    }
    fn to_int(self) -> u16 {
        self
    }
    fn from_int(i: u16) -> Self {
        i
    }
    fn from_ascii(b: u8) -> Self {
        u16::from(b)
    }
    fn from_reg(r: u32) -> Self {
        r as u16
    }
    fn code(self) -> u32 {
        u32::from(self)
    }
}

// ---------- `locale` & facettes (`<xlocale>`, `<xlocinfo>`) ----------

/// `std::locale::facet` : vptr (`~facet`, `_Incref`, `_Decref`) + `_Myrefs`.
#[repr(C)]
pub struct Facet {
    pub vptr: *const *const (),
    pub refs: AtomicU32,
}

/// `std::_Yarn<E>` : chaîne C allouée par `malloc` + élément nul de repli.
#[repr(C)]
pub struct Yarn<E> {
    pub myptr: *mut E,
    pub nul: E,
}

/// `std::locale::_Locimp`.
#[repr(C)]
pub struct Locimp {
    pub facet: Facet,
    pub facetvec: *mut *mut Facet,
    pub facetcount: usize,
    pub catmask: i32,
    pub xparent: bool,
    pub name: Yarn<u8>,
}

/// `std::locale` : unique membre `_Locimp* _Ptr`.
#[repr(C)]
pub struct Locale {
    pub ptr: *mut Locimp,
}

/// `std::_Lockit` : `int _Locktype`.
#[repr(C)]
pub struct Lockit {
    pub locktype: i32,
}

/// `std::_Locinfo`.
#[repr(C)]
pub struct Locinfo {
    pub lock: Lockit,
    pub days: Yarn<u8>,
    pub months: Yarn<u8>,
    pub w_days: Yarn<u16>,
    pub w_months: Yarn<u16>,
    pub oldlocname: Yarn<u8>,
    pub newlocname: Yarn<u8>,
}

/// `_Locinfo::_Ctypevec` (`_Ctypevec` UCRT).
#[repr(C)]
pub struct Ctypevec {
    pub page: u32,
    pub table: *const i16,
    pub delfl: i32,
    pub locale_name: *mut u16,
}

/// `_Locinfo::_Cvtvec` (`_Cvtvec` UCRT).
#[repr(C)]
pub struct Cvtvec {
    pub page: u32,
    pub mbcurmax: u32,
    pub isclocale: i32,
    pub isleadbyte: [u8; 32],
}

/// `std::ctype<wchar_t>`.
#[repr(C)]
pub struct CtypeW {
    pub facet: Facet,
    pub ctype: Ctypevec,
    pub cvt: Cvtvec,
}

/// `std::codecvt<char, char, _Mbstatet>` (aucun membre propre).
#[repr(C)]
pub struct CodecvtC {
    pub facet: Facet,
}

/// `_Mbstatet` (UCRT).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Mbstatet {
    pub wchar: u32,
    pub byte: u16,
    pub state: u16,
}

/// `std::fpos<_Mbstatet>` (`streampos`).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Fpos {
    pub myoff: i64,
    pub fpos: i64,
    pub state: Mbstatet,
}

impl Fpos {
    pub fn from_off(off: i64) -> Self {
        Self {
            myoff: off,
            fpos: 0,
            state: Mbstatet::default(),
        }
    }
    /// `operator streamoff()`.
    pub fn off(&self) -> i64 {
        self.myoff.wrapping_add(self.fpos)
    }
}

// ---------- iostreams ----------

/// `std::basic_streambuf<E>`.
#[repr(C)]
pub struct StreamBuf<E> {
    pub vptr: *const *const (),
    pub gfirst: *mut E,
    pub pfirst: *mut E,
    pub igfirst: *mut *mut E,
    pub ipfirst: *mut *mut E,
    pub gnext: *mut E,
    pub pnext: *mut E,
    pub ignext: *mut *mut E,
    pub ipnext: *mut *mut E,
    pub gcount: i32,
    pub pcount: i32,
    pub igcount: *mut i32,
    pub ipcount: *mut i32,
    pub plocale: *mut Locale,
}

/// `ios_base::_Iosarray`.
#[repr(C)]
pub struct Iosarray {
    pub next: *mut Iosarray,
    pub index: i32,
    pub lo: i32,
    pub vp: *mut c_void,
}

/// `ios_base::_Fnarray`.
#[repr(C)]
pub struct Fnarray {
    pub next: *mut Fnarray,
    pub index: i32,
    pub pfn: Option<unsafe extern "win64" fn(i32, *mut IosBase, i32)>,
}

/// `std::ios_base`.
#[repr(C)]
pub struct IosBase {
    pub vptr: *const *const (),
    pub stdstr: usize,
    pub mystate: i32,
    pub except: i32,
    pub fmtfl: i32,
    pub prec: i64,
    pub wide: i64,
    pub arr: *mut Iosarray,
    pub calls: *mut Fnarray,
    pub ploc: *mut Locale,
}

/// `std::basic_ios<E>`.
#[repr(C)]
pub struct BasicIos<E> {
    pub base: IosBase,
    pub mystrbuf: *mut StreamBuf<E>,
    pub tiestr: *mut c_void,
    pub fillch: E,
}

/// Offsets du sous-objet `basic_ios` (base virtuelle) dans chaque classe de flux.
pub const ISTREAM_VBASE: usize = 0x10;
pub const OSTREAM_VBASE: usize = 0x08;
pub const IOSTREAM_VBASE: usize = 0x18;
pub const IOSTREAM_OSTREAM_OFF: usize = 0x10;

// ---------- Vérifications compile-time des layouts ----------

const _: () = assert!(size_of::<Facet>() == 0x10);
const _: () = assert!(size_of::<Yarn<u8>>() == 0x10);
const _: () = assert!(size_of::<Yarn<u16>>() == 0x10);
const _: () = assert!(size_of::<Locimp>() == 0x38);
const _: () = assert!(offset_of!(Locimp, facetvec) == 0x10);
const _: () = assert!(offset_of!(Locimp, facetcount) == 0x18);
const _: () = assert!(offset_of!(Locimp, catmask) == 0x20);
const _: () = assert!(offset_of!(Locimp, xparent) == 0x24);
const _: () = assert!(offset_of!(Locimp, name) == 0x28);
const _: () = assert!(size_of::<Locale>() == 8);
const _: () = assert!(size_of::<Lockit>() == 4);
const _: () = assert!(size_of::<Locinfo>() == 0x68);
const _: () = assert!(offset_of!(Locinfo, days) == 0x08);
const _: () = assert!(offset_of!(Locinfo, oldlocname) == 0x48);
const _: () = assert!(offset_of!(Locinfo, newlocname) == 0x58);
const _: () = assert!(size_of::<Ctypevec>() == 0x20);
const _: () = assert!(size_of::<Cvtvec>() == 0x2C);
const _: () = assert!(size_of::<CtypeW>() == 0x60);
const _: () = assert!(size_of::<CodecvtC>() == 0x10);
const _: () = assert!(size_of::<Mbstatet>() == 8);
const _: () = assert!(size_of::<Fpos>() == 0x18);
const _: () = assert!(size_of::<StreamBuf<u8>>() == 0x68);
const _: () = assert!(size_of::<StreamBuf<u16>>() == 0x68);
const _: () = assert!(offset_of!(StreamBuf<u8>, gcount) == 0x48);
const _: () = assert!(offset_of!(StreamBuf<u8>, igcount) == 0x50);
const _: () = assert!(offset_of!(StreamBuf<u8>, plocale) == 0x60);
const _: () = assert!(size_of::<Iosarray>() == 0x18);
const _: () = assert!(size_of::<Fnarray>() == 0x18);
const _: () = assert!(size_of::<IosBase>() == 0x48);
const _: () = assert!(offset_of!(IosBase, stdstr) == 0x08);
const _: () = assert!(offset_of!(IosBase, mystate) == 0x10);
const _: () = assert!(offset_of!(IosBase, except) == 0x14);
const _: () = assert!(offset_of!(IosBase, fmtfl) == 0x18);
const _: () = assert!(offset_of!(IosBase, prec) == 0x20);
const _: () = assert!(offset_of!(IosBase, wide) == 0x28);
const _: () = assert!(offset_of!(IosBase, arr) == 0x30);
const _: () = assert!(offset_of!(IosBase, calls) == 0x38);
const _: () = assert!(offset_of!(IosBase, ploc) == 0x40);
const _: () = assert!(size_of::<BasicIos<u8>>() == 0x60);
const _: () = assert!(size_of::<BasicIos<u16>>() == 0x60);
const _: () = assert!(offset_of!(BasicIos<u8>, mystrbuf) == 0x48);
const _: () = assert!(offset_of!(BasicIos<u8>, tiestr) == 0x50);
const _: () = assert!(offset_of!(BasicIos<u8>, fillch) == 0x58);
const _: () = assert!(offset_of!(BasicIos<u16>, fillch) == 0x58);
