//! HLE `msvcp140.dll` (iostreams / locale / helpers `std::_X*`) et compléments `vcruntime140.dll`
//! (RTTI `__RTDynamicCast`, `type_info`, `setjmp`/`longjmp`, `purecall`).
//!
//! Layouts reproduits à l'identique de la STL MSVC v143 x64 (github.com/microsoft/STL, en-têtes
//! `<xiosbase>`, `<streambuf>`, `<ios>`, `<istream>`, `<ostream>`, `<xlocale>`, `<xlocinfo>`), et
//! recoupés avec les structures de la réimplémentation `msvcp` de Wine :
//!
//! | classe                         | taille | détail                                                  |
//! |--------------------------------|--------|---------------------------------------------------------|
//! | `ios_base`                     | 0x48   | vptr, `_Stdstr`, `_Mystate`, `_Except`, `_Fmtfl`, `_Prec`, `_Wide`, `_Arr`, `_Calls`, `_Ploc` |
//! | `basic_ios<E>`                 | 0x60   | `ios_base` + `_Mystrbuf`@0x48, `_Tiestr`@0x50, `_Fillch`@0x58 (+ padding) |
//! | `basic_streambuf<E>`           | 0x68   | vptr + 8 pointeurs indirects + 2 compteurs `int` + `_Plocale`@0x60 (plus de `_Mylock` depuis VS2015) |
//! | `basic_istream<E>`             | 0x70   | vbptr@0, `_Chcount`@8, base virtuelle `basic_ios`@0x10   |
//! | `basic_ostream<E>`             | 0x68   | vbptr@0, base virtuelle `basic_ios`@0x08                 |
//! | `basic_iostream<E>`            | 0x78   | istream(vbptr,_Chcount)@0, ostream(vbptr)@0x10, `basic_ios`@0x18 |
//! | `locale` / `locale::id`        | 0x08   | `_Locimp*` / `size_t _Id`                               |
//! | `locale::facet`                | 0x10   | vptr + `_Myrefs` (compteur atomique 32 bits)             |
//! | `locale::_Locimp`              | 0x38   | facet + `_Facetvec`, `_Facetcount`, `_Catmask`, `_Xparent`, `_Name` |
//! | `_Locinfo`                     | 0x68   | `_Lockit` + 6 `_Yarn`                                   |
//! | `fpos<_Mbstatet>`              | 0x18   | `_Myoff`, `_Fpos`, `_Mystate`                           |
//!
//! ABI : les constructeurs des classes à base virtuelle reçoivent en dernier argument entier caché
//! le drapeau « most-derived » (construction de la base virtuelle) ; les destructeurs `??1` de ces
//! classes reçoivent `this` ajusté sur le sous-objet `basic_ios` (classe introduisant le
//! destructeur virtuel). Les fonctions membres retournant une classe le font par pointeur caché
//! (inséré juste après `this`).
//!
//! Exceptions C++ : `std::_X*` et `_Throw_C_error` lèvent de vraies exceptions `std::exception`
//! (voir `crate::exceptions::cxx::synth`). Les autres sites (`ios_base::failure`, `bad_cast`,
//! `bad_alloc` des locales...) s'exécutent dans des fonctions Rust hôtes, sans contexte invité à
//! dérouler : ils journalisent une erreur puis terminent comme `abort()` (`fatal_throw`).

#![allow(non_snake_case, unused_variables)]

mod ios;
mod layout;
mod locale;
mod streambuf;
mod support;
mod vcrt;

use core::ffi::c_void;
use std::collections::HashMap;
use std::io::Write as _;

pub(crate) use layout::*;

pub(crate) const CHANNEL: &str = "msvcp_ios";

/// Table virtuelle MSVC : `vtable[-1]` = pointeur vers le `CompleteObjectLocator` (nul ici :
/// pas de RTTI pour les classes internes), puis les entrées dans l'ordre de déclaration MSVC.
#[repr(C)]
pub(crate) struct VTable<const N: usize> {
    pub col: *const (),
    pub slots: [*const (); N],
}

// SAFETY: table immuable de pointeurs de fonctions statiques.
unsafe impl<const N: usize> Sync for VTable<N> {}

impl<const N: usize> VTable<N> {
    pub const fn new(slots: [*const (); N]) -> Self {
        Self {
            col: core::ptr::null(),
            slots,
        }
    }

    /// Adresse vue par l'invité (`??_7...@6B@`) : premier slot.
    pub fn addr(&'static self) -> usize {
        self.slots.as_ptr() as usize
    }

    pub fn ptr(&'static self) -> *const *const () {
        self.slots.as_ptr()
    }
}

/// Lit le slot `idx` de la vtable de `obj` (vptr à l'offset 0).
///
/// # Safety
/// `obj` doit pointer vers un objet polymorphe MSVC valide dont la vtable a au moins `idx + 1` slots.
pub(crate) unsafe fn vslot(obj: *const c_void, idx: usize) -> *const () {
    // SAFETY: contrat de l'appelant (vptr valide en tête d'objet).
    unsafe {
        let vt = *(obj as *const *const *const ());
        *vt.add(idx)
    }
}

/// Termine le processus comme `abort()` après une exception C++ impossible à propager.
pub(crate) fn fatal_throw(kind: &str, msg: &str) -> ! {
    sb_common::sb_err!(
        CHANNEL,
        "exception C++ {kind}(\"{msg}\") non propageable depuis le code hôte — abort()"
    );
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(134);
}

/// Exports `msvcp140.dll` (noms décorés MSVC x64 -> adresse).
pub fn exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    locale::register(&mut m);
    streambuf::register(&mut m);
    ios::register(&mut m);
    support::register(&mut m);
    // `std::_X*`, `_Throw_C_error`, `std::exception_ptr` : vraies exceptions C++ (SEH).
    crate::exceptions::cxx::register_msvcp(&mut m);
    m
}

/// Exports `vcruntime140.dll` supplémentaires (fusionnés dans la famille CRT).
pub fn vcruntime_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    vcrt::register(&mut m);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_required_msvcp_symbols_are_exported() {
        let m = exports();
        for name in REQUIRED_MSVCP {
            assert!(m.contains_key(name), "symbole manquant : {name}");
        }
        let v = vcruntime_exports();
        for name in [
            "__RTDynamicCast",
            "__std_type_info_compare",
            "__std_type_info_name",
            "__current_exception",
            "__current_exception_context",
            "_set_purecall_handler",
            "_get_purecall_handler",
            "__intrinsic_setjmp",
            "longjmp",
        ] {
            assert!(v.contains_key(name), "symbole vcruntime manquant : {name}");
        }
    }

    const REQUIRED_MSVCP: &[&str] = &[
        "??0?$basic_ios@DU?$char_traits@D@std@@@std@@IEAA@XZ",
        "??0?$basic_iostream@DU?$char_traits@D@std@@@std@@QEAA@PEAV?$basic_streambuf@DU?$char_traits@D@std@@@1@@Z",
        "??0?$basic_iostream@_WU?$char_traits@_W@std@@@std@@QEAA@PEAV?$basic_streambuf@_WU?$char_traits@_W@std@@@1@@Z",
        "??0?$basic_istream@DU?$char_traits@D@std@@@std@@QEAA@PEAV?$basic_streambuf@DU?$char_traits@D@std@@@1@_N@Z",
        "??0?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAA@PEAV?$basic_streambuf@DU?$char_traits@D@std@@@1@_N@Z",
        "??0?$basic_streambuf@DU?$char_traits@D@std@@@std@@IEAA@XZ",
        "??0?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@IEAA@XZ",
        "??0_Locinfo@std@@QEAA@HPEBD@Z",
        "??0_Lockit@std@@QEAA@H@Z",
        "??0ios_base@std@@IEAA@XZ",
        "??1?$basic_ios@DU?$char_traits@D@std@@@std@@UEAA@XZ",
        "??1?$basic_ios@_WU?$char_traits@_W@std@@@std@@UEAA@XZ",
        "??1?$basic_iostream@DU?$char_traits@D@std@@@std@@UEAA@XZ",
        "??1?$basic_iostream@_WU?$char_traits@_W@std@@@std@@UEAA@XZ",
        "??1?$basic_istream@DU?$char_traits@D@std@@@std@@UEAA@XZ",
        "??1?$basic_ostream@DU?$char_traits@D@std@@@std@@UEAA@XZ",
        "??1?$basic_streambuf@DU?$char_traits@D@std@@@std@@UEAA@XZ",
        "??1?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@UEAA@XZ",
        "??1_Locinfo@std@@QEAA@XZ",
        "??1_Lockit@std@@QEAA@XZ",
        "??1ios_base@std@@UEAA@XZ",
        "??4?$_Yarn@D@std@@QEAAAEAV01@PEBD@Z",
        "??5?$basic_istream@DU?$char_traits@D@std@@@std@@QEAAAEAV01@AEAH@Z",
        "??6?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAAEAV01@H@Z",
        "??6?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAAEAV01@I@Z",
        "??6?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAAEAV01@J@Z",
        "??6?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAAEAV01@M@Z",
        "??6?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAAEAV01@PEBX@Z",
        "??6?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAAEAV01@_K@Z",
        "??Bid@locale@std@@QEAA_KXZ",
        "??_7?$basic_ios@DU?$char_traits@D@std@@@std@@6B@",
        "??_7?$basic_ios@_WU?$char_traits@_W@std@@@std@@6B@",
        "??_7ios_base@std@@6B@",
        "?_Fiopen@std@@YAPEAU_iobuf@@PEBDHH@Z",
        "?_Fiopen@std@@YAPEAU_iobuf@@PEB_WHH@Z",
        "?_Getcat@?$codecvt@DDU_Mbstatet@@@std@@SA_KPEAPEBVfacet@locale@2@PEBV42@@Z",
        "?_Getcat@?$ctype@_W@std@@SA_KPEAPEBVfacet@locale@2@PEBV42@@Z",
        "?_Getgloballocale@locale@std@@CAPEAV_Locimp@12@XZ",
        "?_Gninc@?$basic_streambuf@DU?$char_traits@D@std@@@std@@IEAAPEADXZ",
        "?_Id_cnt@id@locale@std@@0HA",
        "?_Init@?$basic_streambuf@DU?$char_traits@D@std@@@std@@IEAAXXZ",
        "?_Init@locale@std@@CAPEAV_Locimp@12@_N@Z",
        "?_Ios_base_dtor@ios_base@std@@CAXPEAV12@@Z",
        "?_Ipfx@?$basic_istream@DU?$char_traits@D@std@@@std@@QEAA_N_N@Z",
        "?_Lock@?$basic_streambuf@DU?$char_traits@D@std@@@std@@UEAAXXZ",
        "?_Lock@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@UEAAXXZ",
        "?_Makeloc@_Locimp@locale@std@@CAPEAV123@AEBV_Locinfo@3@HPEAV123@PEBV23@@Z",
        "?_New_Locimp@_Locimp@locale@std@@CAPEAV123@_N@Z",
        "?_Osfx@?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAXXZ",
        "?_Osfx@?$basic_ostream@_WU?$char_traits@_W@std@@@std@@QEAAXXZ",
        "?_Pninc@?$basic_streambuf@DU?$char_traits@D@std@@@std@@IEAAPEADXZ",
        "?_Pninc@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@IEAAPEA_WXZ",
        "?_Random_device@std@@YAIXZ",
        "?_Syserror_map@std@@YAPEBDH@Z",
        "?_Throw_C_error@std@@YAXH@Z",
        "?_Unlock@?$basic_streambuf@DU?$char_traits@D@std@@@std@@UEAAXXZ",
        "?_Unlock@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@UEAAXXZ",
        "?_Winerror_map@std@@YAHH@Z",
        "?_Xbad_alloc@std@@YAXXZ",
        "?_Xbad_function_call@std@@YAXXZ",
        "?_Xinvalid_argument@std@@YAXPEBD@Z",
        "?_Xlength_error@std@@YAXPEBD@Z",
        "?_Xout_of_range@std@@YAXPEBD@Z",
        "?_Xruntime_error@std@@YAXPEBD@Z",
        "?always_noconv@codecvt_base@std@@QEBA_NXZ",
        "?classic@locale@std@@SAAEBV12@XZ",
        "?clear@?$basic_ios@DU?$char_traits@D@std@@@std@@QEAAXH_N@Z",
        "?flush@?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAAEAV12@XZ",
        "?flush@?$basic_ostream@_WU?$char_traits@_W@std@@@std@@QEAAAEAV12@XZ",
        "?getloc@?$basic_streambuf@DU?$char_traits@D@std@@@std@@QEBA?AVlocale@2@XZ",
        "?getloc@ios_base@std@@QEBA?AVlocale@2@XZ",
        "?good@ios_base@std@@QEBA_NXZ",
        "?id@?$codecvt@DDU_Mbstatet@@@std@@2V0locale@2@A",
        "?id@?$ctype@_W@std@@2V0locale@2@A",
        "?imbue@?$basic_ios@DU?$char_traits@D@std@@@std@@QEAA?AVlocale@2@AEBV32@@Z",
        "?imbue@?$basic_streambuf@DU?$char_traits@D@std@@@std@@MEAAXAEBVlocale@2@@Z",
        "?imbue@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@MEAAXAEBVlocale@2@@Z",
        "?in@?$codecvt@DDU_Mbstatet@@@std@@QEBAHAEAU_Mbstatet@@PEBD1AEAPEBDPEAD3AEAPEAD@Z",
        "?init@?$basic_ios@DU?$char_traits@D@std@@@std@@IEAAXPEAV?$basic_streambuf@DU?$char_traits@D@std@@@2@_N@Z",
        "?out@?$codecvt@DDU_Mbstatet@@@std@@QEBAHAEAU_Mbstatet@@PEBD1AEAPEBDPEAD3AEAPEAD@Z",
        "?read@?$basic_istream@DU?$char_traits@D@std@@@std@@QEAAAEAV12@PEAD_J@Z",
        "?sbumpc@?$basic_streambuf@DU?$char_traits@D@std@@@std@@QEAAHXZ",
        "?seekg@?$basic_istream@DU?$char_traits@D@std@@@std@@QEAAAEAV12@V?$fpos@U_Mbstatet@@@2@@Z",
        "?seekg@?$basic_istream@DU?$char_traits@D@std@@@std@@QEAAAEAV12@_JH@Z",
        "?setbuf@?$basic_streambuf@DU?$char_traits@D@std@@@std@@MEAAPEAV12@PEAD_J@Z",
        "?setbuf@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@MEAAPEAV12@PEA_W_J@Z",
        "?setstate@?$basic_ios@DU?$char_traits@D@std@@@std@@QEAAXH_N@Z",
        "?setstate@?$basic_ios@_WU?$char_traits@_W@std@@@std@@QEAAXH_N@Z",
        "?showmanyc@?$basic_streambuf@DU?$char_traits@D@std@@@std@@MEAA_JXZ",
        "?showmanyc@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@MEAA_JXZ",
        "?sputc@?$basic_streambuf@DU?$char_traits@D@std@@@std@@QEAAHD@Z",
        "?sputc@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@QEAAG_W@Z",
        "?sync@?$basic_streambuf@DU?$char_traits@D@std@@@std@@MEAAHXZ",
        "?sync@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@MEAAHXZ",
        "?tellg@?$basic_istream@DU?$char_traits@D@std@@@std@@QEAA?AV?$fpos@U_Mbstatet@@@2@XZ",
        "?tellp@?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAA?AV?$fpos@U_Mbstatet@@@2@XZ",
        "?uflow@?$basic_streambuf@DU?$char_traits@D@std@@@std@@MEAAHXZ",
        "?uflow@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@MEAAGXZ",
        "?uncaught_exception@std@@YA_NXZ",
        "?unshift@?$codecvt@DDU_Mbstatet@@@std@@QEBAHAEAU_Mbstatet@@PEAD1AEAPEAD@Z",
        "?widen@?$ctype@_W@std@@QEBA_WD@Z",
        "?write@?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAAEAV12@PEBD_J@Z",
        "?xsgetn@?$basic_streambuf@DU?$char_traits@D@std@@@std@@MEAA_JPEAD_J@Z",
        "?xsgetn@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@MEAA_JPEA_W_J@Z",
        "?xsputn@?$basic_streambuf@DU?$char_traits@D@std@@@std@@MEAA_JPEBD_J@Z",
        "?xsputn@?$basic_streambuf@_WU?$char_traits@_W@std@@@std@@MEAA_JPEB_W_J@Z",
    ];
}
