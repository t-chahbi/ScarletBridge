//! Localisation tolérante du bitcode DXIL dans un tampon : conteneur `DXBC` (chunks `DXIL`,
//! `ILDB`, `ILDN`), `DxilProgramHeader` nu ou bitcode brut (`BC\xC0\xDE` / wrapper `0x0B17C0DE`).
//!
//! Spécification (DirectXShaderCompiler, `DxilContainer.h`) : `DxilProgramHeader { ProgramVersion;
//! SizeInUint32; DxilBitcodeHeader { DxilFourCC; DxilVersion; BitcodeOffset; BitcodeSize } }`,
//! `BitcodeOffset` étant relatif au début du `DxilBitcodeHeader` (octet 8 du chunk).
//! Tolérances : taille du conteneur de l'en-tête ignorée (seule la longueur réelle du tampon
//! compte), chunks tronqués clampés, `BitcodeSize` clampé à la fin du chunk si le magic est bien
//! à `start`, offset invalide -> recherche du magic dans le chunk.

use crate::dxbc_to_spirv::ShaderTranslateError;

const BC_MAGIC: [u8; 4] = [0x42, 0x43, 0xC0, 0xDE];
const WRAPPER_MAGIC: [u8; 4] = [0xDE, 0xC0, 0x17, 0x0B];

pub(super) struct Located<'a> {
    pub bitcode: &'a [u8],
    #[allow(dead_code)] // conservé pour les diagnostics
    /// `ProgramVersion` (kind << 16 | major << 4 | minor) si un `DxilProgramHeader` est présent.
    pub program_version: Option<u32>,
    /// Anomalies tolérées (journalisées).
    pub notes: Vec<String>,
    /// Contexte pour les messages d'erreur ultérieurs.
    pub diag: String,
}

pub(super) struct Chunk<'a> {
    pub fourcc: [u8; 4],
    pub declared: usize,
    pub payload: &'a [u8],
}

fn rd(b: &[u8], o: usize) -> Option<u32> {
    let s = b.get(o..o.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn is_bc(b: &[u8]) -> bool {
    b.starts_with(&BC_MAGIC) || b.starts_with(&WRAPPER_MAGIC)
}

pub(super) fn hex16(b: &[u8]) -> String {
    b.iter()
        .take(16)
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn fourcc_str(f: &[u8; 4]) -> String {
    f.iter()
        .map(|&c| if c.is_ascii_graphic() { c as char } else { '?' })
        .collect()
}

/// Chunks d'un conteneur `DXBC` (tolérant : tailles clampées au tampon, offsets invalides sautés).
pub(super) fn chunks(buf: &[u8]) -> Vec<Chunk<'_>> {
    let mut out = Vec::new();
    if buf.len() < 32 || &buf[0..4] != b"DXBC" {
        return out;
    }
    let count = rd(buf, 28).unwrap_or(0) as usize;
    let count = count.min((buf.len() - 32) / 4);
    for i in 0..count {
        let Some(off) = rd(buf, 32 + i * 4).map(|v| v as usize) else {
            continue;
        };
        let (Some(fc), Some(sz)) = (
            buf.get(off..off.saturating_add(4)),
            rd(buf, off.saturating_add(4)),
        ) else {
            continue;
        };
        let start = off + 8;
        if start > buf.len() {
            continue;
        }
        let end = start.saturating_add(sz as usize).min(buf.len());
        out.push(Chunk {
            fourcc: [fc[0], fc[1], fc[2], fc[3]],
            declared: sz as usize,
            payload: &buf[start..end],
        });
    }
    out
}

/// Premier chunk `DXIL` (sinon `ILDB`) d'un conteneur.
pub(super) fn dxil_chunk(buf: &[u8]) -> Option<&[u8]> {
    let cs = chunks(buf);
    cs.iter()
        .find(|c| &c.fourcc == b"DXIL")
        .or_else(|| cs.iter().find(|c| &c.fourcc == b"ILDB"))
        .map(|c| c.payload)
}

/// Vrai si le tampon doit être traité par le traducteur DXIL.
pub(super) fn sniff_is_dxil(buf: &[u8]) -> bool {
    if buf.starts_with(b"DXBC") {
        return dxil_chunk(buf).is_some();
    }
    is_bc(buf) || (buf.len() >= 24 && buf.get(8..12) == Some(b"DXIL"))
}

fn from_payload<'a>(payload: &'a [u8], what: &str, total: usize) -> Result<Located<'a>, String> {
    let mut notes = Vec::new();
    let version = rd(payload, 0);
    let has_header = payload.len() >= 24 && payload.get(8..12) == Some(b"DXIL");
    let (off, size) = if has_header {
        (rd(payload, 16).unwrap_or(0), rd(payload, 20).unwrap_or(0))
    } else {
        (0, 0)
    };
    let start = 8usize.saturating_add(off as usize);
    let mut diag = format!(
        "conteneur {total} o, {what} {} o, version programme {}, offset {off}, taille {size}, début {start}, entête [{}]",
        payload.len(),
        version.map_or("?".into(), |v| format!("{v:#x}")),
        hex16(payload)
    );
    let mut found = None;
    if has_header && start < payload.len() && is_bc(&payload[start..]) {
        let size = size as usize;
        let end = if size >= 4 && start.saturating_add(size) <= payload.len() {
            start + size
        } else {
            notes.push(format!(
                "BitcodeSize {size} incohérent avec le chunk ({} o restants) : clampé",
                payload.len() - start
            ));
            payload.len()
        };
        found = Some(start..end);
    }
    if found.is_none() {
        let from = if has_header { 24 } else { 0 };
        let pos = payload
            .get(from..)
            .and_then(|s| {
                s.windows(4)
                    .position(|w| w == BC_MAGIC || w == WRAPPER_MAGIC)
            })
            .map(|p| p + from);
        if let Some(p) = pos {
            notes.push(format!(
                "BitcodeOffset {off} invalide : magic bitcode retrouvé à l'offset {p} du chunk"
            ));
            found = Some(p..payload.len());
        }
    }
    match found {
        Some(r) => {
            diag.push_str(&format!(", bitcode [{}..{}]", r.start, r.end));
            Ok(Located {
                bitcode: &payload[r],
                program_version: if has_header { version } else { None },
                notes,
                diag,
            })
        }
        None => Err(format!("{diag} : magic bitcode introuvable")),
    }
}

pub(super) fn locate(buf: &[u8]) -> Result<Located<'_>, ShaderTranslateError> {
    if buf.starts_with(b"DXBC") {
        let cs = chunks(buf);
        let list: Vec<String> = cs
            .iter()
            .map(|c| format!("{}:{}", fourcc_str(&c.fourcc), c.declared))
            .collect();
        let mut first_err = None;
        let ordered = ["DXIL", "ILDB", "ILDN"]
            .iter()
            .flat_map(|n| cs.iter().filter(move |c| c.fourcc == n.as_bytes()[..4]));
        for c in ordered {
            match from_payload(
                c.payload,
                &format!("chunk {}", fourcc_str(&c.fourcc)),
                buf.len(),
            ) {
                Ok(l) => return Ok(l),
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        return Err(ShaderTranslateError::InvalidDxil(match first_err {
            Some(e) => format!("{e} ; {} chunks [{}]", cs.len(), list.join(", ")),
            None => format!(
                "aucun chunk DXIL/ILDB dans le conteneur ({} o, {} chunks lisibles [{}])",
                buf.len(),
                cs.len(),
                list.join(", ")
            ),
        }));
    }
    if is_bc(buf) {
        return Ok(Located {
            bitcode: buf,
            program_version: None,
            notes: vec!["bitcode brut sans conteneur".into()],
            diag: format!("bitcode brut {} o, entête [{}]", buf.len(), hex16(buf)),
        });
    }
    if buf.len() >= 24 && buf.get(8..12) == Some(b"DXIL") {
        let mut l = from_payload(buf, "DxilProgramHeader nu", buf.len())
            .map_err(ShaderTranslateError::InvalidDxil)?;
        l.notes.push("DxilProgramHeader sans conteneur DXBC".into());
        return Ok(l);
    }
    if buf.len() < 32 {
        return Err(ShaderTranslateError::InvalidContainer);
    }
    Err(ShaderTranslateError::InvalidDxil(format!(
        "format de tampon non reconnu ({} o, entête [{}])",
        buf.len(),
        hex16(buf)
    )))
}
