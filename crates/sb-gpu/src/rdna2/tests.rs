//! Tests unitaires du décodeur/désassembleur : vecteurs figés issus de `llvm-mc` (gfx1030).

use std::collections::BTreeSet;

use super::test_vectors::VECTORS;
use super::*;

fn wave(v: u8) -> WaveSize {
    if v == 64 {
        WaveSize::W64
    } else {
        WaveSize::W32
    }
}

fn dec(words: &[u32], w: WaveSize) -> Instruction {
    decode(words, w).expect("décodage")
}

/// Cherche le vecteur dont le texte canonique est `text`.
fn vec_of(text: &str) -> &'static super::test_vectors::Vector {
    VECTORS
        .iter()
        .find(|v| v.text == text)
        .unwrap_or_else(|| panic!("vecteur absent : {text}"))
}

#[test]
fn vectors_roundtrip_to_llvm_text() {
    let mut fails = Vec::new();
    for v in VECTORS {
        let w = wave(v.wave);
        match decode(v.words, w) {
            Ok(i) => {
                let t = disassemble(&i, w);
                if t != v.text || usize::from(i.words) != v.words.len() {
                    fails.push(format!("{} => {t} (len {})", v.text, i.words));
                }
            }
            Err(e) => fails.push(format!("{} => erreur {e}", v.text)),
        }
    }
    assert!(
        fails.is_empty(),
        "{} divergences :\n{}",
        fails.len(),
        fails.join("\n")
    );
}

#[test]
fn vectors_cover_at_least_150_instructions_and_all_families() {
    assert!(VECTORS.len() >= 150, "seulement {} vecteurs", VECTORS.len());
    let fams: BTreeSet<String> = VECTORS
        .iter()
        .map(|v| format!("{:?}", dec(v.words, wave(v.wave)).encoding))
        .collect();
    for f in [
        "Sop1", "Sop2", "Sopk", "Sopc", "Sopp", "Smem", "Vop1", "Vop2", "Vopc", "Vop3", "Vop3p",
        "Vintrp", "Ds", "Mubuf", "Mtbuf", "Mimg", "Exp", "Flat", "Scratch", "Global",
    ] {
        assert!(fams.contains(f), "famille {f} non couverte");
    }
}

#[test]
fn stream_decode_walks_literals_and_wide_words() {
    // s_mov_b32 s0, 0x12345678 ; v_add_f32 v1, v0, v0 ; s_endpgm
    let words = [0xBE80_03FF, 0x1234_5678, 0x0602_0100, 0xBF81_0000];
    let d = decode_stream(&words, WaveSize::W32).unwrap();
    assert_eq!(d.len(), 3);
    assert_eq!((d[0].offset, d[1].offset, d[2].offset), (0, 2, 3));
    assert_eq!(d[0].inst.operands[1].src, Src::Literal(0x1234_5678));
    assert_eq!(d[0].inst.literal(), Some(0x1234_5678));
    assert_eq!(d[2].inst.name, "s_endpgm");
    let text = disassemble_stream(&words, WaveSize::W32).unwrap();
    assert!(text.contains("0000: s_mov_b32 s0, 0x12345678"), "{text}");
    assert!(text.contains("000c: s_endpgm"), "{text}");
}

#[test]
fn truncated_streams_and_unknown_encodings_are_errors() {
    assert_eq!(decode(&[], WaveSize::W32), Err(DecodeError::Truncated(0)));
    // VOP3 sans second dword
    let e = decode(&[0xD580_0000], WaveSize::W32).unwrap_err();
    assert_eq!(e, DecodeError::Truncated(1));
    // littéral manquant
    assert!(decode(&[0xBE80_03FF], WaveSize::W32).is_err());
    // préfixe 0x3B (111011) inexistant
    assert!(matches!(
        decode(&[0xEC00_0000, 0], WaveSize::W32),
        Err(DecodeError::UnknownEncoding(_))
    ));
    // opcode SOP2 non défini (0x5f)
    assert!(matches!(
        decode(&[0x8000_0000 | (0x5f << 23)], WaveSize::W32),
        Err(DecodeError::UnknownOpcode { .. })
    ));
}

#[test]
fn sop2_fields_and_special_operands() {
    let v = vec_of("s_and_b64 s[2:3], vcc, exec");
    let i = dec(v.words, WaveSize::W32);
    assert_eq!(i.encoding, Encoding::Sop2);
    assert_eq!(i.n_dst, 1);
    assert_eq!(i.dst().unwrap().src, Src::Sgpr(2));
    assert_eq!(i.dst().unwrap().width, 2);
    assert_eq!(i.srcs()[0].src, Src::VccLo);
    assert_eq!(i.srcs()[1].src, Src::ExecLo);
    let i = dec(vec_of("s_sub_u32 s3, s4, 5").words, WaveSize::W32);
    assert_eq!(i.srcs()[1].src, Src::InlineInt(5));
    let i = dec(vec_of("s_mov_b32 m0, s0").words, WaveSize::W32);
    assert_eq!(i.dst().unwrap().src, Src::M0);
}

#[test]
fn vop3_modifiers_are_typed() {
    let i = dec(
        vec_of("v_add_f32_e64 v0, -v1, |v2| clamp mul:2").words,
        WaveSize::W32,
    );
    assert_eq!(i.encoding, Encoding::Vop3);
    assert_eq!(i.base_name(), "v_add_f32");
    assert!(i.mods.clamp);
    assert_eq!(i.mods.omod, 1);
    assert!(i.srcs()[0].neg && !i.srcs()[0].abs);
    assert!(i.srcs()[1].abs && !i.srcs()[1].neg);
    let i = dec(
        vec_of("v_fma_f16 v0, v1, v2, v3 op_sel:[1,0,0,0]").words,
        WaveSize::W32,
    );
    assert_eq!(i.mods.op_sel, 1);
    let i = dec(
        vec_of("v_fma_f32 v0, 0x40490fdb, v1, v2").words,
        WaveSize::W32,
    );
    assert_eq!(i.words, 3, "VOP3 + littéral = 3 dwords");
    assert_eq!(i.srcs()[0].src, Src::Literal(0x4049_0fdb));
}

#[test]
fn vop3p_op_sel_and_neg() {
    let i = dec(
        vec_of("v_pk_fma_f16 v0, v1, v2, v3 op_sel_hi:[1,1,0]").words,
        WaveSize::W32,
    );
    assert_eq!(i.encoding, Encoding::Vop3p);
    assert_eq!(i.mods.op_sel_hi, 0b011);
    let i = dec(
        vec_of("v_pk_add_u16 v0, v1, v2 neg_lo:[1,0]").words,
        WaveSize::W32,
    );
    assert_eq!(i.mods.neg_lo, 1);
}

#[test]
fn wave_size_changes_mask_operand_width() {
    let c32 = dec(
        vec_of("v_cmp_lt_f32_e32 vcc_lo, v0, v1").words,
        WaveSize::W32,
    );
    assert_eq!(c32.dst().unwrap().width, 1);
    let words = vec_of("v_cmp_lt_f32_e32 vcc_lo, v0, v1").words;
    let c64 = dec(words, WaveSize::W64);
    assert_eq!(c64.dst().unwrap().width, 2);
    assert_eq!(
        disassemble(&c64, WaveSize::W64),
        "v_cmp_lt_f32_e32 vcc, v0, v1"
    );
    let i = dec(
        vec_of("v_cndmask_b32_e64 v0, v1, v2, s0").words,
        WaveSize::W32,
    );
    assert_eq!(i.srcs()[2].width, 1);
    let i = dec(
        vec_of("v_cndmask_b32_e64 v0, v1, v2, s[2:3]").words,
        WaveSize::W64,
    );
    assert_eq!(i.srcs()[2].width, 2);
    assert_eq!(i.srcs()[2].src, Src::Sgpr(2));
}

#[test]
fn memory_fields_are_decoded() {
    let i = dec(
        vec_of("buffer_load_dwordx4 v[0:3], v1, s[4:7], s8 idxen offset:4 glc").words,
        WaveSize::W32,
    );
    assert_eq!(i.encoding, Encoding::Mubuf);
    assert!(i.mods.idxen && !i.mods.offen && i.mods.glc);
    assert_eq!(i.mods.offset, 4);
    assert_eq!(i.operands[0].width, 4);
    assert_eq!(i.operands[2].src, Src::Sgpr(4));
    assert_eq!(i.operands[3].src, Src::Sgpr(8));
    assert_eq!(i.n_dst, 1);
    let i = dec(
        vec_of("buffer_store_dword v0, v1, s[4:7], 0 offen").words,
        WaveSize::W32,
    );
    assert_eq!(i.n_dst, 0);
    assert!(i.mods.offen);
    let i = dec(
        vec_of("ds_write2_b32 v1, v2, v3 offset0:4 offset1:8").words,
        WaveSize::W32,
    );
    assert_eq!((i.mods.offset0, i.mods.offset1), (4, 8));
    let i = dec(
        vec_of("s_buffer_load_dword s0, s[4:7], 0x20").words,
        WaveSize::W32,
    );
    assert_eq!(i.mods.offset, 0x20);
    assert_eq!(i.operands[1].width, 4);
    let i = dec(
        vec_of("global_load_dwordx4 v[0:3], v[4:5], off offset:-16").words,
        WaveSize::W32,
    );
    assert_eq!(i.encoding, Encoding::Global);
    assert_eq!(i.mods.offset, -16);
}

#[test]
fn mimg_and_exp_fields() {
    let i = dec(
        vec_of("image_sample_l v[0:3], v[4:6], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D")
            .words,
        WaveSize::W32,
    );
    assert_eq!(i.encoding, Encoding::Mimg);
    assert_eq!(i.mods.dmask, 0xF);
    assert_eq!(i.mods.dim, 1);
    assert_eq!(i.operands[1].width, 3, "2 coordonnées + lod");
    assert_eq!(i.operands[2].width, 8);
    assert_eq!(i.operands[3].width, 4);
    let i = dec(
        vec_of("image_sample v[0:3], [v4, v6], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D")
            .words,
        WaveSize::W32,
    );
    assert_eq!(i.mods.nsa, 1);
    assert_eq!(i.words, 3);
    assert_eq!(i.operands[1].src, Src::Vgpr(4));
    assert_eq!(i.operands[2].src, Src::Vgpr(6));
    let i = dec(vec_of("exp pos0, v0, v1, v2, v3 done").words, WaveSize::W32);
    assert_eq!(i.mods.target, 12);
    assert_eq!(i.mods.en, 0xF);
    assert!(i.mods.done);
    let i = dec(vec_of("exp mrt0, v0, off, v1, off").words, WaveSize::W32);
    assert_eq!(i.mods.en, 0b0101);
    assert_eq!(i.operands[1].src, Src::Off);
}

#[test]
fn v_nop_e64_with_literal_preamble_decodes_three_words() {
    // Préambule RDNA 2 console : `v_nop_e64` (VOP3 opcode 0x180, src2=0xff -> littéral 32 bits)
    // suivi de `s_inst_prefetch 3` et `s_endpgm`.
    let words = [
        0xd580_0000u32,
        0x03fe_0100,
        0x0000_0ac0,
        0xbfa0_0003,
        0xbf81_0000,
    ];
    let stream = decode_stream(&words, WaveSize::W32).expect("v_nop_e64 + s_inst_prefetch");
    assert_eq!(stream.len(), 3);
    assert_eq!(stream[0].inst.name, "v_nop_e64");
    assert_eq!(stream[0].inst.base_name(), "v_nop");
    assert_eq!(stream[0].inst.words, 3);
    assert_eq!(stream[1].inst.name, "s_inst_prefetch");
    assert_eq!(stream[2].inst.name, "s_endpgm");
}

#[test]
fn inline_constants_decode_and_print() {
    let i = dec(vec_of("v_mov_b32_e32 v0, 1.0").words, WaveSize::W32);
    assert_eq!(i.srcs()[0].src, Src::InlineFloat(242));
    assert_eq!(disassemble(&i, WaveSize::W32), "v_mov_b32_e32 v0, 1.0");
    let i = dec(vec_of("s_mov_b32 s0, -1").words, WaveSize::W32);
    assert_eq!(i.srcs()[0].src, Src::InlineInt(-1));
}

#[test]
fn waitcnt_printing() {
    let i = dec(vec_of("s_waitcnt vmcnt(0) lgkmcnt(0)").words, WaveSize::W32);
    assert_eq!(i.mods.imm & 0xF, 0);
    assert!(!i.is_branch());
    let i = dec(vec_of("s_cbranch_scc0 3").words, WaveSize::W32);
    assert!(i.is_branch());
    assert_eq!(i.mods.imm, 3);
}

#[test]
fn opcode_tables_are_sorted_and_unique() {
    use super::tables::*;
    for (n, t) in [
        ("SOP2", SOP2),
        ("SOP1", SOP1),
        ("SOPC", SOPC),
        ("SOPK", SOPK),
        ("SOPP", SOPP),
        ("VOP2", VOP2),
        ("VOP1", VOP1),
        ("VOPC", VOPC),
        ("VOP3", VOP3),
        ("VOP3P", VOP3P),
        ("DS", DS),
        ("MUBUF", MUBUF),
        ("MTBUF", MTBUF),
        ("SMEM", SMEM),
        ("FLAT", FLAT),
        ("SCRATCH", SCRATCH),
        ("GLOBAL", GLOBAL),
        ("MIMG", MIMG),
        ("VINTRP", VINTRP),
    ] {
        assert!(!t.is_empty(), "{n} vide");
        assert!(t.windows(2).all(|w| w[0].0 < w[1].0), "{n} non trié/unique");
    }
}
