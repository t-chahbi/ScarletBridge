//! Corpus de validation : extrait tous les conteneurs DXBC d'`OpenLara.exe` (build Xbox, shaders
//! compilés par `fxc`), les traduit et les valide avec `spirv-val --target-env vulkan1.3`.
//!
//! Le test est sauté si le binaire ou `spirv-val` est absent. Rapport détaillé :
//! `cargo test -p sb-gpu --test dxbc_corpus -- --nocapture`.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

use sb_gpu::translate_dxbc_to_spirv;

fn extract_containers(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = data[cursor..].windows(4).position(|w| w == b"DXBC") {
        let off = cursor + rel;
        cursor = off + 4;
        if off + 32 > data.len() {
            break;
        }
        let rd = |o: usize| {
            u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]) as usize
        };
        let total = rd(off + 24);
        let nchunks = rd(off + 28);
        if nchunks == 0 || nchunks > 32 || total < 32 || off + total > data.len() {
            continue;
        }
        out.push(&data[off..off + total]);
    }
    out
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

#[test]
fn openlara_corpus_translates_and_validates() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tmp/openlara-xb1/app/OpenLara.exe");
    let Ok(data) = std::fs::read(&path) else {
        eprintln!("corpus OpenLara absent : test sauté");
        return;
    };
    if Command::new("spirv-val").arg("--version").output().is_err() {
        eprintln!("spirv-val absent : test sauté");
        return;
    }
    let containers = extract_containers(&data);
    let mut translated = 0usize;
    let mut validated = 0usize;
    let mut errors: BTreeMap<String, usize> = BTreeMap::new();
    for dxbc in &containers {
        match translate_dxbc_to_spirv(dxbc) {
            Ok(t) => {
                translated += 1;
                match spirv_val(&t.as_bytes()) {
                    Ok(()) => validated += 1,
                    Err(e) => {
                        let first = e.lines().next().unwrap_or("").to_string();
                        *errors.entry(format!("spirv-val: {first}")).or_default() += 1;
                    }
                }
            }
            Err(e) => *errors.entry(e.to_string()).or_default() += 1,
        }
    }
    let total = containers.len();
    eprintln!("corpus OpenLara : {total} conteneurs DXBC");
    eprintln!("  traduits  : {translated}/{total}");
    eprintln!("  validés   : {validated}/{total} (spirv-val --target-env vulkan1.3)");
    let mut sorted: Vec<_> = errors.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));
    for (msg, n) in sorted {
        eprintln!("  échec x{n} : {msg}");
    }
    assert!(total >= 50, "corpus attendu >= 50 shaders (trouvé {total})");
    assert_eq!(translated, total, "tous les shaders doivent se traduire");
    assert_eq!(validated, total, "tous les shaders doivent être validés");
}
