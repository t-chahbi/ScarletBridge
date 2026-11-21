//! Implémentation ScarletBridge de `xmem.dll` (gestion mémoire console Xbox).
//!
//! Phase 0 : allocateur aligné fonctionnel avec table d'intervalles CPU.
//! Phase 3 : extension de la table d'intervalles vers `mmap`/`mprotect` + Vulkan external memory.

pub mod allocator;
pub mod attributes;
pub mod ffi;
