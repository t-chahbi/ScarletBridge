//! Compléments `vcruntime140.dll` : RTTI (`__RTDynamicCast`), `type_info`
//! (`__std_type_info_compare` / `__std_type_info_name`), `__current_exception[_context]`,
//! `_set/_get_purecall_handler`, `__intrinsic_setjmp[ex]` / `longjmp` (x64, `_JUMP_BUFFER`).
//!
//! RTTI MSVC x64 : toutes les références internes sont des RVA relatives à l'ImageBase du module
//! contenant le `CompleteObjectLocator` ; avec `signature == 1`, l'ImageBase se déduit de
//! `pSelf` (`base = col - col.pSelf`), sinon on interroge le gestionnaire de modules.

use core::ffi::{CStr, c_void};
use core::ptr;
use std::collections::HashMap;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use sb_common::{ffi_guard, sb_debug};

use super::{CHANNEL, fatal_throw};

// ---------- structures RTTI (`rttidata.h`, x64) ----------

/// `TypeDescriptor` : `pVFTable`, `spare`, `name[]` (nom décoré `.?AV...@@`).
const TD_NAME_OFF: usize = 0x10;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Pmd {
    pub mdisp: i32,
    pub pdisp: i32,
    pub vdisp: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BaseClassDescriptor {
    pub p_type_descriptor: i32,
    pub num_contained_bases: u32,
    pub where_: Pmd,
    pub attributes: u32,
    pub p_class_descriptor: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ClassHierarchyDescriptor {
    pub signature: u32,
    pub attributes: u32,
    pub num_base_classes: u32,
    pub p_base_class_array: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CompleteObjectLocator {
    pub signature: u32,
    pub offset: u32,
    pub cd_offset: u32,
    pub p_type_descriptor: i32,
    pub p_class_descriptor: i32,
    pub p_self: i32,
}

const _: () = assert!(size_of::<Pmd>() == 0x0C);
const _: () = assert!(size_of::<BaseClassDescriptor>() == 0x1C);
const _: () = assert!(size_of::<ClassHierarchyDescriptor>() == 0x10);
const _: () = assert!(size_of::<CompleteObjectLocator>() == 0x18);

const BCD_NOTVISIBLE: u32 = 0x01;
const BCD_AMBIGUOUS: u32 = 0x02;
const BCD_PRIVORPROTBASE: u32 = 0x04;
const CHD_MULTINH: u32 = 0x01;
#[cfg_attr(not(test), allow(dead_code))]
const CHD_VIRTINH: u32 = 0x02;

/// Accès en lecture à l'image RTTI (RVA -> adresse).
struct Rtti {
    base: usize,
}

impl Rtti {
    /// # Safety
    /// `rva` désigne une structure RTTI valide dans l'image.
    unsafe fn at<T: Copy>(&self, rva: i32) -> T {
        // SAFETY: contrat de l'appelant ; structures RTTI alignées sur 4 seulement.
        unsafe { ((self.base as isize + rva as isize) as *const T).read_unaligned() }
    }

    fn addr(&self, rva: i32) -> usize {
        (self.base as isize + rva as isize) as usize
    }

    /// # Safety
    /// `chd` valide.
    unsafe fn bcd(&self, chd: &ClassHierarchyDescriptor, i: u32) -> BaseClassDescriptor {
        // SAFETY: contrat de l'appelant ; `i < numBaseClasses`.
        unsafe {
            let rva: i32 = self.at(chd.p_base_class_array + (i as i32) * 4);
            self.at(rva)
        }
    }
}

/// `TYPEIDS_EQ` : même adresse ou même nom décoré.
///
/// # Safety
/// Pointeurs vers des `TypeDescriptor` valides.
unsafe fn typeids_eq(a: *const u8, b: *const u8) -> bool {
    if a == b {
        return true;
    }
    if a.is_null() || b.is_null() {
        return false;
    }
    // SAFETY: noms décorés terminés par 0.
    unsafe {
        CStr::from_ptr(a.add(TD_NAME_OFF).cast()) == CStr::from_ptr(b.add(TD_NAME_OFF).cast())
    }
}

/// `PMDtoOffset`.
///
/// # Safety
/// `this` objet complet valide (vbptr lisible si `pdisp >= 0`).
unsafe fn pmd_to_offset(this: usize, pmd: Pmd) -> isize {
    let mut off: isize = 0;
    if pmd.pdisp >= 0 {
        off = pmd.pdisp as isize;
        // SAFETY: contrat de l'appelant : vbptr à `this + pdisp`, vbtable d'`int`.
        unsafe {
            let vbt = *((this as isize + off) as *const usize);
            off += *((vbt as isize + pmd.vdisp as isize) as *const i32) as isize;
        }
    }
    off + pmd.mdisp as isize
}

/// ImageBase du module invité contenant `addr` (`RtlPcToFileHeader`).
fn image_base_of(addr: usize) -> Option<usize> {
    let desc = sb_loader::module_manager().describe_address(addr)?;
    let (_, rva) = desc.rsplit_once("+0x")?;
    let rva = usize::from_str_radix(rva, 16).ok()?;
    addr.checked_sub(rva)
}

/// Recherche SI (`FindSITargetTypeInstance`).
///
/// # Safety
/// Image RTTI valide.
unsafe fn find_si(
    img: &Rtti,
    chd: &ClassHierarchyDescriptor,
    src: *const u8,
    target: *const u8,
) -> Option<BaseClassDescriptor> {
    // SAFETY: contrat de l'appelant.
    unsafe {
        for i in 0..chd.num_base_classes {
            let b = img.bcd(chd, i);
            if typeids_eq(img.addr(b.p_type_descriptor) as *const u8, target) {
                for j in i + 1..chd.num_base_classes {
                    let s = img.bcd(chd, j);
                    if s.attributes & BCD_PRIVORPROTBASE != 0 {
                        break;
                    }
                    if typeids_eq(img.addr(s.p_type_descriptor) as *const u8, src) {
                        return Some(b);
                    }
                }
                return None;
            }
        }
        None
    }
}

/// Recherche MI/VI : downcast (instance de la cible contenant la source à `src_off`), puis
/// cross-cast (cible publique, non ambiguë, à un offset unique).
///
/// # Safety
/// Image RTTI et objet complet valides.
unsafe fn find_mi_vi(
    img: &Rtti,
    chd: &ClassHierarchyDescriptor,
    complete: usize,
    src: *const u8,
    src_off: isize,
    target: *const u8,
) -> Option<BaseClassDescriptor> {
    // SAFETY: contrat de l'appelant.
    unsafe {
        let n = chd.num_base_classes;
        let is_target = |b: &BaseClassDescriptor| {
            typeids_eq(img.addr(b.p_type_descriptor) as *const u8, target)
        };
        // Downcast.
        let mut found: Option<(BaseClassDescriptor, isize)> = None;
        for i in 0..n {
            let b = img.bcd(chd, i);
            if !is_target(&b) {
                continue;
            }
            for j in 0..b.num_contained_bases {
                if i + 1 + j >= n {
                    break;
                }
                let s = img.bcd(chd, i + 1 + j);
                if typeids_eq(img.addr(s.p_type_descriptor) as *const u8, src)
                    && pmd_to_offset(complete, s.where_) == src_off
                {
                    let off = pmd_to_offset(complete, b.where_);
                    match found {
                        Some((_, o)) if o != off => return None, // ambigu
                        _ => found = Some((b, off)),
                    }
                    break;
                }
            }
        }
        if let Some((b, _)) = found {
            return Some(b);
        }
        // Cross-cast : la source doit être une base publique de l'objet complet.
        let mut src_visible = false;
        for i in 0..n {
            let s = img.bcd(chd, i);
            if typeids_eq(img.addr(s.p_type_descriptor) as *const u8, src)
                && pmd_to_offset(complete, s.where_) == src_off
                && s.attributes & BCD_NOTVISIBLE == 0
            {
                src_visible = true;
                break;
            }
        }
        if !src_visible {
            return None;
        }
        let mut cross: Option<(BaseClassDescriptor, isize)> = None;
        for i in 0..n {
            let b = img.bcd(chd, i);
            if !is_target(&b) || b.attributes & (BCD_NOTVISIBLE | BCD_AMBIGUOUS) != 0 {
                continue;
            }
            let off = pmd_to_offset(complete, b.where_);
            match cross {
                Some((_, o)) if o != off => return None,
                _ => cross = Some((b, off)),
            }
        }
        cross.map(|(b, _)| b)
    }
}

/// Cœur de `__RTDynamicCast` (renvoie `None` si le cast échoue).
///
/// # Safety
/// `inptr` objet polymorphe MSVC valide ; `src`/`target` `TypeDescriptor` valides.
unsafe fn dynamic_cast(
    inptr: usize,
    vf_delta: i32,
    src: *const u8,
    target: *const u8,
) -> Option<usize> {
    // SAFETY: contrat de l'appelant (vptr -> vtable dont l'entrée -1 est le COL).
    unsafe {
        let vt = *(inptr as *const *const usize);
        let col_addr = *vt.sub(1);
        if col_addr == 0 {
            sb_debug!(CHANNEL, "__RTDynamicCast : vtable sans COL (pas de RTTI)");
            return None;
        }
        let col = (col_addr as *const CompleteObjectLocator).read_unaligned();
        let mut complete = inptr.wrapping_sub(col.offset as usize);
        if col.cd_offset != 0 {
            let adj = *((inptr - col.cd_offset as usize) as *const i32);
            complete = complete.wrapping_sub(adj as isize as usize);
        }
        let src_ptr = (inptr as isize - vf_delta as isize) as usize;
        let src_off = src_ptr as isize - complete as isize;
        let base = if col.signature == 0 {
            image_base_of(col_addr)?
        } else {
            (col_addr as isize - col.p_self as isize) as usize
        };
        let img = Rtti { base };
        let chd: ClassHierarchyDescriptor = img.at(col.p_class_descriptor);
        let bcd = if chd.attributes & CHD_MULTINH == 0 {
            find_si(&img, &chd, src, target)
        } else {
            // MI et VI partagent la même recherche (offsets calculés via PMD/vbtable).
            find_mi_vi(&img, &chd, complete, src, src_off, target)
        }?;
        Some((complete as isize + pmd_to_offset(complete, bcd.where_)) as usize)
    }
}

extern "win64" fn rt_dynamic_cast(
    inptr: *mut c_void,
    vf_delta: i32,
    src_type: *const c_void,
    target_type: *const c_void,
    is_reference: i32,
) -> *mut c_void {
    ffi_guard!(CHANNEL, "__RTDynamicCast", ptr::null_mut(), {
        if inptr.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: arguments générés par le compilateur MSVC pour `dynamic_cast`.
        let r = unsafe {
            dynamic_cast(
                inptr as usize,
                vf_delta,
                src_type.cast(),
                target_type.cast(),
            )
        };
        match r {
            Some(p) => p as *mut c_void,
            None if is_reference != 0 => fatal_throw("std::bad_cast", "Bad dynamic_cast!"),
            None => ptr::null_mut(),
        }
    })
}

// ---------- type_info ----------

/// `__std_type_info_data` : `_UndecoratedName` (cache), `_DecoratedName[]` à +8.
#[repr(C)]
pub struct TypeInfoData {
    pub undecorated: AtomicPtr<i8>,
    pub decorated: [u8; 0],
}

const _: () = assert!(core::mem::offset_of!(TypeInfoData, decorated) == 8);

extern "win64" fn type_info_compare(l: *const TypeInfoData, r: *const TypeInfoData) -> i32 {
    if l == r {
        return 0;
    }
    if l.is_null() || r.is_null() {
        return if l.is_null() { -1 } else { 1 };
    }
    // SAFETY: noms décorés terminés par 0 (on compare à partir du 2e caractère, comme MSVC).
    unsafe {
        let a = (&raw const (*l).decorated).cast::<i8>().add(1);
        let b = (&raw const (*r).decorated).cast::<i8>().add(1);
        libc::strcmp(a, b)
    }
}

fn builtin_name(code: &str) -> Option<&'static str> {
    Some(match code {
        "X" => "void",
        "C" => "signed char",
        "D" => "char",
        "E" => "unsigned char",
        "F" => "short",
        "G" => "unsigned short",
        "H" => "int",
        "I" => "unsigned int",
        "J" => "long",
        "K" => "unsigned long",
        "M" => "float",
        "N" => "double",
        "O" => "long double",
        "_J" => "__int64",
        "_K" => "unsigned __int64",
        "_N" => "bool",
        "_W" => "wchar_t",
        "_S" => "char16_t",
        "_U" => "char32_t",
        _ => return None,
    })
}

/// Sous-ensemble de `__unDName(UNDNAME_TYPE_ONLY)` : types fondamentaux et classes non
/// templates (`.?AVns@outer@@` -> `class outer::ns`). Sinon : nom décoré brut.
pub fn undecorate_type(decorated: &str) -> String {
    let s = decorated.strip_prefix('.').unwrap_or(decorated);
    if let Some(b) = builtin_name(s) {
        return b.to_string();
    }
    let parse = || -> Option<String> {
        let rest = s.strip_prefix("?A")?;
        let (kw, rest) = if let Some(r) = rest.strip_prefix("W4") {
            ("enum ", r)
        } else {
            let (k, r) = rest.split_at_checked(1)?;
            let kw = match k {
                "V" => "class ",
                "U" => "struct ",
                "T" => "union ",
                _ => return None,
            };
            (kw, r)
        };
        let body = rest.strip_suffix("@@")?;
        if body.is_empty()
            || body.contains(['?', '$'])
            || body.starts_with(|c: char| c.is_ascii_digit())
        {
            return None;
        }
        let parts: Vec<&str> = body.split('@').collect();
        if parts
            .iter()
            .any(|p| p.is_empty() || p.chars().all(|c| c.is_ascii_digit()))
        {
            return None;
        }
        Some(format!(
            "{kw}{}",
            parts.into_iter().rev().collect::<Vec<_>>().join("::")
        ))
    };
    parse().unwrap_or_else(|| s.to_string())
}

extern "win64" fn type_info_name(data: *mut TypeInfoData, root: *mut c_void) -> *const i8 {
    ffi_guard!(CHANNEL, "__std_type_info_name", ptr::null(), {
        if data.is_null() {
            return ptr::null();
        }
        // SAFETY: `data` = `type_info::_Data` de l'invité.
        unsafe {
            let cached = (*data).undecorated.load(Ordering::Acquire);
            if !cached.is_null() {
                return cached;
            }
            let dec = CStr::from_ptr((&raw const (*data).decorated).cast()).to_string_lossy();
            let name = undecorate_type(&dec);
            let Ok(c) = std::ffi::CString::new(name) else {
                return ptr::null();
            };
            let bytes = c.as_bytes_with_nul();
            let p = libc::malloc(bytes.len()).cast::<i8>();
            if p.is_null() {
                return ptr::null();
            }
            ptr::copy_nonoverlapping(bytes.as_ptr().cast::<i8>(), p, bytes.len());
            match (*data).undecorated.compare_exchange(
                ptr::null_mut(),
                p,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => p,
                Err(other) => {
                    libc::free(p.cast());
                    other
                }
            }
        }
    })
}

// ---------- exception courante & purecall ----------

extern "win64" fn current_exception() -> *mut *mut c_void {
    crate::exceptions::cxx::current_exception_cell()
}

extern "win64" fn current_exception_context() -> *mut *mut c_void {
    crate::exceptions::cxx::current_exception_context_cell()
}

static PURECALL_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Gestionnaire installé par `_set_purecall_handler` (0 si aucun).
pub fn purecall_handler() -> usize {
    PURECALL_HANDLER.load(Ordering::Acquire)
}

extern "win64" fn set_purecall_handler(h: usize) -> usize {
    PURECALL_HANDLER.swap(h, Ordering::AcqRel)
}

extern "win64" fn get_purecall_handler() -> usize {
    purecall_handler()
}

// ---------- setjmp / longjmp ----------

/// `_JUMP_BUFFER` x64 MSVC (0x100 octets).
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct JumpBuffer {
    pub frame: u64,
    pub rbx: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub mxcsr: u32,
    pub fpcsr: u16,
    pub spare: u16,
    pub xmm: [[u64; 2]; 10],
}

const _: () = assert!(size_of::<JumpBuffer>() == 0x100);
const _: () = assert!(core::mem::offset_of!(JumpBuffer, rip) == 0x50);
const _: () = assert!(core::mem::offset_of!(JumpBuffer, mxcsr) == 0x58);
const _: () = assert!(core::mem::offset_of!(JumpBuffer, fpcsr) == 0x5C);
const _: () = assert!(core::mem::offset_of!(JumpBuffer, xmm) == 0x60);

/// `__intrinsic_setjmp(_JUMP_BUFFER*, void* frame)` : sauvegarde les registres non volatils
/// win64, `rsp`/`rip` de l'appelant, MXCSR, mot de contrôle x87 et `xmm6`–`xmm15` ; renvoie 0.
#[unsafe(naked)]
pub extern "win64" fn intrinsic_setjmp(buf: *mut JumpBuffer, frame: usize) -> i32 {
    core::arch::naked_asm!(
        "mov [rcx], rdx",
        "mov [rcx + 0x08], rbx",
        "lea r8, [rsp + 8]",
        "mov [rcx + 0x10], r8",
        "mov [rcx + 0x18], rbp",
        "mov [rcx + 0x20], rsi",
        "mov [rcx + 0x28], rdi",
        "mov [rcx + 0x30], r12",
        "mov [rcx + 0x38], r13",
        "mov [rcx + 0x40], r14",
        "mov [rcx + 0x48], r15",
        "mov r8, [rsp]",
        "mov [rcx + 0x50], r8",
        "stmxcsr dword ptr [rcx + 0x58]",
        "fnstcw word ptr [rcx + 0x5C]",
        "mov word ptr [rcx + 0x5E], 0",
        "movdqu xmmword ptr [rcx + 0x60], xmm6",
        "movdqu xmmword ptr [rcx + 0x70], xmm7",
        "movdqu xmmword ptr [rcx + 0x80], xmm8",
        "movdqu xmmword ptr [rcx + 0x90], xmm9",
        "movdqu xmmword ptr [rcx + 0xA0], xmm10",
        "movdqu xmmword ptr [rcx + 0xB0], xmm11",
        "movdqu xmmword ptr [rcx + 0xC0], xmm12",
        "movdqu xmmword ptr [rcx + 0xD0], xmm13",
        "movdqu xmmword ptr [rcx + 0xE0], xmm14",
        "movdqu xmmword ptr [rcx + 0xF0], xmm15",
        "xor eax, eax",
        "ret",
    )
}

/// `longjmp(_JUMP_BUFFER*, int)` : restaure le contexte et reprend après `setjmp`, qui renvoie
/// `val` (ou 1 si `val == 0`). Pas de déroulement SEH des frames intermédiaires (pas de
/// `RtlUnwindEx` dans le runtime HLE) : équivalent au chemin `Frame == 0` de MSVC.
#[unsafe(naked)]
pub extern "win64" fn longjmp(buf: *const JumpBuffer, val: i32) -> ! {
    core::arch::naked_asm!(
        "mov eax, edx",
        "test eax, eax",
        "jnz 2f",
        "mov eax, 1",
        "2:",
        "mov rbx, [rcx + 0x08]",
        "mov rbp, [rcx + 0x18]",
        "mov rsi, [rcx + 0x20]",
        "mov rdi, [rcx + 0x28]",
        "mov r12, [rcx + 0x30]",
        "mov r13, [rcx + 0x38]",
        "mov r14, [rcx + 0x40]",
        "mov r15, [rcx + 0x48]",
        "ldmxcsr dword ptr [rcx + 0x58]",
        "fnclex",
        "fldcw word ptr [rcx + 0x5C]",
        "movdqu xmm6, xmmword ptr [rcx + 0x60]",
        "movdqu xmm7, xmmword ptr [rcx + 0x70]",
        "movdqu xmm8, xmmword ptr [rcx + 0x80]",
        "movdqu xmm9, xmmword ptr [rcx + 0x90]",
        "movdqu xmm10, xmmword ptr [rcx + 0xA0]",
        "movdqu xmm11, xmmword ptr [rcx + 0xB0]",
        "movdqu xmm12, xmmword ptr [rcx + 0xC0]",
        "movdqu xmm13, xmmword ptr [rcx + 0xD0]",
        "movdqu xmm14, xmmword ptr [rcx + 0xE0]",
        "movdqu xmm15, xmmword ptr [rcx + 0xF0]",
        "mov rsp, [rcx + 0x10]",
        "jmp qword ptr [rcx + 0x50]",
    )
}

pub fn register(m: &mut HashMap<&'static str, usize>) {
    macro_rules! reg {
        ($name:expr, $f:expr) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("__RTDynamicCast", rt_dynamic_cast);
    reg!("__std_type_info_compare", type_info_compare);
    reg!("__std_type_info_name", type_info_name);
    reg!("__current_exception", current_exception);
    reg!("__current_exception_context", current_exception_context);
    reg!("_set_purecall_handler", set_purecall_handler);
    reg!("_get_purecall_handler", get_purecall_handler);
    reg!("__intrinsic_setjmp", intrinsic_setjmp);
    reg!("__intrinsic_setjmpex", intrinsic_setjmp);
    reg!("longjmp", longjmp);
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- setjmp / longjmp -----

    extern "win64" fn jumper(buf: *const JumpBuffer, val: i32) -> ! {
        // Frame Rust intermédiaire (sans destructeur) traversée par le saut.
        longjmp(buf, val)
    }

    /// Appelant win64 : `if (setjmp(buf) == 0) jumper(buf, val); return <valeur de setjmp>`.
    #[unsafe(naked)]
    extern "win64" fn drive(buf: *mut JumpBuffer, val: i32) -> i32 {
        core::arch::naked_asm!(
            "push rbx",
            "push r12",
            "sub rsp, 0x28",
            "mov rbx, rcx",
            "mov r12d, edx",
            "mov rcx, rbx",
            "mov rdx, rsp",
            "call {sj}",
            "test eax, eax",
            "jnz 2f",
            "mov rcx, rbx",
            "mov edx, r12d",
            "call {jmp}",
            "ud2",
            "2:",
            "add rsp, 0x28",
            "pop r12",
            "pop rbx",
            "ret",
            sj = sym intrinsic_setjmp,
            jmp = sym jumper,
        )
    }

    #[test]
    fn setjmp_longjmp_roundtrip() {
        let mut buf = JumpBuffer {
            frame: 0,
            rbx: 0,
            rsp: 0,
            rbp: 0,
            rsi: 0,
            rdi: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rip: 0,
            mxcsr: 0,
            fpcsr: 0,
            spare: 0,
            xmm: [[0; 2]; 10],
        };
        let addr = (&raw mut buf) as u64;
        assert_eq!(drive(&mut buf, 42), 42);
        assert_eq!(buf.rbx, addr);
        assert_eq!(buf.r12, 42);
        assert_ne!(buf.rip, 0);
        assert_ne!(buf.rsp, 0);
        assert_eq!(buf.rsp, buf.frame); // rsp de l'appelant après retour == frame passé
        // val == 0 -> setjmp renvoie 1
        assert_eq!(drive(&mut buf, 0), 1);
        assert_eq!(buf.mxcsr & 0x1F80, 0x1F80); // masques d'exception SSE par défaut
    }

    // ----- type_info -----

    #[test]
    fn type_info_compare_and_name() {
        #[repr(C)]
        struct Ti<const N: usize> {
            und: AtomicPtr<i8>,
            dec: [u8; N],
        }
        let a = Ti {
            und: AtomicPtr::new(ptr::null_mut()),
            dec: *b".?AVWidget@ui@@\0",
        };
        let b = Ti {
            und: AtomicPtr::new(ptr::null_mut()),
            dec: *b".?AVWidget@ui@@\0",
        };
        let c = Ti {
            und: AtomicPtr::new(ptr::null_mut()),
            dec: *b".H\0",
        };
        let pa = (&raw const a).cast::<TypeInfoData>();
        let pb = (&raw const b).cast::<TypeInfoData>();
        let pc = (&raw const c).cast::<TypeInfoData>();
        assert_eq!(type_info_compare(pa, pb), 0);
        assert_ne!(type_info_compare(pa, pc), 0);
        let n = type_info_name(pa.cast_mut(), ptr::null_mut());
        // SAFETY: chaîne allouée par `type_info_name`.
        assert_eq!(unsafe { CStr::from_ptr(n) }, c"class ui::Widget");
        assert_eq!(type_info_name(pa.cast_mut(), ptr::null_mut()), n); // cache
        let n = type_info_name(pc.cast_mut(), ptr::null_mut());
        // SAFETY: idem.
        assert_eq!(unsafe { CStr::from_ptr(n) }, c"int");
        assert_eq!(undecorate_type(".?AUPoint@@"), "struct Point");
        assert_eq!(undecorate_type(".?AW4Color@@"), "enum Color");
        assert_eq!(
            undecorate_type(".?AV?$vector@HV?$allocator@H@std@@@std@@"),
            "?AV?$vector@HV?$allocator@H@std@@@std@@"
        );
    }

    #[test]
    fn current_exception_slots_and_purecall() {
        let p = current_exception();
        let q = current_exception_context();
        assert!(!p.is_null() && !q.is_null() && p != q);
        // SAFETY: emplacements TLS valides.
        unsafe {
            assert!((*p).is_null());
            assert!((*q).is_null());
        }
        assert_eq!(current_exception(), p);
        let old = set_purecall_handler(0x1234);
        assert_eq!(get_purecall_handler(), 0x1234);
        assert_eq!(set_purecall_handler(old), 0x1234);
    }

    // ----- RTTI fabriquée en mémoire -----

    /// Image fictive : les RVA sont relatives à `base` (début du tampon).
    struct Image {
        mem: Vec<u64>,
    }

    impl Image {
        fn new() -> Self {
            Self {
                mem: vec![0u64; 0x1000 / 8],
            }
        }
        fn base(&self) -> usize {
            self.mem.as_ptr() as usize
        }
        fn put<T: Copy>(&mut self, rva: usize, v: T) {
            assert!(rva + size_of::<T>() <= 0x1000);
            // SAFETY: écriture bornée dans le tampon.
            unsafe { (self.mem.as_mut_ptr().cast::<u8>().add(rva) as *mut T).write_unaligned(v) };
        }
        fn td(&mut self, rva: usize, name: &str) -> *const c_void {
            let mut bytes = name.as_bytes().to_vec();
            bytes.push(0);
            for (i, b) in bytes.iter().enumerate() {
                self.put(rva + TD_NAME_OFF + i, *b);
            }
            (self.base() + rva) as *const c_void
        }
        fn bcd(&mut self, rva: usize, td: usize, contained: u32, w: Pmd, attr: u32) {
            self.put(
                rva,
                BaseClassDescriptor {
                    p_type_descriptor: td as i32,
                    num_contained_bases: contained,
                    where_: w,
                    attributes: attr,
                    p_class_descriptor: 0,
                },
            );
        }
        fn hierarchy(&mut self, chd: usize, attr: u32, bca: usize, bcds: &[usize]) {
            for (i, b) in bcds.iter().enumerate() {
                self.put(bca + i * 4, *b as i32);
            }
            self.put(
                chd,
                ClassHierarchyDescriptor {
                    signature: 0,
                    attributes: attr,
                    num_base_classes: bcds.len() as u32,
                    p_base_class_array: bca as i32,
                },
            );
        }
        /// COL + vtable (entrée -1 = COL) ; renvoie l'adresse de la vtable.
        fn col_vt(&mut self, col: usize, offset: u32, td: usize, chd: usize, vt: usize) -> u64 {
            self.put(
                col,
                CompleteObjectLocator {
                    signature: 1,
                    offset,
                    cd_offset: 0,
                    p_type_descriptor: td as i32,
                    p_class_descriptor: chd as i32,
                    p_self: col as i32,
                },
            );
            self.put(vt - 8, (self.base() + col) as u64);
            (self.base() + vt) as u64
        }
    }

    const NOVB: Pmd = Pmd {
        mdisp: 0,
        pdisp: -1,
        vdisp: 0,
    };

    #[test]
    fn rtti_single_inheritance() {
        // Derived : Mid : Base
        let mut img = Image::new();
        let base_td = img.td(0x100, ".?AVBase@@");
        let mid_td = img.td(0x140, ".?AVMid@@");
        let der_td = img.td(0x180, ".?AVDerived@@");
        let other_td = img.td(0x1C0, ".?AVOther@@");
        img.bcd(0x200, 0x180, 2, NOVB, 0x40);
        img.bcd(0x21C, 0x140, 1, NOVB, 0x40);
        img.bcd(0x238, 0x100, 0, NOVB, 0x40);
        img.hierarchy(0x300, 0, 0x320, &[0x200, 0x21C, 0x238]);
        let vt = img.col_vt(0x340, 0, 0x180, 0x300, 0x368);
        let obj: [u64; 2] = [vt, 0];
        let p = obj.as_ptr() as *mut c_void;
        assert_eq!(rt_dynamic_cast(p, 0, base_td, der_td, 0), p);
        assert_eq!(rt_dynamic_cast(p, 0, base_td, mid_td, 0), p);
        assert!(rt_dynamic_cast(p, 0, base_td, other_td, 0).is_null());
        assert!(rt_dynamic_cast(ptr::null_mut(), 0, base_td, der_td, 1).is_null());
        // Comparaison par nom (TypeDescriptor dupliqué dans un autre « module »).
        let mut other = Image::new();
        let der_copy = other.td(0x100, ".?AVDerived@@");
        assert_eq!(rt_dynamic_cast(p, 0, base_td, der_copy, 0), p);
    }

    #[test]
    fn rtti_multiple_inheritance_down_and_cross_cast() {
        // C : A, B  (A @0, B @8)
        let mut img = Image::new();
        let a_td = img.td(0x100, ".?AVA@@");
        let b_td = img.td(0x140, ".?AVB@@");
        let c_td = img.td(0x180, ".?AVC@@");
        let x_td = img.td(0x1C0, ".?AVX@@");
        img.bcd(0x200, 0x180, 2, NOVB, 0x40);
        img.bcd(0x21C, 0x100, 0, NOVB, 0x40);
        img.bcd(
            0x238,
            0x140,
            0,
            Pmd {
                mdisp: 8,
                pdisp: -1,
                vdisp: 0,
            },
            0x40,
        );
        img.hierarchy(0x300, CHD_MULTINH, 0x320, &[0x200, 0x21C, 0x238]);
        let vt_a = img.col_vt(0x340, 0, 0x180, 0x300, 0x368);
        let vt_b = img.col_vt(0x380, 8, 0x180, 0x300, 0x3A8);
        let obj: [u64; 2] = [vt_a, vt_b];
        let full = obj.as_ptr() as usize;
        let as_b = (full + 8) as *mut c_void;
        assert_eq!(rt_dynamic_cast(as_b, 0, b_td, c_td, 0) as usize, full);
        assert_eq!(rt_dynamic_cast(as_b, 0, b_td, a_td, 0) as usize, full);
        assert_eq!(
            rt_dynamic_cast(full as *mut c_void, 0, a_td, b_td, 0) as usize,
            full + 8
        );
        assert!(rt_dynamic_cast(as_b, 0, b_td, x_td, 0).is_null());
    }

    #[test]
    fn rtti_virtual_inheritance() {
        // D : virtual V  -> [vbptr @0][V (vptr) @8], vbtable {0, 8}
        let mut img = Image::new();
        let v_td = img.td(0x100, ".?AVV@@");
        let d_td = img.td(0x140, ".?AVD@@");
        img.bcd(0x200, 0x140, 1, NOVB, 0x40);
        img.bcd(
            0x21C,
            0x100,
            0,
            Pmd {
                mdisp: 0,
                pdisp: 0,
                vdisp: 4,
            },
            0x50,
        );
        img.hierarchy(0x300, CHD_VIRTINH, 0x320, &[0x200, 0x21C]);
        let vt_v = img.col_vt(0x340, 8, 0x140, 0x300, 0x368);
        let vbtable: [i32; 2] = [0, 8];
        let obj: [u64; 2] = [vbtable.as_ptr() as u64, vt_v];
        let full = obj.as_ptr() as usize;
        let as_v = (full + 8) as *mut c_void;
        assert_eq!(rt_dynamic_cast(as_v, 0, v_td, d_td, 0) as usize, full);
    }
}
