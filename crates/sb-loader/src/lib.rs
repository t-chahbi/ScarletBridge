//! Loader PE32+ x86-64 natif Linux pour ScarletBridge (sans Wine).
//!
//! Prend en charge :
//! - mapping mémoire virtuel (`mmap` / `mprotect`),
//! - relocations de base (`.reloc` DIR64 / HIGHLOW),
//! - initialisation `.tls` et `TEB`/`PEB` sur le registre segment `GS` (`arch_prctl`),
//! - résolution IAT vers les fonctions Rust `extern "win64"` (`sb-hle`) ou DLL invitées,
//! - génération dynamique de trampolines JIT x86-64 pour les imports manquants,
//! - diagnostics de signaux matériels (`SIGSEGV` / `SIGILL` avec `module+0xRVA`),
//! - tables `.pdata` et déroulage de pile x64 (`RtlLookupFunctionEntry` / `RtlVirtualUnwind`).

pub mod pe_loader;
pub mod signal;
pub mod teb;
pub mod trampoline;
pub mod unwind;

pub use pe_loader::{
    HleResolver, ImportSymbol, LoadedModule, LoaderError, ModuleManager, module_manager,
};
pub use teb::{
    Peb, Teb, get_teb_tls_slot, init_thread_teb, last_error, process_heap_handle, set_last_error,
    set_teb_tls_slot,
};
