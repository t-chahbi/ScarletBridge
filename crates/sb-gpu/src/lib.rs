//! Sous-système graphique, fenêtre/audio/manette SDL3, Vulkan (`ash`), mémoire unifiée
//! (`VK_EXT_external_memory_host`), détiling AMD RDNA 2 (`XG`) et inspection de shaders DXBC/DXIL.

pub mod dxbc_to_spirv;
pub mod dxil;
pub mod rdna2;
pub mod renderer;
pub mod sdl3;
pub mod shader;
pub mod unified_mem;
pub mod vulkan;
pub mod xg_tiling;

pub use ash;
pub use dxbc_to_spirv::{
    ShaderTranslateError, TranslatedShader, build_dxbc_container, sample_compute_transform_cs_dxbc,
    sample_instanced_vs_dxbc, sample_textured_cube_ps_dxbc, sample_textured_cube_vs_dxbc,
    sample_triangle_ps_dxbc, sample_triangle_vs_dxbc, translate_dxbc_to_spirv,
};
pub use dxil::{
    DxilError, DxilProgramInfo, build_sample_dxil_sm60_container, parse_dxil_container,
};
pub use renderer::{
    GraphicsPipelineConfig, RenderCommand, SSBO_BYTE_SIZE, VertexAttrConfig, VertexPosColor,
    VertexPosUvColor, VulkanRenderer, dxgi_format_byte_size, dxgi_format_to_vk,
};
pub use sdl3::{GamepadSnapshot, Sdl3AudioStream, Sdl3Lib, Sdl3Window};
pub use shader::{ShaderContainer, ShaderFormat, ShaderStage};
pub use unified_mem::{UnifiedMemoryManager, UnifiedRegion};
pub use vulkan::{GpuContext, GpuError, SwapchainPresenter};
