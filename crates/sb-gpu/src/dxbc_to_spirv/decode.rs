//! Décodage du flux de jetons SM4/SM5 en instructions structurées (opérandes, opcodes étendus,
//! déclarations). Aucun panic : toute lecture hors bornes renvoie `TruncatedInstruction`.

use super::ShaderTranslateError;
use super::opcodes::*;

#[derive(Debug, Clone, Default)]
pub struct Index {
    pub imm: u32,
    pub rel: Option<Box<Operand>>,
}

#[derive(Debug, Clone, Default)]
pub struct Operand {
    pub ty: u32,
    pub mask: u8,
    pub swizzle: [u32; 4],
    pub neg: bool,
    pub abs: bool,
    pub dims: usize,
    pub idx: [Index; 3],
    pub imm: [u32; 4],
}

#[derive(Debug, Clone, Default)]
pub struct Inst {
    pub opcode: u32,
    pub token: u32,
    pub operands: Vec<Operand>,
    pub extra: Vec<u32>,
    /// Offsets de texel immédiats (`aoffimmi`), si présents.
    pub offset: Option<[i32; 3]>,
    pub res_dim: Option<(u32, u32)>,
    pub ret_types: Option<[u32; 4]>,
    pub custom: Vec<u32>,
}

impl Inst {
    pub fn saturate(&self) -> bool {
        (self.token >> 13) & 1 != 0
    }
    /// `true` pour la variante `_nz`, `false` pour `_z`.
    pub fn test_nonzero(&self) -> bool {
        (self.token >> 18) & 1 != 0
    }
}

pub fn decode_operand(words: &[u32], cur: &mut usize) -> Option<Operand> {
    let tok = *words.get(*cur)?;
    *cur += 1;
    let num_comp = tok & 3;
    let sel_mode = (tok >> 2) & 3;
    let ty = (tok >> 12) & 0xFF;
    let dims = ((tok >> 20) & 3) as usize;
    let mut op = Operand {
        ty,
        mask: 0xF,
        swizzle: [0, 1, 2, 3],
        dims,
        ..Default::default()
    };
    let mut ext = tok >> 31 != 0;
    while ext {
        let e = *words.get(*cur)?;
        *cur += 1;
        if e & 0x3F == 1 {
            let m = (e >> 6) & 0xFF;
            op.neg = m & 1 != 0;
            op.abs = m & 2 != 0;
        }
        ext = e >> 31 != 0;
    }
    if num_comp == 2 {
        match sel_mode {
            0 => op.mask = ((tok >> 4) & 0xF) as u8,
            1 => {
                op.swizzle = [
                    (tok >> 4) & 3,
                    (tok >> 6) & 3,
                    (tok >> 8) & 3,
                    (tok >> 10) & 3,
                ]
            }
            2 => {
                let c = (tok >> 4) & 3;
                op.swizzle = [c; 4];
            }
            _ => return None,
        }
    } else if num_comp == 1 {
        op.mask = 1;
        op.swizzle = [0; 4];
    }
    if ty == OPERAND_IMMEDIATE32 {
        if num_comp == 1 {
            let v = *words.get(*cur)?;
            *cur += 1;
            op.imm = [v; 4];
        } else {
            for (i, slot) in op.imm.iter_mut().enumerate() {
                *slot = *words.get(*cur + i)?;
            }
            *cur += 4;
        }
        return Some(op);
    }
    if ty == OPERAND_IMMEDIATE64 {
        *cur += if num_comp == 1 { 2 } else { 4 };
        if *cur > words.len() {
            return None;
        }
        return Some(op);
    }
    for (d, slot) in op.idx.iter_mut().enumerate().take(dims) {
        let rep = (tok >> (22 + d as u32 * 3)) & 7;
        let mut index = Index::default();
        match rep {
            0 => {
                index.imm = *words.get(*cur)?;
                *cur += 1;
            }
            1 => {
                index.imm = *words.get(*cur)?;
                *cur += 2;
            }
            2 => index.rel = Some(Box::new(decode_operand(words, cur)?)),
            3 => {
                index.imm = *words.get(*cur)?;
                *cur += 1;
                index.rel = Some(Box::new(decode_operand(words, cur)?));
            }
            4 => {
                index.imm = *words.get(*cur)?;
                *cur += 2;
                index.rel = Some(Box::new(decode_operand(words, cur)?));
            }
            _ => return None,
        }
        *slot = index;
    }
    if *cur > words.len() {
        return None;
    }
    Some(op)
}

/// `(nombre d'opérandes, mots supplémentaires)` des déclarations ; `None` pour une instruction.
fn decl_layout(opcode: u32) -> Option<(usize, usize)> {
    Some(match opcode {
        OP_DCL_RESOURCE => (1, 1),
        OP_DCL_CONSTANT_BUFFER | OP_DCL_SAMPLER | OP_DCL_INPUT | OP_DCL_INPUT_PS => (1, 0),
        OP_DCL_INDEX_RANGE => (1, 1),
        OP_DCL_INPUT_SGV | OP_DCL_INPUT_SIV | OP_DCL_INPUT_PS_SGV | OP_DCL_INPUT_PS_SIV => (1, 1),
        OP_DCL_OUTPUT => (1, 0),
        OP_DCL_OUTPUT_SGV | OP_DCL_OUTPUT_SIV => (1, 1),
        OP_DCL_TEMPS | OP_DCL_MAX_OUTPUT_VERTEX_COUNT | OP_DCL_GS_INSTANCE_COUNT => (0, 1),
        OP_DCL_HS_MAX_TESSFACTOR
        | OP_DCL_HS_FORK_PHASE_INSTANCE_COUNT
        | OP_DCL_HS_JOIN_PHASE_INSTANCE_COUNT => (0, 1),
        OP_DCL_INDEXABLE_TEMP => (0, 3),
        OP_DCL_GLOBAL_FLAGS
        | OP_DCL_GS_OUTPUT_PRIMITIVE_TOPOLOGY
        | OP_DCL_GS_INPUT_PRIMITIVE
        | OP_DCL_INPUT_CONTROL_POINT_COUNT
        | OP_DCL_OUTPUT_CONTROL_POINT_COUNT
        | OP_DCL_TESS_DOMAIN
        | OP_DCL_TESS_PARTITIONING
        | OP_DCL_TESS_OUTPUT_PRIMITIVE => (0, 0),
        OP_DCL_STREAM => (1, 0),
        OP_DCL_THREAD_GROUP => (0, 3),
        OP_DCL_UAV_TYPED => (1, 1),
        OP_DCL_UAV_RAW | OP_DCL_RESOURCE_RAW => (1, 0),
        OP_DCL_UAV_STRUCTURED | OP_DCL_RESOURCE_STRUCTURED | OP_DCL_TGSM_RAW => (1, 1),
        OP_DCL_TGSM_STRUCTURED => (1, 2),
        _ => return None,
    })
}

fn sign4(v: u32) -> i32 {
    ((v << 28) as i32) >> 28
}

/// Décode tout le programme (après les deux mots d'en-tête `version`/`length`).
pub fn decode_program(words: &[u32]) -> Result<Vec<Inst>, ShaderTranslateError> {
    let mut out = Vec::new();
    let mut pc = 0usize;
    while pc < words.len() {
        let token = words[pc];
        let opcode = token & 0x7FF;
        let mut len = ((token >> 24) & 0x7F) as usize;
        let mut inst = Inst {
            opcode,
            token,
            ..Default::default()
        };
        if opcode == OP_CUSTOMDATA {
            len = *words
                .get(pc + 1)
                .ok_or(ShaderTranslateError::TruncatedInstruction(pc))? as usize;
            if len < 2 || pc + len > words.len() {
                return Err(ShaderTranslateError::TruncatedInstruction(pc));
            }
            inst.extra = vec![token >> 11];
            inst.custom = words[pc + 2..pc + len].to_vec();
            out.push(inst);
            pc += len;
            continue;
        }
        if len == 0 || pc + len > words.len() {
            return Err(ShaderTranslateError::TruncatedInstruction(pc));
        }
        let end = pc + len;
        let body = &words[..end];
        let mut cur = pc + 1;
        let mut ext = token >> 31 != 0;
        while ext {
            let e = *body
                .get(cur)
                .ok_or(ShaderTranslateError::TruncatedInstruction(pc))?;
            cur += 1;
            match e & 0x3F {
                1 => {
                    inst.offset = Some([
                        sign4((e >> 9) & 0xF),
                        sign4((e >> 13) & 0xF),
                        sign4((e >> 17) & 0xF),
                    ])
                }
                2 => inst.res_dim = Some(((e >> 6) & 0x1F, (e >> 11) & 0xFFF)),
                3 => {
                    inst.ret_types = Some([
                        (e >> 6) & 0xF,
                        (e >> 10) & 0xF,
                        (e >> 14) & 0xF,
                        (e >> 18) & 0xF,
                    ])
                }
                _ => {}
            }
            ext = e >> 31 != 0;
        }
        let trunc = || ShaderTranslateError::TruncatedInstruction(pc);
        if let Some((nops, nextra)) = decl_layout(opcode) {
            for _ in 0..nops {
                inst.operands
                    .push(decode_operand(body, &mut cur).ok_or_else(trunc)?);
            }
            for _ in 0..nextra {
                inst.extra.push(*body.get(cur).ok_or_else(trunc)?);
                cur += 1;
            }
            // Mots restants (tables d'interfaces...) conservés tels quels.
            inst.extra.extend_from_slice(&body[cur.min(end)..end]);
        } else if matches!(
            opcode,
            OP_DCL_FUNCTION_BODY | OP_DCL_FUNCTION_TABLE | OP_DCL_INTERFACE
        ) {
            inst.extra.extend_from_slice(&body[cur.min(end)..end]);
        } else {
            while cur < end {
                inst.operands
                    .push(decode_operand(body, &mut cur).ok_or_else(trunc)?);
            }
            if cur > end {
                return Err(trunc());
            }
        }
        out.push(inst);
        pc = end;
    }
    Ok(out)
}
