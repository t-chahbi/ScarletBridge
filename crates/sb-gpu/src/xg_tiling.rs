//! Phase 5 — Fondations de détiling / swizzling matériel AMD RDNA 2 (`XG` Xbox Scarlett).
//!
//! Sur Xbox Series X|S (RDNA 2), les textures en mémoire unifiée utilisent le swizzling 2D
//! (micro-tuiles 8x8 / 256 octets en ordre Morton / Z-order et macro-tuiles 64 KiB `SW_64KB_S`).
//! Ce module fournit l'implémentation de référence en Rust sûr (CPU) avant accélération compute shader.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XgTileMode {
    Linear,
    /// Micro-tuiles Morton 8x8 (mode swizzle standard 2D).
    MicroTile2D,
}

/// Calcule l'index Morton 2D (interleaving des bits de `x` et `y`) dans une micro-tuile 8x8.
#[inline]
pub const fn morton_2d_8x8(x: u32, y: u32) -> u32 {
    let bx = x & 7;
    let by = y & 7;
    ((bx & 1) | ((by & 1) << 1))
        | (((bx & 2) << 1) | ((by & 2) << 2))
        | (((bx & 4) << 2) | ((by & 4) << 3))
}

/// Swizzle une texture linéaire (`width x height`, `bpp` octets/pixel) en disposition micro-tuiles 8x8.
pub fn tile_2d(src: &[u8], width: usize, height: usize, bpp: usize, mode: XgTileMode) -> Vec<u8> {
    if mode == XgTileMode::Linear || bpp == 0 {
        return src.to_vec();
    }
    let tiles_x = width.div_ceil(8);
    let tiles_y = height.div_ceil(8);
    let mut dst = vec![0u8; tiles_x * tiles_y * 64 * bpp];

    for y in 0..height {
        for x in 0..width {
            let tile_idx = (y / 8) * tiles_x + (x / 8);
            let in_tile = morton_2d_8x8((x % 8) as u32, (y % 8) as u32) as usize;
            let dst_off = (tile_idx * 64 + in_tile) * bpp;
            let src_off = (y * width + x) * bpp;
            if src_off + bpp <= src.len() && dst_off + bpp <= dst.len() {
                dst[dst_off..dst_off + bpp].copy_from_slice(&src[src_off..src_off + bpp]);
            }
        }
    }
    dst
}

/// Détile une texture disposée en micro-tuiles 8x8 vers un tampon linéaire Vulkan.
pub fn detile_2d(src: &[u8], width: usize, height: usize, bpp: usize, mode: XgTileMode) -> Vec<u8> {
    if mode == XgTileMode::Linear || bpp == 0 {
        return src.to_vec();
    }
    let tiles_x = width.div_ceil(8);
    let mut dst = vec![0u8; width * height * bpp];

    for y in 0..height {
        for x in 0..width {
            let tile_idx = (y / 8) * tiles_x + (x / 8);
            let in_tile = morton_2d_8x8((x % 8) as u32, (y % 8) as u32) as usize;
            let src_off = (tile_idx * 64 + in_tile) * bpp;
            let dst_off = (y * width + x) * bpp;
            if src_off + bpp <= src.len() && dst_off + bpp <= dst.len() {
                dst[dst_off..dst_off + bpp].copy_from_slice(&src[src_off..src_off + bpp]);
            }
        }
    }
    dst
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_tile_and_detile_2d_rgba8() {
        let w = 13;
        let h = 11;
        let bpp = 4;
        let linear: Vec<u8> = (0..(w * h * bpp)).map(|i| (i % 251) as u8).collect();

        let tiled = tile_2d(&linear, w, h, bpp, XgTileMode::MicroTile2D);
        let recovered = detile_2d(&tiled, w, h, bpp, XgTileMode::MicroTile2D);
        assert_eq!(linear, recovered);
    }
}
