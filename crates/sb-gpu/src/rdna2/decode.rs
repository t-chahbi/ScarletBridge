//! Décodeur binaire RDNA 2 (gfx10.3) : mots 32 bits → [`Instruction`].
//!
//! La reconnaissance de la famille d'encodage suit les préfixes de bits du guide ISA public AMD ;
//! les noms d'opcodes et les formes d'opérandes viennent de [`super::tables`] (générées à partir
//! de l'assembleur LLVM). Les champs propres à chaque famille sont extraits à la main ici.

use super::inst::{DecodeError, Encoding, Ext, Instruction, Mods, Operand, Src, WaveSize};
use super::tables::{self, OpDef};

/// Instruction + position (en dwords) dans le flux.
#[derive(Clone, Debug, PartialEq)]
pub struct Decoded {
    pub offset: usize,
    pub inst: Instruction,
}

fn lookup(table: &'static [OpDef], op: u16) -> Option<&'static OpDef> {
    table
        .binary_search_by_key(&op, |e| e.0)
        .ok()
        .map(|i| &table[i])
}

/// Découpe un slot de forme (`"D1"`, `"VM"`, `"d2"`) en (lettre, largeur) ; `M` → largeur masque.
fn slot(tok: &str, wave: WaveSize) -> (u8, u8) {
    let b = tok.as_bytes();
    let w = if b[1] == b'M' {
        wave.mask_dwords()
    } else {
        tok[1..].parse().unwrap_or(0)
    };
    (b[0], w)
}

/// Champs bruts extraits d'une instruction, consommés par le constructeur d'opérandes.
#[derive(Default)]
struct Fields {
    /// Code du champ `D`/`d` (8 bits).
    dst: u16,
    sdst: u16,
    src: [u16; 3],
    /// Les sources A/B/C sont-elles des registres VGPR 8 bits (VOP2/VOPC `vsrc1`) ?
    b_is_vgpr: bool,
    /// Classe du champ D : VGPR (vrai) ou code scalaire.
    d_is_vgpr: bool,
    literal: Option<u32>,
    neg: u8,
    abs: u8,
    addr: u16,
    data0: u16,
    data1: u16,
    srsrc: u16,
    soffset: u16,
}

/// Construit les opérandes depuis la forme ; retourne `(operands, n_dst)`.
fn build_operands(shape: &str, wave: WaveSize, f: &Fields) -> (Vec<Operand>, u8) {
    let mut ops = Vec::new();
    let mut n_dst = 0u8;
    let mut seen_src = false;
    for tok in shape.split_ascii_whitespace() {
        let (l, w) = slot(tok, wave);
        let is_src_slot = !matches!(l, b'D' | b'd' | b'E' | b'V');
        let src = match l {
            b'D' => {
                if f.d_is_vgpr {
                    Some(Src::Vgpr(f.dst as u8))
                } else {
                    Some(Src::from_code(f.dst & 0x7F, None))
                }
            }
            b'd' => Some(Src::from_code(f.dst & 0x7F, None)),
            b'E' => Some(Src::from_code(f.sdst & 0x7F, None)),
            b'A' => Some(Src::from_code(f.src[0], f.literal)),
            b'B' => Some(if f.b_is_vgpr {
                Src::Vgpr(f.src[1] as u8)
            } else {
                Src::from_code(f.src[1], f.literal)
            }),
            b'C' => Some(Src::from_code(f.src[2], f.literal)),
            b'K' => Some(Src::Literal(f.literal.unwrap_or(0))),
            b'V' => Some(Src::VccLo),
            b'Y' => Some(Src::Vgpr(f.addr as u8)),
            b'P' => Some(Src::Vgpr(f.data0 as u8)),
            b'Q' => Some(Src::Vgpr(f.data1 as u8)),
            b'R' => Some(Src::Sgpr(f.srsrc as u8)),
            b'O' => Some(Src::from_code(f.soffset, f.literal)),
            _ => None,
        };
        let Some(src) = src else { continue };
        if is_src_slot {
            seen_src = true;
        } else if !seen_src {
            n_dst += 1;
        }
        let mut op = Operand::new(src, w);
        let idx = match l {
            b'A' => Some(0),
            b'B' => Some(1),
            b'C' => Some(2),
            _ => None,
        };
        if let Some(i) = idx {
            op.neg = f.neg >> i & 1 != 0;
            op.abs = f.abs >> i & 1 != 0;
        }
        ops.push(op);
    }
    (ops, n_dst)
}

fn needs_literal_codes(codes: &[u16]) -> bool {
    codes.contains(&255)
}

fn sext(v: u32, bits: u32) -> i32 {
    let s = 32 - bits;
    ((v << s) as i32) >> s
}

/// Décode l'instruction au début de `words`.
pub fn decode(words: &[u32], wave: WaveSize) -> Result<Instruction, DecodeError> {
    let w0 = *words.first().ok_or(DecodeError::Truncated(0))?;
    let w1 = words.get(1).copied();
    let need1 = || w1.ok_or(DecodeError::Truncated(1));
    let unknown = |encoding, opcode| DecodeError::UnknownOpcode { encoding, opcode };

    // --- scalaire 32 bits ---
    if w0 >> 23 == 0x17D {
        return decode_sop1(words, wave)
            .ok_or_else(|| unknown(Encoding::Sop1, ((w0 >> 8) & 0xFF) as u16));
    }
    if w0 >> 23 == 0x17E {
        return decode_sopc(words, wave)
            .ok_or_else(|| unknown(Encoding::Sopc, ((w0 >> 16) & 0x7F) as u16));
    }
    if w0 >> 23 == 0x17F {
        return decode_sopp(w0).ok_or_else(|| unknown(Encoding::Sopp, ((w0 >> 16) & 0x7F) as u16));
    }
    if w0 >> 28 == 0xB {
        return decode_sopk(w0).ok_or_else(|| unknown(Encoding::Sopk, ((w0 >> 23) & 0x1F) as u16));
    }
    if w0 >> 30 == 0b10 {
        return decode_sop2(words, wave)
            .ok_or_else(|| unknown(Encoding::Sop2, ((w0 >> 23) & 0x7F) as u16));
    }
    // --- vectoriel 32 bits ---
    if w0 >> 31 == 0 {
        return decode_vop32(words, wave);
    }
    // --- 64 bits ---
    match w0 >> 26 {
        0x35 => decode_vop3(words, wave, false),
        0x33 if (w0 >> 24) == 0xCC => decode_vop3(words, wave, true),
        0x32 => decode_vintrp(w0),
        0x36 => decode_ds(w0, need1()?, wave),
        0x37 => decode_flat(w0, need1()?, wave),
        0x38 => decode_mubuf(w0, need1()?, wave),
        0x3A => decode_mtbuf(w0, need1()?, wave),
        0x3C => decode_mimg(words, wave),
        0x3D => decode_smem(w0, need1()?, wave),
        0x3E => decode_exp(w0, need1()?),
        _ => Err(DecodeError::UnknownEncoding(w0)),
    }
}

fn finish(
    encoding: Encoding,
    def: &'static OpDef,
    words: u8,
    shape: &str,
    wave: WaveSize,
    f: &Fields,
    mods: Mods,
) -> Instruction {
    let (operands, n_dst) = build_operands(shape, wave, f);
    Instruction {
        encoding,
        opcode: def.0,
        name: def.1,
        words,
        n_dst,
        operands,
        mods,
    }
}

fn decode_sop2(words: &[u32], wave: WaveSize) -> Option<Instruction> {
    let w0 = words[0];
    let def = lookup(tables::SOP2, ((w0 >> 23) & 0x7F) as u16)?;
    let s0 = (w0 & 0xFF) as u16;
    let s1 = ((w0 >> 8) & 0xFF) as u16;
    let lit = needs_literal_codes(&[s0, s1]);
    let f = Fields {
        dst: ((w0 >> 16) & 0x7F) as u16,
        src: [s0, s1, 0],
        literal: if lit { words.get(1).copied() } else { None },
        ..Default::default()
    };
    if lit && words.len() < 2 {
        return None;
    }
    Some(finish(
        Encoding::Sop2,
        def,
        1 + u8::from(lit),
        def.2,
        wave,
        &f,
        Mods::default(),
    ))
}

fn decode_sop1(words: &[u32], wave: WaveSize) -> Option<Instruction> {
    let w0 = words[0];
    let def = lookup(tables::SOP1, ((w0 >> 8) & 0xFF) as u16)?;
    let s0 = (w0 & 0xFF) as u16;
    let lit = s0 == 255;
    if lit && words.len() < 2 {
        return None;
    }
    let f = Fields {
        dst: ((w0 >> 16) & 0x7F) as u16,
        src: [s0, 0, 0],
        literal: if lit { words.get(1).copied() } else { None },
        ..Default::default()
    };
    Some(finish(
        Encoding::Sop1,
        def,
        1 + u8::from(lit),
        def.2,
        wave,
        &f,
        Mods::default(),
    ))
}

fn decode_sopc(words: &[u32], wave: WaveSize) -> Option<Instruction> {
    let w0 = words[0];
    let def = lookup(tables::SOPC, ((w0 >> 16) & 0x7F) as u16)?;
    let s0 = (w0 & 0xFF) as u16;
    let s1 = ((w0 >> 8) & 0xFF) as u16;
    let lit = needs_literal_codes(&[s0, s1]);
    if lit && words.len() < 2 {
        return None;
    }
    let f = Fields {
        src: [s0, s1, 0],
        literal: if lit { words.get(1).copied() } else { None },
        ..Default::default()
    };
    Some(finish(
        Encoding::Sopc,
        def,
        1 + u8::from(lit),
        def.2,
        wave,
        &f,
        Mods::default(),
    ))
}

fn decode_sopk(w0: u32) -> Option<Instruction> {
    let def = lookup(tables::SOPK, ((w0 >> 23) & 0x1F) as u16)?;
    let f = Fields {
        dst: ((w0 >> 16) & 0x7F) as u16,
        ..Default::default()
    };
    let mods = Mods {
        imm: (w0 & 0xFFFF) as i32,
        ..Default::default()
    };
    // Le registre de SOPK est une source pour s_cmpk_* : n_dst géré par la forme (D = dst).
    let mut inst = finish(Encoding::Sopk, def, 1, def.2, WaveSize::W32, &f, mods);
    if def.1.starts_with("s_cmpk_") || def.1 == "s_setreg_b32" || def.1 == "s_waitcnt_vscnt" {
        inst.n_dst = 0;
    }
    Some(inst)
}

fn decode_sopp(w0: u32) -> Option<Instruction> {
    let def = lookup(tables::SOPP, ((w0 >> 16) & 0x7F) as u16)?;
    let simm = (w0 & 0xFFFF) as u16;
    let mods = Mods {
        imm: if def.1 == "s_branch" || def.1.starts_with("s_cbranch_") {
            i32::from(simm as i16)
        } else {
            i32::from(simm)
        },
        ..Default::default()
    };
    Some(Instruction {
        encoding: Encoding::Sopp,
        opcode: def.0,
        name: def.1,
        words: 1,
        n_dst: 0,
        operands: Vec::new(),
        mods,
    })
}

fn decode_vop32(words: &[u32], wave: WaveSize) -> Result<Instruction, DecodeError> {
    let w0 = words[0];
    let (enc, table, op, dst, vsrc1) = match w0 >> 25 {
        0x3F => (
            Encoding::Vop1,
            tables::VOP1,
            (w0 >> 9) & 0xFF,
            (w0 >> 17) & 0xFF,
            0,
        ),
        0x3E => (
            Encoding::Vopc,
            tables::VOPC,
            (w0 >> 17) & 0xFF,
            0,
            (w0 >> 9) & 0xFF,
        ),
        _ => (
            Encoding::Vop2,
            tables::VOP2,
            (w0 >> 25) & 0x3F,
            (w0 >> 17) & 0xFF,
            (w0 >> 9) & 0xFF,
        ),
    };
    let def = lookup(table, op as u16).ok_or(DecodeError::UnknownOpcode {
        encoding: enc,
        opcode: op as u16,
    })?;
    let s0 = (w0 & 0x1FF) as u16;
    let mut mods = Mods::default();
    let mut words_n = 1u8;
    let mut f = Fields {
        dst: dst as u16,
        src: [s0, vsrc1 as u16, 0],
        b_is_vgpr: true,
        d_is_vgpr: true,
        ..Default::default()
    };
    // Extensions SDWA/DPP : src0 ∈ {249 SDWA, 250 DPP16, 233/234 DPP8}.
    let ext = match s0 {
        249 => Ext::Sdwa,
        250 => Ext::Dpp16,
        233 | 234 => Ext::Dpp8,
        _ => Ext::None,
    };
    if ext != Ext::None {
        let e = *words.get(1).ok_or(DecodeError::Truncated(1))?;
        mods.ext = ext;
        mods.ext_raw = e;
        mods.dpp8_fi = s0 == 234;
        f.src[0] = 256 + (e & 0xFF) as u16;
        if ext == Ext::Sdwa {
            if e >> 23 & 1 != 0 {
                f.src[0] = (e & 0xFF) as u16; // src0 scalaire
            }
            if e >> 31 & 1 != 0 {
                f.b_is_vgpr = false;
            }
        }
        words_n = 2;
    } else if s0 == 255 || def.2.contains('K') {
        f.literal = Some(*words.get(1).ok_or(DecodeError::Truncated(1))?);
        words_n = 2;
    }
    let mut inst = finish(enc, def, words_n, def.2, wave, &f, mods);
    // neg/abs de src0/src1 portés par le second dword
    let (n0, a0, n1, a1) = match inst.mods.ext {
        Ext::Sdwa => {
            let s = inst.mods.sdwa().unwrap_or_default();
            (s.src0_neg, s.src0_abs, s.src1_neg, s.src1_abs)
        }
        Ext::Dpp16 => {
            let d = inst.mods.dpp16().unwrap_or_default();
            (d.src0_neg, d.src0_abs, d.src1_neg, d.src1_abs)
        }
        _ => (false, false, false, false),
    };
    let nd = usize::from(inst.n_dst);
    if let Some(o) = inst.operands.get_mut(nd) {
        o.neg = n0;
        o.abs = a0;
    }
    if let Some(o) = inst.operands.get_mut(nd + 1) {
        o.neg = n1;
        o.abs = a1;
    }
    Ok(inst)
}

fn decode_vop3(words: &[u32], wave: WaveSize, packed: bool) -> Result<Instruction, DecodeError> {
    let w0 = words[0];
    let w1 = *words.get(1).ok_or(DecodeError::Truncated(1))?;
    let (enc, table, op) = if packed {
        (Encoding::Vop3p, tables::VOP3P, (w0 >> 16) & 0x7F)
    } else {
        (Encoding::Vop3, tables::VOP3, (w0 >> 16) & 0x3FF)
    };
    let def = lookup(table, op as u16).ok_or(DecodeError::UnknownOpcode {
        encoding: enc,
        opcode: op as u16,
    })?;
    let s = [
        (w1 & 0x1FF) as u16,
        ((w1 >> 9) & 0x1FF) as u16,
        ((w1 >> 18) & 0x1FF) as u16,
    ];
    let has_sdst = def.2.split(' ').any(|t| t.starts_with('E'));
    let lit = needs_literal_codes(&s);
    let literal = if lit {
        Some(*words.get(2).ok_or(DecodeError::Truncated(2))?)
    } else {
        None
    };
    let mut f = Fields {
        dst: (w0 & 0xFF) as u16,
        sdst: ((w0 >> 8) & 0x7F) as u16,
        src: s,
        d_is_vgpr: true,
        literal,
        neg: (w1 >> 29) as u8 & 7,
        ..Default::default()
    };
    let mut mods = Mods {
        clamp: w0 >> 15 & 1 != 0,
        ..Default::default()
    };
    if packed {
        mods.op_sel = ((w0 >> 11) & 7) as u8;
        mods.op_sel_hi = (((w1 >> 27) & 3) | (((w0 >> 14) & 1) << 2)) as u8;
        mods.neg_lo = f.neg;
        mods.neg_hi = ((w0 >> 8) & 7) as u8;
        f.neg = 0;
    } else {
        mods.omod = ((w1 >> 27) & 3) as u8;
        mods.op_sel = ((w0 >> 11) & 0xF) as u8;
        if !has_sdst {
            f.abs = ((w0 >> 8) & 7) as u8;
        }
    }
    Ok(finish(enc, def, 2 + u8::from(lit), def.2, wave, &f, mods))
}

fn decode_vintrp(w0: u32) -> Result<Instruction, DecodeError> {
    let op = (w0 >> 16) & 3;
    let def = lookup(tables::VINTRP, op as u16).ok_or(DecodeError::UnknownOpcode {
        encoding: Encoding::Vintrp,
        opcode: op as u16,
    })?;
    let mods = Mods {
        attr: ((w0 >> 10) & 0x3F) as u8,
        attr_chan: ((w0 >> 8) & 3) as u8,
        ..Default::default()
    };
    let vdst = ((w0 >> 18) & 0xFF) as u8;
    let vsrc = (w0 & 0xFF) as u8;
    let mut operands = vec![Operand::new(Src::Vgpr(vdst), 1)];
    if def.1.starts_with("v_interp_mov") {
        // vsrc = paramètre (p10/p20/p0) : sélection brute dans `imm`.
        operands.push(Operand::new(Src::Special(u16::from(vsrc)), 1));
    } else {
        operands.push(Operand::new(Src::Vgpr(vsrc), 1));
    }
    Ok(Instruction {
        encoding: Encoding::Vintrp,
        opcode: def.0,
        name: def.1,
        words: 1,
        n_dst: 1,
        operands,
        mods,
    })
}

fn decode_ds(w0: u32, w1: u32, wave: WaveSize) -> Result<Instruction, DecodeError> {
    let op = ((w0 >> 18) & 0xFF) as u16;
    let def = lookup(tables::DS, op).ok_or(DecodeError::UnknownOpcode {
        encoding: Encoding::Ds,
        opcode: op,
    })?;
    let f = Fields {
        dst: (w1 >> 24) as u16,
        d_is_vgpr: true,
        addr: (w1 & 0xFF) as u16,
        data0: ((w1 >> 8) & 0xFF) as u16,
        data1: ((w1 >> 16) & 0xFF) as u16,
        ..Default::default()
    };
    let mods = Mods {
        offset: (w0 & 0xFFFF) as i32,
        offset0: (w0 & 0xFF) as u8,
        offset1: ((w0 >> 8) & 0xFF) as u8,
        gds: w0 >> 17 & 1 != 0,
        ..Default::default()
    };
    Ok(finish(Encoding::Ds, def, 2, def.2, wave, &f, mods))
}

/// Nombre de dwords de données d'après un slot de forme de tête (`D`/`P`).
fn first_width(shape: &str, wave: WaveSize) -> u8 {
    shape
        .split_ascii_whitespace()
        .next()
        .map(|t| slot(t, wave).1)
        .unwrap_or(1)
}

fn decode_mubuf(w0: u32, w1: u32, wave: WaveSize) -> Result<Instruction, DecodeError> {
    let op = ((w0 >> 18) & 0xFF) as u16;
    let def = lookup(tables::MUBUF, op).ok_or(DecodeError::UnknownOpcode {
        encoding: Encoding::Mubuf,
        opcode: op,
    })?;
    let mods = Mods {
        offset: (w0 & 0xFFF) as i32,
        offen: w0 >> 12 & 1 != 0,
        idxen: w0 >> 13 & 1 != 0,
        glc: w0 >> 14 & 1 != 0,
        dlc: w0 >> 15 & 1 != 0,
        lds: w0 >> 16 & 1 != 0,
        slc: w1 >> 22 & 1 != 0,
        tfe: w1 >> 23 & 1 != 0,
        ..Default::default()
    };
    Ok(mem_vbuf(Encoding::Mubuf, def, w1, mods, wave))
}

fn decode_mtbuf(w0: u32, w1: u32, wave: WaveSize) -> Result<Instruction, DecodeError> {
    let op = (((w0 >> 16) & 7) | (((w1 >> 21) & 1) << 3)) as u16;
    let def = lookup(tables::MTBUF, op).ok_or(DecodeError::UnknownOpcode {
        encoding: Encoding::Mtbuf,
        opcode: op,
    })?;
    let mods = Mods {
        offset: (w0 & 0xFFF) as i32,
        offen: w0 >> 12 & 1 != 0,
        idxen: w0 >> 13 & 1 != 0,
        glc: w0 >> 14 & 1 != 0,
        dlc: w0 >> 15 & 1 != 0,
        format: ((w0 >> 19) & 0x7F) as u8,
        slc: w1 >> 22 & 1 != 0,
        tfe: w1 >> 23 & 1 != 0,
        ..Default::default()
    };
    Ok(mem_vbuf(Encoding::Mtbuf, def, w1, mods, wave))
}

/// Opérandes communs MUBUF/MTBUF : vdata, vaddr (0, 1 ou 2 regs), srsrc, soffset.
fn mem_vbuf(
    encoding: Encoding,
    def: &'static OpDef,
    w1: u32,
    mods: Mods,
    wave: WaveSize,
) -> Instruction {
    let mut dw = first_width(def.2, wave);
    // Avec `lds`, la donnée va en LDS : plus de registre de données.
    let has_data = def.2.starts_with('D') && !mods.lds;
    if mods.tfe && has_data {
        dw += 1;
    }
    let mut operands = Vec::new();
    if has_data {
        operands.push(Operand::new(Src::Vgpr(((w1 >> 8) & 0xFF) as u8), dw));
    }
    if def.2.contains('Y') {
        let aw = if mods.offen && mods.idxen { 2 } else { 1 };
        if mods.offen || mods.idxen {
            operands.push(Operand::new(Src::Vgpr((w1 & 0xFF) as u8), aw));
        } else {
            operands.push(Operand::new(Src::Off, 0));
        }
    }
    if def.2.contains('R') {
        operands.push(Operand::new(
            Src::from_code((((w1 >> 16) & 0x1F) * 4) as u16, None),
            4,
        ));
    }
    if def.2.contains('O') {
        operands.push(Operand::new(Src::from_code((w1 >> 24) as u16, None), 1));
    }
    let n_dst = u8::from(has_data && mem_returns(def.1, mods.glc));
    Instruction {
        encoding,
        opcode: def.0,
        name: def.1,
        words: 2,
        n_dst,
        operands,
        mods,
    }
}

/// Vrai si l'instruction mémoire de nom `name` écrit un résultat dans son premier registre.
fn mem_returns(name: &str, glc: bool) -> bool {
    if name.contains("atomic") {
        glc
    } else {
        name.contains("load")
            || name.contains("sample")
            || name.contains("gather")
            || name.contains("get_")
    }
}

fn decode_smem(w0: u32, w1: u32, wave: WaveSize) -> Result<Instruction, DecodeError> {
    let op = ((w0 >> 18) & 0xFF) as u16;
    let def = lookup(tables::SMEM, op).ok_or(DecodeError::UnknownOpcode {
        encoding: Encoding::Smem,
        opcode: op,
    })?;
    let soff = (w1 >> 25) as u16;
    let mods = Mods {
        glc: w0 >> 16 & 1 != 0,
        dlc: w0 >> 14 & 1 != 0,
        offset: sext(w1 & 0x1F_FFFF, 21),
        smem_soffset: soff != 0x7D,
        imm: if def.1.starts_with("s_atc_probe") {
            ((w0 >> 6) & 0x7F) as i32
        } else {
            0
        },
        ..Default::default()
    };
    let sdata = ((w0 >> 6) & 0x7F) as u16;
    let sbase = ((w0 & 0x3F) * 2) as u16;
    let mut operands = Vec::new();
    for tok in def.2.split_ascii_whitespace() {
        let (l, w) = slot(tok, wave);
        match l {
            b'D' | b'd' => operands.push(Operand::new(Src::from_code(sdata, None), w)),
            b'B' => operands.push(Operand::new(Src::from_code(sbase, None), w)),
            _ => {}
        }
    }
    let n_dst = u8::from(def.2.starts_with(['D', 'd']));
    let has_off = def.2.contains('I');
    let mut inst = Instruction {
        encoding: Encoding::Smem,
        opcode: def.0,
        name: def.1,
        words: 2,
        n_dst,
        operands,
        mods,
    };
    if inst.mods.smem_soffset && has_off {
        inst.operands
            .push(Operand::new(Src::from_code(soff, None), 1));
    }
    Ok(inst)
}

fn decode_flat(w0: u32, w1: u32, wave: WaveSize) -> Result<Instruction, DecodeError> {
    let op = ((w0 >> 18) & 0x7F) as u16;
    let seg = (w0 >> 14) & 3;
    let (enc, table) = match seg {
        0 => (Encoding::Flat, tables::FLAT),
        1 => (Encoding::Scratch, tables::SCRATCH),
        2 => (Encoding::Global, tables::GLOBAL),
        _ => return Err(DecodeError::UnknownEncoding(w0)),
    };
    let def = lookup(table, op).ok_or(DecodeError::UnknownOpcode {
        encoding: enc,
        opcode: op,
    })?;
    let saddr = ((w1 >> 16) & 0x7F) as u16;
    let mods = Mods {
        offset: if enc == Encoding::Flat {
            (w0 & 0xFFF) as i32
        } else {
            sext(w0 & 0xFFF, 12)
        },
        dlc: w0 >> 12 & 1 != 0,
        lds: w0 >> 13 & 1 != 0,
        glc: w0 >> 16 & 1 != 0,
        slc: w0 >> 17 & 1 != 0,
        ..Default::default()
    };
    let mut operands = Vec::new();
    // Ordre d'affichage LLVM : vdst (si retour et pas de `lds`), addr, data, saddr.
    let d_w = def
        .2
        .split_ascii_whitespace()
        .next()
        .filter(|t| t.starts_with('D'))
        .map(|t| slot(t, wave).1);
    let returns = d_w.is_some() && mem_returns(def.1, mods.glc) && !mods.lds;
    if let (true, Some(w)) = (returns, d_w) {
        operands.push(Operand::new(Src::Vgpr((w1 >> 24) as u8), w));
    }
    let saddr_off = saddr == 0x7D;
    let addtid = def.1.ends_with("_addtid");
    let scratch_sgpr = enc == Encoding::Scratch && !saddr_off;
    if !addtid {
        if scratch_sgpr {
            operands.push(Operand::new(Src::Off, 0));
        } else {
            let addr_w = if enc == Encoding::Flat || (enc == Encoding::Global && saddr_off) {
                2
            } else {
                1
            };
            operands.push(Operand::new(Src::Vgpr((w1 & 0xFF) as u8), addr_w));
        }
    }
    // Donnée source : largeur lue dans la forme (`P<n>`), ou `D` pour les stores addtid.
    let data_w = def.2.split_ascii_whitespace().find_map(|t| {
        let (l, w) = slot(t, wave);
        (l == b'P').then_some(w)
    });
    if let Some(w) = data_w {
        operands.push(Operand::new(Src::Vgpr(((w1 >> 8) & 0xFF) as u8), w));
    }
    if enc != Encoding::Flat {
        operands.push(if saddr_off {
            Operand::new(Src::Off, 0)
        } else {
            let w = if enc == Encoding::Scratch { 1 } else { 2 };
            Operand::new(Src::from_code(saddr, None), w)
        });
    }
    Ok(Instruction {
        encoding: enc,
        opcode: def.0,
        name: def.1,
        words: 2,
        n_dst: u8::from(returns),
        operands,
        mods,
    })
}

/// Coordonnées d'adresse selon la dimension MIMG (0=1D .. 7=2D_MSAA_ARRAY).
pub(crate) fn mimg_coord_count(dim: u8) -> u8 {
    match dim {
        0 => 1,
        1 | 4 => 2,
        2 | 3 | 5 | 6 => 3,
        _ => 4,
    }
}

/// Nombre de dwords d'adresse (vaddr) d'une instruction MIMG.
///
/// Modèle : coordonnées + arguments supplémentaires (`_c` comparaison, `_b` biais, `_l` LOD,
/// `_cl` clamp, `_o` offset, `_d`/`_cd` dérivées). Avec `a16`, seules les coordonnées sont
/// empaquetées par paires ; avec une variante `_g16`, les dérivées le sont aussi.
pub(crate) fn mimg_addr_dwords(name: &str, dim: u8, a16: bool) -> u8 {
    let coords = mimg_coord_count(dim);
    let grad = match dim {
        0 | 4 => 1,
        2 => 3,
        _ => 2,
    };
    let mut n = if a16 { coords.div_ceil(2) } else { coords };
    if let Some(rest) = name
        .strip_prefix("image_sample")
        .or_else(|| name.strip_prefix("image_gather4"))
    {
        let g16 = rest.ends_with("_g16");
        for tok in rest.split('_').filter(|t| !t.is_empty()) {
            match tok {
                "c" | "cl" | "b" | "l" | "o" => n += 1,
                "d" | "cd" => {
                    n += if !g16 {
                        2 * grad
                    } else {
                        match grad {
                            1 => 1,
                            2 => 2,
                            _ => 4,
                        }
                    }
                }
                _ => {}
            }
        }
    } else if name.ends_with("_mip") {
        n += 1;
    } else if name.starts_with("image_get_resinfo") {
        n = 1;
    } else if name.starts_with("image_bvh") {
        n = 8;
    }
    n
}

fn decode_mimg(words: &[u32], _wave: WaveSize) -> Result<Instruction, DecodeError> {
    let w0 = words[0];
    let w1 = *words.get(1).ok_or(DecodeError::Truncated(1))?;
    let op = (((w0 & 1) << 7) | ((w0 >> 18) & 0x7F)) as u16;
    let def = lookup(tables::MIMG, op).ok_or(DecodeError::UnknownOpcode {
        encoding: Encoding::Mimg,
        opcode: op,
    })?;
    let nsa = ((w0 >> 1) & 3) as u8;
    let mods = Mods {
        dim: ((w0 >> 3) & 7) as u8,
        dlc: w0 >> 7 & 1 != 0,
        dmask: ((w0 >> 8) & 0xF) as u8,
        unorm: w0 >> 12 & 1 != 0,
        glc: w0 >> 13 & 1 != 0,
        r128: w0 >> 15 & 1 != 0,
        tfe: w0 >> 16 & 1 != 0,
        lwe: w0 >> 17 & 1 != 0,
        slc: w0 >> 25 & 1 != 0,
        a16: w1 >> 30 & 1 != 0,
        d16: w1 >> 31 & 1 != 0,
        nsa,
        ..Default::default()
    };
    let total = 2 + usize::from(nsa);
    if words.len() < total {
        return Err(DecodeError::Truncated(words.len()));
    }
    let name = def.1;
    let is_gather = name.starts_with("image_gather4");
    let is_bvh = name.starts_with("image_bvh");
    let is_store = name.starts_with("image_store");
    let is_atomic = name.contains("atomic");
    let is_resinfo = name.starts_with("image_get_");
    let n_comp = if is_gather {
        4
    } else {
        u8::try_from(mods.dmask.count_ones()).unwrap_or(1).max(1)
    };
    let mut dw = if mods.d16 { n_comp.div_ceil(2) } else { n_comp };
    dw += u8::from(mods.tfe);
    if is_gather && mods.d16 {
        dw = 2;
    }
    if is_bvh {
        dw = 4;
    }
    let aw = mimg_addr_dwords(name, mods.dim, mods.a16);
    let samp =
        (name.starts_with("image_sample") || is_gather || name == "image_get_lod") && !is_bvh;
    let mut operands = Vec::new();
    operands.push(Operand::new(Src::Vgpr(((w1 >> 8) & 0xFF) as u8), dw));
    let v0 = (w1 & 0xFF) as u8;
    if nsa == 0 {
        operands.push(Operand::new(Src::Vgpr(v0), aw));
    } else {
        // NSA : vaddr0 puis un octet par adresse dans les dwords suivants.
        let mut regs = vec![v0];
        'outer: for extra in &words[2..total] {
            for b in 0..4 {
                if regs.len() >= usize::from(aw) {
                    break 'outer;
                }
                regs.push(((extra >> (8 * b)) & 0xFF) as u8);
            }
        }
        for r in regs {
            operands.push(Operand::new(Src::Vgpr(r), 1));
        }
    }
    // LLVM n'affiche jamais un T# de 4 dwords : r128 ne change que le modificateur.
    let rw = if is_bvh { 4 } else { 8 };
    operands.push(Operand::new(
        Src::from_code((((w1 >> 16) & 0x1F) * 4) as u16, None),
        rw,
    ));
    if samp {
        operands.push(Operand::new(
            Src::from_code((((w1 >> 21) & 0x1F) * 4) as u16, None),
            4,
        ));
    }
    let n_dst =
        u8::from(!is_store && (mem_returns(name, mods.glc) || is_resinfo || is_atomic && mods.glc));
    Ok(Instruction {
        encoding: Encoding::Mimg,
        opcode: def.0,
        name,
        words: total as u8,
        n_dst,
        operands,
        mods,
    })
}

fn decode_exp(w0: u32, w1: u32) -> Result<Instruction, DecodeError> {
    let mods = Mods {
        en: (w0 & 0xF) as u8,
        target: ((w0 >> 4) & 0x3F) as u8,
        compr: w0 >> 10 & 1 != 0,
        done: w0 >> 11 & 1 != 0,
        vm: w0 >> 12 & 1 != 0,
        ..Default::default()
    };
    let mut operands = Vec::new();
    for i in 0..4 {
        let enabled = mods.en >> i & 1 != 0;
        // En mode compressé, les lanes 0/1 partagent vsrc0 et les lanes 2/3 vsrc1.
        let reg = if mods.compr { i / 2 } else { i };
        if enabled {
            operands.push(Operand::new(Src::Vgpr(((w1 >> (8 * reg)) & 0xFF) as u8), 1));
        } else {
            operands.push(Operand::new(Src::Off, 0));
        }
    }
    Ok(Instruction {
        encoding: Encoding::Exp,
        opcode: 0,
        name: "exp",
        words: 2,
        n_dst: 0,
        operands,
        mods,
    })
}

/// Décode un flux de dwords complet. Arrête à la première erreur.
pub fn decode_stream(words: &[u32], wave: WaveSize) -> Result<Vec<Decoded>, DecodeError> {
    let mut out = Vec::new();
    let mut pc = 0usize;
    while pc < words.len() {
        let inst = decode(&words[pc..], wave).map_err(|e| match e {
            DecodeError::Truncated(n) => DecodeError::Truncated(pc + n),
            other => other,
        })?;
        let len = usize::from(inst.words);
        out.push(Decoded { offset: pc, inst });
        pc += len;
    }
    Ok(out)
}
