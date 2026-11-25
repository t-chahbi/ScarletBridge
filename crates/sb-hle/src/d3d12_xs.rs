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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

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
use crate::frame_pacer::FramePacer;
use crate::gfx10_descriptors::{
    self, BufferDesc, ImageDesc, ImageType, OobSelect, apply_d3d12_component_mapping,
    dxgi_to_gfx10_format,
};
use crate::root_signature::{
    DescriptorRange as RsDescriptorRange, ParsedRootSignature, RootParam as RsRootParam,
    RootParamKind, StaticSampler as RsStaticSampler, clear_xbox_root_sig_flags, parse_rts0,
    serialize_rts0_dxbc,
};
use crate::sync;

#[path = "d3d12_native.rs"]
mod native;

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
/// `IDXGIDevice` (dxgi.h). Les titres le récupèrent via `ID3D12Device::QueryInterface` : un
/// `E_NOINTERFACE` casse l'init graphique (résolution de l'adaptateur, latence des frames, etc.).
pub const IID_IDXGIDEVICE: Guid = Guid::new(
    0x54EC_77FA,
    0x1377,
    0x44E6,
    [0x8C, 0x32, 0x88, 0xFD, 0x5F, 0x44, 0xC8, 0x4C],
);
/// `IDXGIAdapter` (dxgi.h).
pub const IID_IDXGIADAPTER: Guid = Guid::new(
    0x2411_E7E1,
    0x12AC,
    0x4CCF,
    [0xBD, 0x14, 0x97, 0x98, 0xE8, 0x53, 0x4D, 0xC0],
);
/// `IDXGIOutput` (dxgi.h).
pub const IID_IDXGIOUTPUT: Guid = Guid::new(
    0xAE02_EEDB,
    0xC735,
    0x4690,
    [0x8D, 0x52, 0x5A, 0x8D, 0xC2, 0x02, 0x13, 0xAA],
);
/// `ID3D12Object` (`d3d12.h`).
pub const IID_ID3D12OBJECT: Guid = Guid::new(
    0xC4FE_C28F,
    0x7966,
    0x4E95,
    [0x9F, 0x94, 0xF4, 0x31, 0xCB, 0x56, 0xC3, 0xB8],
);
/// `ID3D12Device1`..`ID3D12Device4` (`d3d12.h`).
pub const IID_ID3D12DEVICE1: Guid = Guid::new(
    0x77AC_CE80,
    0x638E,
    0x4E65,
    [0x88, 0x95, 0xC1, 0xF2, 0x33, 0x86, 0x86, 0x3E],
);
pub const IID_ID3D12DEVICE2: Guid = Guid::new(
    0x30BA_A41E,
    0xB15B,
    0x475C,
    [0xA0, 0xBB, 0x1A, 0xF5, 0xC5, 0xB6, 0x43, 0x28],
);
pub const IID_ID3D12DEVICE3: Guid = Guid::new(
    0x81DA_DC15,
    0x2BAD,
    0x4392,
    [0x93, 0xC5, 0x10, 0x13, 0x45, 0xC4, 0xAA, 0x98],
);
pub const IID_ID3D12DEVICE4: Guid = Guid::new(
    0xE865_DF17,
    0xA9EE,
    0x46F9,
    [0xA4, 0x63, 0x30, 0x98, 0x31, 0x5A, 0xA2, 0xE5],
);
/// `ID3D12Device5`..`ID3D12Device10` (`d3d12.h`, vérifiés sur headers SDK/vkd3d).
pub const IID_ID3D12DEVICE5: Guid = Guid::new(
    0x8B4F_173B,
    0x2FEA,
    0x4B80,
    [0x8F, 0x58, 0x43, 0x07, 0x19, 0x1A, 0xB9, 0x5D],
);
pub const IID_ID3D12DEVICE6: Guid = Guid::new(
    0xC70B_221B,
    0x40E4,
    0x4A17,
    [0x89, 0xAF, 0x02, 0x5A, 0x07, 0x27, 0xA6, 0xDC],
);
pub const IID_ID3D12DEVICE7: Guid = Guid::new(
    0x5C01_4B53,
    0x68A1,
    0x4B9B,
    [0x8B, 0xD1, 0xDD, 0x60, 0x46, 0xB9, 0x35, 0x8B],
);
pub const IID_ID3D12DEVICE8: Guid = Guid::new(
    0x9218_E6BB,
    0xF944,
    0x4F7E,
    [0xA7, 0x5C, 0xB1, 0xB2, 0xC7, 0xB7, 0x01, 0xF3],
);
pub const IID_ID3D12DEVICE9: Guid = Guid::new(
    0x4C80_E962,
    0xF032,
    0x4F60,
    [0xBC, 0x9E, 0xEB, 0xC2, 0xCF, 0xA1, 0xD8, 0x3C],
);
pub const IID_ID3D12DEVICE10: Guid = Guid::new(
    0x517F_8718,
    0xAA66,
    0x49F9,
    [0xB0, 0x2B, 0xA7, 0xAB, 0x89, 0xC0, 0x60, 0x31],
);
/// `ID3D12DeviceChild`, `ID3D12Pageable`, `ID3D12CommandList`, `ID3D12GraphicsCommandList3..7`.
pub const IID_ID3D12DEVICECHILD: Guid = Guid::new(
    0x905D_B94B,
    0xA00C,
    0x4140,
    [0x9D, 0xF5, 0x2B, 0x64, 0xCA, 0x9E, 0xA3, 0x57],
);
pub const IID_ID3D12PAGEABLE: Guid = Guid::new(
    0x63EE_58FB,
    0x1268,
    0x4835,
    [0x86, 0xDA, 0xF0, 0x08, 0xCE, 0x62, 0xF0, 0xD6],
);
pub const IID_ID3D12COMMANDLIST: Guid = Guid::new(
    0x7116_D91C,
    0xE7E4,
    0x47CE,
    [0xB8, 0xC6, 0xEC, 0x81, 0x68, 0xF4, 0x37, 0xE5],
);
pub const IID_ID3D12GRAPHICSCOMMANDLIST3: Guid = Guid::new(
    0x6FDA_83A7,
    0xB84C,
    0x4E38,
    [0x9A, 0xC8, 0xC7, 0xBD, 0x22, 0x01, 0x6B, 0x3D],
);
pub const IID_ID3D12GRAPHICSCOMMANDLIST4: Guid = Guid::new(
    0x8754_318E,
    0xD3A9,
    0x4541,
    [0x98, 0xCF, 0x64, 0x5B, 0x50, 0xDC, 0x48, 0x74],
);
pub const IID_ID3D12GRAPHICSCOMMANDLIST5: Guid = Guid::new(
    0x5505_0859,
    0x4024,
    0x474C,
    [0x87, 0xF5, 0x64, 0x72, 0xEA, 0xEE, 0x44, 0xEA],
);
pub const IID_ID3D12GRAPHICSCOMMANDLIST6: Guid = Guid::new(
    0xC382_7890,
    0xE548,
    0x4CFA,
    [0x96, 0xCF, 0x56, 0x89, 0xA9, 0x37, 0x0F, 0x80],
);
pub const IID_ID3D12GRAPHICSCOMMANDLIST7: Guid = Guid::new(
    0xDD17_1223,
    0x8B61,
    0x4769,
    [0x90, 0xE3, 0x16, 0x0C, 0xCD, 0xE4, 0xE2, 0xC1],
);
pub const IID_ID3D12ROOTSIGNATUREDESERIALIZER: Guid = Guid::new(
    0x34AB_647B,
    0x3CC8,
    0x46AC,
    [0x84, 0x1B, 0xC0, 0x96, 0x56, 0x45, 0xC0, 0x46],
);
pub const IID_ID3D12VERSIONEDROOTSIGNATUREDESERIALIZER: Guid = Guid::new(
    0x7F91_CE67,
    0x090C,
    0x4BB7,
    [0xB7, 0x8E, 0xED, 0x8F, 0xF2, 0xE3, 0x1D, 0xA0],
);
/// `ID3D12CommandSignature` (`d3d12.h`).
pub const IID_ID3D12COMMANDSIGNATURE: Guid = Guid::new(
    0xC36A_797C,
    0xEC80,
    0x4F0A,
    [0x89, 0x85, 0xA7, 0xB2, 0x47, 0x50, 0x82, 0xD1],
);
/// `ID3D12QueryHeap` (`d3d12.h`).
pub const IID_ID3D12QUERYHEAP: Guid = Guid::new(
    0x0D96_58AE,
    0xED45,
    0x469E,
    [0xA6, 0x1D, 0x97, 0x0E, 0xC5, 0x83, 0xCA, 0xB4],
);
/// `ID3D12GraphicsCommandList1` / `2` (`d3d12.h`).
pub const IID_ID3D12GRAPHICSCOMMANDLIST1: Guid = Guid::new(
    0x5531_03FB,
    0x1FE7,
    0x4557,
    [0xBB, 0x38, 0x94, 0x6D, 0x7D, 0x0E, 0x7C, 0xA7],
);
pub const IID_ID3D12GRAPHICSCOMMANDLIST2: Guid = Guid::new(
    0x38C3_E585,
    0xFF17,
    0x412C,
    [0x91, 0x50, 0x4F, 0xC6, 0xF9, 0xD7, 0x2A, 0x28],
);
/// `ID3D12DmaCommandList` (liste DMA console Xbox Series X, type 6).
pub const IID_ID3D12CONSDMACOMMANDLIST: Guid = Guid::new(
    0x30BC_77B9,
    0x8379,
    0x4B25,
    [0x95, 0xDE, 0xB8, 0x58, 0x8C, 0x42, 0x27, 0x5D],
);
/// `ID3D12Fence1` (`d3d12.h`).
pub const IID_ID3D12FENCE1: Guid = Guid::new(
    0x4336_85FE,
    0xE22B,
    0x4CA0,
    [0xA8, 0xDB, 0xB5, 0xB4, 0xF4, 0xDD, 0x0E, 0x4A],
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

/// `D3D12_VERSIONED_ROOT_SIGNATURE_DESC` : `Version` (énum) puis l'union `Desc_1_0`/`Desc_1_1`.
/// Les deux membres d'union partagent `NumParameters` (offset 8) et `pParameters` (offset 16) ;
/// seul le type des paramètres diffère (v1.1 plus large), sans incidence sur les champs lus.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct D3D12VersionedRootSignatureDesc {
    pub version: u32,
    pub _pad0: u32,
    pub desc: D3D12RootSignatureDesc,
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

/// `D3D12XBOX_PRESENT_PLANE_PARAMETERS` (ABI console Xbox Series X, 0x48 octets).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct D3D12XboxPresentPlaneParameters {
    pub token: u64,
    pub resource_count: u32,
    pub _pad0: u32,
    pub pp_resources: *const *mut c_void,
    pub src_view_rects: *const c_void,
    pub dest_placement_base: *const [f32; 4],
    pub color_space: i32,
    pub scale_filter: i32,
    pub extended_desc_count: u32,
    pub _pad1: u32,
    pub extended_descs: *const c_void,
    pub flags: u32,
    pub _pad2: u32,
}

/// `D3D12XBOX_PRESENT_PARAMETERS` (ABI console Xbox Series X, 0x20 octets).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct D3D12XboxPresentParameters {
    pub immediate_threshold_percent: f32,
    pub view_count: u32,
    pub extended_desc_count: u32,
    pub _pad0: u32,
    pub extended_descs: *const c_void,
    pub flags: u32,
    pub _pad1: u32,
}

/// `D3D12XBOX_FRAME_OBJECT_LIST` passé à `ScheduleFrameEventX` / `WaitFrameEventX` (0x18 octets).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct D3D12XboxFrameObjectList {
    pub count: u32,
    pub _pad0: u32,
    pub objects: *const usize,
    pub signaled: *mut i32,
}

/// Enregistrement de statistiques de frame retourné par `GetFrameStatisticsX` (0x20 octets).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12XboxFrameStatistics {
    pub stat_type: u32,
    pub _pad0: u32,
    pub reserved: u64,
    pub origin_time_us: u64,
    pub flip_time_us: u64,
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
    assert!(offset_of!(D3D12VersionedRootSignatureDesc, version) == 0);
    assert!(offset_of!(D3D12VersionedRootSignatureDesc, desc) == 8);
    // Structs ABI console Xbox Series X (`PresentX` / frame pacing).
    assert!(size_of::<D3D12XboxPresentPlaneParameters>() == 0x48);
    assert!(offset_of!(D3D12XboxPresentPlaneParameters, token) == 0x00);
    assert!(offset_of!(D3D12XboxPresentPlaneParameters, resource_count) == 0x08);
    assert!(offset_of!(D3D12XboxPresentPlaneParameters, pp_resources) == 0x10);
    assert!(offset_of!(D3D12XboxPresentPlaneParameters, color_space) == 0x28);
    assert!(offset_of!(D3D12XboxPresentPlaneParameters, scale_filter) == 0x2C);
    assert!(offset_of!(D3D12XboxPresentPlaneParameters, extended_desc_count) == 0x30);
    assert!(offset_of!(D3D12XboxPresentPlaneParameters, extended_descs) == 0x38);
    assert!(offset_of!(D3D12XboxPresentPlaneParameters, flags) == 0x40);
    assert!(size_of::<D3D12XboxPresentParameters>() == 0x20);
    assert!(offset_of!(D3D12XboxPresentParameters, immediate_threshold_percent) == 0x00);
    assert!(offset_of!(D3D12XboxPresentParameters, view_count) == 0x04);
    assert!(offset_of!(D3D12XboxPresentParameters, extended_descs) == 0x10);
    assert!(offset_of!(D3D12XboxPresentParameters, flags) == 0x18);
    assert!(size_of::<D3D12XboxFrameObjectList>() == 0x18);
    assert!(offset_of!(D3D12XboxFrameObjectList, objects) == 0x08);
    assert!(offset_of!(D3D12XboxFrameObjectList, signaled) == 0x10);
    assert!(size_of::<D3D12XboxFrameStatistics>() == 0x20);
    assert!(offset_of!(D3D12XboxFrameStatistics, origin_time_us) == 0x10);
    assert!(offset_of!(D3D12XboxFrameStatistics, flip_time_us) == 0x18);
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
    reg!(
        "D3D12CreateRootSignatureDeserializer",
        D3D12CreateRootSignatureDeserializer
    );
    reg!(
        "D3D12CreateVersionedRootSignatureDeserializer",
        D3D12CreateVersionedRootSignatureDeserializer
    );
    reg!(
        "D3D12EnableExperimentalFeatures",
        D3D12EnableExperimentalFeatures
    );
    reg!("D3D12UploadCustomMicrocodeX", D3D12UploadCustomMicrocodeX);
    reg!("D3D12ZeroMemoryX", D3D12ZeroMemoryX);
    reg!("D3D10CreateBlob", D3D10CreateBlob);
    reg!("D3DFlushEntireCpuCache", D3DFlushEntireCpuCache);
    reg!("DXGIXGetFrameStatistics", DXGIXGetFrameStatistics);
    reg!("DXGIXPresentArray", DXGIXPresentArray);
    reg!("DXGIXSetFrameNotification", DXGIXSetFrameNotification);
    reg!("DXGIXSetVLineNotification", DXGIXSetVLineNotification);
    reg!(
        "D3D12XboxEnableMemoryProfiling",
        D3D12XboxEnableMemoryProfiling
    );
    reg!(
        "D3D12XboxGetProcessDebugFlags",
        D3D12XboxGetProcessDebugFlags
    );
    reg!(
        "D3D12XboxSetProcessDebugFlags",
        D3D12XboxSetProcessDebugFlags
    );
    reg!("D3D12XboxUnloadDriverModule", D3D12XboxUnloadDriverModule);
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
    /// Images créées par le bridge pour les T# du chemin natif RDNA 2.
    native_images: native::ImageCache,
    // Cadencement d'images console (`SetFrameIntervalX` / `WaitFrameEventX` / `PresentX`).
    frame_interval_us: AtomicU32,
    next_frame_token: AtomicU64,
    present_count: AtomicU64,
    start_instant: Instant,
    /// `true` lorsque le périphérique a été créé via `D3D12XboxCreateDevice` (ou a reçu un
    /// appel console `slot 48`) : les files de commandes démarrent alors directement dans le
    /// layout console récent (`slot 18` = `GetTimestampFrequency` sur 8 octets, jamais `GetDesc`
    /// sur 16 octets qui écraserait la pile de l'appelant).
    is_console_device: AtomicBool,
}

impl SharedD3D12State {
    fn new(renderer: VulkanRenderer) -> Self {
        Self {
            renderer: Mutex::new(renderer),
            sdl_window: Mutex::new(None),
            descriptors: Mutex::new(HashMap::new()),
            native_images: Mutex::new(HashMap::new()),
            frame_interval_us: AtomicU32::new(16_667),
            // 0 est réservé (`D3D12XBOX_FRAME_PIPELINE_TOKEN_NULL`) : commence à 1.
            next_frame_token: AtomicU64::new(1),
            present_count: AtomicU64::new(0),
            start_instant: Instant::now(),
            is_console_device: AtomicBool::new(false),
        }
    }

    fn alloc_frame_token(&self) -> u64 {
        let raw = self.next_frame_token.fetch_add(1, Ordering::AcqRel);
        let masked = raw & 0x0000_FFFF_FFFF_FFFF;
        if masked == 0 { 1 } else { masked }
    }

    fn elapsed_ns(&self) -> u64 {
        (self.start_instant.elapsed().as_nanos() as u64).max(1)
    }

    fn elapsed_us(&self) -> u64 {
        (self.start_instant.elapsed().as_micros() as u64).max(1)
    }
}

fn is_headless() -> bool {
    cfg!(test) || std::env::var("SB_HEADLESS").ok().as_deref() == Some("1")
}

// ==================== Objets COM D3D12 ====================

extern "win64" fn noop_ok() -> HResult {
    S_OK
}

/// `DXGI_ERROR_NOT_FOUND` (`0x887A0002`) : donnée privée absente dans `GetPrivateData`.
const DXGI_ERROR_NOT_FOUND: HResult = 0x887A_0002u32 as i32;
/// `DXGI_ERROR_MORE_DATA` (`0x887A0003`) : tampon insuffisant (`GetPrivateData`, `GetDisplayModeList`).
const DXGI_ERROR_MORE_DATA: HResult = 0x887A_0003u32 as i32;

/// Dernier `ID3D12Device` actif créé par le bridge (sert à `ID3D12DeviceChild::GetDevice`).
static LAST_D3D12_DEVICE: AtomicUsize = AtomicUsize::new(0);

/// Les tests qui créent un périphérique partagent l'état global `LAST_D3D12_DEVICE` : ils
/// s'exécutent l'un après l'autre.
#[cfg(test)]
static D3D_TEST_SERIAL: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn test_serial() -> impl Drop {
    D3D_TEST_SERIAL.lock()
}

enum PrivateDataEntry {
    Bytes(Vec<u8>),
    Interface(usize),
}

static PRIVATE_DATA: std::sync::LazyLock<Mutex<HashMap<(usize, Guid), PrivateDataEntry>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// `ID3D12Object::GetPrivateData` (slot 3) / `IDXGIObject::GetPrivateData` (slot 5).
extern "win64" fn d3d12_obj_get_private_data(
    this: *mut ComHeader,
    guid: *const Guid,
    data_size: *mut u32,
    data: *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "GetPrivateData", E_UNEXPECTED, {
        if this.is_null() || guid.is_null() || data_size.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: `guid` et `data_size` vérifiés non-nuls.
        let key = (this as usize, unsafe { ptr::read_unaligned(guid) });
        let cap = unsafe { ptr::read_unaligned(data_size) } as usize;
        let map = PRIVATE_DATA.lock();
        let Some(entry) = map.get(&key) else {
            return DXGI_ERROR_NOT_FOUND;
        };
        match entry {
            PrivateDataEntry::Bytes(bytes) => {
                let needed = bytes.len();
                // SAFETY: `data_size` vérifié non-nul.
                unsafe { ptr::write_unaligned(data_size, needed as u32) };
                if data.is_null() {
                    return S_OK;
                }
                if cap < needed {
                    return DXGI_ERROR_MORE_DATA;
                }
                if needed > 0 {
                    // SAFETY: `data` non-nul et `cap >= needed`.
                    unsafe {
                        ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast::<u8>(), needed);
                    }
                }
                S_OK
            }
            PrivateDataEntry::Interface(iface_ptr) => {
                let needed = size_of::<usize>();
                // SAFETY: `data_size` vérifié non-nul.
                unsafe { ptr::write_unaligned(data_size, needed as u32) };
                if data.is_null() {
                    return S_OK;
                }
                if cap < needed {
                    return DXGI_ERROR_MORE_DATA;
                }
                let ptr_val = *iface_ptr;
                drop(map);
                // Incrémenter le compteur COM (`IUnknown::AddRef`, slot 1) si l'objet est mappé.
                if ptr_val >= 0x1_0000
                    && sb_loader::trampoline::peek_memory(ptr_val, size_of::<usize>()).len()
                        == size_of::<usize>()
                {
                    // SAFETY: `ptr_val` est un pointeur COM stocké via `SetPrivateDataInterface`.
                    let vtbl = unsafe { *(ptr_val as *const *const usize) };
                    if !vtbl.is_null()
                        && sb_loader::trampoline::peek_memory(vtbl as usize, 16).len() == 16
                    {
                        // SAFETY: slot 1 de `IUnknown` = `extern "win64" fn(*mut c_void) -> u32`.
                        let add_ref: extern "win64" fn(*mut c_void) -> u32 =
                            unsafe { core::mem::transmute(*vtbl.add(1)) };
                        let _ = add_ref(ptr_val as *mut c_void);
                    }
                }
                // SAFETY: `data` non-nul et `cap >= size_of::<usize>()`.
                unsafe { ptr::write_unaligned(data.cast::<usize>(), ptr_val) };
                S_OK
            }
        }
    })
}

/// `ID3D12Object::SetPrivateData` (slot 4) / `IDXGIObject::SetPrivateData` (slot 3).
extern "win64" fn d3d12_obj_set_private_data(
    this: *mut ComHeader,
    guid: *const Guid,
    data_size: u32,
    data: *const c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "SetPrivateData", E_UNEXPECTED, {
        if this.is_null() || guid.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: `guid` vérifié non-nul.
        let key = (this as usize, unsafe { ptr::read_unaligned(guid) });
        let mut map = PRIVATE_DATA.lock();
        if data_size == 0 || data.is_null() {
            map.remove(&key);
            return S_OK;
        }
        // SAFETY: l'appelant garantit `data_size` octets lisibles à `data`.
        let slice = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), data_size as usize) };
        map.insert(key, PrivateDataEntry::Bytes(slice.to_vec()));
        sb_debug!(
            CHANNEL,
            "SetPrivateData(obj={this:p}, guid={}, len={data_size}) -> S_OK",
            key.1
        );
        S_OK
    })
}

/// `ID3D12Object::SetPrivateDataInterface` (slot 5) / `IDXGIObject::SetPrivateDataInterface` (slot 4).
extern "win64" fn d3d12_obj_set_private_data_interface(
    this: *mut ComHeader,
    guid: *const Guid,
    data: *const c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "SetPrivateDataInterface", E_UNEXPECTED, {
        if this.is_null() || guid.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: `guid` vérifié non-nul.
        let key = (this as usize, unsafe { ptr::read_unaligned(guid) });
        let mut map = PRIVATE_DATA.lock();
        if data.is_null() {
            map.remove(&key);
            return S_OK;
        }
        map.insert(key, PrivateDataEntry::Interface(data as usize));
        sb_debug!(
            CHANNEL,
            "SetPrivateDataInterface(obj={this:p}, guid={}, iface={data:p}) -> S_OK",
            key.1
        );
        S_OK
    })
}

/// `ID3D12Object::SetName` (slot 6).
extern "win64" fn d3d12_obj_set_name(_this: *mut ComHeader, _name: *const u16) -> HResult {
    S_OK
}

/// `ID3D12DeviceChild::GetDevice` (slot 7) : renvoie le périphérique `ID3D12Device` actif
/// (ou `IDXGIDevice` si demandé via `riid`), avec incrément de référence COM.
extern "win64" fn d3d12_child_get_device(
    _this: *mut ComHeader,
    riid: *const Guid,
    ppv_device: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "ID3D12DeviceChild::GetDevice", E_UNEXPECTED, {
        if ppv_device.is_null() {
            return E_POINTER;
        }
        let dev = LAST_D3D12_DEVICE.load(Ordering::Acquire);
        if dev == 0 {
            // SAFETY: `ppv_device` vérifié non-nul.
            unsafe { *ppv_device = ptr::null_mut() };
            return E_FAIL;
        }
        let hr = device_qi(dev as *mut ComHeader, riid, ppv_device);
        sb_debug!(CHANNEL, "ID3D12DeviceChild::GetDevice() -> {hr:#x}");
        hr
    })
}

/// Remplit les slots `ID3D12Object` (`3..=6`) sur une vtable D3D12.
const fn fill_d3d12_object_slots(slots: &mut [VtblSlot; VT_PAD_LEN]) {
    slots[3] = vslot(d3d12_obj_get_private_data as *const c_void);
    slots[4] = vslot(d3d12_obj_set_private_data as *const c_void);
    slots[5] = vslot(d3d12_obj_set_private_data_interface as *const c_void);
    slots[6] = vslot(d3d12_obj_set_name as *const c_void);
}

/// Remplit les slots `ID3D12DeviceChild` (`3..=7`) sur une vtable D3D12.
const fn fill_d3d12_child_slots(slots: &mut [VtblSlot; VT_PAD_LEN]) {
    fill_d3d12_object_slots(slots);
    slots[7] = vslot(d3d12_child_get_device as *const c_void);
}

/// Remplit les slots `IDXGIObject` (`3..=5`) sur une vtable DXGI.
const fn fill_dxgi_object_slots(slots: &mut [VtblSlot; VT_PAD_LEN]) {
    slots[3] = vslot(d3d12_obj_set_private_data as *const c_void);
    slots[4] = vslot(d3d12_obj_set_private_data_interface as *const c_void);
    slots[5] = vslot(d3d12_obj_get_private_data as *const c_void);
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

/// Tables de méthodes tracées (`SB_TRACE_API`) : `(original → copie, copie → original)`.
type VtblMaps = (HashMap<usize, usize>, HashMap<usize, usize>);
static TRACED_VTBLS: std::sync::LazyLock<Mutex<VtblMaps>> =
    std::sync::LazyLock::new(|| Mutex::new((HashMap::new(), HashMap::new())));

/// Construit une copie de `orig` (`VT_PAD_LEN` slots) dont chaque slot passe par un thunk de trace
/// nommé `<objet>::slotN` : les appels de méthodes COM apparaissent alors dans le tampon d'appels du
/// rapport de crash, avec leurs quatre premiers arguments et un aperçu des chaînes pointées.
fn build_traced_vtbl(orig: *const c_void, name: &str) -> *const c_void {
    // SAFETY: toutes les vtables D3D12/DXGI étendues sont des `[VtblSlot; VT_PAD_LEN]` statiques.
    let slots = unsafe { core::slice::from_raw_parts(orig.cast::<VtblSlot>(), VT_PAD_LEN) };
    let traced: Vec<VtblSlot> = slots
        .iter()
        .enumerate()
        .map(|(i, slot)| {
            let thunk = sb_loader::trampoline::allocate_trace_thunk(
                "d3d12",
                &format!("{name}::slot{i}"),
                slot.0 as usize,
            );
            vslot(thunk as *const c_void)
        })
        .collect();
    Box::leak(traced.into_boxed_slice()).as_ptr().cast()
}

/// Table de méthodes à donner à un objet : l'originale, ou sa copie tracée si `SB_TRACE_API` est actif.
fn traced_vtbl(orig: *const c_void, name: &'static str) -> *const c_void {
    if sb_loader::trampoline::api_trace_mode() == sb_loader::trampoline::ApiTraceMode::Off {
        return orig;
    }
    let mut maps = TRACED_VTBLS.lock();
    if let Some(&t) = maps.0.get(&(orig as usize)) {
        return t as *const c_void;
    }
    let t = build_traced_vtbl(orig, name);
    maps.0.insert(orig as usize, t as usize);
    maps.1.insert(t as usize, orig as usize);
    t
}

/// Table d'origine d'une table éventuellement tracée (identité si elle n'est pas tracée).
fn original_vtbl(v: *const c_void) -> *const c_void {
    TRACED_VTBLS
        .lock()
        .1
        .get(&(v as usize))
        .map_or(v, |&o| o as *const c_void)
}

/// Résultat renvoyé par un slot D3D12/DXGI non implémenté.
///
/// Par défaut `S_OK` : les méthodes propres à la console (cadencement d'image, tampons de commandes,
/// réglages) sont presque toutes des réglages que le titre vérifie ; un échec les fait s'arrêter sur
/// une assertion alors qu'un succès sans effet les laisse poursuivre. `SB_D3D12_UNIMPL=notimpl`
/// rétablit `E_NOTIMPL` (utile pour repérer une méthode dont le titre lit réellement le résultat).
fn unimplemented_slot_result() -> HResult {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let ok = *OK.get_or_init(|| {
        !std::env::var("SB_D3D12_UNIMPL").is_ok_and(|v| v.eq_ignore_ascii_case("notimpl"))
    });
    if ok { S_OK } else { E_NOTIMPL }
}

/// Nom de l'objet D3D12/DXGI dont `this` est une instance d'après son pointeur de vtable, ou
/// `None` si `this` n'est pas un objet du bridge. Lecture sûre : `this` peut être n'importe quelle
/// valeur envoyée par le titre.
fn d3d_known_object_name(this: usize) -> Option<&'static str> {
    if this == 0 {
        return None;
    }
    let head = sb_loader::trampoline::peek_memory(this, 8);
    if head.len() < 8 {
        return None;
    }
    let vtbl = original_vtbl(usize::from_le_bytes([
        head[0], head[1], head[2], head[3], head[4], head[5], head[6], head[7],
    ]) as *const c_void) as usize;
    let known: &[(usize, &'static str)] = &[
        (
            (&raw const D3D12_ROOTSIG_VTBL) as usize,
            "ID3D12RootSignature",
        ),
        (
            (&raw const D3D12_CMDSIG_VTBL) as usize,
            "ID3D12CommandSignature",
        ),
        (
            (&raw const D3D12_QUERY_HEAP_VTBL) as usize,
            "ID3D12QueryHeap",
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
        (
            (&raw const D3D12_CONSOLE_CMDLIST_VTBL) as usize,
            "ID3D12GraphicsCommandList",
        ),
        (
            (&raw const D3D12_DMA_CMDLIST_VTBL) as usize,
            "ID3D12DmaCommandList",
        ),
        ((&raw const D3D12_QUEUE_VTBL) as usize, "ID3D12CommandQueue"),
        ((&raw const D3D12_DEVICE_VTBL) as usize, "ID3D12Device"),
        ((&raw const DXGI_DEVICE_VTBL) as usize, "IDXGIDevice"),
        ((&raw const DXGI_ADAPTER_VTBL) as usize, "IDXGIAdapter"),
        ((&raw const DXGI_OUTPUT_VTBL) as usize, "IDXGIOutput"),
        ((&raw const DXGI_SC3_VTBL) as usize, "IDXGISwapChain3"),
        ((&raw const DXGI_FACTORY4_VTBL) as usize, "IDXGIFactory4"),
    ];
    known
        .iter()
        .find(|(addr, _)| *addr == vtbl)
        .map(|(_, n)| *n)
}

/// Nom de l'objet D3D12 dont `this` est une instance (diagnostic d'un slot non implémenté).
fn d3d_object_name(this: *mut c_void) -> &'static str {
    if this.is_null() {
        return "objet NULL";
    }
    d3d_known_object_name(this as usize).unwrap_or("objet D3D12/DXGI")
}

/// Décrit un argument reçu d'un titre pour un journal : octets pointés, IID nommé, et — quand la
/// valeur ou l'un des huit premiers mots pointés désigne un objet du bridge — son type. Cela
/// permet de reconnaître qu'un argument est une file, un tas, un tableau d'objets, etc.
fn describe_d3d_arg(v: usize) -> String {
    let mut s = crate::com::describe_guest_arg(v);
    if let Some(n) = d3d_known_object_name(v) {
        s.push_str(&format!(" [objet du bridge : {n}]"));
    }
    let bytes = sb_loader::trampoline::peek_memory(v, 64);
    for (i, chunk) in bytes.as_chunks::<8>().0.iter().enumerate() {
        let q = u64::from_le_bytes(*chunk) as usize;
        if q > 0xFFFF
            && let Some(n) = d3d_known_object_name(q)
        {
            s.push_str(&format!(" [mot {i} = {q:#x} : objet du bridge {n}]"));
        }
    }
    s
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
            "{name}::slot {N} non implémenté appelé (this={this:p}, a2={}, a3={}, a4={}) -> {} (SB_D3D12_UNIMPL=notimpl pour E_NOTIMPL)",
            describe_d3d_arg(a2),
            describe_d3d_arg(a3),
            describe_d3d_arg(a4),
            if unimplemented_slot_result() == S_OK {
                "S_OK par tolérance"
            } else {
                "E_NOTIMPL"
            }
        );
    }
    unimplemented_slot_result()
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
    /// Disposition des paramètres racine (nature, user-SGPR) pour le chemin natif RDNA 2.
    layout: Option<Arc<native::RootSigLayout>>,
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
    slots[0] = vslot(rootsig_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    fill_d3d12_child_slots(&mut slots);
    D3D12RootSigVtbl { slots }
}

static D3D12_ROOTSIG_VTBL: D3D12RootSigVtbl = build_rootsig_vtbl();

// --- ID3D12CommandSignature ---

#[repr(C)]
struct D3D12CmdSigVtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct D3D12CmdSigObject {
    header: ComHeader,
    byte_stride: u32,
}

extern "win64" fn cmdsig_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "ID3D12CommandSignature",
        this,
        riid,
        ppv,
        &[IID_ID3D12COMMANDSIGNATURE, IID_ID3D12OBJECT],
    )
}

const fn build_cmdsig_vtbl() -> D3D12CmdSigVtbl {
    let mut slots = UNIMPL_SLOTS;
    slots[0] = vslot(cmdsig_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    fill_d3d12_child_slots(&mut slots);
    D3D12CmdSigVtbl { slots }
}

static D3D12_CMDSIG_VTBL: D3D12CmdSigVtbl = build_cmdsig_vtbl();

// --- ID3D12QueryHeap (slot 42 : CreateQueryHeap) ---

#[repr(C)]
struct D3D12QueryHeapVtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct D3D12QueryHeapObject {
    header: ComHeader,
    heap_type: u32,
    count: u32,
}

extern "win64" fn query_heap_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "ID3D12QueryHeap",
        this,
        riid,
        ppv,
        &[IID_ID3D12QUERYHEAP, IID_ID3D12OBJECT],
    )
}

const fn build_query_heap_vtbl() -> D3D12QueryHeapVtbl {
    let mut slots = UNIMPL_SLOTS;
    slots[0] = vslot(query_heap_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    fill_d3d12_child_slots(&mut slots);
    D3D12QueryHeapVtbl { slots }
}

static D3D12_QUERY_HEAP_VTBL: D3D12QueryHeapVtbl = build_query_heap_vtbl();

// --- ID3D12RootSignatureDeserializer / ID3D12VersionedRootSignatureDeserializer ---

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct D3D12DescriptorRangeAbi {
    range_type: u32,
    num_descriptors: u32,
    base_shader_register: u32,
    register_space: u32,
    offset_in_table: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct D3D12DescriptorRange1Abi {
    range_type: u32,
    num_descriptors: u32,
    base_shader_register: u32,
    register_space: u32,
    flags: u32,
    offset_in_table: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct D3D12StaticSamplerDescAbi {
    filter: u32,
    address_u: u32,
    address_v: u32,
    address_w: u32,
    mip_lod_bias: f32,
    max_anisotropy: u32,
    comparison_func: u32,
    border_color: u32,
    min_lod: f32,
    max_lod: f32,
    shader_register: u32,
    register_space: u32,
    shader_visibility: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct D3D12StaticSamplerDesc1Abi {
    base: D3D12StaticSamplerDescAbi,
    flags: u32,
}

const _: () = {
    assert!(size_of::<D3D12DescriptorRangeAbi>() == 20);
    assert!(size_of::<D3D12DescriptorRange1Abi>() == 24);
    assert!(size_of::<D3D12StaticSamplerDescAbi>() == 52);
    assert!(size_of::<D3D12StaticSamplerDesc1Abi>() == 56);
};

#[repr(C)]
struct D3D12RootSigDeserVtbl {
    slots: [VtblSlot; 5],
}

#[repr(C)]
struct D3D12RootSigDeserObject {
    header: ComHeader,
    desc: D3D12VersionedRootSignatureDesc,
    // Tampons propriétaires pointés par `desc` (gardés vivants avec l'objet COM).
    _owned_params: Vec<D3D12RootParameter>,
    _owned_ranges_v10: Vec<Vec<D3D12DescriptorRangeAbi>>,
    _owned_ranges_v11: Vec<Vec<D3D12DescriptorRange1Abi>>,
    _owned_samplers_v10: Vec<D3D12StaticSamplerDescAbi>,
    _owned_samplers_v12: Vec<D3D12StaticSamplerDesc1Abi>,
}

impl D3D12RootSigDeserObject {
    fn from_parsed(parsed: &ParsedRootSignature) -> Box<Self> {
        let use_v11 = parsed.version >= 2;
        let use_v12_samplers = parsed.version >= 3;
        let mut owned_ranges_v10 = Vec::new();
        let mut owned_ranges_v11 = Vec::new();
        let mut owned_params = Vec::with_capacity(parsed.params.len());

        for p in &parsed.params {
            match p.kind {
                RootParamKind::Table => {
                    let ranges_ptr = if use_v11 {
                        let v: Vec<D3D12DescriptorRange1Abi> = p
                            .ranges
                            .iter()
                            .map(|r| D3D12DescriptorRange1Abi {
                                range_type: r.range_type,
                                num_descriptors: r.num_descriptors,
                                base_shader_register: r.base_shader_register,
                                register_space: r.register_space,
                                flags: r.flags,
                                offset_in_table: r.offset_in_table,
                            })
                            .collect();
                        let ptr_val = if v.is_empty() { 0 } else { v.as_ptr() as u64 };
                        owned_ranges_v11.push(v);
                        ptr_val
                    } else {
                        let v: Vec<D3D12DescriptorRangeAbi> = p
                            .ranges
                            .iter()
                            .map(|r| D3D12DescriptorRangeAbi {
                                range_type: r.range_type,
                                num_descriptors: r.num_descriptors,
                                base_shader_register: r.base_shader_register,
                                register_space: r.register_space,
                                offset_in_table: r.offset_in_table,
                            })
                            .collect();
                        let ptr_val = if v.is_empty() { 0 } else { v.as_ptr() as u64 };
                        owned_ranges_v10.push(v);
                        ptr_val
                    };
                    owned_params.push(D3D12RootParameter {
                        parameter_type: 0,
                        _pad0: 0,
                        data0: p.ranges.len() as u32,
                        data1: 0,
                        data2: ranges_ptr,
                        shader_visibility: p.visibility,
                        _pad1: 0,
                    });
                }
                RootParamKind::Constants => {
                    owned_params.push(D3D12RootParameter {
                        parameter_type: 1,
                        _pad0: 0,
                        data0: p.shader_register,
                        data1: p.register_space,
                        data2: u64::from(p.num_values),
                        shader_visibility: p.visibility,
                        _pad1: 0,
                    });
                }
                RootParamKind::Cbv | RootParamKind::Srv | RootParamKind::Uav => {
                    let pt = match p.kind {
                        RootParamKind::Cbv => 2,
                        RootParamKind::Srv => 3,
                        _ => 4,
                    };
                    owned_params.push(D3D12RootParameter {
                        parameter_type: pt,
                        _pad0: 0,
                        data0: p.shader_register,
                        data1: p.register_space,
                        data2: if use_v11 {
                            u64::from(p.descriptor_flags)
                        } else {
                            0
                        },
                        shader_visibility: p.visibility,
                        _pad1: 0,
                    });
                }
            }
        }

        let mut owned_samplers_v10 = Vec::new();
        let mut owned_samplers_v12 = Vec::new();
        let samplers_ptr: *const c_void = if use_v12_samplers {
            owned_samplers_v12 = parsed
                .static_samplers
                .iter()
                .map(|s| D3D12StaticSamplerDesc1Abi {
                    base: D3D12StaticSamplerDescAbi {
                        filter: s.filter,
                        address_u: s.address_u,
                        address_v: s.address_v,
                        address_w: s.address_w,
                        mip_lod_bias: s.mip_lod_bias,
                        max_anisotropy: s.max_anisotropy,
                        comparison_func: s.comparison_func,
                        border_color: s.border_color,
                        min_lod: s.min_lod,
                        max_lod: s.max_lod,
                        shader_register: s.shader_register,
                        register_space: s.register_space,
                        shader_visibility: s.visibility,
                    },
                    flags: s.flags,
                })
                .collect();
            if owned_samplers_v12.is_empty() {
                ptr::null()
            } else {
                owned_samplers_v12.as_ptr().cast()
            }
        } else {
            owned_samplers_v10 = parsed
                .static_samplers
                .iter()
                .map(|s| D3D12StaticSamplerDescAbi {
                    filter: s.filter,
                    address_u: s.address_u,
                    address_v: s.address_v,
                    address_w: s.address_w,
                    mip_lod_bias: s.mip_lod_bias,
                    max_anisotropy: s.max_anisotropy,
                    comparison_func: s.comparison_func,
                    border_color: s.border_color,
                    min_lod: s.min_lod,
                    max_lod: s.max_lod,
                    shader_register: s.shader_register,
                    register_space: s.register_space,
                    shader_visibility: s.visibility,
                })
                .collect();
            if owned_samplers_v10.is_empty() {
                ptr::null()
            } else {
                owned_samplers_v10.as_ptr().cast()
            }
        };

        let params_ptr = if owned_params.is_empty() {
            ptr::null()
        } else {
            owned_params.as_ptr()
        };
        Box::new(Self {
            header: ComHeader::new((&raw const D3D12_ROOTSIG_DESER_VTBL).cast()),
            desc: D3D12VersionedRootSignatureDesc {
                version: parsed.version.clamp(1, 3),
                _pad0: 0,
                desc: D3D12RootSignatureDesc {
                    num_parameters: owned_params.len() as u32,
                    _pad0: 0,
                    parameters: params_ptr,
                    num_static_samplers: parsed.static_samplers.len() as u32,
                    _pad1: 0,
                    static_samplers: samplers_ptr,
                    flags: parsed.flags,
                    _pad2: 0,
                },
            },
            _owned_params: owned_params,
            _owned_ranges_v10: owned_ranges_v10,
            _owned_ranges_v11: owned_ranges_v11,
            _owned_samplers_v10: owned_samplers_v10,
            _owned_samplers_v12: owned_samplers_v12,
        })
    }
}

extern "win64" fn rootsig_deser_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "ID3D12RootSignatureDeserializer",
        this,
        riid,
        ppv,
        &[
            IID_ID3D12ROOTSIGNATUREDESERIALIZER,
            IID_ID3D12VERSIONEDROOTSIGNATUREDESERIALIZER,
        ],
    )
}

extern "win64" fn rootsig_deser_get_desc(
    this: *mut D3D12RootSigDeserObject,
) -> *const D3D12RootSignatureDesc {
    if this.is_null() {
        return ptr::null();
    }
    // SAFETY: `this` est un `D3D12RootSigDeserObject` valide.
    unsafe { &raw const (*this).desc.desc }
}

extern "win64" fn rootsig_deser_get_unconverted_desc(
    this: *mut D3D12RootSigDeserObject,
) -> *const D3D12VersionedRootSignatureDesc {
    if this.is_null() {
        return ptr::null();
    }
    // SAFETY: `this` est un `D3D12RootSigDeserObject` valide.
    unsafe { &raw const (*this).desc }
}

const fn build_rootsig_deser_vtbl() -> D3D12RootSigDeserVtbl {
    D3D12RootSigDeserVtbl {
        slots: [
            vslot(rootsig_deser_qi as *const c_void),
            vslot(com_add_ref as *const c_void),
            vslot(com_release as *const c_void),
            vslot(rootsig_deser_get_desc as *const c_void),
            vslot(rootsig_deser_get_unconverted_desc as *const c_void),
        ],
    }
}

static D3D12_ROOTSIG_DESER_VTBL: D3D12RootSigDeserVtbl = build_rootsig_deser_vtbl();

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
    /// Métadonnées des shaders `SBR2` (chemin natif RDNA 2) ; `None` pour DXBC/DXIL.
    native: Option<Arc<native::NativePso>>,
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
    slots[0] = vslot(pso_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    fill_d3d12_child_slots(&mut slots);
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

extern "win64" fn heap_get_desc(
    this: *mut D3D12DescHeapObject,
    out_desc: *mut D3D12DescriptorHeapDesc,
) -> *mut D3D12DescriptorHeapDesc {
    if !this.is_null() && !out_desc.is_null() {
        // SAFETY: `this` et `out_desc` vérifiés non-nuls (ABI Win64 retour struct > 8o en RDX).
        unsafe {
            *out_desc = (*this).desc;
        }
    }
    out_desc
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
    slots[0] = vslot(heap_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    fill_d3d12_child_slots(&mut slots);
    slots[8] = vslot(heap_get_desc as *const c_void);
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
    slots[0] = vslot(d3dheap_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    fill_d3d12_child_slots(&mut slots);
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
    /// Layout console tuilé de l'allocation (placée dans la mémoire XMem) : `None` tant que le
    /// layout n'est pas connu explicitement (comportement linéaire historique). Voir
    /// `mark_resource_tiled`.
    swizzle_mode: Option<crate::gfx10_tiling::TileMode>,
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

extern "win64" fn res_read_from_subresource(
    this: *mut D3D12ResourceObject,
    dst_data: *mut u8,
    dst_row_pitch: u32,
    _dst_depth_pitch: u32,
    _src_subresource: u32,
    _src_box: *const c_void,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "ID3D12Resource::ReadFromSubresource",
        E_UNEXPECTED,
        {
            if this.is_null() || dst_data.is_null() {
                return E_INVALIDARG;
            }
            // SAFETY: `this` et `dst_data` vérifiés non-nuls.
            let res = unsafe { &*this };
            let (row_bytes, num_rows) = texture_row_layout(
                (res.desc.width as u32).max(1),
                res.desc.height.max(1),
                res.desc.format,
            );
            let pitch = (dst_row_pitch as usize).max(row_bytes);
            let src = res.backing.lock();
            for y in 0..num_rows {
                let s_off = res.heap_offset + y * row_bytes;
                if s_off + row_bytes > src.len() {
                    break;
                }
                // SAFETY: l'appelant garantit `num_rows` lignes de `pitch` octets à `dst_data`.
                unsafe {
                    ptr::copy_nonoverlapping(
                        src[s_off..s_off + row_bytes].as_ptr(),
                        dst_data.add(y * pitch),
                        row_bytes,
                    );
                }
            }
            S_OK
        }
    )
}

extern "win64" fn res_get_heap_properties(
    this: *mut D3D12ResourceObject,
    out_props: *mut D3D12HeapProperties,
    out_flags: *mut u32,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "ID3D12Resource::GetHeapProperties",
        E_UNEXPECTED,
        {
            if this.is_null() {
                return E_INVALIDARG;
            }
            if !out_props.is_null() {
                let props = D3D12HeapProperties {
                    heap_type: 4,              // D3D12_HEAP_TYPE_CUSTOM (UMA console)
                    cpu_page_property: 3,      // D3D12_CPU_PAGE_PROPERTY_WRITE_BACK
                    memory_pool_preference: 1, // D3D12_MEMORY_POOL_L0
                    creation_node_mask: 1,
                    visible_node_mask: 1,
                };
                // SAFETY: `out_props` vérifié non-nul.
                unsafe { ptr::write_unaligned(out_props, props) };
            }
            if !out_flags.is_null() {
                // SAFETY: `out_flags` vérifié non-nul.
                unsafe { ptr::write_unaligned(out_flags, 0) };
            }
            S_OK
        }
    )
}

const fn build_res_vtbl() -> D3D12ResourceVtbl {
    let mut slots = UNIMPL_SLOTS;
    slots[0] = vslot(res_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    fill_d3d12_child_slots(&mut slots);
    slots[8] = vslot(res_map as *const c_void);
    slots[9] = vslot(res_unmap as *const c_void);
    slots[10] = vslot(res_get_desc as *const c_void);
    slots[11] = vslot(res_get_gpu_va as *const c_void);
    slots[12] = vslot(res_write_to_subresource as *const c_void);
    slots[13] = vslot(res_read_from_subresource as *const c_void);
    slots[14] = vslot(res_get_heap_properties as *const c_void);
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
        header: ComHeader::new(traced_vtbl(
            (&raw const D3D12_RESOURCE_VTBL).cast(),
            "ID3D12Resource",
        )),
        desc,
        is_backbuffer,
        backing: Arc::new(Mutex::new(backing)),
        heap_offset: 0,
        byte_len,
        fixed_addr,
        gpu_texture_id: std::sync::atomic::AtomicUsize::new(usize::MAX),
        swizzle_mode: None,
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
        header: ComHeader::new(traced_vtbl(
            (&raw const D3D12_RESOURCE_VTBL).cast(),
            "ID3D12Resource",
        )),
        desc,
        is_backbuffer: false,
        backing: Arc::clone(&heap.backing),
        heap_offset: offset,
        byte_len,
        fixed_addr,
        gpu_texture_id: std::sync::atomic::AtomicUsize::new(usize::MAX),
        swizzle_mode: None,
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
    waiters: Mutex<Vec<(u64, usize)>>,
}

const _: () = {
    assert!(offset_of!(D3D12FenceObject, completed) == 16);
};

impl D3D12FenceObject {
    fn signal_value(&self, value: u64) {
        let prev = self.completed.fetch_max(value, Ordering::AcqRel);
        let now = prev.max(value);
        let mut to_wake = Vec::new();
        {
            let mut waiters = self.waiters.lock();
            waiters.retain(|&(target, handle)| {
                if now >= target {
                    to_wake.push(handle);
                    false
                } else {
                    true
                }
            });
        }
        for handle in to_wake {
            if handle != 0 {
                sync::SetEvent(handle);
            }
        }
    }
}

extern "win64" fn fence_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "ID3D12Fence",
        this,
        riid,
        ppv,
        &[IID_ID3D12FENCE, IID_ID3D12FENCE1, IID_ID3D12OBJECT],
    )
}

extern "win64" fn fence_get_completed(this: *mut D3D12FenceObject) -> u64 {
    if this.is_null() {
        return 0;
    }
    // SAFETY: `this` est un `D3D12FenceObject` valide.
    let f = unsafe { &*this };
    f.completed.load(Ordering::Acquire)
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
    let f = unsafe { &*this };
    if event_handle == 0 {
        return S_OK;
    }
    let mut waiters = f.waiters.lock();
    if f.completed.load(Ordering::Acquire) >= value {
        drop(waiters);
        sync::SetEvent(event_handle);
    } else {
        waiters.push((value, event_handle));
    }
    S_OK
}

extern "win64" fn fence_signal(this: *mut D3D12FenceObject, value: u64) -> HResult {
    if this.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: `this` est un `D3D12FenceObject` valide.
    let f = unsafe { &*this };
    f.signal_value(value);
    S_OK
}

/// `ID3D12Fence::slot 12` (console Xbox `d3d12_xs`) : retourne directement dans `RAX` le
/// pointeur (`const volatile UINT64*`) vers le compteur 64 bits `completed` en mémoire unifiée,
/// afin que le titre lise `*pCompletedValue` sans appel COM (et vérifie `!= UINT64_MAX`).
extern "win64" fn fence_slot12_get_completed_address(this: *mut D3D12FenceObject) -> *const u64 {
    ffi_guard!(CHANNEL, "ID3D12Fence::slot12", ptr::null(), {
        if this.is_null() {
            return ptr::null();
        }
        // SAFETY: `this` est un `D3D12FenceObject` immobile en tas dont `completed: AtomicU64`
        // partage le layout mémoire exact d'un `u64` naturellement aligné (offset 16).
        unsafe { core::ptr::addr_of!((*this).completed).cast::<u64>() }
    })
}

const fn build_fence_vtbl() -> D3D12FenceVtbl {
    let mut slots = UNIMPL_SLOTS;
    slots[0] = vslot(fence_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    fill_d3d12_child_slots(&mut slots);
    slots[8] = vslot(fence_get_completed as *const c_void);
    slots[9] = vslot(fence_set_event as *const c_void);
    slots[10] = vslot(fence_signal as *const c_void);
    slots[11] = vslot(noop_ok as *const c_void); // ID3D12Fence1::GetCreationFlags -> 0
    slots[12] = vslot(fence_slot12_get_completed_address as *const c_void);
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
    oom_callback: AtomicUsize,
    oom_context: AtomicUsize,
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
        &[
            IID_ID3D12COMMANDALLOCATOR,
            IID_ID3D12PAGEABLE,
            IID_ID3D12DEVICECHILD,
            IID_ID3D12OBJECT,
        ],
    )
}

/// Slot 9 console sur `ID3D12CommandAllocator` : enregistrement d'un callback d'allocation /
/// out-of-memory console `(this, pCallback, pContext, flags)`.
extern "win64" fn alloc_slot9_set_callback(
    this: *mut D3D12CmdAllocObject,
    callback: usize,
    context: usize,
    flags: usize,
) -> HResult {
    ffi_guard!(CHANNEL, "ID3D12CommandAllocator::slot9", E_UNEXPECTED, {
        if !this.is_null() {
            // SAFETY: `this` vérifié non-nul.
            let a = unsafe { &*this };
            a.oom_callback.store(callback, Ordering::Release);
            a.oom_context.store(context, Ordering::Release);
        }
        sb_debug!(
            CHANNEL,
            "ID3D12CommandAllocator::slot9(cb={callback:#x}, ctx={context:#x}, flags={flags:#x}) -> S_OK"
        );
        S_OK
    })
}

const fn build_alloc_vtbl() -> D3D12CmdAllocVtbl {
    let mut slots = UNIMPL_SLOTS;
    slots[0] = vslot(alloc_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    fill_d3d12_child_slots(&mut slots);
    slots[8] = vslot(noop_ok as *const c_void); // Reset
    slots[9] = vslot(alloc_slot9_set_callback as *const c_void); // Callback console
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
    CopyBufferRegion {
        dst: usize,
        dst_offset: u64,
        src: usize,
        src_offset: u64,
        num_bytes: u64,
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
    /// `DrawInstanced` avec un PSO natif RDNA 2 (user-SGPR capturés à l'enregistrement).
    NativeDraw {
        pso: Arc<native::NativePso>,
        pipeline_id: usize,
        user_data: Vec<u32>,
        counts: (u32, u32, u32, u32),
    },
    /// `Dispatch` avec un PSO natif RDNA 2.
    NativeDispatch {
        pso: Arc<native::NativePso>,
        pipeline_id: usize,
        user_data: Vec<u32>,
        groups: (u32, u32, u32),
    },
    WriteValue32 {
        address: u64,
        value: u32,
    },
    WriteValue64 {
        address: u64,
        value: u64,
        flags: u32,
    },
}

#[repr(C)]
struct D3D12CmdListVtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct D3D12CmdListObject {
    header: ComHeader,
    /// En-tête de tampon de commandes en ligne de la console (`+0x10..+0x570`) : mis à zéro
    /// pour que `write_ptr (+0x18) == buffer_end (+0x30) == 0`, ce qui force tout code inline
    /// du SDK console à passer proprement par les appels virtuels COM sans corrompre l'état Rust.
    console_inline_pad: [u8; 0x560],
    ops: Mutex<Vec<RecordedOp>>,
    closed: Mutex<bool>,
    graphics_root_regs: Mutex<Vec<u32>>,
    compute_root_regs: Mutex<Vec<u32>>,
    /// Arguments racine / PSO courants pour le chemin natif RDNA 2.
    native: Mutex<native::NativeListState>,
    /// Arènes d'upload allouées via `AllocateUploadMemory` (slot 148).
    upload_arenas: Mutex<Vec<Vec<u8>>>,
    /// Type de liste de commandes (`D3D12_COMMAND_LIST_TYPE` : 0=DIRECT, 2=COMPUTE, 3=COPY, 6=DMA).
    list_type: u32,
}

const _: () = {
    assert!(offset_of!(D3D12CmdListObject, console_inline_pad) == 0x10);
    assert!(offset_of!(D3D12CmdListObject, ops) >= 0x570);
};

impl D3D12CmdListObject {
    /// `Draw` d'un PSO natif : capture les user-SGPR courants (arguments racine graphiques).
    fn native_draw_op(&self, counts: (u32, u32, u32, u32)) -> Option<RecordedOp> {
        let n = self.native.lock();
        let (pipeline_id, pso) = n.gfx_pso.clone()?;
        Some(RecordedOp::NativeDraw {
            pso,
            pipeline_id,
            user_data: n.gfx.user_data(),
            counts,
        })
    }

    /// `Dispatch` d'un PSO natif : capture les user-SGPR courants (arguments racine compute).
    fn native_dispatch_op(&self, groups: (u32, u32, u32)) -> Option<RecordedOp> {
        let n = self.native.lock();
        let (pipeline_id, pso) = n.cs_pso.clone()?;
        Some(RecordedOp::NativeDispatch {
            pso,
            pipeline_id,
            user_data: n.cs.user_data(),
            groups,
        })
    }

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
        &[
            IID_ID3D12GRAPHICSCOMMANDLIST,
            IID_ID3D12GRAPHICSCOMMANDLIST1,
            IID_ID3D12GRAPHICSCOMMANDLIST2,
            IID_ID3D12GRAPHICSCOMMANDLIST3,
            IID_ID3D12GRAPHICSCOMMANDLIST4,
            IID_ID3D12GRAPHICSCOMMANDLIST5,
            IID_ID3D12GRAPHICSCOMMANDLIST6,
            IID_ID3D12GRAPHICSCOMMANDLIST7,
            IID_ID3D12COMMANDLIST,
            IID_ID3D12CONSDMACOMMANDLIST,
            IID_ID3D12DEVICECHILD,
            IID_ID3D12OBJECT,
        ],
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
    *cl.native.lock() = native::NativeListState::default();
    if d3d_known_object_name(initial_state as usize) == Some("ID3D12PipelineState") {
        // SAFETY: `initial_state` validé comme `D3D12PsoObject` du bridge.
        let pso = unsafe { &*initial_state };
        cl.native
            .lock()
            .note_pso(pso.pipeline_id, pso.is_compute, &pso.native);
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
    if let Some(op) = cl.native_draw_op((
        vertex_count,
        instance_count.max(1),
        start_vertex,
        start_instance,
    )) {
        cl.ops.lock().push(op);
        return;
    }
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
    if cl.native.lock().gfx_pso.is_some() {
        sb_warn!(
            CHANNEL,
            "DrawIndexedInstanced ignoré : non supporté avec un pipeline natif RDNA 2 (le VS lit ses sommets par V#)"
        );
        return;
    }
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
    if let Some(op) = cl.native_dispatch_op((
        thread_group_x.max(1),
        thread_group_y.max(1),
        thread_group_z.max(1),
    )) {
        cl.ops.lock().push(op);
        return;
    }
    cl.ops.lock().push(RecordedOp::ComputeDispatch {
        x: thread_group_x.max(1),
        y: thread_group_y.max(1),
        z: thread_group_z.max(1),
    });
}

/// Marque une ressource comme tuilée (`swizzle_mode`) : à n'appeler que lorsque le layout console
/// de l'allocation est connu explicitement. Sans marquage (`None`), les copies restent linéaires.
///
/// # Safety
/// `res` doit désigner un `D3D12ResourceObject` valide, sans autre accès concurrent (à utiliser
/// juste après la création de la ressource).
#[allow(dead_code)]
unsafe fn mark_resource_tiled(res: *mut D3D12ResourceObject, mode: crate::gfx10_tiling::TileMode) {
    // SAFETY: garanti par l'appelant (voir ci-dessus).
    unsafe { (*res).swizzle_mode = Some(mode) };
}

/// Paramètres de tuilage `(mode, largeur, hauteur, format)` de la sous-ressource 0 d'une texture
/// marquée tuilée.
fn tile_params(
    res: &D3D12ResourceObject,
) -> Option<(crate::gfx10_tiling::TileMode, u32, u32, u32)> {
    let mode = res.swizzle_mode?;
    if !(2..=3).contains(&res.desc.dimension) {
        return None;
    }
    let fmt = if res.desc.format != 0 {
        res.desc.format
    } else {
        28
    };
    Some((
        mode,
        res.desc.width.min(u64::from(u32::MAX)).max(1) as u32,
        res.desc.height.max(1),
        fmt,
    ))
}

/// Tuile `linear` (image serrée) dans la mémoire de la ressource. `false` si l'allocation est trop
/// petite pour la surface tuilée (l'appelant retombe alors sur le comportement linéaire).
fn tile_into_resource(res: &D3D12ResourceObject, linear: &[u8]) -> bool {
    let Some((mode, w, h, fmt)) = tile_params(res) else {
        return false;
    };
    let Some(tiled) = crate::d3d12_views::tile_subresource(mode, w, h, fmt, linear) else {
        return false;
    };
    let mut buf = res.backing.lock();
    let end = res.heap_offset + tiled.len();
    if end > buf.len() {
        sb_warn!(
            CHANNEL,
            "ressource tuilée trop petite ({} octets pour {} tuilés) : copie linéaire",
            buf.len().saturating_sub(res.heap_offset),
            tiled.len()
        );
        return false;
    }
    buf[res.heap_offset..end].copy_from_slice(&tiled);
    true
}

/// Détuile la sous-ressource 0 d'une ressource marquée tuilée en image linéaire serrée.
fn detile_from_resource(res: &D3D12ResourceObject) -> Option<Vec<u8>> {
    let (mode, w, h, fmt) = tile_params(res)?;
    let (surf, _) = crate::d3d12_views::tiled_surface_for(mode, w, h, fmt)?;
    let buf = res.backing.lock();
    let tiled = buf.get(res.heap_offset..res.heap_offset + surf.tiled_size())?;
    crate::d3d12_views::detile_subresource(mode, w, h, fmt, tiled)
}

/// `CopyBufferRegion` entre un tampon linéaire et une texture marquée tuilée : la région
/// `[offset, offset + len)` s'exprime dans l'image linéaire (détuilée) de la sous-ressource 0.
/// Renvoie `true` si la copie a été traitée ici ; `false` laisse la copie brute historique.
fn copy_buffer_region_tiled(
    dst: &D3D12ResourceObject,
    d_off: usize,
    src: &D3D12ResourceObject,
    s_off: usize,
    num_bytes: usize,
) -> bool {
    let (src_tiled, dst_tiled) = (tile_params(src).is_some(), tile_params(dst).is_some());
    if src_tiled == dst_tiled {
        return false; // aucune ou les deux tuilées : copie brute
    }
    if src_tiled && dst.desc.dimension == 1 {
        let Some(linear) = detile_from_resource(src) else {
            return false;
        };
        let n = num_bytes
            .min(linear.len().saturating_sub(s_off))
            .min(dst.byte_len.saturating_sub(d_off));
        let mut out = dst.backing.lock();
        let start = dst.heap_offset + d_off;
        if n == 0 || start + n > out.len() {
            return n == 0;
        }
        out[start..start + n].copy_from_slice(&linear[s_off..s_off + n]);
        return true;
    }
    if dst_tiled && src.desc.dimension == 1 {
        let Some(mut linear) = detile_from_resource(dst) else {
            return false;
        };
        let n = num_bytes
            .min(linear.len().saturating_sub(d_off))
            .min(src.byte_len.saturating_sub(s_off));
        if n == 0 {
            return true;
        }
        {
            let inp = src.backing.lock();
            let start = src.heap_offset + s_off;
            if start + n > inp.len() {
                return false;
            }
            linear[d_off..d_off + n].copy_from_slice(&inp[start..start + n]);
        }
        return tile_into_resource(dst, &linear);
    }
    false
}

extern "win64" fn cmdlist_copy_texture_region(
    this: *mut D3D12CmdListObject,
    dst: *const D3D12TextureCopyLocation,
    dst_x: u32,
    dst_y: u32,
    dst_z: u32,
    src: *const D3D12TextureCopyLocation,
    src_box: *const c_void,
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
    // Copie de la sous-ressource entière uniquement (pas d'offset ni de boîte) : sinon le
    // comportement linéaire historique s'applique.
    let whole = dst_x == 0 && dst_y == 0 && dst_z == 0 && src_box.is_null();

    // Texture tuilée -> tampon : détuile la sous-ressource 0 vers l'empreinte placée du tampon.
    if whole
        && d_loc.copy_type == 1
        && s_loc.copy_type == 0
        && s_loc.placed_footprint.offset as u32 == 0
        && dst_res.desc.dimension == 1
        && let Some((_, w, h, fmt)) = tile_params(src_res)
        && let Some(linear) = detile_from_resource(src_res)
    {
        let (row_bytes, rows) = texture_row_layout(w, h, fmt);
        let fp = d_loc.placed_footprint;
        let pitch = (fp.footprint.row_pitch as usize).max(row_bytes);
        let base = dst_res.heap_offset + fp.offset as usize;
        let mut out = dst_res.backing.lock();
        for y in 0..rows {
            let o = base + y * pitch;
            if o + row_bytes <= out.len() {
                out[o..o + row_bytes].copy_from_slice(&linear[y * row_bytes..(y + 1) * row_bytes]);
            }
        }
        return;
    }

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
    // Destination tuilée (sous-ressource 0 entière) : la mémoire reçoit l'image tuilée ; le GPU
    // (`UploadTexture`) continue de recevoir l'image linéaire.
    let tiled_done = whole
        && d_loc.copy_type == 0
        && s_loc.copy_type == 1
        && d_loc.placed_footprint.offset as u32 == 0
        && u64::from(w) == dst_res.desc.width
        && h == dst_res.desc.height.max(1)
        && tile_into_resource(dst_res, &data);
    if !tiled_done {
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
    if this.is_null()
        || d3d_known_object_name(dst as usize) != Some("ID3D12Resource")
        || d3d_known_object_name(src as usize) != Some("ID3D12Resource")
    {
        return;
    }
    // SAFETY: `this` non-nul, `dst` et `src` validés comme `ID3D12Resource` du bridge.
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
    if this.is_null()
        || num_viewports == 0
        || viewports.is_null()
        || sb_loader::trampoline::peek_memory(viewports as usize, size_of::<D3D12Viewport>()).len()
            != size_of::<D3D12Viewport>()
    {
        return;
    }
    // SAFETY: `this` non-nul et `viewports` lisible via `peek_memory`.
    let cl = unsafe { &*this };
    let vp = unsafe { ptr::read_unaligned(viewports) };
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
    if this.is_null()
        || num_rects == 0
        || rects.is_null()
        || sb_loader::trampoline::peek_memory(rects as usize, size_of::<D3D12Rect>()).len()
            != size_of::<D3D12Rect>()
    {
        return;
    }
    // SAFETY: `this` non-nul et `rects` lisible via `peek_memory`.
    let cl = unsafe { &*this };
    let r = unsafe { ptr::read_unaligned(rects) };
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
    if this.is_null() || d3d_known_object_name(pso as usize) != Some("ID3D12PipelineState") {
        return;
    }
    // SAFETY: `this` non-nul et `pso` validé comme `ID3D12PipelineState` du bridge.
    let cl = unsafe { &*this };
    let p = unsafe { &*pso };
    cl.native
        .lock()
        .note_pso(p.pipeline_id, p.is_compute, &p.native);
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
    if this.is_null() || d3d_known_object_name(root_sig as usize) != Some("ID3D12RootSignature") {
        return;
    }
    // SAFETY: `this` non-nul et `root_sig` validé comme `ID3D12RootSignature` du bridge.
    let cl = unsafe { &*this };
    let rs = unsafe { &*root_sig };
    *cl.compute_root_regs.lock() = rs.param_registers.clone();
    cl.native.lock().cs.set_signature(rs.layout.clone());
}

extern "win64" fn cmdlist_set_graphics_root_sig(
    this: *mut D3D12CmdListObject,
    root_sig: *mut D3D12RootSigObject,
) {
    if this.is_null() || d3d_known_object_name(root_sig as usize) != Some("ID3D12RootSignature") {
        return;
    }
    // SAFETY: `this` non-nul et `root_sig` validé comme `ID3D12RootSignature` du bridge.
    let cl = unsafe { &*this };
    let rs = unsafe { &*root_sig };
    *cl.graphics_root_regs.lock() = rs.param_registers.clone();
    cl.native.lock().gfx.set_signature(rs.layout.clone());
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
    cl.native
        .lock()
        .cs
        .set_table(root_param_idx, base_gpu_handle);
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
    cl.native
        .lock()
        .gfx
        .set_table(root_param_idx, base_gpu_handle);
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
    dest_offset: u32,
) {
    if this.is_null() || src_data.is_null() || num_32bit_values == 0 || num_32bit_values > 256 {
        return;
    }
    let byte_len = (num_32bit_values as usize) * 4;
    let bytes = sb_loader::trampoline::peek_memory(src_data as usize, byte_len);
    if bytes.len() != byte_len {
        return;
    }
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    let words: Vec<u32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();
    cl.native
        .lock()
        .cs
        .set_constants(root_param_idx, dest_offset, &words);
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
    dest_offset: u32,
) {
    if this.is_null() || src_data.is_null() || num_32bit_values == 0 || num_32bit_values > 256 {
        return;
    }
    let byte_len = (num_32bit_values as usize) * 4;
    let bytes = sb_loader::trampoline::peek_memory(src_data as usize, byte_len);
    if bytes.len() != byte_len {
        return;
    }
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    let words: Vec<u32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();
    cl.native
        .lock()
        .gfx
        .set_constants(root_param_idx, dest_offset, &words);
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
    cl.native.lock().cs.set_va(root_param_idx, buffer_location);
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
    cl.native.lock().gfx.set_va(root_param_idx, buffer_location);
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
    cl.native.lock().cs.set_va(root_param_idx, buffer_location);
    let slot = cl.resolve_compute_slot(root_param_idx);
    cl.ops.lock().push(RecordedOp::BindComputeStorage {
        slot,
        addr: buffer_location,
    });
}

/// `SetComputeRoot32BitConstant(RootParameterIndex, SrcData, DestOffsetIn32BitValues)` (slot 33,
/// vtable PC publique) : alimente uniquement les user-SGPR du chemin natif RDNA 2.
extern "win64" fn cmdlist_set_compute_root_32bit_constant(
    this: *mut D3D12CmdListObject,
    root_param_idx: u32,
    src_data: u32,
    dest_offset: u32,
) {
    if this.is_null() {
        return;
    }
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    cl.native
        .lock()
        .cs
        .set_constants(root_param_idx, dest_offset, &[src_data]);
}

/// `SetGraphicsRoot32BitConstant` (slot 34, vtable PC publique).
extern "win64" fn cmdlist_set_graphics_root_32bit_constant(
    this: *mut D3D12CmdListObject,
    root_param_idx: u32,
    src_data: u32,
    dest_offset: u32,
) {
    if this.is_null() {
        return;
    }
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    cl.native
        .lock()
        .gfx
        .set_constants(root_param_idx, dest_offset, &[src_data]);
}

/// `SetGraphicsRootShaderResourceView` / `SetGraphicsRootUnorderedAccessView` (slots 40 et 42,
/// vtable PC publique) : alimentent uniquement les user-SGPR du chemin natif RDNA 2.
extern "win64" fn cmdlist_set_graphics_root_srv_uav(
    this: *mut D3D12CmdListObject,
    root_param_idx: u32,
    buffer_location: u64,
) {
    if this.is_null() || buffer_location == 0 {
        return;
    }
    // SAFETY: `this` valide.
    let cl = unsafe { &*this };
    cl.native.lock().gfx.set_va(root_param_idx, buffer_location);
}

extern "win64" fn cmdlist_ia_set_index_buffer(
    this: *mut D3D12CmdListObject,
    view: *const D3D12IndexBufferView,
) {
    if this.is_null()
        || view.is_null()
        || sb_loader::trampoline::peek_memory(view as usize, size_of::<D3D12IndexBufferView>())
            .len()
            != size_of::<D3D12IndexBufferView>()
    {
        return;
    }
    // SAFETY: `view` vérifié lisible via `peek_memory`.
    let v = unsafe { ptr::read_unaligned(view) };
    if v.buffer_location == 0
        || v.size_in_bytes == 0
        || sb_loader::trampoline::peek_memory(v.buffer_location as usize, 1).is_empty()
    {
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
    if this.is_null()
        || num_views == 0
        || views.is_null()
        || sb_loader::trampoline::peek_memory(views as usize, size_of::<D3D12VertexBufferView>())
            .len()
            != size_of::<D3D12VertexBufferView>()
    {
        return;
    }
    // SAFETY: `views` vérifié lisible pour au moins un `D3D12VertexBufferView`.
    let view = unsafe { ptr::read_unaligned(views) };
    if view.buffer_location == 0
        || view.size_in_bytes == 0
        || sb_loader::trampoline::peek_memory(view.buffer_location as usize, 1).is_empty()
    {
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
    if this.is_null()
        || color_rgba.is_null()
        || sb_loader::trampoline::peek_memory(color_rgba as usize, 16).len() != 16
    {
        return;
    }
    // SAFETY: `this` et `color_rgba` vérifiés lisibles.
    let cl = unsafe { &*this };
    let rgba = unsafe { ptr::read_unaligned(color_rgba) };
    cl.ops
        .lock()
        .push(RecordedOp::Render(RenderCommand::Clear { rgba }));
}

extern "win64" fn cmdlist_copy_buffer_region(
    this: *mut D3D12CmdListObject,
    dst: *mut D3D12ResourceObject,
    dst_offset: u64,
    src: *mut D3D12ResourceObject,
    src_offset: u64,
    num_bytes: u64,
) {
    if this.is_null() || dst.is_null() || src.is_null() || num_bytes == 0 {
        return;
    }
    if !is_d3d12_resource(dst as usize) || !is_d3d12_resource(src as usize) {
        return;
    }
    // SAFETY: `this` vérifié non-nul.
    let cl = unsafe { &*this };
    cl.ops.lock().push(RecordedOp::CopyBufferRegion {
        dst: dst as usize,
        dst_offset,
        src: src as usize,
        src_offset,
        num_bytes,
    });
}

extern "win64" fn cmdlist_write_value32(
    this: *mut D3D12CmdListObject,
    address: u64,
    value: u32,
    _flush: u32,
    _flags: u32,
) {
    if this.is_null() || address == 0 {
        return;
    }
    // SAFETY: `this` vérifié non-nul.
    let cl = unsafe { &*this };
    cl.ops
        .lock()
        .push(RecordedOp::WriteValue32 { address, value });
}

extern "win64" fn cmdlist_write_value64(
    this: *mut D3D12CmdListObject,
    address: u64,
    value: u64,
    _flush: u32,
    flags: u32,
) {
    if this.is_null() || address == 0 {
        return;
    }
    // SAFETY: `this` vérifié non-nul.
    let cl = unsafe { &*this };
    cl.ops.lock().push(RecordedOp::WriteValue64 {
        address,
        value,
        flags,
    });
}

extern "win64" fn cmdlist_wait_value32(
    _this: *mut D3D12CmdListObject,
    address: u64,
    value: u32,
    mask: u32,
    _flags: u32,
) {
    sb_trace!(
        CHANNEL,
        "ID3D12GraphicsCommandList::WaitValue32(addr={address:#x}, val={value:#x}, mask={mask:#x})"
    );
}

extern "win64" fn cmdlist_wait_value64(
    _this: *mut D3D12CmdListObject,
    address: u64,
    value: u64,
    mask: u64,
    _flags: u32,
) {
    sb_trace!(
        CHANNEL,
        "ID3D12GraphicsCommandList::WaitValue64(addr={address:#x}, val={value:#x}, mask={mask:#x})"
    );
}

/// `ID3D12GraphicsCommandList::AllocateUploadMemory` (slot 148 console) : alloue un tampon
/// aligné dans la mémoire unifiée CPU/GPU (adresse directe utilisable en écriture CPU et en
/// `GPUVirtualAddress` côté shaders).
extern "win64" fn cmdlist_allocate_upload_memory(
    this: *mut D3D12CmdListObject,
    size: u64,
    alignment: u64,
) -> *mut c_void {
    ffi_guard!(CHANNEL, "AllocateUploadMemory", ptr::null_mut(), {
        if this.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: `this` vérifié non-nul.
        let cl = unsafe { &*this };
        let align = (alignment as usize).max(16).next_power_of_two().min(65536);
        let sz = (size as usize).max(16);
        let mut buf = vec![0u8; sz + align];
        let raw = buf.as_mut_ptr() as usize;
        let aligned = (raw + align - 1) & !(align - 1);
        ALLOCATED_BUFFER_SIZES.lock().insert(aligned, sz);
        cl.upload_arenas.lock().push(buf);
        aligned as *mut c_void
    })
}

/// `ID3D12CommandList::GetType` (slot 8).
extern "win64" fn cmdlist_get_type(this: *mut D3D12CmdListObject) -> u32 {
    if this.is_null() {
        return 0;
    }
    // SAFETY: `this` est un `D3D12CmdListObject` valide.
    unsafe { (*this).list_type }
}

/// Retourne `S_OK` (0 dans `RAX`) : sans effet pour les méthodes `void` (registre `RAX`
/// volatile en ABI Win64) et sûr si un slot console renvoie un `HRESULT`.
extern "win64" fn cmdlist_noop_void() -> HResult {
    S_OK
}

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
    fill_d3d12_child_slots(&mut slots);
    slots[8] = vslot(cmdlist_get_type as *const c_void);
    slots[9] = vslot(cmdlist_close as *const c_void);
    slots[10] = vslot(cmdlist_reset as *const c_void);
    slots[12] = vslot(cmdlist_draw_instanced as *const c_void);
    slots[13] = vslot(cmdlist_draw_indexed_instanced as *const c_void);
    slots[14] = vslot(cmdlist_dispatch as *const c_void);
    slots[15] = vslot(cmdlist_copy_buffer_region as *const c_void);
    slots[16] = vslot(cmdlist_copy_texture_region as *const c_void);
    slots[17] = vslot(cmdlist_copy_resource as *const c_void);
    slots[20] = vslot(cmdlist_noop_void as *const c_void); // IASetPrimitiveTopology
    slots[21] = vslot(cmdlist_rs_set_viewports as *const c_void);
    slots[22] = vslot(cmdlist_rs_set_scissor_rects as *const c_void);
    slots[25] = vslot(cmdlist_set_pipeline_state as *const c_void);
    slots[26] = vslot(cmdlist_noop_void as *const c_void); // ResourceBarrier
    slots[29] = vslot(cmdlist_set_compute_root_sig as *const c_void);
    slots[30] = vslot(cmdlist_set_graphics_root_sig as *const c_void);
    slots[31] = vslot(cmdlist_set_compute_root_desc_table as *const c_void);
    slots[32] = vslot(cmdlist_set_graphics_root_desc_table as *const c_void);
    slots[33] = vslot(cmdlist_set_compute_root_32bit_constant as *const c_void);
    slots[34] = vslot(cmdlist_set_graphics_root_32bit_constant as *const c_void);
    slots[35] = vslot(cmdlist_set_compute_root_32bit_constants as *const c_void);
    slots[36] = vslot(cmdlist_set_graphics_root_32bit_constants as *const c_void);
    slots[37] = vslot(cmdlist_set_compute_root_cbv as *const c_void);
    slots[38] = vslot(cmdlist_set_graphics_root_cbv as *const c_void);
    slots[39] = vslot(cmdlist_set_compute_root_storage as *const c_void);
    slots[40] = vslot(cmdlist_set_graphics_root_srv_uav as *const c_void);
    slots[41] = vslot(cmdlist_set_compute_root_storage as *const c_void);
    slots[42] = vslot(cmdlist_set_graphics_root_srv_uav as *const c_void);
    slots[43] = vslot(cmdlist_ia_set_index_buffer as *const c_void);
    slots[44] = vslot(cmdlist_ia_set_vertex_buffers as *const c_void);
    slots[47] = vslot(cmdlist_clear_dsv as *const c_void);
    slots[48] = vslot(cmdlist_clear_rtv as *const c_void);
    // Extensions console Xbox Series X (ABI publique d3d12_xs).
    slots[70] = vslot(cmdlist_noop_void as *const c_void); // OMSetDepthBounds
    slots[71] = vslot(cmdlist_write_value32 as *const c_void);
    slots[72] = vslot(cmdlist_write_value64 as *const c_void);
    slots[73] = vslot(cmdlist_wait_value32 as *const c_void);
    slots[74] = vslot(cmdlist_wait_value64 as *const c_void);
    slots[75] = vslot(cmdlist_noop_void as *const c_void); // SetPixelShaderDepthOrderX
    slots[80] = vslot(cmdlist_noop_void as *const c_void); // ExecuteIndirect
    slots[83] = vslot(cmdlist_copy_texture_region as *const c_void); // CopyTextureRegionX (ancien layout)
    slots[85] = vslot(cmdlist_copy_texture_region as *const c_void); // CopyTextureRegionX (nouveau layout)
    slots[108] = vslot(cmdlist_noop_void as *const c_void); // ResolveSubresourceRegion
    slots[131] = vslot(cmdlist_noop_void as *const c_void); // Réglage console de liste de commandes
    slots[132] = vslot(cmdlist_noop_void as *const c_void); // Réglage console de liste de commandes
    slots[139] = vslot(cmdlist_reset as *const c_void); // Reset (console)
    slots[146] = vslot(cmdlist_noop_void as *const c_void); // Barrier (enhanced)
    slots[148] = vslot(cmdlist_allocate_upload_memory as *const c_void); // AllocateUploadMemory
    D3D12CmdListVtbl { slots }
}

static D3D12_CMDLIST_VTBL: D3D12CmdListVtbl = build_cmdlist_vtbl();

/// Vtable `ID3D12GraphicsCommandList` **console Xbox Series X** (`d3d12_xs.h`) :
/// décalage `+4` à partir de `SetDescriptorHeaps` (slot 32 au lieu de 28 sur PC), jusqu'à
/// `ClearRenderTargetView` (slot 52 au lieu de 48), suivi des extensions console (`70..=148`).
const fn build_console_cmdlist_vtbl() -> D3D12CmdListVtbl {
    let mut slots = UNIMPL_SLOTS;
    let mut pad = 0;
    while pad < 64 {
        slots[pad] = vslot(cmdlist_noop_void as *const c_void);
        pad += 1;
    }
    slots[0] = vslot(cmdlist_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    fill_d3d12_child_slots(&mut slots);
    slots[8] = vslot(cmdlist_get_type as *const c_void);
    slots[9] = vslot(cmdlist_close as *const c_void);
    slots[10] = vslot(cmdlist_reset as *const c_void);
    slots[12] = vslot(cmdlist_draw_instanced as *const c_void);
    slots[13] = vslot(cmdlist_draw_indexed_instanced as *const c_void);
    slots[14] = vslot(cmdlist_dispatch as *const c_void);
    slots[15] = vslot(cmdlist_copy_buffer_region as *const c_void);
    slots[16] = vslot(cmdlist_copy_texture_region as *const c_void);
    slots[17] = vslot(cmdlist_copy_resource as *const c_void);
    slots[20] = vslot(cmdlist_noop_void as *const c_void); // IASetPrimitiveTopology
    slots[21] = vslot(cmdlist_rs_set_viewports as *const c_void);
    slots[22] = vslot(cmdlist_rs_set_scissor_rects as *const c_void);
    slots[25] = vslot(cmdlist_set_pipeline_state as *const c_void);
    slots[26] = vslot(cmdlist_noop_void as *const c_void); // ResourceBarrier
    // Décalage +4 console Xbox (`d3d12_xs_console_abi.md` §5.3).
    slots[32] = vslot(cmdlist_noop_void as *const c_void); // SetDescriptorHeaps
    slots[33] = vslot(cmdlist_set_compute_root_sig as *const c_void);
    slots[34] = vslot(cmdlist_set_graphics_root_sig as *const c_void);
    slots[35] = vslot(cmdlist_set_compute_root_desc_table as *const c_void);
    slots[36] = vslot(cmdlist_set_graphics_root_desc_table as *const c_void);
    slots[37] = vslot(cmdlist_noop_void as *const c_void); // SetComputeRoot32BitConstant
    slots[38] = vslot(cmdlist_noop_void as *const c_void); // SetGraphicsRoot32BitConstant
    slots[39] = vslot(cmdlist_set_compute_root_32bit_constants as *const c_void);
    slots[40] = vslot(cmdlist_set_graphics_root_32bit_constants as *const c_void);
    slots[41] = vslot(cmdlist_set_compute_root_cbv as *const c_void);
    slots[42] = vslot(cmdlist_set_graphics_root_cbv as *const c_void);
    slots[43] = vslot(cmdlist_set_compute_root_storage as *const c_void); // SetComputeRootSRV
    slots[44] = vslot(cmdlist_set_graphics_root_cbv as *const c_void); // SetGraphicsRootSRV
    slots[45] = vslot(cmdlist_set_compute_root_storage as *const c_void); // SetComputeRootUAV
    slots[46] = vslot(cmdlist_set_graphics_root_cbv as *const c_void); // SetGraphicsRootUAV
    slots[47] = vslot(cmdlist_ia_set_index_buffer as *const c_void);
    slots[48] = vslot(cmdlist_ia_set_vertex_buffers as *const c_void);
    slots[51] = vslot(cmdlist_clear_dsv as *const c_void);
    slots[52] = vslot(cmdlist_clear_rtv as *const c_void);
    // Extensions console Xbox Series X.
    slots[70] = vslot(cmdlist_noop_void as *const c_void); // OMSetDepthBounds
    slots[71] = vslot(cmdlist_write_value32 as *const c_void);
    slots[72] = vslot(cmdlist_write_value64 as *const c_void);
    slots[73] = vslot(cmdlist_wait_value32 as *const c_void);
    slots[74] = vslot(cmdlist_wait_value64 as *const c_void);
    slots[75] = vslot(cmdlist_noop_void as *const c_void); // SetPixelShaderDepthOrderX
    slots[80] = vslot(cmdlist_noop_void as *const c_void); // ExecuteIndirect
    slots[83] = vslot(cmdlist_copy_texture_region as *const c_void); // CopyTextureRegionX (ancien)
    slots[85] = vslot(cmdlist_copy_texture_region as *const c_void); // CopyTextureRegionX (nouveau)
    slots[108] = vslot(cmdlist_noop_void as *const c_void); // ResolveSubresourceRegion
    slots[126] = vslot(cmdlist_noop_void as *const c_void); // Réglage console
    slots[131] = vslot(cmdlist_noop_void as *const c_void); // Réglage console
    slots[132] = vslot(cmdlist_noop_void as *const c_void); // Réglage console
    slots[139] = vslot(cmdlist_reset as *const c_void); // Reset (console)
    slots[142] = vslot(cmdlist_noop_void as *const c_void); // Réglage / marqueur console
    slots[146] = vslot(cmdlist_noop_void as *const c_void); // Barrier (enhanced)
    slots[148] = vslot(cmdlist_allocate_upload_memory as *const c_void); // AllocateUploadMemory
    D3D12CmdListVtbl { slots }
}

static D3D12_CONSOLE_CMDLIST_VTBL: D3D12CmdListVtbl = build_console_cmdlist_vtbl();

/// Vtable `ID3D12DmaCommandList` (type 6, IID `30bc77b9-8379-4b25-95de-b8588c42275d`) :
/// slots 9 = `Close`, 10 = `Reset`, 11 = `CopyBufferRegion`, 12 = `CopyTextureRegion`,
/// 13 = `CopyResource`. Partage la structure `D3D12CmdListObject`.
const fn build_dma_cmdlist_vtbl() -> D3D12CmdListVtbl {
    let mut slots = UNIMPL_SLOTS;
    let mut pad = 0;
    while pad < 24 {
        slots[pad] = vslot(cmdlist_noop_void as *const c_void);
        pad += 1;
    }
    slots[0] = vslot(cmdlist_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    fill_d3d12_child_slots(&mut slots);
    slots[8] = vslot(cmdlist_get_type as *const c_void);
    slots[9] = vslot(cmdlist_close as *const c_void);
    slots[10] = vslot(cmdlist_reset as *const c_void);
    slots[11] = vslot(cmdlist_copy_buffer_region as *const c_void);
    slots[12] = vslot(cmdlist_copy_texture_region as *const c_void);
    slots[13] = vslot(cmdlist_copy_resource as *const c_void);
    D3D12CmdListVtbl { slots }
}

static D3D12_DMA_CMDLIST_VTBL: D3D12CmdListVtbl = build_dma_cmdlist_vtbl();

// --- ID3D12CommandQueue ---

#[repr(C)]
struct D3D12QueueVtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct D3D12QueueObject {
    header: ComHeader,
    /// Doit rester juste après `header` (même offset que `D3D12DeviceObject::shared`).
    shared: Arc<SharedD3D12State>,
    desc: D3D12CommandQueueDesc,
    /// Détecte automatiquement le layout console récent (+2 slots à partir de `Signal` en 16).
    detected_newer_layout: AtomicBool,
}

const _: () = {
    assert!(offset_of!(D3D12QueueObject, shared) == offset_of!(D3D12DeviceObject, shared));
};

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
                    RecordedOp::CopyBufferRegion {
                        dst,
                        dst_offset,
                        src,
                        src_offset,
                        num_bytes,
                    } => {
                        flush_render(&mut renderer, &mut render_cmds);
                        let src_obj = unsafe { &*(*src as *const D3D12ResourceObject) };
                        let dst_obj = unsafe { &*(*dst as *const D3D12ResourceObject) };
                        let s_off = (*src_offset as usize).min(src_obj.byte_len);
                        let d_off = (*dst_offset as usize).min(dst_obj.byte_len);
                        let avail_src = src_obj.byte_len.saturating_sub(s_off);
                        let avail_dst = dst_obj.byte_len.saturating_sub(d_off);
                        let len = (*num_bytes as usize).min(avail_src).min(avail_dst);
                        if copy_buffer_region_tiled(
                            dst_obj,
                            *dst_offset as usize,
                            src_obj,
                            *src_offset as usize,
                            *num_bytes as usize,
                        ) {
                            // Copie traitée avec (dé)tuilage.
                        } else if len > 0 {
                            // SAFETY: bornes vérifiées sur `src_obj.byte_len` et `dst_obj.byte_len`.
                            let tmp = unsafe {
                                std::slice::from_raw_parts(
                                    (src_obj.fixed_addr + s_off) as *const u8,
                                    len,
                                )
                                .to_vec()
                            };
                            unsafe {
                                ptr::copy_nonoverlapping(
                                    tmp.as_ptr(),
                                    (dst_obj.fixed_addr + d_off) as *mut u8,
                                    len,
                                );
                            }
                        }
                    }
                    RecordedOp::WriteValue32 { address, value } => {
                        flush_render(&mut renderer, &mut render_cmds);
                        let addr = *address as usize;
                        if addr >= 0x1_0000
                            && sb_loader::trampoline::peek_memory(addr, 4).len() == 4
                        {
                            std::sync::atomic::fence(Ordering::Release);
                            // SAFETY: `addr` vérifié lisible/mappé sur 4 octets.
                            unsafe {
                                ptr::write_unaligned(addr as *mut u32, *value);
                            }
                            std::sync::atomic::fence(Ordering::SeqCst);
                        }
                    }
                    RecordedOp::WriteValue64 {
                        address,
                        value,
                        flags,
                    } => {
                        flush_render(&mut renderer, &mut render_cmds);
                        let addr = *address as usize;
                        if addr >= 0x1_0000
                            && sb_loader::trampoline::peek_memory(addr, 8).len() == 8
                        {
                            // Drapeaux 1 ou 2 = horodatage GPU / QPC monotone (1 GHz).
                            let written = if *flags == 1 || *flags == 2 {
                                q.shared.elapsed_ns()
                            } else {
                                *value
                            };
                            std::sync::atomic::fence(Ordering::Release);
                            // SAFETY: `addr` vérifié lisible/mappé sur 8 octets.
                            unsafe {
                                ptr::write_unaligned(addr as *mut u64, written);
                            }
                            std::sync::atomic::fence(Ordering::SeqCst);
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
                    RecordedOp::NativeDraw {
                        pso,
                        pipeline_id,
                        user_data,
                        counts,
                    } => {
                        let (vs, ps) = {
                            let mut ctx = native::ResolveCtx {
                                renderer: &mut renderer,
                                pending: &mut render_cmds,
                                descs: &descs,
                                images: &q.shared.native_images,
                            };
                            let vs = native::resolve_stage(&mut ctx, &pso.stages[0], user_data);
                            let ps = native::resolve_stage(&mut ctx, &pso.stages[1], user_data);
                            (vs, ps)
                        };
                        render_cmds.push(native::draw_cmd(*pipeline_id, *counts, vs, ps));
                    }
                    RecordedOp::NativeDispatch {
                        pso,
                        pipeline_id,
                        user_data,
                        groups,
                    } => {
                        flush_render(&mut renderer, &mut render_cmds);
                        let cs = {
                            let mut ctx = native::ResolveCtx {
                                renderer: &mut renderer,
                                pending: &mut render_cmds,
                                descs: &descs,
                                images: &q.shared.native_images,
                            };
                            native::resolve_stage(&mut ctx, &pso.stages[0], user_data)
                        };
                        native::run_dispatch(&mut renderer, *pipeline_id, *groups, cs);
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
        let f = unsafe { &*fence };
        f.signal_value(value);
        sb_trace!(CHANNEL, "ID3D12CommandQueue::Signal(value={value})");
    }
    S_OK
}

extern "win64" fn queue_wait(
    _this: *mut D3D12QueueObject,
    _fence: *mut D3D12FenceObject,
    _value: u64,
) -> HResult {
    S_OK
}

/// Slot 16 de `ID3D12CommandQueue` (double layout console) :
/// - si `arg1` est un `ID3D12Fence` du bridge → layout récent `Signal(fence, value)` ;
/// - sinon → layout PC / ancien `GetTimestampFrequency(out_freq)`.
extern "win64" fn queue_slot16_signal_or_freq(
    this: *mut D3D12QueueObject,
    arg1: usize,
    arg2: u64,
) -> HResult {
    ffi_guard!(CHANNEL, "ID3D12CommandQueue::slot16", E_UNEXPECTED, {
        if d3d_known_object_name(arg1) == Some("ID3D12Fence") {
            if !this.is_null() {
                // SAFETY: `this` vérifié non-nul.
                unsafe { (*this).detected_newer_layout.store(true, Ordering::Release) };
            }
            return queue_signal(this, arg1 as *mut D3D12FenceObject, arg2);
        }
        if arg1 == 0 {
            return E_POINTER;
        }
        // SAFETY: `arg1` est un `*mut u64` fourni par l'appelant pour `GetTimestampFrequency`.
        unsafe {
            ptr::write_unaligned(arg1 as *mut u64, 1_000_000_000u64);
        }
        S_OK
    })
}

/// Slot 17 de `ID3D12CommandQueue` (double layout console) :
/// - si `arg1` est un `ID3D12Fence` du bridge → layout récent `Wait(fence, value)` ;
/// - sinon → layout PC / ancien `GetClockCalibration(out_gpu, out_cpu)`.
extern "win64" fn queue_slot17_wait_or_clock(
    this: *mut D3D12QueueObject,
    arg1: usize,
    arg2: u64,
) -> HResult {
    ffi_guard!(CHANNEL, "ID3D12CommandQueue::slot17", E_UNEXPECTED, {
        if d3d_known_object_name(arg1) == Some("ID3D12Fence") {
            if !this.is_null() {
                // SAFETY: `this` vérifié non-nul.
                unsafe { (*this).detected_newer_layout.store(true, Ordering::Release) };
            }
            return queue_wait(this, arg1 as *mut D3D12FenceObject, arg2);
        }
        queue_get_clock_calibration(this, arg1 as *mut u64, arg2 as *mut u64)
    })
}

extern "win64" fn queue_get_clock_calibration(
    this: *mut D3D12QueueObject,
    out_gpu: *mut u64,
    out_cpu: *mut u64,
) -> HResult {
    let ts = if this.is_null() {
        1u64
    } else {
        // SAFETY: `this` vérifié non-nul.
        unsafe { (*this).shared.elapsed_ns() }
    };
    if !out_gpu.is_null() {
        // SAFETY: `out_gpu` non-nul.
        unsafe { ptr::write_unaligned(out_gpu, ts) };
    }
    if !out_cpu.is_null() {
        // SAFETY: `out_cpu` non-nul.
        unsafe { ptr::write_unaligned(out_cpu, ts) };
    }
    S_OK
}

/// Slot 18 de `ID3D12CommandQueue` :
/// - en layout console récent → `GetTimestampFrequency(out_freq) -> HResult` (`0`) ;
/// - en layout PC / ancien → `GetDesc(out_desc) -> *mut D3D12CommandQueueDesc`.
extern "win64" fn queue_slot18_freq_or_desc(this: *mut D3D12QueueObject, arg1: usize) -> usize {
    if this.is_null() {
        return 0;
    }
    // SAFETY: `this` vérifié non-nul.
    let q = unsafe { &*this };
    if q.detected_newer_layout.load(Ordering::Acquire) {
        if arg1 == 0 {
            return E_POINTER as usize;
        }
        // SAFETY: `arg1` est `*mut u64` (`GetTimestampFrequency`).
        unsafe {
            ptr::write_unaligned(arg1 as *mut u64, 1_000_000_000u64);
        }
        return S_OK as usize;
    }
    if arg1 != 0 {
        // SAFETY: `arg1` est `*mut D3D12CommandQueueDesc` (`GetDesc`).
        unsafe {
            ptr::write_unaligned(arg1 as *mut D3D12CommandQueueDesc, q.desc);
        }
    }
    arg1
}

/// `ID3D12CommandQueue::PresentX` (slot 30 ancien layout / slot 32 nouveau layout).
extern "win64" fn queue_present_x(
    this: *mut D3D12QueueObject,
    plane_count: u32,
    planes: *const D3D12XboxPresentPlaneParameters,
    _params: *const D3D12XboxPresentParameters,
) -> HResult {
    ffi_guard!(CHANNEL, "ID3D12CommandQueue::PresentX", E_UNEXPECTED, {
        if this.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: `this` vérifié non-nul.
        let q = unsafe { &*this };
        let token = if plane_count > 0 && !planes.is_null() {
            // SAFETY: `planes` pointe vers au moins un `D3D12XboxPresentPlaneParameters`.
            unsafe { (*planes).token }
        } else {
            0
        };

        let mut renderer = q.shared.renderer.lock();
        if let Ok(path_str) = std::env::var("SB_DUMP_FRAME")
            && !path_str.is_empty()
        {
            let path = PathBuf::from(&path_str);
            if let Err(err) = renderer.save_ppm(&path) {
                sb_warn!(CHANNEL, "PresentX SB_DUMP_FRAME échoué: {err}");
            }
        }
        if let Err(err) = renderer.present_to_surface() {
            sb_warn!(CHANNEL, "PresentX present_to_surface: {err}");
        }
        let frame_no = q.shared.present_count.fetch_add(1, Ordering::AcqRel) + 1;
        sb_debug!(
            CHANNEL,
            "ID3D12CommandQueue::PresentX(frame #{frame_no}, planes={plane_count}, token={token:#x}) -> S_OK"
        );
        S_OK
    })
}

extern "win64" fn queue_present_x_newer(
    this: *mut D3D12QueueObject,
    plane_count: u32,
    planes: *const D3D12XboxPresentPlaneParameters,
    params: *const D3D12XboxPresentParameters,
) -> HResult {
    if !this.is_null() {
        // SAFETY: `this` vérifié non-nul.
        unsafe { (*this).detected_newer_layout.store(true, Ordering::Release) };
    }
    queue_present_x(this, plane_count, planes, params)
}

/// `ID3D12CommandQueue::GetDesc` (slot 20 en layout console récent).
extern "win64" fn queue_get_desc(
    this: *mut D3D12QueueObject,
    out_desc: *mut D3D12CommandQueueDesc,
) -> *mut D3D12CommandQueueDesc {
    if !this.is_null() && !out_desc.is_null() {
        // SAFETY: `this` et `out_desc` vérifiés non-nuls.
        unsafe {
            ptr::write_unaligned(out_desc, (*this).desc);
        }
    }
    out_desc
}

const fn build_queue_vtbl() -> D3D12QueueVtbl {
    let mut slots = UNIMPL_SLOTS;
    let mut pad = 0;
    while pad < 21 {
        slots[pad] = vslot(noop_ok as *const c_void);
        pad += 1;
    }
    slots[0] = vslot(queue_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    fill_d3d12_child_slots(&mut slots);
    slots[10] = vslot(queue_execute_command_lists as *const c_void);
    slots[14] = vslot(queue_signal as *const c_void);
    slots[15] = vslot(queue_wait as *const c_void);
    slots[16] = vslot(queue_slot16_signal_or_freq as *const c_void);
    slots[17] = vslot(queue_slot17_wait_or_clock as *const c_void);
    slots[18] = vslot(queue_slot18_freq_or_desc as *const c_void);
    slots[19] = vslot(queue_get_clock_calibration as *const c_void);
    slots[20] = vslot(queue_get_desc as *const c_void);
    slots[23] = vslot(queue_signal as *const c_void); // SignalX (ancien layout)
    slots[25] = vslot(queue_signal as *const c_void); // SignalX (nouveau layout)
    slots[30] = vslot(queue_present_x as *const c_void); // PresentX (ancien layout)
    slots[31] = vslot(noop_ok as *const c_void); // CopyPageMappingsBatchX (nouveau layout)
    slots[32] = vslot(queue_present_x_newer as *const c_void); // PresentX (nouveau layout)
    slots[35] = vslot(queue_execute_command_lists as *const c_void); // ExecuteCommandListsX (console)
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
    if ppv.is_null() {
        return E_POINTER;
    }
    // `IDXGIDevice` / `IDXGIDevice1..3` : les titres demandent l'adaptateur DXGI via le périphérique D3D12.
    if !riid.is_null() {
        // SAFETY: `riid` vérifié non-nul.
        let wanted = unsafe { ptr::read_unaligned(riid) };
        if wanted == IID_IDXGIDEVICE
            || wanted == crate::com::IID_IDXGIDEVICE1
            || wanted == crate::com::IID_IDXGIDEVICE2
            || wanted == crate::com::IID_IDXGIDEVICE3
        {
            let dev = this as usize;
            let obj = Box::new(DxgiDeviceObject {
                header: ComHeader::new(traced_vtbl(
                    (&raw const DXGI_DEVICE_VTBL).cast(),
                    "IDXGIDevice",
                )),
                device: dev,
            });
            // SAFETY: `ppv` vérifié non-nul ; `obj` est un objet du bridge.
            unsafe { *ppv = Box::into_raw(obj).cast() };
            return S_OK;
        }
    }
    com_query_interface_list(
        "ID3D12Device",
        this,
        riid,
        ppv,
        &[
            IID_ID3D12DEVICE,
            IID_ID3D12DEVICE1,
            IID_ID3D12DEVICE2,
            IID_ID3D12DEVICE3,
            IID_ID3D12DEVICE4,
            IID_ID3D12DEVICE5,
            IID_ID3D12DEVICE6,
            IID_ID3D12DEVICE7,
            IID_ID3D12DEVICE8,
            IID_ID3D12DEVICE9,
            IID_ID3D12DEVICE10,
            IID_ID3D12OBJECT,
        ],
    )
}

extern "win64" fn device_get_node_count(_this: *mut D3D12DeviceObject) -> u32 {
    1
}

extern "win64" fn device_create_command_queue(
    this: *mut D3D12DeviceObject,
    desc: *const D3D12CommandQueueDesc,
    _riid: *const Guid,
    out_queue: *mut *mut c_void,
) -> HResult {
    if this.is_null() || out_queue.is_null() {
        return E_POINTER;
    }
    // SAFETY: `this` et `out_queue` vérifiés non-nuls.
    let dev = unsafe { &*this };
    let q_desc = if desc.is_null() {
        D3D12CommandQueueDesc::default()
    } else {
        // SAFETY: `desc` vérifié non-nul.
        unsafe { ptr::read_unaligned(desc) }
    };
    let q = Box::new(D3D12QueueObject {
        header: ComHeader::new(traced_vtbl(
            (&raw const D3D12_QUEUE_VTBL).cast(),
            "ID3D12CommandQueue",
        )),
        shared: Arc::clone(&dev.shared),
        desc: q_desc,
        detected_newer_layout: AtomicBool::new(
            dev.shared.is_console_device.load(Ordering::Acquire),
        ),
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
        header: ComHeader::new(traced_vtbl(
            (&raw const D3D12_CMD_ALLOC_VTBL).cast(),
            "ID3D12CommandAllocator",
        )),
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
        if sb_gpu::rdna2_native::is_native_container(vs_bytes)
            || sb_gpu::rdna2_native::is_native_container(ps_bytes)
        {
            return native::create_native_graphics_pso(dev, vs_bytes, ps_bytes, &cfg, out_pso);
        }
        let mut renderer = dev.shared.renderer.lock();
        match renderer.create_pipeline_from_dxbc_with_config(vs_bytes, ps_bytes, &cfg) {
            Ok(pipeline_id) => {
                let pso = Box::new(D3D12PsoObject {
                    header: ComHeader::new(traced_vtbl(
                        (&raw const D3D12_PSO_VTBL).cast(),
                        "ID3D12PipelineState",
                    )),
                    pipeline_id,
                    is_compute: false,
                    native: None,
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

        if sb_gpu::rdna2_native::is_native_container(cs_bytes) {
            return native::create_native_compute_pso(dev, cs_bytes, out_pso);
        }
        let mut renderer = dev.shared.renderer.lock();
        match renderer.create_compute_pipeline_from_dxbc(cs_bytes) {
            Ok(pipeline_id) => {
                let pso = Box::new(D3D12PsoObject {
                    header: ComHeader::new(traced_vtbl(
                        (&raw const D3D12_PSO_VTBL).cast(),
                        "ID3D12PipelineState",
                    )),
                    pipeline_id,
                    is_compute: true,
                    native: None,
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
                // D3D12_FEATURE_D3D12_OPTIONS (0x3C octets PC, 0x40 octets console)
                bytes.fill(0);
                write_u32(bytes, 4, 1); // OutputMergerLogicOp
                write_u32(bytes, 8, 3); // MinPrecisionSupport (10-bit | 16-bit)
                write_u32(bytes, 12, 3); // TiledResourcesTier = 3
                write_u32(bytes, 16, 3); // ResourceBindingTier = TIER_3
                write_u32(bytes, 20, 1); // PSSpecifiedStencilRefSupported
                write_u32(bytes, 24, 1); // TypedUAVLoadAdditionalFormats
                write_u32(bytes, 28, 1); // ROVsSupported
                write_u32(bytes, 32, 3); // ConservativeRasterizationTier = 3
                write_u32(bytes, 36, 40); // MaxGPUVirtualAddressBitsPerResource
                write_u32(bytes, 40, 1); // StandardSwizzle64KBSupported
                write_u32(bytes, 48, 1); // CrossAdapterRowMajorTextureSupported
                write_u32(bytes, 52, 1); // VPAndRTArrayIndex...
                write_u32(bytes, 56, 2); // ResourceHeapTier = TIER_2
            }
            1 => {
                // D3D12_FEATURE_ARCHITECTURE : conserve NodeIndex (+0), signale UMA cohérente.
                write_u32(bytes, 4, 0); // TileBasedRenderer = FALSE
                write_u32(bytes, 8, 1); // UMA = TRUE
                write_u32(bytes, 12, 1); // CacheCoherentUMA = TRUE
            }
            2 => {
                // D3D12_FEATURE_FEATURE_LEVELS : MaxSupportedFeatureLevel à l'offset 16
                write_u32(bytes, 16, 0xC100); // D3D_FEATURE_LEVEL_12_1
            }
            4 => {
                // D3D12_FEATURE_GPU_VIRTUAL_ADDRESS_SUPPORT
                bytes.fill(0);
                write_u32(bytes, 0, 40); // MaxGPUVirtualAddressBitsPerResource
                write_u32(bytes, 4, 40); // MaxGPUVirtualAddressBitsPerProcess
            }
            7 => {
                // D3D12_FEATURE_SHADER_MODEL : plafonne à SM 6.6 (0x66)
                if bytes.len() >= 4 {
                    let req = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                    let supported = req.clamp(0x51, 0x66);
                    write_u32(bytes, 0, supported);
                }
            }
            12 => {
                // D3D12_FEATURE_ROOT_SIGNATURE : plafonne à 1.1 (2)
                if bytes.len() >= 4 {
                    let req = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                    let supported = if req == 1 { 1 } else { 2 };
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
    this: *mut D3D12DeviceObject,
    _node_mask: u32,
    list_type: u32,
    _allocator: *mut c_void,
    initial_state: *mut D3D12PsoObject,
    riid: *const Guid,
    out_list: *mut *mut c_void,
) -> HResult {
    if out_list.is_null() {
        return E_POINTER;
    }
    let mut ops = Vec::new();
    if d3d_known_object_name(initial_state as usize) == Some("ID3D12PipelineState") {
        // SAFETY: `initial_state` validé comme `D3D12PsoObject` du bridge.
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
    let is_dma = list_type == 6
        || (!riid.is_null()
            // SAFETY: `riid` vérifié non-nul.
            && unsafe { ptr::read_unaligned(riid) } == IID_ID3D12CONSDMACOMMANDLIST);
    let is_console = !this.is_null()
        // SAFETY: `this` vérifié non-nul.
        && unsafe { (&*this).shared.is_console_device.load(Ordering::Acquire) };
    let vtbl = if is_dma {
        traced_vtbl(
            (&raw const D3D12_DMA_CMDLIST_VTBL).cast(),
            "ID3D12DmaCommandList",
        )
    } else if is_console {
        traced_vtbl(
            (&raw const D3D12_CONSOLE_CMDLIST_VTBL).cast(),
            "ID3D12GraphicsCommandList",
        )
    } else {
        traced_vtbl(
            (&raw const D3D12_CMDLIST_VTBL).cast(),
            "ID3D12GraphicsCommandList",
        )
    };
    let cl = Box::new(D3D12CmdListObject {
        header: ComHeader::new(vtbl),
        console_inline_pad: [0u8; 0x560],
        ops: Mutex::new(ops),
        closed: Mutex::new(false),
        graphics_root_regs: Mutex::new(Vec::new()),
        compute_root_regs: Mutex::new(Vec::new()),
        native: Mutex::new(native::NativeListState::default()),
        upload_arenas: Mutex::new(Vec::new()),
        list_type,
    });
    if d3d_known_object_name(initial_state as usize) == Some("ID3D12PipelineState") {
        // SAFETY: `initial_state` validé comme `D3D12PsoObject` du bridge.
        let pso = unsafe { &*initial_state };
        cl.native
            .lock()
            .note_pso(pso.pipeline_id, pso.is_compute, &pso.native);
    }
    // SAFETY: `out_list` vérifié non-nul.
    unsafe {
        *out_list = Box::into_raw(cl).cast();
    }
    S_OK
}

/// Plages mémoire `(début, octets)` des tas de descripteurs créés par le bridge : seules ces
/// zones reçoivent les mots matériels V#/T#/S# (jamais une adresse arbitraire du titre).
static DESC_HEAP_RANGES: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

/// Écrit un emplacement de descripteur (32 octets) à `dest` s'il est dans un tas de descripteurs
/// connu. Renvoie `false` (rien d'écrit) sinon.
fn write_descriptor_slot(dest: usize, slot: &crate::d3d12_views::SlotBytes) -> bool {
    let n = crate::d3d12_views::SLOT_BYTES;
    if dest == 0 {
        return false;
    }
    let known = DESC_HEAP_RANGES
        .lock()
        .iter()
        .any(|&(b, l)| dest >= b && l >= n && dest - b <= l - n);
    if !known {
        return false;
    }
    // SAFETY: `dest..dest+32` est inclus dans une zone de tas de descripteurs (mapping RW du
    // bridge ou réservation XMem fournie par le titre), vérifiée juste au-dessus.
    unsafe { ptr::copy_nonoverlapping(slot.as_ptr(), dest as *mut u8, n) };
    true
}

/// Lit `len` octets d'un descripteur de vue. `Ok(None)` si le pointeur est NUL (vue par défaut),
/// `Err(())` si la mémoire n'est pas lisible.
fn view_desc_bytes(desc: *const c_void, len: usize) -> Result<Option<Vec<u8>>, ()> {
    if desc.is_null() {
        return Ok(None);
    }
    let v = sb_loader::trampoline::peek_memory(desc as usize, len);
    if v.is_empty() { Err(()) } else { Ok(Some(v)) }
}

/// Construit puis écrit la vue de ressource (`SRV`/`UAV`/`RTV`/`DSV`) à `dest`.
fn write_resource_view(
    resource: *mut D3D12ResourceObject,
    desc: *const c_void,
    desc_len: usize,
    dest: usize,
    build: fn(
        &crate::d3d12_views::ResourceView,
        Option<&[u8]>,
    ) -> Option<crate::d3d12_views::SlotBytes>,
) {
    let Ok(bytes) = view_desc_bytes(desc, desc_len) else {
        return;
    };
    // SAFETY: appelant : `resource` validé par `is_d3d12_resource`.
    let res = unsafe { &*resource };
    let view = crate::d3d12_views::ResourceView {
        desc: &res.desc,
        address: res.fixed_addr as u64,
    };
    if let Some(slot) = build(&view, bytes.as_deref()) {
        write_descriptor_slot(dest, &slot);
    }
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
    let mut base = NEXT_DESC_HANDLE.fetch_add(count * 32, Ordering::Relaxed) as usize;
    let mut provided = false;
    // Variante console : le descripteur est suivi (à partir de l'octet 16) d'une adresse de mémoire
    // fournie par le titre, et celui-ci vérifie que le handle CPU de début du tas la reprend
    // exactement (assertion `int3` sinon). On adopte donc la première adresse située dans une réservation XMem.
    if !desc.is_null() {
        let raw = sb_loader::trampoline::peek_memory(desc as usize, 64);
        let mut given = 0usize;
        for chunk in raw.get(16..).unwrap_or(&[]).as_chunks::<8>().0 {
            let v = u64::from_le_bytes(*chunk) as usize;
            if v >= 0x1_0000 && crate::gdk_xs::is_xmem_address(v) {
                given = v;
                break;
            }
        }
        static LOGGED: AtomicU32 = AtomicU32::new(0);
        if LOGGED.fetch_add(1, Ordering::Relaxed) < 4 {
            sb_info!(
                CHANNEL,
                "CreateDescriptorHeap : type={} n={} drapeaux={:#x} masque={:#x}, octets 16..64={:02x?} -> adresse fournie {given:#x}",
                d.heap_type,
                d.num_descriptors,
                d.flags,
                d.node_mask,
                raw.get(16..).unwrap_or(&[])
            );
        }
        if given != 0 {
            base = given;
            provided = true;
        }
    }
    // Sur la console, les handles CPU de descripteurs sont de vraies adresses que le titre écrit
    // directement : sans mémoire fournie, le tas reçoit sa propre zone (`count` × 32 octets).
    if !provided {
        let len = (count as usize * 32).next_multiple_of(4096);
        // SAFETY: mapping anonyme privé ; zone libérée avec le processus (les tas vivent longtemps).
        let p = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if p != libc::MAP_FAILED {
            base = p as usize;
        }
    }
    DESC_HEAP_RANGES.lock().push((base, count as usize * 32));
    let heap = Box::new(D3D12DescHeapObject {
        header: ComHeader::new(traced_vtbl(
            (&raw const D3D12_DESC_HEAP_VTBL).cast(),
            "ID3D12DescriptorHeap",
        )),
        desc: d,
        base_handle: base,
    });
    // SAFETY: `out_heap` vérifié non-nul.
    unsafe {
        *out_heap = Box::into_raw(heap).cast();
    }
    S_OK
}

/// Copie `count` descripteurs de 32 octets (mémoire des tas + table des liaisons).
fn copy_descriptor_run(dev: &D3D12DeviceObject, dst: usize, src: usize, count: usize) {
    if dst == 0 || src == 0 || dst == src {
        return;
    }
    let mut table = dev.shared.descriptors.lock();
    for i in 0..count {
        let (d, s) = (dst + i * 32, src + i * 32);
        if let Some(b) = table.get(&s).copied() {
            table.insert(d, b);
        } else {
            table.remove(&d);
        }
    }
    drop(table);
    // Les handles sont de vraies adresses sur la console : le titre peut lire/écrire les octets.
    if crate::gdk_xs::is_xmem_address(src) || !sb_loader::trampoline::peek_memory(src, 1).is_empty()
    {
        // SAFETY: les deux plages sont des zones de tas de descripteurs de `count × 32` octets.
        unsafe { core::ptr::copy(src as *const u8, dst as *mut u8, count * 32) };
    }
}

/// `ID3D12Device::CopyDescriptorsSimple(num, destStart, srcStart, type)` (slot 24).
extern "win64" fn device_copy_descriptors_simple(
    this: *mut D3D12DeviceObject,
    num: u32,
    dest_start: usize,
    src_start: usize,
) {
    if this.is_null() {
        return;
    }
    // SAFETY: `this` vérifié non-nul.
    copy_descriptor_run(unsafe { &*this }, dest_start, src_start, num as usize);
}

/// `ID3D12Device::CopyDescriptors(numDest, destStarts, destSizes, numSrc, srcStarts, srcSizes, type)`
/// (slot 23) : les plages sont aplaties puis copiées descripteur par descripteur.
extern "win64" fn device_copy_descriptors(
    this: *mut D3D12DeviceObject,
    num_dest: u32,
    dest_starts: *const usize,
    dest_sizes: *const u32,
    num_src: u32,
    src_starts: *const usize,
    src_sizes: *const u32,
) {
    if this.is_null() || dest_starts.is_null() || src_starts.is_null() {
        return;
    }
    let ranges = |n: u32, starts: *const usize, sizes: *const u32| -> Vec<usize> {
        let mut v = Vec::new();
        for i in 0..n as usize {
            // SAFETY: l'appelant fournit `n` débuts (et `n` tailles si `sizes` est non nul).
            let (start, size) = unsafe {
                (
                    *starts.add(i),
                    if sizes.is_null() { 1 } else { *sizes.add(i) },
                )
            };
            for k in 0..size as usize {
                v.push(start + k * 32);
            }
        }
        v
    };
    let dst = ranges(num_dest, dest_starts, dest_sizes);
    let src = ranges(num_src, src_starts, src_sizes);
    // SAFETY: `this` vérifié non-nul.
    let dev = unsafe { &*this };
    for (d, s) in dst.iter().zip(src.iter()) {
        copy_descriptor_run(dev, *d, *s, 1);
    }
}

/// Taille d'allocation (octets, alignée sur 64 KiB) d'une ressource décrite par `d`.
fn resource_allocation_size(d: &D3D12ResourceDesc) -> u64 {
    const ALIGN: u64 = 65536;
    let raw = if d.dimension == 1 {
        d.width
    } else {
        let (row, rows) =
            texture_row_layout(d.width.min(u64::from(u32::MAX)) as u32, d.height, d.format);
        let pitch = (row as u64).next_multiple_of(256);
        let depth = u64::from(d.depth_or_array_size.max(1));
        let base = pitch * rows as u64 * depth;
        // Chaîne de mips : ~4/3 de la base.
        if d.mip_levels > 1 {
            base + base / 3
        } else {
            base
        }
    };
    raw.max(1).next_multiple_of(ALIGN)
}

/// `ID3D12Device::GetResourceAllocationInfo(visibleMask, numDescs, descs)` (slot 25).
///
/// La structure rendue (16 octets) passe par un pointeur de retour caché : `rcx` = destination,
/// `rdx` = `this`. Taille = somme des tailles alignées sur 64 KiB, alignement 64 KiB.
extern "win64" fn device_get_resource_allocation_info(
    out: *mut [u64; 2],
    _this: *mut D3D12DeviceObject,
    _visible_mask: u32,
    num_descs: u32,
    descs: *const D3D12ResourceDesc,
) -> *mut [u64; 2] {
    let mut total = 0u64;
    if !descs.is_null() {
        for i in 0..num_descs as usize {
            // SAFETY: l'appelant fournit `num_descs` descripteurs consécutifs.
            let d = unsafe { *descs.add(i) };
            total += resource_allocation_size(&d);
        }
    }
    if !out.is_null() {
        // SAFETY: `out` est la destination fournie par l'appelant (16 octets).
        unsafe { *out = [total, 65536] };
    }
    static LOGGED: AtomicU32 = AtomicU32::new(0);
    if LOGGED.fetch_add(1, Ordering::Relaxed) < 4 {
        let first = if descs.is_null() || num_descs == 0 {
            None
        } else {
            // SAFETY: `descs` non nul, au moins un descripteur.
            Some(unsafe { *descs })
        };
        sb_info!(
            CHANNEL,
            "GetResourceAllocationInfo : {num_descs} desc(s), premier = {:?} -> taille {total:#x}, alignement 0x10000",
            first.map(|d| (d.dimension, d.width, d.height, d.format, d.layout))
        );
    }
    out
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
    let mut layout = None;
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
            // Extension : `count` paires (type, num_values) après les registres.
            let ext = 8 + count * 4;
            let rd = |o: usize| {
                bytes
                    .get(o..o + 4)
                    .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            };
            let params: Option<Vec<_>> = (0..count)
                .map(|i| {
                    let kind = native::RootSigLayout::kind_from_d3d12(rd(ext + i * 8)?)?;
                    Some((kind, rd(ext + i * 8 + 4)?))
                })
                .collect();
            if let Some(p) = params {
                layout = native::RootSigLayout::from_params(p).map(Arc::new);
            }
        } else if &bytes[0..4] == b"DXBC"
            && let Ok(sig) = crate::root_signature::parse_rts0(bytes)
        {
            layout = native::RootSigLayout::from_params(
                sig.params.iter().map(|p| (p.kind, p.num_values)).collect(),
            )
            .map(Arc::new);
        }
    }
    let rs = Box::new(D3D12RootSigObject {
        header: ComHeader::new(traced_vtbl(
            (&raw const D3D12_ROOTSIG_VTBL).cast(),
            "ID3D12RootSignature",
        )),
        param_registers,
        layout,
    });
    // SAFETY: `out_rs` vérifié non-nul.
    unsafe {
        *out_rs = Box::into_raw(rs).cast();
    }
    sb_debug!(
        CHANNEL,
        "ID3D12Device::CreateRootSignature(blob_len={blob_len}) -> S_OK"
    );
    S_OK
}

extern "win64" fn device_create_cbv(
    this: *mut D3D12DeviceObject,
    desc: *const D3D12ConstantBufferViewDesc,
    dest_descriptor: usize,
) {
    if this.is_null()
        || desc.is_null()
        || sb_loader::trampoline::peek_memory(
            desc as usize,
            size_of::<D3D12ConstantBufferViewDesc>(),
        )
        .len()
            != size_of::<D3D12ConstantBufferViewDesc>()
    {
        return;
    }
    // SAFETY: `this` non-nul et `desc` vérifié lisible via `peek_memory`.
    let dev = unsafe { &*this };
    let d = unsafe { ptr::read_unaligned(desc) };
    if dev.shared.is_console_device.load(Ordering::Acquire)
        && !crate::gdk_xs::is_xmem_address(dest_descriptor)
    {
        static LOGGED_LOCAL: AtomicU32 = AtomicU32::new(0);
        if LOGGED_LOCAL.fetch_add(1, Ordering::Relaxed) < 2 {
            let raw_desc = sb_loader::trampoline::peek_memory(desc as usize, 16);
            let raw_dest = sb_loader::trampoline::peek_memory(dest_descriptor, 16);
            sb_info!(
                CHANNEL,
                "ID3D12Device::CreateConstantBufferView console hors-XMem (va={:#x}, taille={:#x}, dest={dest_descriptor:#x}, desc={raw_desc:02x?}, dest_avant={raw_dest:02x?})",
                d.buffer_location,
                d.size_in_bytes
            );
        }
    }
    dev.shared.descriptors.lock().insert(
        dest_descriptor,
        DescriptorBinding::Cbv {
            addr: d.buffer_location,
            size: d.size_in_bytes,
        },
    );
    write_descriptor_slot(
        dest_descriptor,
        &crate::d3d12_views::cbv_slot(d.buffer_location, d.size_in_bytes),
    );
}

/// `true` si `p` désigne une `ID3D12Resource` créée par le bridge. Sur console, un titre peut
/// passer le pointeur « unbound » `0xFFFF_FFFF` (descripteur NULL) : sans cette vérification,
/// lire `fixed_addr`/`byte_len` à travers ce pointeur provoquerait un défaut d'accès hôte.
fn is_d3d12_resource(p: usize) -> bool {
    d3d_known_object_name(p) == Some("ID3D12Resource")
}

extern "win64" fn device_create_srv(
    this: *mut D3D12DeviceObject,
    resource: *mut D3D12ResourceObject,
    desc: *const c_void,
    dest_descriptor: usize,
) {
    if this.is_null() || !is_d3d12_resource(resource as usize) {
        return;
    }
    // SAFETY: `this` et `resource` vérifiés (objet `ID3D12Resource` du bridge).
    let dev = unsafe { &*this };
    dev.shared.descriptors.lock().insert(
        dest_descriptor,
        DescriptorBinding::SrvTexture {
            resource_ptr: resource as usize,
        },
    );
    write_resource_view(
        resource,
        desc,
        48,
        dest_descriptor,
        crate::d3d12_views::srv_slot,
    );
}

extern "win64" fn device_create_uav(
    this: *mut D3D12DeviceObject,
    resource: *mut D3D12ResourceObject,
    _counter_resource: *mut c_void,
    desc: *const c_void,
    dest_descriptor: usize,
) {
    if this.is_null() || !is_d3d12_resource(resource as usize) {
        return;
    }
    // SAFETY: `this` et `resource` vérifiés (objet `ID3D12Resource` du bridge).
    let dev = unsafe { &*this };
    let res = unsafe { &*resource };
    dev.shared.descriptors.lock().insert(
        dest_descriptor,
        DescriptorBinding::UavBuffer {
            addr: res.fixed_addr as u64,
            size: res.byte_len as u32,
        },
    );
    write_resource_view(
        resource,
        desc,
        48,
        dest_descriptor,
        crate::d3d12_views::uav_slot,
    );
}

extern "win64" fn device_create_rtv(
    _this: *mut D3D12DeviceObject,
    resource: *mut D3D12ResourceObject,
    desc: *const c_void,
    dest_descriptor: usize,
) {
    if is_d3d12_resource(resource as usize) {
        write_resource_view(
            resource,
            desc,
            32,
            dest_descriptor,
            crate::d3d12_views::rtv_slot,
        );
    }
}

extern "win64" fn device_create_dsv(
    _this: *mut D3D12DeviceObject,
    resource: *mut D3D12ResourceObject,
    desc: *const c_void,
    dest_descriptor: usize,
) {
    if is_d3d12_resource(resource as usize) {
        write_resource_view(
            resource,
            desc,
            32,
            dest_descriptor,
            crate::d3d12_views::dsv_slot,
        );
    }
}

/// `ID3D12Device::CreateSampler(desc, destDescriptor)` : écrit le S# GFX10 à l'adresse du handle.
extern "win64" fn device_create_sampler(
    _this: *mut D3D12DeviceObject,
    desc: *const c_void,
    dest_descriptor: usize,
) {
    if desc.is_null() {
        return;
    }
    let bytes = sb_loader::trampoline::peek_memory(desc as usize, 52);
    if let Some(slot) = crate::d3d12_views::sampler_slot(&bytes) {
        write_descriptor_slot(dest_descriptor, &slot);
    }
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
            header: ComHeader::new(traced_vtbl(
                (&raw const D3D12_HEAP_VTBL).cast(),
                "ID3D12Heap",
            )),
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
        header: ComHeader::new(traced_vtbl(
            (&raw const D3D12_FENCE_VTBL).cast(),
            "ID3D12Fence",
        )),
        completed: AtomicU64::new(initial_value),
        waiters: Mutex::new(Vec::new()),
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
            header: ComHeader::new(traced_vtbl(
                (&raw const D3D12_HEAP_VTBL).cast(),
                "ID3D12Heap",
            )),
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

/// Slot 48 du périphérique **console** : constructeur dont l'objet est choisi par l'IID demandé.
///
/// Mesuré sur un appel réel : `(this, desc*, REFIID, void**)` avec `riid = ID3D12CommandQueue` et un
/// descripteur de 32 octets dont les 16 premiers sont une `D3D12_COMMAND_QUEUE_DESC` (type, priorité,
/// drapeaux, masque de nœud) suivie de champs propres à la console. Ce slot n'est donc pas
/// `ID3D12Device3::OpenExistingHeapFromAddress` (même position dans la table PC) : sur la console
/// c'est une variante de `CreateCommandQueue`. On aiguille d'après l'IID :
/// - `ID3D12CommandQueue` → création de file (préfixe de descripteur standard, extension ignorée) ;
/// - sinon → `OpenExistingHeapFromAddress` (lecture PC du slot).
extern "win64" fn device_slot48(
    this: *mut D3D12DeviceObject,
    arg: *const c_void,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "ID3D12Device::slot48", E_UNEXPECTED, {
        let wants_queue = !riid.is_null()
            // SAFETY: `riid` non-nul ; GUID lu sans exigence d'alignement.
            && unsafe { ptr::read_unaligned(riid) } == IID_ID3D12COMMANDQUEUE;
        if !wants_queue {
            return device_open_existing_heap_from_address(this, arg, riid, out);
        }
        if arg.is_null() || out.is_null() {
            return E_POINTER;
        }
        // SAFETY: `out` non-nul ; sortie remise à NULL en cas d'échec.
        unsafe { *out = ptr::null_mut() };
        // Préfixe standard du descripteur (16 octets) ; le reste est une extension de la console.
        let raw = sb_loader::trampoline::peek_memory(arg as usize, 16);
        if raw.len() < 16 {
            sb_warn!(
                CHANNEL,
                "slot 48 (file de commandes) : descripteur illisible -> E_INVALIDARG"
            );
            return E_INVALIDARG;
        }
        let word = |i: usize| u32::from_le_bytes([raw[i], raw[i + 1], raw[i + 2], raw[i + 3]]);
        let desc = D3D12CommandQueueDesc {
            queue_type: word(0),
            priority: word(4) as i32,
            flags: word(8),
            node_mask: word(12),
        };
        if desc.queue_type > 3 && desc.queue_type != 6 {
            sb_warn!(
                CHANNEL,
                "slot 48 (file de commandes) : type {} invalide -> E_INVALIDARG",
                desc.queue_type
            );
            return E_INVALIDARG;
        }
        if !this.is_null() {
            // SAFETY: `this` vérifié non-nul (appel console `slot 48`).
            let dev = unsafe { &*this };
            dev.shared.is_console_device.store(true, Ordering::Release);
        }
        let hr = device_create_command_queue(this, &desc, riid, out);
        if hr == S_OK {
            static LOGGED: AtomicU32 = AtomicU32::new(0);
            if LOGGED.fetch_add(1, Ordering::Relaxed) < 6 {
                sb_info!(
                    CHANNEL,
                    "slot 48 : CreateCommandQueue de la console (type={}, priorité={}, drapeaux={:#x}, masque={:#x}) -> S_OK (les suivants en debug)",
                    desc.queue_type,
                    desc.priority,
                    desc.flags,
                    desc.node_mask
                );
            }
        }
        hr
    })
}

/// Slot 73 du périphérique **console** : création d'une liste de commandes.
///
/// Mesuré sur un appel réel : `(this, desc*, ID3D12CommandAllocator*, NULL, …)` où `desc` est un
/// descripteur étendu de 32 octets ou plus (même forme que celui de la création de file, type de
/// liste dans le premier mot), le troisième argument est un allocateur renvoyé par le bridge et le
/// quatrième est NULL (PSO initial). L'IID et la sortie suivent sur la pile : c'est la forme de
/// `CreateCommandList(nodeMask, type, allocator, pso, riid, ppv)` où le masque et le type sont
/// remplacés par un descripteur. Signature **déduite de ces arguments**.
extern "win64" fn device_slot73(
    this: *mut D3D12DeviceObject,
    desc: *const c_void,
    allocator: *mut c_void,
    initial_pso: *mut D3D12PsoObject,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "ID3D12Device::slot73", E_UNEXPECTED, {
        if out.is_null() {
            return E_POINTER;
        }
        // SAFETY: `out` non-nul ; sortie remise à NULL en cas d'échec.
        unsafe { *out = ptr::null_mut() };
        if d3d_known_object_name(allocator as usize) != Some("ID3D12CommandAllocator") {
            sb_warn!(
                CHANNEL,
                "slot 73 (liste de commandes) : le troisième argument n'est pas un allocateur du bridge -> E_INVALIDARG"
            );
            return E_INVALIDARG;
        }
        if !riid.is_null() {
            // SAFETY: `riid` non-nul ; GUID lu sans exigence d'alignement.
            let wanted = unsafe { ptr::read_unaligned(riid) };
            if wanted != IID_ID3D12GRAPHICSCOMMANDLIST
                && wanted != IID_ID3D12GRAPHICSCOMMANDLIST1
                && wanted != IID_ID3D12GRAPHICSCOMMANDLIST2
                && wanted != IID_ID3D12CONSDMACOMMANDLIST
                && wanted != crate::com::IID_IUNKNOWN
                && let Some(other) = crate::com::known_iid_name(&wanted)
            {
                sb_warn!(
                    CHANNEL,
                    "slot 73 : interface {other} demandée pour une liste de commandes -> E_NOINTERFACE"
                );
                return sb_common::abi::E_NOINTERFACE;
            }
        }
        // Type de liste : premier mot du descripteur (0 = direct, 1 = bundle, 2 = calcul, 3 = copie, 6 = DMA console).
        let raw = sb_loader::trampoline::peek_memory(desc as usize, 4);
        let list_type = if raw.len() == 4 {
            u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])
        } else {
            0
        };
        if !this.is_null() {
            // SAFETY: `this` vérifié non-nul (appel console `slot 73`).
            let dev = unsafe { &*this };
            dev.shared.is_console_device.store(true, Ordering::Release);
        }
        if list_type > 3 && list_type != 6 {
            sb_warn!(
                CHANNEL,
                "slot 73 : type de liste {list_type} invalide -> E_INVALIDARG"
            );
            return E_INVALIDARG;
        }
        let hr = device_create_command_list(this, 0, list_type, allocator, initial_pso, riid, out);
        if hr == S_OK {
            static LOGGED: AtomicU32 = AtomicU32::new(0);
            if LOGGED.fetch_add(1, Ordering::Relaxed) < 6 {
                sb_info!(
                    CHANNEL,
                    "slot 73 : CreateCommandList de la console (type={list_type}, PSO initial={initial_pso:p}) -> S_OK (les suivants en debug)"
                );
            }
        }
        hr
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
        let f = unsafe { &*fence };
        f.signal_value(fence_value);
        S_OK
    })
}

/// Lit un `Guid` à l'adresse `addr` s'il est mappé et lisible.
fn try_read_guid(addr: usize) -> Option<Guid> {
    if addr < 0x1_0000 {
        return None;
    }
    let bytes = sb_loader::trampoline::peek_memory(addr, size_of::<Guid>());
    if bytes.len() != size_of::<Guid>() {
        return None;
    }
    // SAFETY: `bytes.len() == size_of::<Guid>()`.
    Some(unsafe { ptr::read_unaligned(bytes.as_ptr().cast::<Guid>()) })
}

/// Slot 70 du périphérique **console** : sur console Xbox Series X, sert à créer une vue
/// (SRV/UAV) directement depuis un `D3D12_RESOURCE_DESC*` en `a2` vers un descripteur en `a4`
/// (sans allouer de ressource), ou une ressource si une paire `(REFIID == IID_ID3D12Resource, ppv)`
/// est fournie dans `a3..=a8`.
#[allow(clippy::too_many_arguments)]
extern "win64" fn device_slot70(
    _this: *mut D3D12DeviceObject,
    desc: *const D3D12ResourceDesc,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
    a7: usize,
    a8: usize,
) -> HResult {
    if desc.is_null()
        || sb_loader::trampoline::peek_memory(desc as usize, size_of::<D3D12ResourceDesc>()).len()
            != size_of::<D3D12ResourceDesc>()
    {
        return E_POINTER;
    }
    // SAFETY: `desc` vérifié lisible via `peek_memory`.
    let d = unsafe { ptr::read_unaligned(desc) };
    let tail = [a3, a4, a5, a6, a7, a8];
    for i in 0..tail.len() - 1 {
        if let Some(g) = try_read_guid(tail[i])
            && (g == IID_ID3D12RESOURCE || g == IID_ID3D12OBJECT || g == crate::com::IID_IUNKNOWN)
        {
            let ppv = tail[i + 1];
            if ppv >= 0x1_0000
                && sb_loader::trampoline::peek_memory(ppv, size_of::<usize>()).len()
                    == size_of::<usize>()
            {
                let res = alloc_d3d12_resource(d, false);
                // SAFETY: `ppv` suit immédiatement `REFIID == IID_ID3D12Resource` et est mappé.
                unsafe { *(ppv as *mut *mut c_void) = res.cast() };
                sb_debug!(
                    CHANNEL,
                    "slot 70 : ressource console créée (dim={}, w={}, h={}, fmt={}) -> {res:p}",
                    d.dimension,
                    d.width,
                    d.height,
                    d.format
                );
                return S_OK;
            }
        }
    }
    sb_debug!(
        CHANNEL,
        "slot 70 : vue console depuis descripteur (dim={}, w={}, h={}, fmt={}, view_desc={a3:#x}, dest={a4:#x}) -> S_OK",
        d.dimension,
        d.width,
        d.height,
        d.format
    );
    S_OK
}

/// Slot 127 du périphérique **console** : création d'une ressource dans la mémoire fournie par le
/// titre (variante console de `CreateCommittedResource` / `CreatePlacedResource`).
///
/// Signature mesurée sur un appel réel (11 arguments) :
/// `(this, pReservationDesc*, UINT flags, D3D12_RESOURCE_DESC*, UINT state, void* clearValue,
///   void*, void*, void*, REFIID, void** ppvResource)`.
/// Le 4ᵉ argument est un `D3D12_RESOURCE_DESC` standard (vérifié : 3840×2160 RGBA8, layout 5).
/// Le titre fournit la mémoire via `XMem` ; le bridge alloue son propre tampon unifié équivalent.
#[allow(clippy::too_many_arguments)]
extern "win64" fn device_slot127(
    _this: *mut D3D12DeviceObject,
    _p_reservation: *const c_void,
    _flags: u32,
    desc: *const D3D12ResourceDesc,
    _state: u32,
    _clear_value: *mut c_void,
    _extra1: *mut c_void,
    _extra2: *mut c_void,
    _extra3: *mut c_void,
    _riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    if desc.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    // SAFETY: `desc` est un `D3D12ResourceDesc` valide fourni par le titre.
    let d = unsafe { *desc };
    let res = alloc_d3d12_resource(d, false);
    // SAFETY: `ppv` vérifié non-nul ; `res` est un objet `ID3D12Resource` du bridge.
    unsafe { *ppv = res.cast() };
    static LOGGED: AtomicU32 = AtomicU32::new(0);
    if LOGGED.fetch_add(1, Ordering::Relaxed) < 4 {
        sb_info!(
            CHANNEL,
            "slot 127 : ressource console créée (dim={}, w={}, h={}, fmt={}) -> {res:p}",
            d.dimension,
            d.width,
            d.height,
            d.format
        );
    }
    S_OK
}

/// `D3D12_RESOURCE_ALLOCATION_INFO1` (24 octets) renseigné par `GetResourceAllocationInfo1`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct D3D12ResourceAllocationInfo1 {
    offset: u64,
    alignment: u64,
    size_in_bytes: u64,
}

const _: () = {
    assert!(size_of::<D3D12ResourceAllocationInfo1>() == 24);
    assert!(offset_of!(D3D12ResourceAllocationInfo1, offset) == 0);
    assert!(offset_of!(D3D12ResourceAllocationInfo1, alignment) == 8);
    assert!(offset_of!(D3D12ResourceAllocationInfo1, size_in_bytes) == 16);
};

/// Slot 108 du périphérique **console** : variante console de `GetResourceAllocationInfo1` qui
/// écrit `{SizeInBytes, Alignment}` (16 octets) dans le pointeur de retour caché `rcx` (et le
/// retourne dans `rax` conformément à l'ABI Win64 pour un retour struct > 8 octets), et remplit
/// `out_info1` (`D3D12_RESOURCE_ALLOCATION_INFO1[num_descs]`) lorsqu'il est non nul.
extern "win64" fn device_slot108(
    out: *mut [u64; 2],
    _this: *mut D3D12DeviceObject,
    _visible_mask: u32,
    num_descs: u32,
    desc: *const D3D12ResourceDesc,
    out_info1: *mut D3D12ResourceAllocationInfo1,
) -> *mut [u64; 2] {
    if out.is_null() {
        return ptr::null_mut();
    }
    let count = if desc.is_null() {
        0usize
    } else if num_descs == 0 {
        1usize
    } else {
        (num_descs as usize).min(1024)
    };
    let info1_bytes = count.saturating_mul(size_of::<D3D12ResourceAllocationInfo1>());
    let write_info1 = !out_info1.is_null()
        && info1_bytes > 0
        && sb_loader::trampoline::peek_memory(out_info1 as usize, info1_bytes).len() == info1_bytes;

    let mut total = 0u64;
    let mut max_align = 0x1000u64;
    let mut first_dim = 0u32;
    for i in 0..count {
        // SAFETY: `desc` non-nul et contient `count` descripteurs consécutifs.
        let d = unsafe { ptr::read_unaligned(desc.add(i)) };
        if i == 0 {
            first_dim = d.dimension;
        }
        let size = resource_allocation_size(&d);
        let align = d.alignment.max(0x1000);
        let offset = total.next_multiple_of(align);
        total = offset.saturating_add(size);
        max_align = max_align.max(align);
        if write_info1 {
            // SAFETY: `out_info1` vérifié mappé pour `count` entrées via `peek_memory`.
            unsafe {
                ptr::write_unaligned(
                    out_info1.add(i),
                    D3D12ResourceAllocationInfo1 {
                        offset,
                        alignment: align,
                        size_in_bytes: size,
                    },
                );
            }
        }
    }
    // SAFETY: `out` vérifié non-nul (16 octets).
    unsafe { *out = [total, max_align] };
    static LOGGED: AtomicU32 = AtomicU32::new(0);
    if LOGGED.fetch_add(1, Ordering::Relaxed) < 4 {
        sb_info!(
            CHANNEL,
            "slot 108 : GetResourceAllocationInfo console (n={count}, dim={first_dim}) -> taille {total:#x}, alignement {max_align:#x}"
        );
    }
    out
}

/// `ID3D12Device::GetCustomHeapProperties(nodeMask, heapType)` (slot 26).
/// Retourne la structure `D3D12_HEAP_PROPERTIES` (20 octets) par pointeur de retour caché.
/// Tolère les deux ordres de registres (`(out, this, ...)` et `(this, out, ...)`).
extern "win64" fn device_get_custom_heap_properties(
    a1: usize,
    a2: usize,
    node_mask: u32,
    heap_type: u32,
) -> *mut D3D12HeapProperties {
    let out = if d3d_known_object_name(a1) == Some("ID3D12Device") {
        a2 as *mut D3D12HeapProperties
    } else {
        a1 as *mut D3D12HeapProperties
    };
    if !out.is_null() {
        let mask = node_mask.max(1);
        let cpu_page_property = match heap_type {
            2 => 2, // UPLOAD -> WRITE_COMBINE
            3 => 3, // READBACK -> WRITE_BACK
            _ => 1, // DEFAULT -> NOT_AVAILABLE
        };
        // SAFETY: `out` est le tampon de retour caché fourni par l'appelant (20 octets).
        unsafe {
            ptr::write_unaligned(
                out,
                D3D12HeapProperties {
                    heap_type: 4, // D3D12_HEAP_TYPE_CUSTOM
                    cpu_page_property,
                    memory_pool_preference: 1, // D3D12_MEMORY_POOL_L0 (UMA)
                    creation_node_mask: mask,
                    visible_node_mask: mask,
                },
            );
        }
    }
    out
}

/// `ID3D12Device::CreateReservedResource(pDesc, InitialState, pOptimizedClearValue, riid, ppvResource)`
/// (slot 30) : alloue une ressource unifiée équivalente pour les ressources tuilées/réservées.
extern "win64" fn device_create_reserved_resource(
    _this: *mut D3D12DeviceObject,
    desc: *const D3D12ResourceDesc,
    _initial_state: u32,
    _clear_value: *const c_void,
    _riid: *const Guid,
    out_resource: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CreateReservedResource", E_UNEXPECTED, {
        if desc.is_null() || out_resource.is_null() {
            return E_POINTER;
        }
        // SAFETY: `desc` vérifié non-nul.
        let d = unsafe { ptr::read_unaligned(desc) };
        let res = alloc_d3d12_resource(d, false);
        // SAFETY: `out_resource` vérifié non-nul.
        unsafe { *out_resource = res.cast() };
        sb_debug!(
            CHANNEL,
            "ID3D12Device::CreateReservedResource(dim={}, w={}, h={}, fmt={}) -> {res:p}",
            d.dimension,
            d.width,
            d.height,
            d.format
        );
        S_OK
    })
}

/// Slot 39 de `ID3D12Device` :
/// - Sur PC (`d3d12.h`), `GetResourceTiling(pTiledResource, ...)` où `a2` est une `ID3D12Resource`.
/// - Sur console Xbox (`d3d12_xs.h`, décalage `-3` sans les méthodes PC shared/residency),
///   `CreateQueryHeap(pDesc, REFIID, ppvHeap)` où `a2` est un `D3D12_QUERY_HEAP_DESC*`, `a3` un
///   `REFIID` (en `.rdata` lecture seule) et `a4` le pointeur de sortie `void**`.
#[allow(clippy::too_many_arguments)]
extern "win64" fn device_slot39_dispatch(
    this: *mut D3D12DeviceObject,
    a2: *mut c_void,
    a3: *mut c_void,
    a4: *mut c_void,
    a5: *mut c_void,
    a6: *mut c_void,
    a7: u32,
    a8: *mut c_void,
) -> HResult {
    if is_d3d12_resource(a2 as usize) {
        device_get_resource_tiling(
            this,
            a2.cast(),
            a3.cast(),
            a4.cast(),
            a5.cast(),
            a6.cast(),
            a7,
            a8.cast(),
        );
        return S_OK;
    }
    static LOGGED: AtomicU32 = AtomicU32::new(0);
    if LOGGED.fetch_add(1, Ordering::Relaxed) < 4 {
        let desc_words =
            if !a2.is_null() && sb_loader::trampoline::peek_memory(a2 as usize, 12).len() == 12 {
                // SAFETY: 12 octets vérifiés lisibles via `peek_memory`.
                unsafe { ptr::read_unaligned(a2.cast::<[u32; 3]>()) }
            } else {
                [0; 3]
            };
        sb_info!(
            CHANNEL,
            "slot 39 : CreateQueryHeap console (desc={desc_words:?}, riid={}, ppv={a4:p})",
            crate::com::format_guid_opt(a3.cast())
        );
    }
    device_create_query_heap(this, a2.cast(), a3.cast(), a4.cast())
}

/// `ID3D12Device::GetResourceTiling` (PC slot 39) : renseigne des paramètres de tuilage 64 Kio
/// cohérents pour les ressources réservées/tuilées.
#[allow(clippy::too_many_arguments)]
extern "win64" fn device_get_resource_tiling(
    _this: *mut D3D12DeviceObject,
    resource: *mut D3D12ResourceObject,
    num_tiles_total: *mut u32,
    packed_mip_desc: *mut [u32; 3],
    standard_tile_shape: *mut [u32; 3],
    num_subresource_tilings: *mut u32,
    _first_subresource: u32,
    subresource_tilings: *mut [u32; 3],
) {
    let tiles = if is_d3d12_resource(resource as usize) {
        // SAFETY: `resource` validé comme `ID3D12Resource` du bridge.
        let len = unsafe { (*resource).byte_len };
        ((len.div_ceil(65536)).max(1)).min(u32::MAX as usize) as u32
    } else {
        1u32
    };
    if !num_tiles_total.is_null() {
        // SAFETY: `num_tiles_total` vérifié non-nul.
        unsafe { ptr::write_unaligned(num_tiles_total, tiles) };
    }
    if !packed_mip_desc.is_null() {
        // NumStandardMips=1, NumPackedMips=0, NumTilesForPackedMips=0, StartTileIndex=0.
        // SAFETY: `packed_mip_desc` vérifié non-nul (12 octets).
        unsafe { ptr::write_unaligned(packed_mip_desc, [1u32, 0u32, 0u32]) };
    }
    if !standard_tile_shape.is_null() {
        // 128 x 128 x 1 texels par tuile 64 Kio (RGBA8).
        // SAFETY: `standard_tile_shape` vérifié non-nul (12 octets).
        unsafe { ptr::write_unaligned(standard_tile_shape, [128u32, 128u32, 1u32]) };
    }
    if !num_subresource_tilings.is_null() {
        // SAFETY: `num_subresource_tilings` vérifié non-nul.
        let cap = unsafe { ptr::read_unaligned(num_subresource_tilings) };
        if cap > 0 && !subresource_tilings.is_null() {
            // WidthInTiles=tiles, HeightInTiles=1, DepthInTiles=1, StartTileIndex=0.
            let packed_hd = (1u32) | (1u32 << 16);
            // SAFETY: `subresource_tilings` vérifié non-nul et `cap >= 1`.
            unsafe { ptr::write_unaligned(subresource_tilings, [tiles, packed_hd, 0u32]) };
            // SAFETY: `num_subresource_tilings` vérifié non-nul.
            unsafe { ptr::write_unaligned(num_subresource_tilings, 1) };
        } else {
            // SAFETY: `num_subresource_tilings` vérifié non-nul.
            unsafe { ptr::write_unaligned(num_subresource_tilings, 1) };
        }
    }
}

/// `ID3D12Device::GetAdapterLuid()` (slot 40) : LUID 64 bits non nul stable.
extern "win64" fn device_get_adapter_luid(_this: *mut D3D12DeviceObject) -> u64 {
    1
}

/// `ID3D12Device::CreateQueryHeap(pDesc, riid, ppvHeap)` (slot 42).
extern "win64" fn device_create_query_heap(
    _this: *mut D3D12DeviceObject,
    desc: *const c_void,
    _riid: *const Guid,
    out_heap: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CreateQueryHeap", E_UNEXPECTED, {
        if out_heap.is_null() {
            return E_POINTER;
        }
        let (heap_type, count) = if desc.is_null() {
            (0, 0)
        } else {
            // SAFETY: `D3D12_QUERY_HEAP_DESC` débute par `{ UINT Type; UINT Count; }`.
            let words = unsafe { ptr::read_unaligned(desc.cast::<[u32; 2]>()) };
            (words[0], words[1])
        };
        let obj = Box::new(D3D12QueryHeapObject {
            header: ComHeader::new(traced_vtbl(
                (&raw const D3D12_QUERY_HEAP_VTBL).cast(),
                "ID3D12QueryHeap",
            )),
            heap_type,
            count,
        });
        // SAFETY: `out_heap` vérifié non-nul.
        unsafe { *out_heap = Box::into_raw(obj).cast() };
        sb_debug!(
            CHANNEL,
            "ID3D12Device::CreateQueryHeap(type={heap_type}, count={count}) -> S_OK"
        );
        S_OK
    })
}

/// `ID3D12Device::CreateCommandSignature(pDesc, pRootSignature, riid, ppvCommandSignature)` (slot 41).
extern "win64" fn device_create_command_signature(
    _this: *mut D3D12DeviceObject,
    desc: *const c_void,
    _root_sig: *mut c_void,
    _riid: *const Guid,
    out_sig: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CreateCommandSignature", E_UNEXPECTED, {
        if out_sig.is_null() {
            return E_POINTER;
        }
        let byte_stride = if desc.is_null() {
            0
        } else {
            // SAFETY: le premier champ de `D3D12_COMMAND_SIGNATURE_DESC` est `UINT ByteStride`.
            unsafe { ptr::read_unaligned(desc.cast::<u32>()) }
        };
        let obj = Box::new(D3D12CmdSigObject {
            header: ComHeader::new(traced_vtbl(
                (&raw const D3D12_CMDSIG_VTBL).cast(),
                "ID3D12CommandSignature",
            )),
            byte_stride,
        });
        // SAFETY: `out_sig` vérifié non-nul.
        unsafe {
            *out_sig = Box::into_raw(obj).cast();
        }
        sb_debug!(
            CHANNEL,
            "ID3D12Device::CreateCommandSignature(byte_stride={byte_stride}) -> S_OK"
        );
        S_OK
    })
}

/// Slot 52 console : `SetDebugFlags(flags)`.
extern "win64" fn device_set_debug_flags(_this: *mut D3D12DeviceObject, flags: u32) -> HResult {
    PROCESS_DEBUG_FLAGS.store(flags, Ordering::Relaxed);
    sb_debug!(CHANNEL, "ID3D12Device::SetDebugFlags({flags:#x}) -> S_OK");
    S_OK
}

/// Slot 55 console : création d'un PSO dérivé.
extern "win64" fn device_create_derived_pso(
    _this: *mut D3D12DeviceObject,
    base_pso: *mut D3D12PsoObject,
    _desc: *const c_void,
    _riid: *const Guid,
    out_pso: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CreateDerivedPipelineState", E_UNEXPECTED, {
        if out_pso.is_null() {
            return E_POINTER;
        }
        let (pipeline_id, is_compute) =
            if d3d_known_object_name(base_pso as usize) == Some("ID3D12PipelineState") {
                // SAFETY: `base_pso` authentifié comme `D3D12PsoObject`.
                let base = unsafe { &*base_pso };
                (base.pipeline_id, base.is_compute)
            } else {
                (0, false)
            };
        let pso = Box::new(D3D12PsoObject {
            header: ComHeader::new(traced_vtbl(
                (&raw const D3D12_PSO_VTBL).cast(),
                "ID3D12PipelineState",
            )),
            pipeline_id,
            is_compute,
            native: None,
        });
        // SAFETY: `out_pso` vérifié non-nul.
        unsafe { *out_pso = Box::into_raw(pso).cast() };
        sb_debug!(
            CHANNEL,
            "ID3D12Device::CreateDerivedPipelineState(base={base_pso:p}) -> S_OK"
        );
        S_OK
    })
}

/// Slot 62 console : `CreateComputePipelineState` forme console à 6 arguments
/// `(this, pDesc, pExtra1, pExtra2, riid, ppvPipelineState)`.
extern "win64" fn device_create_compute_pso_console(
    this: *mut D3D12DeviceObject,
    desc: *const D3D12ComputePipelineStateDesc,
    _extra1: *const c_void,
    _extra2: *const c_void,
    riid: *const Guid,
    out_pso: *mut *mut c_void,
) -> HResult {
    device_create_compute_pso(this, desc, riid, out_pso)
}

/// Crée un `ID3D12PipelineState` console en localisant automatiquement la paire
/// `(REFIID == IID_ID3D12PipelineState, void** ppv)` parmi `a2..=a7` (les variantes console
/// `Deserialize*PipelineStateX` / `CreatePipelineStateX` peuvent prendre 4, 5 ou 6 arguments).
#[allow(clippy::too_many_arguments)]
fn create_console_pso_multi_arg(
    label: &str,
    is_compute: bool,
    logged: &AtomicU32,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
    a7: usize,
) -> HResult {
    let args = [a2, a3, a4, a5, a6, a7];
    let mut matched: Option<(usize, usize)> = None;
    for i in 0..args.len() - 1 {
        if let Some(g) = try_read_guid(args[i])
            && (g == IID_ID3D12PIPELINESTATE
                || g == IID_ID3D12OBJECT
                || g == crate::com::IID_IUNKNOWN)
        {
            matched = Some((i + 2, args[i + 1]));
            break;
        }
    }
    let out_addr = match matched {
        Some((_, ppv)) => ppv,
        None => a4,
    };
    if out_addr < 0x1_0000
        || sb_loader::trampoline::peek_memory(out_addr, size_of::<usize>()).len()
            != size_of::<usize>()
    {
        return E_POINTER;
    }
    let pso = Box::new(D3D12PsoObject {
        header: ComHeader::new(traced_vtbl(
            (&raw const D3D12_PSO_VTBL).cast(),
            "ID3D12PipelineState",
        )),
        pipeline_id: 0,
        is_compute,
        native: None,
    });
    let raw = Box::into_raw(pso).cast::<c_void>();
    // SAFETY: `out_addr` est vérifié mappé (`peek_memory`) et suit `REFIID` (ou `a4` par défaut).
    unsafe { *(out_addr as *mut *mut c_void) = raw };
    if logged.fetch_add(1, Ordering::Relaxed) < 2 {
        sb_info!(
            CHANNEL,
            "ID3D12Device::{label}(a2={a2:#x}, a3={a3:#x}, a4={a4:#x}, a5={a5:#x}, a6={a6:#x}, a7={a7:#x}, riid_slot={:?}) -> {raw:p}",
            matched.map(|(slot, _)| slot)
        );
    } else {
        sb_debug!(CHANNEL, "ID3D12Device::{label} -> {raw:p}");
    }
    S_OK
}

/// Slots 56 / 57 / 66 console : `DeserializeGraphicsPipelineStateX(..., riid, ppvPipelineState)`.
#[allow(clippy::too_many_arguments)]
extern "win64" fn device_deserialize_graphics_pso_x(
    _this: *mut D3D12DeviceObject,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
    a7: usize,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "DeserializeGraphicsPipelineStateX",
        E_UNEXPECTED,
        {
            static LOGGED: AtomicU32 = AtomicU32::new(0);
            create_console_pso_multi_arg(
                "DeserializeGraphicsPipelineStateX",
                false,
                &LOGGED,
                a2,
                a3,
                a4,
                a5,
                a6,
                a7,
            )
        }
    )
}

/// Slot 67 console : `DeserializeComputePipelineStateX(..., riid, ppvPipelineState)`.
#[allow(clippy::too_many_arguments)]
extern "win64" fn device_deserialize_compute_pso_x(
    _this: *mut D3D12DeviceObject,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
    a7: usize,
) -> HResult {
    ffi_guard!(CHANNEL, "DeserializeComputePipelineStateX", E_UNEXPECTED, {
        static LOGGED: AtomicU32 = AtomicU32::new(0);
        create_console_pso_multi_arg(
            "DeserializeComputePipelineStateX",
            true,
            &LOGGED,
            a2,
            a3,
            a4,
            a5,
            a6,
            a7,
        )
    })
}

/// Slot 78 console : `SetFrameIntervalX(pOutput, intervalUs, period, flags)`.
extern "win64" fn device_set_frame_interval_x(
    this: *mut D3D12DeviceObject,
    _output: *mut c_void,
    interval_us: u32,
    period: u32,
    flags: u32,
) -> HResult {
    ffi_guard!(CHANNEL, "SetFrameIntervalX", E_UNEXPECTED, {
        if this.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: `this` vérifié non-nul.
        let dev = unsafe { &*this };
        let clamped = interval_us.max(1);
        dev.shared
            .frame_interval_us
            .store(clamped, Ordering::Release);
        sb_debug!(
            CHANNEL,
            "ID3D12Device::SetFrameIntervalX(interval_us={clamped}, period={period}, flags={flags:#x}) -> S_OK"
        );
        S_OK
    })
}

/// Slot 79 console : `ScheduleFrameEventX(eventType, offsetUs, pAncillary, flags)`.
extern "win64" fn device_schedule_frame_event_x(
    this: *mut D3D12DeviceObject,
    event_type: u32,
    offset_us: u64,
    ancillary: *const D3D12XboxFrameObjectList,
    flags: u32,
) -> HResult {
    ffi_guard!(CHANNEL, "ScheduleFrameEventX", E_UNEXPECTED, {
        if this.is_null() {
            return E_INVALIDARG;
        }
        if !ancillary.is_null() {
            // SAFETY: `ancillary` pointe vers un `D3D12XboxFrameObjectList` valide.
            let list = unsafe { ptr::read_unaligned(ancillary) };
            let n = (list.count as usize).min(64);
            for i in 0..n {
                if !list.objects.is_null() {
                    // SAFETY: `objects[i]` est un handle/pointeur dans la liste de taille `count`.
                    let handle = unsafe { ptr::read_unaligned(list.objects.add(i)) };
                    let _ = sync::SetEvent(handle);
                }
                if !list.signaled.is_null() {
                    // SAFETY: `signaled[i]` appartient au tableau de taille `count`.
                    unsafe { ptr::write_unaligned(list.signaled.add(i), 1) };
                }
            }
        }
        sb_trace!(
            CHANNEL,
            "ID3D12Device::ScheduleFrameEventX(event={event_type}, offset_us={offset_us}, flags={flags:#x}) -> S_OK"
        );
        S_OK
    })
}

/// Slot 80 console : `WaitFrameEventX(eventType, timeoutMs, pAncillary, flags, pOutToken)`.
/// Retourne toujours un jeton 64 bits non nul (`0 == D3D12XBOX_FRAME_PIPELINE_TOKEN_NULL`).
extern "win64" fn device_wait_frame_event_x(
    this: *mut D3D12DeviceObject,
    event_type: u32,
    timeout_ms: u32,
    _ancillary: *const c_void,
    flags: u32,
    out_token: *mut u64,
) -> HResult {
    ffi_guard!(CHANNEL, "WaitFrameEventX", E_UNEXPECTED, {
        if this.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: `this` vérifié non-nul.
        let dev = unsafe { &*this };
        let token = dev.shared.alloc_frame_token();
        if !out_token.is_null() {
            // SAFETY: `out_token` vérifié non-nul.
            unsafe { ptr::write_unaligned(out_token, token) };
        }
        sb_trace!(
            CHANNEL,
            "ID3D12Device::WaitFrameEventX(event={event_type}, timeout={timeout_ms}ms, flags={flags:#x}) -> token={token:#x}"
        );
        S_OK
    })
}

/// Slot 81 console : `GetFrameStatisticsX(token, typeMask, pInOutCount, pStats)`.
extern "win64" fn device_get_frame_statistics_x(
    this: *mut D3D12DeviceObject,
    _token: u64,
    type_mask: u32,
    in_out_count: *mut u32,
    stats: *mut D3D12XboxFrameStatistics,
) -> HResult {
    ffi_guard!(CHANNEL, "GetFrameStatisticsX", E_UNEXPECTED, {
        if this.is_null() {
            return E_INVALIDARG;
        }
        if in_out_count.is_null() {
            return E_POINTER;
        }
        // SAFETY: `this` et `in_out_count` vérifiés non-nuls.
        let dev = unsafe { &*this };
        let cap = unsafe { ptr::read_unaligned(in_out_count) };
        if cap == 0 || stats.is_null() {
            // SAFETY: `in_out_count` vérifié non-nul.
            unsafe { ptr::write_unaligned(in_out_count, 1) };
            return S_OK;
        }
        let now_us = dev.shared.elapsed_us();
        let interval = u64::from(dev.shared.frame_interval_us.load(Ordering::Acquire));
        let stat_type = if type_mask != 0 {
            1u32 << type_mask.trailing_zeros()
        } else {
            1
        };
        let rec = D3D12XboxFrameStatistics {
            stat_type,
            _pad0: 0,
            reserved: 0,
            origin_time_us: now_us,
            flip_time_us: now_us.saturating_add(interval),
        };
        // SAFETY: `stats` vérifié non-nul et `cap >= 1`.
        unsafe {
            ptr::write_unaligned(stats, rec);
            ptr::write_unaligned(in_out_count, 1);
        }
        S_OK
    })
}

/// Slot 86 console : `CreatePipelineStateX(..., riid, ppvPipelineState)`.
#[allow(clippy::too_many_arguments)]
extern "win64" fn device_create_pipeline_state_x(
    _this: *mut D3D12DeviceObject,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
    a7: usize,
) -> HResult {
    ffi_guard!(CHANNEL, "CreatePipelineStateX", E_UNEXPECTED, {
        static LOGGED: AtomicU32 = AtomicU32::new(0);
        create_console_pso_multi_arg(
            "CreatePipelineStateX",
            false,
            &LOGGED,
            a2,
            a3,
            a4,
            a5,
            a6,
            a7,
        )
    })
}

/// Slot 133 console : `ID3D12DeviceX::CreatePlacedResourceX(gpuVirtualAddress, pDesc,
/// initialState, pOptimizedClearValue, riid, ppvResource)`.
///
/// Place une ressource directement à une adresse virtuelle GPU unifiée fournie par le titre
/// (sans copie lorsque la plage mémoire est déjà mappée, afin que `GetGPUVirtualAddress()` et
/// `Map()` restituent exactement `gpu_va`).
#[allow(clippy::too_many_arguments)]
extern "win64" fn device_create_placed_resource_x(
    _this: *mut D3D12DeviceObject,
    gpu_va: u64,
    desc: *const D3D12ResourceDesc,
    _initial_state: u32,
    _clear_value: *const c_void,
    _riid: *const Guid,
    out_resource: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CreatePlacedResourceX", E_UNEXPECTED, {
        if desc.is_null() || out_resource.is_null() {
            return E_POINTER;
        }
        // SAFETY: `desc` vérifié non-nul.
        let d = unsafe { ptr::read_unaligned(desc) };
        let byte_len = compute_resource_byte_len(d);
        let addr = gpu_va as usize;
        let res = if addr >= 0x1_0000 && sb_loader::trampoline::peek_memory(addr, 1).len() == 1 {
            ALLOCATED_BUFFER_SIZES.lock().insert(addr, byte_len);
            // SAFETY: `addr` appartient à une région mémoire mappée par le titre ; on retient
            // une référence sur l'`Arc` (jamais relâchée) pour ne jamais libérer cette mémoire.
            let backing = unsafe { Vec::from_raw_parts(addr as *mut u8, byte_len, byte_len) };
            let backing = Arc::new(Mutex::new(backing));
            std::mem::forget(Arc::clone(&backing));
            Box::into_raw(Box::new(D3D12ResourceObject {
                header: ComHeader::new(traced_vtbl(
                    (&raw const D3D12_RESOURCE_VTBL).cast(),
                    "ID3D12Resource",
                )),
                desc: d,
                is_backbuffer: false,
                backing,
                heap_offset: 0,
                byte_len,
                fixed_addr: addr,
                gpu_texture_id: std::sync::atomic::AtomicUsize::new(usize::MAX),
                swizzle_mode: None,
            }))
        } else {
            alloc_d3d12_resource(d, false)
        };
        // SAFETY: `out_resource` vérifié non-nul.
        unsafe { *out_resource = res.cast() };
        sb_debug!(
            CHANNEL,
            "ID3D12Device::CreatePlacedResourceX(va={gpu_va:#x}, dim={}, w={}, h={}, fmt={}) -> {res:p}",
            d.dimension,
            d.width,
            d.height,
            d.format
        );
        S_OK
    })
}

const fn build_device_vtbl() -> D3D12DeviceVtbl {
    let mut slots = UNIMPL_SLOTS;
    slots[0] = vslot(device_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    fill_d3d12_object_slots(&mut slots);
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
    slots[23] = vslot(device_copy_descriptors as *const c_void);
    slots[24] = vslot(device_copy_descriptors_simple as *const c_void);
    slots[25] = vslot(device_get_resource_allocation_info as *const c_void);
    slots[26] = vslot(device_get_custom_heap_properties as *const c_void);
    slots[27] = vslot(device_create_committed_resource as *const c_void);
    slots[28] = vslot(device_create_heap as *const c_void);
    slots[29] = vslot(device_create_placed_resource as *const c_void);
    slots[30] = vslot(device_create_reserved_resource as *const c_void);
    // Slots 31..33 (`CreateSharedHandle`/`OpenSharedHandle*` sur PC) laissés en `UNIMPL_SLOTS`
    // pour détecter tout décalage console éventuel au lieu d'un `noop_ok` silencieux.
    slots[34] = vslot(noop_ok as *const c_void); // MakeResident
    slots[35] = vslot(noop_ok as *const c_void); // Evict
    slots[36] = vslot(device_create_fence as *const c_void);
    slots[37] = vslot(noop_ok as *const c_void); // GetDeviceRemovedReason -> S_OK
    slots[38] = vslot(device_get_copyable_footprints as *const c_void);
    slots[39] = vslot(device_slot39_dispatch as *const c_void);
    slots[40] = vslot(device_get_adapter_luid as *const c_void);
    slots[41] = vslot(device_create_command_signature as *const c_void);
    slots[42] = vslot(device_create_query_heap as *const c_void);
    // Slot 43 (`SetStablePowerState` sur PC) laissé en `UNIMPL_SLOTS` pour détecter tout usage console.
    slots[44] = vslot(device_create_placed_resource as *const c_void); // CreatePlacedResource console
    // `ID3D12Device1` à `ID3D12Device3` + extensions console Xbox Series X.
    slots[46] = vslot(device_set_residency_priority as *const c_void);
    slots[48] = vslot(device_slot48 as *const c_void);
    slots[50] = vslot(device_enqueue_make_resident as *const c_void);
    slots[52] = vslot(device_set_debug_flags as *const c_void);
    slots[55] = vslot(device_create_derived_pso as *const c_void);
    slots[56] = vslot(device_deserialize_graphics_pso_x as *const c_void);
    slots[57] = vslot(device_deserialize_graphics_pso_x as *const c_void); // DeserializeGraphicsPipelineStateX
    slots[60] = vslot(device_create_graphics_pso as *const c_void); // CreateGraphicsPipelineState console
    slots[61] = vslot(device_create_sampler as *const c_void); // CreateSamplerX
    slots[62] = vslot(device_create_compute_pso_console as *const c_void); // CreateComputePipelineState (6 args)
    slots[66] = vslot(device_deserialize_graphics_pso_x as *const c_void);
    slots[67] = vslot(device_deserialize_compute_pso_x as *const c_void); // DeserializeComputePipelineStateX
    slots[70] = vslot(device_slot70 as *const c_void);
    slots[73] = vslot(device_slot73 as *const c_void);
    slots[74] = vslot(noop_ok as *const c_void); // RegisterPagePoolX
    slots[75] = vslot(noop_ok as *const c_void); // UnregisterPagePoolX
    slots[76] = vslot(device_create_committed_resource as *const c_void); // CreateCommittedResource console
    slots[78] = vslot(device_set_frame_interval_x as *const c_void);
    slots[79] = vslot(device_schedule_frame_event_x as *const c_void);
    slots[80] = vslot(device_wait_frame_event_x as *const c_void);
    slots[81] = vslot(device_get_frame_statistics_x as *const c_void);
    slots[86] = vslot(device_create_pipeline_state_x as *const c_void);
    slots[108] = vslot(device_slot108 as *const c_void);
    slots[127] = vslot(device_slot127 as *const c_void);
    slots[133] = vslot(device_create_placed_resource_x as *const c_void); // CreatePlacedResourceX
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
                header: ComHeader::new(traced_vtbl(
                    (&raw const D3D12_DEVICE_VTBL).cast(),
                    "ID3D12Device",
                )),
                shared: Arc::new(SharedD3D12State::new(renderer)),
            });
            let raw = Box::into_raw(dev);
            LAST_D3D12_DEVICE.store(raw as usize, Ordering::Release);
            // SAFETY: `out_device` vérifié non-nul.
            unsafe {
                *out_device = raw.cast();
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
                // Extension (lue par `CreateRootSignature`) : nature et nombre de dwords de chaque
                // paramètre, pour le compactage user-SGPR du chemin natif RDNA 2. Les lecteurs
                // anciens ne lisent que les `n` registres ci-dessus.
                for p in params {
                    let num_values = if p.parameter_type == 1 {
                        (p.data2 & 0xFFFF_FFFF) as u32
                    } else {
                        0
                    };
                    data.extend_from_slice(&p.parameter_type.to_le_bytes());
                    data.extend_from_slice(&num_values.to_le_bytes());
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
    // `desc` est un `D3D12_VERSIONED_ROOT_SIGNATURE_DESC` : l'union (équivalente à un
    // `D3D12_ROOT_SIGNATURE_DESC`) commence à l'offset 8, après `Version`.
    let (inner, version) = if desc.is_null() {
        (core::ptr::null(), 1u32)
    } else {
        // SAFETY: `desc` pointe vers un `D3D12_VERSIONED_ROOT_SIGNATURE_DESC` valide.
        let vdesc = unsafe { &*(desc.cast::<D3D12VersionedRootSignatureDesc>()) };
        (
            core::ptr::from_ref(&vdesc.desc).cast::<c_void>(),
            vdesc.version,
        )
    };
    D3D12SerializeRootSignature(inner, version, out_blob, out_error_blob)
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
    if original_vtbl(vtbl) != (&raw const D3D12_DEVICE_VTBL).cast::<c_void>() {
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
    if original_vtbl(vtbl) != (&raw const D3D12_RESOURCE_VTBL).cast::<c_void>() {
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
        let hr = D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_12_2, riid, out_device);
        if hr == S_OK && !out_device.is_null() {
            // SAFETY: `out_device` vient d'être rempli par `D3D12CreateDevice` (`D3D12DeviceObject*`).
            let dev_ptr = unsafe { *out_device }.cast::<D3D12DeviceObject>();
            if !dev_ptr.is_null() {
                // SAFETY: `dev_ptr` est un `D3D12DeviceObject` fraîchement alloué.
                let dev = unsafe { &*dev_ptr };
                dev.shared.is_console_device.store(true, Ordering::Release);
            }
        }
        hr
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

/// Drapeaux de débogage processus (`D3D12XboxGetProcessDebugFlags` / `SetProcessDebugFlags`).
static PROCESS_DEBUG_FLAGS: AtomicU32 = AtomicU32::new(0);

/// `void D3D12ZeroMemoryX(void* dst, size_t size)` : mise à zéro directe mémoire unifiée.
pub(crate) extern "win64" fn D3D12ZeroMemoryX(dst: *mut u8, size: usize) {
    ffi_guard!(CHANNEL, "D3D12ZeroMemoryX", (), {
        if !dst.is_null() && size > 0 {
            // SAFETY: `dst[..size]` est un tampon inscriptible fourni par l'appelant.
            unsafe { ptr::write_bytes(dst, 0, size) };
        }
    });
}

/// `HRESULT D3D10CreateBlob(SIZE_T NumBytes, LPD3D10BLOB* ppBuffer)`.
pub(crate) extern "win64" fn D3D10CreateBlob(
    num_bytes: usize,
    out_blob: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "D3D10CreateBlob", E_UNEXPECTED, {
        if out_blob.is_null() {
            return E_POINTER;
        }
        let blob = Box::new(D3DBlobObject {
            header: ComHeader::new((&raw const D3D_BLOB_VTBL).cast()),
            data: vec![0u8; num_bytes],
        });
        // SAFETY: `out_blob` vérifié non-nul.
        unsafe { *out_blob = Box::into_raw(blob).cast() };
        S_OK
    })
}

/// `HRESULT D3D12CreateRootSignatureDeserializer(pSrcData, SrcDataSizeInBytes, pInterface, ppDeserializer)`.
pub(crate) extern "win64" fn D3D12CreateRootSignatureDeserializer(
    _src_data: *const c_void,
    _src_len: usize,
    _riid: *const Guid,
    out_deserializer: *mut *mut c_void,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "D3D12CreateRootSignatureDeserializer",
        E_UNEXPECTED,
        {
            if out_deserializer.is_null() {
                return E_POINTER;
            }
            let obj = Box::new(D3D12RootSigDeserObject {
                header: ComHeader::new((&raw const D3D12_ROOTSIG_DESER_VTBL).cast()),
                desc: D3D12VersionedRootSignatureDesc {
                    version: 1,
                    _pad0: 0,
                    desc: D3D12RootSignatureDesc {
                        num_parameters: 0,
                        _pad0: 0,
                        parameters: ptr::null(),
                        num_static_samplers: 0,
                        _pad1: 0,
                        static_samplers: ptr::null(),
                        flags: 0,
                        _pad2: 0,
                    },
                },
            });
            // SAFETY: `out_deserializer` vérifié non-nul.
            unsafe { *out_deserializer = Box::into_raw(obj).cast() };
            sb_debug!(
                CHANNEL,
                "D3D12CreateRootSignatureDeserializer(len={_src_len}) -> S_OK"
            );
            S_OK
        }
    )
}

/// `HRESULT D3D12CreateVersionedRootSignatureDeserializer(...)`.
pub(crate) extern "win64" fn D3D12CreateVersionedRootSignatureDeserializer(
    src_data: *const c_void,
    src_len: usize,
    riid: *const Guid,
    out_deserializer: *mut *mut c_void,
) -> HResult {
    D3D12CreateRootSignatureDeserializer(src_data, src_len, riid, out_deserializer)
}

pub(crate) extern "win64" fn D3D12EnableExperimentalFeatures(
    num_features: u32,
    _iids: *const Guid,
    _configs: *const c_void,
    _config_sizes: *const u32,
) -> HResult {
    sb_debug!(
        CHANNEL,
        "D3D12EnableExperimentalFeatures(count={num_features}) -> S_OK"
    );
    S_OK
}

pub(crate) extern "win64" fn D3D12UploadCustomMicrocodeX(
    _a1: usize,
    _a2: usize,
    _a3: usize,
    _a4: usize,
) -> HResult {
    sb_debug!(CHANNEL, "D3D12UploadCustomMicrocodeX -> S_OK");
    S_OK
}

pub(crate) extern "win64" fn D3DFlushEntireCpuCache() {
    std::sync::atomic::fence(Ordering::SeqCst);
}

/// `HRESULT DXGIXGetFrameStatistics(UINT count, DXGIX_FRAME_STATISTICS* pStats)`.
pub(crate) extern "win64" fn DXGIXGetFrameStatistics(count: u32, out_stats: *mut u8) -> HResult {
    ffi_guard!(CHANNEL, "DXGIXGetFrameStatistics", E_UNEXPECTED, {
        if !out_stats.is_null() && count > 0 {
            let bytes = (count as usize).min(16) * 64;
            // SAFETY: `out_stats` pointe vers `count` structures de 64 octets.
            unsafe { ptr::write_bytes(out_stats, 0, bytes) };
        }
        S_OK
    })
}

pub(crate) extern "win64" fn DXGIXPresentArray(
    _sync_interval: u32,
    _present_immediate_threshold: u32,
    _flags: u32,
    _num_swapchains: u32,
    _swapchains: *const *mut c_void,
    _params: *const c_void,
) -> HResult {
    S_OK
}

pub(crate) extern "win64" fn DXGIXSetFrameNotification(
    _device: *mut c_void,
    _event: usize,
) -> HResult {
    S_OK
}

pub(crate) extern "win64" fn DXGIXSetVLineNotification(
    _device: *mut c_void,
    _vline: u32,
    _event: usize,
) -> HResult {
    S_OK
}

pub(crate) extern "win64" fn D3D12XboxEnableMemoryProfiling(_enable: i32) -> HResult {
    S_OK
}

pub(crate) extern "win64" fn D3D12XboxGetProcessDebugFlags() -> u32 {
    PROCESS_DEBUG_FLAGS.load(Ordering::Relaxed)
}

pub(crate) extern "win64" fn D3D12XboxSetProcessDebugFlags(flags: u32) {
    PROCESS_DEBUG_FLAGS.store(flags, Ordering::Relaxed);
}

pub(crate) extern "win64" fn D3D12XboxUnloadDriverModule() -> HResult {
    S_OK
}

#[cfg(test)]
#[path = "d3d12_xs_views_tests.rs"]
mod views_tests;

#[cfg(test)]
mod xbox_ext_tests {
    use super::*;

    #[test]
    fn allocation_info_sums_aligned_sizes_via_hidden_return_pointer() {
        let descs = [
            D3D12ResourceDesc {
                dimension: 1,
                width: 100,
                height: 1,
                depth_or_array_size: 1,
                mip_levels: 1,
                ..Default::default()
            },
            D3D12ResourceDesc {
                dimension: 1,
                width: 70000,
                height: 1,
                depth_or_array_size: 1,
                mip_levels: 1,
                ..Default::default()
            },
        ];
        let mut out = [0u64; 2];
        let r = device_get_resource_allocation_info(
            &mut out,
            core::ptr::null_mut(),
            1,
            2,
            descs.as_ptr(),
        );
        assert_eq!(r, &mut out as *mut _);
        assert_eq!(out, [65536 + 131072, 65536]);
    }

    #[test]
    fn slot108_returns_hidden_pointer_and_populates_info1() {
        let descs = [
            D3D12ResourceDesc {
                dimension: 3,
                alignment: 0x1000,
                width: 64,
                height: 64,
                depth_or_array_size: 1,
                mip_levels: 1,
                format: 28,
                ..Default::default()
            },
            D3D12ResourceDesc {
                dimension: 1,
                alignment: 0x10000,
                width: 80000,
                height: 1,
                depth_or_array_size: 1,
                mip_levels: 1,
                ..Default::default()
            },
        ];
        let mut out = [0u64; 2];
        let mut info1 = [D3D12ResourceAllocationInfo1::default(); 2];
        let r = device_slot108(
            &mut out,
            core::ptr::null_mut(),
            0,
            2,
            descs.as_ptr(),
            info1.as_mut_ptr(),
        );
        assert_eq!(r, &mut out as *mut _);
        assert_eq!(out, [65536 + 131072, 0x10000]);
        assert_eq!(
            info1[0],
            D3D12ResourceAllocationInfo1 {
                offset: 0,
                alignment: 0x1000,
                size_in_bytes: 65536,
            }
        );
        assert_eq!(
            info1[1],
            D3D12ResourceAllocationInfo1 {
                offset: 65536,
                alignment: 0x10000,
                size_in_bytes: 131072,
            }
        );
    }

    #[test]
    fn device_slots_26_30_39_40_42_return_valid_objects_and_data() {
        let mut props = D3D12HeapProperties::default();
        let rp = device_get_custom_heap_properties((&raw mut props) as usize, 0, 1, 2);
        assert_eq!(rp, &mut props as *mut _);
        assert_eq!(props.heap_type, 4);
        assert_eq!(props.cpu_page_property, 2);
        assert_eq!(props.memory_pool_preference, 1);

        let desc = D3D12ResourceDesc {
            dimension: 3,
            width: 256,
            height: 256,
            depth_or_array_size: 1,
            mip_levels: 1,
            format: 28,
            ..Default::default()
        };
        let mut res: *mut c_void = core::ptr::null_mut();
        assert_eq!(
            device_create_reserved_resource(
                core::ptr::null_mut(),
                &desc,
                0,
                core::ptr::null(),
                &IID_ID3D12RESOURCE,
                &mut res,
            ),
            S_OK
        );
        assert!(!res.is_null());

        let mut tiles = 0u32;
        let mut packed = [0u32; 3];
        let mut shape = [0u32; 3];
        let mut sub_n = 1u32;
        let mut sub = [0u32; 3];
        assert_eq!(
            device_slot39_dispatch(
                core::ptr::null_mut(),
                res,
                (&raw mut tiles).cast(),
                (&raw mut packed).cast(),
                (&raw mut shape).cast(),
                (&raw mut sub_n).cast(),
                0,
                (&raw mut sub).cast(),
            ),
            S_OK
        );
        assert!(tiles >= 4);
        assert_eq!(shape, [128, 128, 1]);
        assert_eq!(sub_n, 1);
        assert_eq!(sub[0], tiles);

        // Slot 39 console : CreateQueryHeap(this, pDesc, REFIID, ppvHeap) ne touche pas à REFIID.
        let qdesc = [1u32, 64u32, 0u32];
        let qheap_iid = IID_ID3D12QUERYHEAP;
        let mut qheap_console: *mut c_void = core::ptr::null_mut();
        assert_eq!(
            device_slot39_dispatch(
                core::ptr::null_mut(),
                qdesc.as_ptr().cast_mut().cast(),
                (&raw const qheap_iid).cast_mut().cast(),
                (&raw mut qheap_console).cast(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                0,
                core::ptr::null_mut(),
            ),
            S_OK
        );
        assert!(!qheap_console.is_null());

        assert_ne!(device_get_adapter_luid(core::ptr::null_mut()), 0);

        let mut qheap: *mut c_void = core::ptr::null_mut();
        assert_eq!(
            device_create_query_heap(
                core::ptr::null_mut(),
                qdesc.as_ptr().cast(),
                &IID_ID3D12QUERYHEAP,
                &mut qheap,
            ),
            S_OK
        );
        assert!(!qheap.is_null());
        let mut qi_out: *mut c_void = core::ptr::null_mut();
        assert_eq!(
            query_heap_qi(qheap.cast(), &IID_ID3D12QUERYHEAP, &mut qi_out),
            S_OK
        );
        assert_eq!(qi_out, qheap);
    }

    /// Variante console de `CreateDescriptorHeap` : une adresse XMem placée après le descripteur
    /// devient le handle CPU de début du tas (le titre vérifie cette égalité par assertion).
    #[test]
    fn descriptor_heap_adopts_the_xmem_address_given_after_the_desc() {
        let region = crate::gdk_xs::xmem_test_region(1 << 20);
        assert_ne!(region, 0);
        let given = region + 0x1000;
        // Descripteur standard (type, nombre, drapeaux, masque) puis l'adresse fournie.
        let desc: [u64; 3] = [1 << 32, 0, given as u64];
        let mut out: *mut c_void = core::ptr::null_mut();
        let hr = device_create_descriptor_heap(
            core::ptr::null_mut(),
            desc.as_ptr().cast(),
            core::ptr::null(),
            &mut out,
        );
        assert_eq!(hr, S_OK);
        assert_eq!(heap_get_cpu_start(out.cast()), given);

        // Sans adresse XMem derrière le descripteur : identifiant interne habituel.
        let plain: [u64; 3] = [1 << 32, 0, 0x1234];
        let mut out2: *mut c_void = core::ptr::null_mut();
        assert_eq!(
            device_create_descriptor_heap(
                core::ptr::null_mut(),
                plain.as_ptr().cast(),
                core::ptr::null(),
                &mut out2
            ),
            S_OK
        );
        assert_ne!(heap_get_cpu_start(out2.cast()), 0x1234);
    }

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
                    assert_eq!(
                        f(fake.as_ptr() as *mut c_void, 0, 0, 0),
                        unimplemented_slot_result(),
                        $name
                    );
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
        let _serial = crate::d3d12_xs::test_serial();
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
        // La région peut fusionner avec un mapping voisin : au moins la taille demandée.
        assert!(h.size >= 65536);
        assert_eq!(h.desc.size_in_bytes as usize, h.size);
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

    /// Slot 48 de la console : file de commandes créée d'après l'IID, avec une description étendue.
    #[test]
    fn slot48_creates_a_command_queue_from_an_extended_console_descriptor() {
        let _serial = crate::d3d12_xs::test_serial();
        let mut dev: *mut c_void = ptr::null_mut();
        let hr = D3D12CreateDevice(ptr::null_mut(), 0xC100, &IID_ID3D12DEVICE, &mut dev);
        if hr != S_OK {
            return; // pas de GPU Vulkan dans cet environnement
        }
        // Octets mesurés sur la console : type=2 (calcul), priorité=0, drapeaux=1, masque=0, puis
        // une extension (ici un champ valant 0x140000000).
        let mut desc = [0u8; 32];
        desc[0] = 2;
        desc[8] = 1;
        desc[24..32].copy_from_slice(&0x1_4000_0000u64.to_le_bytes());
        let mut queue: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_slot48(
                dev.cast(),
                desc.as_ptr().cast(),
                &IID_ID3D12COMMANDQUEUE,
                &mut queue
            ),
            S_OK
        );
        assert!(!queue.is_null());
        // Type de file invalide, descripteur NULL, sortie NULL.
        let mut bad = desc;
        bad[0] = 9;
        let mut q2: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_slot48(
                dev.cast(),
                bad.as_ptr().cast(),
                &IID_ID3D12COMMANDQUEUE,
                &mut q2
            ),
            E_INVALIDARG
        );
        assert!(q2.is_null());
        assert_eq!(
            device_slot48(dev.cast(), ptr::null(), &IID_ID3D12COMMANDQUEUE, &mut q2),
            E_POINTER
        );
        // Un IID de tas retombe sur OpenExistingHeapFromAddress (adresse non alignée → refusé).
        assert_eq!(
            device_slot48(dev.cast(), desc.as_ptr().cast(), &IID_ID3D12HEAP, &mut q2),
            E_INVALIDARG
        );
    }

    /// Un argument qui pointe vers un objet du bridge (ou un tableau d'objets) est reconnu.
    #[test]
    fn unimplemented_slot_arguments_recognise_bridge_objects() {
        let queue = D3D12QueueObject {
            header: ComHeader::new(traced_vtbl(
                (&raw const D3D12_QUEUE_VTBL).cast(),
                "ID3D12CommandQueue",
            )),
            shared: Arc::new(SharedD3D12State::new(
                match VulkanRenderer::new(
                    match GpuContext::new(false) {
                        Ok(g) => g,
                        Err(_) => return, // pas de GPU Vulkan ici
                    },
                    64,
                    64,
                ) {
                    Ok(r) => r,
                    Err(_) => return,
                },
            )),
            desc: D3D12CommandQueueDesc::default(),
            detected_newer_layout: AtomicBool::new(false),
        };
        let qptr = (&raw const queue) as usize;
        assert_eq!(d3d_known_object_name(qptr), Some("ID3D12CommandQueue"));
        assert_eq!(d3d_known_object_name(0x10), None);
        assert!(describe_d3d_arg(qptr).contains("objet du bridge : ID3D12CommandQueue"));
        // Tableau dont le premier mot est un pointeur vers une file.
        let array = [qptr as u64, 1, 0, 0];
        let text = describe_d3d_arg(array.as_ptr() as usize);
        assert!(
            text.contains("mot 0") && text.contains("ID3D12CommandQueue"),
            "{text}"
        );
    }

    #[test]
    fn console_queue_slot18_writes_only_8_byte_timestamp_freq() {
        let _serial = crate::d3d12_xs::test_serial();
        let params = D3D12XboxCreateDeviceParametersPrefix {
            version: 0x20004,
            ..Default::default()
        };
        let mut dev: *mut c_void = ptr::null_mut();
        if D3D12XboxCreateDevice(ptr::null_mut(), &params, &IID_ID3D12DEVICE, &mut dev) != S_OK {
            return;
        }
        let desc = D3D12CommandQueueDesc {
            queue_type: 0,
            priority: 0,
            flags: 0,
            node_mask: 1,
        };
        let mut q: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_command_queue(dev.cast(), &desc, &IID_ID3D12COMMANDQUEUE, &mut q),
            S_OK
        );
        // `slot 18` sur une file console doit écrire 8 octets (fréquence 1 GHz) sans toucher aux
        // 8 octets suivants sur la pile (qui contenaient jadis `flags=0, node_mask=1` = 0x100000000).
        let mut buf: [u64; 2] = [0, 0xDEAD_BEEF_CAFE_BABEu64];
        let rc = queue_slot18_freq_or_desc(q.cast(), buf.as_mut_ptr() as usize);
        assert_eq!(rc, S_OK as usize);
        assert_eq!(buf[0], 1_000_000_000u64);
        assert_eq!(buf[1], 0xDEAD_BEEF_CAFE_BABEu64);
    }

    #[test]
    fn console_cmdlist_uses_xbox_slot_layout_and_guards_pointers() {
        let _serial = crate::d3d12_xs::test_serial();
        let params = D3D12XboxCreateDeviceParametersPrefix {
            version: 0x20004,
            ..Default::default()
        };
        let mut dev: *mut c_void = ptr::null_mut();
        if D3D12XboxCreateDevice(ptr::null_mut(), &params, &IID_ID3D12DEVICE, &mut dev) != S_OK {
            return;
        }
        let mut alloc: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_command_allocator(dev.cast(), 0, &IID_ID3D12COMMANDALLOCATOR, &mut alloc),
            S_OK
        );
        let mut cl: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_command_list(
                dev.cast(),
                0,
                0,
                alloc,
                ptr::null_mut(),
                &IID_ID3D12GRAPHICSCOMMANDLIST,
                &mut cl
            ),
            S_OK
        );
        assert_eq!(
            d3d_known_object_name(cl as usize),
            Some("ID3D12GraphicsCommandList")
        );

        // Sur console Xbox Series X, le slot 44 est `SetGraphicsRootShaderResourceView(root_idx, gpu_va)`
        // (décalage +4 par rapport au PC où slot 44 était `IASetVertexBuffers(start, num, views)`).
        // Un appelant console avec R9=0x300 (non utilisé par SetGraphicsRootShaderResourceView)
        // ne doit plus jamais déréférencer 0x300.
        // SAFETY: `cl` est un `D3D12CmdListObject` valide créé par `device_create_command_list`.
        let vtbl_slots = unsafe { *(cl as *const *const usize) };
        let slot44: extern "win64" fn(*mut c_void, u32, u64, usize) =
            unsafe { core::mem::transmute(*vtbl_slots.add(44)) };
        slot44(cl, 0, 0x0000_7000_0000_0000, 0x300);

        // Les slots console 126 et 142 doivent aussi être mappés sans panic.
        let slot126: extern "win64" fn(*mut c_void, usize, usize, usize) -> i32 =
            unsafe { core::mem::transmute(*vtbl_slots.add(126)) };
        let slot142: extern "win64" fn(*mut c_void, usize, usize, usize) -> i32 =
            unsafe { core::mem::transmute(*vtbl_slots.add(142)) };
        assert_eq!(slot126(cl, 0, 0, 0), S_OK);
        assert_eq!(slot142(cl, 0, 0, 0), S_OK);

        // Défense supplémentaire : même un appel direct à `IASetVertexBuffers` avec un pointeur
        // invalide (`0x300`) ne doit pas segfaulter côté hôte.
        cmdlist_ia_set_vertex_buffers(cl.cast(), 0, 1, 0x300 as *const D3D12VertexBufferView);
        cmdlist_rs_set_viewports(cl.cast(), 1, 0x300 as *const D3D12Viewport);
        cmdlist_rs_set_scissor_rects(cl.cast(), 1, 0x300 as *const D3D12Rect);
    }

    #[test]
    fn slot70_writes_resource_pointer_following_refiid() {
        let desc = D3D12ResourceDesc {
            dimension: 1,
            width: 4096,
            height: 1,
            depth_or_array_size: 1,
            mip_levels: 1,
            ..Default::default()
        };
        let iid = IID_ID3D12RESOURCE;
        let mut out: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_slot70(
                ptr::null_mut(),
                &desc,
                0,
                0,
                (&raw const iid) as usize,
                (&raw mut out) as usize,
                0,
                0,
            ),
            S_OK
        );
        assert!(!out.is_null());
        assert_eq!(d3d_known_object_name(out as usize), Some("ID3D12Resource"));
    }

    #[test]
    fn descriptor_heap_slot8_returns_desc_pointer() {
        let desc = D3D12DescriptorHeapDesc {
            heap_type: 2,
            num_descriptors: 64,
            flags: 0,
            node_mask: 1,
        };
        let mut heap: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_descriptor_heap(
                ptr::null_mut(),
                &desc,
                &IID_ID3D12DESCRIPTORHEAP,
                &mut heap
            ),
            S_OK
        );
        let mut got = D3D12DescriptorHeapDesc::default();
        // SAFETY: `heap` est un `D3D12DescHeapObject` valide.
        let vtbl_slots = unsafe { *(heap as *const *const usize) };
        let slot8: extern "win64" fn(
            *mut c_void,
            *mut D3D12DescriptorHeapDesc,
        ) -> *mut D3D12DescriptorHeapDesc = unsafe { core::mem::transmute(*vtbl_slots.add(8)) };
        let ret = slot8(heap, &mut got);
        assert_eq!(ret, &raw mut got);
        assert_eq!(got.heap_type, 2);
        assert_eq!(got.num_descriptors, 64);
        assert_eq!(got.node_mask, 1);
    }

    /// Slot 73 de la console : liste de commandes créée avec un descripteur étendu et un allocateur.
    #[test]
    fn slot73_creates_a_command_list_from_an_allocator_and_descriptor() {
        let _serial = crate::d3d12_xs::test_serial();
        let mut dev: *mut c_void = ptr::null_mut();
        if D3D12CreateDevice(ptr::null_mut(), 0xC100, &IID_ID3D12DEVICE, &mut dev) != S_OK {
            return; // pas de GPU Vulkan dans cet environnement
        }
        let mut alloc: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_command_allocator(dev.cast(), 0, &IID_ID3D12DEVICE, &mut alloc),
            S_OK
        );
        // Octets mesurés : type 0 (direct), 2, 1, 0, puis extension.
        let mut desc = [0u8; 64];
        desc[4] = 2;
        desc[8] = 1;
        desc[24..32].copy_from_slice(&0x1_4000_0000u64.to_le_bytes());
        let mut list: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_slot73(
                dev.cast(),
                desc.as_ptr().cast(),
                alloc,
                ptr::null_mut(),
                &IID_ID3D12GRAPHICSCOMMANDLIST,
                &mut list
            ),
            S_OK
        );
        assert!(!list.is_null());
        assert_eq!(
            d3d_known_object_name(list as usize),
            Some("ID3D12GraphicsCommandList")
        );
        // Allocateur invalide, sortie NULL, IID d'un autre objet, type invalide.
        let mut l2: *mut c_void = ptr::null_mut();
        let bogus = 0x1234usize as *mut c_void;
        assert_eq!(
            device_slot73(
                dev.cast(),
                desc.as_ptr().cast(),
                bogus,
                ptr::null_mut(),
                &IID_ID3D12GRAPHICSCOMMANDLIST,
                &mut l2
            ),
            E_INVALIDARG
        );
        assert_eq!(
            device_slot73(
                dev.cast(),
                desc.as_ptr().cast(),
                alloc,
                ptr::null_mut(),
                &IID_ID3D12GRAPHICSCOMMANDLIST,
                ptr::null_mut()
            ),
            E_POINTER
        );
        assert_eq!(
            device_slot73(
                dev.cast(),
                desc.as_ptr().cast(),
                alloc,
                ptr::null_mut(),
                &IID_ID3D12COMMANDQUEUE,
                &mut l2
            ),
            sb_common::abi::E_NOINTERFACE
        );
        let mut bad = desc;
        bad[0] = 9;
        assert_eq!(
            device_slot73(
                dev.cast(),
                bad.as_ptr().cast(),
                alloc,
                ptr::null_mut(),
                &IID_ID3D12GRAPHICSCOMMANDLIST,
                &mut l2
            ),
            E_INVALIDARG
        );
        assert!(l2.is_null());
    }

    #[test]
    fn residency_methods_succeed_and_signal_the_fence() {
        assert_eq!(device_set_residency_priority(), S_OK);
        let fence = Box::new(D3D12FenceObject {
            header: ComHeader::new(traced_vtbl(
                (&raw const D3D12_FENCE_VTBL).cast(),
                "ID3D12Fence",
            )),
            completed: AtomicU64::new(3),
            waiters: Mutex::new(Vec::new()),
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
    fn fence_slot12_exposes_live_completed_value_pointer_and_wakes_waiters() {
        let mut fence_raw: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_fence(ptr::null_mut(), 5, 0, &IID_ID3D12FENCE, &mut fence_raw),
            S_OK
        );
        assert!(!fence_raw.is_null());

        let completed_ptr = fence_slot12_get_completed_address(fence_raw.cast());
        assert!(!completed_ptr.is_null());
        // SAFETY: `completed_ptr` pointe vers `(*fence).completed`.
        assert_eq!(unsafe { *completed_ptr }, 5);

        let ev = sync::CreateEventW(ptr::null_mut(), 1, 0, ptr::null());
        assert_ne!(ev, 0);
        assert_eq!(fence_set_event(fence_raw.cast(), 10, ev), S_OK);
        // Pas encore signalé (5 < 10).
        assert_eq!(sync::WaitForSingleObject(ev, 0), 0x0000_0102);

        assert_eq!(queue_signal(ptr::null_mut(), fence_raw.cast(), 10), S_OK);
        // SAFETY: le pointeur retourné par le slot 12 reflète immédiatement la nouvelle valeur.
        assert_eq!(unsafe { *completed_ptr }, 10);
        assert_eq!(sync::WaitForSingleObject(ev, 0), 0);
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

    /// Cadencement d'images console (`SetFrameIntervalX` 78 -> `ScheduleFrameEventX` 79 ->
    /// `WaitFrameEventX` 80 -> `PresentX` 30/32 -> `GetFrameStatisticsX` 81).
    #[test]
    fn console_frame_pacing_and_present_x_sequence() {
        let _serial = crate::d3d12_xs::test_serial();
        let mut dev: *mut c_void = ptr::null_mut();
        if D3D12CreateDevice(ptr::null_mut(), 0xC200, &IID_ID3D12DEVICE, &mut dev) != S_OK {
            return; // pas de GPU Vulkan dans l'environnement CI
        }

        // Slots 78..81 via la vtable du périphérique.
        assert_eq!(
            device_set_frame_interval_x(dev.cast(), ptr::null_mut(), 16_667, 1, 0),
            S_OK
        );
        let mut signaled = [0i32; 2];
        let handles = [0usize; 2];
        let ancillary = D3D12XboxFrameObjectList {
            count: 2,
            _pad0: 0,
            objects: handles.as_ptr(),
            signaled: signaled.as_mut_ptr(),
        };
        assert_eq!(
            device_schedule_frame_event_x(dev.cast(), 0, 0, &ancillary, 0),
            S_OK
        );
        assert_eq!(signaled, [1, 1]);

        let mut token1 = 0u64;
        let mut token2 = 0u64;
        assert_eq!(
            device_wait_frame_event_x(dev.cast(), 0, 1000, ptr::null(), 0, &mut token1),
            S_OK
        );
        assert_eq!(
            device_wait_frame_event_x(dev.cast(), 0, 1000, ptr::null(), 0, &mut token2),
            S_OK
        );
        assert_ne!(
            token1, 0,
            "le jeton de pipeline de frame ne doit jamais être 0"
        );
        assert!(token2 > token1);

        let mut queue: *mut c_void = ptr::null_mut();
        let q_desc = D3D12CommandQueueDesc::default();
        assert_eq!(
            device_create_command_queue(dev.cast(), &q_desc, &IID_ID3D12COMMANDQUEUE, &mut queue),
            S_OK
        );
        let plane = D3D12XboxPresentPlaneParameters {
            token: token1,
            resource_count: 0,
            _pad0: 0,
            pp_resources: ptr::null(),
            src_view_rects: ptr::null(),
            dest_placement_base: ptr::null(),
            color_space: 0,
            scale_filter: 0,
            extended_desc_count: 0,
            _pad1: 0,
            extended_descs: ptr::null(),
            flags: 0,
            _pad2: 0,
        };
        let params = D3D12XboxPresentParameters {
            immediate_threshold_percent: 0.0,
            view_count: 1,
            extended_desc_count: 0,
            _pad0: 0,
            extended_descs: ptr::null(),
            flags: 0,
            _pad1: 0,
        };
        assert_eq!(queue_present_x(queue.cast(), 1, &plane, &params), S_OK);
        assert_eq!(
            queue_present_x_newer(queue.cast(), 1, &plane, &params),
            S_OK
        );

        let mut count = 2u32;
        let mut stats = [D3D12XboxFrameStatistics::default(); 2];
        assert_eq!(
            device_get_frame_statistics_x(dev.cast(), token1, 0x4, &mut count, stats.as_mut_ptr()),
            S_OK
        );
        assert_eq!(count, 1);
        assert_eq!(stats[0].stat_type, 0x4);
        assert!(stats[0].origin_time_us > 0);
        assert!(stats[0].flip_time_us >= stats[0].origin_time_us);
    }

    /// Double layout de `ID3D12CommandQueue` (slots 16/17/18) + `ID3D12CommandSignature` (slot 41) +
    /// `QueryInterface` `IDXGIDevice1` / `ID3D12Device4` + `CheckFeatureSupport` console.
    #[test]
    fn console_queue_dual_layout_and_device_console_slots() {
        let _serial = crate::d3d12_xs::test_serial();
        let mut dev: *mut c_void = ptr::null_mut();
        if D3D12CreateDevice(ptr::null_mut(), 0xC200, &IID_ID3D12DEVICE, &mut dev) != S_OK {
            return;
        }

        // QI IDXGIDevice1 & ID3D12Device4
        let mut dxgi1: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_qi(dev.cast(), &crate::com::IID_IDXGIDEVICE1, &mut dxgi1),
            S_OK
        );
        assert_eq!(d3d_known_object_name(dxgi1 as usize), Some("IDXGIDevice"));
        let mut dev4: *mut c_void = ptr::null_mut();
        assert_eq!(device_qi(dev.cast(), &IID_ID3D12DEVICE4, &mut dev4), S_OK);
        assert_eq!(dev4, dev);

        // CheckFeatureSupport : ARCHITECTURE (1), GPU_VA (4), ROOT_SIGNATURE (12), OPTIONS (0, 0x40o)
        let mut arch = [0u32; 4];
        assert_eq!(
            device_check_feature_support(dev.cast(), 1, arch.as_mut_ptr().cast(), 16),
            S_OK
        );
        assert_eq!(
            (arch[2], arch[3]),
            (1, 1),
            "UMA + CacheCoherentUMA attendus"
        );
        let mut gpu_va = [0u32; 2];
        assert_eq!(
            device_check_feature_support(dev.cast(), 4, gpu_va.as_mut_ptr().cast(), 8),
            S_OK
        );
        assert_eq!(gpu_va, [40, 40]);
        let mut rs_ver = 2u32;
        assert_eq!(
            device_check_feature_support(dev.cast(), 12, (&raw mut rs_ver).cast(), 4),
            S_OK
        );
        assert_eq!(rs_ver, 2);

        // Slot 41 : CreateCommandSignature + slot 67 : DeserializeComputePipelineStateX
        let csig_desc = [16u32, 0, 0, 0];
        let mut cmdsig: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_command_signature(
                dev.cast(),
                csig_desc.as_ptr().cast(),
                ptr::null_mut(),
                &IID_ID3D12COMMANDSIGNATURE,
                &mut cmdsig
            ),
            S_OK
        );
        assert_eq!(
            d3d_known_object_name(cmdsig as usize),
            Some("ID3D12CommandSignature")
        );
        // SAFETY: `cmdsig` vient d'être créé ci-dessus.
        assert_eq!(
            unsafe { (*cmdsig.cast::<D3D12CmdSigObject>()).byte_stride },
            16
        );

        let pso_iid = IID_ID3D12PIPELINESTATE;
        let mut cpso: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_deserialize_compute_pso_x(
                dev.cast(),
                0,
                (&raw const pso_iid) as usize,
                (&raw mut cpso) as usize,
                0,
                0,
                0,
            ),
            S_OK
        );
        assert_eq!(
            d3d_known_object_name(cpso as usize),
            Some("ID3D12PipelineState")
        );

        // Slot 57 : DeserializeGraphicsPipelineStateX (forme 4 args et forme 5 args blob+len+riid+ppv)
        let mut gpso: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_deserialize_graphics_pso_x(
                dev.cast(),
                0,
                (&raw const pso_iid) as usize,
                (&raw mut gpso) as usize,
                0,
                0,
                0,
            ),
            S_OK
        );
        assert_eq!(
            d3d_known_object_name(gpso as usize),
            Some("ID3D12PipelineState")
        );
        let mut gpso5: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_pipeline_state_x(
                dev.cast(),
                0,
                64,
                (&raw const pso_iid) as usize,
                (&raw mut gpso5) as usize,
                0,
                0,
            ),
            S_OK
        );
        assert_eq!(
            d3d_known_object_name(gpso5 as usize),
            Some("ID3D12PipelineState")
        );

        let mut guest_pool = vec![0x42u8; 4096];
        let pool_va = guest_pool.as_mut_ptr() as u64;
        let placed_desc = D3D12ResourceDesc {
            dimension: 1,
            width: 4096,
            height: 1,
            depth_or_array_size: 1,
            mip_levels: 1,
            ..Default::default()
        };
        let mut placed_res: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_placed_resource_x(
                dev.cast(),
                pool_va,
                &placed_desc,
                0xFFFF_FFFF,
                ptr::null(),
                &IID_ID3D12RESOURCE,
                &mut placed_res
            ),
            S_OK
        );
        assert_eq!(res_get_gpu_va(placed_res.cast()), pool_va);
        let mut mapped: *mut c_void = ptr::null_mut();
        assert_eq!(
            res_map(placed_res.cast(), 0, ptr::null(), &mut mapped),
            S_OK
        );
        assert_eq!(mapped as u64, pool_va);
        // SAFETY: `mapped` pointe vers `guest_pool[0]`.
        assert_eq!(unsafe { *mapped.cast::<u8>() }, 0x42);

        // File de commandes : test des deux layouts en slot 16/17/18
        let mut queue: *mut c_void = ptr::null_mut();
        let q_desc = D3D12CommandQueueDesc {
            queue_type: 2,
            priority: 100,
            flags: 0,
            node_mask: 1,
        };
        assert_eq!(
            device_create_command_queue(dev.cast(), &q_desc, &IID_ID3D12COMMANDQUEUE, &mut queue),
            S_OK
        );
        // Avant tout Fence en slot 16 : slot 16 = GetTimestampFrequency, slot 18 = GetDesc
        let mut freq = 0u64;
        assert_eq!(
            queue_slot16_signal_or_freq(queue.cast(), (&raw mut freq) as usize, 0),
            S_OK
        );
        assert_eq!(freq, 1_000_000_000);
        let mut got_desc = D3D12CommandQueueDesc::default();
        let ret = queue_slot18_freq_or_desc(queue.cast(), (&raw mut got_desc) as usize);
        assert_eq!(ret, (&raw mut got_desc) as usize);
        assert_eq!(got_desc.queue_type, 2);

        // Dès qu'une Fence arrive en slot 16 → bascule en layout console récent !
        let mut fence: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_fence(dev.cast(), 0, 0, &IID_ID3D12FENCE, &mut fence),
            S_OK
        );
        assert_eq!(
            queue_slot16_signal_or_freq(queue.cast(), fence as usize, 42),
            S_OK
        );
        assert_eq!(fence_get_completed(fence.cast()), 42);
        let mut freq_newer = 0u64;
        assert_eq!(
            queue_slot18_freq_or_desc(queue.cast(), (&raw mut freq_newer) as usize),
            S_OK as usize
        );
        assert_eq!(freq_newer, 1_000_000_000);
    }

    /// Liste de commandes console : en-tête inline nul (`+0x18`/`+0x30`), `AllocateUploadMemory` (148),
    /// `CopyBufferRegion` (15), `WriteValue32`/`WriteValue64` (71/72 dont horodatage), `Reset` (139),
    /// liste DMA (type 6) et exports libres (`D3D12ZeroMemoryX`, `D3D10CreateBlob`, `DXGIXGetFrameStatistics`).
    #[test]
    fn console_command_list_inline_safety_upload_writes_dma_and_free_exports() {
        let _serial = crate::d3d12_xs::test_serial();
        let mut dev: *mut c_void = ptr::null_mut();
        if D3D12CreateDevice(ptr::null_mut(), 0xC200, &IID_ID3D12DEVICE, &mut dev) != S_OK {
            return;
        }
        let mut queue: *mut c_void = ptr::null_mut();
        let q_desc = D3D12CommandQueueDesc::default();
        assert_eq!(
            device_create_command_queue(dev.cast(), &q_desc, &IID_ID3D12COMMANDQUEUE, &mut queue),
            S_OK
        );
        let mut cl: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_command_list(
                dev.cast(),
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                &IID_ID3D12GRAPHICSCOMMANDLIST,
                &mut cl
            ),
            S_OK
        );

        // En-tête inline console à +0x18 (write_ptr) et +0x30 (buffer_end) doivent valoir 0.
        // SAFETY: `cl` est un `D3D12CmdListObject` avec `console_inline_pad` à +0x10..+0x570.
        let (write_ptr, buffer_end) = unsafe {
            let base = cl.cast::<u8>();
            (
                ptr::read_unaligned(base.add(0x18).cast::<u64>()),
                ptr::read_unaligned(base.add(0x30).cast::<u64>()),
            )
        };
        assert_eq!(write_ptr, 0);
        assert_eq!(buffer_end, 0);

        // Slot 148 : AllocateUploadMemory (alignement 256 octets)
        let upload_ptr = cmdlist_allocate_upload_memory(cl.cast(), 512, 256);
        assert!(!upload_ptr.is_null());
        assert_eq!((upload_ptr as usize) % 256, 0);
        // SAFETY: 512 octets alloués et mappés.
        unsafe {
            ptr::write_bytes(upload_ptr.cast::<u8>(), 0x5A, 512);
            assert_eq!(*upload_ptr.cast::<u8>().add(511), 0x5A);
        }

        // Slot 15 : CopyBufferRegion + slots 71/72 : WriteValue32 / WriteValue64 (valeur + timestamp)
        let buf_desc = D3D12ResourceDesc {
            dimension: 1,
            width: 256,
            height: 1,
            depth_or_array_size: 1,
            mip_levels: 1,
            ..Default::default()
        };
        let mut src_res: *mut c_void = ptr::null_mut();
        let mut dst_res: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_committed_resource(
                dev.cast(),
                ptr::null(),
                0,
                &buf_desc,
                0,
                ptr::null(),
                &IID_ID3D12RESOURCE,
                &mut src_res
            ),
            S_OK
        );
        assert_eq!(
            device_create_committed_resource(
                dev.cast(),
                ptr::null(),
                0,
                &buf_desc,
                0,
                ptr::null(),
                &IID_ID3D12RESOURCE,
                &mut dst_res
            ),
            S_OK
        );
        resource_write_bytes(src_res, 16, b"SCARLET_DMA_TEST").expect("source");
        cmdlist_copy_buffer_region(cl.cast(), dst_res.cast(), 32, src_res.cast(), 16, 16);

        let mut marker32 = 0u32;
        let mut marker64 = 0u64;
        let mut timestamp64 = 0u64;
        cmdlist_write_value32(cl.cast(), (&raw mut marker32) as u64, 0xCAFE_BABE, 0, 0);
        cmdlist_write_value64(
            cl.cast(),
            (&raw mut marker64) as u64,
            0x0123_4567_89AB_CDEF,
            0,
            0,
        );
        cmdlist_write_value64(cl.cast(), (&raw mut timestamp64) as u64, 0, 0, 1);
        assert_eq!(cmdlist_close(cl.cast()), S_OK);

        let lists = [cl.cast::<D3D12CmdListObject>()];
        queue_execute_command_lists(queue.cast(), 1, lists.as_ptr());

        assert_eq!(marker32, 0xCAFE_BABE);
        assert_eq!(marker64, 0x0123_4567_89AB_CDEF);
        assert!(timestamp64 > 0, "horodatage GPU attendu > 0");
        // SAFETY: `dst_res` est un `D3D12ResourceObject` valide.
        let dst_obj = unsafe { &*dst_res.cast::<D3D12ResourceObject>() };
        assert_eq!(&dst_obj.backing.lock()[32..48], b"SCARLET_DMA_TEST");

        // Liste DMA console (type 6, IID_ID3D12CONSDMACOMMANDLIST)
        let mut dma_cl: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_command_list(
                dev.cast(),
                0,
                6,
                ptr::null_mut(),
                ptr::null_mut(),
                &IID_ID3D12CONSDMACOMMANDLIST,
                &mut dma_cl
            ),
            S_OK
        );
        assert_eq!(
            d3d_known_object_name(dma_cl as usize),
            Some("ID3D12DmaCommandList")
        );

        // Exports libres d3d12_xs.dll
        let mut buf = [0xFFu8; 32];
        D3D12ZeroMemoryX(buf.as_mut_ptr(), buf.len());
        assert!(buf.iter().all(|&b| b == 0));

        let mut blob: *mut c_void = ptr::null_mut();
        assert_eq!(D3D10CreateBlob(128, &mut blob), S_OK);
        assert_eq!(blob_get_size(blob.cast()), 128);
        assert!(!blob_get_ptr(blob.cast()).is_null());

        let mut dxgix_stats = [0xAAu8; 64];
        assert_eq!(DXGIXGetFrameStatistics(1, dxgix_stats.as_mut_ptr()), S_OK);
        assert!(dxgix_stats.iter().all(|&b| b == 0));

        D3DFlushEntireCpuCache();
        D3D12XboxSetProcessDebugFlags(0x3);
        assert_eq!(D3D12XboxGetProcessDebugFlags(), 0x3);
    }
}
// ==================== `IDXGIDevice` (récupéré via `ID3D12Device::QueryInterface`) ====================

#[repr(C)]
struct DxgiDeviceVtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

/// Vue `IDXGIDevice` sur un `ID3D12Device` : le titre l'obtient par `QueryInterface` et s'en sert
/// pour résoudre l'adaptateur et régler la latence. On partage l'état du périphérique.
#[repr(C)]
struct DxgiDeviceObject {
    header: ComHeader,
    device: usize,
}

extern "win64" fn dxgi_device_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "IDXGIDevice",
        this,
        riid,
        ppv,
        &[
            IID_IDXGIDEVICE,
            crate::com::IID_IDXGIDEVICE1,
            crate::com::IID_IDXGIDEVICE2,
            crate::com::IID_IDXGIDEVICE3,
        ],
    )
}

/// `IDXGIDevice::GetParent(riid, ppParent)` : l'objet parent (l'adaptateur).
extern "win64" fn dxgi_device_get_parent(
    this: *mut DxgiDeviceObject,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    if ppv.is_null() {
        return E_POINTER;
    }
    // SAFETY: `ppv` vérifié non-nul ; aucune adaptation en place, on renvoie null.
    unsafe { *ppv = ptr::null_mut() };
    let _ = (this, riid);
    sb_debug!("dxgi", "IDXGIDevice::GetParent -> S_OK (ppv=null)");
    S_OK
}

/// `IDXGIDevice::GetAdapter` : renvoie un adaptateur DXGI minimal (le titre s'en sert pour
/// énumérer les sorties/moniteurs et régler la latence).
extern "win64" fn dxgi_device_get_adapter(
    _this: *mut DxgiDeviceObject,
    ppv: *mut *mut c_void,
) -> HResult {
    if ppv.is_null() {
        return E_POINTER;
    }
    let adapter = Box::new(DxgiAdapterObject {
        header: ComHeader::new(traced_vtbl(
            (&raw const DXGI_ADAPTER_VTBL).cast(),
            "IDXGIAdapter",
        )),
    });
    // SAFETY: `ppv` vérifié non-nul ; `adapter` est un objet du bridge.
    unsafe { *ppv = Box::into_raw(adapter).cast() };
    sb_debug!("dxgi", "IDXGIDevice::GetAdapter -> S_OK");
    S_OK
}

/// `IDXGIDevice::SetGPUThreadPriority`.
extern "win64" fn dxgi_device_set_gpu_thread_priority(
    _this: *mut DxgiDeviceObject,
    _p: i32,
) -> HResult {
    S_OK
}

/// `IDXGIDevice::GetGPUThreadPriority` : priorité normale (0).
extern "win64" fn dxgi_device_get_gpu_thread_priority(_this: *mut DxgiDeviceObject) -> i32 {
    0
}

/// `IDXGIDevice1::SetMaximumFrameLatency`.
extern "win64" fn dxgi_device_set_max_frame_latency(
    _this: *mut DxgiDeviceObject,
    _l: u32,
) -> HResult {
    S_OK
}

/// `IDXGIDevice1::GetMaximumFrameLatency` : 1 frame.
extern "win64" fn dxgi_device_get_max_frame_latency(_this: *mut DxgiDeviceObject) -> u32 {
    1
}

const fn build_dxgi_device_vtbl() -> DxgiDeviceVtbl {
    let mut slots = UNIMPL_SLOTS;
    let mut pad = 0;
    while pad < 20 {
        slots[pad] = vslot(noop_ok as *const c_void);
        pad += 1;
    }
    slots[0] = vslot(dxgi_device_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    fill_dxgi_object_slots(&mut slots);
    slots[6] = vslot(dxgi_device_get_parent as *const c_void);
    slots[7] = vslot(dxgi_device_get_adapter as *const c_void);
    slots[10] = vslot(dxgi_device_set_gpu_thread_priority as *const c_void);
    slots[11] = vslot(dxgi_device_get_gpu_thread_priority as *const c_void);
    slots[12] = vslot(dxgi_device_set_max_frame_latency as *const c_void);
    slots[13] = vslot(dxgi_device_get_max_frame_latency as *const c_void);
    DxgiDeviceVtbl { slots }
}

static DXGI_DEVICE_VTBL: DxgiDeviceVtbl = build_dxgi_device_vtbl();

#[repr(C)]
#[derive(Clone, Copy)]
struct DxgiAdapterDesc {
    description: [u16; 128],
    vendor_id: u32,
    device_id: u32,
    sub_sys_id: u32,
    revision: u32,
    dedicated_video_memory: usize,
    dedicated_system_memory: usize,
    shared_system_memory: usize,
    adapter_luid: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DxgiOutputDesc {
    device_name: [u16; 32],
    desktop_coordinates: [i32; 4],
    attached_to_desktop: i32,
    rotation: u32,
    monitor: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DxgiModeDesc {
    width: u32,
    height: u32,
    refresh_num: u32,
    refresh_den: u32,
    format: u32,
    scanline_ordering: u32,
    scaling: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DxgiModeDesc1 {
    base: DxgiModeDesc,
    stereo: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DxgiFrameStatistics {
    present_count: u32,
    present_refresh_count: u32,
    sync_refresh_count: u32,
    _pad: u32,
    sync_qpc_time: i64,
    sync_gpu_time: i64,
}

const _: () = {
    assert!(size_of::<DxgiAdapterDesc>() == 304);
    assert!(offset_of!(DxgiAdapterDesc, vendor_id) == 256);
    assert!(offset_of!(DxgiAdapterDesc, dedicated_video_memory) == 272);
    assert!(offset_of!(DxgiAdapterDesc, adapter_luid) == 296);
    assert!(size_of::<DxgiOutputDesc>() == 96);
    assert!(offset_of!(DxgiOutputDesc, desktop_coordinates) == 64);
    assert!(offset_of!(DxgiOutputDesc, attached_to_desktop) == 80);
    assert!(offset_of!(DxgiOutputDesc, monitor) == 88);
    assert!(size_of::<DxgiModeDesc>() == 28);
    assert!(size_of::<DxgiModeDesc1>() == 32);
    assert!(size_of::<DxgiFrameStatistics>() == 32);
    assert!(offset_of!(DxgiFrameStatistics, sync_qpc_time) == 16);
};

const STANDARD_DISPLAY_MODES: [(u32, u32); 4] =
    [(3840, 2160), (2560, 1440), (1920, 1080), (1280, 720)];

/// `IDXGIAdapter` : adaptateur minimal renvoyé par `IDXGIDevice::GetAdapter`. Le titre l'utilise
/// pour énumérer les sorties (`EnumOutputs`) et lire la description de l'adaptateur (`GetDesc`).
#[repr(C)]
struct DxgiAdapterVtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct DxgiAdapterObject {
    header: ComHeader,
}

/// `IDXGIOutput` : sortie/moniteur minimal renvoyé par `IDXGIAdapter::EnumOutputs`. Le titre y
/// appelle `GetDisplayModeList` (slot 8 PC / slot 21 console) pour détecter les fréquences supportées.
#[repr(C)]
struct DxgiOutputVtbl {
    slots: [VtblSlot; VT_PAD_LEN],
}

#[repr(C)]
struct DxgiOutputObject {
    header: ComHeader,
}

extern "win64" fn dxgi_adapter_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("IDXGIAdapter", this, riid, ppv, &[IID_IDXGIADAPTER])
}

/// `IDXGIAdapter::EnumOutputs(Output, ppOutput)` : renvoie la sortie 0, puis `DXGI_ERROR_NOT_FOUND`.
extern "win64" fn dxgi_adapter_enum_outputs(
    _this: *mut DxgiAdapterObject,
    output_idx: u32,
    ppv: *mut *mut c_void,
) -> HResult {
    if ppv.is_null() {
        return E_POINTER;
    }
    if output_idx > 0 {
        // SAFETY: `ppv` vérifié non-nul.
        unsafe { *ppv = ptr::null_mut() };
        return DXGI_ERROR_NOT_FOUND;
    }
    let out = Box::new(DxgiOutputObject {
        header: ComHeader::new(traced_vtbl(
            (&raw const DXGI_OUTPUT_VTBL).cast(),
            "IDXGIOutput",
        )),
    });
    // SAFETY: `ppv` vérifié non-nul ; `out` est un objet du bridge.
    unsafe { *ppv = Box::into_raw(out).cast() };
    sb_debug!("dxgi", "IDXGIAdapter::EnumOutputs(0) -> S_OK");
    S_OK
}

/// `IDXGIAdapter::GetDesc` (slot 8) : description d'adaptateur AMD / ScarletBridge cohérente.
extern "win64" fn dxgi_adapter_get_desc(
    _this: *mut DxgiAdapterObject,
    desc: *mut DxgiAdapterDesc,
) -> HResult {
    if desc.is_null() {
        return E_POINTER;
    }
    let mut name = [0u16; 128];
    for (dst, ch) in name
        .iter_mut()
        .zip("ScarletBridge Radeon RX".encode_utf16())
    {
        *dst = ch;
    }
    let out = DxgiAdapterDesc {
        description: name,
        vendor_id: 0x1002,
        device_id: 0x731F,
        sub_sys_id: 0,
        revision: 0,
        dedicated_video_memory: 10usize * 1024 * 1024 * 1024,
        dedicated_system_memory: 0,
        shared_system_memory: 6usize * 1024 * 1024 * 1024,
        adapter_luid: [1, 0],
    };
    // SAFETY: `desc` vérifié non-nul.
    unsafe { ptr::write_unaligned(desc, out) };
    sb_debug!("dxgi", "IDXGIAdapter::GetDesc -> S_OK");
    S_OK
}

/// `IDXGIAdapter::CheckInterfaceSupport` (slot 9).
extern "win64" fn dxgi_adapter_check_interface_support(
    _this: *mut DxgiAdapterObject,
    _iface_name: *const Guid,
    out_umd_version: *mut i64,
) -> HResult {
    if !out_umd_version.is_null() {
        // SAFETY: `out_umd_version` vérifié non-nul.
        unsafe { ptr::write_unaligned(out_umd_version, 0x001E_0000_0001_0000) };
    }
    S_OK
}

extern "win64" fn dxgi_output_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("IDXGIOutput", this, riid, ppv, &[IID_IDXGIOUTPUT])
}

/// `IDXGIOutput::GetDesc` (slot 7) : moniteur 4K principal actif.
extern "win64" fn dxgi_output_get_desc(
    _this: *mut DxgiOutputObject,
    desc: *mut DxgiOutputDesc,
) -> HResult {
    if desc.is_null() {
        return E_POINTER;
    }
    let mut dev_name = [0u16; 32];
    for (dst, ch) in dev_name.iter_mut().zip(r"\\.\DISPLAY1".encode_utf16()) {
        *dst = ch;
    }
    let out = DxgiOutputDesc {
        device_name: dev_name,
        desktop_coordinates: [0, 0, 3840, 2160],
        attached_to_desktop: 1,
        rotation: 1, // DXGI_MODE_ROTATION_IDENTITY
        monitor: 0x0001_0001,
    };
    // SAFETY: `desc` vérifié non-nul.
    unsafe { ptr::write_unaligned(desc, out) };
    S_OK
}

/// `IDXGIOutput::GetDisplayModeList` (slot 8 standard, `DXGI_MODE_DESC` 28 octets).
extern "win64" fn dxgi_output_get_display_mode_list(
    _this: *mut DxgiOutputObject,
    format: u32,
    _flags: u32,
    out_count: *mut u32,
    desc: *mut DxgiModeDesc,
) -> HResult {
    if out_count.is_null() {
        return E_POINTER;
    }
    let fmt = if format == 0 { 28 } else { format };
    let total = STANDARD_DISPLAY_MODES.len() as u32;
    if desc.is_null() {
        // SAFETY: `out_count` vérifié non-nul.
        unsafe { ptr::write_unaligned(out_count, total) };
        sb_debug!(
            "dxgi",
            "IDXGIOutput::GetDisplayModeList(fmt={fmt}) -> {total} modes (requête taille)"
        );
        return S_OK;
    }
    // SAFETY: `out_count` vérifié non-nul.
    let cap = unsafe { ptr::read_unaligned(out_count) };
    let n = cap.min(total) as usize;
    for (i, &(w, h)) in STANDARD_DISPLAY_MODES.iter().take(n).enumerate() {
        let mode = DxgiModeDesc {
            width: w,
            height: h,
            refresh_num: 60_000,
            refresh_den: 1_000,
            format: fmt,
            scanline_ordering: 1, // DXGI_MODE_SCANLINE_ORDER_PROGRESSIVE
            scaling: 0,           // DXGI_MODE_SCALING_UNSPECIFIED
        };
        // SAFETY: `desc` non-nul et l'appelant a alloué au moins `cap` entrées.
        unsafe { ptr::write_unaligned(desc.add(i), mode) };
    }
    // SAFETY: `out_count` vérifié non-nul.
    unsafe { ptr::write_unaligned(out_count, n as u32) };
    if cap < total {
        DXGI_ERROR_MORE_DATA
    } else {
        S_OK
    }
}

/// `IDXGIOutput::GetFrameStatistics` (slot 18).
extern "win64" fn dxgi_output_get_frame_statistics(
    _this: *mut DxgiOutputObject,
    out_stats: *mut DxgiFrameStatistics,
) -> HResult {
    if out_stats.is_null() {
        return E_POINTER;
    }
    let stats = DxgiFrameStatistics {
        present_count: 1,
        present_refresh_count: 1,
        sync_refresh_count: 1,
        _pad: 0,
        sync_qpc_time: 1,
        sync_gpu_time: 1,
    };
    // SAFETY: `out_stats` vérifié non-nul.
    unsafe { ptr::write_unaligned(out_stats, stats) };
    S_OK
}

/// `IDXGIOutput1::GetDisplayModeList1` (slot 19 standard, `DXGI_MODE_DESC1` 32 octets).
extern "win64" fn dxgi_output_get_display_mode_list1(
    _this: *mut DxgiOutputObject,
    format: u32,
    _flags: u32,
    out_count: *mut u32,
    desc: *mut DxgiModeDesc1,
) -> HResult {
    if out_count.is_null() {
        return E_POINTER;
    }
    let fmt = if format == 0 { 28 } else { format };
    let total = STANDARD_DISPLAY_MODES.len() as u32;
    if desc.is_null() {
        // SAFETY: `out_count` vérifié non-nul.
        unsafe { ptr::write_unaligned(out_count, total) };
        return S_OK;
    }
    // SAFETY: `out_count` vérifié non-nul.
    let cap = unsafe { ptr::read_unaligned(out_count) };
    let n = cap.min(total) as usize;
    for (i, &(w, h)) in STANDARD_DISPLAY_MODES.iter().take(n).enumerate() {
        let mode = DxgiModeDesc1 {
            base: DxgiModeDesc {
                width: w,
                height: h,
                refresh_num: 60_000,
                refresh_den: 1_000,
                format: fmt,
                scanline_ordering: 1,
                scaling: 0,
            },
            stereo: 0,
        };
        // SAFETY: `desc` non-nul et l'appelant a alloué au moins `cap` entrées.
        unsafe { ptr::write_unaligned(desc.add(i), mode) };
    }
    // SAFETY: `out_count` vérifié non-nul.
    unsafe { ptr::write_unaligned(out_count, n as u32) };
    if cap < total {
        DXGI_ERROR_MORE_DATA
    } else {
        S_OK
    }
}

/// `IDXGIOutput::slot 21` (console) : renvoie 1 mode 4K@60Hz (28 octets `DXGI_MODE_DESC`,
/// sûr pour les appelants `DXGI_MODE_DESC` 28 o comme `DXGI_MODE_DESC1` 32 o pour 1 entrée).
extern "win64" fn dxgi_output_console_slot21(
    _this: *mut DxgiOutputObject,
    format: u32,
    flags: u32,
    out_count: *mut u32,
    desc: *mut DxgiModeDesc,
) -> HResult {
    if out_count.is_null() {
        return E_POINTER;
    }
    // SAFETY: `out_count` vérifié non-nul.
    let in_count = unsafe { ptr::read_unaligned(out_count) };
    static LOGGED: AtomicU32 = AtomicU32::new(0);
    if LOGGED.fetch_add(1, Ordering::Relaxed) < 4 {
        sb_info!(
            "dxgi",
            "IDXGIOutput::slot21(fmt={format}, flags={flags:#x}, in_count={in_count}, desc={desc:p}) -> 1 mode 3840x2160@60Hz"
        );
    }
    let fmt = if format == 0 { 28 } else { format };
    if desc.is_null() {
        // SAFETY: `out_count` vérifié non-nul.
        unsafe { ptr::write_unaligned(out_count, 1) };
        return S_OK;
    }
    if in_count == 0 {
        // SAFETY: `out_count` vérifié non-nul.
        unsafe { ptr::write_unaligned(out_count, 1) };
        return DXGI_ERROR_MORE_DATA;
    }
    let mode = DxgiModeDesc {
        width: 3840,
        height: 2160,
        refresh_num: 60_000,
        refresh_den: 1_000,
        format: fmt,
        scanline_ordering: 1,
        scaling: 0,
    };
    // SAFETY: `desc` non-nul et `in_count >= 1`.
    unsafe {
        ptr::write_unaligned(desc, mode);
        ptr::write_unaligned(out_count, 1);
    }
    S_OK
}

const fn build_dxgi_adapter_vtbl() -> DxgiAdapterVtbl {
    let mut slots = UNIMPL_SLOTS;
    slots[0] = vslot(dxgi_adapter_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    fill_dxgi_object_slots(&mut slots);
    slots[6] = vslot(noop_ok as *const c_void); // GetParent
    slots[7] = vslot(dxgi_adapter_enum_outputs as *const c_void);
    slots[8] = vslot(dxgi_adapter_get_desc as *const c_void);
    slots[9] = vslot(dxgi_adapter_check_interface_support as *const c_void);
    DxgiAdapterVtbl { slots }
}

static DXGI_ADAPTER_VTBL: DxgiAdapterVtbl = build_dxgi_adapter_vtbl();

const fn build_dxgi_output_vtbl() -> DxgiOutputVtbl {
    let mut slots = UNIMPL_SLOTS;
    slots[0] = vslot(dxgi_output_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    fill_dxgi_object_slots(&mut slots);
    slots[6] = vslot(noop_ok as *const c_void); // GetParent
    slots[7] = vslot(dxgi_output_get_desc as *const c_void);
    slots[8] = vslot(dxgi_output_get_display_mode_list as *const c_void);
    slots[10] = vslot(noop_ok as *const c_void); // WaitForVBlank
    slots[18] = vslot(dxgi_output_get_frame_statistics as *const c_void);
    slots[19] = vslot(dxgi_output_get_display_mode_list1 as *const c_void);
    slots[21] = vslot(dxgi_output_console_slot21 as *const c_void);
    DxgiOutputVtbl { slots }
}

static DXGI_OUTPUT_VTBL: DxgiOutputVtbl = build_dxgi_output_vtbl();

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
        header: ComHeader::new(traced_vtbl(
            (&raw const DXGI_SC3_VTBL).cast(),
            "IDXGISwapChain3",
        )),
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
            header: ComHeader::new(traced_vtbl(
                (&raw const DXGI_FACTORY4_VTBL).cast(),
                "IDXGIFactory4",
            )),
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
        let _serial = crate::d3d12_xs::test_serial();
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
        let _serial = crate::d3d12_xs::test_serial();
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

    #[test]
    fn d3d12_object_private_data_get_device_and_resource_helpers() {
        let _serial = crate::d3d12_xs::test_serial();
        let mut dev_ptr: *mut c_void = ptr::null_mut();
        assert_eq!(
            D3D12CreateDevice(ptr::null_mut(), 0xc200, &IID_ID3D12DEVICE, &mut dev_ptr),
            S_OK
        );
        assert!(!dev_ptr.is_null());

        // `GetPrivateData` sur clé absente renvoie `DXGI_ERROR_NOT_FOUND` (jamais `S_OK` silencieux).
        let key = Guid::new(0x1122_3344, 0x5566, 0x7788, [1, 2, 3, 4, 5, 6, 7, 8]);
        let mut size = 8u32;
        let mut out_val = 0u64;
        assert_eq!(
            d3d12_obj_get_private_data(dev_ptr.cast(), &key, &mut size, (&raw mut out_val).cast()),
            DXGI_ERROR_NOT_FOUND
        );

        let payload: u64 = 0xDEAD_BEEF_CAFE_BABE;
        assert_eq!(
            d3d12_obj_set_private_data(dev_ptr.cast(), &key, 8, (&raw const payload).cast()),
            S_OK
        );
        let mut small = 4u32;
        assert_eq!(
            d3d12_obj_get_private_data(dev_ptr.cast(), &key, &mut small, (&raw mut out_val).cast()),
            DXGI_ERROR_MORE_DATA
        );
        assert_eq!(small, 8);
        assert_eq!(
            d3d12_obj_get_private_data(dev_ptr.cast(), &key, &mut small, (&raw mut out_val).cast()),
            S_OK
        );
        assert_eq!(out_val, payload);

        // Effacement via `data_size = 0`.
        assert_eq!(
            d3d12_obj_set_private_data(dev_ptr.cast(), &key, 0, ptr::null()),
            S_OK
        );
        assert_eq!(
            d3d12_obj_get_private_data(dev_ptr.cast(), &key, &mut small, (&raw mut out_val).cast()),
            DXGI_ERROR_NOT_FOUND
        );

        // `ID3D12CommandList::GetType` + `ID3D12DeviceChild::GetDevice` (slot 7).
        let mut cl_ptr: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_create_command_list(
                dev_ptr.cast(),
                0,
                2, // COMPUTE
                ptr::null_mut(),
                ptr::null_mut(),
                &IID_ID3D12GRAPHICSCOMMANDLIST,
                &mut cl_ptr
            ),
            S_OK
        );
        assert_eq!(cmdlist_get_type(cl_ptr.cast()), 2);
        let mut got_dev: *mut c_void = ptr::null_mut();
        assert_eq!(
            d3d12_child_get_device(cl_ptr.cast(), &IID_ID3D12DEVICE, &mut got_dev),
            S_OK
        );
        assert_eq!(got_dev, dev_ptr);

        // `ID3D12Resource::WriteToSubresource` -> `ReadFromSubresource` + `GetHeapProperties`.
        let tex_desc = D3D12ResourceDesc {
            dimension: 3,
            width: 4,
            height: 4,
            depth_or_array_size: 1,
            mip_levels: 1,
            format: 28,
            sample_desc: DxgiSampleDesc {
                count: 1,
                quality: 0,
            },
            ..Default::default()
        };
        let res = alloc_d3d12_resource(tex_desc, false);
        let src: Vec<u8> = (0u8..64).collect();
        assert_eq!(
            res_write_to_subresource(res, 0, ptr::null(), src.as_ptr(), 16, 64),
            S_OK
        );
        let mut dst = vec![0u8; 64];
        assert_eq!(
            res_read_from_subresource(res, dst.as_mut_ptr(), 16, 64, 0, ptr::null()),
            S_OK
        );
        assert_eq!(dst, src);

        let mut hp = D3D12HeapProperties::default();
        let mut flags = 99u32;
        assert_eq!(res_get_heap_properties(res, &mut hp, &mut flags), S_OK);
        assert_eq!(hp.heap_type, 4);
        assert_eq!(hp.cpu_page_property, 3);
        assert_eq!(hp.memory_pool_preference, 1);
        assert_eq!(flags, 0);
    }

    #[test]
    fn dxgi_adapter_and_output_expose_valid_descs_and_modes() {
        let _serial = crate::d3d12_xs::test_serial();
        let mut dev_ptr: *mut c_void = ptr::null_mut();
        assert_eq!(
            D3D12CreateDevice(ptr::null_mut(), 0xc200, &IID_ID3D12DEVICE, &mut dev_ptr),
            S_OK
        );
        let mut dxgi_dev: *mut c_void = ptr::null_mut();
        assert_eq!(
            device_qi(dev_ptr.cast(), &IID_IDXGIDEVICE, &mut dxgi_dev),
            S_OK
        );
        let mut adapter: *mut c_void = ptr::null_mut();
        assert_eq!(dxgi_device_get_adapter(dxgi_dev.cast(), &mut adapter), S_OK);

        let mut adesc = core::mem::MaybeUninit::<DxgiAdapterDesc>::zeroed();
        assert_eq!(
            dxgi_adapter_get_desc(adapter.cast(), adesc.as_mut_ptr()),
            S_OK
        );
        // SAFETY: initialisé par `dxgi_adapter_get_desc`.
        let adesc = unsafe { adesc.assume_init() };
        assert_eq!(adesc.vendor_id, 0x1002);
        assert!(adesc.dedicated_video_memory >= 8 * 1024 * 1024 * 1024);

        let mut output: *mut c_void = ptr::null_mut();
        assert_eq!(
            dxgi_adapter_enum_outputs(adapter.cast(), 0, &mut output),
            S_OK
        );
        assert!(!output.is_null());
        let mut output2: *mut c_void = ptr::null_mut();
        assert_eq!(
            dxgi_adapter_enum_outputs(adapter.cast(), 1, &mut output2),
            DXGI_ERROR_NOT_FOUND
        );
        assert!(output2.is_null());

        let mut odesc = core::mem::MaybeUninit::<DxgiOutputDesc>::zeroed();
        assert_eq!(
            dxgi_output_get_desc(output.cast(), odesc.as_mut_ptr()),
            S_OK
        );
        // SAFETY: initialisé par `dxgi_output_get_desc`.
        let odesc = unsafe { odesc.assume_init() };
        assert_eq!(odesc.desktop_coordinates, [0, 0, 3840, 2160]);
        assert_eq!(odesc.attached_to_desktop, 1);

        // `GetDisplayModeList` (slot 8) : taille puis remplissage.
        let mut count = 0u32;
        assert_eq!(
            dxgi_output_get_display_mode_list(output.cast(), 28, 0, &mut count, ptr::null_mut()),
            S_OK
        );
        assert_eq!(count, 4);
        let mut modes = vec![
            DxgiModeDesc {
                width: 0,
                height: 0,
                refresh_num: 0,
                refresh_den: 0,
                format: 0,
                scanline_ordering: 0,
                scaling: 0,
            };
            count as usize
        ];
        assert_eq!(
            dxgi_output_get_display_mode_list(output.cast(), 28, 0, &mut count, modes.as_mut_ptr()),
            S_OK
        );
        assert_eq!(
            (modes[0].width, modes[0].height, modes[0].format),
            (3840, 2160, 28)
        );

        // Slot 21 console : 1 mode 4K@60Hz.
        let mut c_count = 0u32;
        assert_eq!(
            dxgi_output_console_slot21(output.cast(), 28, 0, &mut c_count, ptr::null_mut()),
            S_OK
        );
        assert_eq!(c_count, 1);
        let mut c_mode = modes[0];
        c_mode.width = 0;
        assert_eq!(
            dxgi_output_console_slot21(output.cast(), 28, 0, &mut c_count, &mut c_mode),
            S_OK
        );
        assert_eq!((c_mode.width, c_mode.height), (3840, 2160));
    }
}
