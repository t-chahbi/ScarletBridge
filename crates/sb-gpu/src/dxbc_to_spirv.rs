//! Phase 5 — Compilateur / traducteur natif Rust **DXBC (SM4/SM5) → SPIR-V 1.3 (Vulkan)**.
//!
//! Décode les conteneurs Microsoft `DXBC` (`ISGN`, `OSGN`, `SHDR`/`SHEX`) et traduit vers un
//! module binaire SPIR-V 1.3 conforme `spirv-val --target-env vulkan1.3` :
//! - **ALU vectoriel complet** : `MOV`, `MOVC`, `ADD`, `MUL`, `DIV`, `MAD`, `DP2`, `DP3`, `DP4`,
//!   `MIN`, `MAX`, `RSQ`, `SQRT`, `FRC`, `EXP`, `LOG`, `SINCOS`, `ROUND_*`, `DERIV_RTX/RTY`
//! - **Comparaisons & opérations bit à bit** : `LT`, `GE`, `EQ`, `NE`, `AND`, `OR`, `XOR`,
//!   `ITOF`, `FTOI`, `UTOF`, `FTOU`
//! - **Constant Buffers (`cb0..cb3`)** → `Uniform` std140 (DescriptorSet 0, Bindings `0..3`),
//!   incluant l'indexation dynamique relative `cb0[rN.c + offset]`
//! - **Textures 2D & Samplers (`t0..t3`, `s0..s3`)** → Bindings `8..11` et `16..19` (`SAMPLE`, `SAMPLE_L`)
//! - Swizzles, masques d'écriture, modificateurs `neg` / `abs` / `saturate` et constantes inline.

use std::collections::{BTreeSet, HashMap};

use thiserror::Error;

use crate::shader::ShaderStage;

/// Nombre maximal de slots de Constant Buffers liés en DescriptorSet 0 (bindings `0..4`).
pub const MAX_CBUFFER_SLOTS: u32 = 4;
/// Nombre maximal de vecteurs `vec4` par Constant Buffer (256 × 16o = 4 KiB).
pub const CBUFFER_VEC4_COUNT: u32 = 256;
/// Binding de base des textures 2D échantillonnées (`t0..t3` → bindings `8..11`).
pub const TEXTURE_BINDING_BASE: u32 = 8;
/// Nombre maximal de textures 2D liées simultanément.
pub const MAX_TEXTURE_SLOTS: u32 = 4;
/// Binding de base des samplers (`s0..s3` → bindings `16..19`).
pub const SAMPLER_BINDING_BASE: u32 = 16;
/// Nombre maximal de samplers liés simultanément.
pub const MAX_SAMPLER_SLOTS: u32 = 4;
/// Binding de base des Storage Buffers / UAV (`u0..u3` → bindings `24..27`).
pub const STORAGE_BINDING_BASE: u32 = 24;
/// Nombre maximal de Storage Buffers (SSBO / UAV) liés simultanément.
pub const MAX_STORAGE_SLOTS: u32 = 4;

#[derive(Debug, Error)]
pub enum ShaderTranslateError {
    #[error("en-tête DXBC invalide ou tronqué")]
    InvalidContainer,
    #[error("chunk bytecode SHDR/SHEX introuvable dans le conteneur DXBC")]
    MissingBytecode,
    #[error("stage shader non supporté pour la traduction : {0:?}")]
    UnsupportedStage(ShaderStage),
    #[error("instruction SM4/SM5 tronquée à l'offset mot {0}")]
    TruncatedInstruction(usize),
}

// Opcodes officiels SM4/SM5 (`d3d10tokenizedprogramformat.h` / `D3D10_SB_OPCODE_TYPE`).
const OP_ADD: u32 = 0x00; // 0
const OP_AND: u32 = 0x01; // 1
const OP_DERIV_RTX: u32 = 0x0B; // 11
const OP_DERIV_RTY: u32 = 0x0C; // 12
const OP_DIV: u32 = 0x0E; // 14
const OP_DP2: u32 = 0x0F; // 15
const OP_DP3: u32 = 0x10; // 16
const OP_DP4: u32 = 0x11; // 17
const OP_EQ: u32 = 0x18; // 24
const OP_EXP: u32 = 0x19; // 25
const OP_FRC: u32 = 0x1A; // 26
const OP_FTOI: u32 = 0x1B; // 27
const OP_FTOU: u32 = 0x1C; // 28
const OP_GE: u32 = 0x1D; // 29
const OP_IADD: u32 = 0x1E; // 30
const OP_ITOF: u32 = 0x2B; // 43
const OP_LOG: u32 = 0x2F; // 47
const OP_LT: u32 = 0x31; // 49
const OP_MAD: u32 = 0x32; // 50
const OP_MIN: u32 = 0x33; // 51
const OP_MAX: u32 = 0x34; // 52
const OP_MOV: u32 = 0x36; // 54
const OP_MOVC: u32 = 0x37; // 55
const OP_MUL: u32 = 0x38; // 56
const OP_NE: u32 = 0x39; // 57
const OP_OR: u32 = 0x3C; // 60
const OP_RET: u32 = 0x3E; // 62
const OP_ROUND_NE: u32 = 0x40; // 64
const OP_ROUND_NI: u32 = 0x41; // 65
const OP_ROUND_PI: u32 = 0x42; // 66
const OP_ROUND_Z: u32 = 0x43; // 67
const OP_RSQ: u32 = 0x44; // 68
const OP_SAMPLE: u32 = 0x45; // 69
const OP_SAMPLE_L: u32 = 0x48; // 72
const OP_SINCOS: u32 = 0x4D; // 77
const OP_SQRT: u32 = 0x4B; // 75
const OP_UTOF: u32 = 0x56; // 86
const OP_XOR: u32 = 0x57; // 87
const OP_DCL_TEMPS: u32 = 0x68; // 104
const OP_DCL_THREAD_GROUP: u32 = 0x9B; // 155
const OP_LD_RAW: u32 = 0xA5; // 165
const OP_STORE_RAW: u32 = 0xA6; // 166
const OP_LD_STRUCTURED: u32 = 0xA7; // 167
const OP_STORE_STRUCTURED: u32 = 0xA8; // 168

// Types d'opérandes SM4/SM5 (`D3D10_SB_OPERAND_TYPE` / `D3D11_SB_OPERAND_TYPE`).
const OPERAND_TEMP: u32 = 0;
const OPERAND_INPUT: u32 = 1;
const OPERAND_OUTPUT: u32 = 2;
const OPERAND_IMMEDIATE32: u32 = 4;
const OPERAND_SAMPLER: u32 = 6;
const OPERAND_RESOURCE: u32 = 7;
const OPERAND_CONSTANT_BUFFER: u32 = 8;
const OPERAND_NULL: u32 = 13;
const OPERAND_UNORDERED_ACCESS_VIEW: u32 = 30;
const OPERAND_INPUT_THREAD_ID: u32 = 32;
const OPERAND_INPUT_THREAD_GROUP_ID: u32 = 33;
const OPERAND_INPUT_THREAD_ID_IN_GROUP: u32 = 34;

#[derive(Debug, Clone)]
pub struct SignatureElement {
    pub semantic_name: String,
    pub semantic_index: u32,
    pub system_value_type: u32,
    pub register: u32,
    pub mask: u8,
}

#[derive(Debug, Clone)]
pub struct TranslatedShader {
    pub stage: ShaderStage,
    pub spirv: Vec<u32>,
    pub inputs: Vec<SignatureElement>,
    pub outputs: Vec<SignatureElement>,
    pub used_cbuffers: Vec<u32>,
    pub used_textures: Vec<u32>,
    pub used_samplers: Vec<u32>,
    pub used_storage_buffers: Vec<u32>,
    pub thread_group_size: [u32; 3],
}

impl TranslatedShader {
    /// Retourne les octets bruts little-endian prêts pour `vkCreateShaderModule` ou `spirv-val`.
    pub fn as_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.spirv.len() * 4);
        for &w in &self.spirv {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out
    }
}

/// Traduit un conteneur DXBC complet (`ISGN` + `OSGN` + `SHDR`/`SHEX`) en SPIR-V 1.3 Vulkan.
pub fn translate_dxbc_to_spirv(dxbc: &[u8]) -> Result<TranslatedShader, ShaderTranslateError> {
    if dxbc.len() < 32 || &dxbc[0..4] != b"DXBC" {
        return Err(ShaderTranslateError::InvalidContainer);
    }
    let chunk_count = u32::from_le_bytes([dxbc[28], dxbc[29], dxbc[30], dxbc[31]]) as usize;
    if 32 + chunk_count * 4 > dxbc.len() {
        return Err(ShaderTranslateError::InvalidContainer);
    }

    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut bytecode_words: Option<Vec<u32>> = None;

    for i in 0..chunk_count {
        let off_pos = 32 + i * 4;
        let off = u32::from_le_bytes([
            dxbc[off_pos],
            dxbc[off_pos + 1],
            dxbc[off_pos + 2],
            dxbc[off_pos + 3],
        ]) as usize;
        if off + 8 > dxbc.len() {
            continue;
        }
        let fourcc = &dxbc[off..off + 4];
        let size = u32::from_le_bytes([dxbc[off + 4], dxbc[off + 5], dxbc[off + 6], dxbc[off + 7]])
            as usize;
        let payload_end = (off + 8 + size).min(dxbc.len());
        let payload = &dxbc[off + 8..payload_end];

        match fourcc {
            b"ISGN" => inputs = parse_signature_chunk(payload),
            b"OSGN" => outputs = parse_signature_chunk(payload),
            b"SHDR" | b"SHEX" => {
                let mut words = Vec::with_capacity(payload.len() / 4);
                for &chunk in payload.as_chunks::<4>().0 {
                    words.push(u32::from_le_bytes(chunk));
                }
                bytecode_words = Some(words);
            }
            _ => {}
        }
    }

    let words = bytecode_words.ok_or(ShaderTranslateError::MissingBytecode)?;
    if words.len() < 2 {
        return Err(ShaderTranslateError::MissingBytecode);
    }

    let kind = ((words[0] >> 16) & 0xFFFF) as u16;
    let stage = match kind {
        0 => ShaderStage::Pixel,
        1 => ShaderStage::Vertex,
        5 => ShaderStage::Compute,
        other => {
            return Err(ShaderTranslateError::UnsupportedStage(
                ShaderStage::Unknown(other),
            ));
        }
    };

    let emitted = emit_spirv(stage, &inputs, &outputs, &words[2..])?;
    Ok(TranslatedShader {
        stage,
        spirv: emitted.spirv,
        inputs,
        outputs,
        used_cbuffers: emitted.used_cbuffers,
        used_textures: emitted.used_textures,
        used_samplers: emitted.used_samplers,
        used_storage_buffers: emitted.used_storage_buffers,
        thread_group_size: emitted.thread_group_size,
    })
}

fn parse_signature_chunk(payload: &[u8]) -> Vec<SignatureElement> {
    if payload.len() < 8 {
        return Vec::new();
    }
    let count = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
    let mut elems = Vec::with_capacity(count);
    for i in 0..count {
        let base = 8 + i * 24;
        if base + 24 > payload.len() {
            break;
        }
        let name_off = u32::from_le_bytes([
            payload[base],
            payload[base + 1],
            payload[base + 2],
            payload[base + 3],
        ]) as usize;
        let semantic_index = u32::from_le_bytes([
            payload[base + 4],
            payload[base + 5],
            payload[base + 6],
            payload[base + 7],
        ]);
        let system_value_type = u32::from_le_bytes([
            payload[base + 8],
            payload[base + 9],
            payload[base + 10],
            payload[base + 11],
        ]);
        let register = u32::from_le_bytes([
            payload[base + 16],
            payload[base + 17],
            payload[base + 18],
            payload[base + 19],
        ]);
        let mask = payload[base + 20];

        let semantic_name = if name_off < payload.len() {
            let end = payload[name_off..]
                .iter()
                .position(|&b| b == 0)
                .map(|p| name_off + p)
                .unwrap_or(payload.len());
            String::from_utf8_lossy(&payload[name_off..end]).into_owned()
        } else {
            format!("VAR{i}")
        };

        elems.push(SignatureElement {
            semantic_name,
            semantic_index,
            system_value_type,
            register,
            mask,
        });
    }
    elems
}

#[derive(Debug, Clone)]
enum DecodedOperand {
    Temp {
        index: u32,
        mask: u8,
        swizzle: [u32; 4],
        negate: bool,
        abs: bool,
    },
    Input {
        index: u32,
        swizzle: [u32; 4],
        negate: bool,
        abs: bool,
    },
    Output {
        index: u32,
        mask: u8,
    },
    ConstantBuffer {
        slot: u32,
        index: u32,
        /// Index relatif optionnel (`(temp_reg, component)`), ex. `cb0[r0.x + 4]`.
        rel_temp: Option<(u32, u32)>,
        swizzle: [u32; 4],
        negate: bool,
        abs: bool,
    },
    Resource {
        slot: u32,
        swizzle: [u32; 4],
    },
    UnorderedAccessView {
        slot: u32,
        swizzle: [u32; 4],
    },
    ThreadId {
        kind: u32,
        swizzle: [u32; 4],
    },
    Sampler {
        slot: u32,
    },
    Immediate([f32; 4]),
    Null,
}

fn decode_operand(words: &[u32], cursor: &mut usize) -> Option<DecodedOperand> {
    let tok = *words.get(*cursor)?;
    *cursor += 1;

    let num_comp = tok & 0x3;
    let sel_mode = (tok >> 2) & 0x3;
    let op_type = (tok >> 12) & 0xFF;
    let idx_dim = (tok >> 20) & 0x3;
    let extended = (tok >> 31) != 0;

    let mut negate = false;
    let mut abs = false;
    let mut ext_bit = extended;
    while ext_bit {
        let ext = *words.get(*cursor)?;
        *cursor += 1;
        // D3D10_SB_EXTENDED_OPERAND_MODIFIER (type = 1)
        if (ext & 0x3F) == 1 {
            negate = ((ext >> 6) & 1) != 0;
            abs = ((ext >> 7) & 1) != 0;
        }
        ext_bit = (ext >> 31) != 0;
    }

    let mask = if sel_mode == 0 {
        ((tok >> 4) & 0xF) as u8
    } else {
        0xF
    };

    let swizzle = match sel_mode {
        1 => [
            (tok >> 4) & 3,
            (tok >> 6) & 3,
            (tok >> 8) & 3,
            (tok >> 10) & 3,
        ],
        2 => {
            let c = (tok >> 4) & 3;
            [c, c, c, c]
        }
        _ => [0, 1, 2, 3],
    };

    if op_type == OPERAND_NULL {
        return Some(DecodedOperand::Null);
    }

    if matches!(
        op_type,
        OPERAND_INPUT_THREAD_ID | OPERAND_INPUT_THREAD_GROUP_ID | OPERAND_INPUT_THREAD_ID_IN_GROUP
    ) {
        return Some(DecodedOperand::ThreadId {
            kind: op_type,
            swizzle,
        });
    }

    if op_type == OPERAND_IMMEDIATE32 {
        return if num_comp == 1 {
            let v = f32::from_bits(*words.get(*cursor)?);
            *cursor += 1;
            Some(DecodedOperand::Immediate([v, v, v, v]))
        } else {
            let x = f32::from_bits(*words.get(*cursor)?);
            let y = f32::from_bits(*words.get(*cursor + 1)?);
            let z = f32::from_bits(*words.get(*cursor + 2)?);
            let w = f32::from_bits(*words.get(*cursor + 3)?);
            *cursor += 4;
            Some(DecodedOperand::Immediate([x, y, z, w]))
        };
    }

    // Décode chaque dimension d'index (`IMMEDIATE32`, `IMMEDIATE64`, `RELATIVE`, `IMMEDIATE+RELATIVE`).
    let mut idx0 = 0u32;
    let mut idx1 = 0u32;
    let mut rel_temp: Option<(u32, u32)> = None;
    for dim in 0..idx_dim {
        let rep = (tok >> (22 + dim * 3)) & 0x7;
        let assign = |val: u32, d: u32, i0: &mut u32, i1: &mut u32| {
            if d == 0 {
                *i0 = val;
            } else if d == 1 {
                *i1 = val;
            }
        };
        let capture_rel = |rel_op: DecodedOperand, dst: &mut Option<(u32, u32)>| {
            if let DecodedOperand::Temp { index, swizzle, .. } = rel_op {
                *dst = Some((index, swizzle[0]));
            }
        };
        match rep {
            0 => {
                let idx = *words.get(*cursor)?;
                *cursor += 1;
                assign(idx, dim, &mut idx0, &mut idx1);
            }
            1 => {
                let idx = *words.get(*cursor)?;
                *cursor += 2;
                assign(idx, dim, &mut idx0, &mut idx1);
            }
            2 => {
                let rel = decode_operand(words, cursor)?;
                if dim == 1 {
                    capture_rel(rel, &mut rel_temp);
                }
            }
            3 => {
                let idx = *words.get(*cursor)?;
                *cursor += 1;
                assign(idx, dim, &mut idx0, &mut idx1);
                let rel = decode_operand(words, cursor)?;
                if dim == 1 {
                    capture_rel(rel, &mut rel_temp);
                }
            }
            4 => {
                let idx = *words.get(*cursor)?;
                *cursor += 2;
                assign(idx, dim, &mut idx0, &mut idx1);
                let rel = decode_operand(words, cursor)?;
                if dim == 1 {
                    capture_rel(rel, &mut rel_temp);
                }
            }
            _ => return None,
        }
    }

    match op_type {
        OPERAND_TEMP => Some(DecodedOperand::Temp {
            index: idx0,
            mask,
            swizzle,
            negate,
            abs,
        }),
        OPERAND_INPUT => Some(DecodedOperand::Input {
            index: idx0,
            swizzle,
            negate,
            abs,
        }),
        OPERAND_OUTPUT => Some(DecodedOperand::Output { index: idx0, mask }),
        OPERAND_CONSTANT_BUFFER => Some(DecodedOperand::ConstantBuffer {
            slot: idx0,
            index: idx1,
            rel_temp,
            swizzle,
            negate,
            abs,
        }),
        OPERAND_RESOURCE => Some(DecodedOperand::Resource {
            slot: idx0,
            swizzle,
        }),
        OPERAND_UNORDERED_ACCESS_VIEW => Some(DecodedOperand::UnorderedAccessView {
            slot: idx0,
            swizzle,
        }),
        OPERAND_SAMPLER => Some(DecodedOperand::Sampler { slot: idx0 }),
        _ => None,
    }
}

// ==================== Émetteur binaire SPIR-V 1.3 ====================

struct SpvEmitter {
    next_id: u32,
    annotations: Vec<u32>,
    types_globals: Vec<u32>,
    func_vars: Vec<u32>,
    body: Vec<u32>,
    f32_consts: HashMap<u32, u32>,
    i32_consts: HashMap<i32, u32>,
    u32_consts: HashMap<u32, u32>,
    vec4_consts: HashMap<[u32; 4], u32>,
    ty_bool: u32,
    ty_bvec4: u32,
    ty_f32: u32,
    ty_i32: u32,
    ty_u32: u32,
    ty_vec2: u32,
    ty_vec4: u32,
    ty_ivec4: u32,
    ty_uvec4: u32,
    ty_ptr_fn_vec4: u32,
    ty_ptr_uni_cb: u32,
    ty_ptr_uni_vec4: u32,
    ty_image_2d: u32,
    ty_sampler: u32,
    ty_sampled_image: u32,
    ty_ptr_uc_image: u32,
    ty_ptr_uc_sampler: u32,
    ty_ptr_ssbo_struct: Option<u32>,
    ty_ptr_ssbo_vec4: Option<u32>,
    ext_glsl: u32,
    const_zero_vec4: u32,
    const_uvec4_zero: u32,
    const_uvec4_all1: u32,
    cb_vars: HashMap<u32, u32>,
    tex_vars: HashMap<u32, u32>,
    samp_vars: HashMap<u32, u32>,
    ssbo_vars: HashMap<u32, u32>,
    used_cbuffers: BTreeSet<u32>,
    used_textures: BTreeSet<u32>,
    used_samplers: BTreeSet<u32>,
    used_storage_buffers: BTreeSet<u32>,
}

impl SpvEmitter {
    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn emit_inst(buf: &mut Vec<u32>, opcode: u16, operands: &[u32]) {
        let word_count = (operands.len() + 1) as u32;
        buf.push((word_count << 16) | u32::from(opcode));
        buf.extend_from_slice(operands);
    }

    fn push_str_words(words: &mut Vec<u32>, s: &str) {
        let mut bytes = s.as_bytes().to_vec();
        bytes.push(0);
        while !bytes.len().is_multiple_of(4) {
            bytes.push(0);
        }
        for &chunk in bytes.as_chunks::<4>().0 {
            words.push(u32::from_le_bytes(chunk));
        }
    }

    fn const_i32(&mut self, value: i32) -> u32 {
        if let Some(&id) = self.i32_consts.get(&value) {
            return id;
        }
        let id = self.alloc_id();
        Self::emit_inst(
            &mut self.types_globals,
            43,
            &[self.ty_i32, id, value as u32],
        );
        self.i32_consts.insert(value, id);
        id
    }

    fn const_u32(&mut self, value: u32) -> u32 {
        if let Some(&id) = self.u32_consts.get(&value) {
            return id;
        }
        let id = self.alloc_id();
        Self::emit_inst(&mut self.types_globals, 43, &[self.ty_u32, id, value]);
        self.u32_consts.insert(value, id);
        id
    }

    fn const_f32(&mut self, value: f32) -> u32 {
        let bits = value.to_bits();
        if let Some(&id) = self.f32_consts.get(&bits) {
            return id;
        }
        let id = self.alloc_id();
        // OpConstant (43)
        Self::emit_inst(&mut self.types_globals, 43, &[self.ty_f32, id, bits]);
        self.f32_consts.insert(bits, id);
        id
    }

    fn const_vec4(&mut self, v: [f32; 4]) -> u32 {
        let key = [
            v[0].to_bits(),
            v[1].to_bits(),
            v[2].to_bits(),
            v[3].to_bits(),
        ];
        if let Some(&id) = self.vec4_consts.get(&key) {
            return id;
        }
        let c0 = self.const_f32(v[0]);
        let c1 = self.const_f32(v[1]);
        let c2 = self.const_f32(v[2]);
        let c3 = self.const_f32(v[3]);
        let id = self.alloc_id();
        // OpConstantComposite (44)
        Self::emit_inst(
            &mut self.types_globals,
            44,
            &[self.ty_vec4, id, c0, c1, c2, c3],
        );
        self.vec4_consts.insert(key, id);
        id
    }

    fn get_or_declare_cbuffer(&mut self, slot: u32) -> u32 {
        let clamped = slot % MAX_CBUFFER_SLOTS;
        if let Some(&id) = self.cb_vars.get(&clamped) {
            return id;
        }
        let var_id = self.alloc_id();
        // OpVariable (59) %ty_ptr_uni_cb %var Uniform(2)
        Self::emit_inst(
            &mut self.types_globals,
            59,
            &[self.ty_ptr_uni_cb, var_id, 2],
        );
        // OpDecorate DescriptorSet(34) 0, Binding(33) <slot>
        Self::emit_inst(&mut self.annotations, 71, &[var_id, 34, 0]);
        Self::emit_inst(&mut self.annotations, 71, &[var_id, 33, clamped]);
        self.cb_vars.insert(clamped, var_id);
        self.used_cbuffers.insert(clamped);
        var_id
    }

    fn get_or_declare_texture(&mut self, slot: u32) -> u32 {
        let clamped = slot % MAX_TEXTURE_SLOTS;
        if let Some(&id) = self.tex_vars.get(&clamped) {
            return id;
        }
        let var_id = self.alloc_id();
        // OpVariable (59) %ty_ptr_uc_image %var UniformConstant(0)
        Self::emit_inst(
            &mut self.types_globals,
            59,
            &[self.ty_ptr_uc_image, var_id, 0],
        );
        Self::emit_inst(&mut self.annotations, 71, &[var_id, 34, 0]);
        Self::emit_inst(
            &mut self.annotations,
            71,
            &[var_id, 33, TEXTURE_BINDING_BASE + clamped],
        );
        self.tex_vars.insert(clamped, var_id);
        self.used_textures.insert(clamped);
        var_id
    }

    fn get_or_declare_sampler(&mut self, slot: u32) -> u32 {
        let clamped = slot % MAX_SAMPLER_SLOTS;
        if let Some(&id) = self.samp_vars.get(&clamped) {
            return id;
        }
        let var_id = self.alloc_id();
        // OpVariable (59) %ty_ptr_uc_sampler %var UniformConstant(0)
        Self::emit_inst(
            &mut self.types_globals,
            59,
            &[self.ty_ptr_uc_sampler, var_id, 0],
        );
        Self::emit_inst(&mut self.annotations, 71, &[var_id, 34, 0]);
        Self::emit_inst(
            &mut self.annotations,
            71,
            &[var_id, 33, SAMPLER_BINDING_BASE + clamped],
        );
        self.samp_vars.insert(clamped, var_id);
        self.used_samplers.insert(clamped);
        var_id
    }

    fn get_or_declare_storage_buffer(&mut self, slot: u32) -> (u32, u32) {
        let clamped = slot % MAX_STORAGE_SLOTS;
        let (ptr_ssbo_struct, ptr_ssbo_vec4) =
            if let (Some(ps), Some(pv)) = (self.ty_ptr_ssbo_struct, self.ty_ptr_ssbo_vec4) {
                (ps, pv)
            } else {
                let ty_rta = self.alloc_id();
                let ty_struct = self.alloc_id();
                let ty_ptr_s = self.alloc_id();
                let ty_ptr_v = self.alloc_id();
                // OpTypeRuntimeArray (29) %ty_rta %ty_vec4 + ArrayStride 16
                Self::emit_inst(&mut self.types_globals, 29, &[ty_rta, self.ty_vec4]);
                Self::emit_inst(&mut self.annotations, 71, &[ty_rta, 6, 16]);
                // OpTypeStruct (30) { vec4 data[]; } + Block + Offset 0
                Self::emit_inst(&mut self.types_globals, 30, &[ty_struct, ty_rta]);
                Self::emit_inst(&mut self.annotations, 71, &[ty_struct, 2]);
                Self::emit_inst(&mut self.annotations, 72, &[ty_struct, 0, 35, 0]);
                // OpTypePointer StorageBuffer(12)
                Self::emit_inst(&mut self.types_globals, 32, &[ty_ptr_s, 12, ty_struct]);
                Self::emit_inst(&mut self.types_globals, 32, &[ty_ptr_v, 12, self.ty_vec4]);
                self.ty_ptr_ssbo_struct = Some(ty_ptr_s);
                self.ty_ptr_ssbo_vec4 = Some(ty_ptr_v);
                (ty_ptr_s, ty_ptr_v)
            };

        if let Some(&id) = self.ssbo_vars.get(&clamped) {
            return (id, ptr_ssbo_vec4);
        }
        let var_id = self.alloc_id();
        // OpVariable (59) %ty_ptr_ssbo_struct %var StorageBuffer(12)
        Self::emit_inst(&mut self.types_globals, 59, &[ptr_ssbo_struct, var_id, 12]);
        Self::emit_inst(&mut self.annotations, 71, &[var_id, 34, 0]); // DescriptorSet 0
        Self::emit_inst(
            &mut self.annotations,
            71,
            &[var_id, 33, STORAGE_BINDING_BASE + clamped],
        ); // Binding 24 + slot
        self.ssbo_vars.insert(clamped, var_id);
        self.used_storage_buffers.insert(clamped);
        (var_id, ptr_ssbo_vec4)
    }

    fn load_vec4(&mut self, var_ptr: u32) -> u32 {
        let id = self.alloc_id();
        // OpLoad (61)
        Self::emit_inst(&mut self.body, 61, &[self.ty_vec4, id, var_ptr]);
        id
    }

    fn store_vec4(&mut self, var_ptr: u32, val: u32) {
        // OpStore (62)
        Self::emit_inst(&mut self.body, 62, &[var_ptr, val]);
    }

    fn swizzle_vec4(&mut self, val: u32, swz: [u32; 4]) -> u32 {
        if swz == [0, 1, 2, 3] {
            return val;
        }
        let id = self.alloc_id();
        // OpVectorShuffle (79)
        Self::emit_inst(
            &mut self.body,
            79,
            &[self.ty_vec4, id, val, val, swz[0], swz[1], swz[2], swz[3]],
        );
        id
    }

    fn apply_modifiers(&mut self, mut val: u32, abs: bool, negate: bool) -> u32 {
        if abs {
            let id = self.alloc_id();
            // OpExtInst (12) GLSL.std.450 FAbs (4)
            Self::emit_inst(
                &mut self.body,
                12,
                &[self.ty_vec4, id, self.ext_glsl, 4, val],
            );
            val = id;
        }
        if negate {
            let id = self.alloc_id();
            // OpFNegate (127)
            Self::emit_inst(&mut self.body, 127, &[self.ty_vec4, id, val]);
            val = id;
        }
        val
    }

    fn store_masked(&mut self, dst_ptr: u32, new_val: u32, mask: u8) {
        if (mask & 0xF) == 0xF {
            self.store_vec4(dst_ptr, new_val);
            return;
        }
        let old_val = self.load_vec4(dst_ptr);
        let c0 = if (mask & 1) != 0 { 4 } else { 0 };
        let c1 = if (mask & 2) != 0 { 5 } else { 1 };
        let c2 = if (mask & 4) != 0 { 6 } else { 2 };
        let c3 = if (mask & 8) != 0 { 7 } else { 3 };
        let merged = self.alloc_id();
        // OpVectorShuffle (79) : [0..3]=old_val, [4..7]=new_val
        Self::emit_inst(
            &mut self.body,
            79,
            &[self.ty_vec4, merged, old_val, new_val, c0, c1, c2, c3],
        );
        self.store_vec4(dst_ptr, merged);
    }
}

struct EmittedSpirv {
    spirv: Vec<u32>,
    used_cbuffers: Vec<u32>,
    used_textures: Vec<u32>,
    used_samplers: Vec<u32>,
    used_storage_buffers: Vec<u32>,
    thread_group_size: [u32; 3],
}

fn emit_spirv(
    stage: ShaderStage,
    inputs: &[SignatureElement],
    outputs: &[SignatureElement],
    inst_words: &[u32],
) -> Result<EmittedSpirv, ShaderTranslateError> {
    let mut spv = SpvEmitter {
        next_id: 1,
        annotations: Vec::new(),
        types_globals: Vec::new(),
        func_vars: Vec::new(),
        body: Vec::new(),
        f32_consts: HashMap::new(),
        i32_consts: HashMap::new(),
        u32_consts: HashMap::new(),
        vec4_consts: HashMap::new(),
        ty_bool: 0,
        ty_bvec4: 0,
        ty_f32: 0,
        ty_i32: 0,
        ty_u32: 0,
        ty_vec2: 0,
        ty_vec4: 0,
        ty_ivec4: 0,
        ty_uvec4: 0,
        ty_ptr_fn_vec4: 0,
        ty_ptr_uni_cb: 0,
        ty_ptr_uni_vec4: 0,
        ty_image_2d: 0,
        ty_sampler: 0,
        ty_sampled_image: 0,
        ty_ptr_uc_image: 0,
        ty_ptr_uc_sampler: 0,
        ty_ptr_ssbo_struct: None,
        ty_ptr_ssbo_vec4: None,
        ext_glsl: 0,
        const_zero_vec4: 0,
        const_uvec4_zero: 0,
        const_uvec4_all1: 0,
        cb_vars: HashMap::new(),
        tex_vars: HashMap::new(),
        samp_vars: HashMap::new(),
        ssbo_vars: HashMap::new(),
        used_cbuffers: BTreeSet::new(),
        used_textures: BTreeSet::new(),
        used_samplers: BTreeSet::new(),
        used_storage_buffers: BTreeSet::new(),
    };

    let ext_glsl = spv.alloc_id();
    spv.ext_glsl = ext_glsl;
    let main_fn_id = spv.alloc_id();

    let ty_void = spv.alloc_id();
    let ty_fn_void = spv.alloc_id();
    let ty_bool = spv.alloc_id();
    let ty_bvec4 = spv.alloc_id();
    let ty_f32 = spv.alloc_id();
    let ty_i32 = spv.alloc_id();
    let ty_u32 = spv.alloc_id();
    let ty_vec2 = spv.alloc_id();
    let ty_vec4 = spv.alloc_id();
    let ty_ivec4 = spv.alloc_id();
    let ty_uvec4 = spv.alloc_id();
    let ty_ptr_in_vec4 = spv.alloc_id();
    let ty_ptr_out_vec4 = spv.alloc_id();
    let ty_ptr_fn_vec4 = spv.alloc_id();

    spv.ty_bool = ty_bool;
    spv.ty_bvec4 = ty_bvec4;
    spv.ty_f32 = ty_f32;
    spv.ty_i32 = ty_i32;
    spv.ty_u32 = ty_u32;
    spv.ty_vec2 = ty_vec2;
    spv.ty_vec4 = ty_vec4;
    spv.ty_ivec4 = ty_ivec4;
    spv.ty_uvec4 = ty_uvec4;
    spv.ty_ptr_fn_vec4 = ty_ptr_fn_vec4;

    // Déclarations de types standards.
    SpvEmitter::emit_inst(&mut spv.types_globals, 19, &[ty_void]); // OpTypeVoid
    SpvEmitter::emit_inst(&mut spv.types_globals, 33, &[ty_fn_void, ty_void]); // OpTypeFunction
    SpvEmitter::emit_inst(&mut spv.types_globals, 20, &[ty_bool]); // OpTypeBool
    SpvEmitter::emit_inst(&mut spv.types_globals, 23, &[ty_bvec4, ty_bool, 4]); // bvec4
    SpvEmitter::emit_inst(&mut spv.types_globals, 22, &[ty_f32, 32]); // OpTypeFloat 32
    SpvEmitter::emit_inst(&mut spv.types_globals, 21, &[ty_i32, 32, 1]); // OpTypeInt 32 signed
    SpvEmitter::emit_inst(&mut spv.types_globals, 21, &[ty_u32, 32, 0]); // OpTypeInt 32 unsigned
    SpvEmitter::emit_inst(&mut spv.types_globals, 23, &[ty_vec2, ty_f32, 2]); // vec2
    SpvEmitter::emit_inst(&mut spv.types_globals, 23, &[ty_vec4, ty_f32, 4]); // vec4
    SpvEmitter::emit_inst(&mut spv.types_globals, 23, &[ty_ivec4, ty_i32, 4]); // ivec4
    SpvEmitter::emit_inst(&mut spv.types_globals, 23, &[ty_uvec4, ty_u32, 4]); // uvec4
    SpvEmitter::emit_inst(&mut spv.types_globals, 32, &[ty_ptr_in_vec4, 1, ty_vec4]); // Input
    SpvEmitter::emit_inst(&mut spv.types_globals, 32, &[ty_ptr_out_vec4, 3, ty_vec4]); // Output
    SpvEmitter::emit_inst(&mut spv.types_globals, 32, &[ty_ptr_fn_vec4, 7, ty_vec4]); // Function

    // Types Uniform Buffer (`cb0..cb3` : struct { vec4 data[256]; } en layout std140).
    let const_u32_cb_len = spv.const_u32(CBUFFER_VEC4_COUNT);
    let ty_arr_cb = spv.alloc_id();
    let ty_cb_struct = spv.alloc_id();
    let ty_ptr_uni_cb = spv.alloc_id();
    let ty_ptr_uni_vec4 = spv.alloc_id();
    SpvEmitter::emit_inst(
        &mut spv.types_globals,
        28,
        &[ty_arr_cb, ty_vec4, const_u32_cb_len],
    ); // OpTypeArray
    SpvEmitter::emit_inst(&mut spv.annotations, 71, &[ty_arr_cb, 6, 16]); // ArrayStride 16
    SpvEmitter::emit_inst(&mut spv.types_globals, 30, &[ty_cb_struct, ty_arr_cb]); // OpTypeStruct
    SpvEmitter::emit_inst(&mut spv.annotations, 71, &[ty_cb_struct, 2]); // Block
    SpvEmitter::emit_inst(&mut spv.annotations, 72, &[ty_cb_struct, 0, 35, 0]); // Offset 0
    SpvEmitter::emit_inst(
        &mut spv.types_globals,
        32,
        &[ty_ptr_uni_cb, 2, ty_cb_struct],
    ); // Uniform ptr
    SpvEmitter::emit_inst(&mut spv.types_globals, 32, &[ty_ptr_uni_vec4, 2, ty_vec4]);
    spv.ty_ptr_uni_cb = ty_ptr_uni_cb;
    spv.ty_ptr_uni_vec4 = ty_ptr_uni_vec4;

    // Types Texture 2D + Sampler (`t0..t3`, `s0..s3`).
    let ty_image_2d = spv.alloc_id();
    let ty_sampler = spv.alloc_id();
    let ty_sampled_image = spv.alloc_id();
    let ty_ptr_uc_image = spv.alloc_id();
    let ty_ptr_uc_sampler = spv.alloc_id();
    // OpTypeImage (25) : SampledType=f32, Dim=2D(1), Depth=0, Arrayed=0, MS=0, Sampled=1, Format=Unknown(0)
    SpvEmitter::emit_inst(
        &mut spv.types_globals,
        25,
        &[ty_image_2d, ty_f32, 1, 0, 0, 0, 1, 0],
    );
    SpvEmitter::emit_inst(&mut spv.types_globals, 26, &[ty_sampler]); // OpTypeSampler
    SpvEmitter::emit_inst(&mut spv.types_globals, 27, &[ty_sampled_image, ty_image_2d]); // OpTypeSampledImage
    SpvEmitter::emit_inst(
        &mut spv.types_globals,
        32,
        &[ty_ptr_uc_image, 0, ty_image_2d],
    );
    SpvEmitter::emit_inst(
        &mut spv.types_globals,
        32,
        &[ty_ptr_uc_sampler, 0, ty_sampler],
    );
    spv.ty_image_2d = ty_image_2d;
    spv.ty_sampler = ty_sampler;
    spv.ty_sampled_image = ty_sampled_image;
    spv.ty_ptr_uc_image = ty_ptr_uc_image;
    spv.ty_ptr_uc_sampler = ty_ptr_uc_sampler;

    let const_i32_0 = spv.const_i32(0);
    let const_u32_0 = spv.const_u32(0);
    let const_u32_all1 = spv.const_u32(u32::MAX);
    let const_uvec4_zero = spv.alloc_id();
    let const_uvec4_all1 = spv.alloc_id();
    SpvEmitter::emit_inst(
        &mut spv.types_globals,
        44,
        &[
            ty_uvec4,
            const_uvec4_zero,
            const_u32_0,
            const_u32_0,
            const_u32_0,
            const_u32_0,
        ],
    );
    SpvEmitter::emit_inst(
        &mut spv.types_globals,
        44,
        &[
            ty_uvec4,
            const_uvec4_all1,
            const_u32_all1,
            const_u32_all1,
            const_u32_all1,
            const_u32_all1,
        ],
    );
    spv.const_uvec4_zero = const_uvec4_zero;
    spv.const_uvec4_all1 = const_uvec4_all1;
    spv.const_zero_vec4 = spv.const_vec4([0.0, 0.0, 0.0, 0.0]);

    // Variables d'interface SPIR-V (`Input` et `Output`) dérivées de `ISGN` et `OSGN`.
    let mut interface_ids = Vec::new();
    let mut input_vars: HashMap<u32, u32> = HashMap::new();
    let mut builtin_u32_inputs: HashMap<u32, u32> = HashMap::new();
    let mut output_stage_vars: HashMap<u32, u32> = HashMap::new();
    let mut output_final_vars: Vec<(u32, u32, bool)> = Vec::new(); // (reg, target_var, is_pv_pos)

    // Variables BuiltIn Compute Shader (`GlobalInvocationId`, `WorkgroupId`, `LocalInvocationId`).
    let mut ty_uvec3 = 0u32;
    let mut cs_gid_var: Option<u32> = None;
    let mut cs_wgid_var: Option<u32> = None;
    let mut cs_lid_var: Option<u32> = None;
    if stage == ShaderStage::Compute {
        ty_uvec3 = spv.alloc_id();
        let ty_ptr_in_uvec3 = spv.alloc_id();
        SpvEmitter::emit_inst(&mut spv.types_globals, 23, &[ty_uvec3, ty_u32, 3]);
        SpvEmitter::emit_inst(&mut spv.types_globals, 32, &[ty_ptr_in_uvec3, 1, ty_uvec3]);

        let gid = spv.alloc_id();
        SpvEmitter::emit_inst(&mut spv.types_globals, 59, &[ty_ptr_in_uvec3, gid, 1]);
        SpvEmitter::emit_inst(&mut spv.annotations, 71, &[gid, 11, 28]); // BuiltIn GlobalInvocationId
        interface_ids.push(gid);
        cs_gid_var = Some(gid);

        let wgid = spv.alloc_id();
        SpvEmitter::emit_inst(&mut spv.types_globals, 59, &[ty_ptr_in_uvec3, wgid, 1]);
        SpvEmitter::emit_inst(&mut spv.annotations, 71, &[wgid, 11, 26]); // BuiltIn WorkgroupId
        interface_ids.push(wgid);
        cs_wgid_var = Some(wgid);

        let lid = spv.alloc_id();
        SpvEmitter::emit_inst(&mut spv.types_globals, 59, &[ty_ptr_in_uvec3, lid, 1]);
        SpvEmitter::emit_inst(&mut spv.annotations, 71, &[lid, 11, 27]); // BuiltIn LocalInvocationId
        interface_ids.push(lid);
        cs_lid_var = Some(lid);
    }

    // Bloc `gl_PerVertex` pour `SV_Position` en Vertex Shader (exigé par Vulkan SPIR-V).
    let mut pv_var_id: Option<u32> = None;
    if stage == ShaderStage::Vertex {
        let ty_pv = spv.alloc_id();
        let ty_ptr_pv = spv.alloc_id();
        let var_pv = spv.alloc_id();
        SpvEmitter::emit_inst(&mut spv.types_globals, 30, &[ty_pv, ty_vec4]); // OpTypeStruct
        SpvEmitter::emit_inst(&mut spv.types_globals, 32, &[ty_ptr_pv, 3, ty_pv]); // Output ptr
        SpvEmitter::emit_inst(&mut spv.types_globals, 59, &[ty_ptr_pv, var_pv, 3]); // OpVariable Output
        SpvEmitter::emit_inst(&mut spv.annotations, 71, &[ty_pv, 2]); // Block
        SpvEmitter::emit_inst(&mut spv.annotations, 72, &[ty_pv, 0, 11, 0]); // BuiltIn Position
        interface_ids.push(var_pv);
        pv_var_id = Some(var_pv);
    }

    let mut ty_ptr_in_u32: Option<u32> = None;
    let mut next_in_loc = 0u32;
    for inp in inputs {
        if input_vars.contains_key(&inp.register)
            || builtin_u32_inputs.contains_key(&inp.register)
            || inp.register == u32::MAX
        {
            continue;
        }
        let is_inst_id = stage == ShaderStage::Vertex
            && (inp.system_value_type == 8
                || inp.semantic_name.eq_ignore_ascii_case("SV_INSTANCEID"));
        let is_vert_id = stage == ShaderStage::Vertex
            && (inp.system_value_type == 6
                || inp.semantic_name.eq_ignore_ascii_case("SV_VERTEXID"));
        if is_inst_id || is_vert_id {
            let ptr_ty = *ty_ptr_in_u32.get_or_insert_with(|| {
                let id = spv.alloc_id();
                SpvEmitter::emit_inst(&mut spv.types_globals, 32, &[id, 1, ty_u32]);
                id
            });
            let var_id = spv.alloc_id();
            SpvEmitter::emit_inst(&mut spv.types_globals, 59, &[ptr_ty, var_id, 1]);
            let builtin = if is_inst_id { 43 } else { 42 }; // InstanceIndex=43, VertexIndex=42
            SpvEmitter::emit_inst(&mut spv.annotations, 71, &[var_id, 11, builtin]);
            interface_ids.push(var_id);
            builtin_u32_inputs.insert(inp.register, var_id);
            continue;
        }
        let var_id = spv.alloc_id();
        SpvEmitter::emit_inst(&mut spv.types_globals, 59, &[ty_ptr_in_vec4, var_id, 1]);
        if stage == ShaderStage::Pixel
            && (inp.system_value_type == 1 || inp.semantic_name.eq_ignore_ascii_case("SV_POSITION"))
        {
            // OpDecorate %var BuiltIn(11) FragCoord(15)
            SpvEmitter::emit_inst(&mut spv.annotations, 71, &[var_id, 11, 15]);
        } else {
            // OpDecorate %var Location(30) <loc>
            let loc = next_in_loc;
            next_in_loc += 1;
            SpvEmitter::emit_inst(&mut spv.annotations, 71, &[var_id, 30, loc]);
        }
        interface_ids.push(var_id);
        input_vars.insert(inp.register, var_id);
    }

    let mut next_out_loc = 0u32;
    for out in outputs {
        if output_stage_vars.contains_key(&out.register)
            || out.register == u32::MAX
            || out.semantic_name.eq_ignore_ascii_case("SV_DEPTH")
        {
            continue;
        }
        let local_id = spv.alloc_id();
        SpvEmitter::emit_inst(&mut spv.func_vars, 59, &[ty_ptr_fn_vec4, local_id, 7]);
        output_stage_vars.insert(out.register, local_id);

        let is_sv_pos = stage == ShaderStage::Vertex
            && (out.system_value_type == 1
                || out.semantic_name.eq_ignore_ascii_case("SV_POSITION"));

        if is_sv_pos && let Some(pv) = pv_var_id {
            output_final_vars.push((out.register, pv, true));
        } else {
            let var_id = spv.alloc_id();
            SpvEmitter::emit_inst(&mut spv.types_globals, 59, &[ty_ptr_out_vec4, var_id, 3]);
            let loc = if stage == ShaderStage::Pixel {
                out.register
            } else {
                let l = next_out_loc;
                next_out_loc += 1;
                l
            };
            SpvEmitter::emit_inst(&mut spv.annotations, 71, &[var_id, 30, loc]);
            interface_ids.push(var_id);
            output_final_vars.push((out.register, var_id, false));
        }
    }

    // Passe 1 : détermine le nombre de registres temporaires (`DCL_TEMPS`) et la taille de groupe (`DCL_THREAD_GROUP`).
    let mut num_temps = 0u32;
    let mut thread_group_size = [1u32, 1u32, 1u32];
    let mut scan = 0usize;
    while scan < inst_words.len() {
        let tok = inst_words[scan];
        let opcode = tok & 0x7FF;
        let len = ((tok >> 24) & 0x7F) as usize;
        let step = len.max(1);
        if opcode == OP_DCL_TEMPS && scan + 1 < inst_words.len() {
            num_temps = num_temps.max(inst_words[scan + 1]);
        } else if opcode == OP_DCL_THREAD_GROUP && scan + 3 < inst_words.len() {
            thread_group_size = [
                inst_words[scan + 1].max(1),
                inst_words[scan + 2].max(1),
                inst_words[scan + 3].max(1),
            ];
        }
        scan += step;
    }

    let mut temp_vars: Vec<u32> = Vec::with_capacity(num_temps as usize);
    for _ in 0..num_temps {
        let v = spv.alloc_id();
        SpvEmitter::emit_inst(&mut spv.func_vars, 59, &[ty_ptr_fn_vec4, v, 7]);
        temp_vars.push(v);
    }

    // Initialise chaque sortie et registre temporaire à `vec4(0.0)`.
    for &tv in &temp_vars {
        let z = spv.const_zero_vec4;
        spv.store_vec4(tv, z);
    }
    for &ov in output_stage_vars.values() {
        let z = spv.const_zero_vec4;
        spv.store_vec4(ov, z);
    }

    // Helpers de lecture/écriture d'opérandes DXBC.
    let read_src = |spv: &mut SpvEmitter, op: &DecodedOperand| -> u32 {
        match *op {
            DecodedOperand::Immediate(v) => spv.const_vec4(v),
            DecodedOperand::Input {
                index,
                swizzle,
                negate,
                abs,
            } => {
                let raw = if let Some(&var) = builtin_u32_inputs.get(&index) {
                    let u_val = spv.alloc_id();
                    SpvEmitter::emit_inst(&mut spv.body, 61, &[ty_u32, u_val, var]);
                    let f_bits = spv.alloc_id();
                    SpvEmitter::emit_inst(&mut spv.body, 124, &[ty_f32, f_bits, u_val]);
                    let zf = spv.const_f32(0.0);
                    let v4 = spv.alloc_id();
                    SpvEmitter::emit_inst(&mut spv.body, 80, &[ty_vec4, v4, f_bits, zf, zf, zf]);
                    v4
                } else if let Some(&var) = input_vars.get(&index) {
                    spv.load_vec4(var)
                } else {
                    spv.const_zero_vec4
                };
                let swz = spv.swizzle_vec4(raw, swizzle);
                spv.apply_modifiers(swz, abs, negate)
            }
            DecodedOperand::ThreadId { kind, swizzle } => {
                let var_opt = match kind {
                    OPERAND_INPUT_THREAD_ID => cs_gid_var,
                    OPERAND_INPUT_THREAD_GROUP_ID => cs_wgid_var,
                    OPERAND_INPUT_THREAD_ID_IN_GROUP => cs_lid_var,
                    _ => None,
                };
                let raw = if let Some(var) = var_opt {
                    let u3 = spv.alloc_id();
                    SpvEmitter::emit_inst(&mut spv.body, 61, &[ty_uvec3, u3, var]);
                    let ux = spv.alloc_id();
                    let uy = spv.alloc_id();
                    let uz = spv.alloc_id();
                    SpvEmitter::emit_inst(&mut spv.body, 81, &[ty_u32, ux, u3, 0]);
                    SpvEmitter::emit_inst(&mut spv.body, 81, &[ty_u32, uy, u3, 1]);
                    SpvEmitter::emit_inst(&mut spv.body, 81, &[ty_u32, uz, u3, 2]);
                    let u4 = spv.alloc_id();
                    SpvEmitter::emit_inst(
                        &mut spv.body,
                        80,
                        &[ty_uvec4, u4, ux, uy, uz, const_u32_0],
                    );
                    let f4 = spv.alloc_id();
                    SpvEmitter::emit_inst(&mut spv.body, 124, &[ty_vec4, f4, u4]);
                    f4
                } else {
                    spv.const_zero_vec4
                };
                spv.swizzle_vec4(raw, swizzle)
            }
            DecodedOperand::Temp {
                index,
                swizzle,
                negate,
                abs,
                ..
            } => {
                let raw = if let Some(&var) = temp_vars.get(index as usize) {
                    spv.load_vec4(var)
                } else {
                    spv.const_zero_vec4
                };
                let swz = spv.swizzle_vec4(raw, swizzle);
                spv.apply_modifiers(swz, abs, negate)
            }
            DecodedOperand::Output { index, .. } => {
                if let Some(&var) = output_stage_vars.get(&index) {
                    spv.load_vec4(var)
                } else {
                    spv.const_zero_vec4
                }
            }
            DecodedOperand::ConstantBuffer {
                slot,
                index,
                rel_temp,
                swizzle,
                negate,
                abs,
            } => {
                let cb_var = spv.get_or_declare_cbuffer(slot);
                let base_idx = spv.const_i32((index % CBUFFER_VEC4_COUNT) as i32);
                let final_idx = if let Some((t_reg, comp)) = rel_temp
                    && let Some(&t_var) = temp_vars.get(t_reg as usize)
                {
                    let tv = spv.load_vec4(t_var);
                    let comp_f = spv.alloc_id();
                    // OpCompositeExtract (81)
                    SpvEmitter::emit_inst(&mut spv.body, 81, &[ty_f32, comp_f, tv, comp & 3]);
                    let comp_i = spv.alloc_id();
                    // OpBitcast (124) f32 -> i32 (en SM4/SM5 les indices relatifs sont des entiers en registre)
                    SpvEmitter::emit_inst(&mut spv.body, 124, &[ty_i32, comp_i, comp_f]);
                    let sum = spv.alloc_id();
                    // OpIAdd (128)
                    SpvEmitter::emit_inst(&mut spv.body, 128, &[ty_i32, sum, comp_i, base_idx]);
                    let mask_255 = spv.const_i32((CBUFFER_VEC4_COUNT - 1) as i32);
                    let clamped = spv.alloc_id();
                    // OpBitwiseAnd (199) pour rester dans [0..255]
                    SpvEmitter::emit_inst(&mut spv.body, 199, &[ty_i32, clamped, sum, mask_255]);
                    clamped
                } else {
                    base_idx
                };
                let ptr_id = spv.alloc_id();
                // OpAccessChain (65) %ty_ptr_uni_vec4 %ptr %cb_var %const_i32_0 %final_idx
                SpvEmitter::emit_inst(
                    &mut spv.body,
                    65,
                    &[ty_ptr_uni_vec4, ptr_id, cb_var, const_i32_0, final_idx],
                );
                let raw = spv.load_vec4(ptr_id);
                let swz = spv.swizzle_vec4(raw, swizzle);
                spv.apply_modifiers(swz, abs, negate)
            }
            _ => spv.const_zero_vec4,
        }
    };

    let write_dst = |spv: &mut SpvEmitter, op: &DecodedOperand, val: u32| match *op {
        DecodedOperand::Temp { index, mask, .. } => {
            if let Some(&var) = temp_vars.get(index as usize) {
                spv.store_masked(var, val, mask);
            }
        }
        DecodedOperand::Output { index, mask } => {
            if let Some(&var) = output_stage_vars.get(&index) {
                spv.store_masked(var, val, mask);
            }
        }
        _ => {}
    };

    // Passe 2 : traduction instruction par instruction du bytecode SM4/SM5.
    let mut pc = 0usize;
    while pc < inst_words.len() {
        let tok = inst_words[pc];
        let opcode = tok & 0x7FF;
        let len = ((tok >> 24) & 0x7F) as usize;
        if len == 0 || pc + len > inst_words.len() {
            return Err(ShaderTranslateError::TruncatedInstruction(pc));
        }
        let end = pc + len;
        let inst_slice = &inst_words[..end];
        let saturate = ((tok >> 13) & 1) != 0;
        let mut cur = pc + 1;
        // Saute les tokens d'opcode étendus éventuels.
        let mut has_ext = (tok >> 31) != 0;
        while has_ext && cur < end {
            has_ext = (inst_slice[cur] >> 31) != 0;
            cur += 1;
        }

        let maybe_sat = |spv: &mut SpvEmitter, val: u32| -> u32 {
            if !saturate {
                return val;
            }
            let z = spv.const_vec4([0.0, 0.0, 0.0, 0.0]);
            let one = spv.const_vec4([1.0, 1.0, 1.0, 1.0]);
            let clamped = spv.alloc_id();
            // GLSL.std.450 FClamp = 43
            SpvEmitter::emit_inst(
                &mut spv.body,
                12,
                &[ty_vec4, clamped, ext_glsl, 43, val, z, one],
            );
            clamped
        };

        match opcode {
            OP_MOV => {
                if let (Some(dst), Some(src)) = (
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                ) {
                    let v = read_src(&mut spv, &src);
                    let v = maybe_sat(&mut spv, v);
                    write_dst(&mut spv, &dst, v);
                }
            }
            OP_MOVC => {
                if let (Some(dst), Some(cond), Some(a), Some(b)) = (
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                ) {
                    let vc = read_src(&mut spv, &cond);
                    let va = read_src(&mut spv, &a);
                    let vb = read_src(&mut spv, &b);
                    let uc = spv.alloc_id();
                    // OpBitcast (124) vec4 -> uvec4
                    SpvEmitter::emit_inst(&mut spv.body, 124, &[ty_uvec4, uc, vc]);
                    let bc = spv.alloc_id();
                    // OpINotEqual (171) uvec4 != 0 -> bvec4
                    SpvEmitter::emit_inst(
                        &mut spv.body,
                        171,
                        &[ty_bvec4, bc, uc, const_uvec4_zero],
                    );
                    let res = spv.alloc_id();
                    // OpSelect (169)
                    SpvEmitter::emit_inst(&mut spv.body, 169, &[ty_vec4, res, bc, va, vb]);
                    let res = maybe_sat(&mut spv, res);
                    write_dst(&mut spv, &dst, res);
                }
            }
            OP_ADD => {
                if let (Some(dst), Some(a), Some(b)) = (
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                ) {
                    let va = read_src(&mut spv, &a);
                    let vb = read_src(&mut spv, &b);
                    let res = spv.alloc_id();
                    // OpFAdd (129)
                    SpvEmitter::emit_inst(&mut spv.body, 129, &[ty_vec4, res, va, vb]);
                    let res = maybe_sat(&mut spv, res);
                    write_dst(&mut spv, &dst, res);
                }
            }
            OP_MUL | OP_DIV => {
                if let (Some(dst), Some(a), Some(b)) = (
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                ) {
                    let va = read_src(&mut spv, &a);
                    let vb = read_src(&mut spv, &b);
                    let res = spv.alloc_id();
                    // OpFMul (133) / OpFDiv (136)
                    let spv_op = if opcode == OP_MUL { 133 } else { 136 };
                    SpvEmitter::emit_inst(&mut spv.body, spv_op, &[ty_vec4, res, va, vb]);
                    let res = maybe_sat(&mut spv, res);
                    write_dst(&mut spv, &dst, res);
                }
            }
            OP_MAD => {
                if let (Some(dst), Some(a), Some(b), Some(c)) = (
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                ) {
                    let va = read_src(&mut spv, &a);
                    let vb = read_src(&mut spv, &b);
                    let vc = read_src(&mut spv, &c);
                    let mul_id = spv.alloc_id();
                    let add_id = spv.alloc_id();
                    SpvEmitter::emit_inst(&mut spv.body, 133, &[ty_vec4, mul_id, va, vb]);
                    SpvEmitter::emit_inst(&mut spv.body, 129, &[ty_vec4, add_id, mul_id, vc]);
                    let res = maybe_sat(&mut spv, add_id);
                    write_dst(&mut spv, &dst, res);
                }
            }
            OP_DP4 | OP_DP3 | OP_DP2 => {
                if let (Some(dst), Some(a), Some(b)) = (
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                ) {
                    let va = read_src(&mut spv, &a);
                    let vb = read_src(&mut spv, &b);
                    let prod = if opcode == OP_DP4 {
                        let dot = spv.alloc_id();
                        // OpDot (148)
                        SpvEmitter::emit_inst(&mut spv.body, 148, &[ty_f32, dot, va, vb]);
                        dot
                    } else {
                        let mask_arr = if opcode == OP_DP3 {
                            [1.0, 1.0, 1.0, 0.0]
                        } else {
                            [1.0, 1.0, 0.0, 0.0]
                        };
                        let m = spv.const_vec4(mask_arr);
                        let ma = spv.alloc_id();
                        SpvEmitter::emit_inst(&mut spv.body, 133, &[ty_vec4, ma, va, m]);
                        let dot = spv.alloc_id();
                        SpvEmitter::emit_inst(&mut spv.body, 148, &[ty_f32, dot, ma, vb]);
                        dot
                    };
                    let splat = spv.alloc_id();
                    // OpCompositeConstruct (80)
                    SpvEmitter::emit_inst(
                        &mut spv.body,
                        80,
                        &[ty_vec4, splat, prod, prod, prod, prod],
                    );
                    let res = maybe_sat(&mut spv, splat);
                    write_dst(&mut spv, &dst, res);
                }
            }
            OP_MIN | OP_MAX => {
                if let (Some(dst), Some(a), Some(b)) = (
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                ) {
                    let va = read_src(&mut spv, &a);
                    let vb = read_src(&mut spv, &b);
                    let res = spv.alloc_id();
                    let glsl_op = if opcode == OP_MIN { 37 } else { 40 }; // FMin=37, FMax=40
                    SpvEmitter::emit_inst(
                        &mut spv.body,
                        12,
                        &[ty_vec4, res, ext_glsl, glsl_op, va, vb],
                    );
                    let res = maybe_sat(&mut spv, res);
                    write_dst(&mut spv, &dst, res);
                }
            }
            OP_SQRT | OP_RSQ | OP_FRC | OP_EXP | OP_LOG | OP_ROUND_NE | OP_ROUND_NI
            | OP_ROUND_PI | OP_ROUND_Z => {
                if let (Some(dst), Some(a)) = (
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                ) {
                    let va = read_src(&mut spv, &a);
                    let res = spv.alloc_id();
                    let glsl_op = match opcode {
                        OP_SQRT => 31,    // Sqrt
                        OP_RSQ => 32,     // InverseSqrt
                        OP_FRC => 10,     // Fract
                        OP_EXP => 29,     // Exp2
                        OP_LOG => 30,     // Log2
                        OP_ROUND_NE => 2, // RoundEven
                        OP_ROUND_NI => 8, // Floor
                        OP_ROUND_PI => 9, // Ceil
                        _ => 3,           // Trunc
                    };
                    SpvEmitter::emit_inst(
                        &mut spv.body,
                        12,
                        &[ty_vec4, res, ext_glsl, glsl_op, va],
                    );
                    let res = maybe_sat(&mut spv, res);
                    write_dst(&mut spv, &dst, res);
                }
            }
            OP_SINCOS => {
                if let (Some(dst_sin), Some(dst_cos), Some(src)) = (
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                ) {
                    let va = read_src(&mut spv, &src);
                    if !matches!(dst_sin, DecodedOperand::Null) {
                        let s = spv.alloc_id();
                        // GLSL.std.450 Sin = 13
                        SpvEmitter::emit_inst(&mut spv.body, 12, &[ty_vec4, s, ext_glsl, 13, va]);
                        let s = maybe_sat(&mut spv, s);
                        write_dst(&mut spv, &dst_sin, s);
                    }
                    if !matches!(dst_cos, DecodedOperand::Null) {
                        let c = spv.alloc_id();
                        // GLSL.std.450 Cos = 14
                        SpvEmitter::emit_inst(&mut spv.body, 12, &[ty_vec4, c, ext_glsl, 14, va]);
                        let c = maybe_sat(&mut spv, c);
                        write_dst(&mut spv, &dst_cos, c);
                    }
                }
            }
            OP_LT | OP_GE | OP_EQ | OP_NE => {
                if let (Some(dst), Some(a), Some(b)) = (
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                ) {
                    let va = read_src(&mut spv, &a);
                    let vb = read_src(&mut spv, &b);
                    let cmp_op = match opcode {
                        OP_LT => 184, // OpFOrdLessThan
                        OP_GE => 190, // OpFOrdGreaterThanEqual
                        OP_EQ => 180, // OpFOrdEqual
                        _ => 183,     // OpFUnordNotEqual
                    };
                    let bres = spv.alloc_id();
                    SpvEmitter::emit_inst(&mut spv.body, cmp_op, &[ty_bvec4, bres, va, vb]);
                    let ures = spv.alloc_id();
                    SpvEmitter::emit_inst(
                        &mut spv.body,
                        169,
                        &[ty_uvec4, ures, bres, const_uvec4_all1, const_uvec4_zero],
                    );
                    let fres = spv.alloc_id();
                    SpvEmitter::emit_inst(&mut spv.body, 124, &[ty_vec4, fres, ures]);
                    write_dst(&mut spv, &dst, fres);
                }
            }
            OP_AND | OP_OR | OP_XOR | OP_IADD => {
                if let (Some(dst), Some(a), Some(b)) = (
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                ) {
                    let va = read_src(&mut spv, &a);
                    let vb = read_src(&mut spv, &b);
                    let ua = spv.alloc_id();
                    let ub = spv.alloc_id();
                    SpvEmitter::emit_inst(&mut spv.body, 124, &[ty_uvec4, ua, va]);
                    SpvEmitter::emit_inst(&mut spv.body, 124, &[ty_uvec4, ub, vb]);
                    let int_op = match opcode {
                        OP_AND => 199,  // OpBitwiseAnd
                        OP_OR => 197,   // OpBitwiseOr
                        OP_IADD => 128, // OpIAdd
                        _ => 198,       // OpBitwiseXor
                    };
                    let ures = spv.alloc_id();
                    SpvEmitter::emit_inst(&mut spv.body, int_op, &[ty_uvec4, ures, ua, ub]);
                    let fres = spv.alloc_id();
                    SpvEmitter::emit_inst(&mut spv.body, 124, &[ty_vec4, fres, ures]);
                    write_dst(&mut spv, &dst, fres);
                }
            }
            OP_ITOF | OP_UTOF | OP_FTOI | OP_FTOU => {
                if let (Some(dst), Some(a)) = (
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                ) {
                    let va = read_src(&mut spv, &a);
                    let res = match opcode {
                        OP_ITOF => {
                            let iv = spv.alloc_id();
                            let fv = spv.alloc_id();
                            SpvEmitter::emit_inst(&mut spv.body, 124, &[ty_ivec4, iv, va]);
                            SpvEmitter::emit_inst(&mut spv.body, 111, &[ty_vec4, fv, iv]);
                            fv
                        }
                        OP_UTOF => {
                            let uv = spv.alloc_id();
                            let fv = spv.alloc_id();
                            SpvEmitter::emit_inst(&mut spv.body, 124, &[ty_uvec4, uv, va]);
                            SpvEmitter::emit_inst(&mut spv.body, 112, &[ty_vec4, fv, uv]);
                            fv
                        }
                        OP_FTOI => {
                            let iv = spv.alloc_id();
                            let fv = spv.alloc_id();
                            SpvEmitter::emit_inst(&mut spv.body, 110, &[ty_ivec4, iv, va]);
                            SpvEmitter::emit_inst(&mut spv.body, 124, &[ty_vec4, fv, iv]);
                            fv
                        }
                        _ => {
                            let uv = spv.alloc_id();
                            let fv = spv.alloc_id();
                            SpvEmitter::emit_inst(&mut spv.body, 109, &[ty_uvec4, uv, va]);
                            SpvEmitter::emit_inst(&mut spv.body, 124, &[ty_vec4, fv, uv]);
                            fv
                        }
                    };
                    write_dst(&mut spv, &dst, res);
                }
            }
            OP_DERIV_RTX | OP_DERIV_RTY if stage == ShaderStage::Pixel => {
                if let (Some(dst), Some(a)) = (
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                ) {
                    let va = read_src(&mut spv, &a);
                    let res = spv.alloc_id();
                    let spv_op = if opcode == OP_DERIV_RTX { 207 } else { 208 }; // OpDPdx / OpDPdy
                    SpvEmitter::emit_inst(&mut spv.body, spv_op, &[ty_vec4, res, va]);
                    let res = maybe_sat(&mut spv, res);
                    write_dst(&mut spv, &dst, res);
                }
            }
            OP_SAMPLE | OP_SAMPLE_L => {
                if let (Some(dst), Some(coord), Some(res_op), Some(samp_op)) = (
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                    decode_operand(inst_slice, &mut cur),
                ) {
                    let lod_op = if opcode == OP_SAMPLE_L {
                        decode_operand(inst_slice, &mut cur)
                    } else {
                        None
                    };
                    let (tex_slot, tex_swz) = match res_op {
                        DecodedOperand::Resource { slot, swizzle } => (slot, swizzle),
                        _ => (0, [0, 1, 2, 3]),
                    };
                    let samp_slot = match samp_op {
                        DecodedOperand::Sampler { slot } => slot,
                        _ => 0,
                    };
                    let tex_var = spv.get_or_declare_texture(tex_slot);
                    let samp_var = spv.get_or_declare_sampler(samp_slot);
                    let coord_v4 = read_src(&mut spv, &coord);
                    let uv = spv.alloc_id();
                    // OpVectorShuffle (79) vec4 -> vec2 (xy)
                    SpvEmitter::emit_inst(
                        &mut spv.body,
                        79,
                        &[ty_vec2, uv, coord_v4, coord_v4, 0, 1],
                    );
                    let img_val = spv.alloc_id();
                    let smp_val = spv.alloc_id();
                    let combined = spv.alloc_id();
                    SpvEmitter::emit_inst(&mut spv.body, 61, &[ty_image_2d, img_val, tex_var]);
                    SpvEmitter::emit_inst(&mut spv.body, 61, &[ty_sampler, smp_val, samp_var]);
                    // OpSampledImage (86)
                    SpvEmitter::emit_inst(
                        &mut spv.body,
                        86,
                        &[ty_sampled_image, combined, img_val, smp_val],
                    );
                    let sampled = spv.alloc_id();
                    if stage == ShaderStage::Pixel && opcode == OP_SAMPLE {
                        // OpImageSampleImplicitLod (87)
                        SpvEmitter::emit_inst(&mut spv.body, 87, &[ty_vec4, sampled, combined, uv]);
                    } else {
                        let lod_f32 = if let Some(ref lop) = lod_op {
                            let lv = read_src(&mut spv, lop);
                            let lf = spv.alloc_id();
                            SpvEmitter::emit_inst(&mut spv.body, 81, &[ty_f32, lf, lv, 0]);
                            lf
                        } else {
                            spv.const_f32(0.0)
                        };
                        // OpImageSampleExplicitLod (88) avec ImageOperand Lod (0x2)
                        SpvEmitter::emit_inst(
                            &mut spv.body,
                            88,
                            &[ty_vec4, sampled, combined, uv, 2, lod_f32],
                        );
                    }
                    let swz = spv.swizzle_vec4(sampled, tex_swz);
                    let res = maybe_sat(&mut spv, swz);
                    write_dst(&mut spv, &dst, res);
                }
            }
            OP_LD_STRUCTURED | OP_LD_RAW => {
                let decoded = if opcode == OP_LD_STRUCTURED {
                    (
                        decode_operand(inst_slice, &mut cur),
                        decode_operand(inst_slice, &mut cur),
                        decode_operand(inst_slice, &mut cur),
                        decode_operand(inst_slice, &mut cur),
                    )
                } else {
                    (
                        decode_operand(inst_slice, &mut cur),
                        None,
                        decode_operand(inst_slice, &mut cur),
                        decode_operand(inst_slice, &mut cur),
                    )
                };
                if let (Some(dst), struct_idx_op, Some(byte_off_op), Some(buf_op)) = decoded {
                    let (slot, swz) = match buf_op {
                        DecodedOperand::UnorderedAccessView { slot, swizzle }
                        | DecodedOperand::Resource { slot, swizzle } => (slot, swizzle),
                        _ => (0, [0, 1, 2, 3]),
                    };
                    let read_u32_idx = |spv: &mut SpvEmitter, op: &DecodedOperand| -> u32 {
                        if let DecodedOperand::Immediate(v) = *op {
                            return spv.const_u32(v[0].to_bits());
                        }
                        let v4 = read_src(spv, op);
                        let f0 = spv.alloc_id();
                        SpvEmitter::emit_inst(&mut spv.body, 81, &[ty_f32, f0, v4, 0]);
                        let u0 = spv.alloc_id();
                        SpvEmitter::emit_inst(&mut spv.body, 124, &[ty_u32, u0, f0]);
                        u0
                    };
                    let byte_u = read_u32_idx(&mut spv, &byte_off_op);
                    let shift4 = spv.const_u32(4);
                    let off_v4 = spv.alloc_id();
                    // OpShiftRightLogical (194) : byte_offset / 16
                    SpvEmitter::emit_inst(&mut spv.body, 194, &[ty_u32, off_v4, byte_u, shift4]);
                    let elem_idx = if let Some(ref s_op) = struct_idx_op {
                        let s_u = read_u32_idx(&mut spv, s_op);
                        let sum = spv.alloc_id();
                        SpvEmitter::emit_inst(&mut spv.body, 128, &[ty_u32, sum, s_u, off_v4]);
                        sum
                    } else {
                        off_v4
                    };
                    let (ssbo_var, ty_ptr_ssbo_vec4) = spv.get_or_declare_storage_buffer(slot);
                    let ptr_id = spv.alloc_id();
                    SpvEmitter::emit_inst(
                        &mut spv.body,
                        65,
                        &[ty_ptr_ssbo_vec4, ptr_id, ssbo_var, const_i32_0, elem_idx],
                    );
                    let raw = spv.load_vec4(ptr_id);
                    let val = spv.swizzle_vec4(raw, swz);
                    write_dst(&mut spv, &dst, val);
                }
            }
            OP_STORE_STRUCTURED | OP_STORE_RAW => {
                let decoded = if opcode == OP_STORE_STRUCTURED {
                    (
                        decode_operand(inst_slice, &mut cur),
                        decode_operand(inst_slice, &mut cur),
                        decode_operand(inst_slice, &mut cur),
                        decode_operand(inst_slice, &mut cur),
                    )
                } else {
                    (
                        decode_operand(inst_slice, &mut cur),
                        None,
                        decode_operand(inst_slice, &mut cur),
                        decode_operand(inst_slice, &mut cur),
                    )
                };
                if let (Some(uav_op), struct_idx_op, Some(byte_off_op), Some(src_op)) = decoded {
                    let slot = match uav_op {
                        DecodedOperand::UnorderedAccessView { slot, .. }
                        | DecodedOperand::Resource { slot, .. } => slot,
                        _ => 0,
                    };
                    let read_u32_idx = |spv: &mut SpvEmitter, op: &DecodedOperand| -> u32 {
                        if let DecodedOperand::Immediate(v) = *op {
                            return spv.const_u32(v[0].to_bits());
                        }
                        let v4 = read_src(spv, op);
                        let f0 = spv.alloc_id();
                        SpvEmitter::emit_inst(&mut spv.body, 81, &[ty_f32, f0, v4, 0]);
                        let u0 = spv.alloc_id();
                        SpvEmitter::emit_inst(&mut spv.body, 124, &[ty_u32, u0, f0]);
                        u0
                    };
                    let byte_u = read_u32_idx(&mut spv, &byte_off_op);
                    let shift4 = spv.const_u32(4);
                    let off_v4 = spv.alloc_id();
                    SpvEmitter::emit_inst(&mut spv.body, 194, &[ty_u32, off_v4, byte_u, shift4]);
                    let elem_idx = if let Some(ref s_op) = struct_idx_op {
                        let s_u = read_u32_idx(&mut spv, s_op);
                        let sum = spv.alloc_id();
                        SpvEmitter::emit_inst(&mut spv.body, 128, &[ty_u32, sum, s_u, off_v4]);
                        sum
                    } else {
                        off_v4
                    };
                    let val = read_src(&mut spv, &src_op);
                    let (ssbo_var, ty_ptr_ssbo_vec4) = spv.get_or_declare_storage_buffer(slot);
                    let ptr_id = spv.alloc_id();
                    SpvEmitter::emit_inst(
                        &mut spv.body,
                        65,
                        &[ty_ptr_ssbo_vec4, ptr_id, ssbo_var, const_i32_0, elem_idx],
                    );
                    spv.store_vec4(ptr_id, val);
                }
            }
            OP_RET => break,
            _ => {
                // Déclarations (`DCL_*`) déjà exploitées via ISGN/OSGN ou ignorées proprement.
            }
        }
        pc = end;
    }

    // Copie des sorties locales vers les variables `Output` SPIR-V finales.
    for (reg, target_var, is_pv_pos) in output_final_vars {
        if let Some(&local_var) = output_stage_vars.get(&reg) {
            let val = spv.load_vec4(local_var);
            if is_pv_pos {
                let ptr_pos = spv.alloc_id();
                // OpAccessChain (65) %ty_ptr_out_vec4 %ptr_pos %gl_PerVertex %const_i32_0
                SpvEmitter::emit_inst(
                    &mut spv.body,
                    65,
                    &[ty_ptr_out_vec4, ptr_pos, target_var, const_i32_0],
                );
                spv.store_vec4(ptr_pos, val);
            } else {
                spv.store_vec4(target_var, val);
            }
        }
    }

    // Assemblage final du module SPIR-V 1.3 (ordre de sections strict exigé par `spirv-val`).
    let label_entry = spv.alloc_id();
    // 1. En-tête (5 mots) : Magic SPIR-V, Version 1.3, Generator, Bound, Schema
    let mut out = vec![0x0723_0203, 0x0001_0300, 0x0000_0000, spv.next_id, 0];

    // 2. Capabilities
    SpvEmitter::emit_inst(&mut out, 17, &[1]); // OpCapability Shader

    // 3. ExtInstImport "GLSL.std.450"
    let mut glsl_ops = vec![ext_glsl];
    SpvEmitter::push_str_words(&mut glsl_ops, "GLSL.std.450");
    SpvEmitter::emit_inst(&mut out, 11, &glsl_ops);

    // 4. MemoryModel Logical GLSL450
    SpvEmitter::emit_inst(&mut out, 14, &[0, 1]);

    // 5. EntryPoint (SPIR-V 1.3 : uniquement les variables Input/Output dans `interface_ids`)
    let exec_model = match stage {
        ShaderStage::Vertex => 0,  // Vertex
        ShaderStage::Pixel => 4,   // Fragment
        ShaderStage::Compute => 5, // GLCompute
        _ => 0,
    };
    let mut ep_ops = vec![exec_model, main_fn_id];
    SpvEmitter::push_str_words(&mut ep_ops, "main");
    ep_ops.extend_from_slice(&interface_ids);
    SpvEmitter::emit_inst(&mut out, 15, &ep_ops);

    // 6. ExecutionMode (Fragment requiert OriginUpperLeft=7 ; Compute requiert LocalSize=17)
    if stage == ShaderStage::Pixel {
        SpvEmitter::emit_inst(&mut out, 16, &[main_fn_id, 7]);
    } else if stage == ShaderStage::Compute {
        SpvEmitter::emit_inst(
            &mut out,
            16,
            &[
                main_fn_id,
                17,
                thread_group_size[0],
                thread_group_size[1],
                thread_group_size[2],
            ],
        );
    }

    // 7. Annotations (OpDecorate / OpMemberDecorate)
    out.extend_from_slice(&spv.annotations);

    // 8. Types, constantes & variables globales
    out.extend_from_slice(&spv.types_globals);

    // 9. Fonction `main()`
    SpvEmitter::emit_inst(&mut out, 54, &[ty_void, main_fn_id, 0, ty_fn_void]); // OpFunction
    SpvEmitter::emit_inst(&mut out, 248, &[label_entry]); // OpLabel
    out.extend_from_slice(&spv.func_vars);
    out.extend_from_slice(&spv.body);
    SpvEmitter::emit_inst(&mut out, 253, &[]); // OpReturn
    SpvEmitter::emit_inst(&mut out, 56, &[]); // OpFunctionEnd

    Ok(EmittedSpirv {
        spirv: out,
        used_cbuffers: spv.used_cbuffers.into_iter().collect(),
        used_textures: spv.used_textures.into_iter().collect(),
        used_samplers: spv.used_samplers.into_iter().collect(),
        used_storage_buffers: spv.used_storage_buffers.into_iter().collect(),
        thread_group_size,
    })
}

// ==================== Constructeurs DXBC (pour tests et harnais) ====================

/// Construit un conteneur DXBC SM5.0 complet (`ISGN` + `OSGN` + `SHEX`) prêt pour D3D12/Vulkan.
pub fn build_dxbc_container(
    stage: ShaderStage,
    inputs: &[(&str, u32, u32, u32)],
    outputs: &[(&str, u32, u32, u32)],
    instructions: &[u32],
) -> Vec<u8> {
    let isgn = build_sig_chunk(inputs);
    let osgn = build_sig_chunk(outputs);

    let kind: u32 = match stage {
        ShaderStage::Pixel => 0,
        ShaderStage::Vertex => 1,
        ShaderStage::Compute => 5,
        _ => 1,
    };
    let version = (kind << 16) | 0x0050; // SM 5.0
    let total_words = (2 + instructions.len()) as u32;
    let mut shex = Vec::with_capacity((total_words as usize) * 4);
    shex.extend_from_slice(&version.to_le_bytes());
    shex.extend_from_slice(&total_words.to_le_bytes());
    for &w in instructions {
        shex.extend_from_slice(&w.to_le_bytes());
    }

    let chunks: [(&[u8; 4], &[u8]); 3] = [(b"ISGN", &isgn), (b"OSGN", &osgn), (b"SHEX", &shex)];
    let header_len = 32 + chunks.len() * 4;
    let mut offsets = Vec::with_capacity(chunks.len());
    let mut cur = header_len;
    for (_, data) in &chunks {
        offsets.push(cur as u32);
        cur += 8 + data.len();
    }

    let mut out = Vec::with_capacity(cur);
    out.extend_from_slice(b"DXBC");
    out.extend_from_slice(&[0u8; 16]); // hash placeholder
    out.extend_from_slice(&1u32.to_le_bytes()); // version
    out.extend_from_slice(&(cur as u32).to_le_bytes()); // total_size
    out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
    for o in offsets {
        out.extend_from_slice(&o.to_le_bytes());
    }
    for (fourcc, data) in chunks {
        out.extend_from_slice(fourcc);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
    }
    out
}

fn build_sig_chunk(elems: &[(&str, u32, u32, u32)]) -> Vec<u8> {
    let table_end = 8 + elems.len() * 24;
    let mut strings = Vec::new();
    let mut name_offsets = Vec::with_capacity(elems.len());
    for &(name, _, _, _) in elems {
        name_offsets.push((table_end + strings.len()) as u32);
        strings.extend_from_slice(name.as_bytes());
        strings.push(0);
    }
    while !strings.len().is_multiple_of(4) {
        strings.push(0);
    }

    let mut out = Vec::with_capacity(table_end + strings.len());
    out.extend_from_slice(&(elems.len() as u32).to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes());
    for (i, &(_, sidx, sv, reg)) in elems.iter().enumerate() {
        out.extend_from_slice(&name_offsets[i].to_le_bytes());
        out.extend_from_slice(&sidx.to_le_bytes());
        out.extend_from_slice(&sv.to_le_bytes());
        out.extend_from_slice(&3u32.to_le_bytes()); // float32
        out.extend_from_slice(&reg.to_le_bytes());
        out.extend_from_slice(&[0x0F, 0x0F, 0, 0]);
    }
    out.extend_from_slice(&strings);
    out
}

/// Construit un Vertex Shader DXBC SM5.0 (passthrough `POSITION -> SV_Position`, `COLOR -> COLOR`).
pub fn sample_triangle_vs_dxbc() -> Vec<u8> {
    let dst_o = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_OUTPUT << 12) | (1 << 20) };
    let src_v = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_INPUT << 12) | (1 << 20);
    let inst_mov = (5u32 << 24) | OP_MOV;
    let inst_ret = (1u32 << 24) | OP_RET;

    let code = [
        inst_mov,
        dst_o(0xF),
        0,
        src_v,
        0,
        inst_mov,
        dst_o(0xF),
        1,
        src_v,
        1,
        inst_ret,
    ];
    build_dxbc_container(
        ShaderStage::Vertex,
        &[("POSITION", 0, 0, 0), ("COLOR", 0, 0, 1)],
        &[("SV_POSITION", 0, 1, 0), ("COLOR", 0, 0, 1)],
        &code,
    )
}

/// Construit un Pixel Shader DXBC SM5.0 (copie la couleur interpolée `v1 -> o0 SV_Target`).
pub fn sample_triangle_ps_dxbc() -> Vec<u8> {
    let dst_o = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_OUTPUT << 12) | (1 << 20) };
    let src_v = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_INPUT << 12) | (1 << 20);
    let inst_mov = (5u32 << 24) | OP_MOV;
    let inst_ret = (1u32 << 24) | OP_RET;

    let code = [inst_mov, dst_o(0xF), 0, src_v, 1, inst_ret];
    build_dxbc_container(
        ShaderStage::Pixel,
        &[("SV_POSITION", 0, 1, 0), ("COLOR", 0, 0, 1)],
        &[("SV_Target", 0, 0, 0)],
        &code,
    )
}

/// Construit un Vertex Shader DXBC SM5.0 3D complet :
/// multiplie `v0` (`POSITION`) par la matrice 4×4 `cb0[0..4]` (4 × `dp4`) et transmet `v1` (`TEXCOORD`) et `v2` (`COLOR`).
pub fn sample_textured_cube_vs_dxbc() -> Vec<u8> {
    let dst_o = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_OUTPUT << 12) | (1 << 20) };
    let src_v = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_INPUT << 12) | (1 << 20);
    // cb0[idx] : 4 comp (2), swizzle (1<<2), xyzw (0xE4<<4), CB (8<<12), 2D (2<<20)
    let src_cb = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_CONSTANT_BUFFER << 12) | (2 << 20);
    let inst_dp4 = (8u32 << 24) | OP_DP4;
    let inst_mov = (5u32 << 24) | OP_MOV;
    let inst_ret = (1u32 << 24) | OP_RET;

    let code = [
        // dp4 o0.x, v0, cb0[0]
        inst_dp4,
        dst_o(0x1),
        0,
        src_v,
        0,
        src_cb,
        0,
        0,
        // dp4 o0.y, v0, cb0[1]
        inst_dp4,
        dst_o(0x2),
        0,
        src_v,
        0,
        src_cb,
        0,
        1,
        // dp4 o0.z, v0, cb0[2]
        inst_dp4,
        dst_o(0x4),
        0,
        src_v,
        0,
        src_cb,
        0,
        2,
        // dp4 o0.w, v0, cb0[3]
        inst_dp4,
        dst_o(0x8),
        0,
        src_v,
        0,
        src_cb,
        0,
        3,
        // mov o1.xyzw, v1.xyzw (TEXCOORD0)
        inst_mov,
        dst_o(0xF),
        1,
        src_v,
        1,
        // mov o2.xyzw, v2.xyzw (COLOR0)
        inst_mov,
        dst_o(0xF),
        2,
        src_v,
        2,
        inst_ret,
    ];
    build_dxbc_container(
        ShaderStage::Vertex,
        &[
            ("POSITION", 0, 0, 0),
            ("TEXCOORD", 0, 0, 1),
            ("COLOR", 0, 0, 2),
        ],
        &[
            ("SV_POSITION", 0, 1, 0),
            ("TEXCOORD", 0, 0, 1),
            ("COLOR", 0, 0, 2),
        ],
        &code,
    )
}

/// Construit un Pixel Shader DXBC SM5.0 3D complet :
/// échantillonne la texture 2D `t0` avec le sampler `s0` à `v1.xy`, puis multiplie par la teinte `v2`.
pub fn sample_textured_cube_ps_dxbc() -> Vec<u8> {
    let dst_r = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_TEMP << 12) | (1 << 20) };
    let dst_o = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_OUTPUT << 12) | (1 << 20) };
    let src_v = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_INPUT << 12) | (1 << 20);
    let src_r = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_TEMP << 12) | (1 << 20);
    let src_t = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_RESOURCE << 12) | (1 << 20);
    let src_s = (OPERAND_SAMPLER << 12) | (1 << 20);

    let code = [
        (2u32 << 24) | OP_DCL_TEMPS,
        1,
        // sample r0.xyzw, v1.xyzw, t0.xyzw, s0
        (9u32 << 24) | OP_SAMPLE,
        dst_r(0xF),
        0,
        src_v,
        1,
        src_t,
        0,
        src_s,
        0,
        // mul o0.xyzw, r0.xyzw, v2.xyzw
        (7u32 << 24) | OP_MUL,
        dst_o(0xF),
        0,
        src_r,
        0,
        src_v,
        2,
        (1u32 << 24) | OP_RET,
    ];
    build_dxbc_container(
        ShaderStage::Pixel,
        &[
            ("SV_POSITION", 0, 1, 0),
            ("TEXCOORD", 0, 0, 1),
            ("COLOR", 0, 0, 2),
        ],
        &[("SV_Target", 0, 0, 0)],
        &code,
    )
}

/// Construit un Compute Shader DXBC SM5.0 (`numthreads(64, 1, 1)`) :
/// calcule `u1[gid.x] = u0[gid.x] * cb0[0] + cb0[1]` sur le GPU Vulkan.
pub fn sample_compute_transform_cs_dxbc() -> Vec<u8> {
    let dst_r = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_TEMP << 12) | (1 << 20) };
    let src_r = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_TEMP << 12) | (1 << 20);
    let src_cb = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_CONSTANT_BUFFER << 12) | (2 << 20);
    let src_u = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_UNORDERED_ACCESS_VIEW << 12) | (1 << 20);
    let dst_u =
        |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_UNORDERED_ACCESS_VIEW << 12) | (1 << 20) };
    let src_tid_x = 2 | (2 << 2) | (OPERAND_INPUT_THREAD_ID << 12);
    let imm1 = 1 | (OPERAND_IMMEDIATE32 << 12);

    let code = [
        (2u32 << 24) | OP_DCL_TEMPS,
        2,
        (4u32 << 24) | OP_DCL_THREAD_GROUP,
        64,
        1,
        1,
        // ld_structured r0.xyzw, vThreadID.x, l(0), u0.xyzw
        (8u32 << 24) | OP_LD_STRUCTURED,
        dst_r(0xF),
        0,
        src_tid_x,
        imm1,
        0,
        src_u,
        0,
        // mad r1.xyzw, r0.xyzw, cb0[0].xyzw, cb0[1].xyzw
        (11u32 << 24) | OP_MAD,
        dst_r(0xF),
        1,
        src_r,
        0,
        src_cb,
        0,
        0,
        src_cb,
        0,
        1,
        // store_structured u1.xyzw, vThreadID.x, l(0), r1.xyzw
        (8u32 << 24) | OP_STORE_STRUCTURED,
        dst_u(0xF),
        1,
        src_tid_x,
        imm1,
        0,
        src_r,
        1,
        (1u32 << 24) | OP_RET,
    ];
    build_dxbc_container(ShaderStage::Compute, &[], &[], &code)
}

/// Construit un Vertex Shader DXBC SM5.0 3D **instancié** (`SV_InstanceID`) :
/// additionne l'offset par instance `cb0[v3.x + 4]` à `v0` (`POSITION`), projette par `cb0[0..4]`
/// et propage `TEXCOORD0` (`v1`) et `COLOR0` (`v2`).
pub fn sample_instanced_vs_dxbc() -> Vec<u8> {
    let dst_r = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_TEMP << 12) | (1 << 20) };
    let dst_o = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_OUTPUT << 12) | (1 << 20) };
    let src_v = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_INPUT << 12) | (1 << 20);
    let src_r = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_TEMP << 12) | (1 << 20);
    let src_r_x = 2 | (2 << 2) | (OPERAND_TEMP << 12) | (1 << 20);
    let src_cb = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_CONSTANT_BUFFER << 12) | (2 << 20);
    // cb0[r0.x + 4] : dim0=imm(0), dim1=imm_plus_rel(3 << 25)
    let src_cb_rel = src_cb | (3 << 25);

    let code = [
        (2u32 << 24) | OP_DCL_TEMPS,
        2,
        // mov r0, v3 (SV_InstanceID)
        (5u32 << 24) | OP_MOV,
        dst_r(0xF),
        0,
        src_v,
        3,
        // add r1, v0, cb0[r0.x + 4]
        (10u32 << 24) | OP_ADD,
        dst_r(0xF),
        1,
        src_v,
        0,
        src_cb_rel,
        0,
        4,
        src_r_x,
        0,
        // dp4 o0.{x,y,z,w}, r1, cb0[0..3]
        (8u32 << 24) | OP_DP4,
        dst_o(0x1),
        0,
        src_r,
        1,
        src_cb,
        0,
        0,
        (8u32 << 24) | OP_DP4,
        dst_o(0x2),
        0,
        src_r,
        1,
        src_cb,
        0,
        1,
        (8u32 << 24) | OP_DP4,
        dst_o(0x4),
        0,
        src_r,
        1,
        src_cb,
        0,
        2,
        (8u32 << 24) | OP_DP4,
        dst_o(0x8),
        0,
        src_r,
        1,
        src_cb,
        0,
        3,
        // mov o1, v1 (TEXCOORD0) & mov o2, v2 (COLOR0)
        (5u32 << 24) | OP_MOV,
        dst_o(0xF),
        1,
        src_v,
        1,
        (5u32 << 24) | OP_MOV,
        dst_o(0xF),
        2,
        src_v,
        2,
        (1u32 << 24) | OP_RET,
    ];
    build_dxbc_container(
        ShaderStage::Vertex,
        &[
            ("POSITION", 0, 0, 0),
            ("TEXCOORD", 0, 0, 1),
            ("COLOR", 0, 0, 2),
            ("SV_INSTANCEID", 0, 8, 3),
        ],
        &[
            ("SV_POSITION", 0, 1, 0),
            ("TEXCOORD", 0, 0, 1),
            ("COLOR", 0, 0, 2),
        ],
        &code,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn validate_with_spirv_val(spv_bytes: &[u8]) {
        if let Ok(mut child) = Command::new("spirv-val")
            .args(["--target-env", "vulkan1.3", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(spv_bytes).expect("write spv stdin");
            }
            let out = child.wait_with_output().expect("wait spirv-val");
            assert!(
                out.status.success(),
                "spirv-val a rejeté le SPIR-V généré :\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn translates_dxbc_vs_and_ps_to_valid_vulkan13_spirv() {
        let vs_dxbc = sample_triangle_vs_dxbc();
        let vs = translate_dxbc_to_spirv(&vs_dxbc).expect("traduction VS DXBC -> SPIR-V");
        assert_eq!(vs.stage, ShaderStage::Vertex);
        assert_eq!(vs.spirv[0], 0x0723_0203);
        validate_with_spirv_val(&vs.as_bytes());

        let ps_dxbc = sample_triangle_ps_dxbc();
        let ps = translate_dxbc_to_spirv(&ps_dxbc).expect("traduction PS DXBC -> SPIR-V");
        assert_eq!(ps.stage, ShaderStage::Pixel);
        assert_eq!(ps.spirv[0], 0x0723_0203);
        validate_with_spirv_val(&ps.as_bytes());
    }

    #[test]
    fn translates_textured_3d_cube_shaders_with_cbuffer_and_sample() {
        let vs_dxbc = sample_textured_cube_vs_dxbc();
        let vs = translate_dxbc_to_spirv(&vs_dxbc).expect("traduction VS cube MVP");
        assert_eq!(vs.used_cbuffers, vec![0]);
        validate_with_spirv_val(&vs.as_bytes());

        let ps_dxbc = sample_textured_cube_ps_dxbc();
        let ps = translate_dxbc_to_spirv(&ps_dxbc).expect("traduction PS cube texture");
        assert_eq!(ps.used_textures, vec![0]);
        assert_eq!(ps.used_samplers, vec![0]);
        validate_with_spirv_val(&ps.as_bytes());
    }

    #[test]
    fn translates_compute_shader_and_instanced_vs_to_valid_vulkan13_spirv() {
        let cs_dxbc = sample_compute_transform_cs_dxbc();
        let cs = translate_dxbc_to_spirv(&cs_dxbc).expect("traduction CS DXBC -> SPIR-V");
        assert_eq!(cs.stage, ShaderStage::Compute);
        assert_eq!(cs.thread_group_size, [64, 1, 1]);
        assert_eq!(cs.used_cbuffers, vec![0]);
        assert_eq!(cs.used_storage_buffers, vec![0, 1]);
        validate_with_spirv_val(&cs.as_bytes());

        let vs_dxbc = sample_instanced_vs_dxbc();
        let vs = translate_dxbc_to_spirv(&vs_dxbc).expect("traduction VS instancié -> SPIR-V");
        assert_eq!(vs.stage, ShaderStage::Vertex);
        assert_eq!(vs.used_cbuffers, vec![0]);
        validate_with_spirv_val(&vs.as_bytes());
    }

    #[test]
    fn translates_alu_mad_and_dp4_to_valid_spirv() {
        let dst_r = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_TEMP << 12) | (1 << 20) };
        let dst_o = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_OUTPUT << 12) | (1 << 20) };
        let src_v = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_INPUT << 12) | (1 << 20);
        let src_r = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_TEMP << 12) | (1 << 20);
        let imm4 = 2 | (OPERAND_IMMEDIATE32 << 12);

        let code = [
            (2u32 << 24) | OP_DCL_TEMPS,
            1,
            // mad r0, v0, l(0.5, 0.5, 0.5, 1.0), l(0.1, 0.2, 0.0, 0.0)
            (15u32 << 24) | OP_MAD,
            dst_r(0xF),
            0,
            src_v,
            0,
            imm4,
            0.5f32.to_bits(),
            0.5f32.to_bits(),
            0.5f32.to_bits(),
            1.0f32.to_bits(),
            imm4,
            0.1f32.to_bits(),
            0.2f32.to_bits(),
            0.0f32.to_bits(),
            0.0f32.to_bits(),
            // mov o0, r0
            (5u32 << 24) | OP_MOV,
            dst_o(0xF),
            0,
            src_r,
            0,
            (1u32 << 24) | OP_RET,
        ];
        let dxbc = build_dxbc_container(
            ShaderStage::Vertex,
            &[("POSITION", 0, 0, 0)],
            &[("SV_POSITION", 0, 1, 0)],
            &code,
        );
        let res = translate_dxbc_to_spirv(&dxbc).expect("traduction MAD");
        validate_with_spirv_val(&res.as_bytes());
    }

    #[test]
    fn translates_real_xbox_openlara_fxc_shaders_to_valid_spirv() {
        let exe_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tmp/openlara-xb1/app/OpenLara.exe");
        let Ok(data) = std::fs::read(&exe_path) else {
            return;
        };

        let mut validated = 0usize;
        let mut with_cb = 0usize;
        let mut with_tex = 0usize;
        let mut cursor = 0usize;
        while let Some(rel) = data[cursor..].windows(4).position(|w| w == b"DXBC") {
            let off = cursor + rel;
            cursor = off + 4;
            if off + 32 > data.len() {
                break;
            }
            let total_size = u32::from_le_bytes([
                data[off + 24],
                data[off + 25],
                data[off + 26],
                data[off + 27],
            ]) as usize;
            let nchunks = u32::from_le_bytes([
                data[off + 28],
                data[off + 29],
                data[off + 30],
                data[off + 31],
            ]);
            if nchunks == 0 || nchunks > 32 || total_size < 32 || off + total_size > data.len() {
                continue;
            }
            let dxbc = &data[off..off + total_size];
            let translated =
                translate_dxbc_to_spirv(dxbc).expect("traduction d'un vrai shader fxc OpenLara");
            validate_with_spirv_val(&translated.as_bytes());
            if !translated.used_cbuffers.is_empty() {
                with_cb += 1;
            }
            if !translated.used_textures.is_empty() {
                with_tex += 1;
            }
            validated += 1;
        }
        assert!(
            validated >= 50,
            "au moins 50 vrais shaders DXBC attendus dans OpenLara.exe (trouvé {validated})"
        );
        assert!(with_cb > 0 && with_tex > 0, "cb={with_cb}, tex={with_tex}");
    }
}
