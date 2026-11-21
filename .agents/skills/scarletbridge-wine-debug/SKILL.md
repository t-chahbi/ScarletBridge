---
name: scarletbridge-wine-debug
description: >-
  Procédure d'exécution native des binaires PE32+ sous le loader ScarletBridge (sb-loader / sb-launcher),
  lecture des traces SB_LOG et des appels JIT stubbés, et débogage CPU avec rust-gdb / llvm-objdump.
  Utiliser pour lancer, tracer ou diagnostiquer un crash d'un binaire PE32+ sous ScarletBridge.
---

# ScarletBridge — Exécution Native PE32+, Traces & Débogage CPU (Sans Wine)

## 1. Exécution native d'un binaire ou dossier Xbox / GDK
ScarletBridge exécute directement les PE32+ x86-64 sous Linux via son loader intégré (`sb-loader` + `sb-hle`) :
```bash
# Auto-test du harnais PE32+ dll_probe.exe chargeant xmem.dll
mise exec -- cargo run -p sb-launcher -- run \
  target/x86_64-pc-windows-gnu/debug/dll_probe.exe \
  -- target/x86_64-pc-windows-gnu/debug/xmem.dll --self-test-xmem

# Lancement tracé d'un titre extrait (ex: OpenLara Xbox)
SB_LOG=debug mise exec -- cargo run -p sb-launcher -- run tmp/openlara-xb1/app
```

## 2. Diagnostic par niveaux de logs
- **Vue globale des chargements PE32+, relocations, TEB et stubs** :
  ```bash
  SB_LOG="debug" mise exec -- cargo run -p sb-launcher -- run <cible>
  ```
- **Suivi détaillé du loader et d'un module HLE** :
  ```bash
  SB_LOG="info,loader=trace,hle=debug,xmem=trace" SB_STUB_ONCE=0 mise exec -- cargo run -p sb-launcher -- run <cible>
  ```
- **Capturer la stack trace à chaque stub non implémenté** :
  ```bash
  SB_STUB_BACKTRACE=1 SB_LOG="debug" mise exec -- cargo run -p sb-launcher -- run <cible>
  ```

## 3. Boucle de résolution d'un crash ou d'un appel stubbé
1. Repérer le dernier `[SB][STUB]` émis par le trampoline JIT (affiche le nom exact `dll!fonction` et les 4 premiers registres Win64 `rcx, rdx, r8, r9`) ou l'offset `module+0xRVA` du gestionnaire de signal `SIGSEGV`.
2. Désassembler l'instruction appelante avec `llvm-objdump -d --x86-asm-syntax=intel` ou inspecter la fonction dans **Ghidra MCP** (skill `scarletbridge-abi-re`).
3. Implémenter la fonction `extern "win64"` correspondante dans `crates/sb-hle/` et ajouter un test unitaire `cargo test -p sb-hle`.
4. Relancer immédiatement `sb-launcher run` et observer l'avancement.
