---
name: scarletbridge-abi-re
description: >-
  Workflow d'analyse ABI Windows x86-64 / Xbox GDK et rétro-ingénierie assistée (Ghidra MCP,
  llvm-objdump, signatures extern "win64", layouts #[repr(C)], interfaces COM, UTF-16LE).
  Utiliser avant d'implémenter un vrai shim Rust remplaçant un stub dans ScarletBridge.
---

# ScarletBridge — Analyse ABI Windows x86-64 & Rétro-Ingénierie

## Règle d'or
**Ne jamais deviner silencieusement une signature GDK/Scarlett.** Toute fonction qui passe du statut de stub générique à une implémentation réelle doit avoir une signature et un layout mémoire vérifiés (documentation publique GDK, exports/symboles ou Ghidra MCP).

## Convention d'appel Windows x86-64 (`extern "win64"`)
- Premiers arguments entiers/pointeurs : `RCX`, `RDX`, `R8`, `R9` (flottants : `XMM0`–`XMM3`).
- Valeur de retour : `RAX` (ou `XMM0` pour flottants).
- Shadow space : 32 octets réservés sur la pile par l'appelant (géré automatiquement par `extern "win64"` en Rust).
- Structs de taille > 8 octets ou non-puissance de 2 : passées par pointeur caché.
- Méthodes COM : `this: *mut MyComObject` toujours en premier paramètre (`RCX`).

## Checklist d'implémentation d'un shim Rust
1. **Collecter la signature** via :
   - headers publics GDK / Win32 (`windows-sys`),
   - ou **Ghidra MCP** (décompilation de l'appelant, taille des buffers alloués sur la pile, offsets de champs lus/écrits),
   - ou `llvm-objdump -d --x86-asm-syntax=intel` sur les binaires de test.
2. **Documenter** les découvertes dans `docs/abi/<dll_name>.md` (signature, conventions d'erreurs `HRESULT`/`GetLastError`, notes de comportement).
3. **Déclarer les structs ABI** dans `ffi.rs` avec `#[repr(C)]` et assertions compile-time :
   ```rust
   const _: () = {
       assert!(core::mem::size_of::<MyStruct>() == 0x20);
       assert!(core::mem::offset_of!(MyStruct, flags) == 0x08);
   };
   ```
4. **Encapsuler l'entrée FFI** :
   - Utiliser `sb_common::ffi::ffi_guard!` (protège contre tout unwind de panic).
   - Valider tous les pointeurs (`is_null()` → `E_POINTER` ou `E_INVALIDARG`).
   - Convertir les chaînes UTF-16LE via `sb_common::wide`.
   - Déléguer immédiatement à une fonction **safe Rust** testable par `cargo test`.
5. **Écrire un test de régression** dans `tests/harness/` appelant la fonction via `LoadLibrary` / `GetProcAddress` ou liaison statique.
