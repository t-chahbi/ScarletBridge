//! Codec **DXBC / RTS0** complet pour les signatures racines Direct3D 12 (v1.0, v1.1, v1.2) :
//! - hachage MD5 variante DXBC (`dxbc_checksum`) ;
//! - sérialisation vers un conteneur DXBC valide contenant un chunk `RTS0` (`serialize_rts0`) ;
//! - décodage complet d'un conteneur DXBC/`RTS0` ou payload brut (`parse_rts0`) ;
//! - nettoyage de l'octet haut console Xbox (`0xFF00_0000`, observé `0x0300_0000`) dans les drapeaux ;
//! - compactage vers les 32 user-SGPR RDNA 2 (`pack_user_sgprs`).

#![allow(dead_code)]

/// Masque de l'octet haut des `D3D12_ROOT_SIGNATURE_FLAGS` réservé aux extensions console Xbox.
pub const XBOX_ROOT_SIGNATURE_FLAG_HIGH_BYTE: u32 = 0xFF00_0000;

/// Budget maximal D3D12 (64 dwords) et nombre maximal de user-SGPR RDNA 2 (32 dwords).
pub const MAX_D3D12_COST: u32 = 64;
pub const MAX_USER_SGPRS: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootParamKind {
    Table,
    Constants,
    Cbv,
    Srv,
    Uav,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DescriptorRange {
    pub range_type: u32,
    pub num_descriptors: u32,
    pub base_shader_register: u32,
    pub register_space: u32,
    pub flags: u32,
    pub offset_in_table: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootParam {
    pub kind: RootParamKind,
    pub visibility: u32,
    pub shader_register: u32,
    pub register_space: u32,
    pub num_values: u32,
    pub descriptor_flags: u32,
    pub ranges: Vec<DescriptorRange>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StaticSampler {
    pub filter: u32,
    pub address_u: u32,
    pub address_v: u32,
    pub address_w: u32,
    pub mip_lod_bias: f32,
    pub max_anisotropy: u32,
    pub comparison_func: u32,
    pub border_color: u32,
    pub min_lod: f32,
    pub max_lod: f32,
    pub shader_register: u32,
    pub register_space: u32,
    pub visibility: u32,
    pub flags: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedRootSignature {
    /// Version RTS0 : 1 = 1.0, 2 = 1.1, 3 = 1.2.
    pub version: u32,
    pub flags: u32,
    pub params: Vec<RootParam>,
    pub static_samplers: Vec<StaticSampler>,
}

/// Supprime l'octet haut console Xbox des drapeaux de signature racine.
pub const fn clear_xbox_root_sig_flags(flags: u32) -> u32 {
    flags & !XBOX_ROOT_SIGNATURE_FLAG_HIGH_BYTE
}

// ==================== MD5 variante DXBC ====================

fn md5_block(state: &mut [u32; 4], m: &[u32; 16]) {
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];
    const R: [u32; 16] = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];
    let [mut a, mut b, mut c, mut d] = *state;
    for (i, &ki) in K.iter().enumerate() {
        let (f, g) = if i < 16 {
            ((b & c) | (!b & d), i)
        } else if i < 32 {
            ((d & b) | (!d & c), (5 * i + 1) % 16)
        } else if i < 48 {
            (b ^ c ^ d, (3 * i + 5) % 16)
        } else {
            (c ^ (b | !d), (7 * i) % 16)
        };
        let t = d;
        d = c;
        c = b;
        let rot = R[(i / 16) * 4 + (i % 4)];
        b = b.wrapping_add(
            a.wrapping_add(f)
                .wrapping_add(ki)
                .wrapping_add(m[g])
                .rotate_left(rot),
        );
        a = t;
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
}

fn bytes_to_u32x16(block: &[u8; 64]) -> [u32; 16] {
    let mut m = [0u32; 16];
    for (i, slot) in m.iter_mut().enumerate() {
        let o = i * 4;
        *slot = u32::from_le_bytes([block[o], block[o + 1], block[o + 2], block[o + 3]]);
    }
    m
}

/// Calcule le condensat MD5 spécifique aux conteneurs DXBC (octets `20..len`).
pub fn dxbc_checksum(blob: &[u8]) -> [u32; 4] {
    if blob.len() < 20 {
        return [0; 4];
    }
    let data = &blob[20..];
    let n = data.len() as u32;
    let bits = n.wrapping_mul(8);
    let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let full = (data.len() / 64) * 64;
    for chunk in data[..full].as_chunks::<64>().0 {
        md5_block(&mut state, &bytes_to_u32x16(chunk));
    }
    let left = data.len() - full;
    let mut blk = [0u8; 64];
    let mut m = if left >= 56 {
        blk[..left].copy_from_slice(&data[full..]);
        blk[left] = 0x80;
        md5_block(&mut state, &bytes_to_u32x16(&blk));
        [0u32; 16]
    } else {
        blk[4..4 + left].copy_from_slice(&data[full..]);
        blk[4 + left] = 0x80;
        bytes_to_u32x16(&blk)
    };
    m[0] = bits;
    m[15] = (bits >> 2) | 1;
    md5_block(&mut state, &m);
    state
}

/// Nettoie les drapeaux hauts Xbox dans un blob DXBC/RTS0 et recalcule son checksum DXBC.
/// Renvoie `Some(blob_corrigé)` uniquement si l'octet haut était non nul.
pub fn clear_xbox_flags_in_rts0_blob(blob: &[u8]) -> Option<Vec<u8>> {
    if blob.len() < 36 || &blob[0..4] != b"DXBC" {
        return None;
    }
    let chunk_count = u32::from_le_bytes([blob[28], blob[29], blob[30], blob[31]]) as usize;
    if chunk_count == 0 || chunk_count > 16 || 32 + chunk_count * 4 > blob.len() {
        return None;
    }
    for i in 0..chunk_count {
        let off_pos = 32 + i * 4;
        let off = u32::from_le_bytes([
            blob[off_pos],
            blob[off_pos + 1],
            blob[off_pos + 2],
            blob[off_pos + 3],
        ]) as usize;
        if off.checked_add(32).is_none_or(|e| e > blob.len()) {
            continue;
        }
        if &blob[off..off + 4] != b"RTS0" {
            continue;
        }
        let flags_pos = off + 8 + 20;
        let flags = u32::from_le_bytes([
            blob[flags_pos],
            blob[flags_pos + 1],
            blob[flags_pos + 2],
            blob[flags_pos + 3],
        ]);
        let cleared = clear_xbox_root_sig_flags(flags);
        if cleared == flags {
            return None;
        }
        let mut out = blob.to_vec();
        out[flags_pos..flags_pos + 4].copy_from_slice(&cleared.to_le_bytes());
        let hash = dxbc_checksum(&out);
        for (k, w) in hash.iter().enumerate() {
            out[4 + k * 4..8 + k * 4].copy_from_slice(&w.to_le_bytes());
        }
        return Some(out);
    }
    None
}

// ==================== Décodeur RTS0 ====================

fn rd_u32(data: &[u8], off: usize) -> Option<u32> {
    let b = data.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn rd_f32(data: &[u8], off: usize) -> Option<f32> {
    Some(f32::from_bits(rd_u32(data, off)?))
}

/// Décode un payload `RTS0` brut (sans l'en-tête DXBC).
pub fn parse_rts0_payload(data: &[u8]) -> Result<ParsedRootSignature, &'static str> {
    let ver = rd_u32(data, 0).ok_or("en-tête RTS0 tronqué")?;
    let nparams = rd_u32(data, 4).ok_or("en-tête RTS0 tronqué")? as usize;
    let params_off = rd_u32(data, 8).ok_or("en-tête RTS0 tronqué")? as usize;
    let nsamplers = rd_u32(data, 12).ok_or("en-tête RTS0 tronqué")? as usize;
    let samplers_off = rd_u32(data, 16).ok_or("en-tête RTS0 tronqué")? as usize;
    let flags = rd_u32(data, 20).ok_or("en-tête RTS0 tronqué")?;

    if !(1..=3).contains(&ver) {
        return Err("version RTS0 inconnue");
    }
    if nparams > 64 {
        return Err("plus de 64 paramètres racines RTS0");
    }
    if nsamplers > 4096 {
        return Err("nombre de samplers statiques RTS0 excessif");
    }

    let flags_v11 = ver >= 2;
    let mut params = Vec::with_capacity(nparams);
    for i in 0..nparams {
        let eoff = params_off
            .checked_add(i * 12)
            .ok_or("table paramètres RTS0 hors limites")?;
        let dtype = rd_u32(data, eoff).ok_or("entrée paramètre RTS0 tronquée")?;
        let visibility = rd_u32(data, eoff + 4).ok_or("entrée paramètre RTS0 tronquée")?;
        let poff = rd_u32(data, eoff + 8).ok_or("entrée paramètre RTS0 tronquée")? as usize;

        match dtype {
            0 => {
                let nranges =
                    rd_u32(data, poff).ok_or("table descripteurs RTS0 tronquée")? as usize;
                let ranges_off =
                    rd_u32(data, poff + 4).ok_or("table descripteurs RTS0 tronquée")? as usize;
                if nranges > 1024 {
                    return Err("nombre de plages RTS0 excessif");
                }
                let stride = if flags_v11 { 24 } else { 20 };
                let mut ranges = Vec::with_capacity(nranges);
                for j in 0..nranges {
                    let roff = ranges_off
                        .checked_add(j * stride)
                        .ok_or("plages RTS0 hors limites")?;
                    let range_type = rd_u32(data, roff).ok_or("plage RTS0 tronquée")?;
                    let num_descriptors = rd_u32(data, roff + 4).ok_or("plage RTS0 tronquée")?;
                    let base_shader_register =
                        rd_u32(data, roff + 8).ok_or("plage RTS0 tronquée")?;
                    let register_space = rd_u32(data, roff + 12).ok_or("plage RTS0 tronquée")?;
                    let (rflags, offset_in_table) = if flags_v11 {
                        (
                            rd_u32(data, roff + 16).ok_or("plage RTS0 tronquée")?,
                            rd_u32(data, roff + 20).ok_or("plage RTS0 tronquée")?,
                        )
                    } else {
                        (0, rd_u32(data, roff + 16).ok_or("plage RTS0 tronquée")?)
                    };
                    ranges.push(DescriptorRange {
                        range_type,
                        num_descriptors,
                        base_shader_register,
                        register_space,
                        flags: rflags,
                        offset_in_table,
                    });
                }
                let first_reg = ranges.first().map_or(0, |r| r.base_shader_register);
                params.push(RootParam {
                    kind: RootParamKind::Table,
                    visibility,
                    shader_register: first_reg,
                    register_space: 0,
                    num_values: 0,
                    descriptor_flags: 0,
                    ranges,
                });
            }
            1 => {
                let shader_register = rd_u32(data, poff).ok_or("constantes RTS0 tronquées")?;
                let register_space = rd_u32(data, poff + 4).ok_or("constantes RTS0 tronquées")?;
                let num_values = rd_u32(data, poff + 8).ok_or("constantes RTS0 tronquées")?;
                params.push(RootParam {
                    kind: RootParamKind::Constants,
                    visibility,
                    shader_register,
                    register_space,
                    num_values,
                    descriptor_flags: 0,
                    ranges: Vec::new(),
                });
            }
            2..=4 => {
                let kind = match dtype {
                    2 => RootParamKind::Cbv,
                    3 => RootParamKind::Srv,
                    _ => RootParamKind::Uav,
                };
                let shader_register = rd_u32(data, poff).ok_or("descripteur RTS0 tronqué")?;
                let register_space = rd_u32(data, poff + 4).ok_or("descripteur RTS0 tronqué")?;
                let descriptor_flags = if flags_v11 {
                    rd_u32(data, poff + 8).ok_or("drapeaux descripteur RTS0 tronqués")?
                } else {
                    0
                };
                params.push(RootParam {
                    kind,
                    visibility,
                    shader_register,
                    register_space,
                    num_values: 0,
                    descriptor_flags,
                    ranges: Vec::new(),
                });
            }
            _ => return Err("type de paramètre RTS0 inconnu"),
        }
    }

    let sampler_flags = ver == 3;
    let sampler_stride = if sampler_flags { 56 } else { 52 };
    let mut static_samplers = Vec::with_capacity(nsamplers);
    for i in 0..nsamplers {
        let soff = samplers_off
            .checked_add(i * sampler_stride)
            .ok_or("samplers RTS0 hors limites")?;
        let s = StaticSampler {
            filter: rd_u32(data, soff).ok_or("sampler RTS0 tronqué")?,
            address_u: rd_u32(data, soff + 4).ok_or("sampler RTS0 tronqué")?,
            address_v: rd_u32(data, soff + 8).ok_or("sampler RTS0 tronqué")?,
            address_w: rd_u32(data, soff + 12).ok_or("sampler RTS0 tronqué")?,
            mip_lod_bias: rd_f32(data, soff + 16).ok_or("sampler RTS0 tronqué")?,
            max_anisotropy: rd_u32(data, soff + 20).ok_or("sampler RTS0 tronqué")?,
            comparison_func: rd_u32(data, soff + 24).ok_or("sampler RTS0 tronqué")?,
            border_color: rd_u32(data, soff + 28).ok_or("sampler RTS0 tronqué")?,
            min_lod: rd_f32(data, soff + 32).ok_or("sampler RTS0 tronqué")?,
            max_lod: rd_f32(data, soff + 36).ok_or("sampler RTS0 tronqué")?,
            shader_register: rd_u32(data, soff + 40).ok_or("sampler RTS0 tronqué")?,
            register_space: rd_u32(data, soff + 44).ok_or("sampler RTS0 tronqué")?,
            visibility: rd_u32(data, soff + 48).ok_or("sampler RTS0 tronqué")?,
            flags: if sampler_flags {
                rd_u32(data, soff + 52).ok_or("drapeaux sampler RTS0 tronqués")?
            } else {
                0
            },
        };
        static_samplers.push(s);
    }

    Ok(ParsedRootSignature {
        version: ver,
        flags: clear_xbox_root_sig_flags(flags),
        params,
        static_samplers,
    })
}

/// Décode un conteneur DXBC contenant un chunk `RTS0` (ou un payload `RTS0` nu).
pub fn parse_rts0(data: &[u8]) -> Result<ParsedRootSignature, &'static str> {
    if data.len() < 4 {
        return Err("blob trop petit pour DXBC/RTS0");
    }
    if &data[0..4] != b"DXBC" {
        return parse_rts0_payload(data);
    }
    let nchunks = rd_u32(data, 28).ok_or("en-tête DXBC tronqué")? as usize;
    if nchunks > 1024 {
        return Err("nombre de chunks DXBC excessif");
    }
    for k in 0..nchunks {
        let off = rd_u32(data, 32 + k * 4).ok_or("table chunks DXBC tronquée")? as usize;
        let hdr_end = off.checked_add(8).ok_or("offset chunk DXBC invalide")?;
        if hdr_end > data.len() {
            return Err("chunk DXBC hors limites");
        }
        if &data[off..off + 4] == b"RTS0" {
            let payload_len = rd_u32(data, off + 4).ok_or("taille RTS0 tronquée")? as usize;
            let payload_end = hdr_end
                .checked_add(payload_len)
                .ok_or("taille RTS0 hors limites")?;
            if payload_end > data.len() {
                return Err("payload RTS0 dépasse le conteneur");
            }
            return parse_rts0_payload(&data[hdr_end..payload_end]);
        }
    }
    Err("aucun chunk RTS0 dans le conteneur DXBC")
}

// ==================== Sérialiseur DXBC / RTS0 ====================

/// Sérialise une `ParsedRootSignature` en un conteneur DXBC valide (chunk `RTS0` + MD5 DXBC).
pub fn serialize_rts0_dxbc(sig: &ParsedRootSignature) -> Vec<u8> {
    let ver = sig.version.clamp(1, 3);
    let flags_v11 = ver >= 2;
    let sampler_flags = ver == 3;

    // 1. Construire le payload RTS0.
    let mut payload = Vec::new();
    let put_u32 = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_le_bytes());

    let params_off = 24u32;
    // Réserve en-tête (24 o) + table de paramètres (12 o × nparams).
    payload.resize(24 + sig.params.len() * 12, 0);

    // Corps de chaque paramètre.
    let mut param_offsets = Vec::with_capacity(sig.params.len());
    for p in &sig.params {
        let poff = payload.len() as u32;
        param_offsets.push(poff);
        match p.kind {
            RootParamKind::Table => {
                let nranges = p.ranges.len() as u32;
                let ranges_off = poff + 8;
                put_u32(&mut payload, nranges);
                put_u32(&mut payload, ranges_off);
                for r in &p.ranges {
                    put_u32(&mut payload, r.range_type);
                    put_u32(&mut payload, r.num_descriptors);
                    put_u32(&mut payload, r.base_shader_register);
                    put_u32(&mut payload, r.register_space);
                    if flags_v11 {
                        put_u32(&mut payload, r.flags);
                    }
                    put_u32(&mut payload, r.offset_in_table);
                }
            }
            RootParamKind::Constants => {
                put_u32(&mut payload, p.shader_register);
                put_u32(&mut payload, p.register_space);
                put_u32(&mut payload, p.num_values);
            }
            RootParamKind::Cbv | RootParamKind::Srv | RootParamKind::Uav => {
                put_u32(&mut payload, p.shader_register);
                put_u32(&mut payload, p.register_space);
                if flags_v11 {
                    put_u32(&mut payload, p.descriptor_flags);
                }
            }
        }
    }

    let samplers_off = payload.len() as u32;
    for s in &sig.static_samplers {
        put_u32(&mut payload, s.filter);
        put_u32(&mut payload, s.address_u);
        put_u32(&mut payload, s.address_v);
        put_u32(&mut payload, s.address_w);
        put_u32(&mut payload, s.mip_lod_bias.to_bits());
        put_u32(&mut payload, s.max_anisotropy);
        put_u32(&mut payload, s.comparison_func);
        put_u32(&mut payload, s.border_color);
        put_u32(&mut payload, s.min_lod.to_bits());
        put_u32(&mut payload, s.max_lod.to_bits());
        put_u32(&mut payload, s.shader_register);
        put_u32(&mut payload, s.register_space);
        put_u32(&mut payload, s.visibility);
        if sampler_flags {
            put_u32(&mut payload, s.flags);
        }
    }

    // Remplit l'en-tête RTS0 et la table de paramètres.
    let hdr = [
        ver,
        sig.params.len() as u32,
        params_off,
        sig.static_samplers.len() as u32,
        samplers_off,
        clear_xbox_root_sig_flags(sig.flags),
    ];
    for (i, w) in hdr.iter().enumerate() {
        payload[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    for (i, p) in sig.params.iter().enumerate() {
        let dtype: u32 = match p.kind {
            RootParamKind::Table => 0,
            RootParamKind::Constants => 1,
            RootParamKind::Cbv => 2,
            RootParamKind::Srv => 3,
            RootParamKind::Uav => 4,
        };
        let off = 24 + i * 12;
        payload[off..off + 4].copy_from_slice(&dtype.to_le_bytes());
        payload[off + 4..off + 8].copy_from_slice(&p.visibility.to_le_bytes());
        payload[off + 8..off + 12].copy_from_slice(&param_offsets[i].to_le_bytes());
    }

    // 2. Enveloppe dans un conteneur DXBC à 1 chunk (`RTS0`).
    let chunk_off = 36u32; // 32 (en-tête DXBC) + 4 (1 offset de chunk)
    let total_len = chunk_off as usize + 8 + payload.len();
    let mut dxbc = Vec::with_capacity(total_len);
    dxbc.extend_from_slice(b"DXBC");
    dxbc.extend_from_slice(&[0u8; 16]); // emplacement MD5
    put_u32(&mut dxbc, 1); // version DXBC = 1
    put_u32(&mut dxbc, total_len as u32);
    put_u32(&mut dxbc, 1); // 1 chunk
    put_u32(&mut dxbc, chunk_off);
    dxbc.extend_from_slice(b"RTS0");
    put_u32(&mut dxbc, payload.len() as u32);
    dxbc.extend_from_slice(&payload);

    let hash = dxbc_checksum(&dxbc);
    for (k, w) in hash.iter().enumerate() {
        dxbc[4 + k * 4..8 + k * 4].copy_from_slice(&w.to_le_bytes());
    }
    dxbc
}

/// Calcule le mapping `param_index -> premier user-SGPR` (dwords) et le coût D3D12 total.
pub fn pack_user_sgprs(sig: &ParsedRootSignature) -> Result<(Vec<u32>, u32), &'static str> {
    if sig.params.len() > 64 {
        return Err("plus de 64 paramètres racines");
    }
    let mut first_sgpr = Vec::with_capacity(sig.params.len());
    let mut cost = 0u64;
    let mut sgpr = 0u64;
    for p in &sig.params {
        let (param_cost, dwords) = match p.kind {
            RootParamKind::Table => (1u64, 2u64),
            RootParamKind::Constants => {
                if p.num_values == 0 {
                    return Err("paramètre constantes racine avec 0 dword");
                }
                (u64::from(p.num_values), u64::from(p.num_values))
            }
            RootParamKind::Cbv | RootParamKind::Srv | RootParamKind::Uav => (2u64, 2u64),
        };
        cost += param_cost;
        if cost > u64::from(MAX_D3D12_COST) {
            return Err("coût signature racine > 64 dwords");
        }
        if sgpr + dwords > u64::from(MAX_USER_SGPRS) {
            return Err("dépassement des 32 user-SGPR (spill non modélisé)");
        }
        first_sgpr.push(sgpr as u32);
        sgpr += dwords;
    }
    Ok((first_sgpr, cost as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rts0_serialize_parse_and_xbox_flag_strip() {
        let orig = ParsedRootSignature {
            version: 2, // 1.1
            flags: 0x0300_0001,
            params: vec![
                RootParam {
                    kind: RootParamKind::Cbv,
                    visibility: 0,
                    shader_register: 0,
                    register_space: 0,
                    num_values: 0,
                    descriptor_flags: 2,
                    ranges: Vec::new(),
                },
                RootParam {
                    kind: RootParamKind::Table,
                    visibility: 5,
                    shader_register: 0,
                    register_space: 0,
                    num_values: 0,
                    descriptor_flags: 0,
                    ranges: vec![DescriptorRange {
                        range_type: 0, // SRV
                        num_descriptors: 4,
                        base_shader_register: 2,
                        register_space: 1,
                        flags: 0,
                        offset_in_table: 0,
                    }],
                },
                RootParam {
                    kind: RootParamKind::Constants,
                    visibility: 1,
                    shader_register: 3,
                    register_space: 0,
                    num_values: 4,
                    descriptor_flags: 0,
                    ranges: Vec::new(),
                },
            ],
            static_samplers: vec![StaticSampler {
                filter: 0x15,
                address_u: 1,
                address_v: 1,
                address_w: 1,
                mip_lod_bias: 0.0,
                max_anisotropy: 1,
                comparison_func: 1,
                border_color: 0,
                min_lod: 0.0,
                max_lod: 16.0,
                shader_register: 0,
                register_space: 0,
                visibility: 5,
                flags: 0,
            }],
        };

        let mut blob = serialize_rts0_dxbc(&orig);
        assert_eq!(&blob[0..4], b"DXBC");
        let parsed = parse_rts0(&blob).expect("parse RTS0");
        assert_eq!(parsed.flags, 1, "octet haut Xbox 0x03000000 effacé");
        assert_eq!(parsed.params.len(), 3);
        assert_eq!(parsed.params[1].ranges[0].base_shader_register, 2);
        assert_eq!(parsed.static_samplers.len(), 1);

        // Réinjecte le drapeau haut Xbox dans le blob brut et vérifie `clear_xbox_flags_in_rts0_blob`.
        let rts_flags_pos = 36 + 8 + 20;
        blob[rts_flags_pos..rts_flags_pos + 4].copy_from_slice(&0x0300_0001u32.to_le_bytes());
        let cleaned = clear_xbox_flags_in_rts0_blob(&blob).expect("nettoyage attendu");
        let reparsed = parse_rts0(&cleaned).expect("reparse");
        assert_eq!(reparsed.flags, 1);

        // Vérifie le compactage user-SGPR (CBV=2, Table=2, Constants=4 -> [0, 2, 4], coût=7).
        let (sgprs, cost) = pack_user_sgprs(&reparsed).expect("pack");
        assert_eq!(sgprs, vec![0, 2, 4]);
        assert_eq!(cost, 7);
    }
}
