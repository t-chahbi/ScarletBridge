# ABI Reference — `xmem.dll` (Xbox GDK / Scarlett Memory API)

> Documentation de travail ScarletBridge (Phase 0 / Phase 3).
> Toutes les signatures exportées utilisent l'ABI Windows x86-64 (`extern "win64"`).

## Exports implémentés (Phase 0)

| Symbole | Signature Rust (`extern "win64"`) | Notes |
| :--- | :--- | :--- |
| `XMemAlloc` | `fn(size: usize, attributes: u64) -> *mut c_void` | Délègue à `XMemAllocDefault` (ou hook si configuré) |
| `XMemAllocDefault` | `fn(size: usize, attributes: u64) -> *mut c_void` | Alloue un bloc aligné et l'enregistre dans la table d'intervalles |
| `XMemFree` | `fn(address: *mut c_void, attributes: u64)` | Délègue à `XMemFreeDefault` |
| `XMemFreeDefault` | `fn(address: *mut c_void, attributes: u64)` | Libère le bloc et le retire atomiquement de la table |
| `XMemSize` | `fn(address: *const c_void, attributes: u64) -> usize` | Retourne la taille demandée de l'allocation active |
| `XMemSizeDefault` | `fn(address: *const c_void, attributes: u64) -> usize` | Lookup dans la table `BTreeMap` |
| `XMemSetAllocationHooks` | `fn(alloc_hook: *const c_void, free_hook: *const c_void)` | `SB_FIXME` en Phase 0 |

## Décodage `XMEMALLOC_ATTRIBUTES` (32 bits bas)
- Bit `31` : `dwMemoryType` (`0` = Heap, `1` = Physical)
- Bits `21..=28` : `dwAllocatorId`
- Bits `15..=18` : `dwAlignment` (Heap : 4B–4KiB ; Physical : 4KiB, 8KiB, 16KiB, 32KiB, 64KiB, 2MiB)
- Bit `14` : `dwZeroInitialize`

## Plan Phase 3 (Mémoire Unifiée Vulkan)
- Remplacer le backend `std::alloc` des allocations physiques par `mmap`/`mprotect` aligné page + enregistrement `VK_EXT_external_memory_host` (ou `VkDeviceMemory` mappable cohérent).
- Exposer `find_containing_region(cpu_addr)` à `d3d12_xs.dll` pour la résolution directe CPU ↔ GPU.
