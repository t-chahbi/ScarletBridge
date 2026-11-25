//! Test différentiel du désassembleur RDNA 2 contre `llvm-mc` (ignoré par défaut).
//!
//! Le fichier TSV est produit par `python3 -I tools/rdna2/gen_diff.py <graine> <n>` ;
//! `RDNA2_DIFF_TSV=/chemin/diff.tsv cargo test -p sb-gpu --test rdna2_diff -- --ignored --nocapture`.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use sb_gpu::rdna2::{WaveSize, decode, disassemble};

#[test]
#[ignore = "nécessite RDNA2_DIFF_TSV (généré via llvm-mc)"]
fn rdna2_differential_vs_llvm() {
    let Ok(path) = std::env::var("RDNA2_DIFF_TSV") else {
        eprintln!("RDNA2_DIFF_TSV non défini : test ignoré");
        return;
    };
    let data = std::fs::read_to_string(path).expect("lecture TSV");
    let (mut total, mut ok) = (0usize, 0usize);
    let mut bad: BTreeMap<String, (usize, String)> = BTreeMap::new();
    for line in data.lines() {
        let mut it = line.split('\t');
        let wave = match it.next() {
            Some("32") => WaveSize::W32,
            _ => WaveSize::W64,
        };
        let words: Vec<u32> = it
            .next()
            .unwrap()
            .split(',')
            .map(|h| u32::from_str_radix(h, 16).unwrap())
            .collect();
        let want = it
            .next()
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        total += 1;
        let got = match decode(&words, wave) {
            Ok(i) if usize::from(i.words) == words.len() => disassemble(&i, wave),
            Ok(i) => format!(
                "<len {} != {}> {}",
                i.words,
                words.len(),
                disassemble(&i, wave)
            ),
            Err(e) => format!("<err {e}>"),
        };
        if got == want {
            ok += 1;
        } else {
            let key = want.split_whitespace().next().unwrap_or("").to_string();
            let e = bad.entry(key).or_insert((0, String::new()));
            e.0 += 1;
            if e.1.is_empty() {
                e.1 = format!("{wave:?} {words:08x?}\n    want: {want}\n    got : {got}");
            }
        }
    }
    eprintln!("{ok}/{total} identiques à LLVM");
    for (k, (n, ex)) in &bad {
        eprintln!("[{k}] x{n}: {ex}");
    }
    assert!(bad.is_empty(), "{} mnémoniques divergents", bad.len());
}
