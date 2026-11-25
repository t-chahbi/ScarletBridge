//! Mémoire virtuelle Win32 : table des réservations `VirtualAlloc` (MEM_RESERVE / MEM_COMMIT),
//! `VirtualFree` (MEM_DECOMMIT / MEM_RELEASE) et `VirtualQuery(Ex)` (via `/proc/self/maps`).

use core::ffi::c_void;
use core::mem::{offset_of, size_of};
use std::collections::BTreeMap;
use std::ptr;

use parking_lot::Mutex;
use sb_common::abi::{WIN_FALSE, WIN_TRUE, WinBool};
use sb_common::ffi_guard;
use sb_loader::{module_manager, set_last_error};

use super::{
    CHANNEL, ERROR_ACCESS_DENIED, ERROR_BAD_LENGTH, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER,
    ERROR_NOT_ENOUGH_MEMORY,
};
use crate::process::PSEUDO_CURRENT_PROCESS;

pub(crate) const MEM_COMMIT: u32 = 0x0000_1000;
pub(crate) const MEM_RESERVE: u32 = 0x0000_2000;
pub(crate) const MEM_DECOMMIT: u32 = 0x0000_4000;
pub(crate) const MEM_RELEASE: u32 = 0x0000_8000;
pub(crate) const MEM_FREE: u32 = 0x0001_0000;
pub(crate) const MEM_PRIVATE: u32 = 0x0002_0000;
pub(crate) const MEM_MAPPED: u32 = 0x0004_0000;
pub(crate) const MEM_RESET: u32 = 0x0008_0000;
pub(crate) const MEM_TOP_DOWN: u32 = 0x0010_0000;
pub(crate) const MEM_LARGE_PAGES: u32 = 0x2000_0000;
pub(crate) const MEM_IMAGE: u32 = 0x0100_0000;

const PAGE_NOACCESS: u32 = 0x01;
const PAGE_READONLY: u32 = 0x02;
const PAGE_READWRITE: u32 = 0x04;
const PAGE_EXECUTE: u32 = 0x10;
const PAGE_EXECUTE_READ: u32 = 0x20;
const PAGE_EXECUTE_READWRITE: u32 = 0x40;

const PAGE: usize = 4096;
const GRANULARITY: usize = 65536;
/// `lpMaximumApplicationAddress` Windows x64.
pub(crate) const MAX_USER_ADDRESS: usize = 0x7FFF_FFFE_FFFF;

/// `MEMORY_BASIC_INFORMATION` x64 (48 octets).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MemoryBasicInformation {
    pub base_address: usize,
    pub allocation_base: usize,
    pub allocation_protect: u32,
    pub partition_id: u16,
    pub _pad0: u16,
    pub region_size: usize,
    pub state: u32,
    pub protect: u32,
    pub type_: u32,
    pub _pad1: u32,
}

const _: () = {
    assert!(size_of::<MemoryBasicInformation>() == 48);
    assert!(offset_of!(MemoryBasicInformation, allocation_protect) == 16);
    assert!(offset_of!(MemoryBasicInformation, region_size) == 24);
    assert!(offset_of!(MemoryBasicInformation, state) == 32);
    assert!(offset_of!(MemoryBasicInformation, protect) == 36);
    assert!(offset_of!(MemoryBasicInformation, type_) == 40);
};

/// Réservation `VirtualAlloc` : taille et protection initiale (`AllocationProtect`).
#[derive(Debug, Clone, Copy)]
struct Reservation {
    size: usize,
    protect: u32,
}

static RESERVATIONS: Mutex<BTreeMap<usize, Reservation>> = Mutex::new(BTreeMap::new());

fn reservation_containing(addr: usize) -> Option<(usize, Reservation)> {
    let r = RESERVATIONS.lock();
    let (&base, &res) = r.range(..=addr).next_back()?;
    (addr < base + res.size).then_some((base, res))
}

pub(crate) fn win_protect_to_posix(protect: u32) -> Option<libc::c_int> {
    Some(match protect & 0xFF {
        PAGE_NOACCESS => libc::PROT_NONE,
        PAGE_READONLY => libc::PROT_READ,
        PAGE_READWRITE | 0x08 => libc::PROT_READ | libc::PROT_WRITE,
        PAGE_EXECUTE => libc::PROT_EXEC,
        PAGE_EXECUTE_READ => libc::PROT_READ | libc::PROT_EXEC,
        PAGE_EXECUTE_READWRITE | 0x80 => libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
        _ => return None,
    })
}

const fn align_up(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}

/// Implémentation de `VirtualAlloc` (processus courant).
pub(crate) fn virtual_alloc(addr: usize, size: usize, alloc_type: u32, protect: u32) -> usize {
    let known = MEM_COMMIT | MEM_RESERVE | MEM_RESET | MEM_TOP_DOWN | MEM_LARGE_PAGES | 0x0040_0000;
    if size == 0
        || (alloc_type & !known) != 0
        || (alloc_type & (MEM_COMMIT | MEM_RESERVE | MEM_RESET)) == 0
    {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let Some(prot) = win_protect_to_posix(protect) else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    if (alloc_type & MEM_RESET) != 0 {
        let start = addr & !(PAGE - 1);
        let len = align_up(addr + size, PAGE) - start;
        // SAFETY: indique au noyau que le contenu de la plage peut être jeté.
        unsafe { libc::madvise(start as *mut libc::c_void, len, libc::MADV_DONTNEED) };
        return addr;
    }

    // Engagement de pages dans une réservation existante.
    if addr != 0
        && let Some((base, res)) = reservation_containing(addr)
    {
        let start = addr & !(PAGE - 1);
        let end = align_up(addr + size, PAGE);
        if end > base + res.size {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        if (alloc_type & MEM_COMMIT) == 0 {
            // Re-réservation d'une plage déjà réservée.
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        // SAFETY: plage alignée à l'intérieur d'une réservation que nous avons mappée.
        if unsafe { libc::mprotect(start as *mut libc::c_void, end - start, prot) } != 0 {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        return start;
    }

    // Nouvelle réservation (alignée sur la granularité 64 Kio pour une adresse imposée).
    let (base_hint, len) = if addr != 0 {
        let b = addr & !(GRANULARITY - 1);
        (b, align_up(addr + size, PAGE) - b)
    } else {
        (0, align_up(size, PAGE))
    };
    let initial = if (alloc_type & MEM_COMMIT) != 0 {
        prot
    } else {
        libc::PROT_NONE
    };
    let mut flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE;
    if base_hint != 0 {
        flags |= libc::MAP_FIXED_NOREPLACE;
    }
    // SAFETY: mapping anonyme ; `MAP_FIXED_NOREPLACE` n'écrase jamais un mapping existant.
    let p = unsafe { libc::mmap(base_hint as *mut libc::c_void, len, initial, flags, -1, 0) };
    if p == libc::MAP_FAILED || (base_hint != 0 && p as usize != base_hint) {
        if p != libc::MAP_FAILED {
            // SAFETY: mapping que nous venons de créer à une autre adresse (noyau ancien).
            unsafe { libc::munmap(p, len) };
        }
        set_last_error(if base_hint != 0 {
            487 // ERROR_INVALID_ADDRESS
        } else {
            ERROR_NOT_ENOUGH_MEMORY
        });
        return 0;
    }
    let base = p as usize;
    RESERVATIONS
        .lock()
        .insert(base, Reservation { size: len, protect });
    if addr != 0 { addr & !(PAGE - 1) } else { base }
}

/// Implémentation de `VirtualFree`.
/// `MEM_COALESCE_PLACEHOLDERS` / `MEM_PRESERVE_PLACEHOLDER` (combinés à `MEM_RELEASE`).
const MEM_PLACEHOLDER_BITS: u32 = 0x0000_0003;

pub(crate) fn virtual_free(addr: usize, size: usize, free_type: u32) -> bool {
    // API de placeholders (Windows 10+) : `VirtualFree(addr, size, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER)`
    // scinde un placeholder, `MEM_COALESCE_PLACEHOLDERS` en fusionne. La mémoire reste réservée et
    // aucun mapping n'est modifié : un succès sans effet suffit (la libération finale
    // `MEM_RELEASE` sur la base la rendra).
    if addr != 0
        && free_type & MEM_RELEASE != 0
        && free_type & MEM_PLACEHOLDER_BITS != 0
        && free_type & !(MEM_RELEASE | MEM_PLACEHOLDER_BITS) == 0
    {
        return true;
    }
    if addr == 0 || !(free_type == MEM_RELEASE || free_type == MEM_DECOMMIT) {
        set_last_error(ERROR_INVALID_PARAMETER);
        return false;
    }
    let Some((base, res)) = reservation_containing(addr) else {
        // Bloc non issu de `VirtualAlloc` : comportement historique (munmap direct).
        if free_type == MEM_RELEASE && size == 0 {
            set_last_error(ERROR_INVALID_PARAMETER);
            return false;
        }
        // SAFETY: plage indiquée explicitement par l'appelant.
        unsafe { libc::munmap(addr as *mut libc::c_void, size) };
        return true;
    };
    if free_type == MEM_RELEASE {
        if size != 0 || addr != base {
            set_last_error(ERROR_INVALID_PARAMETER);
            return false;
        }
        RESERVATIONS.lock().remove(&base);
        // SAFETY: libère la réservation entière que nous avions mappée.
        unsafe { libc::munmap(base as *mut libc::c_void, res.size) };
        return true;
    }
    let start = addr & !(PAGE - 1);
    let end = if size == 0 {
        base + res.size
    } else {
        align_up(addr + size, PAGE)
    };
    if end > base + res.size {
        set_last_error(ERROR_INVALID_PARAMETER);
        return false;
    }
    // SAFETY: plage alignée dans la réservation ; contenu jeté puis pages inaccessibles.
    unsafe {
        libc::madvise(start as *mut libc::c_void, end - start, libc::MADV_DONTNEED);
        libc::mprotect(start as *mut libc::c_void, end - start, libc::PROT_NONE);
    }
    true
}

struct MapLine {
    start: usize,
    end: usize,
    perms: [u8; 4],
    path: String,
}

fn read_maps() -> Vec<MapLine> {
    let text = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let range = it.next()?;
            let perms = it.next()?.as_bytes();
            let (a, b) = range.split_once('-')?;
            let _offset = it.next();
            let _dev = it.next();
            let _inode = it.next();
            let path = it.collect::<Vec<_>>().join(" ");
            Some(MapLine {
                start: usize::from_str_radix(a, 16).ok()?,
                end: usize::from_str_radix(b, 16).ok()?,
                perms: [
                    *perms.first()?,
                    *perms.get(1)?,
                    *perms.get(2)?,
                    *perms.get(3)?,
                ],
                path,
            })
        })
        .collect()
}

fn perms_to_protect(p: &[u8; 4]) -> u32 {
    match (p[0] == b'r', p[1] == b'w', p[2] == b'x') {
        (_, true, true) => PAGE_EXECUTE_READWRITE,
        (_, false, true) if p[0] == b'r' => PAGE_EXECUTE_READ,
        (_, false, true) => PAGE_EXECUTE,
        (_, true, false) => PAGE_READWRITE,
        (true, false, false) => PAGE_READONLY,
        (false, false, false) => PAGE_NOACCESS,
    }
}

/// Calcule la `MEMORY_BASIC_INFORMATION` de `addr` dans le processus courant.
pub(crate) fn query(addr: usize) -> Option<MemoryBasicInformation> {
    if addr > MAX_USER_ADDRESS {
        return None;
    }
    let page = addr & !(PAGE - 1);
    let maps = read_maps();
    let Some(idx) = maps.iter().position(|m| page >= m.start && page < m.end) else {
        let next = maps
            .iter()
            .filter(|m| m.start > page)
            .map(|m| m.start)
            .min()
            .unwrap_or(MAX_USER_ADDRESS + 1)
            .min(MAX_USER_ADDRESS + 1);
        return Some(MemoryBasicInformation {
            base_address: page,
            region_size: next - page,
            state: MEM_FREE,
            protect: PAGE_NOACCESS,
            ..Default::default()
        });
    };
    let line = &maps[idx];
    let module = module_manager().find_by_address(addr);
    let reservation = reservation_containing(addr);
    let reserved = line.perms[..3] == *b"---";
    let protect = perms_to_protect(&line.perms);
    // Fin de région : lignes contiguës de mêmes permissions et même nature.
    let mut end = line.end;
    for next in &maps[idx + 1..] {
        if next.start != end || next.perms != line.perms || next.path != line.path {
            break;
        }
        end = next.end;
    }
    let (alloc_base, alloc_protect, type_) = if let Some(m) = &module {
        end = end.min(m.base + m.image_size);
        (m.base, 0x80, MEM_IMAGE) // PAGE_EXECUTE_WRITECOPY
    } else if let Some((base, res)) = reservation {
        end = end.min(base + res.size);
        (base, res.protect, MEM_PRIVATE)
    } else {
        let t = if line.perms[3] == b's' || line.path.starts_with('/') {
            MEM_MAPPED
        } else {
            MEM_PRIVATE
        };
        (line.start, protect, t)
    };
    Some(MemoryBasicInformation {
        base_address: page,
        allocation_base: alloc_base,
        allocation_protect: alloc_protect,
        partition_id: 0,
        _pad0: 0,
        region_size: end - page,
        state: if reserved { MEM_RESERVE } else { 0x1000 },
        protect: if reserved { 0 } else { protect },
        type_,
        _pad1: 0,
    })
}

/// `VirtualQuery` commun : retourne 48 ou 0 (avec `GetLastError`).
pub(crate) fn virtual_query(addr: usize, buf: *mut MemoryBasicInformation, len: usize) -> usize {
    if len < size_of::<MemoryBasicInformation>() {
        set_last_error(ERROR_BAD_LENGTH);
        return 0;
    }
    if buf.is_null() {
        set_last_error(998); // ERROR_NOACCESS
        return 0;
    }
    match query(addr) {
        Some(mbi) => {
            // SAFETY: `buf` non-nul, au moins 48 octets.
            unsafe { buf.write_unaligned(mbi) };
            size_of::<MemoryBasicInformation>()
        }
        None => {
            set_last_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

pub(super) extern "win64" fn VirtualQueryEx(
    process: usize,
    addr: *const c_void,
    buf: *mut MemoryBasicInformation,
    len: usize,
) -> usize {
    ffi_guard!(CHANNEL, "VirtualQueryEx", 0, {
        // SAFETY: `getpid` sans précondition.
        let pid = unsafe { libc::getpid() } as usize;
        if process == 0 {
            set_last_error(ERROR_INVALID_HANDLE);
            return 0;
        }
        if process != PSEUDO_CURRENT_PROCESS && process != pid {
            // Les autres processus ne sont pas accessibles depuis le bac à sable.
            set_last_error(ERROR_ACCESS_DENIED);
            return 0;
        }
        virtual_query(addr as usize, buf, len)
    })
}

pub(super) extern "win64" fn VirtualAllocEx(
    process: usize,
    addr: *mut c_void,
    size: usize,
    alloc_type: u32,
    protect: u32,
) -> *mut c_void {
    ffi_guard!(CHANNEL, "VirtualAllocEx", ptr::null_mut(), {
        if process != PSEUDO_CURRENT_PROCESS {
            set_last_error(ERROR_ACCESS_DENIED);
            return ptr::null_mut();
        }
        virtual_alloc(addr as usize, size, alloc_type, protect) as *mut c_void
    })
}

pub(super) extern "win64" fn VirtualFreeEx(
    process: usize,
    addr: *mut c_void,
    size: usize,
    free_type: u32,
) -> WinBool {
    ffi_guard!(CHANNEL, "VirtualFreeEx", WIN_FALSE, {
        if process != PSEUDO_CURRENT_PROCESS {
            set_last_error(ERROR_ACCESS_DENIED);
            return WIN_FALSE;
        }
        if virtual_free(addr as usize, size, free_type) {
            WIN_TRUE
        } else {
            WIN_FALSE
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_loader::last_error;

    #[test]
    fn placeholder_release_flags_succeed_without_unmapping() {
        super::super::init_test_thread();
        let base = virtual_alloc(0, 0x40000, MEM_RESERVE, PAGE_READWRITE);
        assert_ne!(base, 0);
        // Scission de placeholder (MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) : succès, réservation intacte.
        assert!(virtual_free(base, 0x10000, MEM_RELEASE | 0x2));
        assert_eq!(query(base).expect("mbi").state, MEM_RESERVE);
        // Fusion (MEM_COALESCE_PLACEHOLDERS) idem ; des bits inconnus restent refusés.
        assert!(virtual_free(base, 0x40000, MEM_RELEASE | 0x1));
        assert!(!virtual_free(base, 0, MEM_RELEASE | 0x100));
        assert!(virtual_free(base, 0, MEM_RELEASE));
    }

    #[test]
    fn reserve_commit_query_release() {
        super::super::init_test_thread();
        let base = virtual_alloc(0, 0x40000, MEM_RESERVE, PAGE_READWRITE);
        assert_ne!(base, 0);
        let mut mbi = MemoryBasicInformation::default();
        assert_eq!(
            VirtualQueryEx(PSEUDO_CURRENT_PROCESS, base as *const c_void, &mut mbi, 48),
            48
        );
        assert_eq!(mbi.state, MEM_RESERVE);
        assert_eq!(mbi.allocation_base, base);
        assert_eq!(mbi.type_, MEM_PRIVATE);
        assert_eq!(mbi.allocation_protect, PAGE_READWRITE);

        // Engagement d'une page au milieu.
        let p = virtual_alloc(base + 0x10010, 0x100, MEM_COMMIT, PAGE_READWRITE);
        assert_eq!(p, base + 0x10000);
        // SAFETY: page engagée en lecture/écriture.
        unsafe { *(p as *mut u64) = 42 };
        let q = query(base + 0x10008).expect("mbi");
        assert_eq!(
            (q.base_address, q.state, q.protect, q.allocation_base),
            (base + 0x10000, 0x1000, PAGE_READWRITE, base)
        );
        assert_eq!(q.region_size, 0x1000);
        let r = query(base + 0x11000).expect("mbi");
        assert_eq!(r.state, MEM_RESERVE);

        assert!(!virtual_free(base, 0x1000, MEM_RELEASE));
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        assert!(virtual_free(p, 0x1000, MEM_DECOMMIT));
        assert_eq!(query(p).expect("mbi").state, MEM_RESERVE);
        assert!(virtual_free(base, 0, MEM_RELEASE));
        // Libérée : l'adresse peut déjà être réutilisée par un mapping d'un test parallèle, mais ce
        // ne doit plus être notre réservation.
        let after = query(base).expect("mbi");
        assert!(after.state == MEM_FREE || after.state == MEM_COMMIT);

        // Mémoire de pile (privée, engagée) et adresse hors espace utilisateur.
        let local = 0u64;
        let s = query(&raw const local as usize).expect("pile");
        assert_eq!(
            (s.state, s.type_, s.protect),
            (0x1000, MEM_PRIVATE, PAGE_READWRITE)
        );
        assert_eq!(
            VirtualQueryEx(
                PSEUDO_CURRENT_PROCESS,
                0x8000_0000_0000 as *const c_void,
                &mut mbi,
                48
            ),
            0
        );
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        assert_eq!(
            VirtualQueryEx(PSEUDO_CURRENT_PROCESS, base as *const c_void, &mut mbi, 8),
            0
        );
        assert_eq!(last_error(), ERROR_BAD_LENGTH);
        assert_eq!(
            VirtualQueryEx(0x1234, base as *const c_void, &mut mbi, 48),
            0
        );
        assert_eq!(last_error(), ERROR_ACCESS_DENIED);
    }

    #[test]
    fn commit_reserve_in_one_call_and_bad_params() {
        super::super::init_test_thread();
        let p = virtual_alloc(0, 10, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READ);
        assert_ne!(p, 0);
        let q = query(p).expect("mbi");
        assert_eq!(
            (q.state, q.protect, q.region_size),
            (0x1000, PAGE_EXECUTE_READ, 0x1000)
        );
        assert!(virtual_free(p, 0, MEM_RELEASE));
        assert_eq!(virtual_alloc(0, 0, MEM_COMMIT, PAGE_READWRITE), 0);
        assert_eq!(virtual_alloc(0, 16, MEM_COMMIT, 0x1234), 0);
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
    }
}
