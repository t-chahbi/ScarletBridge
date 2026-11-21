---
name: scarletbridge-stubgen
description: >-
  Workflow ScarletBridge pour inspecter un binaire ou une DLL Xbox GDK / Scarlett PE32+,
  lister ses imports/exports et générer automatiquement une crate Rust cdylib de stubs
  (avec fichier .def, ABI extern "win64", logging sb_stub! et protection anti-panic FFI).
  Utiliser dès qu'une DLL manquante apparaît ou lorsqu'un nouveau binaire de test est analysé.
---

# ScarletBridge — Inspection PE32+ & Génération de Stubs (`sb-stubgen`)

## Quand utiliser ce skill
- L'utilisateur fournit un nouveau binaire PE32+ (`.exe`) ou une DLL cible (`.dll`) non chiffrée à analyser.
- Wine signale une DLL Xbox/GDK manquante ou une fonction exportée non trouvée (`err:module:import_dll`).
- Tu dois créer rapidement une crate stub Rust compilable en `x86_64-pc-windows-gnu`.

## Procédure étape par étape

### 1. Inspecter le binaire cible
```bash
cargo run -p sb-stubgen -- inspect /chemin/vers/cible.exe
cargo run -p sb-stubgen -- inspect /chemin/vers/cible.dll
```
Vérifier :
- Que le binaire est bien **PE32+ x86-64** (`Machine = 0x8664`).
- La liste des DLL importées (repérer les DLL console : `xmem.dll`, `d3d12_xs.dll`, `XAudio2_9.dll`, `Bink2Scarlett.dll`, `XCurl.dll`, `libHttpClient.GDK.dll`, `amd_fidelityfx_*_xbox.dll`, `XboxBvhBuilder.dll`, `xgameruntime.dll`, etc.).
- Les exports et ordinaux éventuels.

### 2. Générer une crate stub Rust
- **À partir d'une DLL de référence** (export table complète) :
  ```bash
  cargo run -p sb-stubgen -- generate /chemin/vers/foo.dll \
    --out crates/dlls/foo \
    --default-return e-notimpl
  ```
- **À partir de la table d'imports d'un `.exe`** (quand la DLL cible n'est pas disponible) :
  ```bash
  cargo run -p sb-stubgen -- from-imports /chemin/vers/app.exe \
    --dll foo.dll \
    --out crates/dlls/foo \
    --default-return e-notimpl
  ```

### 3. Intégrer & cross-compiler en PE32+
1. Ajouter le chemin de la crate dans `Cargo.toml` (racine, `[workspace.members]`).
2. Compiler la DLL PE32+ :
   ```bash
   cargo build --target x86_64-pc-windows-gnu -p sb-dll-foo
   ```
3. Vérifier la table d'exports générée :
   ```bash
   cargo run -p sb-stubgen -- inspect target/x86_64-pc-windows-gnu/debug/foo.dll
   ```

### 4. Valider sous Wine avec le harnais
```bash
./scripts/run-wine.sh target/x86_64-pc-windows-gnu/debug/dll_probe.exe \
  target/x86_64-pc-windows-gnu/debug/foo.dll
```
Confirmer l'apparition de `DllMain (DLL_PROCESS_ATTACH)` et des traces `SB_STUB` dans les logs, sans crash ni panic.
