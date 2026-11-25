//! Variantes de conteneurs DXIL déformées de façon contrôlée (tolérance du localiseur de bitcode).
//! Chaque variante récupérable doit produire exactement le même SPIR-V que le conteneur d'origine.

use std::path::Path;

use sb_gpu::{ShaderStage, is_dxil_container, translate_dxil_to_spirv};

fn load(name: &str) -> Vec<u8> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/shaders/dxil")
        .join(format!("{name}.dxil"));
    std::fs::read(p).expect("corpus")
}

fn rd(b: &[u8], o: usize) -> usize {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as usize
}

/// Payload du chunk `DXIL` d'un conteneur valide.
fn dxil_payload(c: &[u8]) -> Vec<u8> {
    let n = rd(c, 28);
    for i in 0..n {
        let off = rd(c, 32 + i * 4);
        if &c[off..off + 4] == b"DXIL" {
            let sz = rd(c, off + 4);
            return c[off + 8..off + 8 + sz].to_vec();
        }
    }
    panic!("chunk DXIL absent");
}

fn build(chunks: &[(&[u8; 4], Vec<u8>)], total: Option<u32>, declared_extra: usize) -> Vec<u8> {
    let hdr = 32 + chunks.len() * 4;
    let mut offs = Vec::new();
    let mut cur = hdr;
    for (_, d) in chunks {
        offs.push(cur as u32);
        cur += 8 + d.len();
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"DXBC");
    out.extend_from_slice(&[0u8; 16]);
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&total.unwrap_or(cur as u32).to_le_bytes());
    out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
    for o in offs {
        out.extend_from_slice(&o.to_le_bytes());
    }
    for (i, (f, d)) in chunks.iter().enumerate() {
        out.extend_from_slice(*f);
        let extra = if i + 1 == chunks.len() {
            declared_extra
        } else {
            0
        };
        out.extend_from_slice(&((d.len() + extra) as u32).to_le_bytes());
        out.extend_from_slice(d);
    }
    out
}

fn spirv(c: &[u8]) -> Result<Vec<u32>, String> {
    translate_dxil_to_spirv(c, ShaderStage::Unknown(0))
        .map(|t| t.spirv)
        .map_err(|e| e.to_string())
}

fn set(p: &mut [u8], o: usize, v: u32) {
    p[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

#[test]
fn recoverable_container_variants_translate_identically() {
    for name in ["vs_basic", "ps_tex2d", "cs_control"] {
        let orig = load(name);
        let reference = spirv(&orig).expect("original");
        let payload = dxil_payload(&orig);
        let bc_off = 8 + rd(&payload, 16);
        let junk = vec![0x55u8; 40];
        let mut variants: Vec<(&str, Vec<u8>)> = Vec::new();

        variants.push((
            "taille de conteneur 0",
            build(&[(b"DXIL", payload.clone())], Some(0), 0),
        ));
        variants.push((
            "taille de conteneur énorme",
            build(&[(b"DXIL", payload.clone())], Some(u32::MAX), 0),
        ));
        variants.push((
            "taille de chunk déclarée trop grande",
            build(&[(b"DXIL", payload.clone())], None, 4096),
        ));
        let mut p = payload.clone();
        set(&mut p, 20, 0x00FF_FFFF);
        variants.push(("BitcodeSize énorme", build(&[(b"DXIL", p)], None, 0)));
        let mut p = payload.clone();
        set(&mut p, 20, 0);
        variants.push(("BitcodeSize nul", build(&[(b"DXIL", p)], None, 0)));
        let mut p = payload.clone();
        set(&mut p, 16, 0xFFFF);
        variants.push(("BitcodeOffset hors chunk", build(&[(b"DXIL", p)], None, 0)));
        let mut p = payload.clone();
        set(&mut p, 16, 4);
        variants.push(("BitcodeOffset décalé", build(&[(b"DXIL", p)], None, 0)));
        let mut p = payload.clone();
        p[8..12].copy_from_slice(b"XXXX");
        variants.push((
            "magic DXIL de l'en-tête corrompu",
            build(&[(b"DXIL", p)], None, 0),
        ));
        variants.push(("chunk ILDB", build(&[(b"ILDB", payload.clone())], None, 0)));
        variants.push((
            "chunks parasites avant (PSV0, ISG1, RDAT)",
            build(
                &[
                    (b"PSV0", junk.clone()),
                    (b"ISG1", junk.clone()),
                    (b"RDAT", junk.clone()),
                    (b"DXIL", payload.clone()),
                ],
                None,
                0,
            ),
        ));
        variants.push((
            "ILDN puis DXIL",
            build(
                &[(b"ILDN", junk.clone()), (b"DXIL", payload.clone())],
                None,
                0,
            ),
        ));
        variants.push((
            "DXIL invalide puis ILDB valide",
            build(
                &[(b"DXIL", junk.clone()), (b"ILDB", payload.clone())],
                None,
                0,
            ),
        ));
        variants.push(("DxilProgramHeader nu", payload.clone()));
        variants.push(("bitcode brut", payload[bc_off..].to_vec()));
        let mut padded = payload.clone();
        padded.extend_from_slice(&[0u8; 37]);
        variants.push((
            "padding après le bitcode",
            build(&[(b"DXIL", padded)], None, 0),
        ));

        for (what, c) in variants {
            assert!(
                is_dxil_container(&c),
                "{name}/{what}: non reconnu comme DXIL"
            );
            match spirv(&c) {
                Ok(s) => assert_eq!(s, reference, "{name}/{what}: SPIR-V différent"),
                Err(e) => panic!("{name}/{what}: {e}"),
            }
        }
    }
}

#[test]
fn bitcode_size_overshoot_with_tail_cut_is_recovered() {
    // BitcodeSize annonce plus que le chunk, chunk et tampon coupés après la fin du bitcode.
    let orig = load("ps_basic");
    let mut p = dxil_payload(&orig);
    let real = rd(&p, 20);
    set(&mut p, 20, (real + 64) as u32);
    let c = build(&[(b"DXIL", p)], Some(0), 0);
    assert_eq!(
        spirv(&c).expect("récupérable"),
        spirv(&orig).expect("original")
    );
}

#[test]
fn unrecoverable_inputs_give_diagnostic_errors() {
    let orig = load("cs_basic");
    let payload = dxil_payload(&orig);
    // Aucun magic bitcode.
    let mut p = payload.clone();
    for b in p.iter_mut().skip(24) {
        *b = 0x11;
    }
    let msg = spirv(&build(&[(b"DXIL", p)], None, 0)).expect_err("irrécupérable");
    assert!(msg.contains("conteneur"), "{msg}");
    assert!(msg.contains("version programme"), "{msg}");
    assert!(msg.contains("entête ["), "{msg}");
    assert!(msg.contains("DXIL:"), "liste des chunks attendue : {msg}");
    // Conteneur sans chunk DXIL.
    let msg = spirv(&build(
        &[(b"PSV0", vec![0u8; 16]), (b"RDAT", vec![1u8; 8])],
        None,
        0,
    ))
    .expect_err("sans DXIL");
    assert!(msg.contains("2 chunks") && msg.contains("PSV0:16"), "{msg}");
    // Bitcode réellement tronqué : erreur typée avec contexte, pas de panique.
    let mut p = payload.clone();
    p.truncate(p.len() * 6 / 10);
    let msg = spirv(&build(&[(b"DXIL", p)], None, 0)).expect_err("tronqué");
    assert!(msg.contains("bitcode"), "{msg}");
    assert!(msg.contains("conteneur"), "{msg}");
    // Tampons absurdes.
    for junk in [vec![], vec![0u8; 3], vec![0xFFu8; 100], b"DXBC".to_vec()] {
        assert!(spirv(&junk).is_err());
    }
}
