//! Descripteurs matériels **AMD GFX10 / RDNA 2 (Xbox Series X)** :
//! - **V#** (`SQ_BUF_RSRC_WORD0..3`, 16 octets) : Constant Buffer Views, Buffer SRV/UAV ;
//! - **T#** (`SQ_IMG_RSRC_WORD0..7`, 32 octets) : Texture SRV/UAV (1D/2D/3D/Cube/Array/MSAA) ;
//! - **S#** (`SQ_IMG_SAMP_WORD0..3`, 16 octets) : Samplers (`CreateSampler` / `CreateSampler2`).
//!
//! Sur la console Xbox Series X, les handles CPU de descripteurs sont de véritables adresses
//! mémoire unifiée contenant ces mots matériels. Les écrire lors de `CreateConstantBufferView`,
//! `CreateShaderResourceView`, `CreateUnorderedAccessView` et `CreateSampler` garantit la
//! cohérence des copies (`CopyDescriptors*`), des hachages et de la lecture par le titre ou
//! les shaders.
//!
//! Positions des champs et constantes : base publique Mesa MIT (`gfx10-rsrc.json`, `gfx6.json`,
//! `ac_descriptors.c`).

#![allow(dead_code)]

/// Mots d'un descripteur tampon `V#` (16 octets).
pub type BufferWords = [u32; 4];
/// Mots d'un descripteur image `T#` (32 octets).
pub type ImageWords = [u32; 8];
/// Mots d'un descripteur échantillonneur `S#` (16 octets).
pub type SamplerWords = [u32; 4];
/// Swizzle matériel `DST_SEL_X..W` (`SQ_SEL_XYZW01`).
pub type Swizzle = [u8; 4];

pub const SEL_ZERO: u8 = 0;
pub const SEL_ONE: u8 = 1;
pub const SEL_X: u8 = 4;
pub const SEL_Y: u8 = 5;
pub const SEL_Z: u8 = 6;
pub const SEL_W: u8 = 7;

pub const SWIZZLE_IDENTITY: Swizzle = [SEL_X, SEL_Y, SEL_Z, SEL_W];
pub const SWIZZLE_R: Swizzle = [SEL_X, SEL_ZERO, SEL_ZERO, SEL_ONE];
pub const SWIZZLE_RG: Swizzle = [SEL_X, SEL_Y, SEL_ZERO, SEL_ONE];
pub const SWIZZLE_RGB: Swizzle = [SEL_X, SEL_Y, SEL_Z, SEL_ONE];
pub const SWIZZLE_BGRA: Swizzle = [SEL_Z, SEL_Y, SEL_X, SEL_W];
pub const SWIZZLE_BGR: Swizzle = [SEL_Z, SEL_Y, SEL_X, SEL_ONE];

/// Code `GFX10_FORMAT_32_UINT` (utilisé pour les tampons bruts/CBV).
pub const GFX10_FORMAT_32_UINT: u32 = 20;
/// Code `GFX10_FORMAT_8_8_8_8_UNORM` (RGBA8 / BGRA8).
pub const GFX10_FORMAT_8_8_8_8_UNORM: u32 = 56;

#[inline]
const fn field(value: u32, low: u32, width: u32) -> u32 {
    (value & ((1u32 << width) - 1)) << low
}

#[inline]
const fn get(word: u32, low: u32, width: u32) -> u32 {
    (word >> low) & ((1u32 << width) - 1)
}

#[inline]
const fn valid_swizzle(s: Swizzle) -> bool {
    let mut i = 0;
    while i < 4 {
        if s[i] > 7 || s[i] == 2 || s[i] == 3 {
            return false;
        }
        i += 1;
    }
    true
}

#[inline]
fn clamp_f32(v: f32, lo: f32, hi: f32) -> f32 {
    if v > lo {
        if v < hi { v } else { hi }
    } else {
        lo
    }
}

#[inline]
fn unsigned_fixed_8(v: f32, lo: f32, hi: f32) -> u32 {
    (clamp_f32(v, lo, hi) * 256.0) as u32
}

#[inline]
fn signed_fixed_8(v: f32, lo: f32, hi: f32) -> u32 {
    ((clamp_f32(v, lo, hi) * 256.0) as i32) as u32
}

// ==================== V# : SQ_BUF_RSRC_WORD0..3 ====================

/// Mode de contrôle de bornes matériel `SQ_BUF_RSRC_WORD3.OOB_SELECT`.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OobSelect {
    StructuredWithOffset = 0,
    Structured = 1,
    Disabled = 2,
    Raw = 3,
}

impl OobSelect {
    fn from_u32(v: u32) -> Self {
        match v & 3 {
            0 => Self::StructuredWithOffset,
            1 => Self::Structured,
            2 => Self::Disabled,
            _ => Self::Raw,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferDesc {
    pub address: u64,
    pub stride: u32,
    pub num_records: u32,
    pub format: u32,
    pub swizzle: Swizzle,
    pub oob_select: OobSelect,
}

impl Default for BufferDesc {
    fn default() -> Self {
        Self {
            address: 0,
            stride: 0,
            num_records: 0,
            format: GFX10_FORMAT_32_UINT,
            swizzle: SWIZZLE_IDENTITY,
            oob_select: OobSelect::Raw,
        }
    }
}

/// Encode un descripteur tampon `V#` (16 octets, `SQ_BUF_RSRC_WORD0..3`).
pub fn encode_buffer(b: &BufferDesc) -> BufferWords {
    [
        b.address as u32,
        field((b.address >> 32) as u32, 0, 16) | field(b.stride, 16, 14),
        b.num_records,
        field(u32::from(b.swizzle[0]), 0, 3)
            | field(u32::from(b.swizzle[1]), 3, 3)
            | field(u32::from(b.swizzle[2]), 6, 3)
            | field(u32::from(b.swizzle[3]), 9, 3)
            | field(b.format, 12, 7)
            | field(b.oob_select as u32, 28, 2),
    ]
}

/// Décode un descripteur `V#`. Retourne `None` si `TYPE != 0` (ex. un `T#`) ou swizzle réservé.
pub fn decode_buffer(words: &BufferWords) -> Option<BufferDesc> {
    if get(words[3], 30, 2) != 0 {
        return None;
    }
    let swizzle = [
        get(words[3], 0, 3) as u8,
        get(words[3], 3, 3) as u8,
        get(words[3], 6, 3) as u8,
        get(words[3], 9, 3) as u8,
    ];
    if !valid_swizzle(swizzle) {
        return None;
    }
    Some(BufferDesc {
        address: (u64::from(get(words[1], 0, 16)) << 32) | u64::from(words[0]),
        stride: get(words[1], 16, 14),
        num_records: words[2],
        format: get(words[3], 12, 7),
        swizzle,
        oob_select: OobSelect::from_u32(get(words[3], 28, 2)),
    })
}

// ==================== T# : SQ_IMG_RSRC_WORD0..7 ====================

/// Type d'image `SQ_RSRC_IMG_TYPE` (`WORD3.TYPE`, valeurs 8..=15).
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageType {
    Image1D = 8,
    Image2D = 9,
    Image3D = 10,
    Cube = 11,
    Image1DArray = 12,
    Image2DArray = 13,
    Image2DMsaa = 14,
    Image2DMsaaArray = 15,
}

impl ImageType {
    pub const fn is_msaa(self) -> bool {
        matches!(self, Self::Image2DMsaa | Self::Image2DMsaaArray)
    }

    fn from_u32(v: u32) -> Option<Self> {
        match v {
            8 => Some(Self::Image1D),
            9 => Some(Self::Image2D),
            10 => Some(Self::Image3D),
            11 => Some(Self::Cube),
            12 => Some(Self::Image1DArray),
            13 => Some(Self::Image2DArray),
            14 => Some(Self::Image2DMsaa),
            15 => Some(Self::Image2DMsaaArray),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageDesc {
    pub address: u64,
    pub format: u32,
    pub image_type: ImageType,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub first_layer: u32,
    pub layer_count: u32,
    pub first_mip: u32,
    pub mip_count: u32,
    pub resource_mips: u32,
    pub samples: u32,
    pub swizzle: Swizzle,
    pub min_lod: f32,
}

impl Default for ImageDesc {
    fn default() -> Self {
        Self {
            address: 0,
            format: GFX10_FORMAT_8_8_8_8_UNORM,
            image_type: ImageType::Image2D,
            width: 1,
            height: 1,
            depth: 1,
            first_layer: 0,
            layer_count: 1,
            first_mip: 0,
            mip_count: 1,
            resource_mips: 1,
            samples: 1,
            swizzle: SWIZZLE_IDENTITY,
            min_lod: 0.0,
        }
    }
}

/// Indique si les 8 mots forment un descripteur image `T#` (`WORD3.TYPE >= 8`).
pub const fn is_image_descriptor(words: &ImageWords) -> bool {
    get(words[3], 28, 4) >= 8
}

/// Encode un descripteur image `T#` (32 octets, `SQ_IMG_RSRC_WORD0..7`).
pub fn encode_image(img: &ImageDesc) -> ImageWords {
    let msaa = img.image_type.is_msaa();
    let sample_log2 = img.samples.max(1).ilog2();
    let base_level = if msaa { 0 } else { img.first_mip };
    let last_level = if msaa {
        sample_log2
    } else {
        img.first_mip
            .saturating_add(img.mip_count.max(1))
            .saturating_sub(1)
    };
    let max_mip = if msaa {
        sample_log2
    } else {
        img.resource_mips.max(1).saturating_sub(1)
    };
    let last = if img.image_type == ImageType::Image3D {
        img.depth.max(1).saturating_sub(1)
    } else {
        img.first_layer
            .saturating_add(img.layer_count.max(1))
            .saturating_sub(1)
    };
    let units = img.address >> 8;
    let w_m1 = img.width.max(1).saturating_sub(1);
    let h_m1 = img.height.max(1).saturating_sub(1);
    [
        units as u32,
        field((units >> 32) as u32, 0, 8)
            | field(unsigned_fixed_8(img.min_lod, 0.0, 15.0), 8, 12)
            | field(img.format, 20, 9)
            | field(w_m1, 30, 2),
        field(w_m1 >> 2, 0, 12) | field(h_m1, 14, 14),
        field(u32::from(img.swizzle[0]), 0, 3)
            | field(u32::from(img.swizzle[1]), 3, 3)
            | field(u32::from(img.swizzle[2]), 6, 3)
            | field(u32::from(img.swizzle[3]), 9, 3)
            | field(base_level, 12, 4)
            | field(last_level, 16, 4)
            | field(img.image_type as u32, 28, 4),
        field(last, 0, 13) | field(img.first_layer, 16, 13),
        field(max_mip, 4, 4),
        0,
        0,
    ]
}

/// Décode un descripteur image `T#` (32 octets).
pub fn decode_image(words: &ImageWords) -> Option<ImageDesc> {
    let image_type = ImageType::from_u32(get(words[3], 28, 4))?;
    let swizzle = [
        get(words[3], 0, 3) as u8,
        get(words[3], 3, 3) as u8,
        get(words[3], 6, 3) as u8,
        get(words[3], 9, 3) as u8,
    ];
    if !valid_swizzle(swizzle) {
        return None;
    }
    let address = ((u64::from(get(words[1], 0, 8)) << 32) | u64::from(words[0])) << 8;
    let min_lod = (get(words[1], 8, 12) as f32) / 256.0;
    let format = get(words[1], 20, 9);
    let width = (get(words[1], 30, 2) | (get(words[2], 0, 12) << 2)) + 1;
    let height = get(words[2], 14, 14) + 1;
    let base_level = get(words[3], 12, 4);
    let last_level = get(words[3], 16, 4);
    let (first_mip, mip_count, resource_mips, samples) = if image_type.is_msaa() {
        if base_level != 0 {
            return None;
        }
        (0, 1, 1, 1u32 << last_level)
    } else {
        if last_level < base_level {
            return None;
        }
        (
            base_level,
            last_level - base_level + 1,
            get(words[5], 4, 4) + 1,
            1,
        )
    };
    let last = get(words[4], 0, 13);
    let first_layer = get(words[4], 16, 13);
    let (depth, layer_count) = if image_type == ImageType::Image3D {
        (last + 1, 1)
    } else {
        if last < first_layer {
            return None;
        }
        let layers = last - first_layer + 1;
        if image_type == ImageType::Cube && !layers.is_multiple_of(6) {
            return None;
        }
        (1, layers)
    };
    Some(ImageDesc {
        address,
        format,
        image_type,
        width,
        height,
        depth,
        first_layer,
        layer_count,
        first_mip,
        mip_count,
        resource_mips,
        samples,
        swizzle,
        min_lod,
    })
}

/// Mappe un `DXGI_FORMAT` vers `(code GFX10_FORMAT, swizzle par défaut)`.
pub const fn dxgi_to_gfx10_format(dxgi: u32) -> (u32, Swizzle) {
    match dxgi {
        // R32G32B32A32
        2 | 1 => (77, SWIZZLE_IDENTITY),
        3 => (75, SWIZZLE_IDENTITY),
        4 => (76, SWIZZLE_IDENTITY),
        // R32G32B32
        6 | 5 => (74, SWIZZLE_RGB),
        7 => (72, SWIZZLE_RGB),
        8 => (73, SWIZZLE_RGB),
        // R16G16B16A16
        10 | 9 => (71, SWIZZLE_IDENTITY),
        11 => (65, SWIZZLE_IDENTITY),
        12 => (69, SWIZZLE_IDENTITY),
        13 => (66, SWIZZLE_IDENTITY),
        14 => (70, SWIZZLE_IDENTITY),
        // R32G32
        16 | 15 => (64, SWIZZLE_RG),
        17 => (62, SWIZZLE_RG),
        18 => (63, SWIZZLE_RG),
        // R10G10B10A2
        24 | 23 => (50, SWIZZLE_IDENTITY),
        25 => (54, SWIZZLE_IDENTITY),
        // R11G11B10_FLOAT
        26 => (36, SWIZZLE_RGB),
        // R8G8B8A8
        28 | 27 => (56, SWIZZLE_IDENTITY),
        29 => (130, SWIZZLE_IDENTITY),
        30 => (60, SWIZZLE_IDENTITY),
        31 => (57, SWIZZLE_IDENTITY),
        32 => (61, SWIZZLE_IDENTITY),
        // R16G16
        34 | 33 => (29, SWIZZLE_RG),
        35 => (23, SWIZZLE_RG),
        36 => (27, SWIZZLE_RG),
        37 => (24, SWIZZLE_RG),
        38 => (28, SWIZZLE_RG),
        // R32 / D32_FLOAT
        39..=41 => (22, SWIZZLE_R),
        42 => (20, SWIZZLE_R),
        43 => (21, SWIZZLE_R),
        // D24_UNORM_S8_UINT / R24G8
        44..=46 => (141, SWIZZLE_R),
        // R8G8
        49 | 48 => (14, SWIZZLE_RG),
        50 => (18, SWIZZLE_RG),
        51 => (15, SWIZZLE_RG),
        52 => (19, SWIZZLE_RG),
        // R16 / D16
        54 | 53 => (13, SWIZZLE_R),
        55 | 56 => (7, SWIZZLE_R),
        57 => (11, SWIZZLE_R),
        58 => (8, SWIZZLE_R),
        59 => (12, SWIZZLE_R),
        // R8 / A8
        61 | 60 => (1, SWIZZLE_R),
        62 => (5, SWIZZLE_R),
        63 => (2, SWIZZLE_R),
        64 => (6, SWIZZLE_R),
        65 => (1, [SEL_ZERO, SEL_ZERO, SEL_ZERO, SEL_X]), // A8_UNORM
        // R9G9B9E5
        67 => (132, SWIZZLE_RGB),
        // BC1..BC7
        70 | 71 => (169, SWIZZLE_IDENTITY),
        72 => (170, SWIZZLE_IDENTITY),
        73 | 74 => (171, SWIZZLE_IDENTITY),
        75 => (172, SWIZZLE_IDENTITY),
        76 | 77 => (173, SWIZZLE_IDENTITY),
        78 => (174, SWIZZLE_IDENTITY),
        79 | 80 => (175, SWIZZLE_R),
        81 => (176, SWIZZLE_R),
        82 | 83 => (177, SWIZZLE_RG),
        84 => (178, SWIZZLE_RG),
        // B5G6R5 / B5G5R5A1 / B8G8R8A8 / B8G8R8X8
        85 => (133, SWIZZLE_RGB),
        86 => (134, SWIZZLE_BGRA),
        87 | 90 => (56, SWIZZLE_BGRA),
        88 | 92 => (56, SWIZZLE_BGR),
        91 => (130, SWIZZLE_BGRA),
        93 => (130, SWIZZLE_BGR),
        // BC6H / BC7
        94 | 95 => (179, SWIZZLE_RGB),
        96 => (180, SWIZZLE_RGB),
        97 | 98 => (181, SWIZZLE_IDENTITY),
        99 => (182, SWIZZLE_IDENTITY),
        // D32_FLOAT_S8X24_UINT
        19..=21 => (146, SWIZZLE_R),
        _ => (GFX10_FORMAT_8_8_8_8_UNORM, SWIZZLE_IDENTITY),
    }
}

/// Applique un `D3D12_SHADER_4_COMPONENT_MAPPING` (12 bits) au swizzle de base du format.
pub const fn apply_d3d12_component_mapping(base: Swizzle, mapping: u32) -> Swizzle {
    let mut out = [SEL_ZERO; 4];
    let mut i = 0;
    while i < 4 {
        let sel = (mapping >> (i * 3)) & 7;
        out[i] = match sel {
            0..=3 => base[sel as usize],
            4 => SEL_ZERO,
            _ => SEL_ONE,
        };
        i += 1;
    }
    out
}

// ==================== S# : SQ_IMG_SAMP_WORD0..3 ====================

pub const K_INTEGER_BORDER_PTR: u32 = 0x800;
pub const K_COMPARE_NEVER: u32 = 1 << 12;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SamplerDesc {
    pub address_u: u32,
    pub address_v: u32,
    pub address_w: u32,
    pub mag_filter: u32,
    pub min_filter: u32,
    pub mip_filter: u32,
    pub max_anisotropy: u32,
    pub compare: u32,
    pub min_lod: f32,
    pub max_lod: f32,
    pub lod_bias: f32,
    pub border_color: u32,
    pub border_color_index: u32,
    pub unnormalized: bool,
    pub filter_mode: u32,
    pub integer_border: bool,
    pub compare_never: bool,
}

impl Default for SamplerDesc {
    fn default() -> Self {
        Self {
            address_u: 0,
            address_v: 0,
            address_w: 0,
            mag_filter: 0,
            min_filter: 0,
            mip_filter: 0,
            max_anisotropy: 1,
            compare: 0,
            min_lod: 0.0,
            max_lod: 15.0,
            lod_bias: 0.0,
            border_color: 0,
            border_color_index: 0,
            unnormalized: false,
            filter_mode: 0,
            integer_border: false,
            compare_never: false,
        }
    }
}

/// Convertit un `D3D12_TEXTURE_ADDRESS_MODE` (1..=5) vers `SQ_TEX_CLAMP`.
pub const fn d3d12_address_to_gfx10(mode: u32) -> u32 {
    match mode {
        1 => 0, // WRAP
        2 => 1, // MIRROR
        3 => 2, // CLAMP -> clamp_last_texel
        4 => 6, // BORDER -> clamp_border
        5 => 3, // MIRROR_ONCE -> mirror_once_last_texel
        _ => 2,
    }
}

/// Convertit un `D3D12_COMPARISON_FUNC` (1..=8) vers `SQ_TEX_DEPTH_COMPARE` (0..=7).
pub const fn d3d12_compare_to_gfx10(func: u32) -> u32 {
    if func >= 1 && func <= 8 { func - 1 } else { 0 }
}

/// Construit un `SamplerDesc` GFX10 depuis les champs d'un `D3D12_SAMPLER_DESC`.
#[allow(clippy::too_many_arguments)]
pub fn sampler_from_d3d12(
    filter: u32,
    address_u: u32,
    address_v: u32,
    address_w: u32,
    mip_lod_bias: f32,
    max_anisotropy: u32,
    comparison_func: u32,
    border_color: [f32; 4],
    min_lod: f32,
    max_lod: f32,
) -> SamplerDesc {
    // Codage D3D12_FILTER : bit 0 = mip linear, bit 2 = mag linear, bit 4 = min linear,
    // bit 6 = aniso, bits 7..8 = reduction (0 standard, 1 comparison, 2 min, 3 max).
    let aniso = (filter & 0x40) != 0;
    let min_lin = (filter & 0x10) != 0 || aniso;
    let mag_lin = (filter & 0x04) != 0 || aniso;
    let mip_lin = (filter & 0x01) != 0 || aniso;
    let reduction = (filter >> 7) & 3;
    let filter_mode = match reduction {
        2 => 1, // minimum
        3 => 2, // maximum
        _ => 0, // blend
    };
    let is_comparison = reduction == 1;
    let compare = if is_comparison {
        d3d12_compare_to_gfx10(comparison_func)
    } else {
        0
    };
    let compare_never = is_comparison && compare == 0;
    let border_type = if border_color == [0.0, 0.0, 0.0, 0.0] {
        0 // transparent_black
    } else if border_color == [0.0, 0.0, 0.0, 1.0] {
        1 // opaque_black
    } else if border_color == [1.0, 1.0, 1.0, 1.0] {
        2 // opaque_white
    } else {
        0
    };
    SamplerDesc {
        address_u: d3d12_address_to_gfx10(address_u),
        address_v: d3d12_address_to_gfx10(address_v),
        address_w: d3d12_address_to_gfx10(address_w),
        mag_filter: if aniso {
            3
        } else if mag_lin {
            1
        } else {
            0
        },
        min_filter: if aniso {
            3
        } else if min_lin {
            1
        } else {
            0
        },
        mip_filter: if mip_lin { 2 } else { 1 },
        max_anisotropy: if aniso {
            max_anisotropy.clamp(1, 16)
        } else {
            1
        },
        compare,
        min_lod,
        max_lod,
        lod_bias: mip_lod_bias,
        border_color: border_type,
        border_color_index: 0,
        unnormalized: false,
        filter_mode,
        integer_border: false,
        compare_never,
    }
}

/// Encode un descripteur échantillonneur `S#` (16 octets, `SQ_IMG_SAMP_WORD0..3`).
pub fn encode_sampler(s: &SamplerDesc) -> SamplerWords {
    let aniso_log2 = s.max_anisotropy.max(1).ilog2().min(4);
    let marked = s.integer_border && s.border_color != 3;
    let mut w3 = field(
        if marked {
            K_INTEGER_BORDER_PTR
        } else {
            s.border_color_index
        },
        0,
        12,
    ) | field(s.border_color, 30, 2);
    if s.compare == 0 && s.compare_never {
        w3 |= K_COMPARE_NEVER;
    }
    [
        field(s.address_u, 0, 3)
            | field(s.address_v, 3, 3)
            | field(s.address_w, 6, 3)
            | field(aniso_log2, 9, 3)
            | field(s.compare, 12, 3)
            | field(u32::from(s.unnormalized), 15, 1)
            | field(s.filter_mode, 29, 2),
        field(unsigned_fixed_8(s.min_lod, 0.0, 15.0), 0, 12)
            | field(unsigned_fixed_8(s.max_lod, 0.0, 15.0), 12, 12),
        field(signed_fixed_8(s.lod_bias, -32.0, 31.0), 0, 14)
            | field(s.mag_filter, 20, 2)
            | field(s.min_filter, 22, 2)
            | field(s.mip_filter, 26, 2),
        w3,
    ]
}

/// Décode un descripteur échantillonneur `S#` (16 octets).
pub fn decode_sampler(words: &SamplerWords) -> SamplerDesc {
    let deriv_adjust = get(words[2], 31, 1) != 0;
    let compare = get(words[0], 12, 3);
    let bias_raw = get(words[2], 0, 14);
    let lod_bias = (((bias_raw ^ 0x2000) as i32) - 0x2000) as f32 / 256.0;
    let border_color = get(words[3], 30, 2);
    let mut border_color_index = get(words[3], 0, 12);
    let mut integer_border = false;
    if border_color != 3 && border_color_index == K_INTEGER_BORDER_PTR {
        integer_border = true;
        border_color_index = 0;
    }
    SamplerDesc {
        address_u: get(words[0], 0, 3),
        address_v: get(words[0], 3, 3),
        address_w: get(words[0], 6, 3),
        max_anisotropy: 1u32 << get(words[0], 9, 3).min(4),
        compare,
        compare_never: !deriv_adjust && compare == 0 && (words[3] & K_COMPARE_NEVER) != 0,
        unnormalized: get(words[0], 15, 1) != 0,
        filter_mode: get(words[0], 29, 2),
        min_lod: (get(words[1], 0, 12) as f32) / 256.0,
        max_lod: (get(words[1], 12, 12) as f32) / 256.0,
        lod_bias,
        mag_filter: get(words[2], 20, 2),
        min_filter: get(words[2], 22, 2),
        mip_filter: get(words[2], 26, 2),
        border_color,
        border_color_index,
        integer_border,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_vsharp_roundtrip() {
        let orig = BufferDesc {
            address: 0x0000_7F12_3456_7890,
            stride: 64,
            num_records: 1024,
            format: GFX10_FORMAT_32_UINT,
            swizzle: SWIZZLE_IDENTITY,
            oob_select: OobSelect::Structured,
        };
        let words = encode_buffer(&orig);
        assert_eq!(decode_buffer(&words), Some(orig));
    }

    #[test]
    fn image_tsharp_roundtrip_and_not_confused_with_buffer() {
        let orig = ImageDesc {
            address: 0x0000_7F80_1234_5600,
            format: 169, // BC1_UNORM
            image_type: ImageType::Image2DArray,
            width: 3840,
            height: 2160,
            depth: 1,
            first_layer: 2,
            layer_count: 6,
            first_mip: 1,
            mip_count: 4,
            resource_mips: 8,
            samples: 1,
            swizzle: SWIZZLE_BGRA,
            min_lod: 1.5,
        };
        let words = encode_image(&orig);
        assert!(is_image_descriptor(&words));
        assert!(decode_buffer(&[words[0], words[1], words[2], words[3]]).is_none());
        assert_eq!(decode_image(&words), Some(orig));
    }

    #[test]
    fn sampler_ssharp_roundtrip() {
        let orig = sampler_from_d3d12(
            0x55, // ANISOTROPIC
            1,
            3,
            4,
            -1.25,
            16,
            0,
            [1.0, 1.0, 1.0, 1.0],
            0.0,
            12.5,
        );
        let words = encode_sampler(&orig);
        let decoded = decode_sampler(&words);
        assert_eq!(decoded, orig);
    }
}
