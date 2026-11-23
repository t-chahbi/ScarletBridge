//! Divers kernel32/kernelbase : comparaisons `lstrcmp*`, WER, notifications d'état
//! d'application, pipes nommés (absents), `GetModuleFileName*`, `LoadLibraryEx*`
//! (y compris `LOAD_LIBRARY_AS_DATAFILE` / `AS_IMAGE_RESOURCE`).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use sb_common::abi::{E_INVALIDARG, HResult, S_OK, WIN_FALSE, WinBool};
use sb_common::wide::from_pcwstr;
use sb_common::{ffi_guard, sb_debug};
use sb_loader::{module_manager, set_last_error};

use super::{
    CHANNEL, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_HANDLE,
    ERROR_INVALID_PARAMETER, ERROR_MOD_NOT_FOUND, ERROR_SUCCESS, ansi_to_string, write_out,
};

// ==================== lstrcmp* ====================

fn wide_slice<'a>(p: *const u16) -> Option<&'a [u16]> {
    if p.is_null() {
        return None;
    }
    let mut n = 0;
    // SAFETY: chaîne UTF-16 terminée par nul fournie par l'invité.
    while unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: `n` unités lisibles.
    Some(unsafe { std::slice::from_raw_parts(p, n) })
}

/// Comparaison « linguistique » simplifiée de `CompareString` : insensible à la casse en
/// premier, puis minuscule < majuscule pour départager (comme `lstrcmp`).
fn compare_strings(a: &str, b: &str, ignore_case: bool) -> i32 {
    let la: Vec<char> = a.chars().flat_map(char::to_lowercase).collect();
    let lb: Vec<char> = b.chars().flat_map(char::to_lowercase).collect();
    let ord = la.cmp(&lb).then_with(|| {
        if ignore_case {
            std::cmp::Ordering::Equal
        } else {
            // Départage : à position égale, la minuscule précède la majuscule.
            a.chars()
                .zip(b.chars())
                .find(|(x, y)| x != y)
                .map(|(x, _)| {
                    if x.is_lowercase() {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    }
                })
                .unwrap_or(std::cmp::Ordering::Equal)
        }
    });
    ord as i32
}

fn lstrcmp_common(a: Option<String>, b: Option<String>, ignore_case: bool) -> i32 {
    match (a, b) {
        (None, None) => 0,
        (None, Some(_)) => -1,
        (Some(_), None) => 1,
        (Some(x), Some(y)) => compare_strings(&x, &y, ignore_case),
    }
}

pub(super) extern "win64" fn lstrcmpW(a: *const u16, b: *const u16) -> i32 {
    ffi_guard!(CHANNEL, "lstrcmpW", 0, {
        let (x, y) = (wide_slice(a), wide_slice(b));
        lstrcmp_common(
            x.map(String::from_utf16_lossy),
            y.map(String::from_utf16_lossy),
            false,
        )
    })
}

pub(super) extern "win64" fn lstrcmpiW(a: *const u16, b: *const u16) -> i32 {
    ffi_guard!(CHANNEL, "lstrcmpiW", 0, {
        let (x, y) = (wide_slice(a), wide_slice(b));
        lstrcmp_common(
            x.map(String::from_utf16_lossy),
            y.map(String::from_utf16_lossy),
            true,
        )
    })
}

pub(super) extern "win64" fn lstrcmpA(a: *const i8, b: *const i8) -> i32 {
    ffi_guard!(CHANNEL, "lstrcmpA", 0, {
        lstrcmp_common(ansi_to_string(a), ansi_to_string(b), false)
    })
}

pub(super) extern "win64" fn lstrcmpiA(a: *const i8, b: *const i8) -> i32 {
    ffi_guard!(CHANNEL, "lstrcmpiA", 0, {
        lstrcmp_common(ansi_to_string(a), ansi_to_string(b), true)
    })
}

pub(super) extern "win64" fn lstrlenA(s: *const i8) -> i32 {
    ffi_guard!(CHANNEL, "lstrlenA", 0, {
        if s.is_null() {
            return 0;
        }
        // SAFETY: chaîne C terminée par nul.
        unsafe { std::ffi::CStr::from_ptr(s) }.to_bytes().len() as i32
    })
}

// ==================== WER ====================

/// `WerRegisterFile` : aucun rapport WER n'est jamais émis sous ScarletBridge ; l'enregistrement
/// est accepté (S_OK) après validation des paramètres, sans effet.
pub(super) extern "win64" fn WerRegisterFile(
    path: *const u16,
    file_type: u32,
    flags: u32,
) -> HResult {
    ffi_guard!(CHANNEL, "WerRegisterFile", E_INVALIDARG, {
        if path.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: `path` non-nul.
        let p = unsafe { from_pcwstr(path) }.unwrap_or_default();
        sb_debug!(
            CHANNEL,
            "WerRegisterFile('{p}', type={file_type}, flags={flags:#x}) ignoré"
        );
        S_OK
    })
}

pub(super) extern "win64" fn WerUnregisterFile(path: *const u16) -> HResult {
    if path.is_null() { E_INVALIDARG } else { S_OK }
}

// ==================== Notifications d'état d'application ====================

#[derive(Clone, Copy)]
struct Registration {
    routine: usize,
    context: usize,
}

static NEXT_REG: AtomicUsize = AtomicUsize::new(0x5B00_0010);
static APP_STATE: LazyLock<Mutex<HashMap<usize, Registration>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static APP_CONSTRAINED: LazyLock<Mutex<HashMap<usize, Registration>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn register(
    table: &Mutex<HashMap<usize, Registration>>,
    routine: usize,
    context: usize,
    out: *mut usize,
) -> u32 {
    if routine == 0 || out.is_null() {
        return ERROR_INVALID_PARAMETER;
    }
    let h = NEXT_REG.fetch_add(0x10, Ordering::AcqRel);
    table.lock().insert(h, Registration { routine, context });
    write_out(out, h);
    ERROR_SUCCESS
}

fn notify(table: &Mutex<HashMap<usize, Registration>>, flag: bool) -> usize {
    let regs: Vec<Registration> = table.lock().values().copied().collect();
    for r in &regs {
        // SAFETY: routine `void (BOOLEAN, PVOID)` Win64 enregistrée par l'invité.
        let f: unsafe extern "win64" fn(u8, usize) = unsafe { core::mem::transmute(r.routine) };
        // SAFETY: appel du callback invité avec son contexte.
        unsafe { f(u8::from(flag), r.context) };
    }
    regs.len()
}

/// Notifie les routines `RegisterAppStateChangeNotification` (suspension = `quiesced`).
/// API destinée au cycle de vie `sb-runtime` (suspension / reprise).
#[allow(dead_code)]
pub(crate) fn notify_app_state_change(quiesced: bool) -> usize {
    notify(&APP_STATE, quiesced)
}

/// Notifie les routines `RegisterAppConstrainedChangeNotification`.
#[allow(dead_code)]
pub(crate) fn notify_app_constrained_change(constrained: bool) -> usize {
    notify(&APP_CONSTRAINED, constrained)
}

pub(super) extern "win64" fn RegisterAppStateChangeNotification(
    routine: usize,
    context: usize,
    out: *mut usize,
) -> u32 {
    ffi_guard!(
        CHANNEL,
        "RegisterAppStateChangeNotification",
        ERROR_INVALID_PARAMETER,
        { register(&APP_STATE, routine, context, out) }
    )
}

pub(super) extern "win64" fn UnregisterAppStateChangeNotification(reg: usize) {
    ffi_guard!(CHANNEL, "UnregisterAppStateChangeNotification", (), {
        APP_STATE.lock().remove(&reg);
    })
}

pub(super) extern "win64" fn RegisterAppConstrainedChangeNotification(
    routine: usize,
    context: usize,
    out: *mut usize,
) -> u32 {
    ffi_guard!(
        CHANNEL,
        "RegisterAppConstrainedChangeNotification",
        ERROR_INVALID_PARAMETER,
        { register(&APP_CONSTRAINED, routine, context, out) }
    )
}

pub(super) extern "win64" fn UnregisterAppConstrainedChangeNotification(reg: usize) {
    ffi_guard!(CHANNEL, "UnregisterAppConstrainedChangeNotification", (), {
        APP_CONSTRAINED.lock().remove(&reg);
    })
}

// ==================== Pipes nommés (non pris en charge) ====================

/// Aucun pipe nommé n'existe dans le bac à sable : tout handle est invalide pour cette API.
pub(super) extern "win64" fn SetNamedPipeHandleState(
    pipe: usize,
    mode: *const u32,
    max_collection: *const u32,
    timeout: *const u32,
) -> WinBool {
    set_last_error(ERROR_INVALID_HANDLE);
    WIN_FALSE
}

/// Aucun serveur de pipe nommé : `ERROR_FILE_NOT_FOUND` (nom inexistant) ou
/// `ERROR_INVALID_PARAMETER` si le nom est absent.
pub(super) extern "win64" fn WaitNamedPipeW(name: *const u16, timeout: u32) -> WinBool {
    ffi_guard!(CHANNEL, "WaitNamedPipeW", WIN_FALSE, {
        // SAFETY: `name` est un `PCWSTR` valide ou nul.
        let code = if unsafe { from_pcwstr(name) }.is_some() {
            ERROR_FILE_NOT_FOUND
        } else {
            ERROR_INVALID_PARAMETER
        };
        set_last_error(code);
        WIN_FALSE
    })
}

pub(super) extern "win64" fn WaitNamedPipeA(name: *const i8, timeout: u32) -> WinBool {
    set_last_error(if name.is_null() {
        ERROR_INVALID_PARAMETER
    } else {
        ERROR_FILE_NOT_FOUND
    });
    WIN_FALSE
}

// ==================== Noms de modules ====================

/// Chemin invité d'un module (jamais un chemin hôte) : `G:\...` pour les images du jeu,
/// `C:\Windows\System32\<dll>` pour les DLL système virtuelles.
pub(crate) fn guest_module_path(module: usize) -> Option<String> {
    let mm = module_manager();
    let base = if module == 0 {
        mm.main_module_base()
    } else {
        module
    };
    if let Some(m) = mm.find_by_base(base) {
        let st = crate::process::process_state().read();
        let rel = st.vfs.as_ref().and_then(|vfs| {
            m.path
                .strip_prefix(vfs.mount_root(sb_vfs::VfsMount::Game))
                .ok()
                .map(|r| r.to_string_lossy().replace('/', "\\"))
        });
        let rel = rel.unwrap_or_else(|| {
            m.path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| m.name.clone())
        });
        return Some(format!("G:\\{rel}"));
    }
    if let Some(name) = mm.virtual_module_name(base) {
        return Some(format!("C:\\Windows\\System32\\{name}"));
    }
    if module == 0 {
        return Some("G:\\app.exe".to_string());
    }
    None
}

/// Copie selon la convention `GetModuleFileName` : troncature → `nSize` + `ERROR_INSUFFICIENT_BUFFER`.
fn copy_module_name<T: Copy + Default>(units: &[T], buf: *mut T, size: u32) -> u32 {
    if buf.is_null() || size == 0 {
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    let cap = size as usize;
    let n = units.len().min(cap - 1);
    // SAFETY: `buf` dispose de `size` éléments.
    unsafe {
        std::ptr::copy_nonoverlapping(units.as_ptr(), buf, n);
        *buf.add(n) = T::default();
    }
    if units.len() >= cap {
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        size
    } else {
        set_last_error(ERROR_SUCCESS);
        n as u32
    }
}

pub(crate) fn module_file_name_w(module: usize, buf: *mut u16, size: u32) -> u32 {
    match guest_module_path(module) {
        Some(p) => copy_module_name(&p.encode_utf16().collect::<Vec<_>>(), buf, size),
        None => {
            set_last_error(ERROR_MOD_NOT_FOUND);
            0
        }
    }
}

pub(super) extern "win64" fn GetModuleFileNameA(module: usize, buf: *mut u8, size: u32) -> u32 {
    ffi_guard!(CHANNEL, "GetModuleFileNameA", 0, {
        match guest_module_path(module) {
            Some(p) => copy_module_name(p.as_bytes(), buf, size),
            None => {
                set_last_error(ERROR_MOD_NOT_FOUND);
                0
            }
        }
    })
}

// ==================== LoadLibraryEx ====================

const DONT_RESOLVE_DLL_REFERENCES: u32 = 0x0000_0001;
const LOAD_LIBRARY_AS_DATAFILE: u32 = 0x0000_0002;
const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;
const LOAD_IGNORE_CODE_AUTHZ_LEVEL: u32 = 0x0000_0010;
const LOAD_LIBRARY_AS_IMAGE_RESOURCE: u32 = 0x0000_0020;
const LOAD_LIBRARY_AS_DATAFILE_EXCLUSIVE: u32 = 0x0000_0040;
const LOAD_LIBRARY_REQUIRE_SIGNED_TARGET: u32 = 0x0000_0080;
const LOAD_LIBRARY_SEARCH_FLAGS: u32 = 0x0000_1F00;
const LOAD_LIBRARY_SAFE_CURRENT_DIRS: u32 = 0x0000_2000;
const LOAD_LIBRARY_OS_INTEGRITY_CONTINUITY: u32 = 0x0000_8000;
const VALID_FLAGS: u32 = DONT_RESOLVE_DLL_REFERENCES
    | LOAD_LIBRARY_AS_DATAFILE
    | LOAD_WITH_ALTERED_SEARCH_PATH
    | LOAD_IGNORE_CODE_AUTHZ_LEVEL
    | LOAD_LIBRARY_AS_IMAGE_RESOURCE
    | LOAD_LIBRARY_AS_DATAFILE_EXCLUSIVE
    | LOAD_LIBRARY_REQUIRE_SIGNED_TARGET
    | LOAD_LIBRARY_SEARCH_FLAGS
    | LOAD_LIBRARY_SAFE_CURRENT_DIRS
    | LOAD_LIBRARY_OS_INTEGRITY_CONTINUITY;

/// Modules chargés en tant que données : handle (avec bits bas) -> (adresse, taille).
static DATAFILES: LazyLock<Mutex<HashMap<usize, (usize, usize)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn rd16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}

fn rd32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// Disposition « image » (sections à leurs RVA) d'un PE brut.
fn layout_as_image(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.get(0..2)? != b"MZ" {
        return None;
    }
    let pe = rd32(raw, 0x3C)? as usize;
    if raw.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let nsec = usize::from(rd16(raw, pe + 6)?);
    let opt_size = usize::from(rd16(raw, pe + 20)?);
    let opt = pe + 24;
    let size_of_image = rd32(raw, opt + 56)? as usize;
    let size_of_headers = (rd32(raw, opt + 60)? as usize).min(raw.len());
    if size_of_image == 0 || size_of_image > 1 << 30 {
        return None;
    }
    let mut img = vec![0u8; size_of_image];
    let h = size_of_headers.min(size_of_image);
    img[..h].copy_from_slice(&raw[..h]);
    let sec = opt + opt_size;
    for i in 0..nsec {
        let s = sec + i * 40;
        let va = rd32(raw, s + 12)? as usize;
        let raw_size = rd32(raw, s + 16)? as usize;
        let raw_ptr = rd32(raw, s + 20)? as usize;
        let n = raw_size
            .min(raw.len().saturating_sub(raw_ptr))
            .min(size_of_image.saturating_sub(va));
        if n > 0 {
            img[va..va + n].copy_from_slice(&raw[raw_ptr..raw_ptr + n]);
        }
    }
    Some(img)
}

fn load_as_data(name: &str, as_image: bool) -> usize {
    let Some(host) = crate::file_io::resolve_vfs_path(name) else {
        set_last_error(ERROR_MOD_NOT_FOUND);
        return 0;
    };
    let Ok(raw) = std::fs::read(&host) else {
        set_last_error(ERROR_MOD_NOT_FOUND);
        return 0;
    };
    let bytes = if as_image {
        match layout_as_image(&raw) {
            Some(b) => b,
            None => {
                set_last_error(193); // ERROR_BAD_EXE_FORMAT
                return 0;
            }
        }
    } else {
        raw
    };
    let len = bytes.len().max(1).next_multiple_of(4096);
    // SAFETY: mapping anonyme RW, rempli puis protégé en lecture seule.
    let p = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        set_last_error(super::ERROR_NOT_ENOUGH_MEMORY);
        return 0;
    }
    // SAFETY: `p` dispose de `len >= bytes.len()` octets.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
        libc::mprotect(p, len, libc::PROT_READ);
    }
    // Bit 0 = datafile, bit 1 = image resource (convention LDR_IS_DATAFILE/IMAGEMAPPING).
    let handle = p as usize | if as_image { 2 } else { 1 };
    DATAFILES.lock().insert(handle, (p as usize, len));
    set_last_error(ERROR_SUCCESS);
    handle
}

/// Libère un module chargé comme données ; `false` si `handle` n'en est pas un.
pub(crate) fn free_datafile_module(handle: usize) -> bool {
    let Some((addr, len)) = DATAFILES.lock().remove(&handle) else {
        return false;
    };
    // SAFETY: mapping créé par `load_as_data`.
    unsafe { libc::munmap(addr as *mut libc::c_void, len) };
    true
}

fn load_library_ex(name: Option<String>, file: usize, flags: u32) -> usize {
    let Some(name) = name else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    if file != 0 || (flags & !VALID_FLAGS) != 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let data_flags = LOAD_LIBRARY_AS_DATAFILE
        | LOAD_LIBRARY_AS_DATAFILE_EXCLUSIVE
        | LOAD_LIBRARY_AS_IMAGE_RESOURCE;
    if (flags & data_flags) != 0 {
        // Module déjà chargé comme exécutable : son handle normal est retourné.
        if let Some(m) = module_manager().find_by_name(&name) {
            set_last_error(ERROR_SUCCESS);
            return m.base;
        }
        if let Some(h) = module_manager().virtual_module_handle(&name) {
            set_last_error(ERROR_SUCCESS);
            return h;
        }
        return load_as_data(&name, (flags & LOAD_LIBRARY_AS_IMAGE_RESOURCE) != 0);
    }
    // Les drapeaux de recherche et DONT_RESOLVE_DLL_REFERENCES n'influencent pas le loader HLE.
    crate::kernel32::load_library_impl(&name)
}

pub(super) extern "win64" fn LoadLibraryExW(name: *const u16, file: usize, flags: u32) -> usize {
    ffi_guard!(CHANNEL, "LoadLibraryExW", 0, {
        // SAFETY: `name` est un `PCWSTR` valide ou nul.
        load_library_ex(unsafe { from_pcwstr(name) }, file, flags)
    })
}

pub(super) extern "win64" fn LoadLibraryExA(name: *const i8, file: usize, flags: u32) -> usize {
    ffi_guard!(CHANNEL, "LoadLibraryExA", 0, {
        load_library_ex(ansi_to_string(name), file, flags)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_common::wide::to_wide_null;
    use sb_loader::last_error;

    #[test]
    fn lstrcmp_semantics() {
        let a = to_wide_null("apple");
        let b = to_wide_null("Banana");
        let a2 = to_wide_null("Apple");
        assert!(lstrcmpW(a.as_ptr(), b.as_ptr()) < 0);
        assert!(lstrcmpW(b.as_ptr(), a.as_ptr()) > 0);
        assert!(
            lstrcmpW(a.as_ptr(), a2.as_ptr()) < 0,
            "minuscule avant majuscule"
        );
        assert_eq!(lstrcmpiW(a.as_ptr(), a2.as_ptr()), 0);
        assert_eq!(lstrcmpW(a.as_ptr(), a.as_ptr()), 0);
        assert_eq!(lstrcmpW(core::ptr::null(), a.as_ptr()), -1);
        assert_eq!(lstrcmpW(core::ptr::null(), core::ptr::null()), 0);
        assert_eq!(lstrcmpiA(c"HELLO".as_ptr(), c"hello".as_ptr()), 0);
        assert!(lstrcmpA(c"abc".as_ptr(), c"abd".as_ptr()) < 0);
        assert_eq!(lstrlenA(c"four".as_ptr()), 4);
    }

    static HITS: AtomicUsize = AtomicUsize::new(0);
    extern "win64" fn on_state(flag: u8, ctx: usize) {
        HITS.fetch_add(ctx + usize::from(flag), Ordering::SeqCst);
    }

    #[test]
    fn wer_and_app_state_notifications() {
        super::super::init_test_thread();
        let p = to_wide_null("G:\\log.txt");
        assert_eq!(WerRegisterFile(p.as_ptr(), 1, 0), S_OK);
        assert_eq!(WerRegisterFile(core::ptr::null(), 1, 0), E_INVALIDARG);

        let mut reg = 0usize;
        assert_eq!(
            RegisterAppStateChangeNotification(on_state as *const () as usize, 100, &mut reg),
            ERROR_SUCCESS
        );
        assert_ne!(reg, 0);
        assert!(notify_app_state_change(true) >= 1);
        assert!(HITS.load(Ordering::SeqCst) >= 101);
        UnregisterAppStateChangeNotification(reg);
        let mut reg2 = 0usize;
        assert_eq!(
            RegisterAppConstrainedChangeNotification(on_state as *const () as usize, 0, &mut reg2),
            ERROR_SUCCESS
        );
        UnregisterAppConstrainedChangeNotification(reg2);
        assert_eq!(
            RegisterAppStateChangeNotification(0, 0, &mut reg),
            ERROR_INVALID_PARAMETER
        );
    }

    #[test]
    fn pipes_and_module_names() {
        super::super::init_test_thread();
        assert_eq!(
            SetNamedPipeHandleState(
                0x1234,
                core::ptr::null(),
                core::ptr::null(),
                core::ptr::null()
            ),
            0
        );
        assert_eq!(last_error(), ERROR_INVALID_HANDLE);
        let n = to_wide_null("\\\\.\\pipe\\nothing");
        assert_eq!(WaitNamedPipeW(n.as_ptr(), 0), 0);
        assert_eq!(last_error(), ERROR_FILE_NOT_FOUND);

        let mut buf = [0u8; 64];
        let len = GetModuleFileNameA(0, buf.as_mut_ptr(), 64);
        assert!(len > 3);
        assert!(buf[..len as usize].starts_with(b"G:\\"));
        let mut small = [0u8; 3];
        assert_eq!(GetModuleFileNameA(0, small.as_mut_ptr(), 3), 3);
        assert_eq!(last_error(), ERROR_INSUFFICIENT_BUFFER);
        assert_eq!(small[2], 0);
        assert_eq!(GetModuleFileNameA(0x1230_0000, buf.as_mut_ptr(), 64), 0);
        assert_eq!(last_error(), ERROR_MOD_NOT_FOUND);
    }

    #[test]
    fn load_library_ex_flags_and_image_layout() {
        super::super::init_test_thread();
        let n = to_wide_null("whatever.dll");
        assert_eq!(LoadLibraryExW(n.as_ptr(), 0x1234, 0), 0);
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        assert_eq!(LoadLibraryExW(n.as_ptr(), 0, 0x0100_0000), 0);
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);

        // PE minimal : en-têtes 0x200, une section à RVA 0x1000 (raw 0x200, 0x10 octets).
        let mut raw = vec![0u8; 0x210];
        raw[0..2].copy_from_slice(b"MZ");
        raw[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        raw[0x40..0x44].copy_from_slice(b"PE\0\0");
        raw[0x46..0x48].copy_from_slice(&1u16.to_le_bytes());
        raw[0x54..0x56].copy_from_slice(&0xF0u16.to_le_bytes());
        let opt = 0x58;
        raw[opt + 56..opt + 60].copy_from_slice(&0x2000u32.to_le_bytes());
        raw[opt + 60..opt + 64].copy_from_slice(&0x200u32.to_le_bytes());
        let s = opt + 0xF0;
        raw[s + 12..s + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        raw[s + 16..s + 20].copy_from_slice(&0x10u32.to_le_bytes());
        raw[s + 20..s + 24].copy_from_slice(&0x200u32.to_le_bytes());
        raw[0x200..0x210].copy_from_slice(b"SECTION-PAYLOAD!");
        let img = layout_as_image(&raw).expect("image");
        assert_eq!(img.len(), 0x2000);
        assert_eq!(&img[0x1000..0x1010], b"SECTION-PAYLOAD!");
        assert!(layout_as_image(b"not a pe").is_none());
        assert!(!free_datafile_module(0x1001));
    }
}
