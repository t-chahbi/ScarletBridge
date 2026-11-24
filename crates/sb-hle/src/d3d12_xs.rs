//! Traduction COM native **Direct3D 12 Xbox Series X (`d3d12_xs.dll` / `d3d12.dll`) + `dxgi.dll`**
//! vers notre backend **Vulkan 1.4 (`sb-gpu::VulkanRenderer`)** + traducteur **DXBC → SPIR-V 1.3**.
//!
//! Implémente les interfaces COM exactes de `d3d12.h` / `dxgi1_4.h` (ABI Windows x86-64) :
//! - `D3D12CreateDevice` / `D3D12XBOX_CreateDevice` / `D3D12SerializeRootSignature`
//! - `ID3D12Device` (vtable 44 slots)
//! - `ID3D12CommandQueue`, `ID3D12CommandAllocator`, `ID3D12GraphicsCommandList` (vtable 60 slots)
//! - `ID3D12Resource` (mémoire CPU/GPU unifiée `Map` / `GetGPUVirtualAddress` + readback `CopyResource`)
//! - `ID3D12PipelineState` (compilation native DXBC → SPIR-V → `VkPipeline` sur GPU AMD)
//! - `ID3D12RootSignature`, `ID3D12DescriptorHeap`, `ID3D12Fence`, `ID3DBlob`
//! - `CreateDXGIFactory1` / `CreateDXGIFactory2`, `IDXGIFactory4`, `IDXGISwapChain3`

#![allow(non_snake_case, unused_variables)]

use core::ffi::c_void;
use core::mem::{offset_of, size_of};
use std::collections::HashMap;
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use parking_lot::Mutex;
use sb_common::abi::{E_FAIL, E_INVALIDARG, E_NOTIMPL, E_POINTER, E_UNEXPECTED, HResult, S_OK};
use sb_common::{ffi_guard, sb_debug, sb_info, sb_trace, sb_warn};
use sb_gpu::ash::vk;
use sb_gpu::{
    GpuContext, GraphicsPipelineConfig, RenderCommand, SSBO_BYTE_SIZE, Sdl3Window,
    VertexAttrConfig, VulkanRenderer, dxgi_format_byte_size, dxgi_format_to_vk,
};

use crate::com::{
    ComHeader, Guid, VtblSlot, com_add_ref, com_query_interface_list, com_release, vslot,
};
use crate::sync;

const CHANNEL: &str = "d3d12_xs";

// ==================== GUIDs D3D12 & DXGI ====================

pub const IID_ID3D12DEVICE: Guid = Guid::new(
    0x1898_19F1,
    0x1DB6,
    0x4B57,
    [0xBE, 0x54, 0x18, 0x21, 0x33, 0x9B, 0x85, 0xF7],
);
pub const IID_ID3D12COMMANDQUEUE: Guid = Guid::new(
    0x0EC8_70A6,
    0x5D7E,
    0x4C22,
    [0x8C, 0xFC, 0x5B, 0xAA, 0xE0, 0x76, 0x16, 0xED],
);
pub const IID_ID3D12COMMANDALLOCATOR: Guid = Guid::new(
    0x6102_DEE4,
    0xAF59,
    0x4B09,
    [0xB9, 0x99, 0xB4, 0x4D, 0x73, 0xF0, 0x9B, 0x24],
);
pub const IID_ID3D12GRAPHICSCOMMANDLIST: Guid = Guid::new(
    0x5B16_0D0F,
    0xAC1B,
    0x4185,
    [0x8B, 0xA8, 0xB3, 0xAE, 0x42, 0xA5, 0xA4, 0x55],
);
pub const IID_ID3D12RESOURCE: Guid = Guid::new(
    0x6964_42BE,
    0xA72E,
    0x4059,
    [0xBC, 0x79, 0x5B, 0x5C, 0x98, 0x04, 0x0F, 0xAD],
);
pub const IID_ID3D12PIPELINESTATE: Guid = Guid::new(
    0x765A_30F3,
    0xF624,
    0x4C6F,
    [0xA8, 0x28, 0xAC, 0xE9, 0x48, 0x62, 0x24, 0x45],
);
pub const IID_ID3D12ROOTSIGNATURE: Guid = Guid::new(
    0xC54A_6B66,
    0x72DF,
    0x4EE8,
    [0x8B, 0xE5, 0xA9, 0x46, 0xA1, 0x42, 0x92, 0x14],
);
pub const IID_ID3D12DESCRIPTORHEAP: Guid = Guid::new(
    0x8EFB_471D,
    0x616C,
    0x4F49,
    [0x90, 0xF7, 0x12, 0x7B, 0xB7, 0x63, 0xFA, 0x51],
);
pub const IID_ID3D12FENCE: Guid = Guid::new(
    0x0A75_3DCF,
    0xC4D8,
    0x4B91,
    [0xAD, 0xF6, 0xBE, 0x5A, 0x60, 0xD9, 0x5A, 0x76],
);
pub const IID_ID3D12HEAP: Guid = Guid::new(
    0x6B3B_2502,
    0x6E51,
    0x45B3,
    [0x90, 0xEE, 0x98, 0x84, 0x26, 0x5E, 0x8D, 0xF3],
);
pub const IID_IDXGIFACTORY4: Guid = Guid::new(
    0x1BC6_EA02,
    0xEF36,
    0x464F,
    [0xBF, 0x0C, 0x21, 0xCA, 0x39, 0xE5, 0x16, 0x8A],
);
pub const IID_IDXGISWAPCHAIN3: Guid = Guid::new(
    0x94D9_9BDB,
    0xF1F8,
    0x4AB0,
    [0xB2, 0x36, 0x7D, 0xA0, 0x17, 0x0E, 0xDA, 0xB1],
);

// ==================== Structs ABI Windows x86-64 (`d3d12.h` / `dxgi1_2.h`) ====================

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DxgiSampleDesc {
    pub count: u32,
    pub quality: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct D3D12ShaderBytecode {
    pub shader_bytecode: *const u8,
    pub bytecode_length: usize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct D3D12StreamOutputDesc {
    pub so_declaration: *const c_void,
    pub num_entries: u32,
    pub _pad0: u32,
    pub buffer_strides: *const u32,
    pub num_strides: u32,
    pub rasterized_stream: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12RenderTargetBlendDesc {
    pub blend_enable: i32,
    pub logic_op_enable: i32,
    pub src_blend: u32,
    pub dest_blend: u32,
    pub blend_op: u32,
    pub src_blend_alpha: u32,
    pub dest_blend_alpha: u32,
    pub blend_op_alpha: u32,
    pub logic_op: u32,
    pub render_target_write_mask: u8,
    pub _pad: [u8; 3],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12BlendDesc {
    pub alpha_to_coverage_enable: i32,
    pub independent_blend_enable: i32,
    pub render_target: [D3D12RenderTargetBlendDesc; 8],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12RasterizerDesc {
    pub fill_mode: u32,
    pub cull_mode: u32,
    pub front_counter_clockwise: i32,
    pub depth_bias: i32,
    pub depth_bias_clamp: f32,
    pub slope_scaled_depth_bias: f32,
    pub depth_clip_enable: i32,
    pub multisample_enable: i32,
    pub antialiased_line_enable: i32,
    pub forced_sample_count: u32,
    pub conservative_raster: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12DepthStencilOpDesc {
    pub stencil_fail_op: u32,
    pub stencil_depth_fail_op: u32,
    pub stencil_pass_op: u32,
    pub stencil_func: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12DepthStencilDesc {
    pub depth_enable: i32,
    pub depth_write_mask: u32,
    pub depth_func: u32,
    pub stencil_enable: i32,
    pub stencil_read_mask: u8,
    pub stencil_write_mask: u8,
    pub _pad: [u8; 2],
    pub front_face: D3D12DepthStencilOpDesc,
    pub back_face: D3D12DepthStencilOpDesc,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct D3D12InputElementDesc {
    pub semantic_name: *const u8,
    pub semantic_index: u32,
    pub format: u32,
    pub input_slot: u32,
    pub aligned_byte_offset: u32,
    pub input_slot_class: u32,
    pub instance_data_step_rate: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct D3D12InputLayoutDesc {
    pub input_element_descs: *const D3D12InputElementDesc,
    pub num_elements: u32,
    pub _pad: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct D3D12CachedPipelineState {
    pub cached_blob: *const c_void,
    pub cached_blob_size_in_bytes: usize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct D3D12GraphicsPipelineStateDesc {
    pub root_signature: *mut c_void,
    pub vs: D3D12ShaderBytecode,
    pub ps: D3D12ShaderBytecode,
    pub ds: D3D12ShaderBytecode,
    pub hs: D3D12ShaderBytecode,
    pub gs: D3D12ShaderBytecode,
    pub stream_output: D3D12StreamOutputDesc,
    pub blend_state: D3D12BlendDesc,
    pub sample_mask: u32,
    pub rasterizer_state: D3D12RasterizerDesc,
    pub depth_stencil_state: D3D12DepthStencilDesc,
    pub input_layout: D3D12InputLayoutDesc,
    pub ib_strip_cut_value: u32,
    pub primitive_topology_type: u32,
    pub num_render_targets: u32,
    pub rtv_formats: [u32; 8],
    pub dsv_format: u32,
    pub sample_desc: DxgiSampleDesc,
    pub node_mask: u32,
    pub cached_pso: D3D12CachedPipelineState,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct D3D12ComputePipelineStateDesc {
    pub root_signature: *mut c_void,
    pub cs: D3D12ShaderBytecode,
    pub node_mask: u32,
    pub _pad0: u32,
    pub cached_pso: D3D12CachedPipelineState,
    pub flags: u32,
    pub _pad1: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12HeapDesc {
    pub size_in_bytes: u64,
    pub properties: D3D12HeapProperties,
    pub _pad0: u32,
    pub alignment: u64,
    pub flags: u32,
    pub _pad1: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12RootParameter {
    pub parameter_type: u32,
    pub _pad0: u32,
    /// `ShaderRegister` pour `CBV`/`SRV`/`UAV`/`32BIT_CONSTANTS`, ou `NumDescriptorRanges` pour `TABLE`.
    pub data0: u32,
    pub data1: u32,
    pub data2: u64,
    pub shader_visibility: u32,
    pub _pad1: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct D3D12RootSignatureDesc {
    pub num_parameters: u32,
    pub _pad0: u32,
    pub parameters: *const D3D12RootParameter,
    pub num_static_samplers: u32,
    pub _pad1: u32,
    pub static_samplers: *const c_void,
    pub flags: u32,
    pub _pad2: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12CommandQueueDesc {
    pub queue_type: u32,
    pub priority: i32,
    pub flags: u32,
    pub node_mask: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12DescriptorHeapDesc {
    pub heap_type: u32,
    pub num_descriptors: u32,
    pub flags: u32,
    pub node_mask: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12HeapProperties {
    pub heap_type: u32,
    pub cpu_page_property: u32,
    pub memory_pool_preference: u32,
    pub creation_node_mask: u32,
    pub visible_node_mask: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12ResourceDesc {
    pub dimension: u32,
    pub _pad0: u32,
    pub alignment: u64,
    pub width: u64,
    pub height: u32,
    pub depth_or_array_size: u16,
    pub mip_levels: u16,
    pub format: u32,
    pub sample_desc: DxgiSampleDesc,
    pub layout: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12VertexBufferView {
    pub buffer_location: u64,
    pub size_in_bytes: u32,
    pub stride_in_bytes: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12IndexBufferView {
    pub buffer_location: u64,
    pub size_in_bytes: u32,
    pub format: u32, // 57 = DXGI_FORMAT_R16_UINT, 42 = DXGI_FORMAT_R32_UINT
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12SubresourceFootprint {
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub row_pitch: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12PlacedSubresourceFootprint {
    pub offset: u64,
    pub footprint: D3D12SubresourceFootprint,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct D3D12TextureCopyLocation {
    pub resource: *mut c_void,
    pub copy_type: u32, // 0 = SUBRESOURCE_INDEX, 1 = PLACED_FOOTPRINT
    pub _pad: u32,
    pub placed_footprint: D3D12PlacedSubresourceFootprint,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12ConstantBufferViewDesc {
    pub buffer_location: u64,
    pub size_in_bytes: u32,
    pub _pad: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12Viewport {
    pub top_left_x: f32,
    pub top_left_y: f32,
    pub width: f32,
    pub height: f32,
    pub min_depth: f32,
    pub max_depth: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DxgiSwapChainDesc1 {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub stereo: i32,
    pub sample_desc: DxgiSampleDesc,
    pub buffer_usage: u32,
    pub buffer_count: u32,
    pub scaling: u32,
    pub swap_effect: u32,
    pub alpha_mode: u32,
    pub flags: u32,
}

// Assertions ABI compile-time strictes contre `d3d12.h` x86-64 (Règle 3 AGENTS.md).
const _: () = {
    assert!(size_of::<D3D12ShaderBytecode>() == 16);
    assert!(size_of::<D3D12StreamOutputDesc>() == 32);
    assert!(size_of::<D3D12RenderTargetBlendDesc>() == 40);
    assert!(size_of::<D3D12BlendDesc>() == 328);
    assert!(size_of::<D3D12RasterizerDesc>() == 44);
    assert!(size_of::<D3D12DepthStencilDesc>() == 52);
    assert!(size_of::<D3D12InputElementDesc>() == 32);
    assert!(size_of::<D3D12InputLayoutDesc>() == 16);
    assert!(size_of::<D3D12GraphicsPipelineStateDesc>() == 656);
    assert!(offset_of!(D3D12GraphicsPipelineStateDesc, vs) == 8);
    assert!(offset_of!(D3D12GraphicsPipelineStateDesc, ps) == 24);
    assert!(offset_of!(D3D12GraphicsPipelineStateDesc, blend_state) == 120);
    assert!(offset_of!(D3D12GraphicsPipelineStateDesc, sample_mask) == 448);
    assert!(offset_of!(D3D12GraphicsPipelineStateDesc, rasterizer_state) == 452);
    assert!(offset_of!(D3D12GraphicsPipelineStateDesc, depth_stencil_state) == 496);
    assert!(offset_of!(D3D12GraphicsPipelineStateDesc, input_layout) == 552);
    assert!(offset_of!(D3D12GraphicsPipelineStateDesc, num_render_targets) == 576);
    assert!(offset_of!(D3D12GraphicsPipelineStateDesc, rtv_formats) == 580);
    assert!(offset_of!(D3D12GraphicsPipelineStateDesc, cached_pso) == 632);
    assert!(offset_of!(D3D12GraphicsPipelineStateDesc, flags) == 648);
    assert!(size_of::<D3D12ResourceDesc>() == 56);
    assert!(offset_of!(D3D12ResourceDesc, width) == 16);
    assert!(offset_of!(D3D12ResourceDesc, height) == 24);
    assert!(offset_of!(D3D12ResourceDesc, format) == 32);
    assert!(size_of::<D3D12VertexBufferView>() == 16);
    assert!(size_of::<D3D12IndexBufferView>() == 16);
    assert!(size_of::<D3D12SubresourceFootprint>() == 20);
    assert!(size_of::<D3D12PlacedSubresourceFootprint>() == 32);
    assert!(size_of::<D3D12TextureCopyLocation>() == 48);
    assert!(offset_of!(D3D12TextureCopyLocation, placed_footprint) == 16);
    assert!(size_of::<D3D12ConstantBufferViewDesc>() == 16);
    assert!(size_of::<D3D12Viewport>() == 24);
    assert!(size_of::<DxgiSwapChainDesc1>() == 48);
    assert!(size_of::<D3D12ComputePipelineStateDesc>() == 56);
    assert!(offset_of!(D3D12ComputePipelineStateDesc, cs) == 8);
    assert!(offset_of!(D3D12ComputePipelineStateDesc, cached_pso) == 32);
    assert!(offset_of!(D3D12ComputePipelineStateDesc, flags) == 48);
    assert!(size_of::<D3D12HeapDesc>() == 48);
    assert!(offset_of!(D3D12HeapDesc, properties) == 8);
    assert!(offset_of!(D3D12HeapDesc, alignment) == 32);
    assert!(offset_of!(D3D12HeapDesc, flags) == 40);
    assert!(size_of::<D3D12RootParameter>() == 32);
    assert!(offset_of!(D3D12RootParameter, data0) == 8);
    assert!(offset_of!(D3D12RootParameter, shader_visibility) == 24);
    assert!(size_of::<D3D12RootSignatureDesc>() == 40);
    assert!(offset_of!(D3D12RootSignatureDesc, parameters) == 8);
    assert!(offset_of!(D3D12RootSignatureDesc, flags) == 32);
};

// ==================== Exports DLL (`d3d12_xs.dll` & `dxgi.dll`) ====================

pub fn d3d12_xs_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("D3D12CreateDevice", D3D12CreateDevice);
    reg!("D3D12XboxCreateDevice", D3D12XboxCreateDevice);
    reg!("D3D12XBOX_CreateDevice", D3D12XboxCreateDevice);
    reg!("D3D12ConfigureVirtualMemoryX", D3D12ConfigureVirtualMemoryX);
    reg!("D3D12GetDebugInterface", D3D12GetDebugInterface);
    reg!("D3D12SerializeRootSignature", D3D12SerializeRootSignature);
    reg!(
        "D3D12SerializeVersionedRootSignature",
        D3D12SerializeVersionedRootSignature
    );
    m
}

pub fn dxgi_exports() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    macro_rules! reg {
        ($name:expr, $f:ident) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("CreateDXGIFactory", CreateDXGIFactory1);
    reg!("CreateDXGIFactory1", CreateDXGIFactory1);
    reg!("CreateDXGIFactory2", CreateDXGIFactory2);
    m
}

// ==================== État partagé GPU Vulkan ====================

#[derive(Debug, Clone, Copy)]
enum DescriptorBinding {
    SrvTexture { resource_ptr: usize },
    Cbv { addr: u64, size: u32 },
    UavBuffer { addr: u64, size: u32 },
}

struct SharedD3D12State {
    renderer: Mutex<VulkanRenderer>,
    // Garde la fenêtre SDL3 vivante pour la durée du swapchain si non-headless.
    sdl_window: Mutex<Option<Sdl3Window>>,
    // Table des descripteurs SRV/CBV indexée par handle CPU/GPU D3D12.
    descriptors: Mutex<HashMap<usize, DescriptorBinding>>,
}

fn is_headless() -> bool {
    cfg!(test) || std::env::var("SB_HEADLESS").ok().as_deref() == Some("1")
}

// ==================== Objets COM D3D12 ====================

extern "win64" fn noop_ok() -> HResult {
    S_OK
}

// ==================== Slots de réserve des vtables D3D12 / DXGI ====================
//
// Un titre appelle des méthodes bien au-delà de l'interface de base : `ID3D12Device1`…`14`,
// `ID3D12GraphicsCommandList1`…`10`, et les méthodes propres à la console (`PresentX`,
// `AllocateUploadMemory`, ...). Lire au-delà d'une vtable statique reviendrait à exécuter un
// pointeur pris dans la donnée voisine. Toutes les vtables ont donc `VT_PAD_LEN` slots : les slots
// non implémentés journalisent « objet::slot N » (une fois) et renvoient `E_NOTIMPL`.

/// Longueur uniforme des vtables D3D12 / DXGI (slots 0..`VT_PAD_LEN`).
const VT_PAD_LEN: usize = 192;

/// Nom de l'objet D3D12 dont `this` est une instance, d'après son pointeur de vtable.
fn d3d_object_name(this: *mut c_void) -> &'static str {
    if this.is_null() {
        return "objet NULL";
    }
    // SAFETY: lecture du premier mot (pointeur de vtable) d'un objet COM du bridge ; `this` vient
    // du titre et peut être invalide, mais la lecture ne sert qu'à comparer des adresses.
    let vtbl = unsafe { ptr::read_unaligned(this.cast::<usize>()) };
    let known: [(usize, &'static str); 12] = [
        (
            (&raw const D3D12_ROOTSIG_VTBL) as usize,
            "ID3D12RootSignature",
        ),
        ((&raw const D3D12_PSO_VTBL) as usize, "ID3D12PipelineState"),
        (
            (&raw const D3D12_DESC_HEAP_VTBL) as usize,
            "ID3D12DescriptorHeap",
        ),
        ((&raw const D3D12_HEAP_VTBL) as usize, "ID3D12Heap"),
        ((&raw const D3D12_RESOURCE_VTBL) as usize, "ID3D12Resource"),
        ((&raw const D3D12_FENCE_VTBL) as usize, "ID3D12Fence"),
        (
            (&raw const D3D12_CMD_ALLOC_VTBL) as usize,
            "ID3D12CommandAllocator",
        ),
        (
            (&raw const D3D12_CMDLIST_VTBL) as usize,
            "ID3D12GraphicsCommandList",
        ),
        ((&raw const D3D12_QUEUE_VTBL) as usize, "ID3D12CommandQueue"),
        ((&raw const D3D12_DEVICE_VTBL) as usize, "ID3D12Device"),
        ((&raw const DXGI_SC3_VTBL) as usize, "IDXGISwapChain3"),
        ((&raw const DXGI_FACTORY4_VTBL) as usize, "IDXGIFactory4"),
    ];
    known
        .iter()
        .find(|(addr, _)| *addr == vtbl)
        .map_or("objet D3D12/DXGI", |(_, name)| name)
}

/// Slot `N` non implémenté : journalise (une fois par objet et par slot) puis `E_NOTIMPL`.
extern "win64" fn d3d_unimpl_slot<const N: usize>(
    this: *mut c_void,
    a2: usize,
    a3: usize,
    a4: usize,
) -> HResult {
    static SEEN: std::sync::OnceLock<Mutex<std::collections::HashSet<(&'static str, usize)>>> =
        std::sync::OnceLock::new();
    let name = d3d_object_name(this);
    if SEEN
        .get_or_init(|| Mutex::new(std::collections::HashSet::new()))
        .lock()
        .insert((name, N))
    {
        sb_warn!(
            CHANNEL,
            "{name}::slot {N} non implémenté appelé (this={this:p}, a2={}, a3={}, a4={}) -> E_NOTIMPL",
            crate::com::describe_guest_arg(a2),
            crate::com::describe_guest_arg(a3),
            crate::com::describe_guest_arg(a4)
        );
    }
    E_NOTIMPL
}

/// Slots de réserve : `d3d_unimpl_slot::<0>` … `d3d_unimpl_slot::<191>`.
const UNIMPL_SLOTS: [VtblSlot; VT_PAD_LEN] = [
    vslot(d3d_unimpl_slot::<0> as *const c_void),
    vslot(d3d_unimpl_slot::<1> as *const c_void),
    vslot(d3d_unimpl_slot::<2> as *const c_void),
    vslot(d3d_unimpl_slot::<3> as *const c_void),
    vslot(d3d_unimpl_slot::<4> as *const c_void),
    vslot(d3d_unimpl_slot::<5> as *const c_void),
    vslot(d3d_unimpl_slot::<6> as *const c_void),
    vslot(d3d_unimpl_slot::<7> as *const c_void),
    vslot(d3d_unimpl_slot::<8> as *const c_void),
    vslot(d3d_unimpl_slot::<9> as *const c_void),
    vslot(d3d_unimpl_slot::<10> as *const c_void),
    vslot(d3d_unimpl_slot::<11> as *const c_void),
    vslot(d3d_unimpl_slot::<12> as *const c_void),
    vslot(d3d_unimpl_slot::<13> as *const c_void),
    vslot(d3d_unimpl_slot::<14> as *const c_void),
    vslot(d3d_unimpl_slot::<15> as *const c_void),
    vslot(d3d_unimpl_slot::<16> as *const c_void),
    vslot(d3d_unimpl_slot::<17> as *const c_void),
    vslot(d3d_unimpl_slot::<18> as *const c_void),
    vslot(d3d_unimpl_slot::<19> as *const c_void),
    vslot(d3d_unimpl_slot::<20> as *const c_void),
    vslot(d3d_unimpl_slot::<21> as *const c_void),
    vslot(d3d_unimpl_slot::<22> as *const c_void),
    vslot(d3d_unimpl_slot::<23> as *const c_void),
    vslot(d3d_unimpl_slot::<24> as *const c_void),
    vslot(d3d_unimpl_slot::<25> as *const c_void),
    vslot(d3d_unimpl_slot::<26> as *const c_void),
    vslot(d3d_unimpl_slot::<27> as *const c_void),
    vslot(d3d_unimpl_slot::<28> as *const c_void),
    vslot(d3d_unimpl_slot::<29> as *const c_void),
    vslot(d3d_unimpl_slot::<30> as *const c_void),
    vslot(d3d_unimpl_slot::<31> as *const c_void),
    vslot(d3d_unimpl_slot::<32> as *const c_void),
    vslot(d3d_unimpl_slot::<33> as *const c_void),
    vslot(d3d_unimpl_slot::<34> as *const c_void),
    vslot(d3d_unimpl_slot::<35> as *const c_void),
    vslot(d3d_unimpl_slot::<36> as *const c_void),
    vslot(d3d_unimpl_slot::<37> as *const c_void),
    vslot(d3d_unimpl_slot::<38> as *const c_void),
    vslot(d3d_unimpl_slot::<39> as *const c_void),
    vslot(d3d_unimpl_slot::<40> as *const c_void),
    vslot(d3d_unimpl_slot::<41> as *const c_void),
    vslot(d3d_unimpl_slot::<42> as *const c_void),
    vslot(d3d_unimpl_slot::<43> as *const c_void),
    vslot(d3d_unimpl_slot::<44> as *const c_void),
    vslot(d3d_unimpl_slot::<45> as *const c_void),
    vslot(d3d_unimpl_slot::<46> as *const c_void),
    vslot(d3d_unimpl_slot::<47> as *const c_void),
    vslot(d3d_unimpl_slot::<48> as *const c_void),
    vslot(d3d_unimpl_slot::<49> as *const c_void),
    vslot(d3d_unimpl_slot::<50> as *const c_void),
    vslot(d3d_unimpl_slot::<51> as *const c_void),
    vslot(d3d_unimpl_slot::<52> as *const c_void),
    vslot(d3d_unimpl_slot::<53> as *const c_void),
    vslot(d3d_unimpl_slot::<54> as *const c_void),
    vslot(d3d_unimpl_slot::<55> as *const c_void),
    vslot(d3d_unimpl_slot::<56> as *const c_void),
    vslot(d3d_unimpl_slot::<57> as *const c_void),
    vslot(d3d_unimpl_slot::<58> as *const c_void),
    vslot(d3d_unimpl_slot::<59> as *const c_void),
    vslot(d3d_unimpl_slot::<60> as *const c_void),
    vslot(d3d_unimpl_slot::<61> as *const c_void),
    vslot(d3d_unimpl_slot::<62> as *const c_void),
    vslot(d3d_unimpl_slot::<63> as *const c_void),
    vslot(d3d_unimpl_slot::<64> as *const c_void),
    vslot(d3d_unimpl_slot::<65> as *const c_void),
    vslot(d3d_unimpl_slot::<66> as *const c_void),
    vslot(d3d_unimpl_slot::<67> as *const c_void),
    vslot(d3d_unimpl_slot::<68> as *const c_void),
    vslot(d3d_unimpl_slot::<69> as *const c_void),
    vslot(d3d_unimpl_slot::<70> as *const c_void),
    vslot(d3d_unimpl_slot::<71> as *const c_void),
    vslot(d3d_unimpl_slot::<72> as *const c_void),
    vslot(d3d_unimpl_slot::<73> as *const c_void),
    vslot(d3d_unimpl_slot::<74> as *const c_void),
    vslot(d3d_unimpl_slot::<75> as *const c_void),
    vslot(d3d_unimpl_slot::<76> as *const c_void),
    vslot(d3d_unimpl_slot::<77> as *const c_void),
    vslot(d3d_unimpl_slot::<78> as *const c_void),
    vslot(d3d_unimpl_slot::<79> as *const c_void),
    vslot(d3d_unimpl_slot::<80> as *const c_void),
    vslot(d3d_unimpl_slot::<81> as *const c_void),
    vslot(d3d_unimpl_slot::<82> as *const c_void),
    vslot(d3d_unimpl_slot::<83> as *const c_void),
    vslot(d3d_unimpl_slot::<84> as *const c_void),
    vslot(d3d_unimpl_slot::<85> as *const c_void),
    vslot(d3d_unimpl_slot::<86> as *const c_void),
    vslot(d3d_unimpl_slot::<87> as *const c_void),
    vslot(d3d_unimpl_slot::<88> as *const c_void),
    vslot(d3d_unimpl_slot::<89> as *const c_void),
    vslot(d3d_unimpl_slot::<90> as *const c_void),
    vslot(d3d_unimpl_slot::<91> as *const c_void),
    vslot(d3d_unimpl_slot::<92> as *const c_void),
    vslot(d3d_unimpl_slot::<93> as *const c_void),
    vslot(d3d_unimpl_slot::<94> as *const c_void),
    vslot(d3d_unimpl_slot::<95> as *const c_void),
    vslot(d3d_unimpl_slot::<96> as *const c_void),
    vslot(d3d_unimpl_slot::<97> as *const c_void),
    vslot(d3d_unimpl_slot::<98> as *const c_void),
    vslot(d3d_unimpl_slot::<99> as *const c_void),
    vslot(d3d_unimpl_slot::<100> as *const c_void),
    vslot(d3d_unimpl_slot::<101> as *const c_void),
    vslot(d3d_unimpl_slot::<102> as *const c_void),
    vslot(d3d_unimpl_slot::<103> as *const c_void),
    vslot(d3d_unimpl_slot::<104> as *const c_void),
    vslot(d3d_unimpl_slot::<105> as *const c_void),
    vslot(d3d_unimpl_slot::<106> as *const c_void),
    vslot(d3d_unimpl_slot::<107> as *const c_void),
    vslot(d3d_unimpl_slot::<108> as *const c_void),
    vslot(d3d_unimpl_slot::<109> as *const c_void),
    vslot(d3d_unimpl_slot::<110> as *const c_void),
    vslot(d3d_unimpl_slot::<111> as *const c_void),
    vslot(d3d_unimpl_slot::<112> as *const c_void),
    vslot(d3d_unimpl_slot::<113> as *const c_void),
    vslot(d3d_unimpl_slot::<114> as *const c_void),
    vslot(d3d_unimpl_slot::<115> as *const c_void),
    vslot(d3d_unimpl_slot::<116> as *const c_void),
    vslot(d3d_unimpl_slot::<117> as *const c_void),
    vslot(d3d_unimpl_slot::<118> as *const c_void),
    vslot(d3d_unimpl_slot::<119> as *const c_void),
    vslot(d3d_unimpl_slot::<120> as *const c_void),
    vslot(d3d_unimpl_slot::<121> as *const c_void),
    vslot(d3d_unimpl_slot::<122> as *const c_void),
    vslot(d3d_unimpl_slot::<123> as *const c_void),
    vslot(d3d_unimpl_slot::<124> as *const c_void),
    vslot(d3d_unimpl_slot::<125> as *const c_void),
    vslot(d3d_unimpl_slot::<126> as *const c_void),
    vslot(d3d_unimpl_slot::<127> as *const c_void),
    vslot(d3d_unimpl_slot::<128> as *const c_void),
    vslot(d3d_unimpl_slot::<129> as *const c_void),
    vslot(d3d_unimpl_slot::<130> as *const c_void),
    vslot(d3d_unimpl_slot::<131> as *const c_void),
    vslot(d3d_unimpl_slot::<132> as *const c_void),
    vslot(d3d_unimpl_slot::<133> as *const c_void),
    vslot(d3d_unimpl_slot::<134> as *const c_void),
    vslot(d3d_unimpl_slot::<135> as *const c_void),
    vslot(d3d_unimpl_slot::<136> as *const c_void),
    vslot(d3d_unimpl_slot::<137> as *const c_void),
    vslot(d3d_unimpl_slot::<138> as *const c_void),
    vslot(d3d_unimpl_slot::<139> as *const c_void),
    vslot(d3d_unimpl_slot::<140> as *const c_void),
    vslot(d3d_unimpl_slot::<141> as *const c_void),
    vslot(d3d_unimpl_slot::<142> as *const c_void),
    vslot(d3d_unimpl_slot::<143> as *const c_void),
    vslot(d3d_unimpl_slot::<144> as *const c_void),
    vslot(d3d_unimpl_slot::<145> as *const c_void),
    vslot(d3d_unimpl_slot::<146> as *const c_void),
    vslot(d3d_unimpl_slot::<147> as *const c_void),
    vslot(d3d_unimpl_slot::<148> as *const c_void),
    vslot(d3d_unimpl_slot::<149> as *const c_void),
    vslot(d3d_unimpl_slot::<150> as *const c_void),
    vslot(d3d_unimpl_slot::<151> as *const c_void),
    vslot(d3d_unimpl_slot::<152> as *const c_void),
    vslot(d3d_unimpl_slot::<153> as *const c_void),
    vslot(d3d_unimpl_slot::<154> as *const c_void),
    vslot(d3d_unimpl_slot::<155> as *const c_void),
    vslot(d3d_unimpl_slot::<156> as *const c_void),
    vslot(d3d_unimpl_slot::<157> as *const c_void),
    vslot(d3d_unimpl_slot::<158> as *const c_void),
    vslot(d3d_unimpl_slot::<159> as *const c_void),
    vslot(d3d_unimpl_slot::<160> as *const c_void),
    vslot(d3d_unimpl_slot::<161> as *const c_void),
    vslot(d3d_unimpl_slot::<162> as *const c_void),
    vslot(d3d_unimpl_slot::<163> as *const c_void),
    vslot(d3d_unimpl_slot::<164> as *const c_void),
    vslot(d3d_unimpl_slot::<165> as *const c_void),
    vslot(d3d_unimpl_slot::<166> as *const c_void),
    vslot(d3d_unimpl_slot::<167> as *const c_void),
    vslot(d3d_unimpl_slot::<168> as *const c_void),
    vslot(d3d_unimpl_slot::<169> as *const c_void),
    vslot(d3d_unimpl_slot::<170> as *const c_void),
    vslot(d3d_unimpl_slot::<171> as *const c_void),
    vslot(d3d_unimpl_slot::<172> as *const c_void),
    vslot(d3d_unimpl_slot::<173> as *const c_void),
    vslot(d3d_unimpl_slot::<174> as *const c_void),
    vslot(d3d_unimpl_slot::<175> as *const c_void),
    vslot(d3d_unimpl_slot::<176> as *const c_void),
    vslot(d3d_unimpl_slot::<177> as *const c_void),
    vslot(d3d_unimpl_slot::<178> as *const c_void),
    vslot(d3d_unimpl_slot::<179> as *const c_void),
    vslot(d3d_unimpl_slot::<180> as *const c_void),
    vslot(d3d_unimpl_slot::<181> as *const c_void),
    vslot(d3d_unimpl_slot::<182> as *const c_void),
    vslot(d3d_unimpl_slot::<183> as *const c_void),
    vslot(d3d_unimpl_slot::<184> as *const c_void),
    vslot(d3d_unimpl_slot::<185> as *const c_void),
    vslot(d3d_unimpl_slot::<186> as *const c_void),
    vslot(d3d_unimpl_slot::<187> as *const c_void),
    vslot(d3d_unimpl_slot::<188> as *const c_void),
    vslot(d3d_unimpl_slot::<189> as *const c_void),
    vslot(d3d_unimpl_slot::<190> as *const c_void),
    vslot(d3d_unimpl_slot::<191> as *const c_void),
];

// --- ID3DBlob ---

#[repr(C)]
struct D3DBlobVtbl {
    slots: [VtblSlot; 5],
}

#[repr(C)]
struct D3DBlobObject {
    header: ComHeader,
    data: Vec<u8>,
}

extern "win64" fn blob_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("ID3DBlob", this, riid, ppv, &[])
}

extern "win64" fn blob_get_ptr(this: *mut D3DBlobObject) -> *mut c_void {
    if this.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `this` est un `D3DBlobObject` valide.
    unsafe { (*this).data.as_mut_ptr().cast() }
}

extern "win64" fn blob_get_size(this: *mut D3DBlobObject) -> usize {
    if this.is_null() {
        return 0;
    }
    // SAFETY: `this` est un `D3DBlobObject` valide.
    unsafe { (*this).data.len() }
}

const fn build_blob_vtbl() -> D3DBlobVtbl {
    D3DBlobVtbl {
        slots: [
            vslot(blob_qi as *const c_void),
            vslot(com_add_ref as *const c_void),
            vslot(com_release as *const c_void),
            vslot(blob_get_ptr as *const c_void),
            vslot(blob_get_size as *const c_void),
        ],
    }
}

static D3D_BLOB_VTBL: D3DBlobVtbl = build_blob_vtbl();

// --- ID3D12RootSignature ---

#[repr(C)]
struct D3D12RootSigVtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct D3D12RootSigObject {
    header: ComHeader,
    /// Table `root_param_idx -> shader_register` décodée depuis `D3D12SerializeRootSignature`.
    param_registers: Vec<u32>,
}

extern "win64" fn rootsig_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "ID3D12RootSignature",
        this,
        riid,
        ppv,
        &[IID_ID3D12ROOTSIGNATURE],
    )
}

const fn build_rootsig_vtbl() -> D3D12RootSigVtbl {
    let mut slots = UNIMPL_SLOTS;
    let mut pad = 0;
    while pad < 8 {
        slots[pad] = vslot(noop_ok as *const c_void);
        pad += 1;
    }
    slots[0] = vslot(rootsig_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    D3D12RootSigVtbl { slots }
}

static D3D12_ROOTSIG_VTBL: D3D12RootSigVtbl = build_rootsig_vtbl();

// --- ID3D12PipelineState ---

#[repr(C)]
struct D3D12PsoVtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct D3D12PsoObject {
    header: ComHeader,
    pipeline_id: usize,
    is_compute: bool,
}

extern "win64" fn pso_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "ID3D12PipelineState",
        this,
        riid,
        ppv,
        &[IID_ID3D12PIPELINESTATE],
    )
}

const fn build_pso_vtbl() -> D3D12PsoVtbl {
    let mut slots = UNIMPL_SLOTS;
    let mut pad = 0;
    while pad < 9 {
        slots[pad] = vslot(noop_ok as *const c_void);
        pad += 1;
    }
    slots[0] = vslot(pso_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    D3D12PsoVtbl { slots }
}

static D3D12_PSO_VTBL: D3D12PsoVtbl = build_pso_vtbl();

// --- ID3D12DescriptorHeap ---

#[repr(C)]
struct D3D12DescHeapVtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct D3D12DescHeapObject {
    header: ComHeader,
    desc: D3D12DescriptorHeapDesc,
    base_handle: usize,
}

extern "win64" fn heap_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "ID3D12DescriptorHeap",
        this,
        riid,
        ppv,
        &[IID_ID3D12DESCRIPTORHEAP],
    )
}

extern "win64" fn heap_get_cpu_start(this: *mut D3D12DescHeapObject) -> usize {
    if this.is_null() {
        return 0;
    }
    // SAFETY: `this` est un `D3D12DescHeapObject` valide.
    unsafe { (*this).base_handle }
}

extern "win64" fn heap_get_gpu_start(this: *mut D3D12DescHeapObject) -> u64 {
    if this.is_null() {
        return 0;
    }
    // SAFETY: `this` est un `D3D12DescHeapObject` valide.
    unsafe { (*this).base_handle as u64 }
}

const fn build_heap_vtbl() -> D3D12DescHeapVtbl {
    let mut slots = UNIMPL_SLOTS;
    let mut pad = 0;
    while pad < 12 {
        slots[pad] = vslot(noop_ok as *const c_void);
        pad += 1;
    }
    slots[0] = vslot(heap_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    // Slots 9 & 10 : ABI C / x86_64-pc-windows-gnu (retour `SIZE_T` / `UINT64` dans RAX).
    slots[9] = vslot(heap_get_cpu_start as *const c_void);
    slots[10] = vslot(heap_get_gpu_start as *const c_void);
    D3D12DescHeapVtbl { slots }
}

static D3D12_DESC_HEAP_VTBL: D3D12DescHeapVtbl = build_heap_vtbl();
static NEXT_DESC_HANDLE: AtomicU64 = AtomicU64::new(0x0001_0000);

// --- ID3D12Heap (mémoire unifiée pour CreatePlacedResource / aliasing AAA) ---

#[repr(C)]
struct D3D12HeapVtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct D3D12HeapObject {
    header: ComHeader,
    desc: D3D12HeapDesc,
    backing: Arc<Mutex<Vec<u8>>>,
    base_addr: usize,
    size: usize,
}

extern "win64" fn d3dheap_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("ID3D12Heap", this, riid, ppv, &[IID_ID3D12HEAP])
}

extern "win64" fn d3dheap_get_desc(
    this: *mut D3D12HeapObject,
    out_desc: *mut D3D12HeapDesc,
) -> *mut D3D12HeapDesc {
    if !this.is_null() && !out_desc.is_null() {
        // SAFETY: `this` et `out_desc` vérifiés non-nuls (ABI Win64 retour struct > 8o en RDX).
        unsafe {
            *out_desc = (*this).desc;
        }
    }
    out_desc
}

const fn build_d3dheap_vtbl() -> D3D12HeapVtbl {
    let mut slots = UNIMPL_SLOTS;
    let mut pad = 0;
    while pad < 10 {
        slots[pad] = vslot(noop_ok as *const c_void);
        pad += 1;
    }
    slots[0] = vslot(d3dheap_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[8] = vslot(d3dheap_get_desc as *const c_void);
    D3D12HeapVtbl { slots }
}

static D3D12_HEAP_VTBL: D3D12HeapVtbl = build_d3dheap_vtbl();

// --- ID3D12Resource ---

#[repr(C)]
struct D3D12ResourceVtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct D3D12ResourceObject {
    header: ComHeader,
    desc: D3D12ResourceDesc,
    is_backbuffer: bool,
    /// Tampon mémoire partagé (propre à la ressource ou aliasé sur un `ID3D12Heap`).
    backing: Arc<Mutex<Vec<u8>>>,
    heap_offset: usize,
    byte_len: usize,
    /// Adresse fixe (`backing.as_ptr() + heap_offset`), jamais réallouée pour garantir la
    /// cohérence CPU/GPU unifiée entre `Map()` et `GetGPUVirtualAddress()`.
    fixed_addr: usize,
    /// Identifiant de texture GPU Vulkan (`usize::MAX` si pas encore téléversée).
    gpu_texture_id: std::sync::atomic::AtomicUsize,
}

extern "win64" fn res_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("ID3D12Resource", this, riid, ppv, &[IID_ID3D12RESOURCE])
}

extern "win64" fn res_map(
    this: *mut D3D12ResourceObject,
    _subresource: u32,
    _read_range: *const c_void,
    out_data: *mut *mut c_void,
) -> HResult {
    if this.is_null() {
        return E_INVALIDARG;
    }
    if !out_data.is_null() {
        // SAFETY: `this` et `out_data` vérifiés non-nuls.
        unsafe {
            *out_data = (*this).fixed_addr as *mut c_void;
        }
    }
    S_OK
}

extern "win64" fn res_unmap(
    _this: *mut D3D12ResourceObject,
    _subresource: u32,
    _written_range: *const c_void,
) {
}

extern "win64" fn res_get_desc(
    this: *mut D3D12ResourceObject,
    out_desc: *mut D3D12ResourceDesc,
) -> *mut D3D12ResourceDesc {
    if !this.is_null() && !out_desc.is_null() {
        // SAFETY: `this` et `out_desc` vérifiés non-nuls (ABI Win64 retour struct > 8o en RDX).
        unsafe {
            *out_desc = (*this).desc;
        }
    }
    out_desc
}

extern "win64" fn res_get_gpu_va(this: *mut D3D12ResourceObject) -> u64 {
    if this.is_null() {
        return 0;
    }
    // SAFETY: `this` est un `D3D12ResourceObject` valide.
    unsafe { (*this).fixed_addr as u64 }
}

pub(crate) fn texture_row_layout(width: u32, height: u32, dxgi_format: u32) -> (usize, usize) {
    let w = width.max(1) as usize;
    let h = height.max(1) as usize;
    match dxgi_format {
        // BC1 / BC4 : 8 octets par bloc 4×4
        71 | 72 | 80 => (w.div_ceil(4) * 8, h.div_ceil(4)),
        // BC2 / BC3 / BC5 / BC6H / BC7 : 16 octets par bloc 4×4
        74 | 77 | 78 | 83 | 95 | 98 | 99 => (w.div_ceil(4) * 16, h.div_ceil(4)),
        // HDR R16G16B16A16_FLOAT
        10 => (w * 8, h),
        // HDR R32G32B32A32_FLOAT
        2 => (w * 16, h),
        // RGBA8 / BGRA8 par défaut
        _ => (w * 4, h),
    }
}

extern "win64" fn res_write_to_subresource(
    this: *mut D3D12ResourceObject,
    _dst_subresource: u32,
    _dst_box: *const c_void,
    src_data: *const u8,
    src_row_pitch: u32,
    _src_depth_pitch: u32,
) -> HResult {
    if this.is_null() || src_data.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: `this` et `src_data` vérifiés non-nuls.
    let res = unsafe { &*this };
    let (row_bytes, num_rows) = texture_row_layout(
        (res.desc.width as u32).max(1),
        res.desc.height.max(1),
        res.desc.format,
    );
    let pitch = (src_row_pitch as usize).max(row_bytes);
    let mut dst = res.backing.lock();
    for y in 0..num_rows {
        let d_off = res.heap_offset + y * row_bytes;
        if d_off + row_bytes > dst.len() {
            break;
        }
        // SAFETY: `src_data` contient `num_rows` lignes de `pitch` octets.
        let src_row = unsafe { std::slice::from_raw_parts(src_data.add(y * pitch), row_bytes) };
        dst[d_off..d_off + row_bytes].copy_from_slice(src_row);
    }
    res.gpu_texture_id.store(usize::MAX, Ordering::Release);
    S_OK
}

const fn build_res_vtbl() -> D3D12ResourceVtbl {
    let mut slots = UNIMPL_SLOTS;
    let mut pad = 0;
    while pad < 16 {
        slots[pad] = vslot(noop_ok as *const c_void);
        pad += 1;
    }
    slots[0] = vslot(res_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[8] = vslot(res_map as *const c_void);
    slots[9] = vslot(res_unmap as *const c_void);
    slots[10] = vslot(res_get_desc as *const c_void);
    slots[11] = vslot(res_get_gpu_va as *const c_void);
    slots[12] = vslot(res_write_to_subresource as *const c_void);
    D3D12ResourceVtbl { slots }
}

static D3D12_RESOURCE_VTBL: D3D12ResourceVtbl = build_res_vtbl();

static ALLOCATED_BUFFER_SIZES: std::sync::LazyLock<Mutex<HashMap<usize, usize>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

fn compute_resource_byte_len(desc: D3D12ResourceDesc) -> usize {
    if desc.dimension == 1 {
        // D3D12_RESOURCE_DIMENSION_BUFFER
        (desc.width as usize).max(64)
    } else {
        let (row_bytes, num_rows) =
            texture_row_layout((desc.width as u32).max(1), desc.height.max(1), desc.format);
        row_bytes.saturating_mul(num_rows).max(64)
    }
}

fn alloc_d3d12_resource(desc: D3D12ResourceDesc, is_backbuffer: bool) -> *mut D3D12ResourceObject {
    let byte_len = compute_resource_byte_len(desc);
    let mut backing = vec![0u8; byte_len];
    let fixed_addr = backing.as_mut_ptr() as usize;
    ALLOCATED_BUFFER_SIZES.lock().insert(fixed_addr, byte_len);
    let obj = Box::new(D3D12ResourceObject {
        header: ComHeader::new((&raw const D3D12_RESOURCE_VTBL).cast()),
        desc,
        is_backbuffer,
        backing: Arc::new(Mutex::new(backing)),
        heap_offset: 0,
        byte_len,
        fixed_addr,
        gpu_texture_id: std::sync::atomic::AtomicUsize::new(usize::MAX),
    });
    Box::into_raw(obj)
}

fn alloc_placed_d3d12_resource(
    heap: &D3D12HeapObject,
    heap_offset: u64,
    desc: D3D12ResourceDesc,
) -> Result<*mut D3D12ResourceObject, HResult> {
    let offset = heap_offset as usize;
    let byte_len = compute_resource_byte_len(desc);
    if offset.saturating_add(byte_len) > heap.size {
        sb_warn!(
            CHANNEL,
            "CreatePlacedResource hors limites du heap (offset={offset}, len={byte_len}, heap={})",
            heap.size
        );
        return Err(E_INVALIDARG);
    }
    let fixed_addr = heap.base_addr + offset;
    ALLOCATED_BUFFER_SIZES.lock().insert(fixed_addr, byte_len);
    let obj = Box::new(D3D12ResourceObject {
        header: ComHeader::new((&raw const D3D12_RESOURCE_VTBL).cast()),
        desc,
        is_backbuffer: false,
        backing: Arc::clone(&heap.backing),
        heap_offset: offset,
        byte_len,
        fixed_addr,
        gpu_texture_id: std::sync::atomic::AtomicUsize::new(usize::MAX),
    });
    Ok(Box::into_raw(obj))
}

// --- ID3D12Fence ---

#[repr(C)]
struct D3D12FenceVtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct D3D12FenceObject {
    header: ComHeader,
    completed: AtomicU64,
}

extern "win64" fn fence_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("ID3D12Fence", this, riid, ppv, &[IID_ID3D12FENCE])
}

extern "win64" fn fence_get_completed(this: *mut D3D12FenceObject) -> u64 {
    if this.is_null() {
        return 0;
    }
    // SAFETY: `this` est un `D3D12FenceObject` valide.
    unsafe { (*this).completed.load(Ordering::Acquire) }
}

extern "win64" fn fence_set_event(
    this: *mut D3D12FenceObject,
    value: u64,
    event_handle: usize,
) -> HResult {
    if this.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: `this` est un `D3D12FenceObject` valide.
    let current = unsafe { (*this).completed.load(Ordering::Acquire) };
    if current >= value && event_handle != 0 {
        sync::SetEvent(event_handle);
    }
    S_OK
}

extern "win64" fn fence_signal(this: *mut D3D12FenceObject, value: u64) -> HResult {
    if this.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: `this` est un `D3D12FenceObject` valide.
    unsafe {
        (*this).completed.store(value, Ordering::Release);
    }
    S_OK
}

const fn build_fence_vtbl() -> D3D12FenceVtbl {
    let mut slots = UNIMPL_SLOTS;
    let mut pad = 0;
    while pad < 12 {
        slots[pad] = vslot(noop_ok as *const c_void);
        pad += 1;
    }
    slots[0] = vslot(fence_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[8] = vslot(fence_get_completed as *const c_void);
    slots[9] = vslot(fence_set_event as *const c_void);
    slots[10] = vslot(fence_signal as *const c_void);
    D3D12FenceVtbl { slots }
}

static D3D12_FENCE_VTBL: D3D12FenceVtbl = build_fence_vtbl();

// --- ID3D12CommandAllocator ---

#[repr(C)]
struct D3D12CmdAllocVtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct D3D12CmdAllocObject {
    header: ComHeader,
}

extern "win64" fn alloc_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "ID3D12CommandAllocator",
        this,
        riid,
        ppv,
        &[IID_ID3D12COMMANDALLOCATOR],
    )
}

const fn build_alloc_vtbl() -> D3D12CmdAllocVtbl {
    let mut slots = UNIMPL_SLOTS;
    let mut pad = 0;
    while pad < 10 {
        slots[pad] = vslot(noop_ok as *const c_void);
        pad += 1;
    }
    slots[0] = vslot(alloc_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[8] = vslot(noop_ok as *const c_void); // Reset
    D3D12CmdAllocVtbl { slots }
}

static D3D12_CMD_ALLOC_VTBL: D3D12CmdAllocVtbl = build_alloc_vtbl();

// --- ID3D12GraphicsCommandList ---

enum RecordedOp {
    Render(RenderCommand),
    CopyBackbufferToResource {
        dst: usize,
    },
    CopyBufferResource {
        dst: usize,
        src: usize,
    },
    UploadTexture {
        dst: usize,
        width: u32,
        height: u32,
        format: u32,
        data: Vec<u8>,
    },
    BindRootCbv {
        slot: u32,
        addr: u64,
    },
    BindComputeStorage {
        slot: u32,
        addr: u64,
    },
    ComputeDispatch {
        x: u32,
        y: u32,
        z: u32,
    },
    BindDescriptorTable {
        handle: usize,
        slot: u32,
    },
}

#[repr(C)]
struct D3D12CmdListVtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct D3D12CmdListObject {
    header: ComHeader,
    ops: Mutex<Vec<RecordedOp>>,
    closed: Mutex<bool>,
    graphics_root_regs: Mutex<Vec<u32>>,
    compute_root_regs: Mutex<Vec<u32>>,
}

impl D3D12CmdListObject {
    fn resolve_gfx_slot(&self, root_param_idx: u32) -> u32 {
        let regs = self.graphics_root_regs.lock();
        regs.get(root_param_idx as usize)
            .copied()
            .unwrap_or(root_param_idx)
            % 4
    }

    fn resolve_compute_slot(&self, root_param_idx: u32) -> u32 {
        let regs = self.compute_root_regs.lock();
        regs.get(root_param_idx as usize)
            .copied()
            .unwrap_or(root_param_idx)
            % 4
    }

    fn resolve_table_slot(&self, is_compute: bool, root_param_idx: u32) -> u32 {
        let regs = if is_compute {
            self.compute_root_regs.lock()
        } else {
            self.graphics_root_regs.lock()
        };
        regs.get(root_param_idx as usize).copied().unwrap_or(0) % 4
    }
}

extern "win64" fn cmdlist_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "ID3D12GraphicsCommandList",
        this,
        riid,
        ppv,
        &[IID_ID3D12GRAPHICSCOMMANDLIST],
    )
}

extern "win64" fn cmdlist_close(this: *mut D3D12CmdListObject) -> HResult {
    if this.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    *cl.closed.lock() = true;
    S_OK
}

extern "win64" fn cmdlist_reset(
    this: *mut D3D12CmdListObject,
    _allocator: *mut c_void,
    initial_state: *mut D3D12PsoObject,
) -> HResult {
    if this.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    let mut ops = cl.ops.lock();
    ops.clear();
    cl.graphics_root_regs.lock().clear();
    cl.compute_root_regs.lock().clear();
    if !initial_state.is_null() {
        // SAFETY: `initial_state` est un `D3D12PsoObject` valide.
        let pso = unsafe { &*initial_state };
        let rc = if pso.is_compute {
            RenderCommand::SetComputePipeline {
                pipeline_id: pso.pipeline_id,
            }
        } else {
            RenderCommand::SetPipeline {
                pipeline_id: pso.pipeline_id,
            }
        };
        ops.push(RecordedOp::Render(rc));
    }
    *cl.closed.lock() = false;
    S_OK
}

extern "win64" fn cmdlist_draw_instanced(
    this: *mut D3D12CmdListObject,
    vertex_count: u32,
    instance_count: u32,
    start_vertex: u32,
    start_instance: u32,
) {
    if this.is_null() {
        return;
    }
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    cl.ops.lock().push(RecordedOp::Render(RenderCommand::Draw {
        vertex_count,
        instance_count: instance_count.max(1),
        first_vertex: start_vertex,
        first_instance: start_instance,
    }));
}

extern "win64" fn cmdlist_draw_indexed_instanced(
    this: *mut D3D12CmdListObject,
    index_count: u32,
    instance_count: u32,
    start_index: u32,
    base_vertex: i32,
    start_instance: u32,
) {
    if this.is_null() {
        return;
    }
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    cl.ops
        .lock()
        .push(RecordedOp::Render(RenderCommand::DrawIndexed {
            index_count,
            instance_count: instance_count.max(1),
            first_index: start_index,
            vertex_offset: base_vertex,
            first_instance: start_instance,
        }));
}

extern "win64" fn cmdlist_dispatch(
    this: *mut D3D12CmdListObject,
    thread_group_x: u32,
    thread_group_y: u32,
    thread_group_z: u32,
) {
    if this.is_null() {
        return;
    }
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    cl.ops.lock().push(RecordedOp::ComputeDispatch {
        x: thread_group_x.max(1),
        y: thread_group_y.max(1),
        z: thread_group_z.max(1),
    });
}

extern "win64" fn cmdlist_copy_texture_region(
    this: *mut D3D12CmdListObject,
    dst: *const D3D12TextureCopyLocation,
    _dst_x: u32,
    _dst_y: u32,
    _dst_z: u32,
    src: *const D3D12TextureCopyLocation,
    _src_box: *const c_void,
) {
    if this.is_null() || dst.is_null() || src.is_null() {
        return;
    }
    // SAFETY: `this`, `dst`, `src` vérifiés non-nuls.
    let cl = unsafe { &*this };
    let d_loc = unsafe { *dst };
    let s_loc = unsafe { *src };
    if d_loc.resource.is_null() || s_loc.resource.is_null() {
        return;
    }
    let dst_res = unsafe { &*(d_loc.resource.cast::<D3D12ResourceObject>()) };
    let src_res = unsafe { &*(s_loc.resource.cast::<D3D12ResourceObject>()) };

    let fmt = if s_loc.copy_type == 1 && s_loc.placed_footprint.footprint.format != 0 {
        s_loc.placed_footprint.footprint.format
    } else if dst_res.desc.format != 0 {
        dst_res.desc.format
    } else {
        28
    };

    let (w, h, row_pitch, offset) = if s_loc.copy_type == 1 {
        let fp = s_loc.placed_footprint.footprint;
        let (rb, _) = texture_row_layout(fp.width.max(1), fp.height.max(1), fmt);
        (
            fp.width.max(1),
            fp.height.max(1),
            (fp.row_pitch as usize).max(rb),
            s_loc.placed_footprint.offset as usize,
        )
    } else {
        let w = (dst_res.desc.width as u32).max(1);
        let h = dst_res.desc.height.max(1);
        let (rb, _) = texture_row_layout(w, h, fmt);
        (w, h, rb, 0)
    };

    let (row_bytes, num_rows) = texture_row_layout(w, h, fmt);
    let src_buf = src_res.backing.lock();
    let mut data = vec![0u8; row_bytes * num_rows];
    for y in 0..num_rows {
        let s_off = src_res.heap_offset + offset + y * row_pitch;
        if s_off + row_bytes <= src_buf.len() {
            data[y * row_bytes..(y + 1) * row_bytes]
                .copy_from_slice(&src_buf[s_off..s_off + row_bytes]);
        }
    }
    drop(src_buf);
    {
        let mut dst_buf = dst_res.backing.lock();
        let copy_len = data.len().min(dst_res.byte_len);
        if dst_res.heap_offset + copy_len <= dst_buf.len() {
            dst_buf[dst_res.heap_offset..dst_res.heap_offset + copy_len]
                .copy_from_slice(&data[..copy_len]);
        }
    }
    cl.ops.lock().push(RecordedOp::UploadTexture {
        dst: d_loc.resource as usize,
        width: w,
        height: h,
        format: fmt,
        data,
    });
}

extern "win64" fn cmdlist_copy_resource(
    this: *mut D3D12CmdListObject,
    dst: *mut D3D12ResourceObject,
    src: *mut D3D12ResourceObject,
) {
    if this.is_null() || dst.is_null() || src.is_null() {
        return;
    }
    // SAFETY: `this`, `dst`, `src` vérifiés non-nuls.
    let cl = unsafe { &*this };
    let src_is_bb = unsafe { (*src).is_backbuffer };
    if src_is_bb {
        cl.ops
            .lock()
            .push(RecordedOp::CopyBackbufferToResource { dst: dst as usize });
    } else {
        // Exécution différée sur la timeline GPU (`ExecuteCommandLists`), après tout `Dispatch` !
        cl.ops.lock().push(RecordedOp::CopyBufferResource {
            dst: dst as usize,
            src: src as usize,
        });
    }
}

extern "win64" fn cmdlist_rs_set_viewports(
    this: *mut D3D12CmdListObject,
    num_viewports: u32,
    viewports: *const D3D12Viewport,
) {
    if this.is_null() || num_viewports == 0 || viewports.is_null() {
        return;
    }
    // SAFETY: `this` et `viewports` vérifiés non-nuls.
    let cl = unsafe { &*this };
    let vp = unsafe { *viewports };
    cl.ops
        .lock()
        .push(RecordedOp::Render(RenderCommand::SetViewport {
            x: vp.top_left_x,
            y: vp.top_left_y,
            width: vp.width,
            height: vp.height,
        }));
}

extern "win64" fn cmdlist_rs_set_scissor_rects(
    this: *mut D3D12CmdListObject,
    num_rects: u32,
    rects: *const D3D12Rect,
) {
    if this.is_null() || num_rects == 0 || rects.is_null() {
        return;
    }
    // SAFETY: `this` et `rects` vérifiés non-nuls.
    let cl = unsafe { &*this };
    let r = unsafe { *rects };
    let w = (r.right - r.left).max(1) as u32;
    let h = (r.bottom - r.top).max(1) as u32;
    cl.ops
        .lock()
        .push(RecordedOp::Render(RenderCommand::SetScissor {
            x: r.left.max(0),
            y: r.top.max(0),
            width: w,
            height: h,
        }));
}

extern "win64" fn cmdlist_set_pipeline_state(
    this: *mut D3D12CmdListObject,
    pso: *mut D3D12PsoObject,
) {
    if this.is_null() || pso.is_null() {
        return;
    }
    // SAFETY: `this` et `pso` vérifiés non-nuls.
    let cl = unsafe { &*this };
    let p = unsafe { &*pso };
    let rc = if p.is_compute {
        RenderCommand::SetComputePipeline {
            pipeline_id: p.pipeline_id,
        }
    } else {
        RenderCommand::SetPipeline {
            pipeline_id: p.pipeline_id,
        }
    };
    cl.ops.lock().push(RecordedOp::Render(rc));
}

extern "win64" fn cmdlist_set_compute_root_sig(
    this: *mut D3D12CmdListObject,
    root_sig: *mut D3D12RootSigObject,
) {
    if this.is_null() || root_sig.is_null() {
        return;
    }
    // SAFETY: `this` et `root_sig` vérifiés non-nuls.
    let cl = unsafe { &*this };
    let rs = unsafe { &*root_sig };
    *cl.compute_root_regs.lock() = rs.param_registers.clone();
}

extern "win64" fn cmdlist_set_graphics_root_sig(
    this: *mut D3D12CmdListObject,
    root_sig: *mut D3D12RootSigObject,
) {
    if this.is_null() || root_sig.is_null() {
        return;
    }
    // SAFETY: `this` et `root_sig` vérifiés non-nuls.
    let cl = unsafe { &*this };
    let rs = unsafe { &*root_sig };
    *cl.graphics_root_regs.lock() = rs.param_registers.clone();
}

extern "win64" fn cmdlist_set_compute_root_desc_table(
    this: *mut D3D12CmdListObject,
    root_param_idx: u32,
    base_gpu_handle: u64,
) {
    if this.is_null() || base_gpu_handle == 0 {
        return;
    }
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    let slot = cl.resolve_table_slot(true, root_param_idx);
    cl.ops.lock().push(RecordedOp::BindDescriptorTable {
        handle: base_gpu_handle as usize,
        slot,
    });
}

extern "win64" fn cmdlist_set_graphics_root_desc_table(
    this: *mut D3D12CmdListObject,
    root_param_idx: u32,
    base_gpu_handle: u64,
) {
    if this.is_null() || base_gpu_handle == 0 {
        return;
    }
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    let slot = cl.resolve_table_slot(false, root_param_idx);
    cl.ops.lock().push(RecordedOp::BindDescriptorTable {
        handle: base_gpu_handle as usize,
        slot,
    });
}

extern "win64" fn cmdlist_set_compute_root_32bit_constants(
    this: *mut D3D12CmdListObject,
    root_param_idx: u32,
    num_32bit_values: u32,
    src_data: *const u32,
    _dest_offset: u32,
) {
    if this.is_null() || src_data.is_null() || num_32bit_values == 0 {
        return;
    }
    // SAFETY: `src_data` pointe vers `num_32bit_values` mots 32 bits.
    let bytes = unsafe {
        std::slice::from_raw_parts(src_data.cast::<u8>(), (num_32bit_values as usize) * 4).to_vec()
    };
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    let slot = cl.resolve_compute_slot(root_param_idx);
    cl.ops
        .lock()
        .push(RecordedOp::Render(RenderCommand::UpdateConstantBuffer {
            slot,
            data: bytes,
        }));
}

extern "win64" fn cmdlist_set_graphics_root_32bit_constants(
    this: *mut D3D12CmdListObject,
    root_param_idx: u32,
    num_32bit_values: u32,
    src_data: *const u32,
    _dest_offset: u32,
) {
    if this.is_null() || src_data.is_null() || num_32bit_values == 0 {
        return;
    }
    // SAFETY: `src_data` pointe vers `num_32bit_values` mots 32 bits.
    let bytes = unsafe {
        std::slice::from_raw_parts(src_data.cast::<u8>(), (num_32bit_values as usize) * 4).to_vec()
    };
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    let slot = cl.resolve_gfx_slot(root_param_idx);
    cl.ops
        .lock()
        .push(RecordedOp::Render(RenderCommand::UpdateConstantBuffer {
            slot,
            data: bytes,
        }));
}

extern "win64" fn cmdlist_set_compute_root_cbv(
    this: *mut D3D12CmdListObject,
    root_param_idx: u32,
    buffer_location: u64,
) {
    if this.is_null() || buffer_location == 0 {
        return;
    }
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    let slot = cl.resolve_compute_slot(root_param_idx);
    cl.ops.lock().push(RecordedOp::BindRootCbv {
        slot,
        addr: buffer_location,
    });
}

extern "win64" fn cmdlist_set_graphics_root_cbv(
    this: *mut D3D12CmdListObject,
    root_param_idx: u32,
    buffer_location: u64,
) {
    if this.is_null() || buffer_location == 0 {
        return;
    }
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    let slot = cl.resolve_gfx_slot(root_param_idx);
    cl.ops.lock().push(RecordedOp::BindRootCbv {
        slot,
        addr: buffer_location,
    });
}

extern "win64" fn cmdlist_set_compute_root_storage(
    this: *mut D3D12CmdListObject,
    root_param_idx: u32,
    buffer_location: u64,
) {
    if this.is_null() || buffer_location == 0 {
        return;
    }
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    let slot = cl.resolve_compute_slot(root_param_idx);
    cl.ops.lock().push(RecordedOp::BindComputeStorage {
        slot,
        addr: buffer_location,
    });
}

extern "win64" fn cmdlist_ia_set_index_buffer(
    this: *mut D3D12CmdListObject,
    view: *const D3D12IndexBufferView,
) {
    if this.is_null() || view.is_null() {
        return;
    }
    // SAFETY: `view` valide.
    let v = unsafe { *view };
    if v.buffer_location == 0 || v.size_in_bytes == 0 {
        return;
    }
    // SAFETY: `v.buffer_location` pointe vers `v.size_in_bytes` octets d'indices.
    let data = unsafe {
        std::slice::from_raw_parts(v.buffer_location as *const u8, v.size_in_bytes as usize)
            .to_vec()
    };
    let index32 = v.format == 42; // DXGI_FORMAT_R32_UINT = 42, R16_UINT = 57
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    cl.ops
        .lock()
        .push(RecordedOp::Render(RenderCommand::SetIndexBuffer {
            data,
            index32,
        }));
}

extern "win64" fn cmdlist_ia_set_vertex_buffers(
    this: *mut D3D12CmdListObject,
    _start_slot: u32,
    num_views: u32,
    views: *const D3D12VertexBufferView,
) {
    if this.is_null() || num_views == 0 || views.is_null() {
        return;
    }
    // SAFETY: `views` pointe vers au moins `num_views` `D3D12VertexBufferView`.
    let view = unsafe { *views };
    if view.buffer_location == 0 || view.size_in_bytes == 0 {
        return;
    }
    let bytes = unsafe {
        std::slice::from_raw_parts(
            view.buffer_location as *const u8,
            view.size_in_bytes as usize,
        )
        .to_vec()
    };
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    cl.ops
        .lock()
        .push(RecordedOp::Render(RenderCommand::SetVertexBuffer {
            data: bytes,
            stride: view.stride_in_bytes,
        }));
}

extern "win64" fn cmdlist_clear_dsv(
    this: *mut D3D12CmdListObject,
    _dsv_handle: usize,
    _clear_flags: u32,
    depth: f32,
    _stencil: u8,
    _num_rects: u32,
    _rects: *const D3D12Rect,
) {
    if this.is_null() {
        return;
    }
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    cl.ops
        .lock()
        .push(RecordedOp::Render(RenderCommand::ClearDepth { depth }));
}

extern "win64" fn cmdlist_clear_rtv(
    this: *mut D3D12CmdListObject,
    _rtv_handle: usize,
    color_rgba: *const [f32; 4],
    _num_rects: u32,
    _rects: *const D3D12Rect,
) {
    if this.is_null() || color_rgba.is_null() {
        return;
    }
    // SAFETY: `this` et `color_rgba` vérifiés non-nuls.
    let cl = unsafe { &*this };
    let rgba = unsafe { *color_rgba };
    cl.ops
        .lock()
        .push(RecordedOp::Render(RenderCommand::Clear { rgba }));
}

extern "win64" fn cmdlist_noop_void() {}

const fn build_cmdlist_vtbl() -> D3D12CmdListVtbl {
    let mut slots = UNIMPL_SLOTS;
    let mut pad = 0;
    while pad < 60 {
        slots[pad] = vslot(cmdlist_noop_void as *const c_void);
        pad += 1;
    }
    slots[0] = vslot(cmdlist_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[9] = vslot(cmdlist_close as *const c_void);
    slots[10] = vslot(cmdlist_reset as *const c_void);
    slots[12] = vslot(cmdlist_draw_instanced as *const c_void);
    slots[13] = vslot(cmdlist_draw_indexed_instanced as *const c_void);
    slots[14] = vslot(cmdlist_dispatch as *const c_void);
    slots[16] = vslot(cmdlist_copy_texture_region as *const c_void);
    slots[17] = vslot(cmdlist_copy_resource as *const c_void);
    slots[21] = vslot(cmdlist_rs_set_viewports as *const c_void);
    slots[22] = vslot(cmdlist_rs_set_scissor_rects as *const c_void);
    slots[25] = vslot(cmdlist_set_pipeline_state as *const c_void);
    slots[29] = vslot(cmdlist_set_compute_root_sig as *const c_void);
    slots[30] = vslot(cmdlist_set_graphics_root_sig as *const c_void);
    slots[31] = vslot(cmdlist_set_compute_root_desc_table as *const c_void);
    slots[32] = vslot(cmdlist_set_graphics_root_desc_table as *const c_void);
    slots[35] = vslot(cmdlist_set_compute_root_32bit_constants as *const c_void);
    slots[36] = vslot(cmdlist_set_graphics_root_32bit_constants as *const c_void);
    slots[37] = vslot(cmdlist_set_compute_root_cbv as *const c_void);
    slots[38] = vslot(cmdlist_set_graphics_root_cbv as *const c_void);
    slots[39] = vslot(cmdlist_set_compute_root_storage as *const c_void);
    slots[41] = vslot(cmdlist_set_compute_root_storage as *const c_void);
    slots[43] = vslot(cmdlist_ia_set_index_buffer as *const c_void);
    slots[44] = vslot(cmdlist_ia_set_vertex_buffers as *const c_void);
    slots[47] = vslot(cmdlist_clear_dsv as *const c_void);
    slots[48] = vslot(cmdlist_clear_rtv as *const c_void);
    D3D12CmdListVtbl { slots }
}

static D3D12_CMDLIST_VTBL: D3D12CmdListVtbl = build_cmdlist_vtbl();

// --- ID3D12CommandQueue ---

#[repr(C)]
struct D3D12QueueVtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct D3D12QueueObject {
    header: ComHeader,
    shared: Arc<SharedD3D12State>,
}

extern "win64" fn queue_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "ID3D12CommandQueue",
        this,
        riid,
        ppv,
        &[IID_ID3D12COMMANDQUEUE],
    )
}

extern "win64" fn queue_execute_command_lists(
    this: *mut D3D12QueueObject,
    num_lists: u32,
    lists: *const *mut D3D12CmdListObject,
) {
    ffi_guard!(CHANNEL, "ExecuteCommandLists", (), {
        if this.is_null() || num_lists == 0 || lists.is_null() {
            return;
        }
        // SAFETY: `this` et `lists[..num_lists]` valides.
        let q = unsafe { &*this };
        let list_ptrs = unsafe { std::slice::from_raw_parts(lists, num_lists as usize) };

        let mut render_cmds = Vec::new();
        let mut renderer = q.shared.renderer.lock();
        let descs = q.shared.descriptors.lock().clone();
        let mut active_compute_pid: Option<usize> = None;
        let mut bound_uav_addrs: [Option<(usize, usize)>; 4] = [None; 4];

        let flush_render = |renderer: &mut VulkanRenderer, cmds: &mut Vec<RenderCommand>| {
            if cmds.is_empty() {
                return;
            }
            if let Err(err) = renderer.execute_commands(cmds) {
                sb_warn!(CHANNEL, "ExecuteCommandLists -> Vulkan erreur: {err}");
            } else {
                sb_debug!(
                    CHANNEL,
                    "ExecuteCommandLists: {} commande(s) exécutée(s) sur GPU Vulkan",
                    cmds.len()
                );
            }
            cmds.clear();
        };

        let ensure_tex_uploaded = |renderer: &mut VulkanRenderer, res_ptr: usize| -> usize {
            let obj = unsafe { &*(res_ptr as *const D3D12ResourceObject) };
            let cur = obj.gpu_texture_id.load(Ordering::Acquire);
            if cur != usize::MAX {
                return cur;
            }
            let w = (obj.desc.width as u32).max(1);
            let h = obj.desc.height.max(1);
            let fmt = if obj.desc.format == 0 {
                28
            } else {
                obj.desc.format
            };
            let data = {
                let guard = obj.backing.lock();
                let end = (obj.heap_offset + obj.byte_len).min(guard.len());
                guard[obj.heap_offset..end].to_vec()
            };
            match renderer.create_texture_dxgi(w, h, fmt, &data) {
                Ok(id) => {
                    obj.gpu_texture_id.store(id, Ordering::Release);
                    id
                }
                Err(err) => {
                    sb_warn!(CHANNEL, "upload texture GPU échoué: {err}");
                    0
                }
            }
        };

        for &cl_ptr in list_ptrs {
            if cl_ptr.is_null() {
                continue;
            }
            // SAFETY: `cl_ptr` est un `D3D12CmdListObject` valide.
            let cl = unsafe { &*cl_ptr };
            let ops = cl.ops.lock();
            for op in ops.iter() {
                match op {
                    RecordedOp::Render(rc) => {
                        if let RenderCommand::SetComputePipeline { pipeline_id } = *rc {
                            active_compute_pid = Some(pipeline_id);
                        }
                        render_cmds.push(rc.clone());
                    }
                    RecordedOp::UploadTexture {
                        dst,
                        width,
                        height,
                        format,
                        data,
                    } => {
                        if let Ok(tex_id) =
                            renderer.create_texture_dxgi(*width, *height, *format, data)
                        {
                            // SAFETY: `dst` est un `D3D12ResourceObject*` valide.
                            let dst_obj = unsafe { &*(*dst as *const D3D12ResourceObject) };
                            dst_obj.gpu_texture_id.store(tex_id, Ordering::Release);
                        }
                    }
                    RecordedOp::BindRootCbv { slot, addr } => {
                        let a = *addr as usize;
                        let size = ALLOCATED_BUFFER_SIZES
                            .lock()
                            .get(&a)
                            .copied()
                            .unwrap_or(256)
                            .min(4096);
                        // SAFETY: `addr` provient d'un `ID3D12Resource` mappé (taille vérifiée).
                        let data =
                            unsafe { std::slice::from_raw_parts(a as *const u8, size).to_vec() };
                        render_cmds.push(RenderCommand::UpdateConstantBuffer { slot: *slot, data });
                    }
                    RecordedOp::BindComputeStorage { slot, addr } => {
                        let s = (*slot as usize) % 4;
                        let a = *addr as usize;
                        let size = ALLOCATED_BUFFER_SIZES
                            .lock()
                            .get(&a)
                            .copied()
                            .unwrap_or(4096)
                            .min(SSBO_BYTE_SIZE as usize);
                        // SAFETY: `addr` provient d'un `ID3D12Resource` (committed ou placed).
                        let data =
                            unsafe { std::slice::from_raw_parts(a as *const u8, size).to_vec() };
                        render_cmds.push(RenderCommand::UpdateStorageBuffer {
                            slot: s as u32,
                            data,
                        });
                        bound_uav_addrs[s] = Some((a, size));
                    }
                    RecordedOp::ComputeDispatch { x, y, z } => {
                        flush_render(&mut renderer, &mut render_cmds);
                        if let Some(cpid) = active_compute_pid {
                            if let Err(err) = renderer.dispatch_compute(cpid, *x, *y, *z) {
                                sb_warn!(CHANNEL, "Dispatch({x},{y},{z}) échoué: {err}");
                            } else {
                                // Cohérence mémoire unifiée : copie les UAV modifiés vers la mémoire CPU/GPU.
                                for (slot_idx, entry) in bound_uav_addrs.iter().enumerate() {
                                    if let Some((addr, len)) = *entry
                                        && let Ok(out) =
                                            renderer.readback_storage_buffer(slot_idx as u32, len)
                                    {
                                        // SAFETY: `addr` provient d'un buffer `ID3D12Resource` valide de taille >= `len`.
                                        unsafe {
                                            ptr::copy_nonoverlapping(
                                                out.as_ptr(),
                                                addr as *mut u8,
                                                out.len().min(len),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    RecordedOp::CopyBufferResource { dst, src } => {
                        flush_render(&mut renderer, &mut render_cmds);
                        let src_obj = unsafe { &*(*src as *const D3D12ResourceObject) };
                        let dst_obj = unsafe { &*(*dst as *const D3D12ResourceObject) };
                        let len = src_obj.byte_len.min(dst_obj.byte_len);
                        if len > 0 {
                            // Copie via les adresses fixes unifiées (évite un double-lock si même `ID3D12Heap` !).
                            let tmp = unsafe {
                                std::slice::from_raw_parts(src_obj.fixed_addr as *const u8, len)
                                    .to_vec()
                            };
                            unsafe {
                                ptr::copy_nonoverlapping(
                                    tmp.as_ptr(),
                                    dst_obj.fixed_addr as *mut u8,
                                    len,
                                );
                            }
                        }
                    }
                    RecordedOp::CopyBackbufferToResource { dst } => {
                        flush_render(&mut renderer, &mut render_cmds);
                        match renderer.readback_rgba() {
                            Ok(rgba) => {
                                let dst_ptr = *dst as *mut D3D12ResourceObject;
                                // SAFETY: `dst_ptr` provient d'un `ID3D12Resource` passé à `CopyResource`.
                                let dst_obj = unsafe { &*dst_ptr };
                                let len = rgba.len().min(dst_obj.byte_len);
                                unsafe {
                                    ptr::copy_nonoverlapping(
                                        rgba.as_ptr(),
                                        dst_obj.fixed_addr as *mut u8,
                                        len,
                                    );
                                }
                                sb_debug!(
                                    CHANNEL,
                                    "CopyResource GPU->CPU readback: {len} octets copiés vers {dst_ptr:p}"
                                );
                            }
                            Err(err) => sb_warn!(CHANNEL, "readback_rgba échoué: {err}"),
                        }
                    }
                    RecordedOp::BindDescriptorTable { handle, slot } => {
                        if let Some(&binding) = descs.get(handle) {
                            match binding {
                                DescriptorBinding::SrvTexture { resource_ptr } => {
                                    let tid = ensure_tex_uploaded(&mut renderer, resource_ptr);
                                    render_cmds.push(RenderCommand::BindTexture {
                                        slot: *slot % 4,
                                        texture_id: tid,
                                    });
                                }
                                DescriptorBinding::Cbv { addr, size } => {
                                    let len = (size as usize).min(4096);
                                    if addr != 0 && len > 0 {
                                        // SAFETY: `addr` provient d'un CBV D3D12 valide.
                                        let data = unsafe {
                                            std::slice::from_raw_parts(addr as *const u8, len)
                                                .to_vec()
                                        };
                                        render_cmds.push(RenderCommand::UpdateConstantBuffer {
                                            slot: *slot % 4,
                                            data,
                                        });
                                    }
                                }
                                DescriptorBinding::UavBuffer { addr, size } => {
                                    let s = (*slot as usize) % 4;
                                    let len = (size as usize).min(SSBO_BYTE_SIZE as usize);
                                    if addr != 0 && len > 0 {
                                        // SAFETY: `addr` provient d'un UAV D3D12 valide.
                                        let data = unsafe {
                                            std::slice::from_raw_parts(addr as *const u8, len)
                                                .to_vec()
                                        };
                                        render_cmds.push(RenderCommand::UpdateStorageBuffer {
                                            slot: s as u32,
                                            data,
                                        });
                                        bound_uav_addrs[s] = Some((addr as usize, len));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        flush_render(&mut renderer, &mut render_cmds);
    });
}

extern "win64" fn queue_signal(
    _this: *mut D3D12QueueObject,
    fence: *mut D3D12FenceObject,
    value: u64,
) -> HResult {
    if !fence.is_null() {
        // SAFETY: `fence` vérifié non-nul.
        unsafe {
            (*fence).completed.store(value, Ordering::Release);
        }
        sb_trace!(CHANNEL, "ID3D12CommandQueue::Signal(value={value})");
    }
    S_OK
}

const fn build_queue_vtbl() -> D3D12QueueVtbl {
    let mut slots = UNIMPL_SLOTS;
    let mut pad = 0;
    while pad < 20 {
        slots[pad] = vslot(noop_ok as *const c_void);
        pad += 1;
    }
    slots[0] = vslot(queue_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[10] = vslot(queue_execute_command_lists as *const c_void);
    slots[14] = vslot(queue_signal as *const c_void);
    D3D12QueueVtbl { slots }
}

static D3D12_QUEUE_VTBL: D3D12QueueVtbl = build_queue_vtbl();

// --- ID3D12Device ---

#[repr(C)]
struct D3D12DeviceVtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct D3D12DeviceObject {
    header: ComHeader,
    shared: Arc<SharedD3D12State>,
}

extern "win64" fn device_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("ID3D12Device", this, riid, ppv, &[IID_ID3D12DEVICE])
}

extern "win64" fn device_get_node_count(_this: *mut D3D12DeviceObject) -> u32 {
    1
}

extern "win64" fn device_create_command_queue(
    this: *mut D3D12DeviceObject,
    _desc: *const D3D12CommandQueueDesc,
    _riid: *const Guid,
    out_queue: *mut *mut c_void,
) -> HResult {
    if this.is_null() || out_queue.is_null() {
        return E_POINTER;
    }
    // SAFETY: `this` et `out_queue` vérifiés non-nuls.
    let dev = unsafe { &*this };
    let q = Box::new(D3D12QueueObject {
        header: ComHeader::new((&raw const D3D12_QUEUE_VTBL).cast()),
        shared: Arc::clone(&dev.shared),
    });
    unsafe {
        *out_queue = Box::into_raw(q).cast();
    }
    sb_debug!(CHANNEL, "ID3D12Device::CreateCommandQueue() -> S_OK");
    S_OK
}

extern "win64" fn device_create_command_allocator(
    _this: *mut D3D12DeviceObject,
    _list_type: u32,
    _riid: *const Guid,
    out_alloc: *mut *mut c_void,
) -> HResult {
    if out_alloc.is_null() {
        return E_POINTER;
    }
    let alloc = Box::new(D3D12CmdAllocObject {
        header: ComHeader::new((&raw const D3D12_CMD_ALLOC_VTBL).cast()),
    });
    // SAFETY: `out_alloc` vérifié non-nul.
    unsafe {
        *out_alloc = Box::into_raw(alloc).cast();
    }
    S_OK
}

fn d3d12_blend_to_vk(b: u32, default: vk::BlendFactor) -> vk::BlendFactor {
    match b {
        1 => vk::BlendFactor::ZERO,
        2 => vk::BlendFactor::ONE,
        3 => vk::BlendFactor::SRC_COLOR,
        4 => vk::BlendFactor::ONE_MINUS_SRC_COLOR,
        5 => vk::BlendFactor::SRC_ALPHA,
        6 => vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
        7 => vk::BlendFactor::DST_ALPHA,
        8 => vk::BlendFactor::ONE_MINUS_DST_ALPHA,
        9 => vk::BlendFactor::DST_COLOR,
        10 => vk::BlendFactor::ONE_MINUS_DST_COLOR,
        11 => vk::BlendFactor::SRC_ALPHA_SATURATE,
        14 => vk::BlendFactor::CONSTANT_COLOR,
        15 => vk::BlendFactor::ONE_MINUS_CONSTANT_COLOR,
        _ => default,
    }
}

fn d3d12_blend_op_to_vk(op: u32) -> vk::BlendOp {
    match op {
        2 => vk::BlendOp::SUBTRACT,
        3 => vk::BlendOp::REVERSE_SUBTRACT,
        4 => vk::BlendOp::MIN,
        5 => vk::BlendOp::MAX,
        _ => vk::BlendOp::ADD,
    }
}

fn d3d12_compare_to_vk(func: u32) -> vk::CompareOp {
    match func {
        1 => vk::CompareOp::NEVER,
        2 => vk::CompareOp::LESS,
        3 => vk::CompareOp::EQUAL,
        4 => vk::CompareOp::LESS_OR_EQUAL,
        5 => vk::CompareOp::GREATER,
        6 => vk::CompareOp::NOT_EQUAL,
        7 => vk::CompareOp::GREATER_OR_EQUAL,
        8 => vk::CompareOp::ALWAYS,
        _ => vk::CompareOp::LESS_OR_EQUAL,
    }
}

fn build_graphics_pipeline_config(d: &D3D12GraphicsPipelineStateDesc) -> GraphicsPipelineConfig {
    let mut cfg = GraphicsPipelineConfig::default();

    // InputLayout personnalisé (`D3D12_INPUT_ELEMENT_DESC`) si fourni.
    if !d.input_layout.input_element_descs.is_null() && d.input_layout.num_elements > 0 {
        // SAFETY: `input_element_descs` pointe vers `num_elements` descripteurs valides.
        let elems = unsafe {
            std::slice::from_raw_parts(
                d.input_layout.input_element_descs,
                (d.input_layout.num_elements as usize).min(16),
            )
        };
        let mut running_offset = 0u32;
        for (i, e) in elems.iter().enumerate() {
            let offset = if e.aligned_byte_offset == 0xFFFF_FFFF {
                running_offset
            } else {
                e.aligned_byte_offset
            };
            let sz = dxgi_format_byte_size(e.format);
            cfg.vertex_attrs.push(VertexAttrConfig {
                location: i as u32,
                format: dxgi_format_to_vk(e.format),
                offset,
            });
            running_offset = offset + sz;
        }
        cfg.vertex_stride = running_offset.max(16);
    } else if d.input_layout.num_elements > 0 {
        cfg.vertex_stride = (d.input_layout.num_elements * 16).max(32);
    }

    // Blend state (RT0)
    let rt0 = d.blend_state.render_target[0];
    cfg.blend_enable = rt0.blend_enable != 0;
    cfg.src_color_blend = d3d12_blend_to_vk(rt0.src_blend, vk::BlendFactor::SRC_ALPHA);
    cfg.dst_color_blend = d3d12_blend_to_vk(rt0.dest_blend, vk::BlendFactor::ONE_MINUS_SRC_ALPHA);
    cfg.color_blend_op = d3d12_blend_op_to_vk(rt0.blend_op);
    cfg.src_alpha_blend = d3d12_blend_to_vk(rt0.src_blend_alpha, vk::BlendFactor::ONE);
    cfg.dst_alpha_blend = d3d12_blend_to_vk(rt0.dest_blend_alpha, vk::BlendFactor::ZERO);
    cfg.alpha_blend_op = d3d12_blend_op_to_vk(rt0.blend_op_alpha);
    if rt0.render_target_write_mask != 0 {
        let mut mask = vk::ColorComponentFlags::empty();
        if (rt0.render_target_write_mask & 1) != 0 {
            mask |= vk::ColorComponentFlags::R;
        }
        if (rt0.render_target_write_mask & 2) != 0 {
            mask |= vk::ColorComponentFlags::G;
        }
        if (rt0.render_target_write_mask & 4) != 0 {
            mask |= vk::ColorComponentFlags::B;
        }
        if (rt0.render_target_write_mask & 8) != 0 {
            mask |= vk::ColorComponentFlags::A;
        }
        cfg.color_write_mask = mask;
    }

    // Rasterizer state
    cfg.wireframe = d.rasterizer_state.fill_mode == 2;
    cfg.cull_mode = match d.rasterizer_state.cull_mode {
        2 => vk::CullModeFlags::FRONT,
        3 => vk::CullModeFlags::BACK,
        _ => vk::CullModeFlags::NONE,
    };
    cfg.front_ccw = d.rasterizer_state.front_counter_clockwise != 0;

    // Depth-Stencil state
    if d.depth_stencil_state.depth_enable != 0 {
        cfg.depth_test = true;
        cfg.depth_write = d.depth_stencil_state.depth_write_mask != 0;
        cfg.depth_compare = d3d12_compare_to_vk(d.depth_stencil_state.depth_func);
    } else if d.dsv_format == 0 {
        cfg.depth_test = false;
        cfg.depth_write = false;
    }

    // Topology type
    cfg.topology = match d.primitive_topology_type {
        1 => vk::PrimitiveTopology::POINT_LIST,
        2 => vk::PrimitiveTopology::LINE_LIST,
        _ => vk::PrimitiveTopology::TRIANGLE_LIST,
    };

    cfg
}

extern "win64" fn device_create_graphics_pso(
    this: *mut D3D12DeviceObject,
    desc: *const D3D12GraphicsPipelineStateDesc,
    _riid: *const Guid,
    out_pso: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CreateGraphicsPipelineState", E_UNEXPECTED, {
        if this.is_null() || desc.is_null() || out_pso.is_null() {
            return E_POINTER;
        }
        // SAFETY: `this` et `desc` vérifiés non-nuls.
        let dev = unsafe { &*this };
        let d = unsafe { &*desc };
        if d.vs.shader_bytecode.is_null()
            || d.vs.bytecode_length < 32
            || d.ps.shader_bytecode.is_null()
            || d.ps.bytecode_length < 32
        {
            sb_warn!(
                CHANNEL,
                "CreateGraphicsPipelineState: bytecode VS/PS absent"
            );
            return E_INVALIDARG;
        }
        // SAFETY: tranches valides selon `D3D12_SHADER_BYTECODE`.
        let vs_bytes =
            unsafe { std::slice::from_raw_parts(d.vs.shader_bytecode, d.vs.bytecode_length) };
        let ps_bytes =
            unsafe { std::slice::from_raw_parts(d.ps.shader_bytecode, d.ps.bytecode_length) };

        let cfg = build_graphics_pipeline_config(d);
        let mut renderer = dev.shared.renderer.lock();
        match renderer.create_pipeline_from_dxbc_with_config(vs_bytes, ps_bytes, &cfg) {
            Ok(pipeline_id) => {
                let pso = Box::new(D3D12PsoObject {
                    header: ComHeader::new((&raw const D3D12_PSO_VTBL).cast()),
                    pipeline_id,
                    is_compute: false,
                });
                // SAFETY: `out_pso` vérifié non-nul.
                unsafe {
                    *out_pso = Box::into_raw(pso).cast();
                }
                sb_info!(
                    CHANNEL,
                    "ID3D12Device::CreateGraphicsPipelineState -> VkPipeline #{pipeline_id} prêt"
                );
                S_OK
            }
            Err(err) => {
                sb_warn!(CHANNEL, "CreateGraphicsPipelineState échoué: {err}");
                E_FAIL
            }
        }
    })
}

extern "win64" fn device_create_compute_pso(
    this: *mut D3D12DeviceObject,
    desc: *const D3D12ComputePipelineStateDesc,
    _riid: *const Guid,
    out_pso: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CreateComputePipelineState", E_UNEXPECTED, {
        if this.is_null() || desc.is_null() || out_pso.is_null() {
            return E_POINTER;
        }
        // SAFETY: `this` et `desc` vérifiés non-nuls.
        let dev = unsafe { &*this };
        let d = unsafe { &*desc };
        if d.cs.shader_bytecode.is_null() || d.cs.bytecode_length < 32 {
            sb_warn!(CHANNEL, "CreateComputePipelineState: bytecode CS absent");
            return E_INVALIDARG;
        }
        // SAFETY: tranche valide selon `D3D12_SHADER_BYTECODE`.
        let cs_bytes =
            unsafe { std::slice::from_raw_parts(d.cs.shader_bytecode, d.cs.bytecode_length) };

        let mut renderer = dev.shared.renderer.lock();
        match renderer.create_compute_pipeline_from_dxbc(cs_bytes) {
            Ok(pipeline_id) => {
                let pso = Box::new(D3D12PsoObject {
                    header: ComHeader::new((&raw const D3D12_PSO_VTBL).cast()),
                    pipeline_id,
                    is_compute: true,
                });
                // SAFETY: `out_pso` vérifié non-nul.
                unsafe {
                    *out_pso = Box::into_raw(pso).cast();
                }
                sb_info!(
                    CHANNEL,
                    "ID3D12Device::CreateComputePipelineState -> VkPipeline Compute #{pipeline_id} prêt"
                );
                S_OK
            }
            Err(err) => {
                sb_warn!(CHANNEL, "CreateComputePipelineState échoué: {err}");
                E_FAIL
            }
        }
    })
}

extern "win64" fn device_check_feature_support(
    this: *mut D3D12DeviceObject,
    feature: u32,
    data: *mut c_void,
    data_size: u32,
) -> HResult {
    ffi_guard!(CHANNEL, "CheckFeatureSupport", E_UNEXPECTED, {
        if this.is_null() || data.is_null() || data_size == 0 {
            return E_INVALIDARG;
        }
        // SAFETY: `this` et `data[..data_size]` valides.
        let dev = unsafe { &*this };
        let has_rt = dev.shared.renderer.lock().gpu.caps.has_ray_tracing;
        let bytes =
            unsafe { std::slice::from_raw_parts_mut(data.cast::<u8>(), data_size as usize) };

        let write_u32 = |buf: &mut [u8], off: usize, val: u32| {
            if off + 4 <= buf.len() {
                buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
            }
        };

        match feature {
            0 => {
                // D3D12_FEATURE_D3D12_OPTIONS
                bytes.fill(0);
                write_u32(bytes, 12, 3); // TiledResourcesTier = 3
                write_u32(bytes, 16, 3); // ResourceBindingTier = TIER_3
                write_u32(bytes, 24, 1); // TypedUAVLoadAdditionalFormats
                write_u32(bytes, 36, 40); // MaxGPUVirtualAddressBitsPerResource
                write_u32(bytes, 40, 1); // StandardSwizzle64KBSupported
                write_u32(bytes, 52, 1); // VPAndRTArrayIndex...
                write_u32(bytes, 56, 2); // ResourceHeapTier = TIER_2
            }
            2 => {
                // D3D12_FEATURE_FEATURE_LEVELS : MaxSupportedFeatureLevel à l'offset 16
                write_u32(bytes, 16, 0xC100); // D3D_FEATURE_LEVEL_12_1
            }
            7 => {
                // D3D12_FEATURE_SHADER_MODEL : plafonne à SM 6.6 (0x66)
                if bytes.len() >= 4 {
                    let req = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                    let supported = req.clamp(0x51, 0x66);
                    write_u32(bytes, 0, supported);
                }
            }
            27 => {
                // D3D12_FEATURE_D3D12_OPTIONS5 : RenderPasses + RaytracingTier
                bytes.fill(0);
                write_u32(bytes, 4, 2); // D3D12_RENDER_PASS_TIER_2
                write_u32(bytes, 8, if has_rt { 11 } else { 0 }); // TIER_1_1 ou NOT_SUPPORTED
            }
            32 => {
                // D3D12_FEATURE_D3D12_OPTIONS7 : MeshShader + SamplerFeedback
                bytes.fill(0);
            }
            _ => {}
        }
        sb_debug!(
            CHANNEL,
            "ID3D12Device::CheckFeatureSupport(feature={feature}, size={data_size}) -> S_OK"
        );
        S_OK
    })
}

extern "win64" fn device_create_command_list(
    _this: *mut D3D12DeviceObject,
    _node_mask: u32,
    _list_type: u32,
    _allocator: *mut c_void,
    initial_state: *mut D3D12PsoObject,
    _riid: *const Guid,
    out_list: *mut *mut c_void,
) -> HResult {
    if out_list.is_null() {
        return E_POINTER;
    }
    let mut ops = Vec::new();
    if !initial_state.is_null() {
        // SAFETY: `initial_state` est un `D3D12PsoObject` valide.
        let pso = unsafe { &*initial_state };
        let rc = if pso.is_compute {
            RenderCommand::SetComputePipeline {
                pipeline_id: pso.pipeline_id,
            }
        } else {
            RenderCommand::SetPipeline {
                pipeline_id: pso.pipeline_id,
            }
        };
        ops.push(RecordedOp::Render(rc));
    }
    let cl = Box::new(D3D12CmdListObject {
        header: ComHeader::new((&raw const D3D12_CMDLIST_VTBL).cast()),
        ops: Mutex::new(ops),
        closed: Mutex::new(false),
        graphics_root_regs: Mutex::new(Vec::new()),
        compute_root_regs: Mutex::new(Vec::new()),
    });
    // SAFETY: `out_list` vérifié non-nul.
    unsafe {
        *out_list = Box::into_raw(cl).cast();
    }
    S_OK
}

extern "win64" fn device_create_descriptor_heap(
    _this: *mut D3D12DeviceObject,
    desc: *const D3D12DescriptorHeapDesc,
    _riid: *const Guid,
    out_heap: *mut *mut c_void,
) -> HResult {
    if out_heap.is_null() {
        return E_POINTER;
    }
    let d = if desc.is_null() {
        D3D12DescriptorHeapDesc::default()
    } else {
        // SAFETY: `desc` vérifié non-nul.
        unsafe { *desc }
    };
    let count = u64::from(d.num_descriptors.max(1));
    let base = NEXT_DESC_HANDLE.fetch_add(count * 32, Ordering::Relaxed) as usize;
    let heap = Box::new(D3D12DescHeapObject {
        header: ComHeader::new((&raw const D3D12_DESC_HEAP_VTBL).cast()),
        desc: d,
        base_handle: base,
    });
    // SAFETY: `out_heap` vérifié non-nul.
    unsafe {
        *out_heap = Box::into_raw(heap).cast();
    }
    S_OK
}

extern "win64" fn device_get_desc_increment(_this: *mut D3D12DeviceObject, _heap_type: u32) -> u32 {
    32
}

extern "win64" fn device_create_root_signature(
    _this: *mut D3D12DeviceObject,
    _node_mask: u32,
    blob: *const c_void,
    blob_len: usize,
    _riid: *const Guid,
    out_rs: *mut *mut c_void,
) -> HResult {
    if out_rs.is_null() {
        return E_POINTER;
    }
    let mut param_registers = Vec::new();
    if !blob.is_null() && blob_len >= 8 {
        // SAFETY: `blob[..blob_len]` est le blob retourné par `D3D12SerializeRootSignature`.
        let bytes = unsafe { std::slice::from_raw_parts(blob.cast::<u8>(), blob_len) };
        if &bytes[0..4] == b"SBRS" {
            let count = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
            for i in 0..count {
                let off = 8 + i * 4;
                if off + 4 <= bytes.len() {
                    param_registers.push(u32::from_le_bytes([
                        bytes[off],
                        bytes[off + 1],
                        bytes[off + 2],
                        bytes[off + 3],
                    ]));
                }
            }
        }
    }
    let rs = Box::new(D3D12RootSigObject {
        header: ComHeader::new((&raw const D3D12_ROOTSIG_VTBL).cast()),
        param_registers,
    });
    // SAFETY: `out_rs` vérifié non-nul.
    unsafe {
        *out_rs = Box::into_raw(rs).cast();
    }
    S_OK
}

extern "win64" fn device_create_cbv(
    this: *mut D3D12DeviceObject,
    desc: *const D3D12ConstantBufferViewDesc,
    dest_descriptor: usize,
) {
    if this.is_null() || desc.is_null() {
        return;
    }
    // SAFETY: `this` et `desc` vérifiés non-nuls.
    let dev = unsafe { &*this };
    let d = unsafe { *desc };
    dev.shared.descriptors.lock().insert(
        dest_descriptor,
        DescriptorBinding::Cbv {
            addr: d.buffer_location,
            size: d.size_in_bytes,
        },
    );
}

extern "win64" fn device_create_srv(
    this: *mut D3D12DeviceObject,
    resource: *mut D3D12ResourceObject,
    _desc: *const c_void,
    dest_descriptor: usize,
) {
    if this.is_null() || resource.is_null() {
        return;
    }
    // SAFETY: `this` et `resource` vérifiés non-nuls.
    let dev = unsafe { &*this };
    dev.shared.descriptors.lock().insert(
        dest_descriptor,
        DescriptorBinding::SrvTexture {
            resource_ptr: resource as usize,
        },
    );
}

extern "win64" fn device_create_uav(
    this: *mut D3D12DeviceObject,
    resource: *mut D3D12ResourceObject,
    _counter_resource: *mut c_void,
    _desc: *const c_void,
    dest_descriptor: usize,
) {
    if this.is_null() || resource.is_null() {
        return;
    }
    // SAFETY: `this` et `resource` vérifiés non-nuls.
    let dev = unsafe { &*this };
    let res = unsafe { &*resource };
    dev.shared.descriptors.lock().insert(
        dest_descriptor,
        DescriptorBinding::UavBuffer {
            addr: res.fixed_addr as u64,
            size: res.byte_len as u32,
        },
    );
}

extern "win64" fn device_create_rtv(
    _this: *mut D3D12DeviceObject,
    _resource: *mut D3D12ResourceObject,
    _desc: *const c_void,
    _dest_descriptor: usize,
) {
}

extern "win64" fn device_create_dsv(
    _this: *mut D3D12DeviceObject,
    _resource: *mut D3D12ResourceObject,
    _desc: *const c_void,
    _dest_descriptor: usize,
) {
}

extern "win64" fn device_create_sampler(
    _this: *mut D3D12DeviceObject,
    _desc: *const c_void,
    _dest_descriptor: usize,
) {
}

#[allow(clippy::too_many_arguments)]
extern "win64" fn device_get_copyable_footprints(
    _this: *mut D3D12DeviceObject,
    res_desc: *const D3D12ResourceDesc,
    _first_subresource: u32,
    num_subresources: u32,
    base_offset: u64,
    out_layouts: *mut D3D12PlacedSubresourceFootprint,
    out_num_rows: *mut u32,
    out_row_size: *mut u64,
    out_total_bytes: *mut u64,
) {
    if res_desc.is_null() || num_subresources == 0 {
        return;
    }
    // SAFETY: `res_desc` vérifié non-nul.
    let d = unsafe { *res_desc };
    let w = (d.width as u32).max(1);
    let h = d.height.max(1);
    let (row_bytes_usize, num_rows_usize) = texture_row_layout(w, h, d.format);
    let row_bytes = row_bytes_usize as u64;
    let num_rows = num_rows_usize as u32;
    // Alignement D3D12 obligatoire : `D3D12_TEXTURE_DATA_PITCH_ALIGNMENT` = 256 octets.
    let row_pitch = ((row_bytes as u32) + 255) & !255;
    let total = u64::from(row_pitch) * u64::from(num_rows);
    if !out_layouts.is_null() {
        // SAFETY: `out_layouts` non-nul.
        unsafe {
            *out_layouts = D3D12PlacedSubresourceFootprint {
                offset: base_offset,
                footprint: D3D12SubresourceFootprint {
                    format: d.format,
                    width: w,
                    height: h,
                    depth: 1,
                    row_pitch,
                },
            };
        }
    }
    if !out_num_rows.is_null() {
        // SAFETY: `out_num_rows` non-nul.
        unsafe {
            *out_num_rows = num_rows;
        }
    }
    if !out_row_size.is_null() {
        // SAFETY: `out_row_size` non-nul.
        unsafe {
            *out_row_size = row_bytes;
        }
    }
    if !out_total_bytes.is_null() {
        // SAFETY: `out_total_bytes` non-nul.
        unsafe {
            *out_total_bytes = total;
        }
    }
}

extern "win64" fn device_create_heap(
    _this: *mut D3D12DeviceObject,
    desc: *const D3D12HeapDesc,
    _riid: *const Guid,
    out_heap: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CreateHeap", E_UNEXPECTED, {
        if desc.is_null() || out_heap.is_null() {
            return E_POINTER;
        }
        // SAFETY: `desc` et `out_heap` vérifiés non-nuls.
        let d = unsafe { *desc };
        let size = (d.size_in_bytes as usize).max(65536);
        let mut buf = vec![0u8; size];
        let base_addr = buf.as_mut_ptr() as usize;
        let heap = Box::new(D3D12HeapObject {
            header: ComHeader::new((&raw const D3D12_HEAP_VTBL).cast()),
            desc: d,
            backing: Arc::new(Mutex::new(buf)),
            base_addr,
            size,
        });
        unsafe {
            *out_heap = Box::into_raw(heap).cast();
        }
        sb_debug!(
            CHANNEL,
            "ID3D12Device::CreateHeap(size={size}o, base={base_addr:#x}) -> S_OK"
        );
        S_OK
    })
}

#[allow(clippy::too_many_arguments)]
extern "win64" fn device_create_committed_resource(
    _this: *mut D3D12DeviceObject,
    _heap_props: *const D3D12HeapProperties,
    _heap_flags: u32,
    desc: *const D3D12ResourceDesc,
    _initial_state: u32,
    _clear_value: *const c_void,
    _riid: *const Guid,
    out_resource: *mut *mut c_void,
) -> HResult {
    if desc.is_null() || out_resource.is_null() {
        return E_POINTER;
    }
    // SAFETY: `desc` vérifié non-nul.
    let d = unsafe { *desc };
    let res = alloc_d3d12_resource(d, false);
    // SAFETY: `out_resource` vérifié non-nul.
    unsafe {
        *out_resource = res.cast();
    }
    sb_trace!(
        CHANNEL,
        "CreateCommittedResource(dim={}, w={}, h={}) -> {res:p}",
        d.dimension,
        d.width,
        d.height
    );
    S_OK
}

#[allow(clippy::too_many_arguments)]
extern "win64" fn device_create_placed_resource(
    _this: *mut D3D12DeviceObject,
    heap: *mut D3D12HeapObject,
    heap_offset: u64,
    desc: *const D3D12ResourceDesc,
    _initial_state: u32,
    _clear_value: *const c_void,
    _riid: *const Guid,
    out_resource: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CreatePlacedResource", E_UNEXPECTED, {
        if desc.is_null() || out_resource.is_null() {
            return E_POINTER;
        }
        // SAFETY: `desc` vérifié non-nul.
        let d = unsafe { *desc };
        let res = if !heap.is_null() {
            // SAFETY: `heap` provient de `ID3D12Device::CreateHeap`.
            let h = unsafe { &*heap };
            match alloc_placed_d3d12_resource(h, heap_offset, d) {
                Ok(ptr_res) => ptr_res,
                Err(hr) => return hr,
            }
        } else {
            alloc_d3d12_resource(d, false)
        };
        // SAFETY: `out_resource` vérifié non-nul.
        unsafe {
            *out_resource = res.cast();
        }
        sb_trace!(
            CHANNEL,
            "CreatePlacedResource(heap={heap:p}, offset={heap_offset}, dim={}, w={}) -> {res:p}",
            d.dimension,
            d.width
        );
        S_OK
    })
}

extern "win64" fn device_create_fence(
    _this: *mut D3D12DeviceObject,
    initial_value: u64,
    _flags: u32,
    _riid: *const Guid,
    out_fence: *mut *mut c_void,
) -> HResult {
    if out_fence.is_null() {
        return E_POINTER;
    }
    let fence = Box::new(D3D12FenceObject {
        header: ComHeader::new((&raw const D3D12_FENCE_VTBL).cast()),
        completed: AtomicU64::new(initial_value),
    });
    // SAFETY: `out_fence` vérifié non-nul.
    unsafe {
        *out_fence = Box::into_raw(fence).cast();
    }
    S_OK
}

/// `S_FALSE` : la validation des paramètres réussit sans créer d'objet (`ppv == NULL`).
const S_FALSE_HR: HResult = 1;

/// `ID3D12Device3::OpenExistingHeapFromAddress(const void* pAddress, REFIID riid, void** ppvHeap)`
/// (slot 48). Sur console, un titre bâtit ses tas D3D12 sur de la mémoire qu'il a déjà obtenue
/// (`XMemVirtualAlloc`) : le tas **partage** la mémoire du titre, sans copie.
///
/// Validations comme la documentation : adresse non NULL, alignée sur la page, dans une région
/// committée lisible ; `riid` = `ID3D12Heap` (ou `IUnknown`). La taille du tas est celle de la
/// région à partir de `pAddress`.
extern "win64" fn device_open_existing_heap_from_address(
    _this: *mut D3D12DeviceObject,
    address: *const c_void,
    riid: *const Guid,
    out_heap: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "OpenExistingHeapFromAddress", E_UNEXPECTED, {
        if !out_heap.is_null() {
            // SAFETY: `out_heap` non-nul, sortie remise à NULL en cas d'échec.
            unsafe { *out_heap = ptr::null_mut() };
        }
        let addr = address as usize;
        if addr == 0 || !addr.is_multiple_of(4096) {
            sb_warn!(
                CHANNEL,
                "OpenExistingHeapFromAddress : adresse NULL ou non alignée sur la page -> E_INVALIDARG (pAddress={}, riid={}, ppvHeap={out_heap:p}) ; si le titre attend un succès, ce slot n'est peut-être pas cette méthode sur la console",
                crate::com::describe_guest_arg(addr),
                crate::com::describe_guest_arg(riid as usize)
            );
            return E_INVALIDARG;
        }
        if !riid.is_null() {
            // SAFETY: `riid` non-nul ; GUID lu sans exigence d'alignement.
            let wanted = unsafe { ptr::read_unaligned(riid) };
            if wanted != IID_ID3D12HEAP && wanted != crate::com::IID_IUNKNOWN {
                sb_warn!(
                    CHANNEL,
                    "OpenExistingHeapFromAddress : interface {{{wanted}}} non fournie -> E_NOINTERFACE"
                );
                return sb_common::abi::E_NOINTERFACE;
            }
        }
        let Some(info) = crate::kernel_ext::vmem::query(addr) else {
            return E_INVALIDARG;
        };
        // Région committée (MEM_COMMIT = 0x1000) et accessible.
        if info.state != 0x1000 || info.protect == 0 {
            sb_warn!(
                CHANNEL,
                "OpenExistingHeapFromAddress({address:p}) : mémoire non committée/accessible -> E_INVALIDARG"
            );
            return E_INVALIDARG;
        }
        let size = (info.base_address + info.region_size).saturating_sub(addr);
        if size == 0 {
            return E_INVALIDARG;
        }
        if out_heap.is_null() {
            return S_FALSE_HR; // validation seule
        }
        // SAFETY: `addr..addr+size` est une région committée du processus (vérifiée par `query`) ;
        // le `Vec` ne possède pas cette mémoire : on retient une référence supplémentaire sur l'`Arc`
        // (jamais relâchée) pour qu'il ne soit jamais détruit, et il n'est jamais redimensionné.
        let backing = unsafe { Vec::from_raw_parts(addr as *mut u8, size, size) };
        let backing = Arc::new(Mutex::new(backing));
        std::mem::forget(Arc::clone(&backing));
        let heap = Box::new(D3D12HeapObject {
            header: ComHeader::new((&raw const D3D12_HEAP_VTBL).cast()),
            desc: D3D12HeapDesc {
                size_in_bytes: size as u64,
                alignment: 65536,
                ..Default::default()
            },
            backing,
            base_addr: addr,
            size,
        });
        // SAFETY: `out_heap` vérifié non-nul.
        unsafe { *out_heap = Box::into_raw(heap).cast() };
        sb_info!(
            CHANNEL,
            "OpenExistingHeapFromAddress({address:p}) -> tas partagé de {size} octets"
        );
        S_OK
    })
}

/// `ID3D12Device1::SetResidencyPriority` (slot 46) : avec une mémoire unifiée, tout est résident ;
/// la priorité est un simple indice. Réussit sans effet.
extern "win64" fn device_set_residency_priority() -> HResult {
    S_OK
}

/// `ID3D12Device3::EnqueueMakeResident(flags, numObjects, ppObjects, pFenceToSignal, fenceValue)`
/// (slot 50) : rien à rendre résident ; la barrière est signalée immédiatement.
extern "win64" fn device_enqueue_make_resident(
    _this: *mut D3D12DeviceObject,
    _flags: u32,
    num_objects: u32,
    objects: *const *mut c_void,
    fence: *mut D3D12FenceObject,
    fence_value: u64,
) -> HResult {
    ffi_guard!(CHANNEL, "EnqueueMakeResident", E_UNEXPECTED, {
        if num_objects > 0 && objects.is_null() {
            return E_INVALIDARG;
        }
        if fence.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: `fence` provient de `CreateFence` (objet du bridge, jamais libéré).
        unsafe { (*fence).completed.fetch_max(fence_value, Ordering::AcqRel) };
        S_OK
    })
}

const fn build_device_vtbl() -> D3D12DeviceVtbl {
    let mut slots = UNIMPL_SLOTS;
    let mut pad = 0;
    while pad < 45 {
        slots[pad] = vslot(noop_ok as *const c_void);
        pad += 1;
    }
    slots[0] = vslot(device_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[7] = vslot(device_get_node_count as *const c_void);
    slots[8] = vslot(device_create_command_queue as *const c_void);
    slots[9] = vslot(device_create_command_allocator as *const c_void);
    slots[10] = vslot(device_create_graphics_pso as *const c_void);
    slots[11] = vslot(device_create_compute_pso as *const c_void);
    slots[12] = vslot(device_create_command_list as *const c_void);
    slots[13] = vslot(device_check_feature_support as *const c_void);
    slots[14] = vslot(device_create_descriptor_heap as *const c_void);
    slots[15] = vslot(device_get_desc_increment as *const c_void);
    slots[16] = vslot(device_create_root_signature as *const c_void);
    slots[17] = vslot(device_create_cbv as *const c_void);
    slots[18] = vslot(device_create_srv as *const c_void);
    slots[19] = vslot(device_create_uav as *const c_void);
    slots[20] = vslot(device_create_rtv as *const c_void);
    slots[21] = vslot(device_create_dsv as *const c_void);
    slots[22] = vslot(device_create_sampler as *const c_void);
    slots[27] = vslot(device_create_committed_resource as *const c_void);
    slots[28] = vslot(device_create_heap as *const c_void);
    slots[29] = vslot(device_create_placed_resource as *const c_void);
    slots[36] = vslot(device_create_fence as *const c_void);
    slots[38] = vslot(device_get_copyable_footprints as *const c_void);
    // `ID3D12Device1` à `ID3D12Device3` (slots 44..50) : seuls ceux de sémantique non ambiguë.
    slots[46] = vslot(device_set_residency_priority as *const c_void);
    slots[48] = vslot(device_open_existing_heap_from_address as *const c_void);
    slots[50] = vslot(device_enqueue_make_resident as *const c_void);
    D3D12DeviceVtbl { slots }
}

static D3D12_DEVICE_VTBL: D3D12DeviceVtbl = build_device_vtbl();

// ==================== Points d'entrée `d3d12_xs.dll` ====================

pub(crate) extern "win64" fn D3D12CreateDevice(
    _adapter: *mut c_void,
    min_feature_level: u32,
    _riid: *const Guid,
    out_device: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "D3D12CreateDevice", E_UNEXPECTED, {
        let enable_surface = !is_headless();
        let gpu = match GpuContext::new(enable_surface) {
            Ok(g) => g,
            Err(err) => {
                sb_warn!(
                    CHANNEL,
                    "GpuContext(surface={enable_surface}) échoué ({err}), tentative headless..."
                );
                match GpuContext::new(false) {
                    Ok(g) => g,
                    Err(e2) => {
                        sb_warn!(CHANNEL, "Vulkan indisponible: {e2}");
                        return E_FAIL;
                    }
                }
            }
        };

        let renderer = match VulkanRenderer::new(Arc::clone(&gpu), 256, 256) {
            Ok(r) => r,
            Err(err) => {
                sb_warn!(CHANNEL, "VulkanRenderer::new échoué: {err}");
                return E_FAIL;
            }
        };

        sb_info!(
            CHANNEL,
            "D3D12CreateDevice(feature_level={min_feature_level:#x}) -> Vulkan 1.4 sur '{}'",
            gpu.caps.device_name
        );

        if !out_device.is_null() {
            let dev = Box::new(D3D12DeviceObject {
                header: ComHeader::new((&raw const D3D12_DEVICE_VTBL).cast()),
                shared: Arc::new(SharedD3D12State {
                    renderer: Mutex::new(renderer),
                    sdl_window: Mutex::new(None),
                    descriptors: Mutex::new(HashMap::new()),
                }),
            });
            // SAFETY: `out_device` vérifié non-nul.
            unsafe {
                *out_device = Box::into_raw(dev).cast();
            }
        }
        S_OK
    })
}

pub(crate) extern "win64" fn D3D12GetDebugInterface(
    _riid: *const Guid,
    out_debug: *mut *mut c_void,
) -> HResult {
    if !out_debug.is_null() {
        // SAFETY: `out_debug` vérifié non-nul.
        unsafe {
            *out_debug = ptr::null_mut();
        }
    }
    E_NOTIMPL
}

pub(crate) extern "win64" fn D3D12SerializeRootSignature(
    desc: *const c_void,
    _version: u32,
    out_blob: *mut *mut c_void,
    out_error_blob: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "D3D12SerializeRootSignature", E_UNEXPECTED, {
        if !out_error_blob.is_null() {
            // SAFETY: `out_error_blob` vérifié non-nul.
            unsafe {
                *out_error_blob = ptr::null_mut();
            }
        }
        if out_blob.is_null() {
            return E_POINTER;
        }
        let mut data = Vec::from(*b"SBRS");
        if !desc.is_null() {
            // SAFETY: `desc` pointe vers un `D3D12RootSignatureDesc` valide.
            let rs_desc = unsafe { &*(desc.cast::<D3D12RootSignatureDesc>()) };
            let n = (rs_desc.num_parameters as usize).min(32);
            if n > 0 && !rs_desc.parameters.is_null() {
                data.extend_from_slice(&(n as u32).to_le_bytes());
                // SAFETY: `parameters[..n]` valide.
                let params = unsafe { std::slice::from_raw_parts(rs_desc.parameters, n) };
                for p in params {
                    let shader_reg = if p.parameter_type == 0 { 0 } else { p.data0 };
                    data.extend_from_slice(&shader_reg.to_le_bytes());
                }
            } else {
                data.extend_from_slice(&0u32.to_le_bytes());
            }
        } else {
            data.extend_from_slice(&0u32.to_le_bytes());
        }
        let blob = Box::new(D3DBlobObject {
            header: ComHeader::new((&raw const D3D_BLOB_VTBL).cast()),
            data,
        });
        // SAFETY: `out_blob` vérifié non-nul.
        unsafe {
            *out_blob = Box::into_raw(blob).cast();
        }
        S_OK
    })
}

pub(crate) extern "win64" fn D3D12SerializeVersionedRootSignature(
    desc: *const c_void,
    out_blob: *mut *mut c_void,
    out_error_blob: *mut *mut c_void,
) -> HResult {
    D3D12SerializeRootSignature(desc, 1, out_blob, out_error_blob)
}

// ==================== Extensions Xbox Scarlett (`d3d12_xs.dll`) ====================
//
// `D3D12XboxCreateDevice` / `D3D12ConfigureVirtualMemoryX` ne sont que partiellement
// documentés publiquement (en-têtes `d3d12_x.h` / `d3d12_xs.h` du GDK console). Les
// signatures et préfixes de structures ci-dessous sont **déduits** prudemment ; seuls les
// champs de tête (lus, jamais écrits) sont interprétés.

/// `D3D_FEATURE_LEVEL_12_2` (niveau exposé par Xbox Series X|S).
const D3D_FEATURE_LEVEL_12_2: u32 = 0xC200;
/// Bit « couche de débogage » de `D3D12XBOX_PROCESS_DEBUG_FLAGS` (déduit : bit 0).
const D3D12XBOX_PROCESS_DEBUG_FLAG_DEBUG: u32 = 0x1;
/// Le backend Vulkan de `sb-gpu` n'active pas encore `VK_KHR_acceleration_structure` sur le
/// périphérique logique : aucune construction de BVH matérielle n'est possible.
const VK_AS_BACKEND_ENABLED: bool = false;

/// Préfixe (déduit) de `D3D12XBOX_CREATE_DEVICE_PARAMETERS` : seuls ces champs de tête sont lus.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12XboxCreateDeviceParametersPrefix {
    /// Doit valoir `D3D12_SDK_VERSION` du GDK (toute valeur nulle est rejetée).
    pub version: u32,
    /// `D3D12XBOX_PROCESS_DEBUG_FLAGS`.
    pub process_debug_flags: u32,
    pub graphics_command_queue_ring_size_bytes: u32,
    pub graphics_scratch_memory_size_bytes: u32,
    pub compute_scratch_memory_size_bytes: u32,
    pub disable_geometry_shader_allocations: i32,
    pub disable_tessellation_shader_allocations: i32,
}

/// `D3D12_BOX` (`d3d12.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct D3D12Box {
    pub left: u32,
    pub top: u32,
    pub front: u32,
    pub right: u32,
    pub bottom: u32,
    pub back: u32,
}

const _: () = {
    assert!(size_of::<D3D12XboxCreateDeviceParametersPrefix>() == 28);
    assert!(
        offset_of!(
            D3D12XboxCreateDeviceParametersPrefix,
            compute_scratch_memory_size_bytes
        ) == 16
    );
    assert!(size_of::<D3D12Box>() == 24);
    assert!(offset_of!(D3D12Box, right) == 12);
};

/// Dernière configuration reçue par `D3D12ConfigureVirtualMemoryX` (bloc brut de 32 octets).
static VIRTUAL_MEMORY_CONFIG: Mutex<Option<[u32; 8]>> = Mutex::new(None);

/// Configuration mémoire virtuelle GPU reçue (copie du bloc de 32 octets), si appelée.
pub fn virtual_memory_config() -> Option<[u32; 8]> {
    *VIRTUAL_MEMORY_CONFIG.lock()
}

/// Indique si `device` est un `ID3D12Device` ScarletBridge ; renvoie alors la disponibilité
/// d'un backend raytracing Vulkan utilisable (`None` pour un pointeur étranger).
pub(crate) fn device_raytracing_available(device: *mut c_void) -> Option<bool> {
    if device.is_null() {
        return None;
    }
    // SAFETY: tout objet COM commence par un pointeur de vtable ; on ne fait que le comparer.
    let vtbl = unsafe { (*device.cast::<ComHeader>()).vtbl };
    if vtbl != (&raw const D3D12_DEVICE_VTBL).cast::<c_void>() {
        return None;
    }
    // SAFETY: la vtable identifie un `D3D12DeviceObject` alloué par `D3D12CreateDevice`.
    let dev = unsafe { &*device.cast::<D3D12DeviceObject>() };
    let has_rt = dev.shared.renderer.lock().gpu.caps.has_ray_tracing;
    Some(has_rt && VK_AS_BACKEND_ENABLED)
}

fn resource_from_ptr<'a>(res: *mut c_void) -> Option<&'a D3D12ResourceObject> {
    if res.is_null() {
        return None;
    }
    // SAFETY: lecture du seul pointeur de vtable pour authentifier l'objet.
    let vtbl = unsafe { (*res.cast::<ComHeader>()).vtbl };
    if vtbl != (&raw const D3D12_RESOURCE_VTBL).cast::<c_void>() {
        return None;
    }
    // SAFETY: la vtable identifie un `D3D12ResourceObject` ScarletBridge vivant.
    Some(unsafe { &*res.cast::<D3D12ResourceObject>() })
}

/// Écrit `data` à `offset` dans le stockage CPU/GPU unifié d'une ressource buffer.
pub(crate) fn resource_write_bytes(
    res: *mut c_void,
    offset: u64,
    data: &[u8],
) -> Result<(), HResult> {
    let r = resource_from_ptr(res).ok_or(E_INVALIDARG)?;
    let off = usize::try_from(offset).map_err(|_| E_INVALIDARG)?;
    let end = off.checked_add(data.len()).ok_or(E_INVALIDARG)?;
    if end > r.byte_len {
        return Err(E_INVALIDARG);
    }
    let mut buf = r.backing.lock();
    let start = r.heap_offset + off;
    if start + data.len() > buf.len() {
        return Err(E_INVALIDARG);
    }
    buf[start..start + data.len()].copy_from_slice(data);
    drop(buf);
    r.gpu_texture_id.store(usize::MAX, Ordering::Release);
    Ok(())
}

/// Disposition d'une région de texture source (`GetCopyableFootprints`) : (octets/ligne, lignes,
/// pas de ligne aligné 256 o, taille minimale requise).
pub(crate) fn texture_region_source_layout(
    format: u32,
    bx: &D3D12Box,
) -> (usize, usize, usize, usize) {
    let w = bx.right.saturating_sub(bx.left).max(1);
    let h = bx.bottom.saturating_sub(bx.top).max(1);
    let (row_bytes, rows) = texture_row_layout(w, h, format);
    let pitch = row_bytes.next_multiple_of(256);
    let needed = pitch * (rows - 1) + row_bytes;
    (row_bytes, rows, pitch, needed)
}

/// Copie une région 2D (disposition source `GetCopyableFootprints`, pas aligné 256 o) dans le
/// mip 0 d'une texture (stockage linéaire dense, identique à `WriteToSubresource`).
pub(crate) fn resource_write_texture_region(
    res: *mut c_void,
    subresource: u32,
    bx: &D3D12Box,
    src: &[u8],
) -> Result<(), HResult> {
    let r = resource_from_ptr(res).ok_or(E_INVALIDARG)?;
    if r.desc.dimension < 2 || subresource != 0 {
        // Seul le mip 0 / slice 0 est matérialisé par le stockage linéaire actuel.
        return Err(E_INVALIDARG);
    }
    let tex_w = (r.desc.width as u32).max(1);
    let tex_h = r.desc.height.max(1);
    if bx.right <= bx.left
        || bx.bottom <= bx.top
        || bx.right > tex_w
        || bx.bottom > tex_h
        || bx.back.saturating_sub(bx.front) != 1
        || bx.front != 0
    {
        return Err(E_INVALIDARG);
    }
    let fmt = r.desc.format;
    let (rb4, rows4) = texture_row_layout(4, 4, fmt);
    let (block, bpe) = if rows4 == 1 {
        (4u32, rb4)
    } else {
        (1u32, rb4 / 4)
    };
    if !bx.left.is_multiple_of(block) || !bx.top.is_multiple_of(block) {
        return Err(E_INVALIDARG);
    }
    let (dst_pitch, dst_rows) = texture_row_layout(tex_w, tex_h, fmt);
    let (row_bytes, rows, src_pitch, needed) = texture_region_source_layout(fmt, bx);
    if src.len() < needed {
        return Err(E_INVALIDARG);
    }
    let first_row = (bx.top / block) as usize;
    let col_off = (bx.left / block) as usize * bpe;
    if first_row + rows > dst_rows || col_off + row_bytes > dst_pitch {
        return Err(E_INVALIDARG);
    }
    let mut buf = r.backing.lock();
    for y in 0..rows {
        let d = r.heap_offset + (first_row + y) * dst_pitch + col_off;
        let s = y * src_pitch;
        if d + row_bytes > buf.len() {
            return Err(E_INVALIDARG);
        }
        buf[d..d + row_bytes].copy_from_slice(&src[s..s + row_bytes]);
    }
    drop(buf);
    r.gpu_texture_id.store(usize::MAX, Ordering::Release);
    Ok(())
}

/// `HRESULT D3D12XboxCreateDevice(IDXGIAdapter* /*NULL en pratique*/,
/// const D3D12XBOX_CREATE_DEVICE_PARAMETERS*, REFIID, void**)` — aussi exporté par ordinal 13
/// (`D3D12ConsCreateDevice`). Le bloc de paramètres fait 24 dwords au layout non public : seul
/// le préfixe déduit est interprété, le reste est journalisé et les flags inconnus acceptés.
/// Crée le même périphérique Vulkan que `D3D12CreateDevice`.
pub(crate) extern "win64" fn D3D12XboxCreateDevice(
    adapter: *mut c_void,
    params: *const D3D12XboxCreateDeviceParametersPrefix,
    riid: *const Guid,
    out_device: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "D3D12XboxCreateDevice", E_UNEXPECTED, {
        if !out_device.is_null() {
            // SAFETY: `out_device` non-nul ; sortie remise à NULL en cas d'échec.
            unsafe {
                *out_device = ptr::null_mut();
            }
        }
        if params.is_null() {
            sb_warn!(
                CHANNEL,
                "D3D12XboxCreateDevice: paramètres NULL -> E_INVALIDARG"
            );
            return E_INVALIDARG;
        }
        // SAFETY: `params` non-nul ; seul le préfixe de 28 octets (dans le bloc de 96 octets) est lu.
        let p = unsafe { ptr::read_unaligned(params) };
        if p.version == 0 {
            sb_warn!(
                CHANNEL,
                "D3D12XboxCreateDevice: Version=0 (D3D12_SDK_VERSION attendu) -> E_INVALIDARG"
            );
            return E_INVALIDARG;
        }
        if p.process_debug_flags & D3D12XBOX_PROCESS_DEBUG_FLAG_DEBUG != 0 {
            let validation = std::env::var("SB_VK_VALIDATION").ok().as_deref() == Some("1");
            sb_info!(
                CHANNEL,
                "D3D12XboxCreateDevice: couche de débogage demandée -> validation Vulkan {}",
                if validation {
                    "active (SB_VK_VALIDATION=1)"
                } else {
                    "inactive (définir SB_VK_VALIDATION=1)"
                }
            );
        }
        if p.process_debug_flags & !D3D12XBOX_PROCESS_DEBUG_FLAG_DEBUG != 0 {
            sb_debug!(
                CHANNEL,
                "D3D12XboxCreateDevice: flags de débogage inconnus {:#x} acceptés sans effet",
                p.process_debug_flags & !D3D12XBOX_PROCESS_DEBUG_FLAG_DEBUG
            );
        }
        if p.graphics_command_queue_ring_size_bytes != 0
            || p.graphics_scratch_memory_size_bytes != 0
            || p.compute_scratch_memory_size_bytes != 0
            || p.disable_geometry_shader_allocations != 0
            || p.disable_tessellation_shader_allocations != 0
        {
            sb_debug!(
                CHANNEL,
                "D3D12XboxCreateDevice: ring={}o scratch gfx={}o compute={}o noGS={} noTess={} : sans équivalent Vulkan (gérés par le pilote) — ignorés",
                p.graphics_command_queue_ring_size_bytes,
                p.graphics_scratch_memory_size_bytes,
                p.compute_scratch_memory_size_bytes,
                p.disable_geometry_shader_allocations,
                p.disable_tessellation_shader_allocations
            );
        }
        sb_info!(
            CHANNEL,
            "D3D12XboxCreateDevice(adapter={adapter:p}, Version={:#x}, DebugFlags={:#x})",
            p.version,
            p.process_debug_flags
        );
        D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_12_2, riid, out_device)
    })
}

/// `HRESULT D3D12ConfigureVirtualMemoryX(const void* config)` (signature déduite d'appels
/// observés) : pointeur unique vers un bloc de 32 octets (mis à zéro par l'appelant, dword à +4
/// = compteur). L'appelant ne teste que l'échec. Les VA GPU de ScarletBridge sont unifiées avec
/// les VA CPU et gérées côté Vulkan/`mmap` : aucune réserve supplémentaire n'est nécessaire, la
/// configuration est validée, mémorisée et journalisée.
pub(crate) extern "win64" fn D3D12ConfigureVirtualMemoryX(config: *const u32) -> HResult {
    ffi_guard!(CHANNEL, "D3D12ConfigureVirtualMemoryX", E_UNEXPECTED, {
        if config.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: `config` non-nul pointe vers le bloc de 32 octets de l'appelant (lecture seule).
        let block = unsafe { ptr::read_unaligned(config.cast::<[u32; 8]>()) };
        sb_info!(
            CHANNEL,
            "D3D12ConfigureVirtualMemoryX(count={}, brut={block:08x?}) -> S_OK (VA GPU unifiées gérées par le backend Vulkan)",
            block[1]
        );
        *VIRTUAL_MEMORY_CONFIG.lock() = Some(block);
        S_OK
    })
}

#[cfg(test)]
mod xbox_ext_tests {
    use super::*;

    /// Chaque vtable D3D12 / DXGI couvre `VT_PAD_LEN` slots : un titre qui appelle une méthode
    /// d'une interface récente ou de la console (slot 150, par exemple) tombe sur un slot de
    /// réserve qui échoue proprement, jamais sur la donnée voisine.
    #[test]
    fn every_d3d12_vtable_is_padded_and_extra_slots_fail_cleanly() {
        macro_rules! check {
            ($vtbl:expr, $name:literal) => {{
                let vtbl = &raw const $vtbl;
                assert_eq!(
                    // SAFETY: lecture de la longueur du tableau d'une vtable statique.
                    unsafe { (*vtbl).slots.len() },
                    VT_PAD_LEN,
                    $name
                );
                // Faux objet COM : seul le premier mot (pointeur de vtable) est lu.
                let fake: [usize; 2] = [vtbl as usize, 0];
                assert_eq!(d3d_object_name(fake.as_ptr() as *mut c_void), $name);
                for idx in [100usize, 150, VT_PAD_LEN - 1] {
                    // SAFETY: `idx < VT_PAD_LEN` ; slot de réserve = fn `extern "win64"`.
                    let slot = unsafe { (*vtbl).slots[idx].0 };
                    // SAFETY: les slots de réserve ont la signature de `d3d_unimpl_slot`.
                    let f: extern "win64" fn(*mut c_void, usize, usize, usize) -> HResult =
                        unsafe { core::mem::transmute(slot) };
                    assert_eq!(f(fake.as_ptr() as *mut c_void, 0, 0, 0), E_NOTIMPL, $name);
                }
            }};
        }
        check!(D3D12_ROOTSIG_VTBL, "ID3D12RootSignature");
        check!(D3D12_PSO_VTBL, "ID3D12PipelineState");
        check!(D3D12_DESC_HEAP_VTBL, "ID3D12DescriptorHeap");
        check!(D3D12_HEAP_VTBL, "ID3D12Heap");
        check!(D3D12_RESOURCE_VTBL, "ID3D12Resource");
        check!(D3D12_FENCE_VTBL, "ID3D12Fence");
        check!(D3D12_CMD_ALLOC_VTBL, "ID3D12CommandAllocator");
        check!(D3D12_CMDLIST_VTBL, "ID3D12GraphicsCommandList");
        check!(D3D12_QUEUE_VTBL, "ID3D12CommandQueue");
        check!(D3D12_DEVICE_VTBL, "ID3D12Device");
        check!(DXGI_SC3_VTBL, "IDXGISwapChain3");
        check!(DXGI_FACTORY4_VTBL, "IDXGIFactory4");
    }

    #[test]
    fn xbox_create_device_validates_parameters_then_creates_vulkan_device() {
        let mut dev: *mut c_void = ptr::null_mut();
        assert_eq!(
            D3D12XboxCreateDevice(ptr::null_mut(), ptr::null(), &IID_ID3D12DEVICE, &mut dev),
            E_INVALIDARG
        );
        let bad = D3D12XboxCreateDeviceParametersPrefix::default();
        assert_eq!(
            D3D12XboxCreateDevice(ptr::null_mut(), &bad, &IID_ID3D12DEVICE, &mut dev),
            E_INVALIDARG
        );
        assert!(dev.is_null());
        let good = D3D12XboxCreateDeviceParametersPrefix {
            version: 0x0001_0000,
            process_debug_flags: D3D12XBOX_PROCESS_DEBUG_FLAG_DEBUG,
            graphics_command_queue_ring_size_bytes: 64 * 1024,
            ..Default::default()
        };
        let hr = D3D12XboxCreateDevice(ptr::null_mut(), &good, &IID_ID3D12DEVICE, &mut dev);
        if hr != S_OK {
            // Pas de GPU Vulkan dans l'environnement CI : échec propre attendu.
            assert_eq!(hr, E_FAIL);
            return;
        }
        assert!(!dev.is_null());
        // Pas de backend VK_KHR_acceleration_structure : raytracing indisponible.
        assert_eq!(device_raytracing_available(dev), Some(false));
        let mut not_dev = 0usize;
        assert_eq!(device_raytracing_available((&raw mut not_dev).cast()), None);
    }

    /// `OpenExistingHeapFromAddress` : valide l'adresse, partage la mémoire du titre sans copie.
    #[test]
    fn open_existing_heap_from_address_shares_guest_memory() {
        // Région committée de 64 Kio, comme `XMemVirtualAlloc` en fournirait une.
        // SAFETY: mapping anonyme privé de 64 Kio, libéré en fin de test.
        let mem = unsafe {
            libc::mmap(
                ptr::null_mut(),
                65536,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(mem, libc::MAP_FAILED);
        let dev = ptr::null_mut();
        let mut heap: *mut c_void = ptr::null_mut();

        // Paramètres invalides.
        assert_eq!(
            device_open_existing_heap_from_address(dev, ptr::null(), &IID_ID3D12HEAP, &mut heap),
            E_INVALIDARG
        );
        assert_eq!(
            device_open_existing_heap_from_address(
                dev,
                (mem as usize + 8) as *const c_void,
                &IID_ID3D12HEAP,
                &mut heap
            ),
            E_INVALIDARG,
            "adresse non alignée"
        );
        let bogus = Guid::new(7, 7, 7, [7; 8]);
        assert_eq!(
            device_open_existing_heap_from_address(dev, mem, &bogus, &mut heap),
            sb_common::abi::E_NOINTERFACE
        );
        assert!(heap.is_null());
        // Validation seule (`ppvHeap == NULL`).
        assert_eq!(
            device_open_existing_heap_from_address(dev, mem, &IID_ID3D12HEAP, ptr::null_mut()),
            S_FALSE_HR
        );

        // Succès : le tas couvre la région et partage la mémoire.
        assert_eq!(
            device_open_existing_heap_from_address(dev, mem, &IID_ID3D12HEAP, &mut heap),
            S_OK
        );
        assert!(!heap.is_null());
        // SAFETY: objet fraîchement créé par la fonction testée.
        let h = unsafe { &*heap.cast::<D3D12HeapObject>() };
        assert_eq!(h.base_addr, mem as usize);
        assert_eq!(h.size, 65536);
        assert_eq!(h.desc.size_in_bytes, 65536);
        h.backing.lock()[0] = 0xAB; // écrit via le tas…
        // SAFETY: lecture du premier octet de la région mappée.
        assert_eq!(
            unsafe { *mem.cast::<u8>() },
            0xAB,
            "…visible dans la mémoire du titre"
        );

        // Mémoire non mappée : refusée.
        // SAFETY: démappe la région pour obtenir une adresse libre (le tas créé plus haut a une
        // référence `Arc` retenue et n'est jamais touché après).
        unsafe { libc::munmap(mem, 65536) };
        let mut gone: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_open_existing_heap_from_address(dev, mem, &IID_ID3D12HEAP, &mut gone),
            E_INVALIDARG
        );
    }

    #[test]
    fn residency_methods_succeed_and_signal_the_fence() {
        assert_eq!(device_set_residency_priority(), S_OK);
        let fence = Box::new(D3D12FenceObject {
            header: ComHeader::new((&raw const D3D12_FENCE_VTBL).cast()),
            completed: AtomicU64::new(3),
        });
        let fence = Box::into_raw(fence);
        let dev = ptr::null_mut();
        assert_eq!(
            device_enqueue_make_resident(dev, 0, 0, ptr::null(), fence, 9),
            S_OK
        );
        // SAFETY: `fence` vient d'être créé ci-dessus.
        assert_eq!(unsafe { (*fence).completed.load(Ordering::Acquire) }, 9);
        assert_eq!(
            device_enqueue_make_resident(dev, 0, 0, ptr::null(), ptr::null_mut(), 1),
            E_INVALIDARG
        );
        assert_eq!(
            device_enqueue_make_resident(dev, 0, 2, ptr::null(), fence, 1),
            E_INVALIDARG
        );
    }

    #[test]
    fn configure_virtual_memory_validates_pointer_and_records_block() {
        assert_eq!(D3D12ConfigureVirtualMemoryX(ptr::null()), E_INVALIDARG);
        let block = [0u32, 7, 0, 0, 0, 0, 0, 0];
        assert_eq!(D3D12ConfigureVirtualMemoryX(block.as_ptr()), S_OK);
        assert_eq!(virtual_memory_config().map(|b| b[1]), Some(7));
    }

    #[test]
    fn texture_region_write_uses_linear_dense_layout() {
        let desc = D3D12ResourceDesc {
            dimension: 3,
            width: 8,
            height: 4,
            depth_or_array_size: 1,
            mip_levels: 1,
            format: 28,
            ..Default::default()
        };
        let res = alloc_d3d12_resource(desc, false);
        let bx = D3D12Box {
            left: 2,
            top: 1,
            front: 0,
            right: 4,
            bottom: 3,
            back: 1,
        };
        let (row_bytes, rows, pitch, needed) = texture_region_source_layout(28, &bx);
        assert_eq!((row_bytes, rows, pitch), (8, 2, 256));
        let mut src = vec![0u8; needed];
        src[..8].copy_from_slice(&[1; 8]);
        src[256..264].copy_from_slice(&[2; 8]);
        resource_write_texture_region(res.cast(), 0, &bx, &src).expect("écriture région");
        // SAFETY: ressource allouée ci-dessus.
        let r = unsafe { &*res };
        let buf = r.backing.lock();
        assert_eq!(&buf[32 + 8..32 + 16], &[1; 8]);
        assert_eq!(&buf[64 + 8..64 + 16], &[2; 8]);
        assert_eq!(buf[32 + 7], 0);
        drop(buf);
        assert_eq!(
            resource_write_texture_region(res.cast(), 1, &bx, &src),
            Err(E_INVALIDARG)
        );
        assert!(resource_write_bytes(res.cast(), 120, &[0; 16]).is_err());
        resource_write_bytes(res.cast(), 120, &[9; 8]).expect("écriture octets");
    }
}
// ==================== `dxgi.dll` (`IDXGIFactory4` + `IDXGISwapChain3`) ====================

#[repr(C)]
struct DxgiSwapChain3Vtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct DxgiSwapChain3Object {
    header: ComHeader,
    shared: Arc<SharedD3D12State>,
    width: u32,
    height: u32,
    buffer_count: u32,
    current_index: AtomicU32,
    backbuffers: Vec<*mut D3D12ResourceObject>,
}

extern "win64" fn sc3_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("IDXGISwapChain3", this, riid, ppv, &[IID_IDXGISWAPCHAIN3])
}

extern "win64" fn sc3_present(
    this: *mut DxgiSwapChain3Object,
    _sync_interval: u32,
    _flags: u32,
) -> HResult {
    ffi_guard!("dxgi", "IDXGISwapChain3::Present", E_UNEXPECTED, {
        if this.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: `this` valide.
        let sc = unsafe { &*this };
        let mut renderer = sc.shared.renderer.lock();

        if let Ok(path_str) = std::env::var("SB_DUMP_FRAME")
            && !path_str.is_empty()
        {
            let path = PathBuf::from(&path_str);
            match renderer.save_ppm(&path) {
                Ok(()) => sb_info!(
                    "dxgi",
                    "frame GPU sauvegardée dans '{}' ({}x{})",
                    path.display(),
                    renderer.width,
                    renderer.height
                ),
                Err(err) => sb_warn!("dxgi", "SB_DUMP_FRAME échoué: {err}"),
            }
        }

        if let Err(err) = renderer.present_to_surface() {
            sb_warn!("dxgi", "present_to_surface: {err}");
        }

        let count = sc.buffer_count.max(1);
        let prev = sc.current_index.fetch_add(1, Ordering::AcqRel);
        sb_debug!(
            "dxgi",
            "IDXGISwapChain3::Present(frame #{}, {}x{}) -> S_OK",
            prev + 1,
            sc.width,
            sc.height
        );
        let _ = (prev + 1) % count;
        S_OK
    })
}

extern "win64" fn sc3_present1(
    this: *mut DxgiSwapChain3Object,
    sync_interval: u32,
    flags: u32,
    _params: *const c_void,
) -> HResult {
    sc3_present(this, sync_interval, flags)
}

extern "win64" fn sc3_get_buffer(
    this: *mut DxgiSwapChain3Object,
    buffer_idx: u32,
    _riid: *const Guid,
    out_surface: *mut *mut c_void,
) -> HResult {
    if this.is_null() || out_surface.is_null() {
        return E_POINTER;
    }
    // SAFETY: `this` et `out_surface` vérifiés non-nuls.
    let sc = unsafe { &*this };
    let idx = (buffer_idx as usize) % sc.backbuffers.len().max(1);
    let bb = sc.backbuffers[idx];
    unsafe {
        (*bb).header.add_ref();
        *out_surface = bb.cast();
    }
    S_OK
}

extern "win64" fn sc3_get_current_backbuffer_index(this: *mut DxgiSwapChain3Object) -> u32 {
    if this.is_null() {
        return 0;
    }
    // SAFETY: `this` valide.
    let sc = unsafe { &*this };
    sc.current_index.load(Ordering::Acquire) % sc.buffer_count.max(1)
}

const fn build_sc3_vtbl() -> DxgiSwapChain3Vtbl {
    let mut slots = UNIMPL_SLOTS;
    let mut pad = 0;
    while pad < 42 {
        slots[pad] = vslot(noop_ok as *const c_void);
        pad += 1;
    }
    slots[0] = vslot(sc3_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[8] = vslot(sc3_present as *const c_void);
    slots[9] = vslot(sc3_get_buffer as *const c_void);
    slots[22] = vslot(sc3_present1 as *const c_void);
    slots[36] = vslot(sc3_get_current_backbuffer_index as *const c_void);
    DxgiSwapChain3Vtbl { slots }
}

static DXGI_SC3_VTBL: DxgiSwapChain3Vtbl = build_sc3_vtbl();

#[repr(C)]
struct DxgiFactory4Vtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct DxgiFactory4Object {
    header: ComHeader,
}

extern "win64" fn factory4_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("IDXGIFactory4", this, riid, ppv, &[IID_IDXGIFACTORY4])
}

fn create_swapchain_impl(
    queue_or_device: *mut c_void,
    desc: *const DxgiSwapChainDesc1,
    out_sc: *mut *mut c_void,
) -> HResult {
    if queue_or_device.is_null() || out_sc.is_null() {
        return E_POINTER;
    }
    let d = if desc.is_null() {
        DxgiSwapChainDesc1 {
            width: 256,
            height: 256,
            buffer_count: 2,
            ..Default::default()
        }
    } else {
        // SAFETY: `desc` vérifié non-nul.
        unsafe { *desc }
    };
    let width = if d.width == 0 { 256 } else { d.width };
    let height = if d.height == 0 { 256 } else { d.height };
    let count = d.buffer_count.clamp(1, 4);

    // En D3D12, `pDevice` passé à `CreateSwapChainForHwnd` est le `ID3D12CommandQueue*`.
    // SAFETY: `queue_or_device` est un `D3D12QueueObject*` (ou `D3D12DeviceObject*` qui a la même `shared` à l'offset après `ComHeader`).
    let q = unsafe { &*(queue_or_device.cast::<D3D12QueueObject>()) };
    let shared = Arc::clone(&q.shared);

    {
        let mut renderer = shared.renderer.lock();
        if let Err(err) = renderer.resize(width, height) {
            sb_warn!("dxgi", "resize VulkanRenderer({width}x{height}): {err}");
        }
        if !is_headless()
            && renderer.gpu.caps.has_swapchain
            && let Some(win) = Sdl3Window::new("ScarletBridge D3D12_XS", width, height)
            && let Some(surface) = win.create_vulkan_surface(renderer.gpu.instance.handle())
        {
            if let Err(err) = renderer.attach_surface(surface) {
                sb_warn!("dxgi", "attach_surface Wayland: {err}");
            } else {
                sb_info!(
                    "dxgi",
                    "fenêtre Wayland SDL3 attachée au swapchain D3D12_XS ({width}x{height})"
                );
                *shared.sdl_window.lock() = Some(win);
            }
        }
    }

    let bb_desc = D3D12ResourceDesc {
        dimension: 3, // TEXTURE2D
        width: u64::from(width),
        height,
        depth_or_array_size: 1,
        mip_levels: 1,
        format: 28, // DXGI_FORMAT_R8G8B8A8_UNORM
        sample_desc: DxgiSampleDesc {
            count: 1,
            quality: 0,
        },
        ..Default::default()
    };
    let mut backbuffers = Vec::with_capacity(count as usize);
    for _ in 0..count {
        backbuffers.push(alloc_d3d12_resource(bb_desc, true));
    }

    let sc = Box::new(DxgiSwapChain3Object {
        header: ComHeader::new((&raw const DXGI_SC3_VTBL).cast()),
        shared,
        width,
        height,
        buffer_count: count,
        current_index: AtomicU32::new(0),
        backbuffers,
    });
    // SAFETY: `out_sc` vérifié non-nul.
    unsafe {
        *out_sc = Box::into_raw(sc).cast();
    }
    sb_info!(
        "dxgi",
        "IDXGIFactory4::CreateSwapChain({width}x{height}, buffers={count}) -> S_OK"
    );
    S_OK
}

extern "win64" fn factory4_create_sc_hwnd(
    _this: *mut DxgiFactory4Object,
    device: *mut c_void,
    _hwnd: usize,
    desc: *const DxgiSwapChainDesc1,
    _fs_desc: *const c_void,
    _restrict_output: *mut c_void,
    out_sc: *mut *mut c_void,
) -> HResult {
    ffi_guard!("dxgi", "CreateSwapChainForHwnd", E_UNEXPECTED, {
        create_swapchain_impl(device, desc, out_sc)
    })
}

extern "win64" fn factory4_create_sc_corewindow(
    _this: *mut DxgiFactory4Object,
    device: *mut c_void,
    _window: *mut c_void,
    desc: *const DxgiSwapChainDesc1,
    _restrict_output: *mut c_void,
    out_sc: *mut *mut c_void,
) -> HResult {
    ffi_guard!("dxgi", "CreateSwapChainForCoreWindow", E_UNEXPECTED, {
        create_swapchain_impl(device, desc, out_sc)
    })
}

const fn build_factory4_vtbl() -> DxgiFactory4Vtbl {
    let mut slots = UNIMPL_SLOTS;
    let mut pad = 0;
    while pad < 32 {
        slots[pad] = vslot(noop_ok as *const c_void);
        pad += 1;
    }
    slots[0] = vslot(factory4_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[15] = vslot(factory4_create_sc_hwnd as *const c_void);
    slots[16] = vslot(factory4_create_sc_corewindow as *const c_void);
    DxgiFactory4Vtbl { slots }
}

static DXGI_FACTORY4_VTBL: DxgiFactory4Vtbl = build_factory4_vtbl();

pub(crate) extern "win64" fn CreateDXGIFactory1(
    _riid: *const Guid,
    out_factory: *mut *mut c_void,
) -> HResult {
    ffi_guard!("dxgi", "CreateDXGIFactory1", E_UNEXPECTED, {
        if out_factory.is_null() {
            return E_POINTER;
        }
        let f = Box::new(DxgiFactory4Object {
            header: ComHeader::new((&raw const DXGI_FACTORY4_VTBL).cast()),
        });
        // SAFETY: `out_factory` vérifié non-nul.
        unsafe {
            *out_factory = Box::into_raw(f).cast();
        }
        S_OK
    })
}

pub(crate) extern "win64" fn CreateDXGIFactory2(
    _flags: u32,
    riid: *const Guid,
    out_factory: *mut *mut c_void,
) -> HResult {
    CreateDXGIFactory1(riid, out_factory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_gpu::VertexPosColor;
    use sb_gpu::dxbc_to_spirv::{sample_triangle_ps_dxbc, sample_triangle_vs_dxbc};

    #[test]
    fn executes_full_d3d12_xs_com_triangle_and_reads_back_gpu_pixels() {
        // Exécution complète via les vtables COM D3D12_XS réelles (comme un binaire PE32+ invité).
        let mut dev_ptr: *mut c_void = ptr::null_mut();
        let hr = D3D12CreateDevice(ptr::null_mut(), 0xC000, &IID_ID3D12DEVICE, &mut dev_ptr);
        if hr != S_OK {
            // Pas de GPU Vulkan dans l'environnement CI.
            return;
        }
        assert!(!dev_ptr.is_null());

        let mut queue_ptr: *mut c_void = ptr::null_mut();
        let q_desc = D3D12CommandQueueDesc::default();
        assert_eq!(
            device_create_command_queue(
                dev_ptr.cast(),
                &q_desc,
                &IID_ID3D12COMMANDQUEUE,
                &mut queue_ptr
            ),
            S_OK
        );

        let mut factory_ptr: *mut c_void = ptr::null_mut();
        assert_eq!(
            CreateDXGIFactory2(0, &IID_IDXGIFACTORY4, &mut factory_ptr),
            S_OK
        );

        let sc_desc = DxgiSwapChainDesc1 {
            width: 128,
            height: 128,
            format: 28,
            buffer_count: 2,
            sample_desc: DxgiSampleDesc {
                count: 1,
                quality: 0,
            },
            ..Default::default()
        };
        let mut sc_ptr: *mut c_void = ptr::null_mut();
        assert_eq!(
            factory4_create_sc_hwnd(
                factory_ptr.cast(),
                queue_ptr,
                0,
                &sc_desc,
                ptr::null(),
                ptr::null_mut(),
                &mut sc_ptr
            ),
            S_OK
        );

        let mut bb_ptr: *mut c_void = ptr::null_mut();
        assert_eq!(
            sc3_get_buffer(sc_ptr.cast(), 0, &IID_ID3D12RESOURCE, &mut bb_ptr),
            S_OK
        );

        let vs = sample_triangle_vs_dxbc();
        let ps = sample_triangle_ps_dxbc();
        let pso_desc = D3D12GraphicsPipelineStateDesc {
            root_signature: ptr::null_mut(),
            vs: D3D12ShaderBytecode {
                shader_bytecode: vs.as_ptr(),
                bytecode_length: vs.len(),
            },
            ps: D3D12ShaderBytecode {
                shader_bytecode: ps.as_ptr(),
                bytecode_length: ps.len(),
            },
            ds: D3D12ShaderBytecode {
                shader_bytecode: ptr::null(),
                bytecode_length: 0,
            },
            hs: D3D12ShaderBytecode {
                shader_bytecode: ptr::null(),
                bytecode_length: 0,
            },
            gs: D3D12ShaderBytecode {
                shader_bytecode: ptr::null(),
                bytecode_length: 0,
            },
            stream_output: D3D12StreamOutputDesc {
                so_declaration: ptr::null(),
                num_entries: 0,
                _pad0: 0,
                buffer_strides: ptr::null(),
                num_strides: 0,
                rasterized_stream: 0,
            },
            blend_state: D3D12BlendDesc::default(),
            sample_mask: u32::MAX,
            rasterizer_state: D3D12RasterizerDesc::default(),
            depth_stencil_state: D3D12DepthStencilDesc::default(),
            input_layout: D3D12InputLayoutDesc {
                input_element_descs: ptr::null(),
                num_elements: 2,
                _pad: 0,
            },
            ib_strip_cut_value: 0,
            primitive_topology_type: 3,
            num_render_targets: 1,
            rtv_formats: [28, 0, 0, 0, 0, 0, 0, 0],
            dsv_format: 0,
            sample_desc: DxgiSampleDesc {
                count: 1,
                quality: 0,
            },
            node_mask: 0,
            cached_pso: D3D12CachedPipelineState {
                cached_blob: ptr::null(),
                cached_blob_size_in_bytes: 0,
            },
            flags: 0,
        };
        let mut pso_ptr: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_graphics_pso(
                dev_ptr.cast(),
                &pso_desc,
                &IID_ID3D12PIPELINESTATE,
                &mut pso_ptr
            ),
            S_OK
        );

        // Vertex Buffer (3 sommets RGB en NDC Direct3D : +Y en haut, -Y en bas).
        let verts = [
            VertexPosColor {
                pos: [0.0, 0.7, 0.0, 1.0],
                color: [1.0, 0.0, 0.0, 1.0],
            },
            VertexPosColor {
                pos: [0.7, -0.7, 0.0, 1.0],
                color: [0.0, 1.0, 0.0, 1.0],
            },
            VertexPosColor {
                pos: [-0.7, -0.7, 0.0, 1.0],
                color: [0.0, 0.0, 1.0, 1.0],
            },
        ];
        let vb_desc = D3D12ResourceDesc {
            dimension: 1,
            width: size_of_val(&verts) as u64,
            height: 1,
            depth_or_array_size: 1,
            mip_levels: 1,
            ..Default::default()
        };
        let mut vb_ptr: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_committed_resource(
                dev_ptr.cast(),
                ptr::null(),
                0,
                &vb_desc,
                0,
                ptr::null(),
                &IID_ID3D12RESOURCE,
                &mut vb_ptr
            ),
            S_OK
        );
        let mut mapped_vb: *mut c_void = ptr::null_mut();
        assert_eq!(res_map(vb_ptr.cast(), 0, ptr::null(), &mut mapped_vb), S_OK);
        // SAFETY: `mapped_vb` contient au moins `size_of_val(&verts)` octets.
        unsafe {
            ptr::copy_nonoverlapping(
                verts.as_ptr().cast::<u8>(),
                mapped_vb.cast::<u8>(),
                size_of_val(&verts),
            );
        }
        res_unmap(vb_ptr.cast(), 0, ptr::null());

        // Readback buffer pour lire la frame rendue côté invité.
        let rb_desc = D3D12ResourceDesc {
            dimension: 1,
            width: 128 * 128 * 4,
            height: 1,
            depth_or_array_size: 1,
            mip_levels: 1,
            ..Default::default()
        };
        let mut rb_ptr: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_committed_resource(
                dev_ptr.cast(),
                ptr::null(),
                0,
                &rb_desc,
                0,
                ptr::null(),
                &IID_ID3D12RESOURCE,
                &mut rb_ptr
            ),
            S_OK
        );

        let mut cl_ptr: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_command_list(
                dev_ptr.cast(),
                0,
                0,
                ptr::null_mut(),
                pso_ptr.cast(),
                &IID_ID3D12GRAPHICSCOMMANDLIST,
                &mut cl_ptr
            ),
            S_OK
        );

        let clear = [0.05f32, 0.05, 0.20, 1.0];
        let vp = D3D12Viewport {
            top_left_x: 0.0,
            top_left_y: 0.0,
            width: 128.0,
            height: 128.0,
            min_depth: 0.0,
            max_depth: 1.0,
        };
        let vbv = D3D12VertexBufferView {
            buffer_location: res_get_gpu_va(vb_ptr.cast()),
            size_in_bytes: size_of_val(&verts) as u32,
            stride_in_bytes: size_of::<VertexPosColor>() as u32,
        };
        cmdlist_clear_rtv(cl_ptr.cast(), 0, &clear, 0, ptr::null());
        cmdlist_rs_set_viewports(cl_ptr.cast(), 1, &vp);
        cmdlist_ia_set_vertex_buffers(cl_ptr.cast(), 0, 1, &vbv);
        cmdlist_draw_instanced(cl_ptr.cast(), 3, 1, 0, 0);
        cmdlist_copy_resource(cl_ptr.cast(), rb_ptr.cast(), bb_ptr.cast());
        assert_eq!(cmdlist_close(cl_ptr.cast()), S_OK);

        let cl_arr = [cl_ptr.cast::<D3D12CmdListObject>()];
        queue_execute_command_lists(queue_ptr.cast(), 1, cl_arr.as_ptr());

        let mut fence_ptr: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_fence(dev_ptr.cast(), 0, 0, &IID_ID3D12FENCE, &mut fence_ptr),
            S_OK
        );
        assert_eq!(queue_signal(queue_ptr.cast(), fence_ptr.cast(), 1), S_OK);
        assert_eq!(fence_get_completed(fence_ptr.cast()), 1);

        // Lecture des pixels réellement rendus par le GPU AMD RX 5700 XT !
        let mut mapped_rb: *mut c_void = ptr::null_mut();
        assert_eq!(res_map(rb_ptr.cast(), 0, ptr::null(), &mut mapped_rb), S_OK);
        // SAFETY: `mapped_rb` pointe vers 128*128*4 octets RGBA8.
        let pixels = unsafe { std::slice::from_raw_parts(mapped_rb.cast::<u8>(), 128 * 128 * 4) };
        let px = |x: usize, y: usize| -> [u8; 4] {
            let i = (y * 128 + x) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };

        // Coin haut-gauche : fond bleu nuit (~13, 13, 51, 255).
        let bg = px(4, 4);
        assert!(bg[2] > 40 && bg[0] < 25, "fond inattendu: {bg:?}");
        // Centre du triangle : mélange RGB.
        let center = px(64, 64);
        assert!(
            center[0] > 60 && center[1] > 40 && center[2] > 40,
            "centre triangle inattendu: {center:?}"
        );
        // Sommet haut : rouge dominant.
        let top = px(64, 24);
        assert!(
            top[0] > 180 && top[1] < 60,
            "sommet haut inattendu: {top:?}"
        );
        // Sommet bas-droit : vert dominant.
        let br = px(102, 102);
        assert!(
            br[1] > 180 && br[0] < 60,
            "sommet bas-droit inattendu: {br:?}"
        );
        // Sommet bas-gauche : bleu dominant.
        let bl = px(26, 102);
        assert!(
            bl[2] > 180 && bl[0] < 60,
            "sommet bas-gauche inattendu: {bl:?}"
        );
    }

    #[test]
    fn executes_d3d12_compute_shader_with_placed_heap_aliasing_and_feature_support() {
        use sb_gpu::sample_compute_transform_cs_dxbc;

        let mut dev_ptr: *mut c_void = ptr::null_mut();
        if D3D12CreateDevice(ptr::null_mut(), 0xC100, &IID_ID3D12DEVICE, &mut dev_ptr) != S_OK {
            return;
        }

        // 1. CheckFeatureSupport : Shader Model 6.x + ResourceBindingTier 3 + Feature Level 12_1
        let mut sm_data = 0x66u32;
        assert_eq!(
            device_check_feature_support(
                dev_ptr.cast(),
                7,
                (&raw mut sm_data).cast(),
                size_of::<u32>() as u32
            ),
            S_OK
        );
        assert!(sm_data >= 0x60, "SM 6.x attendu, reçu {sm_data:#x}");

        let mut opts = [0u32; 16];
        assert_eq!(
            device_check_feature_support(
                dev_ptr.cast(),
                0,
                opts.as_mut_ptr().cast(),
                size_of_val(&opts) as u32
            ),
            S_OK
        );
        assert_eq!(opts[4], 3, "ResourceBindingTier::TIER_3 attendu");

        // 2. CreateHeap (256 KiB) + deux CreatePlacedResource à offsets 0 et 65536 (aliasing réel)
        let heap_desc = D3D12HeapDesc {
            size_in_bytes: 256 * 1024,
            alignment: 65536,
            ..Default::default()
        };
        let mut heap_ptr: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_heap(dev_ptr.cast(), &heap_desc, &IID_ID3D12HEAP, &mut heap_ptr),
            S_OK
        );
        assert!(!heap_ptr.is_null());

        let buf_desc = D3D12ResourceDesc {
            dimension: 1, // BUFFER
            width: 256 * 16,
            height: 1,
            depth_or_array_size: 1,
            mip_levels: 1,
            ..Default::default()
        };
        let mut in_buf: *mut c_void = ptr::null_mut();
        let mut out_buf: *mut c_void = ptr::null_mut();
        let mut alias_buf: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_placed_resource(
                dev_ptr.cast(),
                heap_ptr.cast(),
                0,
                &buf_desc,
                0,
                ptr::null(),
                &IID_ID3D12RESOURCE,
                &mut in_buf
            ),
            S_OK
        );
        assert_eq!(
            device_create_placed_resource(
                dev_ptr.cast(),
                heap_ptr.cast(),
                65536,
                &buf_desc,
                0,
                ptr::null(),
                &IID_ID3D12RESOURCE,
                &mut out_buf
            ),
            S_OK
        );
        // Alias exact à l'offset 65536 sur le même heap !
        assert_eq!(
            device_create_placed_resource(
                dev_ptr.cast(),
                heap_ptr.cast(),
                65536,
                &buf_desc,
                0,
                ptr::null(),
                &IID_ID3D12RESOURCE,
                &mut alias_buf
            ),
            S_OK
        );
        assert_eq!(
            res_get_gpu_va(out_buf.cast()),
            res_get_gpu_va(in_buf.cast()) + 65536
        );
        assert_eq!(
            res_get_gpu_va(alias_buf.cast()),
            res_get_gpu_va(out_buf.cast())
        );

        // 3. RootSignature sérialisée : param 0 = CBV(b0), param 1 = UAV(u0), param 2 = UAV(u1)
        let params = [
            D3D12RootParameter {
                parameter_type: 2, // CBV
                data0: 0,          // b0
                ..Default::default()
            },
            D3D12RootParameter {
                parameter_type: 4, // UAV
                data0: 0,          // u0
                ..Default::default()
            },
            D3D12RootParameter {
                parameter_type: 4, // UAV
                data0: 1,          // u1
                ..Default::default()
            },
        ];
        let rs_desc = D3D12RootSignatureDesc {
            num_parameters: params.len() as u32,
            _pad0: 0,
            parameters: params.as_ptr(),
            num_static_samplers: 0,
            _pad1: 0,
            static_samplers: ptr::null(),
            flags: 0,
            _pad2: 0,
        };
        let mut rs_blob: *mut c_void = ptr::null_mut();
        assert_eq!(
            D3D12SerializeRootSignature(
                (&raw const rs_desc).cast(),
                1,
                &mut rs_blob,
                ptr::null_mut()
            ),
            S_OK
        );
        let mut root_sig: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_root_signature(
                dev_ptr.cast(),
                0,
                blob_get_ptr(rs_blob.cast()),
                blob_get_size(rs_blob.cast()),
                &IID_ID3D12ROOTSIGNATURE,
                &mut root_sig
            ),
            S_OK
        );

        // 4. Compute PSO + Dispatch(4, 1, 1) = 256 threads GPU
        let cs_dxbc = sample_compute_transform_cs_dxbc();
        let cs_desc = D3D12ComputePipelineStateDesc {
            root_signature: root_sig,
            cs: D3D12ShaderBytecode {
                shader_bytecode: cs_dxbc.as_ptr(),
                bytecode_length: cs_dxbc.len(),
            },
            node_mask: 0,
            _pad0: 0,
            cached_pso: D3D12CachedPipelineState {
                cached_blob: ptr::null(),
                cached_blob_size_in_bytes: 0,
            },
            flags: 0,
            _pad1: 0,
        };
        let mut cpso: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_compute_pso(
                dev_ptr.cast(),
                &cs_desc,
                &IID_ID3D12PIPELINESTATE,
                &mut cpso
            ),
            S_OK
        );

        // Remplit in_buf et cb_buf
        let mut p_in: *mut c_void = ptr::null_mut();
        assert_eq!(res_map(in_buf.cast(), 0, ptr::null(), &mut p_in), S_OK);
        // SAFETY: `p_in` pointe vers 256 * 4 f32.
        let in_f32 = unsafe { std::slice::from_raw_parts_mut(p_in.cast::<f32>(), 256 * 4) };
        for i in 0..256 {
            in_f32[i * 4] = i as f32;
            in_f32[i * 4 + 1] = (i as f32) * 0.5;
            in_f32[i * 4 + 2] = 1.0;
            in_f32[i * 4 + 3] = 2.0;
        }

        let mut cb_buf: *mut c_void = ptr::null_mut();
        let cb_desc = D3D12ResourceDesc {
            dimension: 1,
            width: 256,
            height: 1,
            depth_or_array_size: 1,
            mip_levels: 1,
            ..Default::default()
        };
        assert_eq!(
            device_create_committed_resource(
                dev_ptr.cast(),
                ptr::null(),
                0,
                &cb_desc,
                0,
                ptr::null(),
                &IID_ID3D12RESOURCE,
                &mut cb_buf
            ),
            S_OK
        );
        let mut p_cb: *mut c_void = ptr::null_mut();
        assert_eq!(res_map(cb_buf.cast(), 0, ptr::null(), &mut p_cb), S_OK);
        let cb_vals: [f32; 8] = [3.0, 2.0, 10.0, 0.5, 5.0, -1.0, 0.0, 4.0];
        unsafe {
            ptr::copy_nonoverlapping(cb_vals.as_ptr(), p_cb.cast::<f32>(), cb_vals.len());
        }

        let mut queue_ptr: *mut c_void = ptr::null_mut();
        let q_desc = D3D12CommandQueueDesc::default();
        assert_eq!(
            device_create_command_queue(
                dev_ptr.cast(),
                &q_desc,
                &IID_ID3D12COMMANDQUEUE,
                &mut queue_ptr
            ),
            S_OK
        );

        let mut cl_ptr: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_command_list(
                dev_ptr.cast(),
                0,
                0,
                ptr::null_mut(),
                cpso.cast(),
                &IID_ID3D12GRAPHICSCOMMANDLIST,
                &mut cl_ptr
            ),
            S_OK
        );

        cmdlist_set_compute_root_sig(cl_ptr.cast(), root_sig.cast());
        cmdlist_set_compute_root_cbv(cl_ptr.cast(), 0, res_get_gpu_va(cb_buf.cast()));
        cmdlist_set_compute_root_storage(cl_ptr.cast(), 1, res_get_gpu_va(in_buf.cast()));
        cmdlist_set_compute_root_storage(cl_ptr.cast(), 2, res_get_gpu_va(out_buf.cast()));
        cmdlist_dispatch(cl_ptr.cast(), 4, 1, 1);
        assert_eq!(cmdlist_close(cl_ptr.cast()), S_OK);

        let lists = [cl_ptr.cast::<D3D12CmdListObject>()];
        queue_execute_command_lists(queue_ptr.cast(), 1, lists.as_ptr());

        // Vérifie les résultats GPU via le buffer ALIASÉ (`alias_buf` au même offset 65536 du heap) !
        let mut p_alias: *mut c_void = ptr::null_mut();
        assert_eq!(
            res_map(alias_buf.cast(), 0, ptr::null(), &mut p_alias),
            S_OK
        );
        // SAFETY: `p_alias` pointe vers les 256 vec4 calculés par le Compute Shader sur le GPU.
        let out_f32 = unsafe { std::slice::from_raw_parts(p_alias.cast::<f32>(), 256 * 4) };
        for i in 0..256 {
            let f = i as f32;
            assert!((out_f32[i * 4] - (f * 3.0 + 5.0)).abs() < 1e-3);
            assert!((out_f32[i * 4 + 1] - (f * 0.5 * 2.0 - 1.0)).abs() < 1e-3);
            assert!((out_f32[i * 4 + 2] - 10.0).abs() < 1e-3);
            assert!((out_f32[i * 4 + 3] - 5.0).abs() < 1e-3);
        }
    }
}
