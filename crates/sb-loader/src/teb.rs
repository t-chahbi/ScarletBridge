//! Structures Windows x86-64 `TEB` (Thread Environment Block) et `PEB` (Process Environment Block)
//! montées sur le registre segment `GS` via `arch_prctl(ARCH_SET_GS)` sous Linux x86-64.
//!
//! Sous Linux x86-64, `FS` est utilisé par glibc/Rust TLS ; `GS` est libre pour émuler le TEB Windows.

use std::cell::RefCell;
use std::mem::offset_of;
use std::ptr;
use std::sync::OnceLock;

use parking_lot::Mutex;

const ARCH_SET_GS: libc::c_long = 0x1001;
const ARCH_GET_GS: libc::c_long = 0x1004;

/// Process Environment Block Windows x86-64 minimal (offsets MSVC vérifiés).
#[repr(C, align(64))]
pub struct Peb {
    pub inherited_address_space: u8,
    pub read_image_file_exec_options: u8,
    pub being_debugged: u8,
    pub bit_field: u8,
    pub _pad0: u32,
    pub mutant: usize,
    pub image_base_address: usize,
    pub ldr: usize,
    pub process_parameters: usize,
    pub sub_system_data: usize,
    pub process_heap: usize,
    pub _reserved: [u8; 0x118 - 0x38],
    pub os_major_version: u32,
    pub os_minor_version: u32,
    pub os_build_number: u16,
    pub os_csd_version: u16,
    pub os_platform_id: u32,
}

const _: () = {
    assert!(offset_of!(Peb, being_debugged) == 0x02);
    assert!(offset_of!(Peb, image_base_address) == 0x10);
    assert!(offset_of!(Peb, process_parameters) == 0x20);
    assert!(offset_of!(Peb, process_heap) == 0x30);
    assert!(offset_of!(Peb, os_major_version) == 0x118);
    assert!(offset_of!(Peb, os_minor_version) == 0x11C);
    assert!(offset_of!(Peb, os_build_number) == 0x120);
};

/// Thread Environment Block Windows x86-64 (offsets ABI Win64 vérifiés).
#[repr(C, align(4096))]
pub struct Teb {
    pub exception_list: usize,           // 0x00
    pub stack_base: usize,               // 0x08
    pub stack_limit: usize,              // 0x10
    pub sub_system_tib: usize,           // 0x18
    pub fiber_data: usize,               // 0x20
    pub arbitrary_user_pointer: usize,   // 0x28
    pub self_ptr: usize,                 // 0x30 (NtCurrentTeb)
    pub environment_pointer: usize,      // 0x38
    pub process_id: usize,               // 0x40
    pub thread_id: usize,                // 0x48
    pub active_rpc_handle: usize,        // 0x50
    pub thread_local_storage_ptr: usize, // 0x58 (pointe vers le tableau de pointeurs .tls)
    pub peb: usize,                      // 0x60
    pub last_error_value: u32,           // 0x68
    pub count_of_owned_critical_sections: u32,
    pub _reserved1: [u8; 0x1480 - 0x70],
    pub tls_slots: [usize; 64],        // 0x1480
    pub tls_module_array: [usize; 64], // Tableau interne pointé par `thread_local_storage_ptr`
}

const _: () = {
    assert!(offset_of!(Teb, exception_list) == 0x00);
    assert!(offset_of!(Teb, stack_base) == 0x08);
    assert!(offset_of!(Teb, stack_limit) == 0x10);
    assert!(offset_of!(Teb, self_ptr) == 0x30);
    assert!(offset_of!(Teb, process_id) == 0x40);
    assert!(offset_of!(Teb, thread_id) == 0x48);
    assert!(offset_of!(Teb, thread_local_storage_ptr) == 0x58);
    assert!(offset_of!(Teb, peb) == 0x60);
    assert!(offset_of!(Teb, last_error_value) == 0x68);
    assert!(offset_of!(Teb, tls_slots) == 0x1480);
};

struct GlobalPeb(Box<Peb>);
// SAFETY: le PEB est initialisé une seule fois et ses champs scalaires sont accédés en lecture
// par les threads invités.
unsafe impl Send for GlobalPeb {}
unsafe impl Sync for GlobalPeb {}

static PEB: OnceLock<Mutex<GlobalPeb>> = OnceLock::new();

#[repr(C, align(4096))]
struct ProcessHeapPage([u8; 4096]);

static PROCESS_HEAP_HANDLE: OnceLock<usize> = OnceLock::new();

/// Handle du tas processus principal (`PEB.ProcessHeap` / `GetProcessHeap()`).
///
/// Sur Windows x64, `GetProcessHeap()` retourne un pointeur vers une structure `_HEAP`
/// mappée en mémoire ; une sentinelle non mappée (ex. `0x1_0000_0000`) provoque un `SIGSEGV`
/// dès qu'un runtime ou un outil de diagnostic lit `*GetProcessHeap()`. On adosse donc ce
/// handle à une vraie page 4 Kio mappée et mise à zéro, valable toute la durée du processus.
pub fn process_heap_handle() -> usize {
    *PROCESS_HEAP_HANDLE.get_or_init(|| {
        let page = Box::new(ProcessHeapPage([0u8; 4096]));
        Box::into_raw(page) as usize
    })
}

fn global_peb_ptr() -> *mut Peb {
    let lock = PEB.get_or_init(|| {
        let peb = Box::new(Peb {
            inherited_address_space: 0,
            read_image_file_exec_options: 0,
            being_debugged: 0,
            bit_field: 0,
            _pad0: 0,
            mutant: usize::MAX,
            image_base_address: 0,
            ldr: 0,
            process_parameters: 0,
            sub_system_data: 0,
            process_heap: process_heap_handle(),
            _reserved: [0u8; 0x118 - 0x38],
            os_major_version: 10,
            os_minor_version: 0,
            os_build_number: 19041,
            os_csd_version: 0,
            os_platform_id: 2,
        });
        Mutex::new(GlobalPeb(peb))
    });
    let mut guard = lock.lock();
    &mut *guard.0 as *mut Peb
}

/// Met à jour l'adresse de base de l'image principale dans le `PEB`.
pub fn set_peb_image_base(image_base: usize) {
    let peb_ptr = global_peb_ptr();
    // SAFETY: `global_peb_ptr` retourne un pointeur valide vers le `Peb` global.
    unsafe {
        (*peb_ptr).image_base_address = image_base;
    }
}

struct ThreadTeb {
    teb: Box<Teb>,
    /// Tampons `.tls` alloués spécifiquement pour ce thread (libérés automatiquement à la fin du thread).
    owned_tls_blocks: Vec<Box<[u8]>>,
}

/// Base du segment `GS` du thread courant, lue via `arch_prctl(ARCH_GET_GS)`.
///
/// Retourne `None` si l'appel échoue (plateforme non Linux, sandbox). Utilisé par le rapport de
/// crash pour vérifier qu'un thread exécutant du code invité possède bien un `TEB` ScarletBridge.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn gs_base() -> Option<usize> {
    let mut base: usize = 0;
    // SAFETY: `ARCH_GET_GS` écrit un `unsigned long` dans le pointeur fourni.
    let rc = unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_GET_GS, &mut base) };
    if rc == 0 { Some(base) } else { None }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub fn gs_base() -> Option<usize> {
    None
}

/// Décrit le `TEB` actif du thread courant pour le rapport de crash.
///
/// L'hôte n'expose pas de moyen sûr de lire une adresse arbitraire depuis un handler de signal :
/// on passe par `process_vm_readv` sur soi-même, qui échoue proprement (`EFAULT`) au lieu de
/// déclencher un second `SIGSEGV` si l'adresse n'est pas mappée.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn describe_current_teb() -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let gs = gs_base().unwrap_or(0);
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
    let _ = write!(out, "GSBase   : {gs:#018x}  tid={tid}");
    if let Ok(comm) = std::fs::read_to_string(format!("/proc/self/task/{tid}/comm")) {
        let _ = write!(out, "  comm={}", comm.trim_end());
    }
    let _ = writeln!(out);
    if gs == 0 {
        let _ = writeln!(
            out,
            "TEB      : ABSENT (GS base nulle : ce thread n'a jamais appelé init_thread_teb)"
        );
        return out;
    }
    match read_process_memory(gs, 0x70) {
        Some(buf) if buf.len() >= 0x70 => {
            let q = |off: usize| {
                u64::from_ne_bytes([
                    buf[off],
                    buf[off + 1],
                    buf[off + 2],
                    buf[off + 3],
                    buf[off + 4],
                    buf[off + 5],
                    buf[off + 6],
                    buf[off + 7],
                ]) as usize
            };
            let self_ptr = q(0x30);
            let _ = writeln!(
                out,
                "TEB      : self={self_ptr:#018x} stack_base={:#018x} stack_limit={:#018x} peb={:#018x}{}",
                q(0x08),
                q(0x10),
                q(0x60),
                if self_ptr == gs {
                    ""
                } else {
                    "  (self != GSBase : TEB invalide)"
                }
            );
        }
        _ => {
            let _ = writeln!(out, "TEB      : illisible à GSBase {gs:#018x}");
        }
    }
    out
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub fn describe_current_teb() -> String {
    String::new()
}

/// Lecture mémoire sans risque de faute (`process_vm_readv` sur le processus courant).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn read_process_memory(addr: usize, len: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let local = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: len,
    };
    let remote = libc::iovec {
        iov_base: addr as *mut libc::c_void,
        iov_len: len,
    };
    // SAFETY: les deux `iovec` sont valides ; `process_vm_readv` ne fait que copier.
    let n = unsafe { libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0) };
    if n < 0 {
        None
    } else {
        buf.truncate(n as usize);
        Some(buf)
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub fn read_process_memory(_addr: usize, _len: usize) -> Option<Vec<u8>> {
    None
}

thread_local! {
    static CURRENT_TEB: RefCell<Option<ThreadTeb>> = const { RefCell::new(None) };
}

/// Initialise le `TEB` Windows pour le thread Linux courant et configure `GS` via `arch_prctl`.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn init_thread_teb(image_base: usize) -> Result<*mut Teb, std::io::Error> {
    crate::signal::ensure_thread_altstack();
    let peb_ptr = global_peb_ptr();
    if image_base != 0 {
        set_peb_image_base(image_base);
    }

    let stack_probe = 0usize;
    let approx_sp = (&raw const stack_probe) as usize;
    let stack_base = (approx_sp + 0x40000) & !0xFFF;
    let stack_limit = approx_sp.saturating_sub(0x80_0000) & !0xFFF;
    // SAFETY: appels POSIX sans précondition.
    let pid = unsafe { libc::getpid() } as usize;
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as usize;

    let mut teb = Box::new(Teb {
        exception_list: usize::MAX,
        stack_base,
        stack_limit,
        sub_system_tib: 0,
        fiber_data: 0,
        arbitrary_user_pointer: 0,
        self_ptr: 0,
        environment_pointer: 0,
        process_id: pid,
        thread_id: tid,
        active_rpc_handle: 0,
        thread_local_storage_ptr: 0,
        peb: peb_ptr as usize,
        last_error_value: 0,
        count_of_owned_critical_sections: 0,
        _reserved1: [0u8; 0x1480 - 0x70],
        tls_slots: [0usize; 64],
        tls_module_array: [0usize; 64],
    });

    let teb_ptr = (&raw mut *teb) as usize;
    teb.self_ptr = teb_ptr;
    teb.thread_local_storage_ptr = (&raw mut teb.tls_module_array) as usize;

    // SAFETY: `ARCH_SET_GS` modifie uniquement `GS_BASE` du thread courant (n'affecte pas `FS`).
    let rc = unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_SET_GS, teb_ptr) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }

    let raw_ptr = teb_ptr as *mut Teb;
    CURRENT_TEB.with(|cell| {
        *cell.borrow_mut() = Some(ThreadTeb {
            teb,
            owned_tls_blocks: Vec::new(),
        });
    });

    Ok(raw_ptr)
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub fn init_thread_teb(_image_base: usize) -> Result<*mut Teb, std::io::Error> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "TEB GS supporté uniquement sur Linux x86-64",
    ))
}

/// Enregistre le tampon `.tls` d'un module PE dans le slot `slot_index` du thread courant.
pub fn set_module_tls_slot(slot_index: usize, data_ptr: *mut u8) {
    CURRENT_TEB.with(|cell| {
        if let Some(ref mut holder) = *cell.borrow_mut()
            && slot_index < holder.teb.tls_module_array.len()
        {
            holder.teb.tls_module_array[slot_index] = data_ptr as usize;
        }
    });
}

/// Alloue et attache un tampon `.tls` possédé par le thread courant dans `tls_module_array[slot_index]`.
pub fn attach_owned_module_tls(slot_index: usize, mut block: Box<[u8]>) {
    let ptr = block.as_mut_ptr() as usize;
    CURRENT_TEB.with(|cell| {
        if let Some(ref mut holder) = *cell.borrow_mut()
            && slot_index < holder.teb.tls_module_array.len()
        {
            holder.teb.tls_module_array[slot_index] = ptr;
            holder.owned_tls_blocks.push(block);
        }
    });
}

/// Lit un slot `TlsGetValue` directement dans le TEB courant (slots 0..64).
pub fn get_teb_tls_slot(slot: u32) -> Option<usize> {
    let idx = slot as usize;
    if idx >= 64 {
        return None;
    }
    CURRENT_TEB.with(|cell| cell.borrow().as_ref().map(|h| h.teb.tls_slots[idx]))
}

/// Écrit un slot `TlsSetValue` directement dans le TEB courant (slots 0..64).
pub fn set_teb_tls_slot(slot: u32, value: usize) {
    let idx = slot as usize;
    if idx >= 64 {
        return;
    }
    CURRENT_TEB.with(|cell| {
        if let Some(ref mut holder) = *cell.borrow_mut() {
            holder.teb.tls_slots[idx] = value;
        }
    });
}

/// Lit `GetLastError()` depuis le TEB du thread courant.
pub fn last_error() -> u32 {
    CURRENT_TEB.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|h| h.teb.last_error_value)
            .unwrap_or(0)
    })
}

/// Met à jour `SetLastError()` dans le TEB du thread courant.
pub fn set_last_error(code: u32) {
    CURRENT_TEB.with(|cell| {
        if let Some(ref mut holder) = *cell.borrow_mut() {
            holder.teb.last_error_value = code;
        }
    });
}

/// Retourne le pointeur brut vers le TEB courant (ou nul si non initialisé).
pub fn current_teb_ptr() -> *mut Teb {
    CURRENT_TEB.with(|cell| {
        cell.borrow_mut()
            .as_mut()
            .map(|h| &raw mut *h.teb)
            .unwrap_or(ptr::null_mut())
    })
}

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
mod tests {
    use super::*;
    use core::arch::asm;

    #[test]
    fn gs_segment_reads_teb_self_and_peb() {
        let teb_ptr = init_thread_teb(0x0001_4000_0000).expect("arch_prctl ARCH_SET_GS");

        let gs_self: usize;
        let gs_peb: usize;
        // SAFETY: `GS_BASE` vient d'être configuré sur `teb_ptr` (valide pour la durée du test).
        unsafe {
            asm!(
                "mov {self_out}, gs:[0x30]",
                "mov {peb_out}, gs:[0x60]",
                self_out = out(reg) gs_self,
                peb_out = out(reg) gs_peb,
                options(nostack, readonly)
            );
        }
        assert_eq!(gs_self, teb_ptr as usize);
        assert_ne!(gs_peb, 0);

        set_last_error(126);
        let gs_err: u32;
        // SAFETY: offset 0x68 = `last_error_value`.
        unsafe {
            asm!(
                "mov {err_out:e}, gs:[0x68]",
                err_out = out(reg) gs_err,
                options(nostack, readonly)
            );
        }
        assert_eq!(gs_err, 126);
        assert_eq!(last_error(), 126);

        // `PEB.ProcessHeap` doit pointer vers la même page réelle mappée que `process_heap_handle()`.
        let heap = process_heap_handle();
        assert_ne!(heap, 0);
        assert_ne!(heap, 0x0000_0001_0000_0000);
        // SAFETY: `gs_peb` pointe vers le `Peb` global et `heap` vers une page 4 Kio mappée.
        unsafe {
            assert_eq!((*(gs_peb as *const Peb)).process_heap, heap);
            assert_eq!(*(heap as *const u64), 0);
        }
    }
}
