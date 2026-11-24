//! Si l'adresse préférée d'une image PE est déjà occupée, le loader doit reloger l'image (table
//! `.reloc`) ou refuser avec un message explicite — jamais la charger à de mauvaises adresses.

use std::path::PathBuf;

const PREFERRED_BASE: usize = 0x1_4000_0000;

fn harness_exe() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let exe = root.join("target/x86_64-pc-windows-gnu/debug/mt_io_stress.exe");
    exe.is_file().then_some(exe)
}

#[test]
fn occupied_preferred_base_relocates_or_fails_explicitly() {
    let Some(exe) = harness_exe() else {
        eprintln!("harnais PE absent : test ignoré");
        return;
    };
    // Occupe la plage préférée (16 Mio, largement plus que l'image du harnais).
    // SAFETY: mapping anonyme PROT_NONE de 16 Mio à une adresse libre (`MAP_FIXED_NOREPLACE`),
    // conservé pendant tout le test.
    let blocker = unsafe {
        libc::mmap(
            PREFERRED_BASE as *mut libc::c_void,
            16 << 20,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
            -1,
            0,
        )
    };
    if blocker == libc::MAP_FAILED {
        eprintln!("plage préférée déjà occupée dans ce processus : test ignoré");
        return;
    }

    let manager = sb_loader::ModuleManager::new();
    match manager.load_executable(&exe) {
        Ok(module) => {
            assert_ne!(
                module.base, PREFERRED_BASE,
                "l'image ne peut pas être à une base occupée"
            );
            assert!(module.entry_point.is_some_and(|e| module.contains(e)));
        }
        Err(err) => {
            let msg = err.to_string();
            assert!(
                msg.contains("relocation"),
                "l'échec doit expliquer l'absence de relocations : {msg}"
            );
        }
    }
    // SAFETY: libération du mapping bloqueur créé plus haut.
    unsafe { libc::munmap(blocker, 16 << 20) };
}
