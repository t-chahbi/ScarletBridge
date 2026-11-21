//! Génération d'une crate Rust `cdylib` PE32+ complète (Cargo.toml, build.rs, .def, src/lib.rs).

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use clap::ValueEnum;

use crate::pe::ExportedSymbol;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DefaultReturn {
    /// Retourne `E_NOTIMPL` (`0x80004001`, recommandé par défaut pour COM/GDK).
    ENotimpl,
    /// Retourne `S_OK` (`0`).
    SOk,
    /// Retourne `0` (`S_OK` / `FALSE` / `NULL`).
    Zero,
    /// Retourne `E_FAIL` (`0x80004005`).
    EFail,
}

impl DefaultReturn {
    pub const fn rust_expr(self) -> &'static str {
        match self {
            Self::ENotimpl => "sb_common::abi::E_NOTIMPL",
            Self::SOk => "sb_common::abi::S_OK",
            Self::Zero => "0",
            Self::EFail => "sb_common::abi::E_FAIL",
        }
    }
}

pub struct StubGenConfig {
    pub dll_name: String,
    pub crate_name: String,
    pub lib_name: String,
    pub channel_name: String,
    pub default_return: DefaultReturn,
    /// Si défini, redirige chaque export PE32+ vers `<forward_to>.<symbol>` dans le `.def`.
    pub forward_to: Option<String>,
    /// Chemin optionnel vers `crates/sb-common` (si `None`, utilise `sb-common.workspace = true`).
    pub sb_common_path: Option<String>,
    pub exports: Vec<ExportedSymbol>,
}

pub fn sanitize_crate_stem(dll_name: &str) -> String {
    let stem = dll_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(dll_name)
        .to_ascii_lowercase();
    let stem = stem.strip_suffix(".dll").unwrap_or(&stem);
    let mut out: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() || out.as_bytes()[0].is_ascii_digit() {
        out.insert_str(0, "dll_");
    }
    out
}

pub fn sanitize_ident(symbol: &str, index: usize) -> String {
    let cleaned: String = symbol
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('_');
    if trimmed.is_empty() {
        format!("stub_{index}")
    } else {
        format!("stub_{index}_{trimmed}")
    }
}

pub fn render_cargo_toml(cfg: &StubGenConfig) -> String {
    let (sb_common_dep, lints_section) = match &cfg.sb_common_path {
        Some(path) => (format!("sb-common = {{ path = \"{path}\" }}"), ""),
        None => (
            "sb-common.workspace = true".to_string(),
            "\n[lints]\nworkspace = true\n",
        ),
    };

    let dep_section = if cfg.forward_to.is_some() {
        "[target.'cfg(windows)'.dependencies]"
    } else {
        "[dependencies]"
    };

    format!(
        r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2024"
license = "MIT OR Apache-2.0"

[lib]
name = "{lib_name}"
crate-type = ["cdylib", "rlib"]

{dep_section}
{sb_common_dep}
{lints_section}"#,
        crate_name = cfg.crate_name,
        lib_name = cfg.lib_name,
    )
}

pub fn render_build_rs(lib_name: &str) -> String {
    format!(
        r#"use std::env;
use std::path::PathBuf;

fn main() {{
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {{
        let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
        let def_path = manifest_dir.join("{lib_name}.def");
        println!("cargo:rerun-if-changed={{}}", def_path.display());
        println!("cargo:rustc-link-arg={{}}", def_path.display());
    }}
}}
"#
    )
}

pub fn render_def_file(cfg: &StubGenConfig) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "; Généré automatiquement par sb-stubgen pour {}",
        cfg.dll_name
    );
    let _ = writeln!(out, "LIBRARY \"{}\"", cfg.dll_name);
    let _ = writeln!(out, "EXPORTS");

    let mut seen = HashSet::new();
    for sym in &cfg.exports {
        if !seen.insert((sym.name.as_str(), sym.ordinal)) {
            continue;
        }
        let needs_quotes = sym
            .name
            .chars()
            .any(|c| !c.is_ascii_alphanumeric() && c != '_');
        let rendered_name = if needs_quotes {
            format!("\"{}\"", sym.name)
        } else {
            sym.name.clone()
        };
        let target_rhs = cfg.forward_to.as_ref().map(|target_dll| {
            let clean_dll = target_dll.strip_suffix(".dll").unwrap_or(target_dll);
            let fwd = format!("{clean_dll}.{}", sym.name);
            if needs_quotes {
                format!(" = \"{fwd}\"")
            } else {
                format!(" = {fwd}")
            }
        });
        let rhs = target_rhs.as_deref().unwrap_or("");
        if let Some(ord) = sym.ordinal {
            let _ = writeln!(out, "    {rendered_name}{rhs} @{ord}");
        } else {
            let _ = writeln!(out, "    {rendered_name}{rhs}");
        }
    }
    out
}

pub fn render_lib_rs(cfg: &StubGenConfig) -> String {
    let mut out = String::new();
    let ret_expr = cfg.default_return.rust_expr();

    let _ = writeln!(
        out,
        "//! Crate stub générée automatiquement par `sb-stubgen` pour `{}`.",
        cfg.dll_name
    );
    let _ = writeln!(
        out,
        "//! Remplacez progressivement chaque stub par un shim vérifié (voir LLM.md)."
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "#![allow(non_snake_case, unused_variables)]");
    let _ = writeln!(out);
    let _ = writeln!(out, "#[cfg(target_os = \"windows\")]");
    let _ = writeln!(out, "use core::ffi::c_void;");
    let _ = writeln!(out, "#[cfg(target_os = \"windows\")]");
    let _ = writeln!(
        out,
        "use sb_common::abi::{{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH, WIN_TRUE, WinBool}};"
    );
    let _ = writeln!(out, "#[cfg(target_os = \"windows\")]");
    let _ = writeln!(out, "use sb_common::sb_info;");
    if cfg.forward_to.is_none() {
        let _ = writeln!(out, "use sb_common::abi::HResult;");
        let _ = writeln!(out, "use sb_common::{{ffi_guard, sb_stub}};");
    } else {
        let _ = writeln!(out, "#[cfg(target_os = \"windows\")]");
        let _ = writeln!(out, "use sb_common::ffi_guard;");
    }
    let _ = writeln!(out);
    if cfg.forward_to.is_some() {
        let _ = writeln!(out, "#[cfg(target_os = \"windows\")]");
    }
    let _ = writeln!(out, "const CHANNEL: &str = \"{}\";", cfg.channel_name);
    let _ = writeln!(out);
    let _ = writeln!(out, "#[cfg(target_os = \"windows\")]");
    let _ = writeln!(out, "#[unsafe(no_mangle)]");
    let _ = writeln!(
        out,
        "pub extern \"win64\" fn DllMain(_instance: *mut c_void, reason: u32, _reserved: *mut c_void) -> WinBool {{"
    );
    let _ = writeln!(out, "    ffi_guard!(CHANNEL, \"DllMain\", WIN_TRUE, {{");
    let _ = writeln!(out, "        match reason {{");
    let _ = writeln!(out, "            DLL_PROCESS_ATTACH => {{");
    let _ = writeln!(out, "                sb_common::log::init();");
    let _ = writeln!(
        out,
        "                sb_info!(CHANNEL, \"DLL stub chargée : {}\");",
        cfg.dll_name
    );
    let _ = writeln!(out, "            }}");
    let _ = writeln!(out, "            DLL_PROCESS_DETACH => {{");
    let _ = writeln!(
        out,
        "                sb_info!(CHANNEL, \"DLL stub déchargée : {}\");",
        cfg.dll_name
    );
    let _ = writeln!(out, "            }}");
    let _ = writeln!(out, "            _ => {{}}");
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "        WIN_TRUE");
    let _ = writeln!(out, "    }})");
    let _ = writeln!(out, "}}");

    if cfg.forward_to.is_some() {
        return out;
    }

    let mut seen = HashSet::new();
    for (idx, sym) in cfg.exports.iter().enumerate() {
        if !seen.insert(sym.name.as_str()) {
            continue;
        }
        let fn_ident = sanitize_ident(&sym.name, idx);
        let escaped_name = sym.name.escape_default();
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "#[cfg_attr(target_os = \"windows\", unsafe(export_name = \"{escaped_name}\"))]"
        );
        let _ = writeln!(
            out,
            "pub extern \"win64\" fn {fn_ident}(a1: usize, a2: usize, a3: usize, a4: usize) -> HResult {{"
        );
        let _ = writeln!(
            out,
            "    ffi_guard!(CHANNEL, \"{escaped_name}\", sb_common::abi::E_UNEXPECTED, {{"
        );
        let _ = writeln!(
            out,
            "        sb_stub!(CHANNEL, \"{escaped_name}\", \"a1={{a1:#x}}, a2={{a2:#x}}, a3={{a3:#x}}, a4={{a4:#x}}\");"
        );
        let _ = writeln!(out, "        {ret_expr}");
        let _ = writeln!(out, "    }})");
        let _ = writeln!(out, "}}");
    }

    out
}

pub fn write_stub_crate(out_dir: &Path, cfg: &StubGenConfig) -> Result<()> {
    let src_dir = out_dir.join("src");
    fs::create_dir_all(&src_dir)
        .with_context(|| format!("impossible de créer {}", src_dir.display()))?;

    fs::write(out_dir.join("Cargo.toml"), render_cargo_toml(cfg))
        .context("échec d'écriture de Cargo.toml")?;
    fs::write(out_dir.join("build.rs"), render_build_rs(&cfg.lib_name))
        .context("échec d'écriture de build.rs")?;
    fs::write(
        out_dir.join(format!("{}.def", cfg.lib_name)),
        render_def_file(cfg),
    )
    .context("échec d'écriture du fichier .def")?;
    fs::write(src_dir.join("lib.rs"), render_lib_rs(cfg))
        .context("échec d'écriture de src/lib.rs")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_valid_rust_for_c_and_cpp_mangled_symbols() {
        let cfg = StubGenConfig {
            dll_name: "demo_xbox.dll".to_string(),
            crate_name: "sb-dll-demo-xbox".to_string(),
            lib_name: "demo_xbox".to_string(),
            channel_name: "demo_xbox".to_string(),
            default_return: DefaultReturn::ENotimpl,
            forward_to: None,
            sb_common_path: None,
            exports: vec![
                ExportedSymbol {
                    name: "DemoInit".to_string(),
                    ordinal: Some(1),
                    rva: 0x1000,
                    forwarder: None,
                },
                ExportedSymbol {
                    name: "?Create@Demo@@YAJPEAX@Z".to_string(),
                    ordinal: None,
                    rva: 0x1020,
                    forwarder: None,
                },
            ],
        };

        let lib_rs = render_lib_rs(&cfg);
        assert!(lib_rs.contains("export_name = \"DemoInit\""));
        assert!(lib_rs.contains("export_name = \"?Create@Demo@@YAJPEAX@Z\""));
        assert!(lib_rs.contains("pub extern \"win64\" fn stub_0_DemoInit"));
        assert!(lib_rs.contains("sb_common::abi::E_NOTIMPL"));

        let def = render_def_file(&cfg);
        assert!(def.contains("LIBRARY \"demo_xbox.dll\""));
        assert!(def.contains("DemoInit @1"));
        assert!(def.contains("\"?Create@Demo@@YAJPEAX@Z\""));
    }
}
