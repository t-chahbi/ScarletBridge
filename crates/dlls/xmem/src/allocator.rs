//! Table d'allocations mémoire unifiée CPU ↔ GPU (Phase 0 / Phase 3).

use std::alloc::{Layout, alloc, alloc_zeroed, dealloc};
use std::collections::BTreeMap;
use std::sync::OnceLock;

use parking_lot::RwLock;

use crate::attributes::XMemAttributes;

/// Métadonnées d'une région allouée par `xmem.dll`.
/// En Phase 3, cette structure sera étendue avec l'association `VkDeviceMemory` / `VkBuffer`.
#[derive(Debug, Clone, Copy)]
pub struct AllocationRecord {
    pub base_addr: usize,
    pub requested_size: usize,
    pub allocated_size: usize,
    pub alignment: usize,
    pub attributes: XMemAttributes,
}

impl AllocationRecord {
    #[inline]
    pub const fn end_addr(&self) -> usize {
        self.base_addr.saturating_add(self.allocated_size)
    }

    #[inline]
    pub const fn contains(&self, addr: usize) -> bool {
        addr >= self.base_addr && addr < self.end_addr()
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AllocatorStats {
    pub live_allocations: usize,
    pub total_requested_bytes: usize,
    pub total_allocated_bytes: usize,
}

pub struct XMemAllocator {
    regions: RwLock<BTreeMap<usize, AllocationRecord>>,
}

impl Default for XMemAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl XMemAllocator {
    pub fn new() -> Self {
        Self {
            regions: RwLock::new(BTreeMap::new()),
        }
    }

    /// Alloue un bloc aligné selon `attrs` et l'enregistre dans la table d'intervalles.
    pub fn alloc(&self, size: usize, attrs: XMemAttributes) -> Option<*mut u8> {
        let effective_size = size.max(1);
        let alignment = attrs.alignment.max(16).next_power_of_two();
        let allocated_size = align_up(effective_size, alignment)?;
        let layout = Layout::from_size_align(allocated_size, alignment).ok()?;

        // SAFETY: `layout.size() > 0` et `layout.align()` est une puissance de deux valide.
        let ptr = unsafe {
            if attrs.zero_initialize {
                alloc_zeroed(layout)
            } else {
                alloc(layout)
            }
        };

        if ptr.is_null() {
            return None;
        }

        let record = AllocationRecord {
            base_addr: ptr as usize,
            requested_size: size,
            allocated_size,
            alignment,
            attributes: attrs,
        };

        self.regions.write().insert(record.base_addr, record);
        Some(ptr)
    }

    /// Libère un bloc précédemment alloué par `alloc`. Retourne `true` si connu et libéré.
    ///
    /// # Safety
    /// L'appelant garantit qu'aucune référence active ne pointe plus vers la mémoire libérée.
    pub unsafe fn free(&self, ptr: *mut u8) -> bool {
        if ptr.is_null() {
            return true;
        }
        let addr = ptr as usize;
        let Some(record) = self.regions.write().remove(&addr) else {
            return false;
        };

        if let Ok(layout) = Layout::from_size_align(record.allocated_size, record.alignment) {
            // SAFETY: `ptr` provient de `alloc`/`alloc_zeroed` avec exactement ce `layout`,
            // son retrait atomique de `regions` empêche tout double-free, et l'appelant
            // garantit qu'aucun accès concurrent n'est en cours.
            unsafe {
                dealloc(ptr, layout);
            }
            true
        } else {
            false
        }
    }

    /// Retourne la taille demandée d'une allocation active (ou `0` si inconnue).
    pub fn size_of(&self, ptr: *const u8) -> usize {
        if ptr.is_null() {
            return 0;
        }
        self.regions
            .read()
            .get(&(ptr as usize))
            .map(|r| r.requested_size)
            .unwrap_or(0)
    }

    /// Recherche la région contenant une adresse virtuelle CPU quelconque (préparation Phase 3 GPU).
    pub fn find_containing_region(&self, addr: usize) -> Option<AllocationRecord> {
        let regions = self.regions.read();
        let (_, record) = regions.range(..=addr).next_back()?;
        if record.contains(addr) {
            Some(*record)
        } else {
            None
        }
    }

    pub fn stats(&self) -> AllocatorStats {
        let regions = self.regions.read();
        let mut stats = AllocatorStats {
            live_allocations: regions.len(),
            ..AllocatorStats::default()
        };
        for rec in regions.values() {
            stats.total_requested_bytes = stats
                .total_requested_bytes
                .saturating_add(rec.requested_size);
            stats.total_allocated_bytes = stats
                .total_allocated_bytes
                .saturating_add(rec.allocated_size);
        }
        stats
    }
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    let rem = value % align;
    if rem == 0 {
        Some(value)
    } else {
        value.checked_add(align - rem)
    }
}

static GLOBAL_ALLOCATOR: OnceLock<XMemAllocator> = OnceLock::new();

pub fn global() -> &'static XMemAllocator {
    GLOBAL_ALLOCATOR.get_or_init(XMemAllocator::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_aligned_lookup_and_free() {
        let allocator = XMemAllocator::new();
        let attrs = XMemAttributes::decode(XMemAttributes::physical_64k_zeroed());

        let ptr = allocator.alloc(65536, attrs).expect("allocation réussie");
        assert_eq!((ptr as usize) % 65536, 0, "doit être aligné sur 64 KiB");

        // SAFETY: `ptr` pointe vers 65536 octets mis à zéro.
        let slice = unsafe { std::slice::from_raw_parts_mut(ptr, 65536) };
        assert!(slice.iter().all(|&b| b == 0));
        slice[1234] = 0x5A;

        assert_eq!(allocator.size_of(ptr), 65536);

        let mid_addr = (ptr as usize) + 1234;
        let region = allocator
            .find_containing_region(mid_addr)
            .expect("région trouvée depuis adresse interne");
        assert_eq!(region.base_addr, ptr as usize);
        assert_eq!(allocator.stats().live_allocations, 1);

        // SAFETY: plus aucune référence sur `slice` n'est active.
        assert!(unsafe { allocator.free(ptr) });
        assert_eq!(allocator.stats().live_allocations, 0);
        assert!(allocator.find_containing_region(mid_addr).is_none());
        // Double-free ignoré proprement sans UB.
        assert!(!unsafe { allocator.free(ptr) });
    }
}
