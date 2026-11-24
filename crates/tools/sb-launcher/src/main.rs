use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

mod doctor;
mod requirements;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use sb_common::sb_info;
use sb_hle::DefaultHleResolver;
use sb_loader::{HleResolver, ImportSymbol, module_manager, signal};
use sb_stubgen::pe::{import_name_to_export, inspect_pe};
use sb_vfs::{AppManifest, VfsMount, VirtualFs};

#[derive(Debug, Parser)]
#[command(
    name = "scarletbridge",
    about = "Runtime autonome ScarletBridge (sans Wine) : loader PE32+ natif Linux, VFS isolé et HLE Rust"
)]
struct Cli {
    #[command(subcommand)]
    command: SubCmd,
}

#[derive(Debug, Subcommand)]
enum SubCmd {
    /// Inventaire complet de ce qu'un binaire demande (imports, GDK, shaders, CPU, fichiers, bundle…).
    #[command(alias = "needs")]
    Requirements {
        /// Chemin d'un `.exe` PE32+ ou d'un dossier de bundle.
        target: PathBuf,
        /// Répertoire de recherche des DLL invitées PE32+ complémentaires.
        #[arg(long, default_value = "target/x86_64-pc-windows-gnu/debug")]
        dll_dir: PathBuf,
        /// Sortie JSON complète (listes non tronquées).
        #[arg(long)]
        json: bool,
        /// Ne balaie pas le code à la recherche des instructions CPU utilisées (plus rapide).
        #[arg(long)]
        no_cpu: bool,
    },

    /// Vérifie l'environnement (noyau, mémoire, CPU, Vulkan, SDL3, affichage, audio, droits).
    Doctor,

    /// Inspecte un répertoire d'application Xbox/GDK ou un binaire PE32+ (couverture HLE native).
    Inspect {
        /// Répertoire contenant l'application ou chemin direct vers un binaire PE32+.
        target: PathBuf,
        /// Répertoire de recherche des DLL invitées PE32+ complémentaires.
        #[arg(long, default_value = "target/x86_64-pc-windows-gnu/debug")]
        dll_dir: PathBuf,
        /// Liste les symboles importés non couverts par le HLE natif.
        #[arg(long)]
        missing: bool,
    },

    /// Exécute nativement sous Linux un titre/binaire PE32+ x86-64 via le loader ScarletBridge.
    Run {
        /// Répertoire de l'application (avec manifeste) ou chemin vers un `.exe` PE32+.
        target: PathBuf,
        /// Répertoire supplémentaire où chercher des DLL PE32+ invitées (ex: xmem.dll).
        #[arg(long, default_value = "target/x86_64-pc-windows-gnu/debug")]
        dll_dir: PathBuf,
        /// Répertoire racine où sont isolés `save/` et `temp/` (jamais `/home` directement).
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Autoriser le réseau (désactivé par défaut : mode offline strict).
        #[arg(long)]
        online: bool,
        /// Désactiver la fenêtre SDL3/Wayland et la sortie audio PipeWire (mode CI/terminal).
        #[arg(long)]
        headless: bool,
        /// Arrêter proprement après N frames présentées (utile pour tests automatisés).
        #[arg(long)]
        max_frames: Option<u64>,
        /// Sauvegarder la frame GPU rendue au format PPM lors du `Present`.
        #[arg(long)]
        dump_frame: Option<PathBuf>,
        /// Charger et mapper le PE32+ sans sauter au point d'entrée.
        #[arg(long)]
        dry_run: bool,
        /// Arguments transmis au binaire PE32+.
        #[arg(last = true)]
        args: Vec<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        SubCmd::Requirements {
            target,
            dll_dir,
            json,
            no_cpu,
        } => requirements::run(
            &target,
            &requirements::Options {
                json,
                no_cpu,
                dll_dir,
            },
        ),
        SubCmd::Doctor => {
            std::process::exit(doctor::run());
        }
        SubCmd::Inspect {
            target,
            dll_dir,
            missing,
        } => cmd_inspect(&target, &dll_dir, missing),
        SubCmd::Run {
            target,
            dll_dir,
            state_dir,
            online,
            headless,
            max_frames,
            dump_frame,
            dry_run,
            args,
        } => cmd_run(RunOptions {
            target,
            dll_dir,
            state_dir,
            online,
            headless,
            max_frames,
            dump_frame,
            dry_run,
            args,
        }),
    }
}

struct RunOptions {
    target: PathBuf,
    dll_dir: PathBuf,
    state_dir: Option<PathBuf>,
    online: bool,
    headless: bool,
    max_frames: Option<u64>,
    dump_frame: Option<PathBuf>,
    dry_run: bool,
    args: Vec<String>,
}

fn collect_dll_names(dir: &Path) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return set;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_file()
            && let Some(ext) = p.extension().and_then(|e| e.to_str())
            && ext.eq_ignore_ascii_case("dll")
            && let Some(name) = p.file_name().and_then(|n| n.to_str())
        {
            set.insert(name.to_ascii_lowercase());
        }
    }
    set
}

fn cmd_inspect(target: &Path, dll_dir: &Path, list_missing: bool) -> Result<()> {
    let hle = DefaultHleResolver::new();
    let guest_dlls = collect_dll_names(dll_dir);

    let (exe_path, game_dir) = if target.is_dir() {
        let manifest = match AppManifest::discover_in_dir(target) {
            Ok(m) => {
                println!("=== Manifeste détecté : {:?} ===", m.kind);
                println!(
                    "  Titre   : {}",
                    m.display_name
                        .as_deref()
                        .or(m.identity_name.as_deref())
                        .unwrap_or("<inconnu>")
                );
                println!("  TitleId : {}", m.sandbox_id());
                if let Some(mb) = m.persistent_local_storage_mb {
                    println!("  PLS     : {mb} MiB");
                }
                if !m.languages.is_empty() {
                    println!("  Langues : {}", m.languages.join(", "));
                }
                Some(m)
            }
            Err(_) => {
                println!(
                    "Aucun MicrosoftGame.Config / AppxManifest.xml trouvé dans {}",
                    target.display()
                );
                None
            }
        };

        let exe = manifest
            .as_ref()
            .and_then(|m| m.primary_executable())
            .map(|e| target.join(&e.name));
        (exe, target.to_path_buf())
    } else {
        let dir = target
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        (Some(target.to_path_buf()), dir)
    };

    let bundled_dlls = collect_dll_names(&game_dir);

    let Some(exe) = exe_path else {
        println!(
            "Aucun exécutable principal détecté ; passez directement le chemin d'un .exe/.dll."
        );
        return Ok(());
    };

    if !exe.exists() {
        bail!("exécutable introuvable : {}", exe.display());
    }

    let summary = inspect_pe(&exe)?;
    println!("\n=== Binaire cible : {} ===", summary.path);
    println!("  Architecture : {}", summary.machine);

    let mut total_syms = 0usize;
    let mut covered_hle = 0usize;

    println!("\nCouverture HLE native ScarletBridge (sans Wine) :");
    for imp in &summary.imports {
        let mut hle_hits = 0usize;
        let mut missing = Vec::new();
        for fn_name in &imp.functions {
            let exp = import_name_to_export(fn_name);
            let sym = match exp.ordinal {
                Some(ord) => ImportSymbol::Ordinal(ord),
                None => ImportSymbol::Name(exp.name),
            };
            if hle.resolve(&imp.name, &sym).is_some() {
                hle_hits += 1;
            } else {
                missing.push(fn_name.as_str());
            }
        }
        total_syms += imp.functions.len();
        covered_hle += hle_hits;

        let lower = imp.name.to_ascii_lowercase();
        let status = if hle_hits == imp.functions.len() {
            "HLE NATIF (100 %)"
        } else if hle_hits > 0 {
            "HLE PARTIEL + JIT STUBS"
        } else if bundled_dlls.contains(&lower) || guest_dlls.contains(&lower) {
            "DLL INVITÉE PE32+"
        } else {
            "JIT STUBS AUTO"
        };

        println!(
            "  {:<38} {:<24} ({hle_hits}/{})",
            imp.name,
            status,
            imp.functions.len()
        );
        if list_missing {
            for name in &missing {
                println!("      - {name}");
            }
        }
    }

    let pct = (covered_hle * 100).checked_div(total_syms).unwrap_or(100);
    println!(
        "\nRésumé : {covered_hle}/{total_syms} symboles couverts en HLE Rust natif ({pct} %), le reste est tracé automatiquement par trampolines JIT."
    );

    Ok(())
}

fn cmd_run(opts: RunOptions) -> Result<()> {
    if opts.headless {
        // SAFETY: configuré avant le spawn de tout thread invité.
        unsafe {
            std::env::set_var("SB_HEADLESS", "1");
        }
    }
    if let Some(limit) = opts.max_frames {
        // SAFETY: configuré avant le spawn de tout thread invité.
        unsafe {
            std::env::set_var("SB_MAX_FRAMES", limit.to_string());
        }
    }
    if let Some(ref path) = opts.dump_frame {
        // SAFETY: configuré avant le spawn de tout thread invité.
        unsafe {
            std::env::set_var("SB_DUMP_FRAME", path);
        }
    }
    sb_common::log::init();

    let (game_dir, exe_path, manifest) = if opts.target.is_dir() {
        let manifest = AppManifest::discover_in_dir(&opts.target).with_context(|| {
            format!(
                "impossible de découvrir le manifeste dans {}",
                opts.target.display()
            )
        })?;
        let primary = manifest
            .primary_executable()
            .context("le manifeste ne déclare aucun exécutable")?;
        let exe = opts.target.join(&primary.name);
        (opts.target.clone(), exe, Some(manifest))
    } else {
        let dir = opts
            .target
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let manifest = AppManifest::discover_in_dir(&dir).ok();
        (dir, opts.target.clone(), manifest)
    };

    if !exe_path.exists() {
        bail!("binaire introuvable : {}", exe_path.display());
    }

    let title_id = manifest
        .as_ref()
        .map(AppManifest::sandbox_id)
        .unwrap_or_else(|| {
            exe_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "standalone".to_string())
        });

    let state_base = opts.state_dir.unwrap_or_else(default_state_dir);
    let quota_mb = manifest
        .as_ref()
        .and_then(|m| m.persistent_local_storage_mb);
    let vfs = VirtualFs::new_sandboxed(&game_dir, &state_base, &title_id, quota_mb)
        .context("échec d'initialisation du VFS isolé")?;

    sb_info!(
        "launcher",
        "ScarletBridge Standalone (sans Wine) — titre='{title_id}', offline={}, game={}, save={}, temp={}",
        !opts.online,
        vfs.mount_root(VfsMount::Game).display(),
        vfs.mount_root(VfsMount::Save).display(),
        vfs.mount_root(VfsMount::Temp).display()
    );

    // 1. Initialiser l'état du processus invité + le résolveur HLE natif Rust.
    let exe_name = exe_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "app.exe".to_string());
    sb_hle::process::init_process(&exe_name, &opts.args, Some(vfs));

    let mgr = module_manager();
    mgr.set_resolver(DefaultHleResolver::new().into_arc());
    mgr.add_search_path(game_dir);
    if opts.dll_dir.exists() {
        mgr.add_search_path(opts.dll_dir);
    }

    // 2. Installer les diagnostics matériels (SIGSEGV/SIGILL -> module+0xRVA).
    signal::install_crash_handlers();

    // 3. Charger le PE32+ en mémoire Linux (mmap, .reloc, IAT, TEB/GS, .tls, mprotect).
    let module = mgr
        .load_executable(&exe_path)
        .with_context(|| format!("échec de chargement PE32+ de {}", exe_path.display()))?;

    if opts.dry_run {
        println!(
            "=== ScarletBridge Standalone (dry-run) ===\n  Module   : {}\n  Base     : {:#x}..{:#x}\n  Entry    : {:#x}\n  Imports  : HLE={}, DLL invitées={}, trampolines JIT={}",
            module.name,
            module.base,
            module.base + module.image_size,
            module.entry_point.unwrap_or(0),
            module.resolved_hle,
            module.resolved_guest,
            module.stubbed_jit
        );
        return Ok(());
    }

    // 4. Exécuter le point d'entrée x86-64 nativement sous Linux !
    // SAFETY: le PE32+ a été mappé, relocalisé, lié en IAT et le TEB `GS` est initialisé.
    let exit_code = unsafe { mgr.execute_module(&module) }
        .with_context(|| format!("échec d'exécution de {}", exe_path.display()))?;

    sb_info!(
        "launcher",
        "exécutable terminé proprement (code={exit_code})"
    );
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

fn default_state_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("scarletbridge").join("titles");
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("scarletbridge")
            .join("titles");
    }
    PathBuf::from(".scarletbridge-state")
}
