//! `scarletbridge requirements <exe|dossier>` : inventaire **complet** de ce qu'un binaire PE32+
//! demande (pas seulement les blocages) : format et exigences du binaire, adresse de chargement,
//! DLL et imports (normaux et retardés) par niveau de prise en charge, classes du runtime GDK et
//! interfaces référencées, shaders embarqués (DXBC / DXIL), instructions CPU utilisées, fichiers
//! cités, fonctionnalités requises côté bridge, besoins machine et contenu du bundle.
//!
//! Analyse purement statique : le binaire n'est jamais exécuté.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use goblin::pe::PE;
use iced_x86::{Decoder, DecoderOptions};
use sb_hle::DefaultHleResolver;
use sb_hle::com::Guid;
use sb_loader::{HleResolver, ImportSymbol};
use sb_stubgen::pe::{import_name_to_export, inspect_pe_bytes};
use sb_vfs::AppManifest;
use serde::Serialize;

/// Options de la commande.
pub struct Options {
    pub json: bool,
    pub no_cpu: bool,
    pub dll_dir: PathBuf,
}

// ---------------------------------------------------------------------------------------------
// Structures du rapport
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct SectionInfo {
    pub name: String,
    pub virtual_size: u32,
    pub raw_size: u32,
    pub flags: String,
}

#[derive(Debug, Serialize)]
pub struct FileInfo {
    pub path: String,
    pub size_bytes: u64,
    pub machine: String,
    pub kind: String,
    pub subsystem: String,
    pub image_base: u64,
    pub size_of_image: u64,
    pub entry_rva: u32,
    pub os_version: String,
    pub subsystem_version: String,
    pub linker: String,
    pub timestamp: u32,
    pub stack_reserve: u64,
    pub stack_commit: u64,
    pub heap_reserve: u64,
    pub heap_commit: u64,
    pub dll_characteristics: Vec<String>,
    pub has_relocations: bool,
    pub tls_callbacks: usize,
    pub exports: usize,
    pub pdb: Option<String>,
    pub sections: Vec<SectionInfo>,
}

#[derive(Debug, Serialize)]
pub struct LoadInfo {
    pub preferred_base: u64,
    pub relocatable: bool,
    pub preferred_base_free: Option<bool>,
    pub verdict: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DllNeed {
    pub name: String,
    pub category: String,
    /// `natif`, `partiel`, `invité` (DLL du bundle), `stubs` (module HLE sans symbole) ou `absente`.
    pub tier: String,
    pub covered: usize,
    pub total: usize,
    pub missing: Vec<String>,
    pub in_bundle: bool,
}

#[derive(Debug, Serialize)]
pub struct ClassRef {
    pub name: String,
    pub clsid: String,
    pub occurrences: u64,
    pub served: bool,
}

#[derive(Debug, Serialize)]
pub struct IidRef {
    pub name: String,
    pub iid: String,
    pub occurrences: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct ShaderInfo {
    pub containers: u64,
    pub dxbc: u64,
    pub dxil: u64,
    pub by_profile: BTreeMap<String, u64>,
    pub translatable: u64,
    pub unsupported: u64,
    pub unsupported_profiles: BTreeMap<String, u64>,
}

#[derive(Debug, Serialize)]
pub struct CpuFeature {
    pub name: String,
    pub instructions: u64,
    pub host_supports: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct FileRef {
    pub name: String,
    pub present_in_bundle: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct Feature {
    pub name: String,
    /// `ok`, `partiel`, `limité` ou `absent`.
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct ExtStat {
    pub ext: String,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct BundleInfo {
    pub directory: String,
    pub manifest: Option<String>,
    pub title_id: Option<String>,
    pub executables: Vec<String>,
    pub persistent_storage_mb: Option<u64>,
    pub total_files: u64,
    pub total_bytes: u64,
    pub by_extension: Vec<ExtStat>,
    pub dlls_in_bundle: Vec<String>,
    pub encrypted_hints: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub imports_total: usize,
    pub imports_native: usize,
    pub native_percent: usize,
    pub likely_blockers: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub file: FileInfo,
    pub load: LoadInfo,
    pub dlls: Vec<DllNeed>,
    pub delay_imports: Vec<DllNeed>,
    pub gdk_classes: Vec<ClassRef>,
    pub interfaces: Vec<IidRef>,
    pub shaders: Option<ShaderInfo>,
    pub cpu: Option<Vec<CpuFeature>>,
    pub files_referenced: Vec<FileRef>,
    pub features: Vec<Feature>,
    pub machine_needs: Vec<String>,
    pub bundle: Option<BundleInfo>,
    pub summary: Summary,
}

// ---------------------------------------------------------------------------------------------
// Utilitaires
// ---------------------------------------------------------------------------------------------

fn kib(n: u64) -> String {
    if n >= 1 << 30 {
        format!("{:.2} Gio", n as f64 / (1u64 << 30) as f64)
    } else if n >= 1 << 20 {
        format!("{:.1} Mio", n as f64 / (1u64 << 20) as f64)
    } else {
        format!("{:.1} Kio", n as f64 / 1024.0)
    }
}

fn subsystem_name(v: u16) -> String {
    match v {
        1 => "natif".into(),
        2 => "Windows GUI".into(),
        3 => "Windows console".into(),
        9 => "Windows CE".into(),
        10 => "application EFI".into(),
        14 => "Xbox".into(),
        16 => "Windows boot application".into(),
        other => format!("inconnu ({other})"),
    }
}

fn dll_characteristics(v: u16) -> Vec<String> {
    const BITS: [(u16, &str); 11] = [
        (0x0020, "HIGH_ENTROPY_VA"),
        (0x0040, "DYNAMIC_BASE (ASLR)"),
        (0x0080, "FORCE_INTEGRITY"),
        (0x0100, "NX_COMPAT"),
        (0x0200, "NO_ISOLATION"),
        (0x0400, "NO_SEH"),
        (0x0800, "NO_BIND"),
        (0x1000, "APPCONTAINER"),
        (0x2000, "WDM_DRIVER"),
        (0x4000, "GUARD_CF"),
        (0x8000, "TERMINAL_SERVER_AWARE"),
    ];
    BITS.iter()
        .filter(|(b, _)| v & b != 0)
        .map(|(_, n)| (*n).to_string())
        .collect()
}

fn section_flags(c: u32) -> String {
    let mut s = String::new();
    s.push(if c & 0x4000_0000 != 0 { 'R' } else { '-' });
    s.push(if c & 0x8000_0000 != 0 { 'W' } else { '-' });
    s.push(if c & 0x2000_0000 != 0 { 'X' } else { '-' });
    if c & 0x20 != 0 {
        s.push_str(" code");
    }
    s
}

fn guid_bytes(g: &Guid) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&g.data1.to_le_bytes());
    out[4..6].copy_from_slice(&g.data2.to_le_bytes());
    out[6..8].copy_from_slice(&g.data3.to_le_bytes());
    out[8..16].copy_from_slice(&g.data4);
    out
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

// ---------------------------------------------------------------------------------------------
// Analyse du binaire
// ---------------------------------------------------------------------------------------------

fn file_info(pe: &PE<'_>, path: &Path, size: u64) -> FileInfo {
    let h = &pe.header;
    let (opt_std, opt_win, tls_cb, relocs) = match h.optional_header.as_ref() {
        Some(o) => (
            Some(o.standard_fields),
            Some(o.windows_fields),
            pe.tls_data.as_ref().map_or(0, |t| t.callbacks.len()),
            o.data_directories.get_base_relocation_table().is_some(),
        ),
        None => (None, None, 0, false),
    };
    let sections = pe
        .sections
        .iter()
        .map(|s| SectionInfo {
            name: s.name().unwrap_or("?").to_string(),
            virtual_size: s.virtual_size,
            raw_size: s.size_of_raw_data,
            flags: section_flags(s.characteristics),
        })
        .collect();
    FileInfo {
        path: path.display().to_string(),
        size_bytes: size,
        machine: if pe.is_64 {
            "x86-64 (PE32+)".into()
        } else {
            "x86 (PE32)".into()
        },
        kind: if pe.is_lib {
            "DLL".into()
        } else {
            "exécutable".into()
        },
        subsystem: opt_win.map_or("?".into(), |w| subsystem_name(w.subsystem)),
        image_base: opt_win.map_or(0, |w| w.image_base),
        size_of_image: opt_win.map_or(0, |w| u64::from(w.size_of_image)),
        entry_rva: opt_std.map_or(0, |s| u32::try_from(s.address_of_entry_point).unwrap_or(0)),
        os_version: opt_win.map_or("?".into(), |w| {
            format!(
                "{}.{}",
                w.major_operating_system_version, w.minor_operating_system_version
            )
        }),
        subsystem_version: opt_win.map_or("?".into(), |w| {
            format!(
                "{}.{}",
                w.major_subsystem_version, w.minor_subsystem_version
            )
        }),
        linker: opt_std.map_or("?".into(), |s| {
            format!("{}.{}", s.major_linker_version, s.minor_linker_version)
        }),
        timestamp: h.coff_header.time_date_stamp,
        stack_reserve: opt_win.map_or(0, |w| w.size_of_stack_reserve),
        stack_commit: opt_win.map_or(0, |w| w.size_of_stack_commit),
        heap_reserve: opt_win.map_or(0, |w| w.size_of_heap_reserve),
        heap_commit: opt_win.map_or(0, |w| w.size_of_heap_commit),
        dll_characteristics: opt_win.map_or(vec![], |w| dll_characteristics(w.dll_characteristics)),
        has_relocations: relocs,
        tls_callbacks: tls_cb,
        exports: pe.exports.len(),
        pdb: pe
            .debug_data
            .as_ref()
            .and_then(|d| d.codeview_pdb70_debug_info.as_ref())
            .and_then(|c| std::str::from_utf8(c.filename).ok())
            .map(|s| s.trim_end_matches('\0').to_string()),
        sections,
    }
}

fn load_info(f: &FileInfo) -> LoadInfo {
    let free = if f.image_base != 0 && f.size_of_image != 0 {
        // SAFETY: sonde `PROT_NONE` sans remplacement, libérée aussitôt.
        let p = unsafe {
            libc::mmap(
                f.image_base as *mut libc::c_void,
                f.size_of_image as usize,
                libc::PROT_NONE,
                libc::MAP_PRIVATE
                    | libc::MAP_ANONYMOUS
                    | libc::MAP_FIXED_NOREPLACE
                    | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            Some(false)
        } else {
            // SAFETY: libération de la sonde créée ci-dessus.
            unsafe { libc::munmap(p, f.size_of_image as usize) };
            Some(true)
        }
    } else {
        None
    };
    let verdict = match (free, f.has_relocations) {
        (Some(true), _) => "adresse préférée libre : chargement direct".to_string(),
        (Some(false), true) => "adresse occupée mais image relogeable : chargement avec relocation".to_string(),
        (Some(false), false) => {
            "adresse occupée et AUCUNE relocation : le chargement échouera tant que la plage est prise".to_string()
        }
        (None, _) => "indéterminé".to_string(),
    };
    LoadInfo {
        preferred_base: f.image_base,
        relocatable: f.has_relocations,
        preferred_base_free: free,
        verdict,
    }
}

/// Évalue un module importé : couverture HLE, DLL du bundle, niveau de prise en charge.
fn evaluate_dll(
    hle: &DefaultHleResolver,
    name: &str,
    category: &str,
    functions: &[String],
    bundle_dlls: &BTreeSet<String>,
) -> DllNeed {
    let mut covered = 0usize;
    let mut missing = Vec::new();
    for fname in functions {
        let exp = import_name_to_export(fname);
        let sym = match exp.ordinal {
            Some(o) => ImportSymbol::Ordinal(o),
            None => ImportSymbol::Name(exp.name.clone()),
        };
        if hle.resolve(name, &sym).is_some() {
            covered += 1;
        } else {
            missing.push(fname.clone());
        }
    }
    let lower = name.to_ascii_lowercase();
    let in_bundle = bundle_dlls.contains(&lower);
    let total = functions.len();
    let tier = if total > 0 && covered == total {
        "natif"
    } else if covered > 0 {
        "partiel"
    } else if in_bundle {
        "invité"
    } else if hle.is_hle_module(name) {
        "stubs"
    } else {
        "absente"
    };
    DllNeed {
        name: name.to_string(),
        category: category.to_string(),
        tier: tier.to_string(),
        covered,
        total,
        missing,
        in_bundle,
    }
}

/// Imports retardés (`IMAGE_DELAYLOAD_DESCRIPTOR`), absents de `goblin`.
fn delay_imports(pe: &PE<'_>, bytes: &[u8]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let Some(dir) = pe
        .header
        .optional_header
        .as_ref()
        .and_then(|o| o.data_directories.get_delay_import_descriptor().copied())
    else {
        return out;
    };
    let opts = goblin::pe::options::ParseOptions::default();
    let rva_to_off = |rva: u32| -> Option<usize> {
        goblin::pe::utils::find_offset(rva as usize, &pe.sections, 0x200, &opts)
    };
    let cstr = |off: usize| -> String {
        bytes
            .get(off..)
            .map(|s| {
                let end = s.iter().position(|&b| b == 0).unwrap_or(s.len()).min(256);
                String::from_utf8_lossy(&s[..end]).into_owned()
            })
            .unwrap_or_default()
    };
    let Some(mut off) = rva_to_off(dir.virtual_address) else {
        return out;
    };
    for _ in 0..512 {
        let (Some(attrs), Some(name_rva), Some(int_rva)) = (
            u32_at(bytes, off),
            u32_at(bytes, off + 4),
            u32_at(bytes, off + 16),
        ) else {
            break;
        };
        if name_rva == 0 {
            break;
        }
        off += 32;
        // Attribut bit 0 : champs en RVA (VC7+). Sinon : adresses virtuelles, ignorées ici.
        if attrs & 1 == 0 {
            continue;
        }
        let dll = rva_to_off(name_rva).map(&cstr).unwrap_or_default();
        let mut funcs = Vec::new();
        if let Some(mut t) = rva_to_off(int_rva) {
            for _ in 0..8192 {
                let Some(s) = bytes.get(t..t + 8) else { break };
                let v = u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]);
                if v == 0 {
                    break;
                }
                t += 8;
                if v & (1 << 63) != 0 {
                    funcs.push(format!("ORDINAL {}", v & 0xFFFF));
                } else if let Some(n) = rva_to_off(v as u32 + 2) {
                    funcs.push(cstr(n));
                }
            }
        }
        out.entry(dll).or_default().extend(funcs);
    }
    out
}

/// Cherche dans le fichier les GUID de classes GDK et d'interfaces D3D12/DXGI connus.
fn scan_guids(bytes: &[u8]) -> (Vec<ClassRef>, Vec<IidRef>) {
    let classes = sb_hle::gdk_classes::KNOWN_GDK_CLASSES;
    let iids = sb_hle::com::known_iid_table();
    let mut targets: Vec<[u8; 16]> = classes.iter().map(|(_, g)| guid_bytes(g)).collect();
    targets.extend(iids.iter().map(|(g, _)| guid_bytes(g)));
    let mut by_prefix: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, t) in targets.iter().enumerate() {
        let k = u64::from_le_bytes([t[0], t[1], t[2], t[3], t[4], t[5], t[6], t[7]]);
        by_prefix.entry(k).or_default().push(i);
    }
    let mut counts = vec![0u64; targets.len()];
    if bytes.len() >= 16 {
        for i in 0..=bytes.len() - 16 {
            let w = &bytes[i..i + 8];
            let k = u64::from_le_bytes([w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7]]);
            if let Some(cands) = by_prefix.get(&k) {
                for &c in cands {
                    if bytes[i..i + 16] == targets[c] {
                        counts[c] += 1;
                    }
                }
            }
        }
    }
    let class_refs = classes
        .iter()
        .enumerate()
        .filter(|(i, _)| counts[*i] > 0)
        .map(|(i, (name, g))| ClassRef {
            name: (*name).to_string(),
            clsid: g.to_string(),
            occurrences: counts[i],
            served: sb_hle::gdk_classes::is_class_served(g),
        })
        .collect();
    let off = classes.len();
    let iid_refs = iids
        .iter()
        .enumerate()
        .filter(|(i, (_, n))| counts[off + i] > 0 && *n != "IUnknown")
        .map(|(i, (g, n))| IidRef {
            name: (*n).to_string(),
            iid: g.to_string(),
            occurrences: counts[off + i],
        })
        .collect();
    (class_refs, iid_refs)
}

fn program_type_name(t: u32) -> &'static str {
    match t {
        0 => "ps",
        1 => "vs",
        2 => "gs",
        3 => "hs",
        4 => "ds",
        5 => "cs",
        6 => "lib",
        7 => "raygen",
        8 => "intersection",
        9 => "anyhit",
        10 => "closesthit",
        11 => "miss",
        12 => "callable",
        13 => "ms",
        14 => "as",
        _ => "?",
    }
}

/// Conteneurs DXBC embarqués : DXBC SM4/SM5 (`SHDR`/`SHEX`) ou DXIL SM6 (`DXIL`).
fn scan_shaders(bytes: &[u8]) -> ShaderInfo {
    let mut info = ShaderInfo::default();
    let mut i = 0usize;
    while i + 36 <= bytes.len() && info.containers < 500_000 {
        if bytes[i] != b'D' || &bytes[i + 1..i + 4] != b"XBC" {
            i += 1;
            continue;
        }
        let total = u32_at(bytes, i + 24).unwrap_or(0) as usize;
        let chunks = u32_at(bytes, i + 28).unwrap_or(0) as usize;
        if !(36..=(64 << 20)).contains(&total)
            || i + total > bytes.len()
            || chunks == 0
            || chunks > 32
        {
            i += 4;
            continue;
        }
        let mut profile: Option<(bool, u32, u32, u32)> = None; // (dxil, type, major, minor)
        for c in 0..chunks {
            let Some(off) = u32_at(bytes, i + 32 + 4 * c).map(|v| v as usize) else {
                break;
            };
            if off + 12 > total {
                continue;
            }
            let tag = &bytes[i + off..i + off + 4];
            let data = i + off + 8;
            match tag {
                b"SHDR" | b"SHEX" => {
                    if let Some(v) = u32_at(bytes, data) {
                        profile = Some((false, v >> 16, (v >> 4) & 0xF, v & 0xF));
                    }
                }
                b"DXIL" => {
                    if let Some(v) = u32_at(bytes, data) {
                        profile = Some((true, v >> 16, (v >> 4) & 0xF, v & 0xF));
                    }
                }
                _ => {}
            }
        }
        if let Some((dxil, t, major, minor)) = profile {
            info.containers += 1;
            let key = format!(
                "{} {}_{major}_{minor}",
                if dxil { "DXIL" } else { "DXBC" },
                program_type_name(t)
            );
            *info.by_profile.entry(key.clone()).or_default() += 1;
            if dxil {
                info.dxil += 1;
                info.unsupported += 1;
                *info.unsupported_profiles.entry(key).or_default() += 1;
            } else {
                info.dxbc += 1;
                // Traducteur : VS/PS/CS en SM4.0 à SM5.0.
                if matches!(t, 0 | 1 | 5) && (major, minor) <= (5, 0) {
                    info.translatable += 1;
                } else {
                    info.unsupported += 1;
                    *info.unsupported_profiles.entry(key).or_default() += 1;
                }
            }
        }
        i += 4;
    }
    info
}

/// Noms de fichiers cités dans le binaire (chaînes ASCII se terminant par une extension connue).
fn scan_file_references(bytes: &[u8], import_dlls: &BTreeSet<String>) -> Vec<String> {
    const EXTS: [&str; 24] = [
        ".rpf", ".bin", ".dat", ".pak", ".json", ".xml", ".cfg", ".ini", ".bik", ".bk2", ".bnk",
        ".wem", ".awc", ".xvd", ".pri", ".txt", ".lua", ".cso", ".dds", ".ogg", ".mp4", ".dll",
        ".config", ".zip",
    ];
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    let mut start: Option<usize> = None;
    for (idx, &b) in bytes
        .iter()
        .enumerate()
        .chain(std::iter::once((bytes.len(), &0u8)))
    {
        let printable = (0x20..0x7F).contains(&b);
        match (printable, start) {
            (true, None) => start = Some(idx),
            (false, Some(s)) => {
                start = None;
                let len = idx - s;
                if !(5..=260).contains(&len) {
                    continue;
                }
                let Ok(text) = std::str::from_utf8(&bytes[s..idx]) else {
                    continue;
                };
                let lower = text.to_ascii_lowercase();
                if !EXTS.iter().any(|e| lower.ends_with(e)) || lower.contains(' ') {
                    continue;
                }
                let name = lower
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(&lower)
                    .trim_start_matches([':', '.'])
                    .to_string();
                if name.len() < 5
                    || name.starts_with("api-ms-")
                    || name.starts_with("ext-ms-")
                    || import_dlls.contains(&name)
                {
                    continue;
                }
                found.entry(name).or_insert_with(|| text.to_string());
                if found.len() >= 400 {
                    return found.into_keys().collect();
                }
            }
            _ => {}
        }
    }
    found.into_keys().collect()
}

fn host_supports(feature: &str) -> Option<bool> {
    use std::arch::is_x86_feature_detected as has;
    Some(match feature {
        "SSE3" => has!("sse3"),
        "SSSE3" => has!("ssse3"),
        "SSE4_1" => has!("sse4.1"),
        "SSE4_2" => has!("sse4.2"),
        "POPCNT" => has!("popcnt"),
        "AVX" => has!("avx"),
        "AVX2" => has!("avx2"),
        "FMA" => has!("fma"),
        "BMI1" => has!("bmi1"),
        "BMI2" => has!("bmi2"),
        "LZCNT" => has!("lzcnt"),
        "F16C" => has!("f16c"),
        "AES" => has!("aes"),
        "PCLMULQDQ" => has!("pclmulqdq"),
        "ADX" => has!("adx"),
        "AVX512F" => has!("avx512f"),
        "AVX512VL" => has!("avx512vl"),
        "AVX512BW" => has!("avx512bw"),
        "AVX512DQ" => has!("avx512dq"),
        "AVX512CD" => has!("avx512cd"),
        _ => return None,
    })
}

/// Instructions étendues réellement présentes dans les sections de code (balayage linéaire :
/// quelques faux positifs sont possibles là où des données sont décodées comme du code).
fn scan_cpu(pe: &PE<'_>, bytes: &[u8], image_base: u64) -> Vec<CpuFeature> {
    const NOTABLE: [&str; 20] = [
        "SSE3",
        "SSSE3",
        "SSE4_1",
        "SSE4_2",
        "POPCNT",
        "AVX",
        "AVX2",
        "FMA",
        "BMI1",
        "BMI2",
        "LZCNT",
        "F16C",
        "AES",
        "PCLMULQDQ",
        "ADX",
        "MOVBE",
        "RDRAND",
        "RDSEED",
        "SHA",
        "XSAVE",
    ];
    let mut counts: HashMap<String, u64> = HashMap::new();
    for s in &pe.sections {
        if s.characteristics & 0x2000_0000 == 0 {
            continue;
        }
        let off = s.pointer_to_raw_data as usize;
        let len = (s.size_of_raw_data as usize).min(512 << 20);
        let Some(code) = bytes.get(off..off + len.min(bytes.len().saturating_sub(off))) else {
            continue;
        };
        let ip = image_base + u64::from(s.virtual_address);
        let mut decoder = Decoder::with_ip(64, code, ip, DecoderOptions::NONE);
        let mut instr = iced_x86::Instruction::default();
        let mut per_feature = vec![0u64; 512];
        while decoder.can_decode() {
            decoder.decode_out(&mut instr);
            if instr.is_invalid() {
                continue;
            }
            for f in instr.cpuid_features() {
                per_feature[*f as usize] += 1;
            }
        }
        // Reconstruit les noms une seule fois par section.
        for (f, &n) in per_feature.iter().enumerate() {
            if n == 0 {
                continue;
            }
            if let Some(feat) = iced_x86::CpuidFeature::values().find(|v| *v as usize == f) {
                *counts.entry(format!("{feat:?}")).or_default() += n;
            }
        }
    }
    let mut out: Vec<CpuFeature> = counts
        .into_iter()
        .filter(|(n, c)| *c >= 4 && (NOTABLE.contains(&n.as_str()) || n.starts_with("AVX512")))
        .map(|(name, instructions)| CpuFeature {
            host_supports: host_supports(&name),
            name,
            instructions,
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

// ---------------------------------------------------------------------------------------------
// Fonctionnalités requises, besoins machine
// ---------------------------------------------------------------------------------------------

fn feat(name: &str, status: &str, detail: &str) -> Feature {
    Feature {
        name: name.to_string(),
        status: status.to_string(),
        detail: detail.to_string(),
    }
}

fn derive_features(
    imports: &BTreeMap<String, BTreeSet<String>>,
    classes: &[ClassRef],
    shaders: Option<&ShaderInfo>,
) -> Vec<Feature> {
    let has_dll = |stem: &str| {
        imports.keys().any(|d| {
            let l = d.to_ascii_lowercase();
            l.strip_suffix(".dll").unwrap_or(&l) == stem
        })
    };
    let has_dll_prefix = |p: &str| {
        imports
            .keys()
            .any(|d| d.to_ascii_lowercase().starts_with(p))
    };
    let has_sym = |frag: &str| imports.values().any(|s| s.iter().any(|n| n.contains(frag)));
    let mut v = Vec::new();

    if has_dll("d3d12_xs") || has_dll("d3d12_x") || has_dll("d3d12") {
        v.push(feat(
            "Direct3D 12 (Xbox)",
            "partiel",
            "traduit vers Vulkan ; cœur du pipeline en place, méthodes avancées et propres à la console incomplètes (les slots non implémentés sont journalisés)",
        ));
    }
    if has_dll("d3d11") {
        v.push(feat(
            "Direct3D 11",
            "partiel",
            "traduit vers Vulkan (création de périphérique, textures, vues)",
        ));
    }
    if has_dll("dxgi") {
        v.push(feat(
            "DXGI",
            "partiel",
            "fabrique, swapchain et adaptateurs de base",
        ));
    }
    if let Some(sh) = shaders {
        if sh.dxbc > 0 {
            v.push(feat(
                "Shaders DXBC (SM4/SM5)",
                if sh.unsupported == 0 { "ok" } else { "partiel" },
                &format!(
                    "{} conteneur(s) DXBC, {} traduisible(s) en SPIR-V (VS/PS/CS jusqu'à SM5.0)",
                    sh.dxbc, sh.translatable
                ),
            ));
        }
        if sh.dxil > 0 {
            v.push(feat(
                "Shaders DXIL (SM6.x)",
                "absent",
                &format!(
                    "{} conteneur(s) DXIL : seul l'analyseur existe, aucune génération SPIR-V",
                    sh.dxil
                ),
            ));
        }
    }
    if has_dll("xboxbvhbuilder") || has_sym("Raytracing") {
        v.push(feat("Lancer de rayons (BVH)", "limité", "XboxBvhBuilder accepté, tailles nulles et constructions sans effet (pas de VK_KHR_acceleration_structure)"));
    }
    if has_dll("xcrdapi") || has_dll("dstorage") || has_dll("dstorage_xs") {
        v.push(feat(
            "DirectStorage",
            "partiel",
            "files et requêtes via le VFS sandboxé ; décompression GDeflate absente",
        ));
    }
    if has_dll_prefix("xaudio2") {
        v.push(feat("Audio XAudio2", "partiel", "rendu via SDL3/PipeWire"));
    }
    if has_dll("xapu") || has_dll("chad") || has_dll("acphal") {
        v.push(feat(
            "Coprocesseur audio Xbox (APU/DSP)",
            "limité",
            "émulation logicielle sans traitement du signal",
        ));
    }
    if has_dll("mfplat") || has_dll("mfreadwrite") {
        v.push(feat(
            "Media Foundation",
            "limité",
            "attributs et types de média ; aucun décodeur de flux",
        ));
    }
    if has_dll_prefix("bink2") {
        v.push(feat(
            "Vidéo Bink",
            "limité",
            "ouverture et cadence réelles, image noire (codec propriétaire)",
        ));
    }
    if has_dll("ws2_32")
        || has_dll("winhttp")
        || has_dll("xcurl")
        || has_dll_prefix("libhttpclient")
    {
        v.push(feat(
            "Réseau",
            "limité",
            "mode hors ligne strict par défaut ; loopback seulement ; pas de TLS ni de WebSockets",
        ));
    }
    if has_dll("xgameruntime") || !classes.is_empty() {
        let served = classes.iter().filter(|c| c.served).count();
        v.push(feat(
            "Runtime GDK (xgameruntime)",
            if served == classes.len() {
                "ok"
            } else {
                "partiel"
            },
            &format!(
                "{} classe(s) référencée(s), {served} fournie(s) par le bridge",
                classes.len()
            ),
        ));
    }
    if has_dll("xmem") {
        v.push(feat(
            "Mémoire Xbox (xmem)",
            "partiel",
            "pages physiques 64 Kio émulées (memfd), attributs et protections graphiques acceptés",
        ));
    }
    if has_sym("RaiseException")
        || has_sym("_CxxThrowException")
        || has_sym("__CxxFrameHandler")
        || has_sym("RtlUnwind")
    {
        v.push(feat("Exceptions SEH / C++", "ok", "propagation Windows x64 et MSVC implémentée (décodeur FH4 reconstruit, non vérifié sur compilateur réel)"));
    }
    if has_dll("gameinput") || has_sym("Gamepad") {
        v.push(feat(
            "Manette / entrées",
            "ok",
            "GameInput et Windows.Gaming.Input via SDL3",
        ));
    }
    if has_dll_prefix("ext-ms-win-rtcore-ntuser") || has_dll("user32") {
        v.push(feat(
            "Fenêtres (user32)",
            "limité",
            "fenêtres et messages logiques, sans lien direct avec l'affichage",
        ));
    }
    if has_dll("advapi32") && has_sym("Reg") {
        v.push(feat(
            "Registre Windows",
            "limité",
            "aucune clé : les ouvertures répondent « introuvable »",
        ));
    }
    if has_dll("dbghelp") {
        v.push(feat(
            "Symboles de débogage (dbghelp)",
            "limité",
            "initialisation seulement ; pas de résolution de symboles",
        ));
    }
    if has_dll_prefix("vccorlib") || has_dll("combase") || has_sym("RoGetActivationFactory") {
        v.push(feat(
            "WinRT / C++/CX",
            "partiel",
            "classes CoreApplication, CoreWindow, ApplicationData, Gamepad",
        ));
    }
    v
}

fn machine_needs(
    f: &FileInfo,
    features: &[Feature],
    shaders: Option<&ShaderInfo>,
    cpu: Option<&[CpuFeature]>,
    bundle: Option<&BundleInfo>,
) -> Vec<String> {
    let mut v = Vec::new();
    v.push(format!(
        "Mémoire : image virtuelle de {} (non entièrement résidente) ; pile réservée {} ; tas initial {}",
        kib(f.size_of_image),
        kib(f.stack_reserve),
        kib(f.heap_reserve)
    ));
    v.push(format!(
        "Adresse de chargement libre : {:#x} (ou image relogeable)",
        f.image_base
    ));
    if features
        .iter()
        .any(|x| x.name.starts_with("Direct3D") || x.name.starts_with("Shaders"))
    {
        v.push("GPU Vulkan ≥ 1.3 (textures BC1-BC7, VK_EXT_external_memory_host pour la mémoire unifiée)".into());
    }
    if shaders.is_some_and(|s| s.dxil > 0) {
        v.push("Fonctionnalité à écrire : génération SPIR-V depuis DXIL (SM6.x)".into());
    }
    if features.iter().any(|x| {
        x.name.contains("Manette") || x.name.contains("Fenêtres") || x.name.contains("WinRT")
    }) {
        v.push("SDL3 (fenêtre Wayland/X11 et manettes)".into());
    }
    if features.iter().any(|x| x.name.contains("Audio")) {
        v.push("PipeWire ou PulseAudio (via SDL3) pour le son".into());
    }
    if let Some(cpu) = cpu {
        let missing: Vec<&str> = cpu
            .iter()
            .filter(|c| c.host_supports == Some(false))
            .map(|c| c.name.as_str())
            .collect();
        if cpu.is_empty() {
            v.push("CPU : aucune extension notable détectée".into());
        } else {
            v.push(format!(
                "CPU : extensions utilisées {} ; manquantes sur cette machine : {}",
                cpu.iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                if missing.is_empty() {
                    "aucune".into()
                } else {
                    missing.join(", ")
                }
            ));
        }
    }
    if let Some(b) = bundle {
        v.push(format!(
            "Disque : le bundle occupe {} ; prévoir de l'espace pour les sauvegardes{} et le cache de pipelines",
            kib(b.total_bytes),
            b.persistent_storage_mb.map_or(String::new(), |mb| format!(" (quota du manifeste : {mb} Mio)"))
        ));
    }
    v
}

// ---------------------------------------------------------------------------------------------
// Bundle
// ---------------------------------------------------------------------------------------------

fn scan_bundle(dir: &Path) -> BundleInfo {
    let manifest = AppManifest::discover_in_dir(dir).ok();
    let mut total_files = 0u64;
    let mut total_bytes = 0u64;
    let mut by_ext: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut dlls = BTreeSet::new();
    let mut hints = BTreeSet::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            visited += 1;
            if visited > 200_000 {
                break;
            }
            let p = e.path();
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                if depth < 4 {
                    stack.push((p, depth + 1));
                }
                continue;
            }
            total_files += 1;
            total_bytes += md.len();
            let ext = p
                .extension()
                .and_then(|x| x.to_str())
                .map_or("(aucune)".to_string(), str::to_ascii_lowercase);
            let slot = by_ext.entry(ext.clone()).or_default();
            slot.0 += 1;
            slot.1 += md.len();
            if ext == "dll"
                && depth == 0
                && let Some(n) = p.file_name().and_then(|n| n.to_str())
            {
                dlls.insert(n.to_string());
            }
            if matches!(ext.as_str(), "xvc" | "xvd" | "msixvc" | "xbi" | "xvi") {
                hints.insert(format!(
                    "{} ({}) : paquet Xbox chiffré probable — non pris en charge (aucun déchiffrement)",
                    p.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                    kib(md.len())
                ));
            }
        }
    }
    let mut exts: Vec<ExtStat> = by_ext
        .into_iter()
        .map(|(ext, (files, bytes))| ExtStat { ext, files, bytes })
        .collect();
    exts.sort_by_key(|e| std::cmp::Reverse(e.bytes));
    exts.truncate(12);
    BundleInfo {
        directory: dir.display().to_string(),
        manifest: manifest
            .as_ref()
            .and_then(|m| m.source_file.as_ref())
            .map(|p| p.display().to_string()),
        title_id: manifest.as_ref().and_then(|m| m.title_id.clone()),
        executables: manifest
            .as_ref()
            .map(|m| m.executables.iter().map(|e| e.name.clone()).collect())
            .unwrap_or_default(),
        persistent_storage_mb: manifest
            .as_ref()
            .and_then(|m| m.persistent_local_storage_mb),
        total_files,
        total_bytes,
        by_extension: exts,
        dlls_in_bundle: dlls.into_iter().collect(),
        encrypted_hints: hints.into_iter().collect(),
    }
}

/// Noms (minuscules) de tous les fichiers du bundle jusqu'à 4 niveaux, pour vérifier les citations.
fn bundle_file_names(dir: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            visited += 1;
            if visited > 200_000 {
                return names;
            }
            let p = e.path();
            if p.is_dir() {
                if depth < 4 {
                    stack.push((p, depth + 1));
                }
            } else if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                names.insert(n.to_ascii_lowercase());
            }
        }
    }
    names
}

// ---------------------------------------------------------------------------------------------
// Construction du rapport
// ---------------------------------------------------------------------------------------------

pub fn analyze(exe: &Path, bundle_dir: Option<&Path>, opts: &Options) -> Result<Report> {
    let bytes = fs::read(exe).with_context(|| format!("lecture impossible : {}", exe.display()))?;
    let pe = match PE::parse(&bytes) {
        Ok(pe) => pe,
        Err(e) => {
            let magic: Vec<String> = bytes.iter().take(8).map(|b| format!("{b:02x}")).collect();
            bail!(
                "ce fichier n'est pas un PE32+ lisible ({e}). Premiers octets : {}. \
                 Un binaire chiffré, compressé ou empaqueté n'est pas pris en charge.",
                magic.join(" ")
            );
        }
    };
    let file = file_info(&pe, exe, bytes.len() as u64);
    let load = load_info(&file);

    let hle = DefaultHleResolver::new();
    let mut bundle_dlls: BTreeSet<String> = BTreeSet::new();
    for dir in bundle_dir.into_iter().chain(Some(opts.dll_dir.as_path())) {
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                if let Some(n) = e.file_name().to_str()
                    && n.to_ascii_lowercase().ends_with(".dll")
                {
                    bundle_dlls.insert(n.to_ascii_lowercase());
                }
            }
        }
    }

    let summary_pe = inspect_pe_bytes(&bytes, &exe.display().to_string())?;
    let mut all_imports: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut dlls = Vec::new();
    for imp in &summary_pe.imports {
        all_imports
            .entry(imp.name.clone())
            .or_default()
            .extend(imp.functions.iter().cloned());
        dlls.push(evaluate_dll(
            &hle,
            &imp.name,
            &format!("{:?}", imp.category),
            &imp.functions,
            &bundle_dlls,
        ));
    }
    let mut delay = Vec::new();
    for (dll, funcs) in delay_imports(&pe, &bytes) {
        let mut funcs = funcs;
        funcs.sort();
        funcs.dedup();
        all_imports
            .entry(dll.clone())
            .or_default()
            .extend(funcs.iter().cloned());
        delay.push(evaluate_dll(&hle, &dll, "retardé", &funcs, &bundle_dlls));
    }

    let (gdk_classes, interfaces) = scan_guids(&bytes);
    let shaders = scan_shaders(&bytes);
    let shaders = (shaders.containers > 0).then_some(shaders);
    let cpu = (!opts.no_cpu).then(|| scan_cpu(&pe, &bytes, file.image_base));

    let bundle = bundle_dir.map(scan_bundle);
    let import_dll_names: BTreeSet<String> =
        all_imports.keys().map(|d| d.to_ascii_lowercase()).collect();
    let mut files_referenced = Vec::new();
    {
        let existing = bundle_dir.map(bundle_file_names);
        for name in scan_file_references(&bytes, &import_dll_names) {
            // Les noms contenant `%` sont des modèles de format (`track_%02d.ogg`), pas des fichiers.
            let is_pattern = name.contains('%');
            files_referenced.push(FileRef {
                present_in_bundle: if is_pattern {
                    None
                } else {
                    existing.as_ref().map(|e| e.contains(&name))
                },
                name,
            });
        }
    }

    let features = derive_features(&all_imports, &gdk_classes, shaders.as_ref());
    let needs = machine_needs(
        &file,
        &features,
        shaders.as_ref(),
        cpu.as_deref(),
        bundle.as_ref(),
    );

    let all: Vec<&DllNeed> = dlls.iter().chain(delay.iter()).collect();
    let imports_total: usize = all.iter().map(|d| d.total).sum();
    let imports_native: usize = all.iter().map(|d| d.covered).sum();
    let mut blockers = Vec::new();
    if load.preferred_base_free == Some(false) && !file.has_relocations {
        blockers.push("adresse de chargement occupée et image non relogeable".to_string());
    }
    for d in &all {
        if d.tier == "absente" {
            blockers.push(format!("DLL absente : {} ({} import(s))", d.name, d.total));
        }
    }
    if let Some(s) = &shaders
        && s.dxil > 0
    {
        blockers.push(format!(
            "{} shader(s) DXIL : génération SPIR-V non implémentée",
            s.dxil
        ));
    }
    for c in gdk_classes.iter().filter(|c| !c.served) {
        blockers.push(format!(
            "classe GDK référencée mais non fournie : {}",
            c.name
        ));
    }
    if let Some(b) = &bundle
        && !b.encrypted_hints.is_empty()
    {
        blockers.push("paquet(s) Xbox chiffré(s) dans le bundle".to_string());
    }
    let absent: Vec<&str> = files_referenced
        .iter()
        .filter(|f| f.present_in_bundle == Some(false))
        .map(|f| f.name.as_str())
        .collect();
    if !absent.is_empty() {
        blockers.push(format!(
            "{} fichier(s) de données cité(s) mais absent(s) du bundle (à fournir) : {}{}",
            absent.len(),
            absent
                .iter()
                .take(6)
                .copied()
                .collect::<Vec<_>>()
                .join(", "),
            if absent.len() > 6 { ", …" } else { "" }
        ));
    }

    Ok(Report {
        file,
        load,
        dlls,
        delay_imports: delay,
        gdk_classes,
        interfaces,
        shaders,
        cpu,
        files_referenced,
        features,
        machine_needs: needs,
        bundle,
        summary: Summary {
            imports_total,
            imports_native,
            native_percent: (imports_native * 100)
                .checked_div(imports_total)
                .unwrap_or(100),
            likely_blockers: blockers,
        },
    })
}

// ---------------------------------------------------------------------------------------------
// Rendu
// ---------------------------------------------------------------------------------------------

fn print_dlls(title: &str, list: &[DllNeed]) {
    if list.is_empty() {
        return;
    }
    println!("\n── {title} ({}) ──", list.len());
    for d in list {
        let mark = match d.tier.as_str() {
            "natif" => "✔",
            "partiel" => "◐",
            "invité" => "▣",
            "stubs" => "○",
            _ => "✘",
        };
        println!(
            "  {mark} {:<44} {:<8} {}/{}{}",
            d.name,
            d.tier,
            d.covered,
            d.total,
            if d.in_bundle {
                "  [DLL présente dans le bundle]"
            } else {
                ""
            }
        );
        for m in d.missing.iter().take(40) {
            println!("        - {m}");
        }
        if d.missing.len() > 40 {
            println!("        … (+{} autres : voir --json)", d.missing.len() - 40);
        }
    }
}

pub fn print_text(r: &Report) {
    let f = &r.file;
    println!("══════════════ ScarletBridge — besoins du binaire ══════════════");
    println!("\n── Fichier ──");
    println!("  {}", f.path);
    println!("  taille            : {}", kib(f.size_bytes));
    println!(
        "  type              : {} {} (sous-système : {})",
        f.machine, f.kind, f.subsystem
    );
    println!(
        "  base / image      : {:#x} / {}",
        f.image_base,
        kib(f.size_of_image)
    );
    println!("  point d'entrée    : RVA {:#x}", f.entry_rva);
    println!(
        "  OS / sous-syst.   : {} / {}   éditeur de liens : {}",
        f.os_version, f.subsystem_version, f.linker
    );
    println!(
        "  pile / tas        : {} + {} / {} + {} (réservé + validé)",
        kib(f.stack_reserve),
        kib(f.stack_commit),
        kib(f.heap_reserve),
        kib(f.heap_commit)
    );
    println!(
        "  protections       : {}",
        if f.dll_characteristics.is_empty() {
            "aucune".into()
        } else {
            f.dll_characteristics.join(", ")
        }
    );
    println!(
        "  relocations       : {}   rappels TLS : {}   exports : {}",
        if f.has_relocations { "oui" } else { "non" },
        f.tls_callbacks,
        f.exports
    );
    if let Some(p) = &f.pdb {
        println!("  PDB               : {p}");
    }
    println!("  sections          :");
    for s in &f.sections {
        println!(
            "    {:<8} virt {:>10}  brut {:>10}  {}",
            s.name,
            kib(u64::from(s.virtual_size)),
            kib(u64::from(s.raw_size)),
            s.flags
        );
    }
    println!("\n── Chargement ──");
    println!(
        "  base préférée {:#x} : {}",
        r.load.preferred_base, r.load.verdict
    );

    print_dlls("DLL importées", &r.dlls);
    print_dlls("Imports retardés (delay-load)", &r.delay_imports);

    if !r.gdk_classes.is_empty() {
        println!(
            "\n── Classes du runtime GDK référencées ({}) ──",
            r.gdk_classes.len()
        );
        for c in &r.gdk_classes {
            println!(
                "  {} {:<26} {}  ({}×)",
                if c.served { "✔" } else { "✘" },
                c.name,
                c.clsid,
                c.occurrences
            );
        }
    }
    if !r.interfaces.is_empty() {
        println!(
            "\n── Interfaces D3D12/DXGI référencées ({}) ──",
            r.interfaces.len()
        );
        for i in &r.interfaces {
            println!("  {:<28} {}  ({}×)", i.name, i.iid, i.occurrences);
        }
    }
    if let Some(s) = &r.shaders {
        println!("\n── Shaders embarqués ──");
        println!(
            "  {} conteneur(s) : {} DXBC (SM4/5), {} DXIL (SM6) ; {} traduisible(s), {} non pris en charge",
            s.containers, s.dxbc, s.dxil, s.translatable, s.unsupported
        );
        for (k, n) in &s.by_profile {
            println!("    {k:<22} {n}");
        }
    }
    if let Some(cpu) = &r.cpu {
        println!("\n── Instructions CPU utilisées (balayage du code) ──");
        if cpu.is_empty() {
            println!("  aucune extension notable");
        }
        for c in cpu {
            let host = match c.host_supports {
                Some(true) => "hôte : ✔",
                Some(false) => "hôte : ✘ MANQUANT",
                None => "",
            };
            println!("  {:<12} {:>10} instr.  {host}", c.name, c.instructions);
        }
    }
    if !r.files_referenced.is_empty() {
        println!(
            "\n── Fichiers cités dans le binaire ({}) ──",
            r.files_referenced.len()
        );
        for fr in r.files_referenced.iter().take(60) {
            let state = match fr.present_in_bundle {
                Some(true) => "présent",
                Some(false) => "ABSENT",
                None if fr.name.contains('%') => "modèle",
                None => "?",
            };
            println!("  {state:<8} {}", fr.name);
        }
        if r.files_referenced.len() > 60 {
            println!("  … (+{} : voir --json)", r.files_referenced.len() - 60);
        }
    }

    println!("\n── Fonctionnalités requises et état du bridge ──");
    for x in &r.features {
        let mark = match x.status.as_str() {
            "ok" => "✔",
            "partiel" => "◐",
            "limité" => "◔",
            _ => "✘",
        };
        println!("  {mark} {:<34} [{}] {}", x.name, x.status, x.detail);
    }

    println!("\n── Besoins côté machine ──");
    for n in &r.machine_needs {
        println!("  • {n}");
    }

    if let Some(b) = &r.bundle {
        println!("\n── Bundle ──");
        println!("  dossier : {}", b.directory);
        println!("  {} fichier(s), {}", b.total_files, kib(b.total_bytes));
        if let Some(m) = &b.manifest {
            println!("  manifeste : {m}");
        }
        if let Some(t) = &b.title_id {
            println!("  titre : {t}");
        }
        if !b.executables.is_empty() {
            println!("  exécutables : {}", b.executables.join(", "));
        }
        if let Some(q) = b.persistent_storage_mb {
            println!("  stockage persistant demandé : {q} Mio");
        }
        for e in &b.by_extension {
            println!(
                "    .{:<10} {:>7} fichier(s)  {}",
                e.ext,
                e.files,
                kib(e.bytes)
            );
        }
        if !b.dlls_in_bundle.is_empty() {
            println!("  DLL embarquées : {}", b.dlls_in_bundle.join(", "));
        }
        for h in &b.encrypted_hints {
            println!("  ⚠ {h}");
        }
    }

    let s = &r.summary;
    println!("\n── Synthèse ──");
    println!(
        "  imports couverts nativement : {}/{} ({} %)",
        s.imports_native, s.imports_total, s.native_percent
    );
    if s.likely_blockers.is_empty() {
        println!(
            "  aucun blocage statique détecté (le comportement à l'exécution reste à valider)"
        );
    } else {
        println!("  blocages probables :");
        for b in &s.likely_blockers {
            println!("    ✘ {b}");
        }
    }
    println!(
        "\nAnalyse statique uniquement : rien n'a été exécuté. Utilisez --json pour tout exporter."
    );
}

/// Point d'entrée : résout la cible (exécutable ou dossier), analyse et affiche.
pub fn run(target: &Path, opts: &Options) -> Result<()> {
    let (exe, bundle) = if target.is_dir() {
        let manifest = AppManifest::discover_in_dir(target).ok();
        let exe = manifest
            .as_ref()
            .and_then(|m| m.primary_executable())
            .map(|e| target.join(&e.name))
            .or_else(|| {
                fs::read_dir(target).ok().and_then(|rd| {
                    rd.flatten()
                        .map(|e| e.path())
                        .find(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("exe")))
                })
            })
            .context(
                "aucun exécutable principal trouvé dans ce dossier (passez directement le .exe)",
            )?;
        (exe, Some(target.to_path_buf()))
    } else {
        (target.to_path_buf(), target.parent().map(Path::to_path_buf))
    };
    let report = analyze(&exe, bundle.as_deref(), opts)?;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text(&report);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_formatting() {
        assert_eq!(kib(512), "0.5 Kio");
        assert_eq!(kib(3 << 20), "3.0 Mio");
        assert_eq!(kib(5 << 30), "5.00 Gio");
    }

    #[test]
    fn dxbc_container_scan_classifies_profiles() {
        // Conteneur DXBC minimal : en-tête (32 o) + 1 chunk `SHDR` (en-tête 8 o + version 4 o).
        fn container(tag: &[u8; 4], version: u32) -> Vec<u8> {
            let chunk_off = 36u32;
            let total = chunk_off + 8 + 4;
            let mut v = Vec::new();
            v.extend_from_slice(b"DXBC");
            v.extend_from_slice(&[0u8; 16]); // somme de contrôle
            v.extend_from_slice(&1u32.to_le_bytes());
            v.extend_from_slice(&total.to_le_bytes());
            v.extend_from_slice(&1u32.to_le_bytes()); // nombre de chunks
            v.extend_from_slice(&chunk_off.to_le_bytes());
            v.extend_from_slice(tag);
            v.extend_from_slice(&4u32.to_le_bytes());
            v.extend_from_slice(&version.to_le_bytes());
            v
        }
        let mut blob = vec![0u8; 13];
        blob.extend(container(b"SHDR", (1 << 16) | (4 << 4))); // vs_4_0
        blob.extend(vec![0u8; 7]);
        blob.extend(container(b"SHEX", (3 << 16) | (5 << 4))); // hs_5_0 : non traduisible
        blob.extend(vec![0u8; 5]);
        blob.extend(container(b"DXIL", (6 << 4) | 5)); // ps_6_5 (type 0 = ps)
        let s = scan_shaders(&blob);
        assert_eq!((s.containers, s.dxbc, s.dxil), (3, 2, 1));
        assert_eq!(s.translatable, 1);
        assert_eq!(s.unsupported, 2);
        assert_eq!(s.by_profile.get("DXBC vs_4_0"), Some(&1));
        assert_eq!(s.by_profile.get("DXIL ps_6_5"), Some(&1));
    }

    #[test]
    fn guid_scan_finds_gdk_classes_and_names_interfaces() {
        let (class, _) = sb_hle::gdk_classes::KNOWN_GDK_CLASSES[2];
        let _ = class;
        let g = sb_hle::gdk_classes::KNOWN_GDK_CLASSES[2].1;
        let mut blob = vec![0xAAu8; 23];
        blob.extend_from_slice(&guid_bytes(&g));
        blob.extend_from_slice(&[0xBB; 9]);
        let (classes, ifaces) = scan_guids(&blob);
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "XThreading / XAsync");
        assert!(classes[0].served, "XThreadingImpl est fournie");
        assert!(ifaces.is_empty());
    }

    #[test]
    fn file_references_are_extracted_and_system_dlls_ignored() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"\0\0G:\\Media\\audio.rpf\0\0kernel32.dll\0xx\0config.json\0");
        let imports: BTreeSet<String> = ["kernel32.dll".to_string()].into();
        let names = scan_file_references(&blob, &imports);
        assert!(names.contains(&"audio.rpf".to_string()), "{names:?}");
        assert!(names.contains(&"config.json".to_string()), "{names:?}");
        assert!(!names.contains(&"kernel32.dll".to_string()), "{names:?}");
    }

    #[test]
    fn non_pe_input_is_rejected_with_a_clear_message() {
        let dir = std::env::temp_dir().join(format!("sb_req_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dossier");
        let f = dir.join("pas_un_pe.exe");
        std::fs::write(&f, b"XVC0 donnees chiffrees...").expect("fichier");
        let opts = Options {
            json: false,
            no_cpu: true,
            dll_dir: dir.clone(),
        };
        let err = analyze(&f, Some(&dir), &opts).expect_err("doit échouer");
        assert!(err.to_string().contains("PE32+"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
