//! Tuilage / détuilage 2D **AMD GFX10 / RDNA 2 (Xbox Series X)** :
//! - `Sw4KbS` (`GFX10_SW_4K_S_RBPLUS`, tuiles 4 KiB)
//! - `Sw64KbD` (`GFX10_SW_64K_D_RBPLUS`, tuiles 64 KiB)
//!
//! Prend en charge les éléments de 1, 2, 4, 8 et 16 octets (y compris les blocs compressés
//! BC1–BC7 où 1 élément = 1 bloc 4×4 de 8 ou 16 octets). Tables d'entrelacement issues de
//! Mesa `addrlib` (MIT).

#![allow(dead_code)]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileMode {
    Sw4KbS,
    Sw64KbD,
}

/// Patterns Mesa addrlib `GFX10_SW_4K_S_RBPLUS_PATINFO` (bits 0..12).
/// `> 0` = bit X (1-based), `< 0` = bit Y (1-based), `0` = bit intra-élément.
const PATTERNS_4K: [[i8; 12]; 5] = [
    [1, 2, 3, 4, -1, -2, -3, -4, -5, 5, -6, 6],
    [0, 1, 2, 3, -1, -2, -3, 4, -4, 5, -5, 6],
    [0, 0, 1, 2, -1, -2, -3, 3, -4, 4, -5, 5],
    [0, 0, 0, 1, -1, -2, 2, 3, -3, 4, -4, 5],
    [0, 0, 0, 0, -1, -2, 1, 2, -3, 3, -4, 4],
];

/// Patterns Mesa addrlib `GFX10_SW_64K_D_RBPLUS_PATINFO` (bits 0..16).
const PATTERNS_64K: [[i8; 16]; 5] = [
    [1, 2, 3, -2, -1, -3, 4, -4, -5, 5, -6, 6, -7, 7, -8, 8],
    [0, 1, 2, 3, -1, -2, -3, 4, -4, 5, -5, 6, -6, 7, -7, 8],
    [0, 0, 1, 2, -1, -2, 3, -3, -4, 4, -5, 5, -6, 6, -7, 7],
    [0, 0, 0, 1, -1, 2, 3, -2, -3, 4, -4, 5, -5, 6, -6, 7],
    [0, 0, 0, 0, 1, -1, 2, -2, -3, 3, -4, 4, -5, 5, -6, 6],
];

/// Surface 2D tuilée GFX10 (1 niveau de mip autonome, sans mip tail ni XOR de tuile).
#[derive(Debug, Clone, Copy)]
pub struct TiledSurface2D {
    pattern: &'static [i8],
    bits: u32,
    width: usize,
    height: usize,
    element_bytes: usize,
    tile_width: usize,
    tile_height: usize,
    tiles_x: usize,
    tiled_size: usize,
}

impl TiledSurface2D {
    pub fn new(mode: TileMode, element_bytes: usize, width: usize, height: usize) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        let idx = match element_bytes {
            1 => 0,
            2 => 1,
            4 => 2,
            8 => 3,
            16 => 4,
            _ => return None,
        };
        let (pattern, bits): (&'static [i8], u32) = match mode {
            TileMode::Sw4KbS => (&PATTERNS_4K[idx], 12),
            TileMode::Sw64KbD => (&PATTERNS_64K[idx], 16),
        };
        let mut x_bits = 0u32;
        let mut y_bits = 0u32;
        for &src in pattern {
            if src > 0 {
                x_bits += 1;
            } else if src < 0 {
                y_bits += 1;
            }
        }
        let tile_width = 1usize << x_bits;
        let tile_height = 1usize << y_bits;
        let tiles_x = (width - 1) / tile_width + 1;
        let tiles_y = (height - 1) / tile_height + 1;
        let tile_bytes = 1usize << bits;
        let tiled_size = tiles_x.checked_mul(tiles_y)?.checked_mul(tile_bytes)?;
        Some(Self {
            pattern,
            bits,
            width,
            height,
            element_bytes,
            tile_width,
            tile_height,
            tiles_x,
            tiled_size,
        })
    }

    pub const fn tiled_size(&self) -> usize {
        self.tiled_size
    }

    pub const fn tile_width(&self) -> usize {
        self.tile_width
    }

    pub const fn tile_height(&self) -> usize {
        self.tile_height
    }

    pub fn element_offset(&self, x: usize, y: usize) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some(self.address(x, y))
    }

    fn address(&self, x: usize, y: usize) -> usize {
        let mut offset = ((y / self.tile_height) * self.tiles_x + x / self.tile_width) << self.bits;
        for (bit, &src) in self.pattern.iter().enumerate() {
            if src > 0 {
                offset |= ((x >> (src as u32 - 1)) & 1) << bit;
            } else if src < 0 {
                offset |= ((y >> ((-src) as u32 - 1)) & 1) << bit;
            }
        }
        offset
    }

    /// Copie d'un tampon tuilé vers un tampon linéaire (`row_pitch` >= `width * element_bytes`).
    pub fn detile(&self, tiled: &[u8], linear: &mut [u8], row_pitch: usize) -> bool {
        self.copy(tiled, linear, row_pitch, false)
    }

    /// Copie d'un tampon linéaire vers un tampon tuilé (`row_pitch` >= `width * element_bytes`).
    pub fn tile(&self, linear: &[u8], tiled: &mut [u8], row_pitch: usize) -> bool {
        self.copy(linear, tiled, row_pitch, true)
    }

    fn copy(&self, src: &[u8], dst: &mut [u8], pitch: usize, to_tiled: bool) -> bool {
        let row_bytes = self.width.saturating_mul(self.element_bytes);
        if pitch < row_bytes {
            return false;
        }
        let Some(linear_size) = (self.height - 1)
            .checked_mul(pitch)
            .and_then(|v| v.checked_add(row_bytes))
        else {
            return false;
        };
        let (need_src, need_dst) = if to_tiled {
            (linear_size, self.tiled_size)
        } else {
            (self.tiled_size, linear_size)
        };
        if src.len() < need_src || dst.len() < need_dst {
            return false;
        }
        let tile_x_bits = self.tile_width.trailing_zeros();
        let mut x_offsets = [0usize; 256];
        for (x, slot) in x_offsets.iter_mut().take(self.tile_width).enumerate() {
            let mut off = 0usize;
            for (bit, &src_bit) in self.pattern.iter().enumerate() {
                if src_bit > 0 {
                    off |= ((x >> (src_bit as u32 - 1)) & 1) << bit;
                }
            }
            *slot = off;
        }
        let elem = self.element_bytes;
        let x_mask = self.tile_width - 1;
        for y in 0..self.height {
            let mut y_offset = 0usize;
            for (bit, &src_bit) in self.pattern.iter().enumerate() {
                if src_bit < 0 {
                    y_offset |= ((y >> ((-src_bit) as u32 - 1)) & 1) << bit;
                }
            }
            let tile_row = (y / self.tile_height) * self.tiles_x;
            for x in 0..self.width {
                let lin = y * pitch + x * elem;
                let tld = ((tile_row + (x >> tile_x_bits)) << self.bits)
                    + y_offset
                    + x_offsets[x & x_mask];
                if to_tiled {
                    dst[tld..tld + elem].copy_from_slice(&src[lin..lin + elem]);
                } else {
                    dst[lin..lin + elem].copy_from_slice(&src[tld..tld + elem]);
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sw4k_and_sw64k_tile_detile_roundtrip() {
        for mode in [TileMode::Sw4KbS, TileMode::Sw64KbD] {
            for elem in [1usize, 2, 4, 8, 16] {
                let surf = TiledSurface2D::new(mode, elem, 37, 29).expect("surface valide");
                let pitch = 40 * elem;
                let mut linear = vec![0u8; 29 * pitch];
                for (i, b) in linear.iter_mut().enumerate() {
                    *b = ((i * 31 + 7) & 0xFF) as u8;
                }
                let mut tiled = vec![0u8; surf.tiled_size()];
                assert!(surf.tile(&linear, &mut tiled, pitch));
                let mut out = vec![0u8; linear.len()];
                assert!(surf.detile(&tiled, &mut out, pitch));
                for y in 0..29 {
                    let row = y * pitch;
                    assert_eq!(&out[row..row + 37 * elem], &linear[row..row + 37 * elem]);
                }
                // Vérifie cohérence avec `element_offset`.
                let off = surf.element_offset(13, 17).expect("offset");
                let lin = 17 * pitch + 13 * elem;
                assert_eq!(&tiled[off..off + elem], &linear[lin..lin + elem]);
            }
        }
    }
}
