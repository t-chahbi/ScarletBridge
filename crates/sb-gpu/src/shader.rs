//! Phase 5 — Parseur de conteneurs shaders DirectX (`DXBC` / `DXIL` / ISA console).
//!
//! Extrait les chunks (`SHDR`, `SHEX`, `DXIL`, `ISGN`, `OSGN`, `RDEF`), le type de shader
//! (Vertex, Pixel, Compute, Geometry, Hull, Domain, Mesh/Amplification) et le Shader Model.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShaderFormat {
    Dxbc,
    Dxil,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShaderStage {
    Pixel,
    Vertex,
    Geometry,
    Hull,
    Domain,
    Compute,
    LibraryOrMesh,
    Unknown(u16),
}

#[derive(Debug, Clone)]
pub struct ShaderContainer {
    pub format: ShaderFormat,
    pub stage: ShaderStage,
    pub major_version: u8,
    pub minor_version: u8,
    pub total_size: usize,
    pub chunks: Vec<[u8; 4]>,
    pub bytecode_len: usize,
}

impl ShaderContainer {
    /// Parse l'en-tête d'un conteneur DXBC/DXIL (`b"DXBC"`).
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 32 || &bytes[0..4] != b"DXBC" {
            return None;
        }
        let total_size = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]) as usize;
        let chunk_count = u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]) as usize;

        if 32 + chunk_count * 4 > bytes.len() {
            return None;
        }

        let mut chunks = Vec::with_capacity(chunk_count);
        let mut format = ShaderFormat::Unknown;
        let mut stage = ShaderStage::Unknown(0);
        let mut major_version = 0u8;
        let mut minor_version = 0u8;
        let mut bytecode_len = 0usize;

        for i in 0..chunk_count {
            let off_pos = 32 + i * 4;
            let chunk_off = u32::from_le_bytes([
                bytes[off_pos],
                bytes[off_pos + 1],
                bytes[off_pos + 2],
                bytes[off_pos + 3],
            ]) as usize;

            if chunk_off + 8 > bytes.len() {
                continue;
            }
            let fourcc = [
                bytes[chunk_off],
                bytes[chunk_off + 1],
                bytes[chunk_off + 2],
                bytes[chunk_off + 3],
            ];
            let chunk_size = u32::from_le_bytes([
                bytes[chunk_off + 4],
                bytes[chunk_off + 5],
                bytes[chunk_off + 6],
                bytes[chunk_off + 7],
            ]) as usize;
            chunks.push(fourcc);

            let payload_start = chunk_off + 8;
            if (&fourcc == b"SHDR" || &fourcc == b"SHEX") && payload_start + 4 <= bytes.len() {
                format = ShaderFormat::Dxbc;
                bytecode_len = chunk_size;
                let ver_token = u32::from_le_bytes([
                    bytes[payload_start],
                    bytes[payload_start + 1],
                    bytes[payload_start + 2],
                    bytes[payload_start + 3],
                ]);
                let kind = ((ver_token >> 16) & 0xFFFF) as u16;
                major_version = ((ver_token >> 4) & 0xF) as u8;
                minor_version = (ver_token & 0xF) as u8;
                stage = match kind {
                    0 => ShaderStage::Pixel,
                    1 => ShaderStage::Vertex,
                    2 => ShaderStage::Geometry,
                    3 => ShaderStage::Hull,
                    4 => ShaderStage::Domain,
                    5 => ShaderStage::Compute,
                    other => ShaderStage::Unknown(other),
                };
            } else if &fourcc == b"DXIL" && payload_start + 4 <= bytes.len() {
                format = ShaderFormat::Dxil;
                bytecode_len = chunk_size;
                stage = ShaderStage::LibraryOrMesh;
                major_version = 6;
            }
        }

        Some(Self {
            format,
            stage,
            major_version,
            minor_version,
            total_size,
            chunks,
            bytecode_len,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_synthetic_dxbc_vertex_shader_sm50() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"DXBC");
        blob.extend_from_slice(&[0u8; 16]); // hash
        blob.extend_from_slice(&1u32.to_le_bytes()); // version
        blob.extend_from_slice(&48u32.to_le_bytes()); // total_size
        blob.extend_from_slice(&1u32.to_le_bytes()); // chunk_count = 1
        blob.extend_from_slice(&36u32.to_le_bytes()); // chunk 0 offset = 36
        blob.extend_from_slice(b"SHEX");
        blob.extend_from_slice(&4u32.to_le_bytes());
        // Vertex shader (1 << 16) | SM 5.0 (5 << 4)
        blob.extend_from_slice(&((1u32 << 16) | (5u32 << 4)).to_le_bytes());

        let parsed = ShaderContainer::parse(&blob).expect("DXBC valide");
        assert_eq!(parsed.format, ShaderFormat::Dxbc);
        assert_eq!(parsed.stage, ShaderStage::Vertex);
        assert_eq!((parsed.major_version, parsed.minor_version), (5, 0));
    }
}
