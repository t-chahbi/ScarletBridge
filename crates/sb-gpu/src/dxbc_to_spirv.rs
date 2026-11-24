//! Phase 5 — Compilateur / traducteur natif Rust **DXBC (SM4/SM5) → SPIR-V 1.3 (Vulkan)**.
//!
//! Décode les conteneurs Microsoft `DXBC` (`ISGN`, `OSGN`, `SHDR`/`SHEX`) et traduit vers un
//! module binaire SPIR-V 1.3 conforme `spirv-val --target-env vulkan1.3` (VS, PS, CS).
//!
//! Architecture : `decode` (jetons -> instructions), `codegen` (interface, opérandes, flux de
//! contrôle structuré), `alu` (flottants/entiers), `tex` (textures, buffers, UAV, atomiques),
//! `spv` (constructeur de module). Registres DXBC = variables `Private` `uvec4` (bits bruts).
//! Bindings (set 0) : `cb#` -> `#`, `t#` -> `8 + #`, `s#` -> `16 + #`, `u#` -> `24 + #`,
//! buffers SRV bruts/structurés -> `32 + #` (slots `0..8`). GS/HS/DS, doubles, `msad`,
//! `samplepos`, `swapc` et SM5.1 : erreur typée (`ShaderTranslateError`).

use thiserror::Error;

use crate::shader::ShaderStage;

/// Nombre maximal de slots de Constant Buffers liés en DescriptorSet 0 (bindings `0..8`).
pub const MAX_CBUFFER_SLOTS: u32 = 8;
/// Nombre maximal de vecteurs `vec4` par Constant Buffer (256 × 16o = 4 KiB).
pub const CBUFFER_VEC4_COUNT: u32 = 256;
/// Binding de base des textures échantillonnées (`t0..t7` → bindings `8..15`).
pub const TEXTURE_BINDING_BASE: u32 = 8;
/// Nombre maximal de textures liées simultanément.
pub const MAX_TEXTURE_SLOTS: u32 = 8;
/// Binding de base des samplers (`s0..s7` → bindings `16..23`).
pub const SAMPLER_BINDING_BASE: u32 = 16;
/// Nombre maximal de samplers liés simultanément.
pub const MAX_SAMPLER_SLOTS: u32 = 8;
/// Binding de base des Storage Buffers / UAV (`u0..u7` → bindings `24..31`).
pub const STORAGE_BINDING_BASE: u32 = 24;
/// Nombre maximal de Storage Buffers (SSBO / UAV) liés simultanément.
pub const MAX_STORAGE_SLOTS: u32 = 8;
/// Binding de base des buffers SRV bruts/structurés (`t0..t7` → bindings `32..39`, lecture seule).
pub const SRV_BUFFER_BINDING_BASE: u32 = 32;
/// Nombre de slots (`cb#`, `t#`, `s#`, `u#`) exprimables par classe de ressource : les plages de
/// bindings (`0..8`, `8..16`, `16..24`, `24..32`, `32..40`) ne se chevauchent pas.
pub const MAX_BINDING_SLOTS: u32 = 8;

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
    #[error("opcode SM4/SM5 non supporté : {name} (0x{opcode:X})")]
    UnsupportedOpcode { opcode: u32, name: &'static str },
    #[error("opérande invalide ou non supporté : {0}")]
    InvalidOperand(String),
    #[error("fonctionnalité shader non supportée : {0}")]
    Unsupported(String),
}

mod alu;
mod codegen;
mod decode;
mod opcodes;
mod spv;
#[cfg(test)]
mod tests_ext;
mod tex;
#[allow(unused_imports)]
use opcodes::*;

#[derive(Debug, Clone)]
pub struct SignatureElement {
    pub semantic_name: String,
    pub semantic_index: u32,
    pub system_value_type: u32,
    pub register: u32,
    pub mask: u8,
    /// Type de composante : 1 = uint, 2 = sint, 3 = float (`D3D_REGISTER_COMPONENT_TYPE`).
    pub component_type: u32,
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
    /// Buffers SRV bruts/structurés (`t#`), bindings `SRV_BUFFER_BINDING_BASE + slot`.
    pub used_srv_buffers: Vec<u32>,
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
    if chunk_count > dxbc.len() / 4 || 32 + chunk_count * 4 > dxbc.len() {
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
        if off.saturating_add(8) > dxbc.len() {
            continue;
        }
        let fourcc = &dxbc[off..off + 4];
        let size = u32::from_le_bytes([dxbc[off + 4], dxbc[off + 5], dxbc[off + 6], dxbc[off + 7]])
            as usize;
        let payload_end = off.saturating_add(8).saturating_add(size).min(dxbc.len());
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
        2 => ShaderStage::Geometry,
        3 => ShaderStage::Hull,
        4 => ShaderStage::Domain,
        5 => ShaderStage::Compute,
        other => {
            return Err(ShaderTranslateError::UnsupportedStage(
                ShaderStage::Unknown(other),
            ));
        }
    };

    let emitted = codegen::emit_spirv(stage, &inputs, &outputs, &words[2..])?;
    Ok(TranslatedShader {
        stage,
        spirv: emitted.spirv,
        inputs,
        outputs,
        used_cbuffers: emitted.used_cbuffers,
        used_textures: emitted.used_textures,
        used_samplers: emitted.used_samplers,
        used_storage_buffers: emitted.used_storage_buffers,
        used_srv_buffers: emitted.used_srv_buffers,
        thread_group_size: emitted.thread_group_size,
    })
}

fn parse_signature_chunk(payload: &[u8]) -> Vec<SignatureElement> {
    if payload.len() < 8 {
        return Vec::new();
    }
    let count = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
    let mut elems = Vec::new();
    for i in 0..count {
        let Some(base) = i.checked_mul(24).and_then(|b| b.checked_add(8)) else {
            break;
        };
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
        let component_type = u32::from_le_bytes([
            payload[base + 12],
            payload[base + 13],
            payload[base + 14],
            payload[base + 15],
        ]);

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
            component_type,
        });
    }
    elems
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
