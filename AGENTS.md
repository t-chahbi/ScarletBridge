# ScarletBridge — Règles Projet (Agents)

Référence complète d'architecture : [LLM.md](./LLM.md).

## Règles impératives

1. **100 % Rust pour le code ScarletBridge** : tout nouveau code (DLL PE32+, outils, VFS, runtime, harnais de test, traducteurs) est écrit en **Rust (Edition 2024)**. Ne jamais ajouter de `.c`/`.cpp` pour des shims ou outils internes (FFI uniquement vers les briques open-source existantes dans `third_party/`).
2. **Périmètre légal & technique** : uniquement des binaires **PE32+ x86-64 non chiffrés** (harnais de test, homebrew, SDK). Aucun contournement DRM / déchiffrement.
3. **ABI Windows x86-64 stricte** :
   - Tous les exports DLL utilisent `#[unsafe(no_mangle)] pub extern "win64" fn ...` + fichier `.def` de contrôle des exports.
   - Zéro unwind à travers la frontière FFI : envelopper tout point d'entrée dans `sb_common::ffi::ffi_guard!`.
   - Isoler `unsafe` dans `ffi.rs` avec un commentaire `// SAFETY:` obligatoire.
   - Vérifier les structs `#[repr(C)]` par assertions compile-time (`size_of`, `offset_of`).
4. **Isolation & Sécurité** :
   - Le VFS (`sb-vfs`) ne doit **jamais** exposer `/home` ni tolérer le path traversal (`..`, symlinks sortants).
   - Mode offline strict (`SB_OFFLINE_MODE=1`) activé par défaut.
5. **Boucle courte pilotée par les tests** : ne jamais attaquer directement un AAA ; chaque nouvelle API implémentée doit avoir un test unitaire `cargo test` et/ou un test dédié dans `tests/harness/`.
6. **Qualité obligatoire avant fin de tâche** : `cargo fmt --check`, `cargo clippy --workspace`, `cargo test --workspace`, et vérification cross-build `cargo build --target x86_64-pc-windows-gnu` pour les crates DLL/harnais touchées.
