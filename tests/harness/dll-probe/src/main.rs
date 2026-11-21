//! Micro-harnais PE32+ Phase 0 : charge dynamiquement une DLL cible sous Wine,
//! résout ses symboles et exécute des auto-tests rapides et déterministes.

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "[dll_probe] Ce binaire de harnais doit être compilé pour Windows PE32+ :\n\
         cargo build --target x86_64-pc-windows-gnu -p sb-harness-dll-probe"
    );
}

#[cfg(windows)]
fn main() {
    std::process::exit(win_impl::run());
}

#[cfg(windows)]
mod win_impl {
    use core::ffi::c_void;
    use std::ffi::CString;

    use sb_common::wide::to_wide_null;
    use windows_sys::Win32::Foundation::{FreeLibrary, GetLastError, HMODULE};
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    type XMemAllocFn = unsafe extern "win64" fn(usize, u64) -> *mut c_void;
    type XMemFreeFn = unsafe extern "win64" fn(*mut c_void, u64);
    type XMemSizeFn = unsafe extern "win64" fn(*const c_void, u64) -> usize;

    pub fn run() -> i32 {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.is_empty() {
            print_usage();
            return 2;
        }

        let mut dll_path: Option<String> = None;
        let mut symbols: Vec<String> = Vec::new();
        let mut self_test_xmem = false;

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--self-test-xmem" => {
                    self_test_xmem = true;
                }
                "--symbol" | "-s" => {
                    i += 1;
                    if let Some(sym) = args.get(i) {
                        symbols.push(sym.clone());
                    }
                }
                "--help" | "-h" => {
                    print_usage();
                    return 0;
                }
                other => {
                    if dll_path.is_none() {
                        dll_path = Some(other.to_string());
                    } else {
                        symbols.push(other.to_string());
                    }
                }
            }
            i += 1;
        }

        let target = dll_path.unwrap_or_else(|| "xmem.dll".to_string());
        println!("[dll_probe] Chargement de la DLL : {target}");

        let wide = to_wide_null(&target);
        // SAFETY: `wide` est une chaîne UTF-16LE terminée par 0.
        let module: HMODULE = unsafe { LoadLibraryW(wide.as_ptr()) };
        if module.is_null() {
            // SAFETY: `GetLastError` est toujours sûr à appeler après un échec Win32.
            let err = unsafe { GetLastError() };
            eprintln!(
                "[dll_probe] ÉCHEC LoadLibraryW('{target}') -> GetLastError={err} ({err:#x})"
            );
            return 1;
        }
        println!("[dll_probe] OK : module chargé à {:p}", module);

        let mut exit_code = 0;

        for sym in &symbols {
            match resolve_symbol(module, sym) {
                Some(addr) => println!("[dll_probe]   symbole '{sym}' -> {:#x}", addr),
                None => {
                    eprintln!("[dll_probe]   MANQUANT : symbole '{sym}'");
                    exit_code = 1;
                }
            }
        }

        if self_test_xmem && let Err(msg) = run_xmem_self_test(module) {
            eprintln!("[dll_probe] ÉCHEC auto-test xmem : {msg}");
            exit_code = 1;
        }

        // SAFETY: `module` provient d'un `LoadLibraryW` réussi.
        let freed = unsafe { FreeLibrary(module) };
        println!("[dll_probe] FreeLibrary -> {freed}");

        exit_code
    }

    fn resolve_symbol(module: HMODULE, name: &str) -> Option<usize> {
        let c_name = CString::new(name).ok()?;
        // SAFETY: `module` est un HMODULE valide et `c_name` est une chaîne C terminée par nul.
        let proc = unsafe { GetProcAddress(module, c_name.as_ptr().cast::<u8>()) };
        proc.map(|f| f as usize)
    }

    fn run_xmem_self_test(module: HMODULE) -> Result<(), String> {
        let alloc_addr = resolve_symbol(module, "XMemAlloc")
            .ok_or_else(|| "export XMemAlloc introuvable".to_string())?;
        let size_addr = resolve_symbol(module, "XMemSize")
            .ok_or_else(|| "export XMemSize introuvable".to_string())?;
        let free_addr = resolve_symbol(module, "XMemFree")
            .ok_or_else(|| "export XMemFree introuvable".to_string())?;

        // SAFETY: signatures vérifiées dans `crates/dlls/xmem/src/ffi.rs`.
        let xmem_alloc: XMemAllocFn = unsafe { core::mem::transmute(alloc_addr) };
        let xmem_size: XMemSizeFn = unsafe { core::mem::transmute(size_addr) };
        let xmem_free: XMemFreeFn = unsafe { core::mem::transmute(free_addr) };

        // Attributs : Physical (bit 31) + ZeroInit (bit 14) + 64 KiB alignment (code 5 << 15).
        let attrs: u64 = ((1u32 << 31) | (1u32 << 14) | (5u32 << 15)) as u64;
        let requested = 65536usize;

        // SAFETY: appel ABI `extern "win64"` vers xmem.dll chargée.
        let ptr = unsafe { xmem_alloc(requested, attrs) };
        if ptr.is_null() {
            return Err("XMemAlloc a retourné NULL".to_string());
        }
        if !(ptr as usize).is_multiple_of(65536) {
            return Err(format!("pointeur non aligné 64 KiB : {:p}", ptr));
        }

        // SAFETY: `ptr` pointe vers `requested` octets alloués et mis à zéro.
        let bytes = unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), requested) };
        if !bytes.iter().all(|&b| b == 0) {
            return Err("le tampon alloué avec zero_initialize n'est pas nul".to_string());
        }
        bytes.fill(0xA5);

        // SAFETY: `ptr` est une allocation XMem active.
        let reported_size = unsafe { xmem_size(ptr, attrs) };
        if reported_size != requested {
            return Err(format!(
                "XMemSize inattendu : attendu {requested}, reçu {reported_size}"
            ));
        }

        // SAFETY: `ptr` est libéré exactement une fois.
        unsafe { xmem_free(ptr, attrs) };

        println!(
            "[dll_probe] Auto-test xmem.dll RÉUSSI ({requested} octets, aligné 64 KiB, motif vérifié)"
        );
        Ok(())
    }

    fn print_usage() {
        println!("Usage : dll_probe.exe <chemin.dll> [--symbol <nom> ...] [--self-test-xmem]");
    }
}
