//! Lecteur générique de flux de bits LLVM (bitstream) pour DXIL (LLVM 3.7).
//!
//! Produit un arbre `Block` / `Record` complet (abréviations, `BLOCKINFO`, tableaux, char6, blobs).
//! Toutes les allocations sont bornées par la taille de l'entrée : aucune panique sur flux
//! malformé (largeurs, longueurs, imbrication et nombre d'opérandes sont validés).

use super::DxilParseError;

/// Magic du bitcode LLVM (`BC\xC0\xDE`).
pub const BITCODE_MAGIC: [u8; 4] = [0x42, 0x43, 0xC0, 0xDE];
const WRAPPER_MAGIC: u32 = 0x0B17_C0DE;
const MAX_DEPTH: usize = 12;

#[derive(Debug, Clone)]
pub struct Record {
    pub code: u32,
    pub ops: Vec<u64>,
    pub blob: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct Block {
    pub id: u32,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone)]
pub enum Item {
    Record(Record),
    Block(Block),
}

#[derive(Debug, Clone, Copy)]
enum AbbrevOp {
    Literal(u64),
    Fixed(u32),
    Vbr(u32),
    Array,
    Char6,
    Blob,
}

type Abbrev = Vec<AbbrevOp>;

struct Reader<'a> {
    data: &'a [u8],
    pos: usize, // en bits
    budget: usize,
}

fn err<T>(msg: &str) -> Result<T, DxilParseError> {
    Err(DxilParseError::Bitcode(msg.to_string()))
}

impl<'a> Reader<'a> {
    fn bits_left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }

    fn read(&mut self, n: u32) -> Result<u64, DxilParseError> {
        if n == 0 {
            return Ok(0);
        }
        if n > 64 || self.bits_left() < n as usize {
            return err("flux binaire tronqué");
        }
        let mut out = 0u64;
        let mut got = 0u32;
        while got < n {
            let byte = self.data[self.pos / 8];
            let off = (self.pos % 8) as u32;
            let take = (8 - off).min(n - got);
            let bits = (u64::from(byte) >> off) & ((1u64 << take) - 1);
            out |= bits << got;
            got += take;
            self.pos += take as usize;
        }
        Ok(out)
    }

    fn vbr(&mut self, w: u32) -> Result<u64, DxilParseError> {
        if w == 0 {
            return Ok(0);
        }
        if !(2..=32).contains(&w) {
            return err("largeur VBR invalide");
        }
        let hi = 1u64 << (w - 1);
        let mut out = 0u64;
        let mut shift = 0u32;
        loop {
            let c = self.read(w)?;
            if shift < 64 {
                out |= (c & (hi - 1)) << shift;
            }
            if c & hi == 0 {
                return Ok(out);
            }
            shift += w - 1;
            if shift > 70 {
                return err("VBR trop long");
            }
        }
    }

    fn align32(&mut self) {
        self.pos = self.pos.div_ceil(32) * 32;
    }

    fn spend(&mut self, n: usize) -> Result<(), DxilParseError> {
        if n > self.budget {
            return err("budget d'opérandes dépassé");
        }
        self.budget -= n;
        Ok(())
    }
}

fn char6(v: u64) -> u64 {
    const T: &[u8; 64] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._";
    u64::from(T[(v & 63) as usize])
}

#[derive(Default)]
struct BlockInfo {
    // (block id, abbrevs)
    abbrevs: Vec<(u32, Vec<Abbrev>)>,
}

impl BlockInfo {
    fn get(&self, id: u32) -> Vec<Abbrev> {
        self.abbrevs
            .iter()
            .find(|(b, _)| *b == id)
            .map(|(_, a)| a.clone())
            .unwrap_or_default()
    }
    fn push(&mut self, id: u32, a: Abbrev) {
        if let Some((_, v)) = self.abbrevs.iter_mut().find(|(b, _)| *b == id) {
            v.push(a);
        } else {
            self.abbrevs.push((id, vec![a]));
        }
    }
}

fn read_define_abbrev(r: &mut Reader) -> Result<Abbrev, DxilParseError> {
    let n = r.vbr(5)? as usize;
    if n > 4096 {
        return err("abréviation trop longue");
    }
    let mut ops = Vec::with_capacity(n.min(64));
    let mut i = 0;
    while i < n {
        let lit = r.read(1)?;
        if lit == 1 {
            ops.push(AbbrevOp::Literal(r.vbr(8)?));
        } else {
            let enc = r.read(3)?;
            match enc {
                1 => {
                    let w = r.vbr(5)? as u32;
                    if w > 64 {
                        return err("Fixed trop large");
                    }
                    ops.push(AbbrevOp::Fixed(w));
                }
                2 => {
                    let w = r.vbr(5)? as u32;
                    if w > 32 || w == 1 {
                        return err("VBR invalide");
                    }
                    ops.push(AbbrevOp::Vbr(w));
                }
                3 => ops.push(AbbrevOp::Array),
                4 => ops.push(AbbrevOp::Char6),
                5 => ops.push(AbbrevOp::Blob),
                _ => return err("encodage d'abréviation inconnu"),
            }
        }
        i += 1;
    }
    Ok(ops)
}

fn read_scalar(r: &mut Reader, op: AbbrevOp) -> Result<u64, DxilParseError> {
    match op {
        AbbrevOp::Literal(v) => Ok(v),
        AbbrevOp::Fixed(w) => r.read(w),
        AbbrevOp::Vbr(w) => r.vbr(w),
        AbbrevOp::Char6 => Ok(char6(r.read(6)?)),
        _ => err("opérande non scalaire"),
    }
}

fn read_abbrev_record(r: &mut Reader, ab: &Abbrev) -> Result<Record, DxilParseError> {
    let mut vals: Vec<u64> = Vec::new();
    let mut blob = None;
    let mut i = 0;
    while i < ab.len() {
        match ab[i] {
            AbbrevOp::Array => {
                let Some(&elt) = ab.get(i + 1) else {
                    return err("tableau sans type d'élément");
                };
                let n = r.vbr(6)? as usize;
                r.spend(n)?;
                if n > r.bits_left() && !matches!(elt, AbbrevOp::Fixed(0) | AbbrevOp::Literal(_)) {
                    return err("tableau plus grand que le flux");
                }
                for _ in 0..n {
                    vals.push(read_scalar(r, elt)?);
                }
                i += 2;
                continue;
            }
            AbbrevOp::Blob => {
                let n = r.vbr(6)? as usize;
                r.align32();
                if n > r.bits_left() / 8 {
                    return err("blob tronqué");
                }
                let start = r.pos / 8;
                blob = Some(r.data[start..start + n].to_vec());
                r.pos += n * 8;
                r.align32();
            }
            op => {
                r.spend(1)?;
                vals.push(read_scalar(r, op)?);
            }
        }
        i += 1;
    }
    if vals.is_empty() {
        return err("record abrégé sans code");
    }
    let code = vals.remove(0);
    Ok(Record {
        code: code as u32,
        ops: vals,
        blob,
    })
}

fn read_block_body(
    r: &mut Reader,
    id: u32,
    width: u32,
    end_bit: usize,
    info: &mut BlockInfo,
    depth: usize,
) -> Result<Block, DxilParseError> {
    if depth > MAX_DEPTH {
        return err("imbrication de blocs trop profonde");
    }
    let mut abbrevs: Vec<Abbrev> = info.get(id);
    let mut items = Vec::new();
    let mut cur_bid: Option<u32> = None; // SETBID dans BLOCKINFO
    loop {
        if r.pos >= end_bit {
            return err("END_BLOCK manquant");
        }
        let a = r.read(width)? as u32;
        match a {
            0 => {
                r.align32();
                r.pos = end_bit;
                return Ok(Block { id, items });
            }
            1 => {
                let bid = r.vbr(8)? as u32;
                let w = r.vbr(4)? as u32;
                if !(2..=32).contains(&w) {
                    return err("largeur d'abréviation invalide");
                }
                r.align32();
                let words = r.read(32)? as usize;
                let end = r.pos.saturating_add(words.saturating_mul(32));
                if end > r.data.len() * 8 {
                    return err("bloc plus long que le flux");
                }
                let b = read_block_body(r, bid, w, end, info, depth + 1)?;
                items.push(Item::Block(b));
            }
            2 => {
                let ab = read_define_abbrev(r)?;
                if id == 0 {
                    match cur_bid {
                        Some(b) => info.push(b, ab),
                        None => return err("DEFINE_ABBREV sans SETBID"),
                    }
                } else {
                    abbrevs.push(ab);
                }
            }
            3 => {
                let code = r.vbr(6)? as u32;
                let n = r.vbr(6)? as usize;
                r.spend(n.saturating_add(1))?;
                if n > r.bits_left() {
                    return err("record plus grand que le flux");
                }
                let mut ops = Vec::with_capacity(n);
                for _ in 0..n {
                    ops.push(r.vbr(6)?);
                }
                if id == 0 && code == 1 {
                    cur_bid = ops.first().map(|&v| v as u32);
                }
                items.push(Item::Record(Record {
                    code,
                    ops,
                    blob: None,
                }));
            }
            n => {
                let Some(ab) = abbrevs.get((n - 4) as usize) else {
                    return err("identifiant d'abréviation inconnu");
                };
                let ab = ab.clone();
                let rec = read_abbrev_record(r, &ab)?;
                items.push(Item::Record(rec));
            }
        }
    }
}

/// Décode un flux bitcode (avec ou sans wrapper `0x0B17C0DE`) en blocs de premier niveau.
pub fn parse_bitcode(data: &[u8]) -> Result<Vec<Block>, DxilParseError> {
    let mut data = data;
    if data.len() >= 20 && u32::from_le_bytes([data[0], data[1], data[2], data[3]]) == WRAPPER_MAGIC
    {
        let off = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;
        let size = u32::from_le_bytes([data[12], data[13], data[14], data[15]]) as usize;
        let end = off.checked_add(size).filter(|&e| e <= data.len());
        match end {
            Some(e) if off >= 20 => data = &data[off..e],
            _ => return err("wrapper bitcode invalide"),
        }
    }
    if data.len() < 8 || data[..4] != BITCODE_MAGIC {
        return err("magic BC\\xC0\\xDE absent");
    }
    let mut r = Reader {
        data,
        pos: 32,
        budget: data.len().saturating_mul(8) + (1 << 16),
    };
    let mut info = BlockInfo::default();
    let mut top = Vec::new();
    while r.bits_left() >= 32 {
        let a = r.read(2)? as u32;
        if a != 1 {
            // Rembourrage de fin de flux.
            break;
        }
        let bid = r.vbr(8)? as u32;
        let w = r.vbr(4)? as u32;
        if !(2..=32).contains(&w) {
            return err("largeur d'abréviation invalide");
        }
        r.align32();
        let words = r.read(32)? as usize;
        let end = r.pos.saturating_add(words.saturating_mul(32));
        if end > data.len() * 8 {
            return err("bloc plus long que le flux");
        }
        let b = read_block_body(&mut r, bid, w, end, &mut info, 0)?;
        top.push(b);
    }
    Ok(top)
}

/// Lecteur VBR6 autonome (table de longueurs de `METADATA_STRINGS`).
pub struct VbrReader<'a> {
    r: Reader<'a>,
}

impl<'a> VbrReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            r: Reader {
                data,
                pos: 0,
                budget: usize::MAX,
            },
        }
    }

    pub fn vbr6(&mut self) -> Option<u64> {
        self.r.vbr(6).ok()
    }
}
