//! Construction des mots matériels GFX10 (V#/T#/S#) à partir des descripteurs de vues D3D12
//! (`D3D12_SHADER_RESOURCE_VIEW_DESC`, `D3D12_UNORDERED_ACCESS_VIEW_DESC`, `D3D12_RENDER_TARGET_VIEW_DESC`,
//! `D3D12_DEPTH_STENCIL_VIEW_DESC`, `D3D12_SAMPLER_DESC`, `D3D12_CONSTANT_BUFFER_VIEW_DESC`) et utilitaires de
//! (dé)tuilage de sous-ressources pour `d3d12_xs`.
//!
//! Les fonctions de ce module sont pures (aucun accès mémoire du titre) : elles prennent les octets
//! bruts du descripteur de vue et renvoient un emplacement de descripteur de 32 octets. Les
//! dispositions utilisées sont celles, publiques, de `d3d12.h` ; si les octets fournis sont trop
//! courts pour contenir la structure attendue, la fonction renvoie `None` et l'appelant ne touche
//! pas à la mémoire du descripteur.

use crate::d3d12_xs::D3D12ResourceDesc;
use crate::gfx10_descriptors::{
    BufferDesc, GFX10_FORMAT_32_UINT, ImageDesc, ImageType, OobSelect, SWIZZLE_IDENTITY,
    apply_d3d12_component_mapping, dxgi_to_gfx10_format, encode_buffer, encode_image,
    encode_sampler, sampler_from_d3d12,
};
use crate::gfx10_tiling::{TileMode, TiledSurface2D};

/// Taille d'un emplacement de descripteur sur la console (octets).
pub(crate) const SLOT_BYTES: usize = 32;
/// Emplacement de descripteur brut (V#/S# complétés par des zéros, T# sur 32 octets).
pub(crate) type SlotBytes = [u8; SLOT_BYTES];

/// Mapping de composants par défaut (`D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING`).
const DEFAULT_COMPONENT_MAPPING: u32 = 0x1688;

/// Ressource vue : description D3D12 + adresse unifiée de la mémoire de la ressource.
#[derive(Clone, Copy)]
pub(crate) struct ResourceView<'a> {
    pub desc: &'a D3D12ResourceDesc,
    pub address: u64,
}

fn rd32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn rd64(b: &[u8], off: usize) -> Option<u64> {
    let s = b.get(off..off + 8)?;
    Some(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

fn rdf(b: &[u8], off: usize) -> Option<f32> {
    rd32(b, off).map(f32::from_bits)
}

fn buffer_slot(b: &BufferDesc) -> SlotBytes {
    let words = encode_buffer(b);
    let mut out = [0u8; SLOT_BYTES];
    for (i, w) in words.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

fn image_slot_bytes(img: &ImageDesc) -> SlotBytes {
    let words = encode_image(img);
    let mut out = [0u8; SLOT_BYTES];
    for (i, w) in words.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

/// Octets par élément d'un `DXGI_FORMAT` non compressé (formats usuels de vues typées de tampon).
pub(crate) const fn dxgi_bytes_per_element(fmt: u32) -> Option<u32> {
    Some(match fmt {
        1..=4 => 16,
        5..=8 => 12,
        9..=22 => 8,
        23..=47 => 4,
        48..=59 => 2,
        60..=65 => 1,
        67 | 87..=93 => 4,
        _ => return None,
    })
}

/// `(octets par élément, dimension du bloc en texels)` : 8/16 pour BC1..BC7, sinon texel.
/// Cohérent avec `texture_row_layout`.
pub(crate) const fn format_element_layout(dxgi_format: u32) -> (usize, usize) {
    match dxgi_format {
        71 | 72 | 80 => (8, 4),
        74 | 77 | 78 | 83 | 95 | 98 | 99 => (16, 4),
        10 => (8, 1),
        2 => (16, 1),
        _ => (4, 1),
    }
}

// ==================== CBV / sampler ====================

/// `CreateConstantBufferView` : V# sur `[addr, addr + size)`.
pub(crate) fn cbv_slot(addr: u64, size: u32) -> SlotBytes {
    buffer_slot(&BufferDesc {
        address: addr,
        stride: 0,
        num_records: size,
        format: GFX10_FORMAT_32_UINT,
        swizzle: SWIZZLE_IDENTITY,
        oob_select: OobSelect::Raw,
    })
}

/// `CreateSampler` : S# depuis `D3D12_SAMPLER_DESC` (52 octets).
pub(crate) fn sampler_slot(desc: &[u8]) -> Option<SlotBytes> {
    let s = sampler_from_d3d12(
        rd32(desc, 0)?,
        rd32(desc, 4)?,
        rd32(desc, 8)?,
        rd32(desc, 12)?,
        rdf(desc, 16)?,
        rd32(desc, 20)?,
        rd32(desc, 24)?,
        [
            rdf(desc, 28)?,
            rdf(desc, 32)?,
            rdf(desc, 36)?,
            rdf(desc, 40)?,
        ],
        rdf(desc, 44)?,
        rdf(desc, 48)?,
    );
    let words = encode_sampler(&s);
    let mut out = [0u8; SLOT_BYTES];
    for (i, w) in words.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    Some(out)
}

// ==================== T# communs ====================

struct ImageView {
    ty: ImageType,
    first_mip: u32,
    mip_count: u32,
    first_layer: u32,
    layer_count: u32,
    min_lod: f32,
}

fn image_slot(
    res: &ResourceView,
    view_format: u32,
    mapping: u32,
    v: &ImageView,
) -> Option<SlotBytes> {
    let d = res.desc;
    if d.dimension < 2 {
        return None;
    }
    let format = if view_format == 0 {
        d.format
    } else {
        view_format
    };
    let (gfx_format, base) = dxgi_to_gfx10_format(format);
    let swizzle = apply_d3d12_component_mapping(base, mapping);
    let shift = v.first_mip.min(31);
    let depth_total = if d.dimension == 4 {
        u32::from(d.depth_or_array_size).max(1)
    } else {
        1
    };
    let img = ImageDesc {
        address: res.address,
        format: gfx_format,
        image_type: v.ty,
        width: ((d.width.min(u64::from(u32::MAX)) as u32) >> shift).max(1),
        height: (d.height >> shift).max(1),
        depth: (depth_total >> shift).max(1),
        first_layer: v.first_layer,
        layer_count: v.layer_count.max(1),
        first_mip: v.first_mip,
        mip_count: v.mip_count.max(1),
        resource_mips: u32::from(d.mip_levels).max(1),
        samples: if v.ty.is_msaa() {
            d.sample_desc.count.max(1)
        } else {
            1
        },
        swizzle,
        min_lod: v.min_lod,
    };
    Some(image_slot_bytes(&img))
}

fn mip_count_or_all(res: &ResourceView, first_mip: u32, count: u32) -> u32 {
    if count == u32::MAX {
        u32::from(res.desc.mip_levels)
            .max(1)
            .saturating_sub(first_mip)
            .max(1)
    } else {
        count
    }
}

fn array_size_or_rest(res: &ResourceView, first: u32, size: u32) -> u32 {
    if size == u32::MAX {
        u32::from(res.desc.depth_or_array_size)
            .max(1)
            .saturating_sub(first)
            .max(1)
    } else {
        size
    }
}

/// V# pour une vue de tampon (`FirstElement`, `NumElements`, `StructureByteStride`, raw).
fn buffer_view_slot(
    res: &ResourceView,
    first: u64,
    num: u32,
    stride: u32,
    raw: bool,
    format: u32,
    mapping: u32,
) -> Option<SlotBytes> {
    if res.desc.dimension != 1 {
        return None;
    }
    if stride > 0 {
        return Some(buffer_slot(&BufferDesc {
            address: res
                .address
                .wrapping_add(first.wrapping_mul(u64::from(stride))),
            stride,
            num_records: num,
            format: GFX10_FORMAT_32_UINT,
            swizzle: SWIZZLE_IDENTITY,
            oob_select: OobSelect::Structured,
        }));
    }
    if raw || format == 39 {
        return Some(buffer_slot(&BufferDesc {
            address: res.address.wrapping_add(first.wrapping_mul(4)),
            stride: 0,
            num_records: num.saturating_mul(4),
            format: GFX10_FORMAT_32_UINT,
            swizzle: SWIZZLE_IDENTITY,
            oob_select: OobSelect::Raw,
        }));
    }
    let bytes = dxgi_bytes_per_element(format)?;
    let (gfx_format, base) = dxgi_to_gfx10_format(format);
    Some(buffer_slot(&BufferDesc {
        address: res
            .address
            .wrapping_add(first.wrapping_mul(u64::from(bytes))),
        stride: bytes,
        num_records: num,
        format: gfx_format,
        swizzle: apply_d3d12_component_mapping(base, mapping),
        oob_select: OobSelect::Structured,
    }))
}

// ==================== SRV ====================

/// `CreateShaderResourceView` : V# (tampon) ou T# (texture).
///
/// `desc` : octets de `D3D12_SHADER_RESOURCE_VIEW_DESC` (`Format`, `ViewDimension`,
/// `Shader4ComponentMapping`, union à l'octet 16). Sans descripteur (`None`), la vue par défaut de
/// la ressource est utilisée (textures uniquement).
pub(crate) fn srv_slot(res: &ResourceView, desc: Option<&[u8]>) -> Option<SlotBytes> {
    let Some(b) = desc else {
        return default_texture_view(res, DEFAULT_COMPONENT_MAPPING);
    };
    let format = rd32(b, 0)?;
    let dim = rd32(b, 4)?;
    let mapping = rd32(b, 8)?;
    match dim {
        1 => buffer_view_slot(
            res,
            rd64(b, 16)?,
            rd32(b, 24)?,
            rd32(b, 28)?,
            rd32(b, 32)? & 1 != 0,
            format,
            mapping,
        ),
        2 => image_slot(
            res,
            format,
            mapping,
            &ImageView {
                ty: ImageType::Image1D,
                first_mip: rd32(b, 16)?,
                mip_count: mip_count_or_all(res, rd32(b, 16)?, rd32(b, 20)?),
                first_layer: 0,
                layer_count: 1,
                min_lod: rdf(b, 24)?,
            },
        ),
        3 => {
            let (m, f) = (rd32(b, 16)?, rd32(b, 24)?);
            image_slot(
                res,
                format,
                mapping,
                &ImageView {
                    ty: ImageType::Image1DArray,
                    first_mip: m,
                    mip_count: mip_count_or_all(res, m, rd32(b, 20)?),
                    first_layer: f,
                    layer_count: array_size_or_rest(res, f, rd32(b, 28)?),
                    min_lod: rdf(b, 32)?,
                },
            )
        }
        4 => {
            let m = rd32(b, 16)?;
            image_slot(
                res,
                format,
                mapping,
                &ImageView {
                    ty: ImageType::Image2D,
                    first_mip: m,
                    mip_count: mip_count_or_all(res, m, rd32(b, 20)?),
                    first_layer: 0,
                    layer_count: 1,
                    min_lod: rdf(b, 28)?,
                },
            )
        }
        5 => {
            let (m, f) = (rd32(b, 16)?, rd32(b, 24)?);
            image_slot(
                res,
                format,
                mapping,
                &ImageView {
                    ty: ImageType::Image2DArray,
                    first_mip: m,
                    mip_count: mip_count_or_all(res, m, rd32(b, 20)?),
                    first_layer: f,
                    layer_count: array_size_or_rest(res, f, rd32(b, 28)?),
                    min_lod: rdf(b, 36)?,
                },
            )
        }
        6 => image_slot(
            res,
            format,
            mapping,
            &ImageView {
                ty: ImageType::Image2DMsaa,
                first_mip: 0,
                mip_count: 1,
                first_layer: 0,
                layer_count: 1,
                min_lod: 0.0,
            },
        ),
        7 => {
            let f = rd32(b, 16)?;
            image_slot(
                res,
                format,
                mapping,
                &ImageView {
                    ty: ImageType::Image2DMsaaArray,
                    first_mip: 0,
                    mip_count: 1,
                    first_layer: f,
                    layer_count: array_size_or_rest(res, f, rd32(b, 20)?),
                    min_lod: 0.0,
                },
            )
        }
        8 => {
            let m = rd32(b, 16)?;
            image_slot(
                res,
                format,
                mapping,
                &ImageView {
                    ty: ImageType::Image3D,
                    first_mip: m,
                    mip_count: mip_count_or_all(res, m, rd32(b, 20)?),
                    first_layer: 0,
                    layer_count: 1,
                    min_lod: rdf(b, 24)?,
                },
            )
        }
        9 => {
            let m = rd32(b, 16)?;
            image_slot(
                res,
                format,
                mapping,
                &ImageView {
                    ty: ImageType::Cube,
                    first_mip: m,
                    mip_count: mip_count_or_all(res, m, rd32(b, 20)?),
                    first_layer: 0,
                    layer_count: 6,
                    min_lod: rdf(b, 24)?,
                },
            )
        }
        10 => {
            let (m, f, n) = (rd32(b, 16)?, rd32(b, 24)?, rd32(b, 28)?);
            image_slot(
                res,
                format,
                mapping,
                &ImageView {
                    ty: ImageType::Cube,
                    first_mip: m,
                    mip_count: mip_count_or_all(res, m, rd32(b, 20)?),
                    first_layer: f,
                    layer_count: n.saturating_mul(6).max(6),
                    min_lod: rdf(b, 32)?,
                },
            )
        }
        // 0 = UNKNOWN, 11 = RAYTRACING_ACCELERATION_STRUCTURE : disposition non couverte.
        _ => None,
    }
}

/// Vue par défaut d'une texture (`desc == NULL`).
fn default_texture_view(res: &ResourceView, mapping: u32) -> Option<SlotBytes> {
    let d = res.desc;
    let arr = u32::from(d.depth_or_array_size).max(1);
    let mips = u32::from(d.mip_levels).max(1);
    let msaa = d.sample_desc.count > 1;
    let (ty, layers) = match (d.dimension, msaa, arr > 1) {
        (2, _, false) => (ImageType::Image1D, 1),
        (2, _, true) => (ImageType::Image1DArray, arr),
        (3, true, false) => (ImageType::Image2DMsaa, 1),
        (3, true, true) => (ImageType::Image2DMsaaArray, arr),
        (3, false, false) => (ImageType::Image2D, 1),
        (3, false, true) => (ImageType::Image2DArray, arr),
        (4, _, _) => (ImageType::Image3D, 1),
        _ => return None,
    };
    image_slot(
        res,
        0,
        mapping,
        &ImageView {
            ty,
            first_mip: 0,
            mip_count: mips,
            first_layer: 0,
            layer_count: layers,
            min_lod: 0.0,
        },
    )
}

// ==================== UAV ====================

/// `CreateUnorderedAccessView` : V# (tampon) ou T# (texture). `desc` = `D3D12_UNORDERED_ACCESS_VIEW_DESC`
/// (`Format`, `ViewDimension`, union à l'octet 8).
pub(crate) fn uav_slot(res: &ResourceView, desc: Option<&[u8]>) -> Option<SlotBytes> {
    let Some(b) = desc else {
        return default_texture_view(res, DEFAULT_COMPONENT_MAPPING);
    };
    let format = rd32(b, 0)?;
    let dim = rd32(b, 4)?;
    let id = DEFAULT_COMPONENT_MAPPING;
    let one = |ty, m: u32, f: u32, n: u32| {
        image_slot(
            res,
            format,
            id,
            &ImageView {
                ty,
                first_mip: m,
                mip_count: 1,
                first_layer: f,
                layer_count: n,
                min_lod: 0.0,
            },
        )
    };
    match dim {
        1 => buffer_view_slot(
            res,
            rd64(b, 8)?,
            rd32(b, 16)?,
            rd32(b, 20)?,
            rd32(b, 32)? & 1 != 0,
            format,
            id,
        ),
        2 => one(ImageType::Image1D, rd32(b, 8)?, 0, 1),
        3 => {
            let f = rd32(b, 12)?;
            one(
                ImageType::Image1DArray,
                rd32(b, 8)?,
                f,
                array_size_or_rest(res, f, rd32(b, 16)?),
            )
        }
        4 => one(ImageType::Image2D, rd32(b, 8)?, 0, 1),
        5 => {
            let f = rd32(b, 12)?;
            one(
                ImageType::Image2DArray,
                rd32(b, 8)?,
                f,
                array_size_or_rest(res, f, rd32(b, 16)?),
            )
        }
        8 => one(ImageType::Image3D, rd32(b, 8)?, 0, 1),
        _ => None,
    }
}

// ==================== RTV / DSV ====================

/// `CreateRenderTargetView` : T# de la sous-ressource ciblée (pas le registre `CB_COLORn` matériel).
/// `desc` = `D3D12_RENDER_TARGET_VIEW_DESC` (`Format`, `ViewDimension`, union à l'octet 8).
pub(crate) fn rtv_slot(res: &ResourceView, desc: Option<&[u8]>) -> Option<SlotBytes> {
    let Some(b) = desc else {
        return default_target_view(res);
    };
    let format = rd32(b, 0)?;
    let dim = rd32(b, 4)?;
    let one = |ty, m: u32, f: u32, n: u32| {
        image_slot(
            res,
            format,
            DEFAULT_COMPONENT_MAPPING,
            &ImageView {
                ty,
                first_mip: m,
                mip_count: 1,
                first_layer: f,
                layer_count: n,
                min_lod: 0.0,
            },
        )
    };
    match dim {
        1 => buffer_view_slot(
            res,
            rd64(b, 8)?,
            rd32(b, 16)?,
            0,
            false,
            format,
            DEFAULT_COMPONENT_MAPPING,
        ),
        2 => one(ImageType::Image1D, rd32(b, 8)?, 0, 1),
        3 => {
            let f = rd32(b, 12)?;
            one(
                ImageType::Image1DArray,
                rd32(b, 8)?,
                f,
                array_size_or_rest(res, f, rd32(b, 16)?),
            )
        }
        4 => one(ImageType::Image2D, rd32(b, 8)?, 0, 1),
        5 => {
            let f = rd32(b, 12)?;
            one(
                ImageType::Image2DArray,
                rd32(b, 8)?,
                f,
                array_size_or_rest(res, f, rd32(b, 16)?),
            )
        }
        6 => one(ImageType::Image2DMsaa, 0, 0, 1),
        7 => {
            let f = rd32(b, 8)?;
            one(
                ImageType::Image2DMsaaArray,
                0,
                f,
                array_size_or_rest(res, f, rd32(b, 12)?),
            )
        }
        8 => one(ImageType::Image3D, rd32(b, 8)?, 0, 1),
        _ => None,
    }
}

/// `CreateDepthStencilView` : T# de la sous-ressource ciblée (pas le registre `DB_Z_INFO` matériel).
/// `desc` = `D3D12_DEPTH_STENCIL_VIEW_DESC` (`Format`, `ViewDimension`, `Flags`, union à l'octet 12).
pub(crate) fn dsv_slot(res: &ResourceView, desc: Option<&[u8]>) -> Option<SlotBytes> {
    let Some(b) = desc else {
        return default_target_view(res);
    };
    let format = rd32(b, 0)?;
    let dim = rd32(b, 4)?;
    let one = |ty, m: u32, f: u32, n: u32| {
        image_slot(
            res,
            format,
            DEFAULT_COMPONENT_MAPPING,
            &ImageView {
                ty,
                first_mip: m,
                mip_count: 1,
                first_layer: f,
                layer_count: n,
                min_lod: 0.0,
            },
        )
    };
    match dim {
        1 => one(ImageType::Image1D, rd32(b, 12)?, 0, 1),
        2 => {
            let f = rd32(b, 16)?;
            one(
                ImageType::Image1DArray,
                rd32(b, 12)?,
                f,
                array_size_or_rest(res, f, rd32(b, 20)?),
            )
        }
        3 => one(ImageType::Image2D, rd32(b, 12)?, 0, 1),
        4 => {
            let f = rd32(b, 16)?;
            one(
                ImageType::Image2DArray,
                rd32(b, 12)?,
                f,
                array_size_or_rest(res, f, rd32(b, 20)?),
            )
        }
        5 => one(ImageType::Image2DMsaa, 0, 0, 1),
        6 => {
            let f = rd32(b, 12)?;
            one(
                ImageType::Image2DMsaaArray,
                0,
                f,
                array_size_or_rest(res, f, rd32(b, 16)?),
            )
        }
        _ => None,
    }
}

/// Vue de cible par défaut : sous-ressource 0 (mip 0, première couche).
fn default_target_view(res: &ResourceView) -> Option<SlotBytes> {
    let d = res.desc;
    let msaa = d.sample_desc.count > 1;
    let ty = match (d.dimension, msaa) {
        (2, _) => ImageType::Image1D,
        (3, true) => ImageType::Image2DMsaa,
        (3, false) => ImageType::Image2D,
        (4, _) => ImageType::Image3D,
        _ => return None,
    };
    image_slot(
        res,
        0,
        DEFAULT_COMPONENT_MAPPING,
        &ImageView {
            ty,
            first_mip: 0,
            mip_count: 1,
            first_layer: 0,
            layer_count: 1,
            min_lod: 0.0,
        },
    )
}

// ==================== Tuilage de sous-ressources ====================

/// Détuile une surface 2D d'éléments de `elem_bytes` octets (1/2/4/8/16), `width × height` éléments
/// (texels, ou blocs 4×4 pour BC) : `tiled` -> image linéaire serrée (`width * elem_bytes` par ligne).
pub fn detile_surface(
    mode: TileMode,
    elem_bytes: usize,
    width: usize,
    height: usize,
    tiled: &[u8],
) -> Option<Vec<u8>> {
    let surf = TiledSurface2D::new(mode, elem_bytes, width, height)?;
    if tiled.len() < surf.tiled_size() {
        return None;
    }
    let pitch = width * elem_bytes;
    let mut linear = vec![0u8; pitch * height];
    surf.detile(tiled, &mut linear, pitch).then_some(linear)
}

/// Tuile une surface 2D (voir [`detile_surface`]) : image linéaire serrée -> `tiled_size` octets.
pub fn tile_surface(
    mode: TileMode,
    elem_bytes: usize,
    width: usize,
    height: usize,
    linear: &[u8],
) -> Option<Vec<u8>> {
    let surf = TiledSurface2D::new(mode, elem_bytes, width, height)?;
    let pitch = width * elem_bytes;
    if linear.len() < pitch * height {
        return None;
    }
    let mut tiled = vec![0u8; surf.tiled_size()];
    surf.tile(linear, &mut tiled, pitch).then_some(tiled)
}

/// Surface tuilée pour la sous-ressource 0 : `(surface, octets par ligne linéaire)`. Les formats
/// BC1..BC7 sont traités en blocs 4×4 (1 élément = 1 bloc de 8 ou 16 octets).
pub(crate) fn tiled_surface_for(
    mode: TileMode,
    width: u32,
    height: u32,
    dxgi_format: u32,
) -> Option<(TiledSurface2D, usize)> {
    let (elem, block) = format_element_layout(dxgi_format);
    let w = (width.max(1) as usize).div_ceil(block);
    let h = (height.max(1) as usize).div_ceil(block);
    let surf = TiledSurface2D::new(mode, elem, w, h)?;
    Some((surf, w * elem))
}

/// Détuile la sous-ressource 0 (`tiled` -> image linéaire serrée `row_bytes × rows`, dimensions
/// de `texture_row_layout`).
pub fn detile_subresource(
    mode: TileMode,
    width: u32,
    height: u32,
    dxgi_format: u32,
    tiled: &[u8],
) -> Option<Vec<u8>> {
    let (elem, block) = format_element_layout(dxgi_format);
    detile_surface(
        mode,
        elem,
        (width.max(1) as usize).div_ceil(block),
        (height.max(1) as usize).div_ceil(block),
        tiled,
    )
}

/// Tuile la sous-ressource 0 (image linéaire serrée -> `tiled_size` octets tuilés).
pub fn tile_subresource(
    mode: TileMode,
    width: u32,
    height: u32,
    dxgi_format: u32,
    linear: &[u8],
) -> Option<Vec<u8>> {
    let (elem, block) = format_element_layout(dxgi_format);
    tile_surface(
        mode,
        elem,
        (width.max(1) as usize).div_ceil(block),
        (height.max(1) as usize).div_ceil(block),
        linear,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12_xs::{D3D12ResourceDesc, DxgiSampleDesc, texture_row_layout};
    use crate::gfx10_descriptors::{
        ImageType, decode_buffer, decode_image, decode_sampler, is_image_descriptor,
    };

    fn words(slot: &SlotBytes) -> [u32; 8] {
        let mut w = [0u32; 8];
        for (i, c) in slot.chunks_exact(4).enumerate() {
            w[i] = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        }
        w
    }

    fn pattern(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| ((i * 131 + 17) >> 1) as u8 ^ (i as u8))
            .collect()
    }

    fn tex(
        dim: u32,
        w: u64,
        h: u32,
        depth: u16,
        mips: u16,
        fmt: u32,
        samples: u32,
    ) -> D3D12ResourceDesc {
        D3D12ResourceDesc {
            dimension: dim,
            width: w,
            height: h,
            depth_or_array_size: depth,
            mip_levels: mips,
            format: fmt,
            sample_desc: DxgiSampleDesc {
                count: samples,
                quality: 0,
            },
            ..Default::default()
        }
    }

    fn u32s(v: &[u32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    #[test]
    fn tile_roundtrip_all_element_sizes_and_both_modes() {
        for mode in [TileMode::Sw4KbS, TileMode::Sw64KbD] {
            for elem in [1usize, 2, 4, 8, 16] {
                for (w, h) in [(1, 1), (7, 5), (37, 29), (130, 66)] {
                    let linear = pattern(w * h * elem);
                    let tiled = tile_surface(mode, elem, w, h, &linear).expect("tuilage");
                    let surf = TiledSurface2D::new(mode, elem, w, h).unwrap();
                    assert_eq!(tiled.len(), surf.tiled_size());
                    let back = detile_surface(mode, elem, w, h, &tiled).expect("détuilage");
                    assert_eq!(back, linear, "{mode:?} elem={elem} {w}x{h}");
                    // Cohérence avec l'adressage de la surface.
                    let (x, y) = (w - 1, h - 1);
                    let off = surf.element_offset(x, y).unwrap();
                    let lin = (y * w + x) * elem;
                    assert_eq!(&tiled[off..off + elem], &linear[lin..lin + elem]);
                }
            }
        }
    }

    #[test]
    fn tile_roundtrip_bc1_to_bc7() {
        // BC1=71, BC2=74, BC3=77, BC4=80, BC5=83, BC6H=95, BC7=98.
        for mode in [TileMode::Sw4KbS, TileMode::Sw64KbD] {
            for fmt in [71u32, 74, 77, 80, 83, 95, 98] {
                for (w, h) in [(4u32, 4u32), (37, 29), (256, 128)] {
                    let (row_bytes, rows) = texture_row_layout(w, h, fmt);
                    let linear = pattern(row_bytes * rows);
                    let tiled = tile_subresource(mode, w, h, fmt, &linear).expect("tuilage BC");
                    assert!(tiled.len() >= linear.len());
                    let back = detile_subresource(mode, w, h, fmt, &tiled).expect("détuilage BC");
                    assert_eq!(back, linear, "{mode:?} fmt={fmt} {w}x{h}");
                }
            }
        }
    }

    #[test]
    fn tile_rejects_short_buffers_and_bad_elements() {
        let m = TileMode::Sw4KbS;
        assert!(tile_surface(m, 4, 16, 16, &[0u8; 10]).is_none());
        let tiled = tile_surface(m, 4, 16, 16, &[0u8; 16 * 16 * 4]).unwrap();
        assert!(detile_surface(m, 4, 16, 16, &tiled[..tiled.len() - 1]).is_none());
        assert!(tile_surface(m, 3, 16, 16, &[0u8; 16 * 16 * 3]).is_none());
        assert!(tile_surface(m, 4, 0, 16, &[]).is_none());
    }

    #[test]
    fn cbv_slot_is_a_raw_vsharp() {
        let slot = cbv_slot(0x0000_1234_5678_9000, 0x100);
        let w = words(&slot);
        let b = decode_buffer(&[w[0], w[1], w[2], w[3]]).expect("V#");
        assert_eq!(b.address, 0x0000_1234_5678_9000);
        assert_eq!(b.num_records, 0x100);
        assert_eq!(b.oob_select, OobSelect::Raw);
        assert_eq!(
            &slot[16..],
            &[0u8; 16],
            "moitié haute de l'emplacement à zéro"
        );
    }

    #[test]
    fn sampler_slot_matches_sampler_from_d3d12() {
        // Filtre linéaire (0x15), WRAP/MIRROR/CLAMP, biais 0.5, aniso 1, ALWAYS, bordure noire opaque.
        let mut d = u32s(&[0x15, 1, 2, 3]);
        d.extend(0.5f32.to_le_bytes());
        d.extend(u32s(&[1, 8]));
        for c in [0.0f32, 0.0, 0.0, 1.0, 0.25, 8.0] {
            d.extend(c.to_le_bytes());
        }
        assert_eq!(d.len(), 52);
        let slot = sampler_slot(&d).expect("S#");
        let w = words(&slot);
        let got = decode_sampler(&[w[0], w[1], w[2], w[3]]);
        let want = sampler_from_d3d12(0x15, 1, 2, 3, 0.5, 1, 8, [0.0, 0.0, 0.0, 1.0], 0.25, 8.0);
        assert_eq!(got.address_u, want.address_u);
        assert_eq!(got.address_v, want.address_v);
        assert_eq!(got.address_w, want.address_w);
        assert_eq!(got.mag_filter, want.mag_filter);
        assert_eq!(got.min_filter, want.min_filter);
        assert_eq!(got.mip_filter, want.mip_filter);
        assert_eq!(got.lod_bias, 0.5);
        assert_eq!(got.min_lod, 0.25);
        assert_eq!(got.max_lod, 8.0);
        assert_eq!(got.border_color, want.border_color);
        assert!(sampler_slot(&d[..51]).is_none());
        assert_eq!(&slot[16..], &[0u8; 16]);
    }

    #[test]
    fn srv_texture2d_slot_describes_the_view() {
        let d = tex(3, 256, 128, 1, 9, 28, 1);
        let r = ResourceView {
            desc: &d,
            address: 0x7000_0001_0000,
        };
        // Format 28 (RGBA8), dimension TEXTURE2D (4), mapping par défaut, mip 2, 3 mips, plan 0, clamp 1.0.
        let mut b = u32s(&[28, 4, 0x1688, 0, 2, 3, 0]);
        b.extend(1.0f32.to_le_bytes());
        b.resize(48, 0);
        let slot = srv_slot(&r, Some(&b)).expect("T#");
        let w = words(&slot);
        assert!(is_image_descriptor(&w));
        let img = decode_image(&w).expect("T# valide");
        assert_eq!(img.address, 0x7000_0001_0000);
        assert_eq!(img.image_type, ImageType::Image2D);
        assert_eq!((img.width, img.height), (64, 32), "mip 2 de 256x128");
        assert_eq!((img.first_mip, img.mip_count, img.resource_mips), (2, 3, 9));
        assert_eq!(img.format, dxgi_to_gfx10_format(28).0);
        assert_eq!(img.swizzle, SWIZZLE_IDENTITY);
        assert_eq!(img.min_lod, 1.0);
    }

    #[test]
    fn srv_default_view_and_array_and_cube_types() {
        let d = tex(3, 64, 64, 6, 1, 28, 1);
        let r = ResourceView {
            desc: &d,
            address: 0x1000,
        };
        let img = decode_image(&words(&srv_slot(&r, None).unwrap())).unwrap();
        assert_eq!(img.image_type, ImageType::Image2DArray);
        assert_eq!(img.layer_count, 6);
        // Cube (dim 9) : 6 faces ; MostDetailedMip=0, MipLevels=-1 (tous).
        let mut b = u32s(&[28, 9, 0x1688, 0, 0, u32::MAX, 0]);
        b.resize(48, 0);
        let img = decode_image(&words(&srv_slot(&r, Some(&b)).unwrap())).unwrap();
        assert_eq!(img.image_type, ImageType::Cube);
        assert_eq!(img.layer_count, 6);
        // MSAA par défaut.
        let ms = tex(3, 64, 64, 1, 1, 28, 4);
        let rm = ResourceView {
            desc: &ms,
            address: 0x1000,
        };
        let img = decode_image(&words(&srv_slot(&rm, None).unwrap())).unwrap();
        assert_eq!(img.image_type, ImageType::Image2DMsaa);
        assert_eq!(img.samples, 4);
        // Dimension inconnue ou descripteur tronqué : rien.
        assert!(srv_slot(&r, Some(&u32s(&[28, 0, 0x1688, 0]))).is_none());
        assert!(srv_slot(&r, Some(&u32s(&[28, 4]))).is_none());
    }

    #[test]
    fn buffer_views_srv_and_uav() {
        let d = tex(1, 4096, 1, 1, 1, 0, 1);
        let r = ResourceView {
            desc: &d,
            address: 0x2000,
        };
        // SRV structuré : FirstElement 4, 10 éléments de 16 octets.
        let mut b = u32s(&[0, 1, 0x1688, 0, 4, 0, 10, 16, 0]);
        b.resize(48, 0);
        let v = decode_buffer(&{
            let w = words(&srv_slot(&r, Some(&b)).unwrap());
            [w[0], w[1], w[2], w[3]]
        })
        .unwrap();
        assert_eq!((v.address, v.stride, v.num_records), (0x2000 + 64, 16, 10));
        assert_eq!(v.oob_select, OobSelect::Structured);
        // UAV brut (R32_TYPELESS, flag RAW) : 8 éléments = 32 octets.
        let mut u = u32s(&[39, 1, 2, 0, 8, 0, 0, 0]);
        u.resize(48, 0);
        u[32] = 1;
        let v = decode_buffer(&{
            let w = words(&uav_slot(&r, Some(&u)).unwrap());
            [w[0], w[1], w[2], w[3]]
        })
        .unwrap();
        assert_eq!((v.address, v.stride, v.num_records), (0x2000 + 8, 0, 32));
        assert_eq!(v.oob_select, OobSelect::Raw);
        // UAV typé R32_FLOAT (41).
        let mut t = u32s(&[41, 1, 0, 0, 16, 0, 0, 0]);
        t.resize(48, 0);
        let v = decode_buffer(&{
            let w = words(&uav_slot(&r, Some(&t)).unwrap());
            [w[0], w[1], w[2], w[3]]
        })
        .unwrap();
        assert_eq!((v.stride, v.num_records), (4, 16));
        // Une vue de tampon sur une texture est refusée.
        let td = tex(3, 8, 8, 1, 1, 28, 1);
        let rt = ResourceView {
            desc: &td,
            address: 0,
        };
        assert!(srv_slot(&rt, Some(&b)).is_none());
    }

    #[test]
    fn uav_rtv_dsv_texture_slots() {
        let d = tex(3, 128, 128, 4, 5, 28, 1);
        let r = ResourceView {
            desc: &d,
            address: 0x4000_0000,
        };
        // UAV TEXTURE2DARRAY (5) : mip 1, première couche 1, 2 couches.
        let u = u32s(&[28, 5, 1, 1, 2, 0]);
        let img = decode_image(&words(&uav_slot(&r, Some(&u)).unwrap())).unwrap();
        assert_eq!(img.image_type, ImageType::Image2DArray);
        assert_eq!((img.first_mip, img.first_layer, img.layer_count), (1, 1, 2));
        assert_eq!((img.width, img.height), (64, 64));
        // RTV TEXTURE2D (4), mip 3.
        let rt = u32s(&[28, 4, 3, 0]);
        let img = decode_image(&words(&rtv_slot(&r, Some(&rt)).unwrap())).unwrap();
        assert_eq!(img.image_type, ImageType::Image2D);
        assert_eq!((img.first_mip, img.width), (3, 16));
        // RTV par défaut.
        let img = decode_image(&words(&rtv_slot(&r, None).unwrap())).unwrap();
        assert_eq!((img.first_mip, img.width), (0, 128));
        // DSV D32_FLOAT (40) TEXTURE2D (3) : drapeaux à 0, mip 0 à l'octet 12.
        let dd = tex(3, 128, 128, 1, 1, 40, 1);
        let rd = ResourceView {
            desc: &dd,
            address: 0x5000_0000,
        };
        let ds = u32s(&[40, 3, 0, 0]);
        let img = decode_image(&words(&dsv_slot(&rd, Some(&ds)).unwrap())).unwrap();
        assert_eq!(img.format, dxgi_to_gfx10_format(40).0);
        assert_eq!(img.address, 0x5000_0000);
        assert!(dsv_slot(&rd, Some(&u32s(&[40, 99, 0, 0]))).is_none());
    }

    #[test]
    fn is_not_an_image_when_a_buffer() {
        let slot = cbv_slot(0x1000, 64);
        let w = words(&slot);
        assert!(!is_image_descriptor(&w));
    }
}
