//! Décompresseur GDeflate (DirectStorage `DSTORAGE_COMPRESSION_FORMAT_GDEFLATE = 1`) en Rust pur.
//!
//! # Format (faits établis d'après la documentation publique de microsoft/DirectStorage et la
//! # description du format DEFLATE de la RFC 1951 ; implémentation entièrement originale)
//!
//! ## Flux
//! ```text
//! +0   u8   id      = 4 (GDeflate)
//! +1   u8   magic   = id ^ 0xFF
//! +2   u16  numTiles                     (little-endian)
//! +4   u32  bits 0..2   tileSizeIdx = 1  (tuiles de 64 Kio)
//!           bits 2..20  lastTileSize     (0 = la dernière tuile est pleine)
//!           bits 20..32 réservé
//! +8   u32[numTiles]  table des tuiles :
//!           [0]   = taille compressée de la DERNIÈRE tuile
//!           [i>0] = déplacement (depuis la fin de la table) du début de la tuile i
//! ...  données des tuiles, concaténées
//! ```
//! La taille décompressée vaut `numTiles * 65536 - (lastTileSize ? 65536 - lastTileSize : 0)`.
//! Les tuiles sont indépendantes (la fenêtre de références arrière ne dépasse pas la tuile), ce qui
//! permet de les décompresser en parallèle.
//!
//! ## Tuile
//! Une tuile est un flux DEFLATE (variante « Deflate64 » : fenêtre de 64 Kio, symboles de longueur
//! 285..=287 = base 3 + 16 bits d'extension, symboles de distance 30/31 = bases 32769/49153 + 14
//! bits) dont les bits sont répartis sur 32 sous-flux entrelacés :
//!
//! * chaque sous-flux dispose d'un tampon de bits de 64 bits ; à l'initialisation, les sous-flux
//!   0..31 chargent chacun 32 bits (4 octets little-endian) dans l'ordre ;
//! * ensuite, à la fin de chaque « tour », si le sous-flux courant a moins de 32 bits, il charge les
//!   4 octets suivants du flux sérialisé (les chargements apparaissent donc dans l'ordre exact où
//!   le décodeur séquentiel les demande) ;
//! * l'en-tête de bloc (BFINAL, BTYPE), les 14 bits d'en-tête dynamique, `LEN` d'un bloc stocké
//!   appartiennent au sous-flux 0 ;
//! * en-tête dynamique : les longueurs du code de précode (3 bits) sont réparties en round-robin sur
//!   les sous-flux à partir de 0, puis les symboles du précode (et leurs bits d'extension) aussi ;
//! * blocs de symboles : à chaque tour, le sous-flux courant décode un symbole littéral/longueur ;
//!   un littéral est écrit immédiatement ; une longueur est mémorisée (copie différée) et la distance
//!   correspondante est décodée au prochain tour **du même sous-flux** (qui n'obtient donc pas de
//!   nouveau symbole littéral/longueur entre-temps) ; la copie est alors exécutée ;
//! * à la fin de bloc (symbole 256), les copies encore en attente sont exécutées en parcourant les 32
//!   sous-flux à partir du courant ;
//! * bloc stocké : `LEN` sur 16 bits (sous-flux 0, sans `NLEN`), puis un octet par sous-flux en
//!   round-robin à partir de 0 ;
//! * chaque bloc commence sur le sous-flux 0.
//!
//! Les lectures au-delà de la fin d'une tuile renvoient des zéros (le format impose un rembourrage
//! nul) ; une consommation excessive au-delà de la fin déclenche [`GDeflateError::Truncated`].
//!
//! Aucune `unsafe` : tous les accès sont vérifiés, aucune panique sur entrée malformée.

use std::fmt;

/// Taille d'une tuile décompressée.
pub const TILE_SIZE: usize = 64 * 1024;
/// Identifiant de codec GDeflate dans l'en-tête de flux.
pub const GDEFLATE_ID: u8 = 4;
/// Nombre maximal de tuiles d'un flux (champ `u16`).
pub const MAX_TILES: usize = 0xFFFF;

const HEADER_LEN: usize = 8;
const NUM_STREAMS: usize = 32;
const FAST_BITS: u32 = 10;
const MAX_CODE_LEN: usize = 15;
/// Marge (octets) de lecture autorisée au-delà de la fin d'une tuile avant de la juger tronquée.
const OVERREAD_SLACK: usize = 4 * NUM_STREAMS * 2;

/// Erreurs de décompression GDeflate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GDeflateError {
    /// Flux plus court que son en-tête / sa table de tuiles ne l'indique.
    Truncated,
    /// Identifiant, magic ou champs d'en-tête invalides.
    BadHeader,
    /// Table des tailles de tuiles incohérente (ordre, bornes).
    BadTileTable,
    /// Tampon de sortie plus petit que la taille décompressée du flux.
    OutputTooSmall,
    /// Type de bloc DEFLATE invalide (3).
    InvalidBlockType,
    /// Longueurs de codes Huffman invalides (sur-souscrit/incomplet).
    InvalidHuffmanCode,
    /// Symbole non décodable (code absent, symbole réservé).
    InvalidSymbol,
    /// Distance de copie qui dépasse le début de la tuile.
    DistanceTooFar,
    /// La sortie dépasserait la taille de la tuile.
    OutputOverrun,
    /// Le flux de la tuile s'est terminé avant d'avoir produit toute la sortie.
    ShortOutput,
}

impl fmt::Display for GDeflateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Truncated => "flux GDeflate tronqué",
            Self::BadHeader => "en-tête GDeflate invalide",
            Self::BadTileTable => "table des tuiles GDeflate incohérente",
            Self::OutputTooSmall => "tampon de sortie trop petit",
            Self::InvalidBlockType => "type de bloc invalide",
            Self::InvalidHuffmanCode => "code Huffman invalide",
            Self::InvalidSymbol => "symbole Huffman invalide",
            Self::DistanceTooFar => "distance hors fenêtre",
            Self::OutputOverrun => "débordement de la sortie de tuile",
            Self::ShortOutput => "sortie de tuile incomplète",
        };
        f.write_str(s)
    }
}

impl std::error::Error for GDeflateError {}

// ---------------------------------------------------------------------------------------------
// En-tête de flux
// ---------------------------------------------------------------------------------------------

struct Parsed<'a> {
    num_tiles: usize,
    total: usize,
    /// Table des tuiles (4 octets × `num_tiles`).
    table: &'a [u8],
    /// Données des tuiles (après la table).
    data: &'a [u8],
}

fn read_u32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([
        *s.first()?,
        *s.get(1)?,
        *s.get(2)?,
        *s.get(3)?,
    ]))
}

fn parse(src: &[u8]) -> Result<Parsed<'_>, GDeflateError> {
    let hdr = src.get(..HEADER_LEN).ok_or(GDeflateError::Truncated)?;
    let (id, magic) = (hdr[0], hdr[1]);
    if id != magic ^ 0xFF || id != GDEFLATE_ID {
        return Err(GDeflateError::BadHeader);
    }
    let num_tiles = usize::from(u16::from_le_bytes([hdr[2], hdr[3]]));
    let word = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    let tile_size_idx = word & 3;
    let last = ((word >> 2) & 0x3_FFFF) as usize;
    if tile_size_idx != 1 || last >= TILE_SIZE {
        return Err(GDeflateError::BadHeader);
    }
    if num_tiles == 0 && last != 0 {
        return Err(GDeflateError::BadHeader);
    }
    let total = if num_tiles == 0 {
        0
    } else if last == 0 {
        num_tiles * TILE_SIZE
    } else {
        (num_tiles - 1) * TILE_SIZE + last
    };
    let table_end = num_tiles
        .checked_mul(4)
        .and_then(|n| n.checked_add(HEADER_LEN))
        .ok_or(GDeflateError::Truncated)?;
    let table = src
        .get(HEADER_LEN..table_end)
        .ok_or(GDeflateError::Truncated)?;
    let data = src.get(table_end..).ok_or(GDeflateError::Truncated)?;
    Ok(Parsed {
        num_tiles,
        total,
        table,
        data,
    })
}

impl Parsed<'_> {
    /// Octets compressés de la tuile `i`.
    fn tile(&self, i: usize) -> Result<&[u8], GDeflateError> {
        let entry = |k: usize| read_u32(self.table, k * 4).map(|v| v as usize);
        let bad = GDeflateError::BadTileTable;
        let start = if i == 0 { 0 } else { entry(i).ok_or(bad)? };
        let end = if i + 1 < self.num_tiles {
            entry(i + 1).ok_or(bad)?
        } else {
            start
                .checked_add(entry(0).ok_or(bad)?)
                .ok_or(GDeflateError::BadTileTable)?
        };
        if end < start {
            return Err(bad);
        }
        self.data.get(start..end).ok_or(bad)
    }
}

/// Taille décompressée annoncée par l'en-tête du flux (`None` si l'en-tête est invalide).
pub fn gdeflate_uncompressed_size(src: &[u8]) -> Option<usize> {
    parse(src).ok().map(|p| p.total)
}

// ---------------------------------------------------------------------------------------------
// Huffman canonique
// ---------------------------------------------------------------------------------------------

struct Huffman {
    /// `(symbole << 4) | longueur` pour les codes ≤ `FAST_BITS` ; 0 = non résolu en table rapide.
    fast: [u16; 1 << FAST_BITS],
    counts: [u16; MAX_CODE_LEN + 1],
    /// Symboles triés par (longueur, valeur).
    symbols: [u16; 288],
}

impl Huffman {
    #[allow(clippy::needless_range_loop)]
    fn build(lens: &[u8]) -> Result<Self, GDeflateError> {
        let bad = GDeflateError::InvalidHuffmanCode;
        if lens.len() > 288 {
            return Err(bad);
        }
        let mut counts = [0u16; MAX_CODE_LEN + 1];
        for &l in lens {
            let c = counts.get_mut(usize::from(l)).ok_or(bad)?;
            *c += 1;
        }
        counts[0] = 0;
        // Espace de codes : refus du sur-souscrit ; incomplet accepté seulement s'il est vide ou
        // réduit à un unique code de longueur 1 (règle de libdeflate).
        let mut left: i32 = 1;
        let mut used = 0u32;
        for len in 1..=MAX_CODE_LEN {
            left <<= 1;
            left -= i32::from(counts[len]);
            if left < 0 {
                return Err(bad);
            }
            used += u32::from(counts[len]);
        }
        if left > 0 && !(used == 0 || (used == 1 && counts[1] == 1)) {
            return Err(bad);
        }
        let mut offs = [0u16; MAX_CODE_LEN + 2];
        for len in 1..=MAX_CODE_LEN {
            offs[len + 1] = offs[len] + counts[len];
        }
        let mut symbols = [0u16; 288];
        for (sym, &l) in lens.iter().enumerate() {
            if l != 0 {
                let o = &mut offs[usize::from(l)];
                if let Some(slot) = symbols.get_mut(usize::from(*o)) {
                    *slot = sym as u16;
                }
                *o += 1;
            }
        }
        // Table rapide : codes canoniques inversés (premier bit du code = bit de poids faible).
        let mut fast = [0u16; 1 << FAST_BITS];
        let mut code: u32 = 0;
        let mut index = 0usize;
        for len in 1..=FAST_BITS as usize {
            for _ in 0..counts[len] {
                let sym = symbols.get(index).copied().unwrap_or(0);
                index += 1;
                let rev = code.reverse_bits() >> (32 - len as u32);
                let mut k = rev as usize;
                while k < fast.len() {
                    fast[k] = (sym << 4) | len as u16;
                    k += 1 << len;
                }
                code += 1;
            }
            code <<= 1;
        }
        Ok(Self {
            fast,
            counts,
            symbols,
        })
    }

    /// Décode un symbole à partir de `peek` (≥ 15 bits valides, premier bit en poids faible).
    fn decode(&self, peek: u32) -> Option<(u16, u32)> {
        let e = self.fast[(peek & ((1 << FAST_BITS) - 1)) as usize];
        if e != 0 {
            return Some((e >> 4, u32::from(e & 15)));
        }
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..=MAX_CODE_LEN {
            code |= ((peek >> (len - 1)) & 1) as i32;
            let count = i32::from(self.counts[len]);
            if code - count < first {
                let sym = self.symbols.get((index + (code - first)) as usize)?;
                return Some((*sym, len as u32));
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Lecteur de 32 sous-flux
// ---------------------------------------------------------------------------------------------

struct Streams<'a> {
    input: &'a [u8],
    pos: usize,
    buf: [u64; NUM_STREAMS],
    left: [u32; NUM_STREAMS],
    idx: usize,
}

impl<'a> Streams<'a> {
    fn new(input: &'a [u8]) -> Self {
        let mut s = Self {
            input,
            pos: 0,
            buf: [0; NUM_STREAMS],
            left: [0; NUM_STREAMS],
            idx: 0,
        };
        for n in 0..NUM_STREAMS {
            s.idx = n;
            s.ensure();
        }
        s.idx = 0;
        s
    }

    /// Recharge 32 bits dans le sous-flux courant s'il en contient moins de 32.
    fn ensure(&mut self) {
        let i = self.idx;
        if self.left[i] < 32 {
            let mut w = [0u8; 4];
            if let Some(src) = self.input.get(self.pos..) {
                let n = src.len().min(4);
                w[..n].copy_from_slice(&src[..n]);
            }
            self.pos = self.pos.saturating_add(4);
            self.buf[i] |= u64::from(u32::from_le_bytes(w)) << self.left[i];
            self.left[i] += 32;
        }
    }

    fn peek(&self, n: u32) -> u32 {
        (self.buf[self.idx] as u32) & (((1u64 << n) - 1) as u32)
    }

    fn remove(&mut self, n: u32) {
        let i = self.idx;
        self.buf[i] >>= n;
        self.left[i] = self.left[i].saturating_sub(n);
    }

    fn pop(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.remove(n);
        v
    }

    /// Passe au sous-flux suivant après avoir rechargé le courant.
    fn advance(&mut self) {
        self.ensure();
        self.idx = (self.idx + 1) % NUM_STREAMS;
    }

    fn overrun(&self) -> bool {
        self.pos > self.input.len().saturating_add(OVERREAD_SLACK)
    }
}

// ---------------------------------------------------------------------------------------------
// Décodage d'une tuile
// ---------------------------------------------------------------------------------------------

/// Longueur de base et bits d'extension des symboles 257..=287 (Deflate64).
const LEN_BASE: [u16; 31] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 3, 3, 3,
];
const LEN_EXTRA: [u8; 31] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 16, 16, 16,
];
const DIST_BASE: [u16; 32] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577, 32769, 49153,
];
const DIST_EXTRA: [u8; 32] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13, 14, 14,
];
/// Ordre de stockage des longueurs du précode.
const PRECODE_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

fn static_codes() -> Result<(Huffman, Huffman), GDeflateError> {
    let mut lens = [0u8; 288];
    for (i, l) in lens.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    Ok((Huffman::build(&lens)?, Huffman::build(&[5u8; 32])?))
}

/// Exécute la copie différée `(len, dst)` : décode la distance dans le sous-flux courant.
fn do_copy(
    st: &mut Streams<'_>,
    dist: &Huffman,
    out: &mut [u8],
    len: usize,
    dst: usize,
) -> Result<(), GDeflateError> {
    let (sym, l) = dist
        .decode(st.peek(15))
        .ok_or(GDeflateError::InvalidSymbol)?;
    st.remove(l);
    let s = usize::from(sym);
    let base = *DIST_BASE.get(s).ok_or(GDeflateError::InvalidSymbol)?;
    let extra = *DIST_EXTRA.get(s).ok_or(GDeflateError::InvalidSymbol)?;
    let offset = usize::from(base) + st.pop(u32::from(extra)) as usize;
    if offset > dst {
        return Err(GDeflateError::DistanceTooFar);
    }
    let end = dst.checked_add(len).ok_or(GDeflateError::OutputOverrun)?;
    if end > out.len() {
        return Err(GDeflateError::OutputOverrun);
    }
    let src = dst - offset;
    if offset >= len {
        out.copy_within(src..src + len, dst);
    } else {
        for k in 0..len {
            out[dst + k] = out[src + k];
        }
    }
    Ok(())
}

/// Décompresse une tuile ; `out` doit être rempli exactement.
fn decode_tile(input: &[u8], out: &mut [u8]) -> Result<(), GDeflateError> {
    let mut st = Streams::new(input);
    let mut op = 0usize;
    let mut statics: Option<(Huffman, Huffman)> = None;
    let mut pending: [Option<(usize, usize)>; NUM_STREAMS] = [None; NUM_STREAMS];
    loop {
        if st.overrun() {
            return Err(GDeflateError::Truncated);
        }
        st.idx = 0;
        let is_final = st.pop(1) == 1;
        let btype = st.pop(2);
        st.ensure();
        let dynamic;
        let tables: &(Huffman, Huffman) = match btype {
            0 => {
                // Bloc stocké : LEN (16 bits, sous-flux 0) puis un octet par sous-flux.
                let len = st.pop(16) as usize;
                if len > out.len() - op {
                    return Err(GDeflateError::OutputOverrun);
                }
                for _ in 0..len {
                    out[op] = st.pop(8) as u8;
                    op += 1;
                    st.advance();
                }
                if is_final {
                    break;
                }
                continue;
            }
            1 => {
                if statics.is_none() {
                    statics = Some(static_codes()?);
                }
                statics.as_ref().ok_or(GDeflateError::InvalidHuffmanCode)?
            }
            2 => {
                dynamic = read_dynamic(&mut st)?;
                &dynamic
            }
            _ => return Err(GDeflateError::InvalidBlockType),
        };
        let (litlen, dist) = tables;

        st.idx = 0;
        // Boucle principale : un symbole (ou une distance) par tour et par sous-flux.
        loop {
            let i = st.idx;
            if let Some((len, dst)) = pending[i].take() {
                do_copy(&mut st, dist, out, len, dst)?;
            } else {
                let (sym, l) = litlen
                    .decode(st.peek(15))
                    .ok_or(GDeflateError::InvalidSymbol)?;
                st.remove(l);
                if sym < 256 {
                    *out.get_mut(op).ok_or(GDeflateError::OutputOverrun)? = sym as u8;
                    op += 1;
                } else if sym == 256 {
                    break;
                } else {
                    let k = usize::from(sym) - 257;
                    let base = *LEN_BASE.get(k).ok_or(GDeflateError::InvalidSymbol)?;
                    let extra = *LEN_EXTRA.get(k).ok_or(GDeflateError::InvalidSymbol)?;
                    let len = usize::from(base) + st.pop(u32::from(extra)) as usize;
                    if len > out.len() - op {
                        return Err(GDeflateError::OutputOverrun);
                    }
                    pending[i] = Some((len, op));
                    op += len;
                }
            }
            st.advance();
        }
        // Fin de bloc : copies en attente, dans l'ordre des sous-flux à partir du courant.
        for _ in 0..NUM_STREAMS {
            if let Some((len, dst)) = pending[st.idx].take() {
                do_copy(&mut st, dist, out, len, dst)?;
            }
            st.advance();
        }
        if is_final {
            break;
        }
    }
    if op == out.len() {
        Ok(())
    } else {
        Err(GDeflateError::ShortOutput)
    }
}

/// Lit l'en-tête d'un bloc dynamique (14 bits + précode + longueurs) et construit les codes.
fn read_dynamic(st: &mut Streams<'_>) -> Result<(Huffman, Huffman), GDeflateError> {
    let nlit = st.pop(5) as usize + 257;
    let ndist = st.pop(5) as usize + 1;
    let nclen = st.pop(4) as usize + 4;
    st.ensure();
    let mut pre = [0u8; 19];
    for &slot in PRECODE_ORDER.iter().take(nclen) {
        pre[slot] = st.pop(3) as u8;
        st.advance();
    }
    let precode = Huffman::build(&pre)?;
    st.idx = 0;
    let total = nlit + ndist;
    let mut lens = [0u8; 288 + 32];
    let mut i = 0usize;
    while i < total {
        let (sym, l) = precode
            .decode(st.peek(15))
            .ok_or(GDeflateError::InvalidSymbol)?;
        st.remove(l);
        if sym < 16 {
            lens[i] = sym as u8;
            i += 1;
        } else {
            let (val, rep) = match sym {
                16 => {
                    if i == 0 {
                        return Err(GDeflateError::InvalidSymbol);
                    }
                    (lens[i - 1], 3 + st.pop(2) as usize)
                }
                17 => (0, 3 + st.pop(3) as usize),
                _ => (0, 11 + st.pop(7) as usize),
            };
            let n = rep.min(total - i);
            lens[i..i + n].fill(val);
            i += n;
        }
        st.advance();
    }
    let lit = Huffman::build(&lens[..nlit])?;
    let dist = Huffman::build(&lens[nlit..total])?;
    Ok((lit, dist))
}

// ---------------------------------------------------------------------------------------------
// API publique
// ---------------------------------------------------------------------------------------------

/// Décompresse un flux GDeflate (en-tête `TileStream` + tuiles) dans `dst` et renvoie le nombre
/// d'octets écrits (la taille décompressée annoncée). `dst` doit contenir au moins cette taille.
///
/// Les tuiles sont décompressées en parallèle (`std::thread::scope`) pour les entrées volumineuses.
pub fn gdeflate_decompress(src: &[u8], dst: &mut [u8]) -> Result<usize, GDeflateError> {
    let p = parse(src)?;
    if dst.len() < p.total {
        return Err(GDeflateError::OutputTooSmall);
    }
    // Validation de la table avant tout travail (garde aussi l'ordre déterministe des erreurs).
    for i in 0..p.num_tiles {
        p.tile(i)?;
    }
    let out = &mut dst[..p.total];
    let workers = if p.num_tiles >= 8 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(8)
            .min(p.num_tiles / 2)
    } else {
        1
    };
    if workers <= 1 {
        decode_range(&p, 0, out)?;
        return Ok(p.total);
    }
    let per = p.num_tiles.div_ceil(workers);
    let results: Vec<Result<(), GDeflateError>> = std::thread::scope(|s| {
        let mut handles = Vec::new();
        let mut rest: &mut [u8] = out;
        let mut first = 0usize;
        while first < p.num_tiles {
            let tiles = per.min(p.num_tiles - first);
            let bytes = (tiles * TILE_SIZE).min(rest.len());
            let (chunk, tail) = std::mem::take(&mut rest).split_at_mut(bytes);
            rest = tail;
            let pr = &p;
            let f = first;
            handles.push(s.spawn(move || decode_range(pr, f, chunk)));
            first += tiles;
        }
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or(Err(GDeflateError::InvalidSymbol)))
            .collect()
    });
    results.into_iter().collect::<Result<(), _>>()?;
    Ok(p.total)
}

/// Décompresse les tuiles `first..` dans `out` (découpé en tuiles de 64 Kio).
fn decode_range(p: &Parsed<'_>, first: usize, out: &mut [u8]) -> Result<(), GDeflateError> {
    for (k, chunk) in out.chunks_mut(TILE_SIZE).enumerate() {
        decode_tile(p.tile(first + k)?, chunk)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Décompresseur standard RFC 1950 (Zlib) / RFC 1951 (DEFLATE) pour DirectStorage Xbox Series X
// ---------------------------------------------------------------------------------------------

const RFC1951_LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const RFC1951_LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

struct BitReader<'a> {
    input: &'a [u8],
    pos: usize,
    buf: u64,
    bits: u32,
}

impl<'a> BitReader<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            pos: 0,
            buf: 0,
            bits: 0,
        }
    }

    #[inline]
    fn fill(&mut self) {
        while self.bits <= 56 && self.pos < self.input.len() {
            self.buf |= u64::from(self.input[self.pos]) << self.bits;
            self.pos += 1;
            self.bits += 8;
        }
    }

    #[inline]
    fn read_bits(&mut self, n: u32) -> Result<u32, GDeflateError> {
        if n == 0 {
            return Ok(0);
        }
        self.fill();
        if self.bits < n {
            return Err(GDeflateError::Truncated);
        }
        let v = (self.buf & ((1u64 << n) - 1)) as u32;
        self.buf >>= n;
        self.bits -= n;
        Ok(v)
    }

    #[inline]
    fn decode_huff(&mut self, table: &Huffman) -> Result<u16, GDeflateError> {
        self.fill();
        let (sym, len) = table
            .decode(self.buf as u32)
            .ok_or(GDeflateError::InvalidSymbol)?;
        if self.bits < len {
            return Err(GDeflateError::Truncated);
        }
        self.buf >>= len;
        self.bits -= len;
        Ok(sym)
    }

    fn align_byte(&mut self) {
        let drop = self.bits & 7;
        self.buf >>= drop;
        self.bits -= drop;
    }

    fn read_aligned_u16(&mut self) -> Result<u16, GDeflateError> {
        self.align_byte();
        let lo = self.read_bits(8)? as u16;
        let hi = self.read_bits(8)? as u16;
        Ok(lo | (hi << 8))
    }

    fn read_aligned_bytes(&mut self, out: &mut [u8]) -> Result<(), GDeflateError> {
        self.align_byte();
        let mut i = 0;
        while i < out.len() && self.bits >= 8 {
            out[i] = (self.buf & 0xFF) as u8;
            self.buf >>= 8;
            self.bits -= 8;
            i += 1;
        }
        let rem = out.len() - i;
        if rem > 0 {
            let src = self
                .input
                .get(self.pos..self.pos + rem)
                .ok_or(GDeflateError::Truncated)?;
            out[i..].copy_from_slice(src);
            self.pos += rem;
        }
        Ok(())
    }
}

fn decode_rfc1951_dynamic(r: &mut BitReader<'_>) -> Result<(Huffman, Huffman), GDeflateError> {
    let nlit = r.read_bits(5)? as usize + 257;
    let ndist = r.read_bits(5)? as usize + 1;
    let nclen = r.read_bits(4)? as usize + 4;
    if nlit > 286 || ndist > 30 {
        return Err(GDeflateError::InvalidHuffmanCode);
    }
    let mut clens = [0u8; 19];
    for &idx in PRECODE_ORDER.iter().take(nclen) {
        clens[idx] = r.read_bits(3)? as u8;
    }
    let pre = Huffman::build(&clens)?;
    let total = nlit + ndist;
    let mut lens = [0u8; 286 + 32];
    let mut i = 0usize;
    while i < total {
        let sym = r.decode_huff(&pre)?;
        match sym {
            0..=15 => {
                lens[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(GDeflateError::InvalidHuffmanCode);
                }
                let rep = r.read_bits(2)? as usize + 3;
                let prev = lens[i - 1];
                if i + rep > total {
                    return Err(GDeflateError::InvalidHuffmanCode);
                }
                lens[i..i + rep].fill(prev);
                i += rep;
            }
            17 => {
                let rep = r.read_bits(3)? as usize + 3;
                if i + rep > total {
                    return Err(GDeflateError::InvalidHuffmanCode);
                }
                lens[i..i + rep].fill(0);
                i += rep;
            }
            18 => {
                let rep = r.read_bits(7)? as usize + 11;
                if i + rep > total {
                    return Err(GDeflateError::InvalidHuffmanCode);
                }
                lens[i..i + rep].fill(0);
                i += rep;
            }
            _ => return Err(GDeflateError::InvalidHuffmanCode),
        }
    }
    let lit = Huffman::build(&lens[..nlit])?;
    let dist = Huffman::build(&lens[nlit..total])?;
    Ok((lit, dist))
}

/// Décompresse un flux DEFLATE brut (RFC 1951) dans `dst` et renvoie le nombre d'octets produits.
pub fn inflate_raw(src: &[u8], dst: &mut [u8]) -> Result<usize, GDeflateError> {
    let mut r = BitReader::new(src);
    let mut out_pos = 0usize;
    loop {
        let bfinal = r.read_bits(1)?;
        let btype = r.read_bits(2)?;
        match btype {
            0 => {
                let len = usize::from(r.read_aligned_u16()?);
                let nlen = r.read_aligned_u16()?;
                if (len as u16) ^ nlen != 0xFFFF {
                    return Err(GDeflateError::BadHeader);
                }
                if out_pos + len > dst.len() {
                    return Err(GDeflateError::OutputOverrun);
                }
                r.read_aligned_bytes(&mut dst[out_pos..out_pos + len])?;
                out_pos += len;
            }
            1 | 2 => {
                let (lit, dist) = if btype == 1 {
                    static_codes()?
                } else {
                    decode_rfc1951_dynamic(&mut r)?
                };
                loop {
                    let sym = r.decode_huff(&lit)?;
                    if sym < 256 {
                        if out_pos >= dst.len() {
                            return Err(GDeflateError::OutputOverrun);
                        }
                        dst[out_pos] = sym as u8;
                        out_pos += 1;
                    } else if sym == 256 {
                        break;
                    } else {
                        let lidx = usize::from(sym - 257);
                        if lidx >= RFC1951_LEN_BASE.len() {
                            return Err(GDeflateError::InvalidSymbol);
                        }
                        let extra_l = u32::from(RFC1951_LEN_EXTRA[lidx]);
                        let length =
                            usize::from(RFC1951_LEN_BASE[lidx]) + r.read_bits(extra_l)? as usize;
                        let dsym = usize::from(r.decode_huff(&dist)?);
                        if dsym >= 30 {
                            return Err(GDeflateError::InvalidSymbol);
                        }
                        let extra_d = u32::from(DIST_EXTRA[dsym]);
                        let distance =
                            usize::from(DIST_BASE[dsym]) + r.read_bits(extra_d)? as usize;
                        if distance == 0 || distance > out_pos {
                            return Err(GDeflateError::DistanceTooFar);
                        }
                        if out_pos + length > dst.len() {
                            return Err(GDeflateError::OutputOverrun);
                        }
                        for _ in 0..length {
                            dst[out_pos] = dst[out_pos - distance];
                            out_pos += 1;
                        }
                    }
                }
            }
            _ => return Err(GDeflateError::InvalidBlockType),
        }
        if bfinal != 0 {
            break;
        }
    }
    Ok(out_pos)
}

/// Décompresse un flux **Zlib (RFC 1950)** (DirectStorage console `ZlibDecompress = 1`) ou un flux
/// DEFLATE brut (RFC 1951) en repli si aucun en-tête Zlib valide n'est présent.
pub fn zlib_decompress(src: &[u8], dst: &mut [u8]) -> Result<usize, GDeflateError> {
    if src.len() >= 2 {
        let cmf = src[0];
        let flg = src[1];
        let cm = cmf & 0x0F;
        let cinfo = cmf >> 4;
        let hdr = (u16::from(cmf) << 8) | u16::from(flg);
        if cm == 8 && cinfo <= 7 && hdr % 31 == 0 {
            let start = if flg & 0x20 != 0 { 6 } else { 2 };
            if start > src.len() {
                return Err(GDeflateError::Truncated);
            }
            return inflate_raw(&src[start..], dst);
        }
    }
    inflate_raw(src, dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Générateurs déterministes identiques à ceux qui ont produit les vecteurs de référence
    /// (flux compressés par l'implémentation C++ de Microsoft/NVIDIA, compilée hors du dépôt).
    fn sample(kind: &str, n: usize) -> Vec<u8> {
        let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut rnd = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let mut v = vec![0u8; n];
        match kind {
            "zeros" => {}
            "rep" => {
                for (i, b) in v.iter_mut().enumerate() {
                    *b = b"ABCDEFGH"[i % 8];
                }
            }
            "rand" => {
                for b in v.iter_mut() {
                    *b = (rnd() >> 24) as u8;
                }
            }
            "text" => {
                let w: [&[u8]; 11] = [
                    b"the ",
                    b"quick ",
                    b"brown ",
                    b"fox ",
                    b"jumps ",
                    b"over ",
                    b"lazy ",
                    b"dog ",
                    b"scarlet ",
                    b"bridge ",
                    b"\n",
                ];
                let mut i = 0;
                while i < n {
                    for &c in w[((rnd() >> 20) % 11) as usize] {
                        if i < n {
                            v[i] = c;
                            i += 1;
                        }
                    }
                }
            }
            "struct" => {
                for (i, b) in v.iter_mut().enumerate() {
                    let (r, c) = ((i / 16) as u64, i % 16);
                    *b = if c < 4 {
                        (r >> (8 * c)) as u8
                    } else if c < 8 {
                        ((r * 3) >> (8 * (c - 4))) as u8
                    } else if c < 12 {
                        0
                    } else {
                        (rnd() >> 60) as u8
                    };
                }
            }
            "far" => {
                for i in 0..n {
                    v[i] = if i < 40000 {
                        (rnd() >> 24) as u8
                    } else {
                        v[i - 40000]
                    };
                }
            }
            "mix" => {
                for (i, b) in v.iter_mut().enumerate() {
                    *b = if (i / 3000) % 2 == 1 {
                        (rnd() >> 24) as u8
                    } else {
                        b"abcabcxyz"[i % 9]
                    };
                }
            }
            _ => unreachable!(),
        }
        v
    }

    macro_rules! vec_file {
        ($n:literal) => {
            include_bytes!(concat!("../tests/data/gdeflate/", $n, ".gdf")) as &[u8]
        };
    }

    /// (nom, flux GDeflate de référence, données brutes attendues)
    fn vectors() -> Vec<(&'static str, &'static [u8], Vec<u8>)> {
        vec![
            ("one", vec_file!("one"), sample("rand", 1)),
            ("small3", vec_file!("small3"), sample("zeros", 3)),
            ("hundred", vec_file!("hundred"), sample("text", 100)),
            ("tile", vec_file!("tile"), sample("text", 65536)),
            ("tile1", vec_file!("tile1"), sample("text", 65537)),
            ("tile2x", vec_file!("tile2x"), sample("text", 131072)),
            ("rep1m", vec_file!("rep1m"), sample("rep", 1 << 20)),
            ("zeros1m", vec_file!("zeros1m"), sample("zeros", 1 << 20)),
            ("rand70k", vec_file!("rand70k"), sample("rand", 70000)),
            ("text300k", vec_file!("text300k"), sample("text", 300_000)),
            ("text100k", vec_file!("text100k"), sample("text", 100_000)),
            (
                "struct100k",
                vec_file!("struct100k"),
                sample("struct", 100_000),
            ),
            ("far70k", vec_file!("far70k"), sample("far", 70000)),
            ("mix100k", vec_file!("mix100k"), sample("mix", 100_000)),
        ]
    }

    #[test]
    fn reference_vectors_roundtrip_byte_exact() {
        for (name, comp, raw) in vectors() {
            assert_eq!(gdeflate_uncompressed_size(comp), Some(raw.len()), "{name}");
            let mut out = vec![0u8; raw.len()];
            let n = gdeflate_decompress(comp, &mut out).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(n, raw.len(), "{name}");
            assert!(out == raw, "{name}: contenu différent");
        }
    }

    /// Flux de 1 Mio pseudo-aléatoire incompressible : 16 tuiles stockées identiques (la tuile 0 du
    /// vecteur `rand70k`, produite par le compresseur de référence).
    fn random_1mib_stream() -> (Vec<u8>, Vec<u8>) {
        let comp = vec_file!("rand70k");
        let p = parse(comp).expect("en-tête");
        let tile0 = p.tile(0).expect("tuile 0").to_vec();
        let n = 16usize;
        let mut s = vec![GDEFLATE_ID, GDEFLATE_ID ^ 0xFF];
        s.extend_from_slice(&(n as u16).to_le_bytes());
        s.extend_from_slice(&1u32.to_le_bytes());
        let table0 = tile0.len() as u32;
        s.extend_from_slice(&table0.to_le_bytes());
        for i in 1..n {
            s.extend_from_slice(&((i * tile0.len()) as u32).to_le_bytes());
        }
        for _ in 0..n {
            s.extend_from_slice(&tile0);
        }
        let raw: Vec<u8> = sample("rand", TILE_SIZE).repeat(n);
        (s, raw)
    }

    #[test]
    fn random_1mib_multi_tile_parallel() {
        let (s, raw) = random_1mib_stream();
        let mut out = vec![0u8; raw.len()];
        assert_eq!(gdeflate_decompress(&s, &mut out), Ok(1 << 20));
        assert!(out == raw);
    }

    #[test]
    fn empty_stream_zero_tiles() {
        let s = [GDEFLATE_ID, GDEFLATE_ID ^ 0xFF, 0, 0, 1, 0, 0, 0];
        assert_eq!(gdeflate_uncompressed_size(&s), Some(0));
        assert_eq!(gdeflate_decompress(&s, &mut []), Ok(0));
    }

    #[test]
    fn rejects_bad_headers_and_small_outputs() {
        assert_eq!(
            gdeflate_decompress(&[], &mut []),
            Err(GDeflateError::Truncated)
        );
        let mut bad = [GDEFLATE_ID, 0x00, 0, 0, 1, 0, 0, 0];
        assert_eq!(
            gdeflate_decompress(&bad, &mut []),
            Err(GDeflateError::BadHeader)
        );
        bad[1] = GDEFLATE_ID ^ 0xFF;
        bad[4] = 2; // tileSizeIdx != 1
        assert_eq!(
            gdeflate_decompress(&bad, &mut []),
            Err(GDeflateError::BadHeader)
        );
        let comp = vec_file!("hundred");
        let mut small = [0u8; 99];
        assert_eq!(
            gdeflate_decompress(comp, &mut small),
            Err(GDeflateError::OutputTooSmall)
        );
        // Table annonçant plus de tuiles que de données.
        let huge = [GDEFLATE_ID, GDEFLATE_ID ^ 0xFF, 0xFF, 0xFF, 1, 0, 0, 0];
        assert_eq!(
            gdeflate_decompress(&huge, &mut []),
            Err(GDeflateError::Truncated)
        );
        assert_eq!(gdeflate_uncompressed_size(&[1, 2, 3]), None);
    }

    /// Fuzz déterministe : bit-flips et troncatures ne doivent jamais paniquer.
    #[test]
    fn fuzz_bitflips_and_truncations_never_panic() {
        let mut state: u64 = 0xDEAD_BEEF_CAFE_F00D;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut all = vectors()
            .into_iter()
            .map(|(n, c, r)| (n, c.to_vec(), r.len()))
            .collect::<Vec<_>>();
        let (s, raw) = random_1mib_stream();
        all.push(("rand1m", s, raw.len()));
        for (name, comp, rawlen) in all {
            let big = rawlen > 300_000;
            let mut out = vec![0u8; rawlen];
            let flips = if big { 24 } else { 150 };
            for _ in 0..flips {
                let mut m = comp.clone();
                let nflip = 1 + (next() % 3) as usize;
                for _ in 0..nflip {
                    let bit = (next() % (m.len() as u64 * 8)) as usize;
                    m[bit / 8] ^= 1 << (bit % 8);
                }
                let _ = gdeflate_decompress(&m, &mut out);
            }
            let cuts = if big { 12 } else { 60 };
            for k in 0..cuts {
                let len = comp.len() * k / cuts;
                let _ = gdeflate_decompress(&comp[..len], &mut out);
            }
            // Troncature d'un octet seulement + en-tête fantaisiste.
            let _ = gdeflate_decompress(&comp[..comp.len() - 1], &mut out);
            let mut m = comp.clone();
            for b in m.iter_mut().skip(8) {
                *b = (next() >> 32) as u8;
            }
            let _ = gdeflate_decompress(&m, &mut out);
            assert!(!name.is_empty());
        }
    }

    #[test]
    fn corrupted_stream_is_detected_or_differs() {
        // Un flux de zéros après l'en-tête ne peut pas produire la sortie attendue sans erreur
        // infinie : la lecture au-delà de la fin doit être bornée.
        let comp = vec_file!("tile");
        let mut m = comp[..8 + 4].to_vec();
        m[2] = 1;
        m[3] = 0;
        m[8..12].copy_from_slice(&64u32.to_le_bytes());
        m.extend(std::iter::repeat_n(0u8, 64));
        let mut out = vec![0u8; TILE_SIZE];
        assert!(gdeflate_decompress(&m, &mut out).is_err());
    }

    #[test]
    fn zlib_decompress_rfc1950_stored_fixed_and_dynamic() {
        // 1. Bloc stocké RFC 1950 (0x78 0x01 + BFINAL=1 BTYPE=00 + LEN=5 NLEN=0xFFFA + "hello")
        let stored = [
            0x78u8, 0x01, 0x01, 0x05, 0x00, 0xFA, 0xFF, b'h', b'e', b'l', b'l', b'o',
        ];
        let mut out = [0u8; 16];
        assert_eq!(zlib_decompress(&stored, &mut out), Ok(5));
        assert_eq!(&out[..5], b"hello");

        // 2. Flux RFC 1950 compressé en Huffman fixe (BTYPE=1) : "ScarletBridge" x 4
        let fixed = [
            120u8, 156, 11, 78, 78, 44, 202, 73, 45, 113, 42, 202, 76, 73, 79, 13, 38, 142, 3, 0,
            28, 24, 20, 109,
        ];
        let expected = b"ScarletBridgeScarletBridgeScarletBridgeScarletBridge";
        let mut out2 = vec![0u8; expected.len()];
        assert_eq!(zlib_decompress(&fixed, &mut out2), Ok(expected.len()));
        assert_eq!(out2.as_slice(), expected);

        // 3. Flux RFC 1950 compressé en Huffman dynamique (BTYPE=2, 300 octets)
        let dynamic = [
            120u8, 218, 61, 206, 135, 13, 192, 48, 12, 3, 193, 217, 228, 222, 163, 253, 167, 137,
            45, 18, 250, 1, 14, 47, 34, 225, 22, 95, 201, 202, 168, 148, 82, 111, 237, 213, 173,
            129, 230, 156, 235, 182, 95, 199, 250, 144, 170, 138, 128, 131, 71, 48, 65, 51, 14, 30,
            193, 14, 205, 56, 120, 4, 15, 52, 227, 224, 17, 140, 252, 43, 238, 17, 108, 252, 155,
            238, 17, 220, 252, 83, 247, 8, 6, 254, 101, 247, 8, 86, 254, 13, 247, 8, 174, 31, 124,
            3, 84, 119,
        ];
        let mut expected_dyn = Vec::with_capacity(300);
        for i in 0..60usize {
            expected_dyn.extend(std::iter::repeat_n(65u8 + (i % 16) as u8, 3 + (i % 5)));
        }
        let mut out3 = vec![0u8; expected_dyn.len()];
        assert_eq!(zlib_decompress(&dynamic, &mut out3), Ok(expected_dyn.len()));
        assert_eq!(out3, expected_dyn);
    }
}
