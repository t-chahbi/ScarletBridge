//! Table thread-safe de handles opaques générationnels (anti use-after-free / ABA).

use parking_lot::RwLock;

/// Handle opaque 64 bits exposable à travers une ABI Windows (`HANDLE` / `u64`).
/// La valeur `0` est réservée comme handle invalide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct OpaqueHandle(u64);

impl OpaqueHandle {
    pub const INVALID: Self = Self(0);

    #[inline]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    #[inline]
    pub const fn as_raw(self) -> u64 {
        self.0
    }

    #[inline]
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }

    #[inline]
    const fn pack(index: u32, generation: u32) -> Self {
        Self(((generation as u64) << 32) | (index as u64))
    }

    #[inline]
    const fn index(self) -> u32 {
        (self.0 & 0xFFFF_FFFF) as u32
    }

    #[inline]
    const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }
}

struct Slot<T> {
    generation: u32,
    value: Option<T>,
}

/// Conteneur générationnel thread-safe évitant d'exposer des pointeurs Rust bruts au code invité.
pub struct HandleTable<T> {
    inner: RwLock<TableInner<T>>,
}

struct TableInner<T> {
    slots: Vec<Slot<T>>,
    free_list: Vec<u32>,
    live_count: usize,
}

impl<T> Default for HandleTable<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> HandleTable<T> {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(TableInner {
                slots: Vec::new(),
                free_list: Vec::new(),
                live_count: 0,
            }),
        }
    }

    /// Insère une valeur et retourne un handle opaque unique.
    pub fn insert(&self, value: T) -> OpaqueHandle {
        let mut inner = self.inner.write();
        inner.live_count += 1;

        if let Some(index) = inner.free_list.pop() {
            let slot = &mut inner.slots[index as usize];
            slot.value = Some(value);
            return OpaqueHandle::pack(index, slot.generation);
        }

        let index = u32::try_from(inner.slots.len()).unwrap_or(u32::MAX);
        // Génération initiale >= 1 pour garantir que `pack(0, 1) != 0`.
        let generation = 1;
        inner.slots.push(Slot {
            generation,
            value: Some(value),
        });
        OpaqueHandle::pack(index, generation)
    }

    /// Accède en lecture à la valeur associée au handle.
    pub fn with<R>(&self, handle: OpaqueHandle, f: impl FnOnce(&T) -> R) -> Option<R> {
        if !handle.is_valid() {
            return None;
        }
        let inner = self.inner.read();
        let slot = inner.slots.get(handle.index() as usize)?;
        if slot.generation != handle.generation() {
            return None;
        }
        slot.value.as_ref().map(f)
    }

    /// Accède en écriture à la valeur associée au handle.
    pub fn with_mut<R>(&self, handle: OpaqueHandle, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        if !handle.is_valid() {
            return None;
        }
        let mut inner = self.inner.write();
        let slot = inner.slots.get_mut(handle.index() as usize)?;
        if slot.generation != handle.generation() {
            return None;
        }
        slot.value.as_mut().map(f)
    }

    /// Supprime la valeur et invalide définitivement le handle (incrément de génération).
    pub fn remove(&self, handle: OpaqueHandle) -> Option<T> {
        if !handle.is_valid() {
            return None;
        }
        let mut inner = self.inner.write();
        let idx = handle.index() as usize;
        let slot = inner.slots.get_mut(idx)?;
        if slot.generation != handle.generation() {
            return None;
        }
        let value = slot.value.take()?;
        slot.generation = slot.generation.wrapping_add(1).max(1);
        inner.free_list.push(handle.index());
        inner.live_count = inner.live_count.saturating_sub(1);
        Some(value)
    }

    pub fn len(&self) -> usize {
        self.inner.read().live_count
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_lookup_remove_and_prevent_aba() {
        let table = HandleTable::new();
        let h1 = table.insert("alpha".to_string());
        assert!(h1.is_valid());
        assert_eq!(table.len(), 1);
        assert_eq!(table.with(h1, |s| s.clone()).as_deref(), Some("alpha"));

        let removed = table.remove(h1);
        assert_eq!(removed.as_deref(), Some("alpha"));
        assert!(table.is_empty());

        // Réutilisation du même slot avec une nouvelle génération.
        let h2 = table.insert("beta".to_string());
        assert_ne!(h1, h2);
        assert!(table.with(h1, |s| s.clone()).is_none());
        assert_eq!(table.with(h2, |s| s.clone()).as_deref(), Some("beta"));
    }
}
