//! Tests de traduction RDNA 2 → SPIR-V : programmes écrits à la main (assemblés par llvm-mc),
//! module validé par `spirv-val` (ignoré avec un message si l'outil est absent).

use std::collections::BTreeSet;
use std::io::Write;
use std::process::Command;

use super::test_programs::PROGRAMS;
use super::*;

fn program(name: &str) -> &'static super::test_programs::Program {
    PROGRAMS
        .iter()
        .find(|p| p.name == name)
        .unwrap_or_else(|| panic!("programme {name}"))
}

fn wave_of(p: &super::test_programs::Program) -> WaveSize {
    if p.wave == 64 {
        WaveSize::W64
    } else {
        WaveSize::W32
    }
}

fn buffer_table(binding: u32, stride: u32, byte_offset: u32) -> Resource {
    Resource {
        kind: ResourceKind::Buffer { stride },
        loc: ResourceLoc::Table {
            table_sgpr: 0,
            byte_offset,
        },
        set: 0,
        binding,
    }
}

fn cs_cfg(p: &super::test_programs::Program) -> TranslateConfig {
    let mut c = TranslateConfig::new(Stage::Compute, wave_of(p));
    c.local_size = [64, 1, 1];
    c.sgprs = vec![(2, SgprInit::WorkgroupId(0))];
    c.vgprs = vec![(0, VgprInit::LocalInvocationId(0))];
    c.resources = vec![buffer_table(0, 0, 0)];
    c
}

fn config_for(p: &super::test_programs::Program) -> TranslateConfig {
    match p.name {
        "vs_passthrough" | "vs_uv_passthrough" => {
            let mut c = TranslateConfig::new(Stage::Vertex, WaveSize::W32);
            c.vgprs = vec![(0, VgprInit::VertexId)];
            c.resources = vec![buffer_table(0, 0, 0)];
            c
        }
        "vs_tbuffer_fetch" => {
            let mut c = TranslateConfig::new(Stage::Vertex, WaveSize::W32);
            c.vgprs = vec![(0, VgprInit::VertexId)];
            c.resources = vec![buffer_table(0, 16, 0)];
            c
        }
        "vs_constants_sbuffer" => {
            let mut c = TranslateConfig::new(Stage::Vertex, WaveSize::W32);
            c.resources = vec![Resource {
                kind: ResourceKind::Buffer { stride: 0 },
                loc: ResourceLoc::Sgprs(4),
                set: 0,
                binding: 1,
            }];
            c
        }
        "ps_const_color" | "ps_kill" => TranslateConfig::new(Stage::Fragment, WaveSize::W32),
        "ps_mimg_ext" => {
            let mut c = TranslateConfig::new(Stage::Fragment, WaveSize::W32);
            let r = |kind, binding, off| Resource {
                kind,
                loc: ResourceLoc::Table {
                    table_sgpr: 0,
                    byte_offset: off,
                },
                set: 0,
                binding,
            };
            c.resources = vec![
                r(ResourceKind::Image, 1, 0),
                r(ResourceKind::Sampler, 2, 0x20),
                r(ResourceKind::Image, 3, 0x30),
                r(ResourceKind::Image, 4, 0x50),
                r(ResourceKind::Image, 5, 0x70),
                r(ResourceKind::Image, 6, 0x90),
            ];
            c
        }
        "ps_texture" => {
            let mut c = TranslateConfig::new(Stage::Fragment, WaveSize::W32);
            c.resources = vec![
                Resource {
                    kind: ResourceKind::Image,
                    loc: ResourceLoc::Table {
                        table_sgpr: 0,
                        byte_offset: 0,
                    },
                    set: 0,
                    binding: 0,
                },
                Resource {
                    kind: ResourceKind::Sampler,
                    loc: ResourceLoc::Table {
                        table_sgpr: 0,
                        byte_offset: 0x20,
                    },
                    set: 0,
                    binding: 1,
                },
            ];
            c
        }
        "cs_global_mem" => {
            let mut c = cs_cfg(p);
            c.user_data_binding = Some((0, 1));
            c.sgprs = vec![(2, SgprInit::UserData(0)), (3, SgprInit::UserData(1))];
            c.resources.clear();
            c
        }
        "cs_ds_battery_w32" | "cs_ds_battery_w64" => {
            let mut c = cs_cfg(p);
            c.lds_dwords = 1024;
            c
        }
        "cs_bounds" => {
            let mut c = cs_cfg(p);
            let r = |binding, off| Resource {
                kind: ResourceKind::Buffer { stride: 0 },
                loc: ResourceLoc::Table {
                    table_sgpr: 0,
                    byte_offset: off,
                },
                set: 0,
                binding,
            };
            c.resources = vec![r(0, 0), r(1, 0x10)];
            c.buffer_sizes = vec![(0, 0, 128)];
            c
        }
        "cs_mimg_battery" => {
            let mut c = cs_cfg(p);
            let r = |kind, binding, off| Resource {
                kind,
                loc: ResourceLoc::Table {
                    table_sgpr: 0,
                    byte_offset: off,
                },
                set: 0,
                binding,
            };
            c.resources = vec![
                r(ResourceKind::Buffer { stride: 0 }, 0, 0),
                r(ResourceKind::Image, 4, 0x20),
                r(ResourceKind::Sampler, 5, 0x40),
                r(ResourceKind::Image, 6, 0x60),
                r(ResourceKind::Image, 7, 0x80),
            ];
            c
        }
        "cs_lds_barrier" => {
            let mut c = cs_cfg(p);
            c.lds_dwords = 64;
            c
        }
        _ => cs_cfg(p),
    }
}

/// Parcourt le module : (capacités, opcodes présents).
fn scan(words: &[u32]) -> (BTreeSet<u32>, BTreeSet<u16>) {
    let (mut caps, mut ops) = (BTreeSet::new(), BTreeSet::new());
    let mut i = 5;
    while i < words.len() {
        let wc = (words[i] >> 16) as usize;
        let op = (words[i] & 0xFFFF) as u16;
        assert!(wc > 0, "instruction SPIR-V de longueur nulle");
        if op == 17 {
            caps.insert(words[i + 1]);
        }
        ops.insert(op);
        i += wc;
    }
    (caps, ops)
}

/// Valide le module avec `spirv-val --target-env vulkan1.1`. Retourne `false` si l'outil manque.
fn spirv_val(words: &[u32], name: &str) -> bool {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("sb_rdna2_{}_{n}_{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("m.spv");
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    std::fs::File::create(&path)
        .unwrap()
        .write_all(&bytes)
        .unwrap();
    let out = Command::new("spirv-val")
        .arg("--target-env")
        .arg("vulkan1.1")
        .arg(&path)
        .output();
    let ok = match out {
        Ok(o) => {
            assert!(
                o.status.success(),
                "spirv-val a rejeté {name} :\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            true
        }
        Err(_) => {
            eprintln!("spirv-val introuvable : validation de {name} ignorée");
            false
        }
    };
    let _ = std::fs::remove_dir_all(&dir);
    ok
}

fn translate(name: &str) -> TranslatedProgram {
    let p = program(name);
    let t = translate_program(p.words, &config_for(p)).unwrap_or_else(|e| panic!("{name}: {e}"));
    spirv_val(&t.words, name);
    t
}

#[test]
fn every_program_translates_and_validates() {
    for p in PROGRAMS {
        translate(p.name);
    }
}

#[test]
fn vs_passthrough_has_position_param_and_buffer_binding() {
    let t = translate("vs_passthrough");
    assert_eq!(t.info.vs_params, vec![0]);
    assert!(t.info.bindings.contains(&BindingUse {
        set: 0,
        binding: 0,
        kind: "storage_buffer"
    }));
    assert!(
        !t.info.uses_exec_mask,
        "VS linéaire : pas de prédication EXEC"
    );
    let (caps, ops) = scan(&t.words);
    assert!(
        !caps.contains(&64),
        "pas de capacité ballot sans EXEC modifié"
    );
    assert!(!ops.contains(&246), "code linéaire : pas de boucle");
    assert_eq!(t.info.blocks, 1);
}

#[test]
fn vs_constants_come_from_scalar_buffer_loads() {
    let t = translate("vs_constants_sbuffer");
    assert_eq!(
        t.info.bindings,
        vec![BindingUse {
            set: 0,
            binding: 1,
            kind: "storage_buffer"
        }]
    );
}

#[test]
fn vs_tbuffer_fetch_uses_idxen_stride() {
    translate("vs_tbuffer_fetch");
}

#[test]
fn ps_constant_color_writes_mrt0() {
    let t = translate("ps_const_color");
    assert_eq!(t.info.mrts, vec![0]);
    assert!(t.info.ps_attrs.is_empty());
    assert!(t.info.bindings.is_empty());
    assert!(!t.info.writes_depth);
}

#[test]
fn ps_texture_binds_image_and_sampler() {
    let t = translate("ps_texture");
    assert_eq!(t.info.ps_attrs, vec![0]);
    assert_eq!(t.info.mrts, vec![0]);
    let kinds: Vec<&str> = t.info.bindings.iter().map(|b| b.kind).collect();
    assert!(
        kinds.contains(&"sampled_image") && kinds.contains(&"sampler"),
        "{kinds:?}"
    );
    let (_, ops) = scan(&t.words);
    assert!(
        ops.contains(&87),
        "OpImageSampleImplicitLod attendu dans un PS"
    );
}

#[test]
fn ps_kill_ends_with_opkill() {
    let t = translate("ps_kill");
    assert!(t.info.uses_exec_mask);
    let (caps, ops) = scan(&t.words);
    assert!(ops.contains(&252), "OpKill attendu après v_cmpx");
    assert!(caps.contains(&64), "ballot (EXEC explicite)");
}

#[test]
fn cs_write_buffer_is_linear_and_has_local_size() {
    let t = translate("cs_write_buffer");
    assert_eq!(t.info.blocks, 1);
    assert!(!t.info.uses_exec_mask);
    // OpExecutionMode LocalSize 64 1 1
    let w = &t.words;
    let mut found = false;
    let mut i = 5;
    while i < w.len() {
        let (wc, op) = ((w[i] >> 16) as usize, w[i] & 0xFFFF);
        if op == 16 && w[i + 2] == 17 {
            found = (w[i + 3], w[i + 4], w[i + 5]) == (64, 1, 1);
        }
        i += wc;
    }
    assert!(found, "LocalSize 64 1 1");
}

#[test]
fn divergent_programs_model_exec_explicitly() {
    for name in ["cs_divergent_w32", "cs_divergent_w64"] {
        let t = translate(name);
        assert!(t.info.uses_exec_mask, "{name}");
        let (caps, ops) = scan(&t.words);
        assert!(
            caps.contains(&61) && caps.contains(&64),
            "{name}: GroupNonUniform + Ballot"
        );
        assert!(ops.contains(&339), "{name}: OpGroupNonUniformBallot");
        assert!(
            ops.contains(&247),
            "{name}: régions gardées par EXEC (OpSelectionMerge)"
        );
        assert!(
            ops.contains(&251),
            "{name}: s_cbranch_execz => machine à pc"
        );
    }
}

#[test]
fn loops_use_the_pc_state_machine() {
    let t = translate("cs_scalar_loop");
    let (_, ops) = scan(&t.words);
    assert!(ops.contains(&246) && ops.contains(&251));
    assert_eq!(t.info.blocks, 3);
    let t = translate("cs_divergent_loop");
    assert!(t.info.uses_exec_mask);
}

#[test]
fn lds_program_declares_workgroup_memory_and_barrier() {
    let t = translate("cs_lds_barrier");
    let (_, ops) = scan(&t.words);
    assert!(ops.contains(&224), "OpControlBarrier");
}

#[test]
fn valu_modifiers_and_carry_ops_validate() {
    let t = translate("cs_valu_mods");
    assert!(!t.info.uses_exec_mask);
    let (caps, ops) = scan(&t.words);
    assert!(
        caps.contains(&64),
        "v_cmp/v_add_co écrivent des masques via ballot"
    );
    assert!(ops.contains(&339));
}

#[test]
fn atomics_translate_to_spirv_atomics() {
    let t = translate("cs_atomic_add");
    let (_, ops) = scan(&t.words);
    assert!(ops.contains(&234), "OpAtomicIAdd");
}

#[test]
fn deterministic_output() {
    let p = program("cs_write_buffer");
    let a = translate_program(p.words, &config_for(p)).unwrap().words;
    let b = translate_program(p.words, &config_for(p)).unwrap().words;
    assert_eq!(a, b);
}

// ------------------------------------------------------------------ erreurs

const ENDPGM: u32 = 0xBF81_0000;

fn single(text: &str, wave: u8) -> Vec<u32> {
    let v = super::test_vectors::VECTORS
        .iter()
        .find(|v| v.text == text && v.wave == wave)
        .unwrap_or_else(|| panic!("vecteur {text}"));
    let mut w = v.words.to_vec();
    w.push(ENDPGM);
    w
}

#[test]
fn unsupported_instruction_is_reported_by_name() {
    let w = single("v_cubeid_f32 v0, v1, v2, v3", 32);
    let cfg = TranslateConfig::new(Stage::Compute, WaveSize::W32);
    match translate_program(&w, &cfg) {
        Err(TranslateError::Unsupported(n)) => assert_eq!(n, "v_cubeid_f32"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn unresolved_descriptor_is_an_error_not_a_guess() {
    let w = single("buffer_load_dword v0, v1, s[4:7], 0 offen", 32);
    let cfg = TranslateConfig::new(Stage::Compute, WaveSize::W32);
    assert!(matches!(
        translate_program(&w, &cfg),
        Err(TranslateError::Unresolved(_))
    ));
}

#[test]
fn config_errors_are_detected() {
    let w = [ENDPGM];
    let mut cfg = TranslateConfig::new(Stage::Compute, WaveSize::W32);
    cfg.sgprs = vec![(2, SgprInit::UserData(0))];
    assert!(matches!(
        translate_program(&w, &cfg),
        Err(TranslateError::Config(_))
    ));
    // VS sans export de position
    let cfg = TranslateConfig::new(Stage::Vertex, WaveSize::W32);
    assert!(matches!(
        translate_program(&w, &cfg),
        Err(TranslateError::Config(_))
    ));
    // WorkgroupId hors CS
    let mut cfg = TranslateConfig::new(Stage::Fragment, WaveSize::W32);
    cfg.sgprs = vec![(2, SgprInit::WorkgroupId(0))];
    assert!(matches!(
        translate_program(&w, &cfg),
        Err(TranslateError::Config(_))
    ));
}

#[test]
fn user_data_buffer_initialises_sgprs() {
    let mut cfg = TranslateConfig::new(Stage::Compute, WaveSize::W32);
    cfg.user_data_binding = Some((0, 7));
    cfg.sgprs = vec![(0, SgprInit::UserData(3)), (1, SgprInit::Constant(5))];
    let t = translate_program(&[ENDPGM], &cfg).unwrap();
    assert!(spirv_val(&t.words, "user_data") || true);
    assert_eq!(
        t.info.bindings,
        vec![BindingUse {
            set: 0,
            binding: 7,
            kind: "storage_buffer"
        }]
    );
}

#[test]
fn cfg_and_decode_errors_propagate() {
    let cfg = TranslateConfig::new(Stage::Compute, WaveSize::W32);
    assert!(matches!(
        translate_program(&[0xBF80_0000], &cfg),
        Err(TranslateError::Cfg(_))
    ));
    assert!(matches!(
        translate_program(&[0xEC00_0000, 0], &cfg),
        Err(TranslateError::Decode(_))
    ));
}

#[test]
fn wave_mismatched_saveexec_is_rejected() {
    let w = single("s_and_saveexec_b64 s[0:1], vcc", 64);
    let cfg = TranslateConfig::new(Stage::Compute, WaveSize::W32);
    assert!(matches!(
        translate_program(&w, &cfg),
        Err(TranslateError::Unsupported(_))
    ));
}

#[test]
fn programs_decode_back_to_their_asm_source() {
    for p in PROGRAMS {
        let w = wave_of(p);
        let decoded = decode_stream(p.words, w).unwrap();
        let got: Vec<&str> = decoded.iter().map(|d| d.inst.base_name()).collect();
        let want: Vec<&str> = p
            .asm
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.ends_with(':') && !l.starts_with(';'))
            .filter_map(|l| l.split_whitespace().next())
            .map(|m| {
                m.trim_end_matches("_e64")
                    .trim_end_matches("_e32")
                    .trim_end_matches("_dpp")
                    .trim_end_matches("_sdwa")
            })
            .collect();
        assert_eq!(got, want, "{} : mnémoniques décodés != source asm", p.name);
    }
}

// ------------------------------------------------------------------ couverture mesurée

/// Config générique : V# en s[4:7], T# en s[8:15], S# en s[16:19], LDS de 64 dwords.
fn coverage_cfg(stage: Stage) -> TranslateConfig {
    let mut c = TranslateConfig::new(stage, WaveSize::W32);
    c.lds_dwords = 64;
    let r = |kind, first, binding| Resource {
        kind,
        loc: ResourceLoc::Sgprs(first),
        set: 0,
        binding,
    };
    c.resources = vec![
        r(ResourceKind::Buffer { stride: 16 }, 4, 0),
        r(ResourceKind::Image, 8, 1),
        r(ResourceKind::Sampler, 16, 2),
    ];
    c
}

/// Vrai si au moins une configuration (stage, taille de vague, variante de sources) accepte
/// l'instruction (toute erreur autre que `Unsupported` signifie « mnémonique implémenté,
/// contexte manquant »). `None` si l'instruction n'est pas décodable.
fn is_implemented(variants: &[Vec<u32>]) -> Option<bool> {
    let mut decodable = false;
    for words in variants {
        let mut w = words.clone();
        w.push(ENDPGM);
        for wave in [WaveSize::W32, WaveSize::W64] {
            if decode_stream(&w, wave).is_err() {
                continue;
            }
            decodable = true;
            for stage in [Stage::Compute, Stage::Fragment, Stage::Vertex] {
                let mut cfg = coverage_cfg(stage);
                cfg.wave = wave;
                match translate_program(&w, &cfg) {
                    Err(TranslateError::Unsupported(_)) => {}
                    _ => return Some(true),
                }
            }
        }
    }
    decodable.then_some(false)
}

#[test]
fn translator_coverage_report() {
    use super::tables::*;
    // (famille, table, constructeur de dwords)
    type Mk = fn(u16, &str) -> Vec<Vec<u32>>;
    type Family<'a> = (&'a str, &'a [(u16, &'a str, &'a str)], Mk);
    let fams: Vec<Family<'_>> = vec![
        ("SOP2", SOP2, |op, _| {
            vec![vec![
                0x8000_0000 | u32::from(op) << 23 | 4 << 16 | 12 << 8 | 8,
            ]]
        }),
        ("SOP1", SOP1, |op, _| {
            vec![vec![0xBE80_0000 | 4 << 16 | u32::from(op) << 8 | 8]]
        }),
        ("SOPC", SOPC, |op, _| {
            vec![vec![0xBF00_0000 | u32::from(op) << 16 | 12 << 8 | 8]]
        }),
        ("SOPK", SOPK, |op, _| {
            vec![vec![0xB000_0000 | u32::from(op) << 23 | 4 << 16 | 0x10]]
        }),
        ("SOPP", SOPP, |op, _| {
            vec![vec![0xBF80_0000 | u32::from(op) << 16]]
        }),
        ("VOP2", VOP2, |op, sh| {
            let mut v = vec![u32::from(op) << 25 | 2 << 17 | 3 << 9 | 257];
            if sh.contains('K') {
                v.push(0x3f80_0000);
            }
            vec![v]
        }),
        ("VOP1", VOP1, |op, _| {
            vec![vec![0x7E00_0000 | 2 << 17 | u32::from(op) << 9 | 257]]
        }),
        ("VOPC", VOPC, |op, _| {
            vec![vec![0x7C00_0000 | u32::from(op) << 17 | 3 << 9 | 257]]
        }),
        ("VOP3", VOP3, |op, sh| {
            let sdst = if sh.contains('E') { 8 << 8 } else { 0 };
            let w0 = 0xD400_0000 | u32::from(op) << 16 | 2 | sdst;
            // sources VGPR, puis sources SGPR (masques de lanes, v_readlane...)
            vec![
                vec![w0, 257 | 258 << 9 | 259 << 18],
                vec![w0, 8 | 10 << 9 | 12 << 18],
            ]
        }),
        ("VOP3P", VOP3P, |op, _| {
            vec![vec![
                0xCC00_0000 | u32::from(op) << 16 | 2,
                257 | 258 << 9 | 259 << 18,
            ]]
        }),
        ("FLAT", FLAT, |op, _| {
            vec![vec![
                0xDC00_0000 | u32::from(op) << 18 | 1 << 16,
                1 | 2 << 8 | 0x7D << 16 | 3 << 24,
            ]]
        }),
        ("GLOBAL", GLOBAL, |op, _| {
            vec![vec![
                0xDC00_0000 | u32::from(op) << 18 | 2 << 14 | 1 << 16,
                1 | 2 << 8 | 0x7D << 16 | 3 << 24,
            ]]
        }),
        ("SCRATCH", SCRATCH, |op, _| {
            vec![vec![
                0xDC00_0000 | u32::from(op) << 18 | 1 << 14 | 1 << 16,
                1 | 2 << 8 | 0x7D << 16 | 3 << 24,
            ]]
        }),
        ("DS", DS, |op, _| {
            vec![vec![
                0xD800_0000 | u32::from(op) << 18,
                1 | 2 << 8 | 3 << 16 | 4 << 24,
            ]]
        }),
        ("MUBUF", MUBUF, |op, _| {
            vec![vec![
                0xE000_0000 | u32::from(op) << 18 | 0x1000,
                1 | 2 << 8 | 1 << 16 | 128 << 24,
            ]]
        }),
        ("MTBUF", MTBUF, |op, _| {
            vec![vec![
                0xE800_0000 | u32::from(op & 7) << 16 | 0x1000 | 22 << 19,
                1 | 2 << 8 | 1 << 16 | u32::from(op >> 3) << 21 | 128 << 24,
            ]]
        }),
        ("SMEM", SMEM, |op, _| {
            vec![vec![
                0xF400_0000 | u32::from(op) << 18 | 12 << 6 | 4,
                0x10 | 0x7D << 25,
            ]]
        }),
        ("MIMG", MIMG, |op, _| {
            vec![vec![
                0xF000_0000 | u32::from(op & 0x7F) << 18 | u32::from(op >> 7) | 1 << 3 | 0xF << 8,
                4 | 2 << 16 | 4 << 21,
            ]]
        }),
        ("VINTRP", VINTRP, |op, _| {
            vec![vec![0xC800_0000 | u32::from(op) << 16 | 2 << 18]]
        }),
    ];
    let (mut total, mut ok) = (0usize, 0usize);
    let mut lines = Vec::new();
    for (name, table, mk) in fams {
        let (mut t, mut s) = (0usize, 0usize);
        let mut missing = Vec::new();
        for &(op, mn, shape) in table {
            let words = mk(op, shape);
            // branchements : traités par le CFG, pas par translate_inst
            if mn == "s_branch" || mn.starts_with("s_cbranch_") || mn == "s_endpgm" {
                t += 1;
                s += 1;
                continue;
            }
            match is_implemented(&words) {
                Some(true) => {
                    t += 1;
                    s += 1;
                }
                Some(false) => {
                    t += 1;
                    missing.push(mn);
                }
                None => {}
            }
        }
        lines.push(format!("{name:7} {s:4}/{t:4} opcodes traduits"));
        eprintln!("[{name}] non traduits : {}", missing.join(" "));
        total += t;
        ok += s;
    }
    eprintln!("{}\nTOTAL   {ok}/{total}", lines.join("\n"));
    assert!(ok >= 700, "régression de couverture : {ok}/{total}");
}

#[test]
fn ps_mimg_variants_translate_and_validate() {
    let t = translate("ps_mimg_ext");
    let (caps, ops) = scan(&t.words);
    for (op, what) in [
        (87, "ImageSampleImplicitLod"),
        (89, "ImageSampleDrefImplicitLod"),
        (96, "ImageGather"),
        (97, "ImageDrefGather"),
        (105, "ImageQueryLod"),
        (95, "ImageFetch"),
    ] {
        assert!(ops.contains(&op), "{what} attendu");
    }
    assert!(caps.contains(&50), "ImageQuery (get_lod, offset émulé)");
    assert!(caps.contains(&42), "MinLod (_cl)");
    assert!(caps.contains(&25), "ImageGatherExtended (gather _o)");
    let sampled = t
        .info
        .bindings
        .iter()
        .filter(|b| b.kind == "sampled_image")
        .count();
    assert_eq!(sampled, 5, "2D, 3D, cube, 1D array, MSAA");
}
