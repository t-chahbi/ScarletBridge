# ScarletBridge — Guide d'Architecture & Référence pour Agents LLM

> **Document maître de contexte technique pour le projet ScarletBridge.**
> Tout agent LLM ou contributeur intervenant sur ce dépôt doit lire ce document avant de concevoir, générer ou modifier du code.

---

## 1. Vue d'ensemble & Périmètre

**ScarletBridge** est une couche de compatibilité Linux **100 % autonome (sans Wine)**, de type **HLE (High-Level Emulation) / loader PE32+ natif + traducteur d'API**, entièrement écrite en **Rust** (à l'instar de l'approche autonome de **shadPS4**), destinée à exécuter directement sous Linux x86-64 (Wayland / Hyprland, Vulkan, PipeWire) des binaires **PE32+ x86-64** ciblant les API **Xbox GDK / Scarlett / UWP**.

### Périmètre strict
- **Binaires supportés** : uniquement des binaires **non chiffrés** (binaires de test du harnais interne, homebrew, ports open-source Xbox Dev Mode, binaires SDK).
- **Approche** : loader PE32+ x86-64 natif Linux en Rust (`mmap`, relocations, `TEB`/`PEB` sur le segment `GS` via `arch_prctl`, résolution IAT directe vers nos shims `extern "win64"`, trampolines JIT x86-64 pour les imports non implémentés) + réimplémentation HLE des services OS/Xbox au-dessus des primitives Linux et des briques open-source (`Vulkan`/`ash`, `vkd3d-proton`, `FAudio`, `AMD FidelityFX SDK`, `SDL3`, `FFmpeg`). Aucun contournement DRM / déchiffrement.
- **Zéro Wine** : tout s'exécute nativement dans le processus Linux ScarletBridge, donnant un accès direct et sans copie à `mmap`, `io_uring`, `Vulkan` (`VK_EXT_external_memory_host`), `SDL3` Wayland et `PipeWire`.

---

## 2. Architecture Cible (100 % Standalone Rust)

```text
  [ Binaire PE32+ x86-64 (Xbox GDK / Scarlett / UWP non chiffré) ]
         │
         ├── Loader PE32+ natif (sb-loader) ─► mmap + relocs + .tls + TEB/PEB (GS) + IAT + trampolines JIT
         ├── Win32 / CRT / OneCore (sb-hle) ─► Shims Rust `extern "win64"` (kernel32, ucrt, vcruntime, msvcp, WinRT)
         ├── Mémoire console (xmem.dll) ─────► mmap / mprotect ↔ Vulkan external memory (`ash`)
         ├── Graphique (d3d12_xs / d3d11) ───► Vulkan (`ash` / fork vkd3d-proton FFI) → VK_KHR_wayland_surface
         │        ├── Layouts / Tiling (XG) ─► Détiling RDNA 2 en Rust (CPU ou compute shaders Vulkan)
         │        ├── Shaders console ───────► Traducteur DXIL étendu / ISA RDNA 2 → SPIR-V (`rspirv`)
         │        ├── Raytracing (BVH) ──────► XboxBvhBuilder → VK_KHR_acceleration_structure
         │        └── Upscaling (FSR Xbox) ──► amd_fidelityfx_*_xbox → AMD FidelityFX SDK (FFI Vulkan)
         ├── Audio (XAudio2_9) ──────────────► HLE Rust → FAudio (FFI) → PipeWire (+ XMA2 via FFmpeg si requis)
         ├── Vidéo (Bink2Scarlett) ──────────► DLL d'origine chargée par sb-loader ou shim Rust → FFmpeg (`libavcodec`)
         ├── Réseau (XCurl, libHttpClient) ──► Mode offline strict par défaut ou shims Rust (`curl`/`rustls`)
         └── Système (GameOS, WinRT, pad) ───► SDL3 + VFS Linux isolé (sb-vfs) + profils JSON/SQLite + io_uring
```

### Découpage par couches
1. **Loader PE32+ Natif Linux (`sb-loader`)** : parsing PE32+ (`goblin`), allocation virtuelle `mmap`, relocations `.reloc`, initialisation `.tls`, mise en place `TEB`/`PEB` sur le registre segment `GS` via `arch_prctl(ARCH_SET_GS)`, résolution IAT (vers `sb-hle` ou DLL invitées chargées dynamiquement), génération de trampolines x86-64 JIT pour les symboles manquants, permissions `mprotect`, diagnostic `SIGSEGV`/`SIGILL`.
2. **Couche HLE Win32 / CRT / WinRT (`sb-hle`)** : implémentations Rust `extern "win64"` du sous-ensemble Win32 (`kernel32`, `kernelbase`, `api-ms-win-core-*`), UCRT (`api-ms-win-crt-*`), C++/CX (`vccorlib140_app`), STL (`msvcp140_app`), WinRT (`HSTRING`, `RoInitialize`, `RoGetActivationFactory`, `CoreApplication`, `CoreWindow`, `ApplicationData`).
3. **Manifestes & VFS Isolé (`sb-vfs` / `sb-launcher`)** : parsing de `MicrosoftGame.Config` et `AppxManifest.xml`, montage virtuel sandboxé (`game:/`, `save:/`, `temp:/`, `G:\`, `S:\`, `T:\`) sans jamais exposer `/home`.
4. **Services GameOS (`sb-runtime`)** : identité locale offline (`XUser*`), sauvegardes et succès locaux (JSON/SQLite), cycle de vie console (focus, suspend/resume), mode offline strict, I/O asynchrone `io_uring`.
5. **Mémoire Unifiée (`xmem`)** : allocations alignées (`4 KiB`, `64 KiB`, `2 MiB`) via `mmap`/`mprotect` et table d'intervalles CPU ↔ GPU (`VkDeviceMemory` / `VK_EXT_external_memory_host`).
6. **Multimédia, Entrées & Graphique (`d3d12_xs`, `d3d11`, `XAudio2_9`, `GameInput`, `BVH`, `FSR`)** : `SDL3` Wayland/Hyprland, `FAudio`/PipeWire, `FFmpeg`, `Vulkan` (`ash` + fork `vkd3d-proton` FFI + AMD FidelityFX SDK).

---

## 3. Stack Technique, Crates & Briques Open-Source

- **Langage unique du projet** : **Rust** (stable, Edition **2024**). Zéro `.c`/`.cpp` pour le code interne ScarletBridge (FFI uniquement vers les briques open-source externes `third_party/`).
- **Exécution** : binaire natif Linux `x86_64-unknown-linux-gnu` (`sb-launcher` / `scarletbridge`) embarquant `sb-loader` + `sb-hle` + `sb-vfs` + `sb-runtime`.
- **Cross-compilation PE32+ (`x86_64-pc-windows-gnu`)** : utilisée uniquement pour compiler nos **micro-harnais de test PE32+** (`tests/harness/*`) et nos DLL invitées de test (`xmem.dll`, etc.), avec `zig cc` / `mingw-w64` + `llvm-dlltool`.

### Crates & Dépendances clés
| Domaine | Brique / Crate | Rôle dans ScarletBridge |
| :--- | :--- | :--- |
| **Loader PE32+ & OS Linux** | `goblin`, `libc` / `rustix`, `parking_lot` | Parsing PE32+, `mmap`/`mprotect`, `arch_prctl(ARCH_SET_GS)`, signaux, verrous |
| **ABI Win32 / COM / UTF-16** | `sb-common` (`extern "win64"`, `#[repr(C)]`) | `HRESULT`, `HSTRING`, `ffi_guard!`, handles générationnels, conversions UTF-16LE |
| **Manifestes & Données** | `quick-xml`, `serde`, `serde_json`, `rusqlite` | `MicrosoftGame.Config`, `AppxManifest.xml`, profils, sauvegardes et succès |
| **I/O Asynchrone** | `io-uring` | Lectures massives d'archives sans blocage |
| **Graphique & Shaders** | `ash`, `rspirv`, fork `vkd3d-proton` (FFI) | Vulkan natif Wayland (`VK_KHR_wayland_surface`), D3D12X/D3D11, `XG`, DXIL/ISA → SPIR-V, BVH |
| **Upscaling, Audio, Vidéo, Pad** | `AMD FidelityFX SDK`, `FAudio`, `FFmpeg`, `SDL3` | FSR Vulkan, `XAudio2_9` → PipeWire, Bink/XMA2, `GameInput`/`Gamepad` Wayland |
| **Débogage & Profilage** | `rust-gdb`, `llvm-objdump`, `Ghidra MCP`, `VK_LAYER_KHRONOS_validation`, `RenderDoc`, `Tracy`, `perf`, `RGP` | Diagnostic CPU/GPU et profilage |

### Projets de référence open-source
- **shadPS4** : https://github.com/shadps4-emu/shadPS4 (loader natif Linux + HLE + GPU AMD RDNA 2 → Vulkan)
- **vkd3d-proton** : https://github.com/HansKristian-Work/vkd3d-proton (DirectX 12 → Vulkan)
- **WinDurango** : https://github.com/WinDurango (WinRT / UWP / Xbox One ABI & services)

---

## 4. Arborescence du Dépôt (Cargo Workspace)

```text
ScarletBridge/
├── LLM.md                          # Architecture & référence technique
├── AGENTS.md                       # Règles projet impératives
├── Cargo.toml                      # Workspace Cargo racine (Edition 2024)
├── mise.toml                       # Toolchain locale (Rust stable + Zig)
├── .agents/skills/                 # Skills spécialisés Antigravity
├── docs/                           # Notes ABI, analyses binaires, ADR
├── crates/
│   ├── sb-common/                  # Logging, ABI win64, ffi_guard!, UTF-16LE, HandleTable
│   ├── sb-vfs/                     # Parseur MicrosoftGame.Config/AppxManifest.xml + VFS isolé
│   ├── sb-runtime/                 # Profils offline, succès JSON, cycle de vie console
│   ├── sb-loader/                  # Loader PE32+ natif Linux (mmap, relocs, TEB/GS, IAT, JIT stubs)
│   ├── sb-hle/                     # Implémentations HLE Rust extern "win64" (Win32, CRT, WinRT, Xbox)
│   ├── tools/
│   │   ├── sb-stubgen/             # Inspecteur PE32+ + générateur de stubs Rust
│   │   └── sb-launcher/            # CLI ScarletBridge (`inspect`, `run` natif sans Wine)
│   └── dlls/                       # DLL PE32+ de test / exportées (xmem, xaudio2_9, ...)
├── tests/
│   └── harness/                    # Petits binaires PE32+ compilés en Rust (dll_probe, etc.)
└── scripts/                        # Wrappers cross-link PE32+ (win64-linker.sh, win64-ar.sh)
```

---

## 5. Feuille de Route (Phases 0 à 7)

- **Phase 0 — Fondations & Banc de test** : workspace Rust, `sb-common`, `sb-stubgen`, harnais PE32+ (`dll_probe.exe`), `xmem.dll`.
- **Phase 1 — Loader PE32+ Natif, Manifestes & VFS** : `sb-loader` (`mmap`, relocations, `.tls`, `TEB`/`PEB` sur `GS`, IAT + trampolines JIT), `sb-vfs` (`MicrosoftGame.Config` & `AppxManifest.xml`, isolation stricte), exécution native de `dll_probe.exe` et démarrage tracé de `OpenLara.exe`.
- **Phase 2 — Couche GameOS, WinRT & Services Système** : profils offline (`XUser*`), `Windows.ApplicationModel.Core.CoreApplication`, `CoreWindow`, `ApplicationData` (VFS), `XCurl`/`libHttpClient` (mode offline strict), I/O `io_uring`.
- **Phase 3 — Mémoire Unifiée (`xmem`)** : `mmap`/`mprotect` alignés 4K/64K/2M + table CPU ↔ GPU (`VK_EXT_external_memory_host`).
- **Phase 4 — Audio, Vidéo & Entrées** : `XAudio2_9` → FAudio/PipeWire (+ XMA2 FFmpeg), `Bink2Scarlett` (chargement natif via `sb-loader` ou shim FFmpeg), `GameInput`/`Windows.Gaming.Input.Gamepad` → `SDL3`.
- **Phase 5 — Graphique (`d3d12_xs` / `d3d11` → Vulkan)** : surface Wayland (`VK_KHR_wayland_surface`), D3D11/D3D12X, détiling RDNA 2 (`XG`), shaders DXIL/ISA → SPIR-V, raytracing `XboxBvhBuilder`, upscaling `amd_fidelityfx_*_xbox`.
- **Phase 6 — Boucle IA & Debug en continu** : exécution native sous `sb-launcher run`, trace JIT des imports manquants, analyse ABI via Ghidra MCP + `llvm-objdump`, ajout du shim Rust + test, validation GPU (`VK_LAYER_KHRONOS_validation`, RenderDoc).
- **Phase 7 — Stabilisation & Performances** : `VkPipelineCache`, compilation asynchrone shaders, Tracy / `perf` / RGP, zéro-copie mémoire unifiée.

---

## 6. Conventions Techniques Rust Critiques

1. **ABI Windows x86-64 (`extern "win64"`)** : toute fonction HLE exposée à l'IAT d'un binaire PE32+ utilise impérativement `extern "win64"` (qui gère les registres `rcx`, `rdx`, `r8`, `r9`, le shadow space 32 octets et les registres préservés Win64 automatiquement depuis Rust Linux !).
2. **Séparation `FS` vs `GS` sous Linux x86-64** :
   - **`FS`** appartient exclusivement à Linux / glibc / Rust TLS — ne jamais le modifier.
   - **`GS`** pointe vers le `TEB` Windows alloué par `sb-loader` (`arch_prctl(ARCH_SET_GS, teb)`).
3. **Zéro unwind across FFI** : toute fonction `extern "win64"` est enveloppée dans `sb_common::ffi_guard!`.
4. **Isolation `unsafe`** : blocs `unsafe` documentés par `// SAFETY: ...`, structures ABI annotées `#[repr(C)]` et validées par `size_of!`/`offset_of!`.
5. **Isolation VFS & Offline** : `/home` jamais exposé, path traversal et symlinks sortants bloqués, `SB_OFFLINE_MODE=1` par défaut.

---

## 7. D3D12 console (`d3d12_xs`) — descripteurs matériels et tuilage

**Branché** (`crates/sb-hle/src/d3d12_views.rs`, glue dans `d3d12_xs.rs`) :
- Les handles CPU de descripteurs sont de vraies adresses (chaque tas reçoit sa mémoire, ou adopte celle fournie par le titre). `CreateConstantBufferView`, `CreateShaderResourceView`, `CreateUnorderedAccessView`, `CreateRenderTargetView`, `CreateDepthStencilView` et `CreateSampler` écrivent, **en plus** de la table interne `descriptors`, les mots GFX10 dans la case de 32 octets du handle : V# (CBV, vues de tampon : structuré / brut / typé), T# (textures 1D/2D/3D/Cube/tableaux/MSAA, mapping de composants appliqué), S# (`sampler_from_d3d12`). Moitié haute des V#/S# mise à zéro.
- Les écritures n'ont lieu que dans les plages des tas de descripteurs créés par le bridge (`DESC_HEAP_RANGES`) : une adresse hors tas n'est jamais touchée. `CopyDescriptors` / `CopyDescriptorsSimple` copient ces octets (tests : égalité des mots + cohérence avec la table).
- Tuilage (`gfx10_tiling`) : `detile_subresource` / `tile_subresource` (par `DXGI_FORMAT`, BC1..BC7 en blocs) et `detile_surface` / `tile_surface` (taille d'élément 1/2/4/8/16). Champ `D3D12ResourceObject::swizzle_mode: Option<TileMode>` (None par défaut) ; `CopyTextureRegion` (sous-ressource 0 entière, sans offset ni boîte) tuile l'upload dans la mémoire de la ressource (le GPU reçoit toujours l'image linéaire) et détuile le readback vers l'empreinte placée d'un tampon ; `CopyBufferRegion` entre tampon linéaire et texture marquée travaille sur l'image linéaire détuilée.

**Non branché / limites** :
- Aucun chemin de création ne pose `swizzle_mode` : le layout console explicite (`TextureLayout`) n'est pas documenté, donc toutes les ressources restent linéaires (comportement historique). `mark_resource_tiled` existe pour le jour où il est connu.
- Tuilage : sous-ressource 0 / mip 0 seule (pas de mip tail, tableaux, XOR de tuile, ni 3D) ; une allocation plus petite que la surface tuilée retombe sur la copie linéaire.
- RTV/DSV : on écrit le T# de la sous-ressource, pas les registres matériels `CB_COLORn` / `DB_Z_INFO`. T# : adresse = base de la ressource (`>> 8`, tronquée à 256 octets, pas de décalage de sous-ressource ni de plan) ; compteur UAV, `PlaneSlice`, vues de BVH ignorés.
- Dispositions de descripteurs de vues = `d3d12.h` public ; les variantes console éventuelles ne sont pas devinées (descripteur illisible ou dimension inconnue : rien n'est écrit).
- Les mots écrits ne sont pas encore lus par le chemin de rendu (les liaisons GPU utilisent toujours la table interne). `gfx10_descriptors` / `frame_pacer` / `root_signature` : seuls les descripteurs et le tuilage sont branchés ici.
