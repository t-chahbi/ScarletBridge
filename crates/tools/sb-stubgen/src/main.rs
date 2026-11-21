use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use sb_stubgen::codegen::{DefaultReturn, StubGenConfig, sanitize_crate_stem, write_stub_crate};
use sb_stubgen::def::parse_def_file;
use sb_stubgen::pe::{DllCategory, ExportedSymbol, import_name_to_export, inspect_pe};

#[derive(Debug, Parser)]
#[command(
    name = "sb-stubgen",
    about = "Inspecteur PE32+ et générateur automatique de crates stubs Rust pour ScarletBridge"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspecte un binaire PE32+ (.exe ou .dll) et affiche ses imports et exports.
    Inspect {
        /// Chemin vers le binaire PE32+.
        path: PathBuf,
        /// Affiche la sortie au format JSON.
        #[arg(long)]
        json: bool,
    },

    /// Génère une crate Rust stub à partir de la table d'exports d'une DLL PE32+.
    Generate {
        /// Chemin vers la DLL source.
        dll_path: PathBuf,
        /// Répertoire de sortie de la crate générée.
        #[arg(short, long)]
        out: PathBuf,
        /// Nom de la DLL exportée (défaut : nom du fichier).
        #[arg(long)]
        dll_name: Option<String>,
        /// Nom de la crate Cargo (défaut : sb-dll-<stem>).
        #[arg(long)]
        crate_name: Option<String>,
        /// Valeur de retour par défaut des fonctions stubbées.
        #[arg(long, value_enum, default_value_t = DefaultReturn::ENotimpl)]
        default_return: DefaultReturn,
        /// Chemin local vers `sb-common` (si hors workspace).
        #[arg(long)]
        sb_common_path: Option<String>,
    },

    /// Génère une crate Rust stub pour une DLL manquante à partir des imports d'un exécutable PE32+.
    FromImports {
        /// Chemin vers le binaire (.exe ou .dll) qui importe la DLL manquante.
        binary: PathBuf,
        /// Nom de la DLL importée à stubber (insensible à la casse, ex: xmem.dll).
        #[arg(long)]
        dll: String,
        /// Répertoire de sortie de la crate générée.
        #[arg(short, long)]
        out: PathBuf,
        /// Nom de la crate Cargo (défaut : sb-dll-<stem>).
        #[arg(long)]
        crate_name: Option<String>,
        /// Valeur de retour par défaut des fonctions stubbées.
        #[arg(long, value_enum, default_value_t = DefaultReturn::ENotimpl)]
        default_return: DefaultReturn,
        /// Redirige chaque export PE32+ vers une autre DLL (ex: `vcruntime140`, `msvcp140`).
        #[arg(long)]
        forward_to: Option<String>,
        /// Chemin local vers `sb-common` (si hors workspace).
        #[arg(long)]
        sb_common_path: Option<String>,
    },

    /// Génère une crate Rust stub à partir d'un fichier `.def`.
    FromDef {
        /// Chemin vers le fichier `.def`.
        def_path: PathBuf,
        /// Répertoire de sortie de la crate générée.
        #[arg(short, long)]
        out: PathBuf,
        /// Nom de la DLL (surcharge la directive LIBRARY du .def).
        #[arg(long)]
        dll_name: Option<String>,
        /// Nom de la crate Cargo (défaut : sb-dll-<stem>).
        #[arg(long)]
        crate_name: Option<String>,
        /// Valeur de retour par défaut des fonctions stubbées.
        #[arg(long, value_enum, default_value_t = DefaultReturn::ENotimpl)]
        default_return: DefaultReturn,
        /// Chemin local vers `sb-common` (si hors workspace).
        #[arg(long)]
        sb_common_path: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Inspect { path, json } => {
            let summary = inspect_pe(&path)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                println!("=== PE Inspection : {} ===", summary.path);
                println!(
                    "Architecture : {} | Type : {} | Entry : {:#x} | ImageBase : {:#x}",
                    summary.machine,
                    if summary.is_dll { "DLL" } else { "EXE" },
                    summary.entry_point,
                    summary.image_base
                );

                println!("\nImports ({} DLL) :", summary.imports.len());
                for imp in &summary.imports {
                    let badge = match imp.category {
                        DllCategory::XboxConsole => "[XBOX/GDK]",
                        DllCategory::WineSystem => "[WINE/WIN32]",
                        DllCategory::ThirdParty => "[TIERS]",
                    };
                    println!(
                        "  {badge:<12} {:<32} ({} symboles)",
                        imp.name,
                        imp.functions.len()
                    );
                    if imp.category == DllCategory::XboxConsole {
                        for func in imp.functions.iter().take(12) {
                            println!("      - {func}");
                        }
                        if imp.functions.len() > 12 {
                            println!("      ... (+{} autres)", imp.functions.len() - 12);
                        }
                    }
                }

                println!("\nExports ({} symboles) :", summary.exports.len());
                for exp in summary.exports.iter().take(40) {
                    if let Some(ref fwd) = exp.forwarder {
                        println!("  {:<40} RVA={:#x} -> {fwd}", exp.name, exp.rva);
                    } else {
                        println!("  {:<40} RVA={:#x}", exp.name, exp.rva);
                    }
                }
                if summary.exports.len() > 40 {
                    println!("  ... (+{} autres exports)", summary.exports.len() - 40);
                }
            }
        }

        Command::Generate {
            dll_path,
            out,
            dll_name,
            crate_name,
            default_return,
            sb_common_path,
        } => {
            let summary = inspect_pe(&dll_path)?;
            let dll_name = dll_name.unwrap_or_else(|| {
                dll_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "stub.dll".to_string())
            });
            generate_crate(
                &out,
                dll_name,
                crate_name,
                default_return,
                None,
                sb_common_path,
                summary.exports,
            )?;
        }

        Command::FromImports {
            binary,
            dll,
            out,
            crate_name,
            default_return,
            forward_to,
            sb_common_path,
        } => {
            let summary = inspect_pe(&binary)?;
            let found = summary
                .imports
                .iter()
                .find(|imp| imp.name.eq_ignore_ascii_case(&dll))
                .with_context(|| {
                    format!("la DLL '{dll}' n'est pas importée par {}", binary.display())
                })?;

            let exports: Vec<ExportedSymbol> = found
                .functions
                .iter()
                .map(|name| import_name_to_export(name))
                .collect();

            generate_crate(
                &out,
                found.name.clone(),
                crate_name,
                default_return,
                forward_to,
                sb_common_path,
                exports,
            )?;
        }

        Command::FromDef {
            def_path,
            out,
            dll_name,
            crate_name,
            default_return,
            sb_common_path,
        } => {
            let def = parse_def_file(&def_path)?;
            let dll_name = dll_name.or(def.library_name).unwrap_or_else(|| {
                let stem = def_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "stub".to_string());
                format!("{stem}.dll")
            });
            if def.exports.is_empty() {
                bail!("aucun symbole exporté trouvé dans {}", def_path.display());
            }
            generate_crate(
                &out,
                dll_name,
                crate_name,
                default_return,
                None,
                sb_common_path,
                def.exports,
            )?;
        }
    }

    Ok(())
}

fn generate_crate(
    out: &Path,
    dll_name: String,
    crate_name: Option<String>,
    default_return: DefaultReturn,
    forward_to: Option<String>,
    sb_common_path: Option<String>,
    exports: Vec<ExportedSymbol>,
) -> Result<()> {
    let lib_name = sanitize_crate_stem(&dll_name);
    let crate_name = crate_name.unwrap_or_else(|| format!("sb-dll-{}", lib_name.replace('_', "-")));
    let count = exports.len();

    let cfg = StubGenConfig {
        dll_name: dll_name.clone(),
        crate_name: crate_name.clone(),
        lib_name: lib_name.clone(),
        channel_name: lib_name,
        default_return,
        forward_to,
        sb_common_path,
        exports,
    };

    write_stub_crate(out, &cfg)?;
    println!(
        "[sb-stubgen] Crate '{crate_name}' générée dans {} ({count} symboles pour {dll_name})",
        out.display()
    );
    Ok(())
}
