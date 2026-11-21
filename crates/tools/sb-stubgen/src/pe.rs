//! Inspection des binaires PE32+ x86-64 (imports, exports, classification des DLL).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use goblin::pe::PE;
use goblin::pe::header::{COFF_MACHINE_X86, COFF_MACHINE_X86_64};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DllCategory {
    /// DLL système Windows / OneCore / CRT gérée par Wine.
    WineSystem,
    /// DLL spécifique Xbox GDK / Scarlett nécessitant un shim ScarletBridge.
    XboxConsole,
    /// Autre DLL (middleware ou bibliothèque embarquée avec le titre).
    ThirdParty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedSymbol {
    pub name: String,
    pub ordinal: Option<u16>,
    pub rva: usize,
    pub forwarder: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedDll {
    pub name: String,
    pub category: DllCategory,
    pub functions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeSummary {
    pub path: String,
    pub is_64: bool,
    pub is_dll: bool,
    pub machine: String,
    pub entry_point: usize,
    pub image_base: usize,
    pub imports: Vec<ImportedDll>,
    pub exports: Vec<ExportedSymbol>,
}

pub fn inspect_pe(path: &Path) -> Result<PeSummary> {
    let bytes = fs::read(path)
        .with_context(|| format!("impossible de lire le binaire PE : {}", path.display()))?;
    inspect_pe_bytes(&bytes, &path.display().to_string())
}

pub fn inspect_pe_bytes(bytes: &[u8], label: &str) -> Result<PeSummary> {
    let pe = PE::parse(bytes).with_context(|| format!("fichier PE invalide : {label}"))?;

    let machine = match pe.header.coff_header.machine {
        COFF_MACHINE_X86_64 => "x86_64 (PE32+)".to_string(),
        COFF_MACHINE_X86 => "x86 (PE32)".to_string(),
        other => format!("0x{other:04x}"),
    };

    let mut grouped_imports: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for imp in &pe.imports {
        grouped_imports
            .entry(imp.dll.to_string())
            .or_default()
            .push(imp.name.to_string());
    }

    let mut imports = Vec::with_capacity(grouped_imports.len());
    for (dll, mut funcs) in grouped_imports {
        funcs.sort();
        funcs.dedup();
        let category = classify_dll(&dll);
        imports.push(ImportedDll {
            name: dll,
            category,
            functions: funcs,
        });
    }

    let mut exports = Vec::with_capacity(pe.exports.len());
    for (idx, exp) in pe.exports.iter().enumerate() {
        let name = match exp.name {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => format!("ordinal_{}", idx + 1),
        };
        let forwarder = exp.reexport.as_ref().map(|r| format!("{r:?}"));
        exports.push(ExportedSymbol {
            name,
            ordinal: None,
            rva: exp.rva,
            forwarder,
        });
    }

    Ok(PeSummary {
        path: label.to_string(),
        is_64: pe.is_64,
        is_dll: pe.is_lib,
        machine,
        entry_point: pe.entry,
        image_base: pe.image_base,
        imports,
        exports,
    })
}

/// Classifie une DLL importée selon son origine probable (Wine/Win32, Xbox/GDK ou tiers).
pub fn classify_dll(dll_name: &str) -> DllCategory {
    let lower = dll_name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".dll").unwrap_or(&lower);

    // Familles Xbox GDK / Scarlett & UWP C++/CX console prioritaires.
    const XBOX_EXACT: &[&str] = &[
        "xmem",
        "d3d12_xs",
        "d3d12_x",
        "xaudio2_9",
        "xaudio2_9redist",
        "bink2scarlett",
        "xcurl",
        "libhttpclient.gdk",
        "xboxbvhbuilder",
        "xgameruntime",
        "xg",
        "xg_x",
        "xg_xs",
        "gameinput",
        "pixeventruntime",
        "etwplus",
        "xcrypt",
        "xuser",
        "xbl",
        "xsapi",
        "xgameplatform",
        "vccorlib140_app",
        "msvcp140_app",
        "vcruntime140_app",
        "concrt140_app",
    ];
    if XBOX_EXACT.contains(&stem)
        || (stem.starts_with("amd_fidelityfx_") && stem.ends_with("_xbox"))
        || stem.starts_with("xgame")
        || stem.starts_with("xinput")
    {
        return DllCategory::XboxConsole;
    }

    // Familles Windows / OneCore / MSVC CRT gérées par Wine.
    const WINE_EXACT: &[&str] = &[
        "ntdll",
        "kernel32",
        "kernelbase",
        "user32",
        "gdi32",
        "advapi32",
        "shell32",
        "shlwapi",
        "ole32",
        "oleaut32",
        "combase",
        "rpcrt4",
        "ws2_32",
        "winhttp",
        "wininet",
        "winmm",
        "bcrypt",
        "bcryptprimitives",
        "ncrypt",
        "crypt32",
        "wintrust",
        "sechost",
        "dbghelp",
        "version",
        "imm32",
        "iphlpapi",
        "setupapi",
        "psapi",
        "powrprof",
        "userenv",
        "propsys",
        "netapi32",
        "normaliz",
        "mswsock",
        "dnsapi",
        "avrt",
        "mmdevapi",
        "hid",
        "uxtheme",
        "dwmapi",
        "dxgi",
        "dxcore",
        "d3d12",
        "d3d11",
        "ucrtbase",
        "msvcrt",
        "concrt140",
        "vcomp140",
    ];
    if WINE_EXACT.contains(&stem)
        || stem.starts_with("api-ms-win-")
        || stem.starts_with("ext-ms-win-")
        || stem.starts_with("vcruntime140")
        || stem.starts_with("msvcp140")
        || stem.starts_with("concrt140")
    {
        return DllCategory::WineSystem;
    }

    DllCategory::ThirdParty
}

/// Convertit une entrée d'import `goblin` (nom ou `"ORDINAL <N>"`) en `ExportedSymbol`.
pub fn import_name_to_export(name: &str) -> ExportedSymbol {
    if let Some(rest) = name.strip_prefix("ORDINAL ")
        && let Ok(ord) = rest.trim().parse::<u16>()
    {
        return ExportedSymbol {
            name: format!("ordinal_{ord}"),
            ordinal: Some(ord),
            rva: 0,
            forwarder: None,
        };
    }

    ExportedSymbol {
        name: name.to_string(),
        ordinal: None,
        rva: 0,
        forwarder: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_dlls_correctly() {
        assert_eq!(classify_dll("xmem.dll"), DllCategory::XboxConsole);
        assert_eq!(classify_dll("D3D12_XS.DLL"), DllCategory::XboxConsole);
        assert_eq!(
            classify_dll("vccorlib140_app.DLL"),
            DllCategory::XboxConsole
        );
        assert_eq!(
            classify_dll("amd_fidelityfx_loader_xbox.dll"),
            DllCategory::XboxConsole
        );
        assert_eq!(
            classify_dll("libHttpClient.GDK.dll"),
            DllCategory::XboxConsole
        );
        assert_eq!(classify_dll("KERNEL32.dll"), DllCategory::WineSystem);
        assert_eq!(classify_dll("msvcp140_1.dll"), DllCategory::WineSystem);
        assert_eq!(
            classify_dll("api-ms-win-core-synch-l1-2-0.dll"),
            DllCategory::WineSystem
        );
        assert_eq!(classify_dll("custom_engine.dll"), DllCategory::ThirdParty);
    }

    #[test]
    fn parses_ordinal_import_entries() {
        let ord = import_name_to_export("ORDINAL 1");
        assert_eq!(ord.name, "ordinal_1");
        assert_eq!(ord.ordinal, Some(1));

        let named = import_name_to_export("D3D11CreateDevice");
        assert_eq!(named.name, "D3D11CreateDevice");
        assert_eq!(named.ordinal, None);
    }
}
