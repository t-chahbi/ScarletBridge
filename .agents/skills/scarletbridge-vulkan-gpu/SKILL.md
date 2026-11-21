---
name: scarletbridge-vulkan-gpu
description: >-
  Guide d'ingénierie graphique pour les Phases 3 à 5 de ScarletBridge : mémoire unifiée
  xmem.dll ↔ VK_EXT_external_memory_host, d3d12_xs.dll sur fork vkd3d-proton, surface Wayland,
  détiling AMD RDNA 2 (XG), traduction shaders DXIL/ISA → SPIR-V, BVH raytracing, FSR Vulkan,
  Vulkan Validation Layers et RenderDoc.
---

# ScarletBridge — Pipeline Graphique & Mémoire Unifiée (Vulkan)

## Invariants obligatoires en développement GPU
1. **Vulkan Validation Layers toujours actifs** :
   ```bash
   export VK_INSTANCE_LAYERS="VK_LAYER_KHRONOS_validation"
   export VKD3D_DEBUG="warn"
   ```
   Toute erreur de validation Vulkan (barrière manquante, layout invalide, fuite de descripteur, alignement mémoire) est considérée comme un bug bloquant.
2. **Surface cible** : présentation native Wayland/Hyprland via `VK_KHR_wayland_surface`.
3. **Architecture GPU de référence** : AMD RDNA 2 (driver Linux `RADV` / `vulkan-radeon`).

## Pont Mémoire Unifiée (`xmem.dll` → Vulkan)
- Aligner les allocations sur les frontières de pages attendues (`4 KiB`, `64 KiB`, `2 MiB` selon flags `XMEMALLOC_ATTRIBUTES`).
- Maintenir une table d'intervalles (`BTreeMap<usize, MemoryRegion>` sous `parking_lot::RwLock`) associant chaque plage virtuelle CPU à son objet `VkDeviceMemory` / `VkBuffer` (`VK_EXT_external_memory_host` lorsque pris en charge, sinon staging mappable cohérent).
- Garantir la visibilité mémoire avant/après soumission GPU (barrières de domaine mémoire / `vkFlushMappedMemoryRanges` si non cohérent).

## Traduction Graphique (`d3d12_xs.dll` & compagnons)
- **Extensions D3D12X** : ressources placées, descripteurs directs, commandes GPU simplifiées, barrières console au-dessus du fork `vkd3d-proton`.
- **Tiling / Swizzling `XG` (RDNA 2)** : commencer par une implémentation de référence vérifiée sur CPU en Rust sûr (testée par `cargo test`), puis accélérer par compute shaders Vulkan.
- **Shaders** :
  - DXIL étendu → pipeline DXIL → SPIR-V.
  - ISA RDNA 2 précompilé → désassembleur Rust + IR + émission SPIR-V (`rspirv`).
- **Raytracing (`XboxBvhBuilder.dll`)** : mapper vers `VK_KHR_acceleration_structure` + `VK_KHR_ray_tracing_pipeline`.
- **Upscaling (`amd_fidelityfx_*_xbox.dll`)** : wrapper Rust FFI vers le SDK open-source AMD FidelityFX (Vulkan).

## Débogage visuel & Profilage
- **RenderDoc** : capturer une frame du micro-harnais (`tests/harness/triangle` ou cible) et comparer chaque passe (vertex/index buffers, textures, constantes, shaders SPIR-V).
- **Profilage (Phase 7)** : `Tracy` (`tracy-client`), `perf`, et `RGP` (Radeon GPU Profiler sur GPU AMD).
