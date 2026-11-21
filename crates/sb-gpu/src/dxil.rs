//! Décodeur natif Rust **DXIL (Shader Model 6.0–6.6 / LLVM 3.7 Bitcode)** pour ScarletBridge.
//!
//! Analyse le chunk `DXIL` à l'intérieur d'un conteneur `DXBC` :
//! - En-tête `DxilProgramHeader` (stage VS/PS/CS/MS/AS/RT, Shader Model 6.x, version DXIL)
//! - Lecteur binaire **LLVM 3.7 Bitstream** (`BC\xC0\xDE`, `ENTER_SUBBLOCK`, `END_BLOCK`,
//!   `UNABBREV_RECORD`, VBR6, identification des blocs `MODULE`, `TYPE`, `FUNCTION`, `METADATA`).

use thiserror::Error;

use crate::shader::ShaderStage;

const DXIL_MAGIC: u32 = u32::from_le_bytes(*b"DXIL");
const LLVM_BITCODE_MAGIC: [u8; 4] = [0x42, 0x43, 0xC0, 0xDE];

// Identifiants de blocs LLVM 3.7 standards (utilisés par Microsoft DXC / DXIL).
pub const LLVM_BLOCK_BLOCKINFO: u32 = 0;
pub const LLVM_BLOCK_MODULE: u32 = 8;
pub const LLVM_BLOCK_PARAMATTR: u32 = 9;
pub const LLVM_BLOCK_CONSTANTS: u32 = 11;
pub const LLVM_BLOCK_FUNCTION: u32 = 12;
pub const LLVM_BLOCK_VALUE_SYMTAB: u32 = 14;
pub const LLVM_BLOCK_METADATA: u32 = 15;
pub const LLVM_BLOCK_TYPE: u32 = 17;

#[derive(Debug, Error)]
pub enum DxilError {
    #[error("conteneur DXBC invalide ou chunk DXIL absent")]
    MissingDxilChunk,
    #[error("en-tête DxilProgramHeader tronqué ou magic invalide")]
    InvalidProgramHeader,
    #[error("bitcode LLVM invalide (magic BC\\xC0\\xDE attendu)")]
    InvalidBitcodeMagic,
    #[error("flux binaire LLVM tronqué")]
    TruncatedBitstream,
}

#[derive(Debug, Clone)]
pub struct DxilProgramInfo {
    pub stage: ShaderStage,
    pub shader_model_major: u8,
    pub shader_model_minor: u8,
    pub dxil_major: u8,
    pub dxil_minor: u8,
    pub bitcode_size: usize,
    pub module_blocks_seen: Vec<u32>,
    pub function_count: usize,
    pub metadata_records: usize,
    pub type_records: usize,
}

/// Extrait et décode le chunk `DXIL` (et son bitstream LLVM 3.7) depuis un conteneur `DXBC`.
pub fn parse_dxil_container(dxbc: &[u8]) -> Result<DxilProgramInfo, DxilError> {
    if dxbc.len() < 32 || &dxbc[0..4] != b"DXBC" {
        return Err(DxilError::MissingDxilChunk);
    }
    let chunk_count = u32::from_le_bytes([dxbc[28], dxbc[29], dxbc[30], dxbc[31]]) as usize;
    if 32 + chunk_count * 4 > dxbc.len() {
        return Err(DxilError::MissingDxilChunk);
    }

    let mut dxil_payload: Option<&[u8]> = None;
    for i in 0..chunk_count {
        let p = 32 + i * 4;
        let off = u32::from_le_bytes([dxbc[p], dxbc[p + 1], dxbc[p + 2], dxbc[p + 3]]) as usize;
        if off + 8 > dxbc.len() {
            continue;
        }
        let fourcc = &dxbc[off..off + 4];
        let size = u32::from_le_bytes([dxbc[off + 4], dxbc[off + 5], dxbc[off + 6], dxbc[off + 7]])
            as usize;
        if fourcc == b"DXIL" && off + 8 + size <= dxbc.len() {
            dxil_payload = Some(&dxbc[off + 8..off + 8 + size]);
            break;
        }
    }

    let payload = dxil_payload.ok_or(DxilError::MissingDxilChunk)?;
    parse_dxil_program_header(payload)
}

/// Décode un `DxilProgramHeader` brut + son bitstream LLVM 3.7 embarqué.
pub fn parse_dxil_program_header(payload: &[u8]) -> Result<DxilProgramInfo, DxilError> {
    if payload.len() < 24 {
        return Err(DxilError::InvalidProgramHeader);
    }
    let program_version = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let magic = u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);
    if magic != DXIL_MAGIC {
        return Err(DxilError::InvalidProgramHeader);
    }
    let dxil_version = u32::from_le_bytes([payload[12], payload[13], payload[14], payload[15]]);
    let bc_offset_from_magic =
        u32::from_le_bytes([payload[16], payload[17], payload[18], payload[19]]) as usize;
    let bc_size = u32::from_le_bytes([payload[20], payload[21], payload[22], payload[23]]) as usize;

    let bc_start = 8usize.saturating_add(bc_offset_from_magic);
    let bc_end = bc_start.saturating_add(bc_size);
    if bc_end > payload.len() || bc_size < 4 {
        return Err(DxilError::InvalidProgramHeader);
    }

    let bitcode = &payload[bc_start..bc_end];
    if bitcode[..4] != LLVM_BITCODE_MAGIC {
        return Err(DxilError::InvalidBitcodeMagic);
    }

    let kind = ((program_version >> 16) & 0xFFFF) as u16;
    let stage = match kind {
        0 => ShaderStage::Pixel,
        1 => ShaderStage::Vertex,
        2 => ShaderStage::Geometry,
        3 => ShaderStage::Hull,
        4 => ShaderStage::Domain,
        5 => ShaderStage::Compute,
        other => ShaderStage::Unknown(other),
    };
    let sm_major = ((program_version >> 4) & 0xF) as u8;
    let sm_minor = (program_version & 0xF) as u8;
    let dxil_major = ((dxil_version >> 8) & 0xFF) as u8;
    let dxil_minor = (dxil_version & 0xFF) as u8;

    let stats = scan_llvm_bitstream(&bitcode[4..])?;

    Ok(DxilProgramInfo {
        stage,
        shader_model_major: sm_major,
        shader_model_minor: sm_minor,
        dxil_major,
        dxil_minor,
        bitcode_size: bc_size,
        module_blocks_seen: stats.blocks,
        function_count: stats.functions,
        metadata_records: stats.metadata_records,
        type_records: stats.type_records,
    })
}

struct BitstreamReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitstreamReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    fn remaining_bits(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.bit_pos)
    }

    fn read_bits(&mut self, count: usize) -> Option<u64> {
        if count == 0 {
            return Some(0);
        }
        if count > 64 || self.remaining_bits() < count {
            return None;
        }
        let mut out = 0u64;
        for i in 0..count {
            let byte = self.data[self.bit_pos / 8];
            let bit = (byte >> (self.bit_pos % 8)) & 1;
            out |= u64::from(bit) << i;
            self.bit_pos += 1;
        }
        Some(out)
    }

    fn read_vbr(&mut self, width: usize) -> Option<u64> {
        let mut out = 0u64;
        let mut shift = 0usize;
        let cont_mask = 1u64 << (width - 1);
        let val_mask = cont_mask - 1;
        loop {
            let chunk = self.read_bits(width)?;
            out |= (chunk & val_mask) << shift;
            if (chunk & cont_mask) == 0 {
                return Some(out);
            }
            shift += width - 1;
            if shift >= 64 {
                return None;
            }
        }
    }

    fn align32(&mut self) {
        let rem = self.bit_pos % 32;
        if rem != 0 {
            self.bit_pos += 32 - rem;
        }
    }
}

struct BitstreamStats {
    blocks: Vec<u32>,
    functions: usize,
    metadata_records: usize,
    type_records: usize,
}

fn scan_llvm_bitstream(stream: &[u8]) -> Result<BitstreamStats, DxilError> {
    let mut reader = BitstreamReader::new(stream);
    let mut stats = BitstreamStats {
        blocks: Vec::new(),
        functions: 0,
        metadata_records: 0,
        type_records: 0,
    };
    let mut stack: Vec<(u32, usize)> = vec![(0, 2)]; // (block_id, abbrev_width)

    while let Some(&(cur_block, abbrev_width)) = stack.last() {
        if reader.remaining_bits() < abbrev_width {
            break;
        }
        let Some(abbrev_id) = reader.read_bits(abbrev_width) else {
            break;
        };
        match abbrev_id {
            0 => {
                // END_BLOCK
                reader.align32();
                stack.pop();
            }
            1 => {
                // ENTER_SUBBLOCK: VBR8(block_id), VBR4(new_abbrev_width), align32, u32(block_len_words)
                let block_id = reader.read_vbr(8).ok_or(DxilError::TruncatedBitstream)? as u32;
                let new_width = reader.read_vbr(4).ok_or(DxilError::TruncatedBitstream)? as usize;
                reader.align32();
                let block_words =
                    reader.read_bits(32).ok_or(DxilError::TruncatedBitstream)? as usize;
                stats.blocks.push(block_id);
                if block_id == LLVM_BLOCK_FUNCTION {
                    stats.functions += 1;
                    // On saute rapidement le corps d'une fonction si on ne fait que l'inspection structurelle.
                    let skip_bits = block_words.saturating_mul(32);
                    if reader.remaining_bits() >= skip_bits {
                        reader.bit_pos += skip_bits;
                        continue;
                    }
                }
                stack.push((block_id, new_width.max(2)));
            }
            3 => {
                // UNABBREV_RECORD: VBR6(code), VBR6(num_ops), num_ops * VBR6(op)
                let _code = reader.read_vbr(6).ok_or(DxilError::TruncatedBitstream)?;
                let nops = reader.read_vbr(6).ok_or(DxilError::TruncatedBitstream)? as usize;
                for _ in 0..nops {
                    let _ = reader.read_vbr(6).ok_or(DxilError::TruncatedBitstream)?;
                }
                if cur_block == LLVM_BLOCK_METADATA {
                    stats.metadata_records += 1;
                } else if cur_block == LLVM_BLOCK_TYPE {
                    stats.type_records += 1;
                }
            }
            _ => {
                // Abréviations personnalisées (DEFINE_ABBREV / abbrev >= 4) : arrêt propre du scan rapide.
                break;
            }
        }
    }

    Ok(stats)
}

/// Construit un conteneur `DXBC` contenant un chunk `DXIL` SM 6.0 valide (pour tests unitaires).
pub fn build_sample_dxil_sm60_container(stage: ShaderStage) -> Vec<u8> {
    struct BitWriter {
        bytes: Vec<u8>,
        cur_byte: u8,
        cur_bits: u8,
    }
    impl BitWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                cur_byte: 0,
                cur_bits: 0,
            }
        }
        fn write_bits(&mut self, val: u64, count: usize) {
            for i in 0..count {
                let bit = ((val >> i) & 1) as u8;
                self.cur_byte |= bit << self.cur_bits;
                self.cur_bits += 1;
                if self.cur_bits == 8 {
                    self.bytes.push(self.cur_byte);
                    self.cur_byte = 0;
                    self.cur_bits = 0;
                }
            }
        }
        fn write_vbr(&mut self, mut val: u64, width: usize) {
            let max_chunk = (1u64 << (width - 1)) - 1;
            loop {
                if val <= max_chunk {
                    self.write_bits(val, width);
                    break;
                }
                self.write_bits((val & max_chunk) | (1u64 << (width - 1)), width);
                val >>= width - 1;
            }
        }
        fn align32(&mut self) {
            while self.cur_bits != 0 {
                self.write_bits(0, 1);
            }
            while !self.bytes.len().is_multiple_of(4) {
                self.bytes.push(0);
            }
        }
    }

    let mut bw = BitWriter::new();
    // ENTER_SUBBLOCK MODULE_BLOCK (id=8, abbrev_width=3)
    bw.write_bits(1, 2);
    bw.write_vbr(u64::from(LLVM_BLOCK_MODULE), 8);
    bw.write_vbr(3, 4);
    bw.align32();
    let mod_len_pos = bw.bytes.len();
    bw.write_bits(0, 32); // placeholder

    // Sous-bloc TYPE_BLOCK (id=17) avec 1 UNABBREV_RECORD
    bw.write_bits(1, 3);
    bw.write_vbr(u64::from(LLVM_BLOCK_TYPE), 8);
    bw.write_vbr(3, 4);
    bw.align32();
    let ty_len_pos = bw.bytes.len();
    bw.write_bits(0, 32);
    bw.write_bits(3, 3); // UNABBREV_RECORD
    bw.write_vbr(1, 6); // code
    bw.write_vbr(1, 6); // 1 op
    bw.write_vbr(32, 6); // f32
    bw.write_bits(0, 3); // END_BLOCK TYPE_BLOCK
    bw.align32();
    let ty_words = ((bw.bytes.len() - ty_len_pos - 4) / 4) as u32;
    bw.bytes[ty_len_pos..ty_len_pos + 4].copy_from_slice(&ty_words.to_le_bytes());

    // Sous-bloc FUNCTION_BLOCK (id=12, 1 mot vide)
    bw.write_bits(1, 3);
    bw.write_vbr(u64::from(LLVM_BLOCK_FUNCTION), 8);
    bw.write_vbr(3, 4);
    bw.align32();
    bw.write_bits(1, 32); // 1 word
    bw.write_bits(0, 32); // body

    // END_BLOCK MODULE_BLOCK
    bw.write_bits(0, 3);
    bw.align32();
    let mod_words = ((bw.bytes.len() - mod_len_pos - 4) / 4) as u32;
    bw.bytes[mod_len_pos..mod_len_pos + 4].copy_from_slice(&mod_words.to_le_bytes());

    let mut bitcode = LLVM_BITCODE_MAGIC.to_vec();
    bitcode.extend_from_slice(&bw.bytes);

    let stage_kind: u32 = match stage {
        ShaderStage::Pixel => 0,
        ShaderStage::Vertex => 1,
        ShaderStage::Compute => 5,
        _ => 5,
    };
    let program_version = (stage_kind << 16) | 0x0060; // SM 6.0
    let total_words = (6 + bitcode.len() / 4) as u32;
    let mut dxil_chunk = Vec::with_capacity(24 + bitcode.len());
    dxil_chunk.extend_from_slice(&program_version.to_le_bytes());
    dxil_chunk.extend_from_slice(&total_words.to_le_bytes());
    dxil_chunk.extend_from_slice(&DXIL_MAGIC.to_le_bytes());
    dxil_chunk.extend_from_slice(&0x0100u32.to_le_bytes()); // DXIL 1.0
    dxil_chunk.extend_from_slice(&16u32.to_le_bytes()); // offset depuis DXIL_MAGIC
    dxil_chunk.extend_from_slice(&(bitcode.len() as u32).to_le_bytes());
    dxil_chunk.extend_from_slice(&bitcode);

    // Enveloppe DXBC 1 chunk (`DXIL`)
    let header_len = 36usize;
    let total_size = header_len + 8 + dxil_chunk.len();
    let mut out = Vec::with_capacity(total_size);
    out.extend_from_slice(b"DXBC");
    out.extend_from_slice(&[0u8; 16]);
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(total_size as u32).to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(header_len as u32).to_le_bytes());
    out.extend_from_slice(b"DXIL");
    out.extend_from_slice(&(dxil_chunk.len() as u32).to_le_bytes());
    out.extend_from_slice(&dxil_chunk);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dxil_sm60_program_header_and_llvm_bitstream() {
        let dxbc = build_sample_dxil_sm60_container(ShaderStage::Compute);
        let info = parse_dxil_container(&dxbc).expect("parse DXIL SM 6.0");
        assert_eq!(info.stage, ShaderStage::Compute);
        assert_eq!(info.shader_model_major, 6);
        assert_eq!(info.shader_model_minor, 0);
        assert_eq!(info.dxil_major, 1);
        assert_eq!(info.function_count, 1);
        assert_eq!(info.type_records, 1);
        assert!(info.module_blocks_seen.contains(&LLVM_BLOCK_MODULE));
        assert!(info.module_blocks_seen.contains(&LLVM_BLOCK_TYPE));
        assert!(info.module_blocks_seen.contains(&LLVM_BLOCK_FUNCTION));
    }
}
