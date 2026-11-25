//! Corpus de validation DXIL : chaque `.dxil` de `tests/shaders/dxil/` (compilé avec le compilateur
//! officiel Microsoft DXC à partir des `.hlsl` voisins) est traduit puis validé avec
//! `spirv-val --target-env vulkan1.3`. Rapport : `cargo test -p sb-gpu --test dxil_corpus -- --nocapture`.
//!
//! Les fichiers `unsup_*.dxil` (mesh shader, bibliothèque raytracing, GS/HS/DS : stages hors
//! périmètre actuel du traducteur, VS/PS/CS uniquement) doivent échouer avec une
//! erreur typée (jamais de panique). Le test de recompilation via `dxc` est sauté si `dxc` est
//! absent (`DXC_DIR=/chemin/vers/dxc` avec `bin/dxc` et `lib/`).
//!
//! `DXIL_DUMP_DIR=/dossier` écrit les modules SPIR-V produits pour inspection (`spirv-dis`).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sb_gpu::{ShaderStage, is_dxil_container, translate_dxil_to_spirv};

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/shaders/dxil")
}

fn spirv_val(bytes: &[u8]) -> Result<(), String> {
    let mut child = Command::new("spirv-val")
        .args(["--target-env", "vulkan1.3", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(bytes);
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

fn dxil_files() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(corpus_dir())
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("dxil"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn dxil_corpus_translates_and_validates() {
    let files = dxil_files();
    if files.is_empty() {
        eprintln!("corpus DXIL absent : test sauté");
        return;
    }
    let have_val = Command::new("spirv-val").arg("--version").output().is_ok();
    if !have_val {
        eprintln!("spirv-val absent : validation sautée (traduction seule)");
    }
    let dump = std::env::var_os("DXIL_DUMP_DIR").map(PathBuf::from);
    let (mut translated, mut validated, mut total) = (0usize, 0usize, 0usize);
    let mut failures: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut unsup_ok = 0usize;
    let mut unsup_total = 0usize;
    for p in &files {
        let name = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        if let Ok(only) = std::env::var("DXIL_ONLY")
            && name != only
        {
            continue;
        }
        let data = std::fs::read(p).expect("lecture du .dxil");
        assert!(is_dxil_container(&data), "{name}: conteneur DXIL attendu");
        let r = translate_dxil_to_spirv(&data, ShaderStage::Unknown(0));
        if name.starts_with("unsup_") {
            unsup_total += 1;
            match r {
                Err(e) => {
                    unsup_ok += 1;
                    eprintln!("  [typé] {name}: {e}");
                }
                Ok(_) => panic!("{name}: devrait être rejeté avec une erreur typée"),
            }
            continue;
        }
        total += 1;
        match r {
            Ok(t) => {
                translated += 1;
                let bytes = t.as_bytes();
                if let Some(d) = &dump {
                    let _ = std::fs::create_dir_all(d);
                    let _ = std::fs::write(d.join(format!("{name}.spv")), &bytes);
                }
                if have_val {
                    match spirv_val(&bytes) {
                        Ok(()) => validated += 1,
                        Err(e) => {
                            let first = e.lines().next().unwrap_or("").to_string();
                            failures
                                .entry(format!("spirv-val: {first}"))
                                .or_default()
                                .push(name);
                        }
                    }
                } else {
                    validated += 1;
                }
            }
            Err(e) => {
                failures.entry(e.to_string()).or_default().push(name);
            }
        }
    }
    eprintln!("corpus DXIL : {total} shaders (+ {unsup_total} rejets attendus)");
    eprintln!("  traduits : {translated}/{total}");
    eprintln!("  validés  : {validated}/{total} (spirv-val --target-env vulkan1.3)");
    eprintln!("  rejets typés : {unsup_ok}/{unsup_total}");
    let mut sorted: Vec<_> = failures.iter().collect();
    sorted.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    for (msg, names) in sorted {
        eprintln!(
            "  échec x{} : {msg}\n      {}",
            names.len(),
            names.join(", ")
        );
    }
    assert_eq!(
        translated, total,
        "tous les shaders du corpus doivent se traduire"
    );
    assert_eq!(
        validated, total,
        "tous les shaders du corpus doivent être validés"
    );
}

#[test]
fn dxil_recompile_with_dxc_matches_committed_corpus() {
    let Some(dxc_dir) = std::env::var_os("DXC_DIR").map(PathBuf::from) else {
        eprintln!("DXC_DIR non défini : recompilation sautée");
        return;
    };
    let dxc = dxc_dir.join("bin/dxc");
    if !dxc.exists() {
        eprintln!("dxc absent : recompilation sautée");
        return;
    }
    let tmp = std::env::temp_dir().join(format!("sb-dxil-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    let mut n = 0;
    for entry in std::fs::read_dir(corpus_dir()).expect("corpus").flatten() {
        let p = entry.path();
        if p.extension().and_then(|x| x.to_str()) != Some("hlsl") {
            continue;
        }
        let src = std::fs::read_to_string(&p).expect("hlsl");
        let args = src
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("// dxc: "))
            .unwrap_or("-T cs_6_0")
            .to_string();
        let out = tmp.join(p.file_stem().expect("nom").to_string_lossy().to_string() + ".dxil");
        let status = Command::new(&dxc)
            .env("LD_LIBRARY_PATH", dxc_dir.join("lib"))
            .args(args.split_whitespace())
            .args(["-E", "main", "-Fo"])
            .arg(&out)
            .arg(&p)
            .output()
            .expect("dxc");
        assert!(status.status.success(), "dxc a échoué sur {p:?}");
        let data = std::fs::read(&out).expect("sortie dxc");
        assert!(is_dxil_container(&data));
        let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if !name.starts_with("unsup_") {
            translate_dxil_to_spirv(&data, ShaderStage::Unknown(0))
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        n += 1;
    }
    let _ = std::fs::remove_dir_all(&tmp);
    eprintln!("{n} shaders recompilés et traduits");
}

/// Robustesse : conteneurs tronqués / octets corrompus -> jamais de panique (erreur typée ou succès).
#[test]
fn dxil_mutated_inputs_never_panic() {
    let files = dxil_files();
    let mut seed = 0x1234_5678u32;
    let mut next = || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        seed
    };
    let iters: usize = std::env::var("DXIL_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(150);
    let mut cases = 0usize;
    let mut internal = Vec::new();
    let mut check =
        |name: &str, r: Result<sb_gpu::TranslatedShader, sb_gpu::ShaderTranslateError>| {
            if let Err(e) = r
                && e.to_string().contains("erreur interne")
            {
                internal.push(name.to_string());
            }
        };
    for p in &files {
        let data = std::fs::read(p).expect("lecture");
        let nm = p.display().to_string();
        // Troncatures.
        for k in 1..24usize {
            let len = data.len() * k / 24;
            check(
                &nm,
                translate_dxil_to_spirv(&data[..len], ShaderStage::Unknown(0)),
            );
            cases += 1;
        }
        // Octets corrompus (dans tout le conteneur).
        for _ in 0..iters {
            let mut d = data.clone();
            let flips = 1 + (next() % 4) as usize;
            for _ in 0..flips {
                let pos = (next() as usize) % d.len();
                d[pos] ^= 1 << (next() % 8);
            }
            check(&nm, translate_dxil_to_spirv(&d, ShaderStage::Unknown(0)));
            cases += 1;
        }
    }
    eprintln!("{cases} entrées mutées traitées");
    assert!(
        internal.is_empty(),
        "paniques internes interceptées sur : {internal:?}"
    );
}
