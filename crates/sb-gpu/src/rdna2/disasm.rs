//! Désassembleur texte, syntaxe compatible LLVM AMDGPU (`llvm-mc -mcpu=gfx1030`).

use std::fmt::Write;

use super::inst::{Encoding, Ext, Instruction, Mods, Operand, Src, WaveSize};

fn reg_range(prefix: &str, n: u32, width: u8) -> String {
    let end = n + u32::from(width);
    if prefix == "s" && width > 1 && n < 106 && end > 106 {
        // LLVM énumère les registres lorsque la plage recouvre VCC.
        let items: Vec<String> = (n..end)
            .map(|i| match i {
                106 => "vcc_lo".to_string(),
                107 => "vcc_hi".to_string(),
                _ => format!("s{i}"),
            })
            .collect();
        return format!("[{}]", items.join(","));
    }
    if width <= 1 {
        format!("{prefix}{n}")
    } else {
        format!("{prefix}[{n}:{}]", n + u32::from(width) - 1)
    }
}

fn float_text(code: u8) -> &'static str {
    match code {
        240 => "0.5",
        241 => "-0.5",
        242 => "1.0",
        243 => "-1.0",
        244 => "2.0",
        245 => "-2.0",
        246 => "4.0",
        247 => "-4.0",
        _ => "0.15915494",
    }
}

/// Texte d'un opérande (avec `neg`/`abs`).
pub fn format_operand(op: &Operand) -> String {
    fmt_op(op, false)
}

/// `mask16` : le littéral est affiché tronqué à 16 bits (instructions à opérandes 16 bits).
fn fmt_op(op: &Operand, mask16: bool) -> String {
    let is_const = matches!(
        op.src,
        Src::InlineInt(_) | Src::InlineFloat(_) | Src::Literal(_)
    );
    let base = match op.src {
        Src::Sgpr(n) => reg_range("s", u32::from(n), op.width),
        Src::Vgpr(n) => reg_range("v", u32::from(n), op.width),
        Src::Ttmp(n) => reg_range("ttmp", u32::from(n), op.width),
        Src::VccLo if op.width >= 2 => "vcc".into(),
        Src::VccLo => "vcc_lo".into(),
        Src::VccHi => "vcc_hi".into(),
        Src::ExecLo if op.width >= 2 => "exec".into(),
        Src::ExecLo => "exec_lo".into(),
        Src::ExecHi => "exec_hi".into(),
        Src::M0 => "m0".into(),
        Src::Null => "null".into(),
        Src::InlineInt(v) => v.to_string(),
        Src::InlineFloat(248) if op.width >= 2 => "0.15915494309189532".into(),
        Src::InlineFloat(c) => float_text(c).into(),
        Src::Literal(v) if mask16 => format!("0x{:x}", v & 0xFFFF),
        Src::Literal(v) => format!("0x{v:x}"),
        Src::Scc => "src_scc".into(),
        Src::Vccz => "src_vccz".into(),
        Src::Execz => "src_execz".into(),
        Src::LdsDirect => "src_lds_direct".into(),
        Src::Special(235) => "src_shared_base".into(),
        Src::Special(236) => "src_shared_limit".into(),
        Src::Special(237) => "src_private_base".into(),
        Src::Special(238) => "src_private_limit".into(),
        Src::Special(239) => "src_pops_exiting_wave_id".into(),
        Src::Special(c) => format!("src_special_{c}"),
        Src::Off => "off".into(),
    };
    match (op.neg, op.abs, is_const) {
        (false, false, _) => base,
        (false, true, _) => format!("|{base}|"),
        (true, false, true) => format!("neg({base})"),
        (true, false, false) => format!("-{base}"),
        (true, true, _) => format!("-|{base}|"),
    }
}

/// Les modificateurs de source `neg`/`abs` s'appliquent-ils à la source `idx` (opérandes flottants) ?
fn src_has_fp_mods(inst: &Instruction, idx: usize) -> bool {
    let b = inst.base_name();
    if b.starts_with("v_cmp_class")
        || b.starts_with("v_cmpx_class")
        || b.starts_with("v_ldexp_")
        || b.starts_with("v_trig_preop")
        || b == "v_cvt_pk_u8_f32"
    {
        return idx == 0;
    }
    if b.starts_with("v_cvt_") {
        return b.rsplit('_').next().is_some_and(|t| t.starts_with('f'));
    }
    b == "v_cndmask_b32" || b.ends_with("_f32") || b.ends_with("_f16") || b.ends_with("_f64")
}

/// Affichage tronqué à 16 bits du littéral (instructions dont les sources sont 16 bits).
fn literal_is_16bit(inst: &Instruction) -> bool {
    let b = inst.base_name();
    if b.contains("fmamk")
        || b.contains("fmaak")
        || b.starts_with("v_pk_")
        || b.starts_with("v_dot")
        || b.starts_with("v_fma_mix")
        || b.starts_with("s_")
    {
        return false;
    }
    if b.starts_with("v_cvt_") {
        return b
            .rsplit('_')
            .next()
            .is_some_and(|t| matches!(t, "f16" | "i16" | "u16" | "b16"));
    }
    b.ends_with("_f16") || b.ends_with("_i16") || b.ends_with("_u16") || b.ends_with("_b16")
}

/// Le VOP3 supporte-t-il `op_sel` (opérandes 16 bits, hors comparaisons) ?
fn vop3_has_op_sel(inst: &Instruction) -> bool {
    let b = inst.base_name();
    !b.starts_with("v_cmp")
        && !inst.name.ends_with("_e64")
        && (b.contains("_f16") || b.contains("_i16") || b.contains("_u16") || b.contains("_b16"))
}

fn dim_name(dim: u8) -> &'static str {
    [
        "SQ_RSRC_IMG_1D",
        "SQ_RSRC_IMG_2D",
        "SQ_RSRC_IMG_3D",
        "SQ_RSRC_IMG_CUBE",
        "SQ_RSRC_IMG_1D_ARRAY",
        "SQ_RSRC_IMG_2D_ARRAY",
        "SQ_RSRC_IMG_2D_MSAA",
        "SQ_RSRC_IMG_2D_MSAA_ARRAY",
    ][usize::from(dim & 7)]
}

fn bits_list(v: u8, n: usize) -> String {
    let parts: Vec<String> = (0..n).map(|i| ((v >> i) & 1).to_string()).collect();
    format!("[{}]", parts.join(","))
}

fn waitcnt_text(imm: i32) -> String {
    let v = imm as u32;
    let vm = (v & 0xF) | ((v >> 14) & 3) << 4;
    let exp = (v >> 4) & 7;
    let lgkm = (v >> 8) & 0x3F;
    let mut parts = Vec::new();
    if vm != 63 {
        parts.push(format!("vmcnt({vm})"));
    }
    if exp != 7 {
        parts.push(format!("expcnt({exp})"));
    }
    if lgkm != 63 {
        parts.push(format!("lgkmcnt({lgkm})"));
    }
    if parts.is_empty() {
        // Tous les compteurs au maximum : LLVM affiche la valeur brute.
        format!("0x{v:x}")
    } else {
        parts.join(" ")
    }
}

fn mods_alu(inst: &Instruction, out: &mut String) {
    let m: &Mods = &inst.mods;
    match inst.encoding {
        Encoding::Vop3 => {
            let nsrc = inst.srcs().len();
            if m.op_sel != 0 && vop3_has_op_sel(inst) {
                let n = if nsrc >= 3 { 4 } else { nsrc + 1 };
                let _ = write!(out, " op_sel:{}", bits_list(m.op_sel, n));
            }
            if m.clamp {
                out.push_str(" clamp");
            }
            match m.omod {
                1 => out.push_str(" mul:2"),
                2 => out.push_str(" mul:4"),
                3 => out.push_str(" div:2"),
                _ => {}
            }
        }
        Encoding::Vop3p => {
            let nsrc = inst.srcs().len();
            let b = inst.base_name();
            let mix = b.starts_with("v_fma_mix");
            let dot_int = b.starts_with("v_dot4") || b.starts_with("v_dot8");
            if !dot_int {
                if m.op_sel != 0 {
                    let _ = write!(out, " op_sel:{}", bits_list(m.op_sel, nsrc));
                }
                let full = (1u8 << nsrc) - 1;
                let default_hi = if mix { 0 } else { full };
                if m.op_sel_hi & full != default_hi {
                    let _ = write!(out, " op_sel_hi:{}", bits_list(m.op_sel_hi, nsrc));
                }
            }
            if !mix {
                if m.neg_lo != 0 {
                    let _ = write!(out, " neg_lo:{}", bits_list(m.neg_lo, nsrc));
                }
                if m.neg_hi != 0 {
                    let _ = write!(out, " neg_hi:{}", bits_list(m.neg_hi, nsrc));
                }
            }
            if m.clamp {
                out.push_str(" clamp");
            }
        }
        _ => {}
    }
}

fn mods_mem(inst: &Instruction, out: &mut String) {
    let m = &inst.mods;
    match inst.encoding {
        Encoding::Mubuf | Encoding::Mtbuf => {
            if inst.encoding == Encoding::Mtbuf && m.format != 1 {
                match super::tables::TBUF_FORMATS[usize::from(m.format & 0x7F)] {
                    "" => {
                        let _ = write!(out, " format:{}", m.format);
                    }
                    name => {
                        let _ = write!(out, " format:[{name}]");
                    }
                }
            }
            if m.idxen {
                out.push_str(" idxen");
            }
            if m.offen {
                out.push_str(" offen");
            }
            if m.offset != 0 {
                let _ = write!(out, " offset:{}", m.offset);
            }
            for (f, n) in [
                (m.glc, "glc"),
                (m.slc, "slc"),
                (m.dlc, "dlc"),
                (m.tfe, "tfe"),
                (m.lds, "lds"),
            ] {
                if f {
                    out.push(' ');
                    out.push_str(n);
                }
            }
        }
        Encoding::Ds => {
            let two = inst.name.contains("2_") || inst.name.contains("2st64");
            if two {
                if m.offset0 != 0 {
                    let _ = write!(out, " offset0:{}", m.offset0);
                }
                if m.offset1 != 0 {
                    let _ = write!(out, " offset1:{}", m.offset1);
                }
            } else if m.offset != 0 {
                let _ = write!(out, " offset:{}", m.offset);
            }
            if m.gds {
                out.push_str(" gds");
            }
        }
        Encoding::Flat | Encoding::Global | Encoding::Scratch => {
            if m.offset != 0 {
                let _ = write!(out, " offset:{}", m.offset);
            }
            for (f, n) in [
                (m.glc, "glc"),
                (m.slc, "slc"),
                (m.dlc, "dlc"),
                (m.lds, "lds"),
            ] {
                if f {
                    out.push(' ');
                    out.push_str(n);
                }
            }
        }
        Encoding::Mimg => {
            if m.dmask != 0 {
                let _ = write!(out, " dmask:0x{:x}", m.dmask);
            }
            let _ = write!(out, " dim:{}", dim_name(m.dim));
            for (f, n) in [
                (m.unorm, "unorm"),
                (m.glc, "glc"),
                (m.slc, "slc"),
                (m.dlc, "dlc"),
                (m.r128, "r128"),
                (m.a16, "a16"),
                (m.tfe, "tfe"),
                (m.lwe, "lwe"),
                (m.d16, "d16"),
            ] {
                if f {
                    out.push(' ');
                    out.push_str(n);
                }
            }
        }
        _ => {}
    }
}

fn signed_hex(v: i32) -> String {
    if v < 0 {
        format!("-0x{:x}", -i64::from(v))
    } else {
        format!("0x{v:x}")
    }
}

fn exp_target(t: u8) -> String {
    match t {
        0..=7 => format!("mrt{t}"),
        8 => "mrtz".into(),
        9 => "null".into(),
        12..=15 => format!("pos{}", t - 12),
        32..=63 => format!("param{}", t - 32),
        _ => format!("invalid_target_{t}"),
    }
}

/// Texte d'une instruction (sans adresse ni encodage), syntaxe LLVM.
pub fn disassemble(inst: &Instruction, _wave: WaveSize) -> String {
    let mut out = match inst.mods.ext {
        Ext::None => String::from(inst.name),
        Ext::Sdwa => format!(
            "{}_sdwa",
            inst.name.strip_suffix("_e32").unwrap_or(inst.name)
        ),
        Ext::Dpp16 | Ext::Dpp8 => format!(
            "{}_dpp",
            inst.name.strip_suffix("_e32").unwrap_or(inst.name)
        ),
    };
    let mask16 = literal_is_16bit(inst) || inst.base_name().starts_with("v_fma_mix");
    let mix = inst.encoding == Encoding::Vop3p && inst.base_name().starts_with("v_fma_mix");
    let nd = usize::from(inst.n_dst);
    let ops: Vec<String> = inst
        .operands
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let mut o = *o;
            if inst.encoding == Encoding::Vop3 && i >= nd {
                if !src_has_fp_mods(inst, i - nd) {
                    o.neg = false;
                    o.abs = false;
                }
            } else if mix && i >= nd {
                // v_fma_mix : neg_lo = négation, neg_hi = valeur absolue.
                o.neg = inst.mods.neg_lo >> (i - nd) & 1 != 0;
                o.abs = inst.mods.neg_hi >> (i - nd) & 1 != 0;
            }
            fmt_op(&o, mask16)
        })
        .collect();
    let mut ops = ops;
    if let Some(sd) = inst.mods.sdwa() {
        for (k, sext) in [sd.src0_sext, sd.src1_sext].into_iter().enumerate() {
            if sext && let Some(t) = ops.get_mut(nd + k) {
                *t = format!("sext({t})");
            }
        }
    }
    match inst.encoding {
        Encoding::Sopp => {
            let imm = inst.mods.imm;
            let imm16 = (imm as u32) & 0xFFFF;
            match inst.name {
                "s_endpgm"
                | "s_barrier"
                | "s_code_end"
                | "s_icache_inv"
                | "s_wakeup"
                | "s_ttracedata"
                | "s_endpgm_saved"
                | "s_endpgm_ordered_ps_done"
                | "s_trap_nop" => {
                    if imm16 != 0 {
                        let _ = write!(out, " {imm16}");
                    }
                }
                "s_waitcnt" => {
                    out.push(' ');
                    out.push_str(&waitcnt_text(imm));
                }
                n if n == "s_branch"
                    || n.starts_with("s_cbranch_")
                    || n.starts_with("s_sendmsg") =>
                {
                    let _ = write!(out, " {imm16}");
                }
                "s_nop" | "s_sleep" | "s_setprio" if imm16 <= 0xFF => {
                    let _ = write!(out, " {imm16}");
                }
                _ => {
                    let _ = write!(out, " 0x{imm16:x}");
                }
            }
            return out;
        }
        Encoding::Sopk => {
            let imm16 = (inst.mods.imm as u32) & 0xFFFF;
            let imm_text = match inst.name {
                "s_call_b64" | "s_subvector_loop_begin" | "s_subvector_loop_end" => {
                    imm16.to_string()
                }
                "s_getreg_b32" | "s_setreg_b32" => {
                    let (id, off, size) = (
                        imm16 & 0x3F,
                        (imm16 >> 6) & 0x1F,
                        ((imm16 >> 11) & 0x1F) + 1,
                    );
                    let name = super::tables::HWREG_NAMES[id as usize];
                    let id_text = if name.is_empty() {
                        id.to_string()
                    } else {
                        name.to_string()
                    };
                    if off == 0 && size == 32 && !name.is_empty() {
                        format!("hwreg({id_text})")
                    } else {
                        format!("hwreg({id_text}, {off}, {size})")
                    }
                }
                _ => format!("0x{imm16:x}"),
            };
            let mut parts = ops.clone();
            if inst.name == "s_setreg_b32" {
                parts.insert(0, imm_text);
            } else if inst.name == "s_version" {
                parts = vec![imm_text];
            } else {
                parts.push(imm_text);
            }
            let _ = write!(out, " {}", parts.join(", "));
            return out;
        }
        Encoding::Exp => {
            let mut parts = vec![exp_target(inst.mods.target)];
            parts.extend(ops);
            let _ = write!(out, " {}", parts.join(", "));
            if inst.mods.done {
                out.push_str(" done");
            }
            if inst.mods.compr {
                out.push_str(" compr");
            }
            if inst.mods.vm {
                out.push_str(" vm");
            }
            return out;
        }
        Encoding::Vintrp => {
            let m = &inst.mods;
            let chan = ['x', 'y', 'z', 'w'][usize::from(m.attr_chan & 3)];
            let mut parts = vec![ops[0].clone()];
            if inst.name.starts_with("v_interp_mov") {
                let p = match inst.operands[1].src {
                    Src::Special(0) => "p10".to_string(),
                    Src::Special(1) => "p20".to_string(),
                    Src::Special(2) => "p0".to_string(),
                    Src::Special(n) => format!("invalid_param_{n}"),
                    _ => "invalid_param".to_string(),
                };
                parts.push(p);
            } else {
                parts.push(ops[1].clone());
            }
            parts.push(format!("attr{}.{chan}", m.attr));
            let _ = write!(out, " {}", parts.join(", "));
            return out;
        }
        _ => {}
    }

    let mut parts = ops;
    if inst.encoding == Encoding::Smem {
        let no_off = matches!(
            inst.name,
            "s_dcache_inv" | "s_gl1_inv" | "s_memtime" | "s_memrealtime"
        );
        if no_off {
            return out
                + &if parts.is_empty() {
                    String::new()
                } else {
                    format!(" {}", parts.join(", "))
                };
        }
        if inst.name.starts_with("s_atc_probe") {
            let v = inst.mods.imm;
            parts.insert(
                0,
                if v < 64 {
                    v.to_string()
                } else {
                    format!("0x{v:x}")
                },
            );
        }
        // L'offset immédiat ferme la liste ; avec soffset, il devient un modificateur.
        if !inst.mods.smem_soffset {
            // Sans soffset ni offset, LLVM affiche l'opérande `null`.
            parts.push(if inst.mods.offset == 0 {
                "null".to_string()
            } else {
                signed_hex(inst.mods.offset)
            });
        }
    }
    if inst.encoding == Encoding::Mubuf && inst.operands.is_empty() {
        return out;
    }
    if inst.encoding == Encoding::Mimg && inst.mods.nsa > 0 {
        // adresses NSA : tous les registres entre vdata et srsrc affichés en liste.
        let n_addr = inst.operands.len()
            - 2
            - usize::from(
                inst.name.starts_with("image_sample") || inst.name.starts_with("image_gather4"),
            );
        let addrs: Vec<String> = parts[1..1 + n_addr].to_vec();
        let samp_and_rsrc: Vec<String> = parts[1 + n_addr..].to_vec();
        let mut np = vec![parts[0].clone(), format!("[{}]", addrs.join(", "))];
        np.extend(samp_and_rsrc);
        parts = np;
    }
    if !parts.is_empty() {
        let _ = write!(out, " {}", parts.join(", "));
    }
    ext_mods(inst, &mut out);
    mods_alu(inst, &mut out);
    mods_mem(inst, &mut out);
    if inst.encoding == Encoding::Smem {
        if inst.mods.smem_soffset && inst.mods.offset != 0 {
            let _ = write!(out, " offset:{}", signed_hex(inst.mods.offset));
        }
        if inst.mods.glc && !inst.name.starts_with("s_atc_probe") {
            out.push_str(" glc");
        }
        if inst.mods.dlc && !inst.name.starts_with("s_atc_probe") {
            out.push_str(" dlc");
        }
    }
    out
}

const SDWA_SEL: [&str; 7] = [
    "BYTE_0", "BYTE_1", "BYTE_2", "BYTE_3", "WORD_0", "WORD_1", "DWORD",
];

/// Modificateurs DPP / SDWA (syntaxe LLVM).
fn ext_mods(inst: &Instruction, out: &mut String) {
    if let Some(d) = inst.mods.dpp16() {
        let c = d.ctrl;
        let ctrl = match c {
            0..=0xFF => format!(
                "quad_perm:[{},{},{},{}]",
                c & 3,
                (c >> 2) & 3,
                (c >> 4) & 3,
                (c >> 6) & 3
            ),
            0x101..=0x10F => format!("row_shl:{}", c & 15),
            0x111..=0x11F => format!("row_shr:{}", c & 15),
            0x121..=0x12F => format!("row_ror:{}", c & 15),
            0x140 => "row_mirror".to_string(),
            0x141 => "row_half_mirror".to_string(),
            0x150..=0x15F => format!("row_share:{}", c & 15),
            0x160..=0x16F => format!("row_xmask:{}", c & 15),
            _ => format!("dpp_ctrl:0x{c:x}"),
        };
        let _ = write!(
            out,
            " {ctrl} row_mask:0x{:x} bank_mask:0x{:x}",
            d.row_mask, d.bank_mask
        );
        if d.bound_ctrl {
            out.push_str(" bound_ctrl:1");
        }
        if d.fi {
            out.push_str(" fi:1");
        }
    } else if let Some((sel, fi)) = inst.mods.dpp8() {
        let l: Vec<String> = sel.iter().map(|v| v.to_string()).collect();
        let _ = write!(out, " dpp8:[{}]", l.join(","));
        if fi {
            out.push_str(" fi:1");
        }
    } else if let Some(s) = inst.mods.sdwa() {
        if s.clamp {
            out.push_str(" clamp");
        }
        match s.omod {
            1 => out.push_str(" mul:2"),
            2 => out.push_str(" mul:4"),
            3 => out.push_str(" div:2"),
            _ => {}
        }
        let has_dst = inst.encoding != Encoding::Vopc;
        if has_dst {
            let u = [
                "UNUSED_PAD",
                "UNUSED_SEXT",
                "UNUSED_PRESERVE",
                "UNUSED_PRESERVE",
            ][usize::from(s.dst_unused & 3)];
            let _ = write!(
                out,
                " dst_sel:{} dst_unused:{u}",
                SDWA_SEL[usize::from(s.dst_sel.min(6))]
            );
        }
        let _ = write!(
            out,
            " src0_sel:{}",
            SDWA_SEL[usize::from(s.src0_sel.min(6))]
        );
        if inst.srcs().len() > 1 {
            let _ = write!(
                out,
                " src1_sel:{}",
                SDWA_SEL[usize::from(s.src1_sel.min(6))]
            );
        }
    }
}

/// Désassemble un flux complet : une ligne `offset: texte` par instruction.
pub fn disassemble_stream(
    words: &[u32],
    wave: WaveSize,
) -> Result<String, super::inst::DecodeError> {
    let mut out = String::new();
    for d in super::decode::decode_stream(words, wave)? {
        let _ = writeln!(out, "{:04x}: {}", d.offset * 4, disassemble(&d.inst, wave));
    }
    Ok(out)
}
