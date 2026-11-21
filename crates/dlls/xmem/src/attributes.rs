//! Décodage du champ de bits `XMEMALLOC_ATTRIBUTES` (`u64` / `u32`).

/// Type de mémoire demandé par le titre.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryType {
    Heap,
    Physical,
}

/// Attributs décodés pour une allocation `XMemAlloc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XMemAttributes {
    pub raw: u64,
    pub memory_type: MemoryType,
    pub alignment: usize,
    pub zero_initialize: bool,
    pub allocator_id: u8,
    pub page_size_bytes: usize,
}

impl XMemAttributes {
    /// Décode les bits principaux de `XMEMALLOC_ATTRIBUTES`.
    ///
    /// Layout standard GDK/XDK (`XALLOC_ATTRIBUTES`, 32 bits bas) :
    /// - bits `0..=1`  : `dwObjectType`
    /// - bits `2..=13` : `dwHeapTracksAttributes` / flags divers
    /// - bit `14`      : `dwZeroInitialize`
    /// - bits `15..=18`: `dwAlignment` (code d'alignement)
    /// - bits `19..=20`: `dwMemoryProtect` / page size
    /// - bits `21..=28`: `dwAllocatorId`
    /// - bit `31`      : `dwMemoryType` (0 = Heap, 1 = Physical)
    pub const fn decode(raw: u64) -> Self {
        let low = raw as u32;
        let memory_type = if (low & (1 << 31)) != 0 {
            MemoryType::Physical
        } else {
            MemoryType::Heap
        };
        let zero_initialize = (low & (1 << 14)) != 0;
        let align_code = (low >> 15) & 0xF;
        let allocator_id = ((low >> 21) & 0xFF) as u8;

        let alignment = match (memory_type, align_code) {
            (MemoryType::Heap, 0) => 16,
            (MemoryType::Heap, 1) => 4,
            (MemoryType::Heap, 2) => 8,
            (MemoryType::Heap, 3) => 16,
            (MemoryType::Heap, 4) => 32,
            (MemoryType::Heap, 5) => 64,
            (MemoryType::Heap, 6) => 128,
            (MemoryType::Heap, 7) => 256,
            (MemoryType::Heap, 8) => 4096,
            (MemoryType::Physical, 0) | (MemoryType::Physical, 1) => 4096,
            (MemoryType::Physical, 2) => 8192,
            (MemoryType::Physical, 3) => 16384,
            (MemoryType::Physical, 4) => 32768,
            (MemoryType::Physical, 5) => 65536,
            (MemoryType::Physical, 6) => 2 * 1024 * 1024,
            _ => 4096,
        };

        let page_size_bytes = match memory_type {
            MemoryType::Physical if alignment >= 2 * 1024 * 1024 => 2 * 1024 * 1024,
            MemoryType::Physical if alignment >= 65536 => 65536,
            _ => 4096,
        };

        Self {
            raw,
            memory_type,
            alignment,
            zero_initialize,
            allocator_id,
            page_size_bytes,
        }
    }

    /// Construit un attribut physique aligné sur 64 KiB et mis à zéro (utile pour les tests).
    pub const fn physical_64k_zeroed() -> u64 {
        ((1u32 << 31) | (1u32 << 14) | (5u32 << 15)) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_heap_default_and_physical_64k() {
        let heap = XMemAttributes::decode(0);
        assert_eq!(heap.memory_type, MemoryType::Heap);
        assert_eq!(heap.alignment, 16);
        assert!(!heap.zero_initialize);

        let phys = XMemAttributes::decode(XMemAttributes::physical_64k_zeroed());
        assert_eq!(phys.memory_type, MemoryType::Physical);
        assert_eq!(phys.alignment, 65536);
        assert!(phys.zero_initialize);
        assert_eq!(phys.page_size_bytes, 65536);
    }
}
