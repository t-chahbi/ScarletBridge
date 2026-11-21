//! Moteur de rendu 3D **Vulkan 1.4 (`ash`)** complet pour ScarletBridge (`d3d12_xs` / `d3d11` → Vulkan).
//!
//! Fonctionnalités GPU réelles sur matériel AMD/NVIDIA/Intel :
//! - Cible couleur `RGBA8_UNORM` + **Depth Buffer `D32_SFLOAT`** (avec `resize()` dynamique)
//! - Compilation native **DXBC (SM4/SM5) → SPIR-V 1.3 → `VkPipeline`**
//! - **DescriptorSet 0 unifié** : Constant Buffers `cb0..cb3` (bindings `0..3`),
//!   Textures 2D `t0..t3` (bindings `8..11`) et Samplers `s0..s3` (bindings `16..19`)
//! - **Vertex Buffers**, **Index Buffers (`u16` / `u32`)**, `Draw` & `DrawIndexed`
//! - Inversion Y Direct3D (`height = -height`), readback GPU→CPU (`vkCmdCopyImageToBuffer`),
//!   export PPM et présentation Wayland/SDL3 (`vkCmdBlitImage`).

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;

use ash::vk;
use sb_common::{sb_debug, sb_info, sb_warn};

use crate::dxbc_to_spirv::{
    CBUFFER_VEC4_COUNT, MAX_CBUFFER_SLOTS, MAX_SAMPLER_SLOTS, MAX_STORAGE_SLOTS, MAX_TEXTURE_SLOTS,
    SAMPLER_BINDING_BASE, STORAGE_BINDING_BASE, TEXTURE_BINDING_BASE, translate_dxbc_to_spirv,
};
use crate::shader::ShaderStage;
use crate::vulkan::{GpuContext, GpuError};

const CBUFFER_BYTE_SIZE: u64 = (CBUFFER_VEC4_COUNT as u64) * 16; // 4096 octets (256 × vec4)
/// Taille d'un Storage Buffer / UAV (`u0..u3`) : 256 KiB (16 384 `vec4`).
pub const SSBO_BYTE_SIZE: u64 = 256 * 1024;

/// Description d'un attribut de sommet personnalisé (mappé depuis `D3D12_INPUT_ELEMENT_DESC`).
#[derive(Debug, Clone)]
pub struct VertexAttrConfig {
    pub location: u32,
    pub format: vk::Format,
    pub offset: u32,
}

/// Configuration complète d'un pipeline graphique AAA (InputLayout, Blend, Rasterizer, Depth, Topology).
#[derive(Debug, Clone)]
pub struct GraphicsPipelineConfig {
    pub vertex_stride: u32,
    /// Si vide, dérive automatiquement des attributs `R32G32B32A32_SFLOAT` depuis `ISGN`.
    pub vertex_attrs: Vec<VertexAttrConfig>,
    pub blend_enable: bool,
    pub src_color_blend: vk::BlendFactor,
    pub dst_color_blend: vk::BlendFactor,
    pub color_blend_op: vk::BlendOp,
    pub src_alpha_blend: vk::BlendFactor,
    pub dst_alpha_blend: vk::BlendFactor,
    pub alpha_blend_op: vk::BlendOp,
    pub color_write_mask: vk::ColorComponentFlags,
    pub cull_mode: vk::CullModeFlags,
    pub front_ccw: bool,
    pub wireframe: bool,
    pub depth_test: bool,
    pub depth_write: bool,
    pub depth_compare: vk::CompareOp,
    pub topology: vk::PrimitiveTopology,
}

impl Default for GraphicsPipelineConfig {
    fn default() -> Self {
        Self {
            vertex_stride: 32,
            vertex_attrs: Vec::new(),
            blend_enable: false,
            src_color_blend: vk::BlendFactor::SRC_ALPHA,
            dst_color_blend: vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
            color_blend_op: vk::BlendOp::ADD,
            src_alpha_blend: vk::BlendFactor::ONE,
            dst_alpha_blend: vk::BlendFactor::ZERO,
            alpha_blend_op: vk::BlendOp::ADD,
            color_write_mask: vk::ColorComponentFlags::RGBA,
            cull_mode: vk::CullModeFlags::NONE,
            front_ccw: false,
            wireframe: false,
            depth_test: true,
            depth_write: true,
            depth_compare: vk::CompareOp::LESS_OR_EQUAL,
            topology: vk::PrimitiveTopology::TRIANGLE_LIST,
        }
    }
}

/// Convertit un `DXGI_FORMAT` en `VkFormat` Vulkan (sommets, HDR, textures compressées BC1–BC7).
pub fn dxgi_format_to_vk(dxgi_format: u32) -> vk::Format {
    match dxgi_format {
        2 => vk::Format::R32G32B32A32_SFLOAT,
        6 => vk::Format::R32G32B32_SFLOAT,
        10 => vk::Format::R16G16B16A16_SFLOAT,
        16 => vk::Format::R32G32_SFLOAT,
        28 => vk::Format::R8G8B8A8_UNORM,
        29 => vk::Format::R8G8B8A8_SRGB,
        41 => vk::Format::R32_SFLOAT,
        42 => vk::Format::R32_UINT,
        71 => vk::Format::BC1_RGBA_UNORM_BLOCK,
        72 => vk::Format::BC1_RGBA_SRGB_BLOCK,
        74 => vk::Format::BC2_UNORM_BLOCK,
        77 => vk::Format::BC3_UNORM_BLOCK,
        78 => vk::Format::BC3_SRGB_BLOCK,
        80 => vk::Format::BC4_UNORM_BLOCK,
        83 => vk::Format::BC5_UNORM_BLOCK,
        87 => vk::Format::B8G8R8A8_UNORM,
        91 => vk::Format::B8G8R8A8_SRGB,
        95 => vk::Format::BC6H_UFLOAT_BLOCK,
        98 => vk::Format::BC7_UNORM_BLOCK,
        99 => vk::Format::BC7_SRGB_BLOCK,
        _ => vk::Format::R8G8B8A8_UNORM,
    }
}

/// Taille en octets d'un attribut de sommet pour un `DXGI_FORMAT` courant.
pub fn dxgi_format_byte_size(dxgi_format: u32) -> u32 {
    match dxgi_format {
        2 => 16,      // R32G32B32A32_FLOAT
        6 => 12,      // R32G32B32_FLOAT
        10 | 16 => 8, // R16G16B16A16_FLOAT / R32G32_FLOAT
        28 | 29 | 41 | 42 | 87 | 91 => 4,
        _ => 16,
    }
}

fn is_bc_dxgi_format(dxgi_format: u32) -> bool {
    matches!(dxgi_format, 71 | 72 | 74 | 77 | 78 | 80 | 83 | 95 | 98 | 99)
}

fn texture_byte_size_for_dxgi(width: u32, height: u32, dxgi_format: u32) -> usize {
    let w = width.max(1) as usize;
    let h = height.max(1) as usize;
    match dxgi_format {
        // BC1 / BC4 : 8 octets par bloc 4×4
        71 | 72 | 80 => w.div_ceil(4) * h.div_ceil(4) * 8,
        // BC2 / BC3 / BC5 / BC6H / BC7 : 16 octets par bloc 4×4
        74 | 77 | 78 | 83 | 95 | 98 | 99 => w.div_ceil(4) * h.div_ceil(4) * 16,
        // R16G16B16A16_FLOAT : 8 octets / pixel
        10 => w * h * 8,
        // R32G32B32A32_FLOAT : 16 octets / pixel
        2 => w * h * 16,
        // RGBA8 / BGRA8 : 4 octets / pixel
        _ => w * h * 4,
    }
}

fn pipeline_cache_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("scarletbridge/pipeline_cache.bin"));
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".cache/scarletbridge/pipeline_cache.bin"))
}

/// Format de sommet 3D standard (`float4 position` + `float4 color`, 32 octets).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VertexPosColor {
    pub pos: [f32; 4],
    pub color: [f32; 4],
}

/// Format de sommet 3D texturé (`float4 position` + `float4 uv` + `float4 color`, 48 octets).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VertexPosUvColor {
    pub pos: [f32; 4],
    pub uv: [f32; 4],
    pub color: [f32; 4],
}

/// Texture 2D `RGBA8_UNORM` allouée en VRAM GPU.
pub struct GpuTexture2D {
    gpu: Arc<GpuContext>,
    pub width: u32,
    pub height: u32,
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub view: vk::ImageView,
}

impl Drop for GpuTexture2D {
    fn drop(&mut self) {
        // SAFETY: destruction ordonnée de la texture Vulkan.
        unsafe {
            let _ = self.gpu.device.device_wait_idle();
            self.gpu.device.destroy_image_view(self.view, None);
            self.gpu.device.destroy_image(self.image, None);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}

/// Pipeline graphique Vulkan compilé à partir de bytecode DXBC (VS + PS).
pub struct GraphicsPipeline {
    gpu: Arc<GpuContext>,
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
}

impl Drop for GraphicsPipeline {
    fn drop(&mut self) {
        // SAFETY: destruction ordonnée du pipeline Vulkan.
        unsafe {
            let _ = self.gpu.device.device_wait_idle();
            self.gpu.device.destroy_pipeline(self.pipeline, None);
            self.gpu.device.destroy_pipeline_layout(self.layout, None);
        }
    }
}

/// Pipeline Compute Vulkan compilé à partir d'un Compute Shader DXBC SM5.0.
pub struct ComputePipeline {
    gpu: Arc<GpuContext>,
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
}

impl Drop for ComputePipeline {
    fn drop(&mut self) {
        // SAFETY: destruction ordonnée du pipeline Compute Vulkan.
        unsafe {
            let _ = self.gpu.device.device_wait_idle();
            self.gpu.device.destroy_pipeline(self.pipeline, None);
            self.gpu.device.destroy_pipeline_layout(self.layout, None);
        }
    }
}

/// Commande de rendu ou de calcul GPU enregistrée par `ID3D12GraphicsCommandList` / `ID3D11DeviceContext`.
#[derive(Debug, Clone)]
pub enum RenderCommand {
    Clear {
        rgba: [f32; 4],
    },
    ClearDepth {
        depth: f32,
    },
    SetViewport {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    },
    SetScissor {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
    SetPipeline {
        pipeline_id: usize,
    },
    SetComputePipeline {
        pipeline_id: usize,
    },
    SetVertexBuffer {
        data: Vec<u8>,
        stride: u32,
    },
    SetIndexBuffer {
        data: Vec<u8>,
        index32: bool,
    },
    UpdateConstantBuffer {
        slot: u32,
        data: Vec<u8>,
    },
    UpdateStorageBuffer {
        slot: u32,
        data: Vec<u8>,
    },
    BindTexture {
        slot: u32,
        texture_id: usize,
    },
    Dispatch {
        x: u32,
        y: u32,
        z: u32,
    },
    Draw {
        vertex_count: u32,
        instance_count: u32,
        first_vertex: u32,
        first_instance: u32,
    },
    DrawIndexed {
        index_count: u32,
        instance_count: u32,
        first_index: u32,
        vertex_offset: i32,
        first_instance: u32,
    },
}

/// Renderer Vulkan 3D + Compute complet : cible couleur + depth buffer, descriptors CB/Texture/Sampler/SSBO,
/// `VkPipelineCache` persistant, pipelines graphiques & compute compilés depuis DXBC, readback VRAM→CPU et swapchain.
pub struct VulkanRenderer {
    pub gpu: Arc<GpuContext>,
    pub width: u32,
    pub height: u32,
    color_image: vk::Image,
    color_memory: vk::DeviceMemory,
    color_view: vk::ImageView,
    depth_image: vk::Image,
    depth_memory: vk::DeviceMemory,
    depth_view: vk::ImageView,
    pub render_pass: vk::RenderPass,
    render_pass_load: vk::RenderPass,
    framebuffer: vk::Framebuffer,
    cmd_buf: vk::CommandBuffer,
    fence: vk::Fence,
    pipeline_cache: vk::PipelineCache,
    descriptor_set_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,
    descriptor_set: vk::DescriptorSet,
    cb_buffers: Vec<(vk::Buffer, vk::DeviceMemory)>,
    ssbo_buffers: Vec<(vk::Buffer, vk::DeviceMemory)>,
    default_sampler: vk::Sampler,
    textures: Vec<Option<GpuTexture2D>>,
    bound_textures: [usize; MAX_TEXTURE_SLOTS as usize],
    pipelines: Vec<Option<GraphicsPipeline>>,
    compute_pipelines: Vec<Option<ComputePipeline>>,
    // Optionnel : présentation directe vers un swapchain Wayland/SDL3.
    surface_loader: Option<ash::khr::surface::Instance>,
    swapchain_loader: Option<ash::khr::swapchain::Device>,
    surface: Option<vk::SurfaceKHR>,
    swapchain: Option<vk::SwapchainKHR>,
    swapchain_images: Vec<vk::Image>,
    swapchain_extent: vk::Extent2D,
    img_available: Option<vk::Semaphore>,
    render_finished: Option<vk::Semaphore>,
}

impl VulkanRenderer {
    pub fn new(gpu: Arc<GpuContext>, width: u32, height: u32) -> Result<Self, GpuError> {
        let dev = &gpu.device;

        let (color_image, color_memory, color_view) = create_color_target(&gpu, width, height)?;
        let (depth_image, depth_memory, depth_view) = create_depth_target(&gpu, width, height)?;

        let render_pass = create_render_pass(dev, vk::AttachmentLoadOp::CLEAR)?;
        let render_pass_load = create_render_pass(dev, vk::AttachmentLoadOp::LOAD)?;

        let views = [color_view, depth_view];
        let fb_info = vk::FramebufferCreateInfo::default()
            .render_pass(render_pass)
            .attachments(&views)
            .width(width)
            .height(height)
            .layers(1);
        // SAFETY: création du Framebuffer (couleur + depth).
        let framebuffer = unsafe { dev.create_framebuffer(&fb_info, None) }?;

        let cmd_alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(gpu.command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: allocation du CommandBuffer principal + fence + VkPipelineCache persistant.
        let cmd_buf = unsafe { dev.allocate_command_buffers(&cmd_alloc) }?[0];
        let fence = unsafe { dev.create_fence(&vk::FenceCreateInfo::default(), None) }?;

        let cache_bytes = pipeline_cache_path()
            .and_then(|p| std::fs::read(&p).ok())
            .unwrap_or_default();
        let cache_info = if cache_bytes.is_empty() {
            vk::PipelineCacheCreateInfo::default()
        } else {
            vk::PipelineCacheCreateInfo::default().initial_data(&cache_bytes)
        };
        // SAFETY: création du VkPipelineCache (le driver valide automatiquement les données initiales).
        let pipeline_cache = unsafe { dev.create_pipeline_cache(&cache_info, None) }?;

        // DescriptorSetLayout unifié (Set 0) :
        //   - Bindings 0..4   : Uniform Buffers (`cb0..cb3`)
        //   - Bindings 8..12  : Sampled 2D Images (`t0..t3`)
        //   - Bindings 16..20 : Samplers (`s0..s3`)
        //   - Bindings 24..28 : Storage Buffers / UAV (`u0..u3`)
        let all_stages = vk::ShaderStageFlags::ALL_GRAPHICS | vk::ShaderStageFlags::COMPUTE;
        let mut bindings = Vec::new();
        for slot in 0..MAX_CBUFFER_SLOTS {
            bindings.push(
                vk::DescriptorSetLayoutBinding::default()
                    .binding(slot)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(all_stages),
            );
        }
        for slot in 0..MAX_TEXTURE_SLOTS {
            bindings.push(
                vk::DescriptorSetLayoutBinding::default()
                    .binding(TEXTURE_BINDING_BASE + slot)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .descriptor_count(1)
                    .stage_flags(all_stages),
            );
        }
        for slot in 0..MAX_SAMPLER_SLOTS {
            bindings.push(
                vk::DescriptorSetLayoutBinding::default()
                    .binding(SAMPLER_BINDING_BASE + slot)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(all_stages),
            );
        }
        for slot in 0..MAX_STORAGE_SLOTS {
            bindings.push(
                vk::DescriptorSetLayoutBinding::default()
                    .binding(STORAGE_BINDING_BASE + slot)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(all_stages),
            );
        }
        let dsl_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // SAFETY: création du DescriptorSetLayout.
        let descriptor_set_layout = unsafe { dev.create_descriptor_set_layout(&dsl_info, None) }?;

        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::UNIFORM_BUFFER,
                descriptor_count: MAX_CBUFFER_SLOTS,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::SAMPLED_IMAGE,
                descriptor_count: MAX_TEXTURE_SLOTS,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::SAMPLER,
                descriptor_count: MAX_SAMPLER_SLOTS,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: MAX_STORAGE_SLOTS,
            },
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&pool_sizes);
        // SAFETY: création du DescriptorPool + allocation du DescriptorSet 0.
        let descriptor_pool = unsafe { dev.create_descriptor_pool(&pool_info, None) }?;
        let layouts = [descriptor_set_layout];
        let ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&layouts);
        let descriptor_set = unsafe { dev.allocate_descriptor_sets(&ds_alloc) }?[0];

        // 4 Constant Buffers host-visible initialisés à la matrice identité (16 floats).
        let mut init_cb = vec![0u8; CBUFFER_BYTE_SIZE as usize];
        let ident: [f32; 16] = [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ];
        for (i, &f) in ident.iter().enumerate() {
            init_cb[i * 4..i * 4 + 4].copy_from_slice(&f.to_le_bytes());
        }
        let mut cb_buffers = Vec::with_capacity(MAX_CBUFFER_SLOTS as usize);
        for _ in 0..MAX_CBUFFER_SLOTS {
            cb_buffers.push(create_host_buffer_raw(
                &gpu,
                CBUFFER_BYTE_SIZE,
                vk::BufferUsageFlags::UNIFORM_BUFFER,
                Some(&init_cb),
            )?);
        }

        // 4 Storage Buffers / UAV (`u0..u3`) host-visible cohérents (256 KiB chacun).
        let mut ssbo_buffers = Vec::with_capacity(MAX_STORAGE_SLOTS as usize);
        for _ in 0..MAX_STORAGE_SLOTS {
            ssbo_buffers.push(create_host_buffer_raw(
                &gpu,
                SSBO_BYTE_SIZE,
                vk::BufferUsageFlags::STORAGE_BUFFER
                    | vk::BufferUsageFlags::TRANSFER_SRC
                    | vk::BufferUsageFlags::TRANSFER_DST,
                None,
            )?);
        }

        // Sampler par défaut (filtrage linéaire + repeat).
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
            .address_mode_u(vk::SamplerAddressMode::REPEAT)
            .address_mode_v(vk::SamplerAddressMode::REPEAT)
            .address_mode_w(vk::SamplerAddressMode::REPEAT)
            .max_anisotropy(1.0);
        // SAFETY: création du VkSampler par défaut.
        let default_sampler = unsafe { dev.create_sampler(&sampler_info, None) }?;

        let mut renderer = Self {
            gpu,
            width,
            height,
            color_image,
            color_memory,
            color_view,
            depth_image,
            depth_memory,
            depth_view,
            render_pass,
            render_pass_load,
            framebuffer,
            cmd_buf,
            fence,
            pipeline_cache,
            descriptor_set_layout,
            descriptor_pool,
            descriptor_set,
            cb_buffers,
            ssbo_buffers,
            default_sampler,
            textures: Vec::new(),
            bound_textures: [0; MAX_TEXTURE_SLOTS as usize],
            pipelines: Vec::new(),
            compute_pipelines: Vec::new(),
            surface_loader: None,
            swapchain_loader: None,
            surface: None,
            swapchain: None,
            swapchain_images: Vec::new(),
            swapchain_extent: vk::Extent2D { width, height },
            img_available: None,
            render_finished: None,
        };

        // Texture #0 par défaut : blanc opaque 1×1 (`RGBA 255,255,255,255`).
        let default_tex_id = renderer.create_texture_rgba8(1, 1, &[255, 255, 255, 255])?;
        debug_assert_eq!(default_tex_id, 0);
        renderer.write_all_descriptors();

        sb_info!(
            "renderer",
            "VulkanRenderer 3D prêt ({width}x{height} RGBA8+D32F sur '{}')",
            renderer.gpu.caps.device_name
        );
        Ok(renderer)
    }

    /// Crée et téléverse une texture 2D `RGBA8_UNORM` en VRAM GPU (`SHADER_READ_ONLY_OPTIMAL`).
    pub fn create_texture_rgba8(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<usize, GpuError> {
        self.create_texture_dxgi(width, height, 28, rgba)
    }

    /// Crée et téléverse une texture 2D VRAM au format `DXGI_FORMAT` natif
    /// (`RGBA8`, `BGRA8`, HDR `R16G16B16A16F`/`R32G32B32A32F`, compressé matériel **BC1/BC3/BC7**).
    pub fn create_texture_dxgi(
        &mut self,
        width: u32,
        height: u32,
        dxgi_format: u32,
        data: &[u8],
    ) -> Result<usize, GpuError> {
        let use_bc = is_bc_dxgi_format(dxgi_format) && self.gpu.caps.has_bc_compression;
        let effective_dxgi = if is_bc_dxgi_format(dxgi_format) && !use_bc {
            28 // fallback RGBA8 si GPU sans compression BC
        } else {
            dxgi_format
        };
        let vk_format = dxgi_format_to_vk(effective_dxgi);

        let w = if use_bc { width.max(4) } else { width.max(1) };
        let h = if use_bc { height.max(4) } else { height.max(1) };
        let expected = texture_byte_size_for_dxgi(w, h, effective_dxgi);
        let fill = if effective_dxgi == 28 || effective_dxgi == 87 {
            255u8
        } else {
            0u8
        };
        let mut pixels = vec![fill; expected];
        let copy_len = data.len().min(expected);
        pixels[..copy_len].copy_from_slice(&data[..copy_len]);

        let dev = &self.gpu.device;
        let (staging_buf, staging_mem) = create_host_buffer_raw(
            &self.gpu,
            expected as u64,
            vk::BufferUsageFlags::TRANSFER_SRC,
            Some(&pixels),
        )?;

        let img_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);

        // SAFETY: création de l'image 2D VRAM, copie staging → VRAM et transition `SHADER_READ_ONLY_OPTIMAL`.
        let (image, memory, view) = unsafe {
            let image = dev.create_image(&img_info, None)?;
            let reqs = dev.get_image_memory_requirements(image);
            let mem_type = self
                .gpu
                .find_memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                .or_else(|| {
                    self.gpu
                        .find_memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::empty())
                })
                .ok_or(GpuError::Vk(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY))?;
            let memory = dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(mem_type),
                None,
            )?;
            dev.bind_image_memory(image, memory, 0)?;

            dev.reset_fences(&[self.fence])?;
            dev.reset_command_buffer(self.cmd_buf, vk::CommandBufferResetFlags::empty())?;
            dev.begin_command_buffer(
                self.cmd_buf,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            let range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);
            let to_dst = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .image(image)
                .subresource_range(range)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
            dev.cmd_pipeline_barrier(
                self.cmd_buf,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_dst],
            );

            let region = [vk::BufferImageCopy {
                buffer_offset: 0,
                buffer_row_length: 0,
                buffer_image_height: 0,
                image_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                image_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
                image_extent: vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                },
            }];
            dev.cmd_copy_buffer_to_image(
                self.cmd_buf,
                staging_buf,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &region,
            );

            let to_read = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .image(image)
                .subresource_range(range)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ);
            dev.cmd_pipeline_barrier(
                self.cmd_buf,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER | vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_read],
            );

            dev.end_command_buffer(self.cmd_buf)?;
            let cmd_bufs = [self.cmd_buf];
            let submit = [vk::SubmitInfo::default().command_buffers(&cmd_bufs)];
            dev.queue_submit(self.gpu.graphics_queue, &submit, self.fence)?;
            dev.wait_for_fences(&[self.fence], true, u64::MAX)?;

            dev.destroy_buffer(staging_buf, None);
            dev.free_memory(staging_mem, None);

            let view = dev.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk_format)
                    .subresource_range(range),
                None,
            )?;
            (image, memory, view)
        };

        let id = self.textures.len();
        self.textures.push(Some(GpuTexture2D {
            gpu: Arc::clone(&self.gpu),
            width: w,
            height: h,
            image,
            memory,
            view,
        }));
        sb_debug!(
            "renderer",
            "texture GPU #{id} créée ({w}x{h} {vk_format:?})"
        );
        Ok(id)
    }

    fn write_all_descriptors(&self) {
        let dev = &self.gpu.device;
        let mut buf_infos = Vec::with_capacity(MAX_CBUFFER_SLOTS as usize);
        for &(buf, _) in &self.cb_buffers {
            buf_infos.push(
                vk::DescriptorBufferInfo::default()
                    .buffer(buf)
                    .offset(0)
                    .range(CBUFFER_BYTE_SIZE),
            );
        }
        let mut ssbo_infos = Vec::with_capacity(MAX_STORAGE_SLOTS as usize);
        for &(buf, _) in &self.ssbo_buffers {
            ssbo_infos.push(
                vk::DescriptorBufferInfo::default()
                    .buffer(buf)
                    .offset(0)
                    .range(SSBO_BYTE_SIZE),
            );
        }
        let mut img_infos = Vec::with_capacity(MAX_TEXTURE_SLOTS as usize);
        for slot in 0..MAX_TEXTURE_SLOTS as usize {
            let tid = self.bound_textures[slot];
            let view = self
                .textures
                .get(tid)
                .and_then(|t| t.as_ref())
                .or_else(|| self.textures.first().and_then(|t| t.as_ref()))
                .map(|t| t.view)
                .unwrap_or(vk::ImageView::null());
            img_infos.push(
                vk::DescriptorImageInfo::default()
                    .image_view(view)
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
            );
        }
        let samp_infos = [vk::DescriptorImageInfo::default().sampler(self.default_sampler)];

        let mut writes = Vec::new();
        for (slot, info) in buf_infos.iter().enumerate() {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(self.descriptor_set)
                    .dst_binding(slot as u32)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .buffer_info(std::slice::from_ref(info)),
            );
        }
        for (slot, info) in img_infos.iter().enumerate() {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(self.descriptor_set)
                    .dst_binding(TEXTURE_BINDING_BASE + slot as u32)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(std::slice::from_ref(info)),
            );
        }
        for slot in 0..MAX_SAMPLER_SLOTS {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(self.descriptor_set)
                    .dst_binding(SAMPLER_BINDING_BASE + slot)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .image_info(&samp_infos),
            );
        }
        for (slot, info) in ssbo_infos.iter().enumerate() {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(self.descriptor_set)
                    .dst_binding(STORAGE_BINDING_BASE + slot as u32)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(std::slice::from_ref(info)),
            );
        }
        // SAFETY: `writes` référence des tranches valides pour toute la durée de `update_descriptor_sets`.
        unsafe {
            dev.update_descriptor_sets(&writes, &[]);
        }
    }

    fn update_cb_slot(&self, slot: u32, data: &[u8]) -> Result<(), GpuError> {
        let idx = (slot as usize) % self.cb_buffers.len();
        let mem = self.cb_buffers[idx].1;
        let len = data.len().min(CBUFFER_BYTE_SIZE as usize);
        if len == 0 {
            return Ok(());
        }
        // SAFETY: `mem` est host-visible et coherent (`CBUFFER_BYTE_SIZE` octets).
        unsafe {
            let mapped = self.gpu.device.map_memory(
                mem,
                0,
                CBUFFER_BYTE_SIZE,
                vk::MemoryMapFlags::empty(),
            )?;
            ptr::copy_nonoverlapping(data.as_ptr(), mapped.cast::<u8>(), len);
            self.gpu.device.unmap_memory(mem);
        }
        Ok(())
    }

    /// Met à jour un Storage Buffer / UAV (`u0..u3`, jusqu'à 256 KiB).
    pub fn update_storage_buffer(&self, slot: u32, data: &[u8]) -> Result<(), GpuError> {
        let idx = (slot as usize) % self.ssbo_buffers.len();
        let mem = self.ssbo_buffers[idx].1;
        let len = data.len().min(SSBO_BYTE_SIZE as usize);
        if len == 0 {
            return Ok(());
        }
        // SAFETY: `mem` est host-visible et coherent (`SSBO_BYTE_SIZE` octets).
        unsafe {
            let mapped =
                self.gpu
                    .device
                    .map_memory(mem, 0, SSBO_BYTE_SIZE, vk::MemoryMapFlags::empty())?;
            ptr::copy_nonoverlapping(data.as_ptr(), mapped.cast::<u8>(), len);
            self.gpu.device.unmap_memory(mem);
        }
        Ok(())
    }

    /// Relit les résultats calculés par un Compute Shader depuis un Storage Buffer / UAV (`u0..u3`).
    pub fn readback_storage_buffer(&self, slot: u32, byte_len: usize) -> Result<Vec<u8>, GpuError> {
        let idx = (slot as usize) % self.ssbo_buffers.len();
        let mem = self.ssbo_buffers[idx].1;
        let len = byte_len.min(SSBO_BYTE_SIZE as usize);
        let mut out = vec![0u8; len];
        if len == 0 {
            return Ok(out);
        }
        // SAFETY: `mem` est host-visible et coherent (`SSBO_BYTE_SIZE` octets).
        unsafe {
            let mapped =
                self.gpu
                    .device
                    .map_memory(mem, 0, SSBO_BYTE_SIZE, vk::MemoryMapFlags::empty())?;
            ptr::copy_nonoverlapping(mapped.cast::<u8>(), out.as_mut_ptr(), len);
            self.gpu.device.unmap_memory(mem);
        }
        Ok(out)
    }

    /// Redimensionne les cibles couleur + depth et le framebuffer en conservant les `VkPipeline`.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), GpuError> {
        if width == 0 || height == 0 || (width == self.width && height == self.height) {
            return Ok(());
        }
        let dev = &self.gpu.device;
        // SAFETY: attente idle avant recréation des images et du framebuffer.
        unsafe {
            let _ = dev.device_wait_idle();
            dev.destroy_framebuffer(self.framebuffer, None);
            dev.destroy_image_view(self.color_view, None);
            dev.destroy_image(self.color_image, None);
            dev.free_memory(self.color_memory, None);
            dev.destroy_image_view(self.depth_view, None);
            dev.destroy_image(self.depth_image, None);
            dev.free_memory(self.depth_memory, None);
        }

        let (color_image, color_memory, color_view) =
            create_color_target(&self.gpu, width, height)?;
        let (depth_image, depth_memory, depth_view) =
            create_depth_target(&self.gpu, width, height)?;
        let views = [color_view, depth_view];
        let fb_info = vk::FramebufferCreateInfo::default()
            .render_pass(self.render_pass)
            .attachments(&views)
            .width(width)
            .height(height)
            .layers(1);
        // SAFETY: création du nouveau framebuffer.
        let framebuffer = unsafe { dev.create_framebuffer(&fb_info, None) }?;

        self.width = width;
        self.height = height;
        self.color_image = color_image;
        self.color_memory = color_memory;
        self.color_view = color_view;
        self.depth_image = depth_image;
        self.depth_memory = depth_memory;
        self.depth_view = depth_view;
        self.framebuffer = framebuffer;
        sb_info!(
            "renderer",
            "VulkanRenderer redimensionné à {width}x{height}"
        );
        Ok(())
    }

    /// Attache une surface Wayland/SDL3 pour présenter les images rendues à l'écran par `vkCmdBlitImage`.
    pub fn attach_surface(&mut self, surface: vk::SurfaceKHR) -> Result<(), GpuError> {
        let surface_loader = ash::khr::surface::Instance::new(&self.gpu.entry, &self.gpu.instance);
        let swapchain_loader =
            ash::khr::swapchain::Device::new(&self.gpu.instance, &self.gpu.device);

        // SAFETY: interroge la surface Wayland.
        let caps = unsafe {
            surface_loader
                .get_physical_device_surface_capabilities(self.gpu.physical_device, surface)
        }?;
        let formats = unsafe {
            surface_loader.get_physical_device_surface_formats(self.gpu.physical_device, surface)
        }?;
        let fmt = formats
            .iter()
            .copied()
            .find(|f| f.format == vk::Format::B8G8R8A8_UNORM)
            .or_else(|| formats.first().copied())
            .ok_or(GpuError::NoSurface)?;

        let extent = if caps.current_extent.width != u32::MAX {
            caps.current_extent
        } else {
            vk::Extent2D {
                width: self
                    .width
                    .clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                height: self
                    .height
                    .clamp(caps.min_image_extent.height, caps.max_image_extent.height),
            }
        };

        let image_count = (caps.min_image_count + 1).min(if caps.max_image_count == 0 {
            3
        } else {
            caps.max_image_count
        });

        let sc_info = vk::SwapchainCreateInfoKHR::default()
            .surface(surface)
            .min_image_count(image_count)
            .image_format(fmt.format)
            .image_color_space(fmt.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(vk::PresentModeKHR::FIFO)
            .clipped(true);

        // SAFETY: création du swapchain + sémaphores.
        let swapchain = unsafe { swapchain_loader.create_swapchain(&sc_info, None) }?;
        let images = unsafe { swapchain_loader.get_swapchain_images(swapchain) }?;
        let sem_info = vk::SemaphoreCreateInfo::default();
        let img_available = unsafe { self.gpu.device.create_semaphore(&sem_info, None) }?;
        let render_finished = unsafe { self.gpu.device.create_semaphore(&sem_info, None) }?;

        self.surface_loader = Some(surface_loader);
        self.swapchain_loader = Some(swapchain_loader);
        self.surface = Some(surface);
        self.swapchain = Some(swapchain);
        self.swapchain_images = images;
        self.swapchain_extent = extent;
        self.img_available = Some(img_available);
        self.render_finished = Some(render_finished);
        Ok(())
    }

    /// Traduit une paire de shaders DXBC (VS + PS) en SPIR-V 1.3 et compile un vrai `VkPipeline` GPU !
    pub fn create_pipeline_from_dxbc(
        &mut self,
        vs_dxbc: &[u8],
        ps_dxbc: &[u8],
        vertex_stride: u32,
    ) -> Result<usize, String> {
        let cfg = GraphicsPipelineConfig {
            vertex_stride,
            ..Default::default()
        };
        self.create_pipeline_from_dxbc_with_config(vs_dxbc, ps_dxbc, &cfg)
    }

    /// Traduit VS + PS DXBC en SPIR-V 1.3 et compile un `VkPipeline` AAA complet
    /// (InputLayout personnalisé, Alpha Blending, Culling, Wireframe, Depth, Topology, `VkPipelineCache`).
    pub fn create_pipeline_from_dxbc_with_config(
        &mut self,
        vs_dxbc: &[u8],
        ps_dxbc: &[u8],
        cfg: &GraphicsPipelineConfig,
    ) -> Result<usize, String> {
        let vs = translate_dxbc_to_spirv(vs_dxbc).map_err(|e| format!("VS DXBC->SPIR-V: {e}"))?;
        let ps = translate_dxbc_to_spirv(ps_dxbc).map_err(|e| format!("PS DXBC->SPIR-V: {e}"))?;

        sb_info!(
            "renderer",
            "shaders DXBC -> SPIR-V traduits : VS={} mots ({} in, {} out, cb={:?}), PS={} mots (tex={:?})",
            vs.spirv.len(),
            vs.inputs.len(),
            vs.outputs.len(),
            vs.used_cbuffers,
            ps.spirv.len(),
            ps.used_textures
        );

        let dev = &self.gpu.device;
        let vs_mod_info = vk::ShaderModuleCreateInfo::default().code(&vs.spirv);
        let ps_mod_info = vk::ShaderModuleCreateInfo::default().code(&ps.spirv);

        // SAFETY: `vs.spirv` et `ps.spirv` sont des modules SPIR-V 1.3 valides.
        let vs_module = unsafe { dev.create_shader_module(&vs_mod_info, None) }
            .map_err(|e| format!("vkCreateShaderModule(VS): {e}"))?;
        let ps_module = unsafe { dev.create_shader_module(&ps_mod_info, None) }
            .map_err(|e| format!("vkCreateShaderModule(PS): {e}"))?;

        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vs_module)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(ps_module)
                .name(c"main"),
        ];

        // Attributs sommets : personnalisés (D3D12_INPUT_ELEMENT_DESC) ou dérivés de `ISGN` hors BuiltIns.
        let mut attrs = Vec::new();
        let stride = if !cfg.vertex_attrs.is_empty() {
            let mut max_end = 0u32;
            for a in &cfg.vertex_attrs {
                attrs.push(
                    vk::VertexInputAttributeDescription::default()
                        .location(a.location)
                        .binding(0)
                        .format(a.format)
                        .offset(a.offset),
                );
                let sz = match a.format {
                    vk::Format::R32G32B32A32_SFLOAT => 16,
                    vk::Format::R32G32B32_SFLOAT => 12,
                    vk::Format::R32G32_SFLOAT | vk::Format::R16G16B16A16_SFLOAT => 8,
                    _ => 4,
                };
                max_end = max_end.max(a.offset + sz);
            }
            cfg.vertex_stride.max(max_end).max(4)
        } else {
            let mut seen_regs = std::collections::HashSet::new();
            for inp in &vs.inputs {
                let is_builtin = inp.system_value_type == 8
                    || inp.system_value_type == 6
                    || inp.semantic_name.eq_ignore_ascii_case("SV_INSTANCEID")
                    || inp.semantic_name.eq_ignore_ascii_case("SV_VERTEXID");
                if is_builtin || inp.register == u32::MAX || !seen_regs.insert(inp.register) {
                    continue;
                }
                let loc = attrs.len() as u32;
                attrs.push(
                    vk::VertexInputAttributeDescription::default()
                        .location(loc)
                        .binding(0)
                        .format(vk::Format::R32G32B32A32_SFLOAT)
                        .offset(loc * 16),
                );
            }
            let min_stride = (attrs.len() as u32).max(1) * 16;
            cfg.vertex_stride.max(min_stride)
        };

        let bindings = [vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(stride)
            .input_rate(vk::VertexInputRate::VERTEX)];

        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&bindings)
            .vertex_attribute_descriptions(&attrs);

        let input_assembly =
            vk::PipelineInputAssemblyStateCreateInfo::default().topology(cfg.topology);

        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);

        let poly_mode = if cfg.wireframe && self.gpu.caps.has_fill_mode_non_solid {
            vk::PolygonMode::LINE
        } else {
            vk::PolygonMode::FILL
        };
        let front_face = if cfg.front_ccw {
            vk::FrontFace::COUNTER_CLOCKWISE
        } else {
            vk::FrontFace::CLOCKWISE
        };
        let rasterizer = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(poly_mode)
            .cull_mode(cfg.cull_mode)
            .front_face(front_face)
            .line_width(1.0);

        let multisampling = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);

        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(cfg.depth_test)
            .depth_write_enable(cfg.depth_write)
            .depth_compare_op(cfg.depth_compare);

        let blend_att = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(cfg.color_write_mask)
            .blend_enable(cfg.blend_enable)
            .src_color_blend_factor(cfg.src_color_blend)
            .dst_color_blend_factor(cfg.dst_color_blend)
            .color_blend_op(cfg.color_blend_op)
            .src_alpha_blend_factor(cfg.src_alpha_blend)
            .dst_alpha_blend_factor(cfg.dst_alpha_blend)
            .alpha_blend_op(cfg.alpha_blend_op)];
        let color_blending =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_att);

        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);

        let set_layouts = [self.descriptor_set_layout];
        let layout_info = vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts);
        // SAFETY: création du `VkPipelineLayout` (Set 0 CB+Tex+Sampler+SSBO) et du `VkPipeline` graphique.
        let layout = unsafe { dev.create_pipeline_layout(&layout_info, None) }
            .map_err(|e| format!("vkCreatePipelineLayout: {e}"))?;

        let gp_info = [vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterizer)
            .multisample_state(&multisampling)
            .depth_stencil_state(&depth_stencil)
            .color_blend_state(&color_blending)
            .dynamic_state(&dynamic_state)
            .layout(layout)
            .render_pass(self.render_pass)
            .subpass(0)];

        let pipeline_res =
            unsafe { dev.create_graphics_pipelines(self.pipeline_cache, &gp_info, None) };

        unsafe {
            dev.destroy_shader_module(vs_module, None);
            dev.destroy_shader_module(ps_module, None);
        }

        let pipeline = match pipeline_res {
            Ok(list) => list[0],
            Err((_, err)) => {
                unsafe { dev.destroy_pipeline_layout(layout, None) };
                return Err(format!("vkCreateGraphicsPipelines: {err}"));
            }
        };

        let id = self.pipelines.len();
        self.pipelines.push(Some(GraphicsPipeline {
            gpu: Arc::clone(&self.gpu),
            layout,
            pipeline,
        }));
        sb_info!(
            "renderer",
            "VkPipeline graphique #{id} compilé nativement sur '{}' (blend={}, cull={:?}, stride={stride}o)",
            self.gpu.caps.device_name,
            cfg.blend_enable,
            cfg.cull_mode
        );
        self.save_pipeline_cache();
        Ok(id)
    }

    /// Sauvegarde le `VkPipelineCache` sur disque de façon atomique (anti-stutter au lancement suivant).
    pub fn save_pipeline_cache(&self) {
        let Some(path) = pipeline_cache_path() else {
            return;
        };
        // SAFETY: lecture des octets du `VkPipelineCache` valide.
        if let Ok(bytes) = unsafe { self.gpu.device.get_pipeline_cache_data(self.pipeline_cache) }
            && !bytes.is_empty()
        {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let tmp = path.with_extension("bin.tmp");
            if std::fs::write(&tmp, &bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    /// Traduit un Compute Shader DXBC SM5.0 (`CS`) en SPIR-V 1.3 et compile un `VkPipeline` Compute natif !
    pub fn create_compute_pipeline_from_dxbc(&mut self, cs_dxbc: &[u8]) -> Result<usize, String> {
        let cs = translate_dxbc_to_spirv(cs_dxbc).map_err(|e| format!("CS DXBC->SPIR-V: {e}"))?;
        if cs.stage != ShaderStage::Compute {
            return Err(format!("stage Compute attendu, reçu {:?}", cs.stage));
        }

        let dev = &self.gpu.device;
        let mod_info = vk::ShaderModuleCreateInfo::default().code(&cs.spirv);
        // SAFETY: `cs.spirv` est un module SPIR-V 1.3 GLCompute valide.
        let cs_module = unsafe { dev.create_shader_module(&mod_info, None) }
            .map_err(|e| format!("vkCreateShaderModule(CS): {e}"))?;

        let set_layouts = [self.descriptor_set_layout];
        let layout_info = vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts);
        let layout = unsafe { dev.create_pipeline_layout(&layout_info, None) }
            .map_err(|e| format!("vkCreatePipelineLayout(CS): {e}"))?;

        let stage_info = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(cs_module)
            .name(c"main");
        let cp_info = [vk::ComputePipelineCreateInfo::default()
            .stage(stage_info)
            .layout(layout)];

        let res = unsafe { dev.create_compute_pipelines(self.pipeline_cache, &cp_info, None) };
        unsafe {
            dev.destroy_shader_module(cs_module, None);
        }

        let pipeline = match res {
            Ok(list) => list[0],
            Err((_, err)) => {
                unsafe { dev.destroy_pipeline_layout(layout, None) };
                return Err(format!("vkCreateComputePipelines: {err}"));
            }
        };

        let id = self.compute_pipelines.len();
        self.compute_pipelines.push(Some(ComputePipeline {
            gpu: Arc::clone(&self.gpu),
            layout,
            pipeline,
        }));
        sb_info!(
            "renderer",
            "VkPipeline Compute #{id} compilé nativement sur '{}' (threads={:?}, ssbo={:?}, cb={:?})",
            self.gpu.caps.device_name,
            cs.thread_group_size,
            cs.used_storage_buffers,
            cs.used_cbuffers
        );
        self.save_pipeline_cache();
        Ok(id)
    }

    /// Exécute un Compute Shader sur le GPU Vulkan (`vkCmdDispatch`) avec barrière mémoire `SHADER_WRITE -> HOST/GRAPHICS`.
    pub fn dispatch_compute(
        &mut self,
        pipeline_id: usize,
        x: u32,
        y: u32,
        z: u32,
    ) -> Result<(), GpuError> {
        let Some(Some(cp)) = self.compute_pipelines.get(pipeline_id) else {
            return Ok(());
        };
        let cp_pipeline = cp.pipeline;
        let cp_layout = cp.layout;
        let dev = &self.gpu.device;

        // SAFETY: soumission synchrone d'un dispatch Compute hors render pass + barrière mémoire cohérente.
        unsafe {
            dev.reset_fences(&[self.fence])?;
            dev.reset_command_buffer(self.cmd_buf, vk::CommandBufferResetFlags::empty())?;
            dev.begin_command_buffer(
                self.cmd_buf,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            dev.cmd_bind_pipeline(self.cmd_buf, vk::PipelineBindPoint::COMPUTE, cp_pipeline);
            dev.cmd_bind_descriptor_sets(
                self.cmd_buf,
                vk::PipelineBindPoint::COMPUTE,
                cp_layout,
                0,
                &[self.descriptor_set],
                &[],
            );
            dev.cmd_dispatch(self.cmd_buf, x.max(1), y.max(1), z.max(1));

            let barrier = [vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(
                    vk::AccessFlags::HOST_READ
                        | vk::AccessFlags::SHADER_READ
                        | vk::AccessFlags::VERTEX_ATTRIBUTE_READ,
                )];
            dev.cmd_pipeline_barrier(
                self.cmd_buf,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::HOST
                    | vk::PipelineStageFlags::VERTEX_INPUT
                    | vk::PipelineStageFlags::ALL_GRAPHICS,
                vk::DependencyFlags::empty(),
                &barrier,
                &[],
                &[],
            );
            dev.end_command_buffer(self.cmd_buf)?;

            let cmd_bufs = [self.cmd_buf];
            let submit = [vk::SubmitInfo::default().command_buffers(&cmd_bufs)];
            dev.queue_submit(self.gpu.graphics_queue, &submit, self.fence)?;
            dev.wait_for_fences(&[self.fence], true, u64::MAX)?;
        }
        sb_debug!(
            "renderer",
            "Compute Dispatch({x}, {y}, {z}) exécuté sur pipeline #{pipeline_id}"
        );
        Ok(())
    }

    /// Exécute une liste mixte de commandes Compute + 3D sur le GPU Vulkan.
    pub fn execute_commands(&mut self, commands: &[RenderCommand]) -> Result<(), GpuError> {
        if commands.is_empty() {
            return Ok(());
        }

        let mut active_graphics: Option<usize> = None;
        let mut active_compute: Option<usize> = None;
        let mut ran_graphics_batch = false;
        let mut gfx_start = 0usize;
        let mut saw_draw = false;

        let flush_gfx = |renderer: &mut Self,
                         slice: &[RenderCommand],
                         active_g: &mut Option<usize>,
                         ran_gfx: &mut bool|
         -> Result<(), GpuError> {
            if slice.is_empty() {
                return Ok(());
            }
            let has_render_work = slice.iter().any(|c| {
                matches!(
                    c,
                    RenderCommand::Clear { .. }
                        | RenderCommand::ClearDepth { .. }
                        | RenderCommand::Draw { .. }
                        | RenderCommand::DrawIndexed { .. }
                )
            });
            if has_render_work {
                renderer.execute_batch(slice, !*ran_gfx, active_g)?;
                *ran_gfx = true;
            } else {
                // Pas de render pass nécessaire : applique simplement les mises à jour CB/Texture/PSO.
                let mut desc_dirty = false;
                for cmd in slice {
                    match cmd {
                        RenderCommand::UpdateConstantBuffer { slot, data } => {
                            renderer.update_cb_slot(*slot, data)?;
                        }
                        RenderCommand::BindTexture { slot, texture_id } => {
                            let s = (*slot as usize) % (MAX_TEXTURE_SLOTS as usize);
                            renderer.bound_textures[s] = *texture_id;
                            desc_dirty = true;
                        }
                        RenderCommand::SetPipeline { pipeline_id } => {
                            *active_g = Some(*pipeline_id);
                        }
                        _ => {}
                    }
                }
                if desc_dirty {
                    renderer.write_all_descriptors();
                }
            }
            Ok(())
        };

        for (i, cmd) in commands.iter().enumerate() {
            match cmd {
                RenderCommand::SetComputePipeline { pipeline_id } => {
                    flush_gfx(
                        self,
                        &commands[gfx_start..i],
                        &mut active_graphics,
                        &mut ran_graphics_batch,
                    )?;
                    active_compute = Some(*pipeline_id);
                    gfx_start = i + 1;
                    saw_draw = false;
                }
                RenderCommand::UpdateStorageBuffer { slot, data } => {
                    flush_gfx(
                        self,
                        &commands[gfx_start..i],
                        &mut active_graphics,
                        &mut ran_graphics_batch,
                    )?;
                    self.update_storage_buffer(*slot, data)?;
                    gfx_start = i + 1;
                    saw_draw = false;
                }
                RenderCommand::Dispatch { x, y, z } => {
                    flush_gfx(
                        self,
                        &commands[gfx_start..i],
                        &mut active_graphics,
                        &mut ran_graphics_batch,
                    )?;
                    if let Some(cp_id) = active_compute {
                        self.dispatch_compute(cp_id, *x, *y, *z)?;
                    }
                    gfx_start = i + 1;
                    saw_draw = false;
                }
                RenderCommand::Draw { .. } | RenderCommand::DrawIndexed { .. } => {
                    saw_draw = true;
                }
                RenderCommand::UpdateConstantBuffer { .. } | RenderCommand::BindTexture { .. }
                    if saw_draw =>
                {
                    flush_gfx(
                        self,
                        &commands[gfx_start..i],
                        &mut active_graphics,
                        &mut ran_graphics_batch,
                    )?;
                    gfx_start = i;
                    saw_draw = false;
                }
                _ => {}
            }
        }
        flush_gfx(
            self,
            &commands[gfx_start..],
            &mut active_graphics,
            &mut ran_graphics_batch,
        )?;
        Ok(())
    }

    fn execute_batch(
        &mut self,
        commands: &[RenderCommand],
        clear_targets: bool,
        active_pipeline: &mut Option<usize>,
    ) -> Result<(), GpuError> {
        let mut clear_rgba = [0.0f32, 0.0, 0.0, 1.0];
        let mut clear_depth = 1.0f32;
        let mut descriptors_dirty = false;
        let mut temp_allocs: Vec<(vk::Buffer, vk::DeviceMemory)> = Vec::new();
        let mut vb_by_cmd: Vec<Option<vk::Buffer>> = vec![None; commands.len()];
        let mut ib_by_cmd: Vec<Option<(vk::Buffer, vk::IndexType)>> = vec![None; commands.len()];

        for (i, cmd) in commands.iter().enumerate() {
            match cmd {
                RenderCommand::Clear { rgba } => clear_rgba = *rgba,
                RenderCommand::ClearDepth { depth } => clear_depth = *depth,
                RenderCommand::UpdateConstantBuffer { slot, data } => {
                    self.update_cb_slot(*slot, data)?;
                }
                RenderCommand::BindTexture { slot, texture_id } => {
                    let s = (*slot as usize) % (MAX_TEXTURE_SLOTS as usize);
                    self.bound_textures[s] = *texture_id;
                    descriptors_dirty = true;
                }
                RenderCommand::SetVertexBuffer { data, .. } if !data.is_empty() => {
                    let (buf, mem) = create_host_buffer_raw(
                        &self.gpu,
                        data.len() as u64,
                        vk::BufferUsageFlags::VERTEX_BUFFER,
                        Some(data),
                    )?;
                    temp_allocs.push((buf, mem));
                    vb_by_cmd[i] = Some(buf);
                }
                RenderCommand::SetIndexBuffer { data, index32 } if !data.is_empty() => {
                    let (buf, mem) = create_host_buffer_raw(
                        &self.gpu,
                        data.len() as u64,
                        vk::BufferUsageFlags::INDEX_BUFFER,
                        Some(data),
                    )?;
                    temp_allocs.push((buf, mem));
                    let itype = if *index32 {
                        vk::IndexType::UINT32
                    } else {
                        vk::IndexType::UINT16
                    };
                    ib_by_cmd[i] = Some((buf, itype));
                }
                _ => {}
            }
        }

        if descriptors_dirty {
            self.write_all_descriptors();
        }

        let dev = &self.gpu.device;
        // SAFETY: enregistrement et soumission synchrone du CommandBuffer Vulkan.
        unsafe {
            dev.reset_fences(&[self.fence])?;
            dev.reset_command_buffer(self.cmd_buf, vk::CommandBufferResetFlags::empty())?;
            dev.begin_command_buffer(
                self.cmd_buf,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            let clear_vals = [
                vk::ClearValue {
                    color: vk::ClearColorValue {
                        float32: clear_rgba,
                    },
                },
                vk::ClearValue {
                    depth_stencil: vk::ClearDepthStencilValue {
                        depth: clear_depth,
                        stencil: 0,
                    },
                },
            ];
            let rp = if clear_targets {
                self.render_pass
            } else {
                self.render_pass_load
            };
            let rp_begin = vk::RenderPassBeginInfo::default()
                .render_pass(rp)
                .framebuffer(self.framebuffer)
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: vk::Extent2D {
                        width: self.width,
                        height: self.height,
                    },
                })
                .clear_values(&clear_vals);

            dev.cmd_begin_render_pass(self.cmd_buf, &rp_begin, vk::SubpassContents::INLINE);

            // Viewport Direct3D (+Y en haut via hauteur négative VK_KHR_maintenance1).
            let default_vp = [vk::Viewport {
                x: 0.0,
                y: self.height as f32,
                width: self.width as f32,
                height: -(self.height as f32),
                min_depth: 0.0,
                max_depth: 1.0,
            }];
            let default_sc = [vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: self.width,
                    height: self.height,
                },
            }];
            dev.cmd_set_viewport(self.cmd_buf, 0, &default_vp);
            dev.cmd_set_scissor(self.cmd_buf, 0, &default_sc);

            // Re-lier le pipeline actif si continuation de batch.
            if let Some(pid) = *active_pipeline
                && let Some(Some(gp)) = self.pipelines.get(pid)
            {
                dev.cmd_bind_pipeline(self.cmd_buf, vk::PipelineBindPoint::GRAPHICS, gp.pipeline);
                dev.cmd_bind_descriptor_sets(
                    self.cmd_buf,
                    vk::PipelineBindPoint::GRAPHICS,
                    gp.layout,
                    0,
                    &[self.descriptor_set],
                    &[],
                );
            }

            let mut draw_count = 0u32;
            for (i, cmd) in commands.iter().enumerate() {
                match *cmd {
                    RenderCommand::SetViewport {
                        x,
                        y,
                        width,
                        height,
                    } => {
                        let vp = [vk::Viewport {
                            x,
                            y: y + height,
                            width,
                            height: -height,
                            min_depth: 0.0,
                            max_depth: 1.0,
                        }];
                        dev.cmd_set_viewport(self.cmd_buf, 0, &vp);
                    }
                    RenderCommand::SetScissor {
                        x,
                        y,
                        width,
                        height,
                    } => {
                        let sc = [vk::Rect2D {
                            offset: vk::Offset2D {
                                x: x.max(0),
                                y: y.max(0),
                            },
                            extent: vk::Extent2D {
                                width: width.max(1),
                                height: height.max(1),
                            },
                        }];
                        dev.cmd_set_scissor(self.cmd_buf, 0, &sc);
                    }
                    RenderCommand::SetPipeline { pipeline_id } => {
                        if let Some(Some(gp)) = self.pipelines.get(pipeline_id) {
                            *active_pipeline = Some(pipeline_id);
                            dev.cmd_bind_pipeline(
                                self.cmd_buf,
                                vk::PipelineBindPoint::GRAPHICS,
                                gp.pipeline,
                            );
                            dev.cmd_bind_descriptor_sets(
                                self.cmd_buf,
                                vk::PipelineBindPoint::GRAPHICS,
                                gp.layout,
                                0,
                                &[self.descriptor_set],
                                &[],
                            );
                        }
                    }
                    RenderCommand::SetVertexBuffer { .. } => {
                        if let Some(buf) = vb_by_cmd[i] {
                            dev.cmd_bind_vertex_buffers(self.cmd_buf, 0, &[buf], &[0]);
                        }
                    }
                    RenderCommand::SetIndexBuffer { .. } => {
                        if let Some((buf, itype)) = ib_by_cmd[i] {
                            dev.cmd_bind_index_buffer(self.cmd_buf, buf, 0, itype);
                        }
                    }
                    RenderCommand::Draw {
                        vertex_count,
                        instance_count,
                        first_vertex,
                        first_instance,
                    } => {
                        dev.cmd_draw(
                            self.cmd_buf,
                            vertex_count,
                            instance_count.max(1),
                            first_vertex,
                            first_instance,
                        );
                        draw_count += 1;
                    }
                    RenderCommand::DrawIndexed {
                        index_count,
                        instance_count,
                        first_index,
                        vertex_offset,
                        first_instance,
                    } => {
                        dev.cmd_draw_indexed(
                            self.cmd_buf,
                            index_count,
                            instance_count.max(1),
                            first_index,
                            vertex_offset,
                            first_instance,
                        );
                        draw_count += 1;
                    }
                    _ => {}
                }
            }

            dev.cmd_end_render_pass(self.cmd_buf);
            dev.end_command_buffer(self.cmd_buf)?;

            let cmd_bufs = [self.cmd_buf];
            let submit = [vk::SubmitInfo::default().command_buffers(&cmd_bufs)];
            dev.queue_submit(self.gpu.graphics_queue, &submit, self.fence)?;
            dev.wait_for_fences(&[self.fence], true, u64::MAX)?;

            for (buf, mem) in temp_allocs {
                dev.destroy_buffer(buf, None);
                dev.free_memory(mem, None);
            }

            sb_debug!(
                "renderer",
                "exécution GPU Vulkan terminée ({draw_count} draw call(s), clear={clear_rgba:?})"
            );
        }

        Ok(())
    }

    /// Blit l'image rendue vers le swapchain Wayland/SDL3 si attaché.
    pub fn present_to_surface(&mut self) -> Result<(), GpuError> {
        let (Some(sc_loader), Some(swapchain), Some(img_avail), Some(ren_fin)) = (
            self.swapchain_loader.as_ref(),
            self.swapchain,
            self.img_available,
            self.render_finished,
        ) else {
            return Ok(());
        };

        let dev = &self.gpu.device;
        // SAFETY: acquisition swapchain + `vkCmdBlitImage` + `queue_present`.
        unsafe {
            let (idx, _) =
                sc_loader.acquire_next_image(swapchain, u64::MAX, img_avail, vk::Fence::null())?;
            let dst_img = self.swapchain_images[idx as usize];

            dev.reset_fences(&[self.fence])?;
            dev.reset_command_buffer(self.cmd_buf, vk::CommandBufferResetFlags::empty())?;
            dev.begin_command_buffer(
                self.cmd_buf,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            let range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);

            let dst_to_transfer = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .image(dst_img)
                .subresource_range(range)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
            dev.cmd_pipeline_barrier(
                self.cmd_buf,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[dst_to_transfer],
            );

            let blit = [vk::ImageBlit {
                src_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                src_offsets: [
                    vk::Offset3D { x: 0, y: 0, z: 0 },
                    vk::Offset3D {
                        x: self.width as i32,
                        y: self.height as i32,
                        z: 1,
                    },
                ],
                dst_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                dst_offsets: [
                    vk::Offset3D { x: 0, y: 0, z: 0 },
                    vk::Offset3D {
                        x: self.swapchain_extent.width as i32,
                        y: self.swapchain_extent.height as i32,
                        z: 1,
                    },
                ],
            }];
            dev.cmd_blit_image(
                self.cmd_buf,
                self.color_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                dst_img,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &blit,
                vk::Filter::LINEAR,
            );

            let dst_to_present = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .image(dst_img)
                .subresource_range(range)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE);
            dev.cmd_pipeline_barrier(
                self.cmd_buf,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[dst_to_present],
            );

            dev.end_command_buffer(self.cmd_buf)?;

            let wait_sems = [img_avail];
            let wait_stages = [vk::PipelineStageFlags::TRANSFER];
            let cmd_bufs = [self.cmd_buf];
            let sig_sems = [ren_fin];
            let submit = [vk::SubmitInfo::default()
                .wait_semaphores(&wait_sems)
                .wait_dst_stage_mask(&wait_stages)
                .command_buffers(&cmd_bufs)
                .signal_semaphores(&sig_sems)];
            dev.queue_submit(self.gpu.graphics_queue, &submit, self.fence)?;
            dev.wait_for_fences(&[self.fence], true, u64::MAX)?;

            let swapchains = [swapchain];
            let indices = [idx];
            let present = vk::PresentInfoKHR::default()
                .wait_semaphores(&sig_sems)
                .swapchains(&swapchains)
                .image_indices(&indices);
            if let Err(err) = sc_loader.queue_present(self.gpu.graphics_queue, &present) {
                sb_warn!("renderer", "queue_present: {err}");
            }
        }
        Ok(())
    }

    /// Relit les pixels RGBA8 réellement rendus par le GPU (`vkCmdCopyImageToBuffer`).
    pub fn readback_rgba(&self) -> Result<Vec<u8>, GpuError> {
        let dev = &self.gpu.device;
        let byte_len = (self.width as usize) * (self.height as usize) * 4;
        let (staging_buf, staging_mem) = create_host_buffer_raw(
            &self.gpu,
            byte_len as u64,
            vk::BufferUsageFlags::TRANSFER_DST,
            None,
        )?;

        // SAFETY: copie GPU `color_image` (en `TRANSFER_SRC_OPTIMAL`) → `staging_buf`.
        unsafe {
            dev.reset_fences(&[self.fence])?;
            dev.reset_command_buffer(self.cmd_buf, vk::CommandBufferResetFlags::empty())?;
            dev.begin_command_buffer(
                self.cmd_buf,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            let region = [vk::BufferImageCopy {
                buffer_offset: 0,
                buffer_row_length: 0,
                buffer_image_height: 0,
                image_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                image_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
                image_extent: vk::Extent3D {
                    width: self.width,
                    height: self.height,
                    depth: 1,
                },
            }];
            dev.cmd_copy_image_to_buffer(
                self.cmd_buf,
                self.color_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                staging_buf,
                &region,
            );
            dev.end_command_buffer(self.cmd_buf)?;

            let cmd_bufs = [self.cmd_buf];
            let submit = [vk::SubmitInfo::default().command_buffers(&cmd_bufs)];
            dev.queue_submit(self.gpu.graphics_queue, &submit, self.fence)?;
            dev.wait_for_fences(&[self.fence], true, u64::MAX)?;

            let mapped =
                dev.map_memory(staging_mem, 0, byte_len as u64, vk::MemoryMapFlags::empty())?;
            let mut pixels = vec![0u8; byte_len];
            ptr::copy_nonoverlapping(mapped.cast::<u8>(), pixels.as_mut_ptr(), byte_len);
            dev.unmap_memory(staging_mem);
            dev.destroy_buffer(staging_buf, None);
            dev.free_memory(staging_mem, None);

            Ok(pixels)
        }
    }

    /// Sauvegarde le rendu GPU courant au format PPM (P6) pour inspection visuelle rapide.
    pub fn save_ppm(&self, path: &Path) -> Result<(), String> {
        let rgba = self.readback_rgba().map_err(|e| e.to_string())?;
        let mut f = File::create(path).map_err(|e| e.to_string())?;
        writeln!(f, "P6\n{} {}\n255", self.width, self.height).map_err(|e| e.to_string())?;
        let mut rgb = Vec::with_capacity((self.width * self.height * 3) as usize);
        for px in rgba.as_chunks::<4>().0 {
            rgb.push(px[0]);
            rgb.push(px[1]);
            rgb.push(px[2]);
        }
        f.write_all(&rgb).map_err(|e| e.to_string())
    }
}

impl Drop for VulkanRenderer {
    fn drop(&mut self) {
        let dev = &self.gpu.device;
        // SAFETY: destruction ordonnée de toutes les ressources Vulkan.
        unsafe {
            let _ = dev.device_wait_idle();
            self.save_pipeline_cache();
            self.compute_pipelines.clear();
            self.pipelines.clear();
            self.textures.clear();
            dev.destroy_pipeline_cache(self.pipeline_cache, None);
            if let Some(s) = self.img_available.take() {
                dev.destroy_semaphore(s, None);
            }
            if let Some(s) = self.render_finished.take() {
                dev.destroy_semaphore(s, None);
            }
            if let (Some(sc_loader), Some(sc)) =
                (self.swapchain_loader.as_ref(), self.swapchain.take())
            {
                sc_loader.destroy_swapchain(sc, None);
            }
            if let (Some(sf_loader), Some(sf)) = (self.surface_loader.as_ref(), self.surface.take())
            {
                sf_loader.destroy_surface(sf, None);
            }
            for (buf, mem) in self.cb_buffers.drain(..) {
                dev.destroy_buffer(buf, None);
                dev.free_memory(mem, None);
            }
            for (buf, mem) in self.ssbo_buffers.drain(..) {
                dev.destroy_buffer(buf, None);
                dev.free_memory(mem, None);
            }
            dev.destroy_sampler(self.default_sampler, None);
            dev.destroy_descriptor_pool(self.descriptor_pool, None);
            dev.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            dev.destroy_fence(self.fence, None);
            dev.destroy_framebuffer(self.framebuffer, None);
            dev.destroy_render_pass(self.render_pass, None);
            dev.destroy_render_pass(self.render_pass_load, None);
            dev.destroy_image_view(self.color_view, None);
            dev.destroy_image(self.color_image, None);
            dev.free_memory(self.color_memory, None);
            dev.destroy_image_view(self.depth_view, None);
            dev.destroy_image(self.depth_image, None);
            dev.free_memory(self.depth_memory, None);
        }
    }
}

// ==================== Helpers Vulkan internes ====================

fn create_color_target(
    gpu: &GpuContext,
    width: u32,
    height: u32,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView), GpuError> {
    let dev = &gpu.device;
    let img_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::R8G8B8A8_UNORM)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    // SAFETY: allocation cible couleur GPU.
    unsafe {
        let image = dev.create_image(&img_info, None)?;
        let reqs = dev.get_image_memory_requirements(image);
        let mem_type = gpu
            .find_memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
            .or_else(|| {
                gpu.find_memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::empty())
            })
            .ok_or(GpuError::Vk(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY))?;
        let memory = dev.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size)
                .memory_type_index(mem_type),
            None,
        )?;
        dev.bind_image_memory(image, memory, 0)?;
        let view = dev.create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(vk::Format::R8G8B8A8_UNORM)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                ),
            None,
        )?;
        Ok((image, memory, view))
    }
}

fn create_depth_target(
    gpu: &GpuContext,
    width: u32,
    height: u32,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView), GpuError> {
    let dev = &gpu.device;
    let img_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::D32_SFLOAT)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    // SAFETY: allocation cible depth `D32_SFLOAT` GPU.
    unsafe {
        let image = dev.create_image(&img_info, None)?;
        let reqs = dev.get_image_memory_requirements(image);
        let mem_type = gpu
            .find_memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
            .or_else(|| {
                gpu.find_memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::empty())
            })
            .ok_or(GpuError::Vk(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY))?;
        let memory = dev.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size)
                .memory_type_index(mem_type),
            None,
        )?;
        dev.bind_image_memory(image, memory, 0)?;
        let view = dev.create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(vk::Format::D32_SFLOAT)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::DEPTH)
                        .level_count(1)
                        .layer_count(1),
                ),
            None,
        )?;
        Ok((image, memory, view))
    }
}

fn create_render_pass(
    dev: &ash::Device,
    load_op: vk::AttachmentLoadOp,
) -> Result<vk::RenderPass, GpuError> {
    let init_color = if load_op == vk::AttachmentLoadOp::CLEAR {
        vk::ImageLayout::UNDEFINED
    } else {
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL
    };
    let init_depth = if load_op == vk::AttachmentLoadOp::CLEAR {
        vk::ImageLayout::UNDEFINED
    } else {
        vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL
    };
    let attachments = [
        vk::AttachmentDescription::default()
            .format(vk::Format::R8G8B8A8_UNORM)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(load_op)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(init_color)
            .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL),
        vk::AttachmentDescription::default()
            .format(vk::Format::D32_SFLOAT)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(load_op)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(init_depth)
            .final_layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL),
    ];
    let color_ref = [vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
    let depth_ref = vk::AttachmentReference::default()
        .attachment(1)
        .layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL);
    let subpass = [vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_ref)
        .depth_stencil_attachment(&depth_ref)];
    let rp_info = vk::RenderPassCreateInfo::default()
        .attachments(&attachments)
        .subpasses(&subpass);
    // SAFETY: création du RenderPass couleur + depth.
    Ok(unsafe { dev.create_render_pass(&rp_info, None) }?)
}

fn create_host_buffer_raw(
    gpu: &GpuContext,
    size: u64,
    usage: vk::BufferUsageFlags,
    initial_data: Option<&[u8]>,
) -> Result<(vk::Buffer, vk::DeviceMemory), GpuError> {
    let dev = &gpu.device;
    let buf_info = vk::BufferCreateInfo::default()
        .size(size.max(16))
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: création buffer host-visible + copie optionnelle.
    unsafe {
        let buf = dev.create_buffer(&buf_info, None)?;
        let reqs = dev.get_buffer_memory_requirements(buf);
        let flags = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let mem_type = gpu
            .find_memory_type(reqs.memory_type_bits, flags)
            .ok_or(GpuError::Vk(vk::Result::ERROR_OUT_OF_HOST_MEMORY))?;
        let mem = dev.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size)
                .memory_type_index(mem_type),
            None,
        )?;
        dev.bind_buffer_memory(buf, mem, 0)?;
        if let Some(data) = initial_data {
            let mapped = dev.map_memory(mem, 0, size, vk::MemoryMapFlags::empty())?;
            ptr::copy_nonoverlapping(data.as_ptr(), mapped.cast::<u8>(), data.len());
            dev.unmap_memory(mem);
        }
        Ok((buf, mem))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dxbc_to_spirv::{
        sample_compute_transform_cs_dxbc, sample_instanced_vs_dxbc, sample_textured_cube_ps_dxbc,
        sample_textured_cube_vs_dxbc, sample_triangle_ps_dxbc, sample_triangle_vs_dxbc,
    };

    #[test]
    fn renders_rgb_triangle_on_real_vulkan_gpu_from_dxbc_shaders() {
        let Ok(gpu) = GpuContext::new(false) else {
            return;
        };
        let mut renderer = VulkanRenderer::new(gpu, 256, 256).expect("init VulkanRenderer");

        let vs = sample_triangle_vs_dxbc();
        let ps = sample_triangle_ps_dxbc();
        let pid = renderer
            .create_pipeline_from_dxbc(&vs, &ps, std::mem::size_of::<VertexPosColor>() as u32)
            .expect("compilation pipeline DXBC -> SPIR-V -> Vulkan");

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
        // SAFETY: `verts` est `#[repr(C)]` POD.
        let vb_bytes = unsafe {
            std::slice::from_raw_parts(verts.as_ptr().cast::<u8>(), std::mem::size_of_val(&verts))
                .to_vec()
        };

        renderer
            .execute_commands(&[
                RenderCommand::Clear {
                    rgba: [0.05, 0.05, 0.2, 1.0],
                },
                RenderCommand::SetPipeline { pipeline_id: pid },
                RenderCommand::SetVertexBuffer {
                    data: vb_bytes,
                    stride: std::mem::size_of::<VertexPosColor>() as u32,
                },
                RenderCommand::Draw {
                    vertex_count: 3,
                    instance_count: 1,
                    first_vertex: 0,
                    first_instance: 0,
                },
            ])
            .expect("exécution commandes Vulkan");

        let rgba = renderer.readback_rgba().expect("readback GPU");
        let px = |x: usize, y: usize| -> [u8; 4] {
            let i = (y * 256 + x) * 4;
            [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
        };

        let bg = px(8, 8);
        assert!(bg[2] > 40 && bg[0] < 25, "fond attendu: {bg:?}");
        let top = px(128, 48);
        assert!(top[0] > 180 && top[1] < 60, "sommet haut rouge: {top:?}");
        let br = px(204, 204);
        assert!(br[1] > 180 && br[0] < 60, "bas-droit vert: {br:?}");
        let bl = px(52, 204);
        assert!(bl[2] > 180 && bl[0] < 60, "bas-gauche bleu: {bl:?}");
    }

    #[test]
    fn renders_textured_3d_quads_with_cbuffer_index_buffer_and_depth_occlusion() {
        let Ok(gpu) = GpuContext::new(false) else {
            return;
        };
        let mut renderer = VulkanRenderer::new(gpu, 128, 128).expect("init VulkanRenderer");

        let vs = sample_textured_cube_vs_dxbc();
        let ps = sample_textured_cube_ps_dxbc();
        let pid = renderer
            .create_pipeline_from_dxbc(&vs, &ps, std::mem::size_of::<VertexPosUvColor>() as u32)
            .expect("pipeline 3D texturé");

        // Texture damier 2x2 : (0,0)=Blanc, (1,0)=Rouge, (0,1)=Vert, (1,1)=Bleu.
        let tex_pixels: [u8; 16] = [
            255, 255, 255, 255, 255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255,
        ];
        let tex_id = renderer
            .create_texture_rgba8(2, 2, &tex_pixels)
            .expect("upload texture 2x2");

        // Deux quads empilés en profondeur :
        // - Quad proche (z = 0.2, teinte blanche) dessiné EN PREMIER
        // - Quad lointain (z = 0.8, teinte noire) dessiné EN SECOND (doit être rejeté par le depth buffer !)
        let verts = [
            // Quad proche (z=0.2)
            VertexPosUvColor {
                pos: [-0.8, 0.8, 0.2, 1.0],
                uv: [0.25, 0.25, 0.0, 0.0],
                color: [1.0, 1.0, 1.0, 1.0],
            },
            VertexPosUvColor {
                pos: [0.8, 0.8, 0.2, 1.0],
                uv: [0.75, 0.25, 0.0, 0.0],
                color: [1.0, 1.0, 1.0, 1.0],
            },
            VertexPosUvColor {
                pos: [0.8, -0.8, 0.2, 1.0],
                uv: [0.75, 0.75, 0.0, 0.0],
                color: [1.0, 1.0, 1.0, 1.0],
            },
            VertexPosUvColor {
                pos: [-0.8, -0.8, 0.2, 1.0],
                uv: [0.25, 0.75, 0.0, 0.0],
                color: [1.0, 1.0, 1.0, 1.0],
            },
            // Quad lointain (z=0.8) noir — doit être occulté par le Z-buffer !
            VertexPosUvColor {
                pos: [-0.9, 0.9, 0.8, 1.0],
                uv: [0.25, 0.25, 0.0, 0.0],
                color: [0.0, 0.0, 0.0, 1.0],
            },
            VertexPosUvColor {
                pos: [0.9, 0.9, 0.8, 1.0],
                uv: [0.25, 0.25, 0.0, 0.0],
                color: [0.0, 0.0, 0.0, 1.0],
            },
            VertexPosUvColor {
                pos: [0.9, -0.9, 0.8, 1.0],
                uv: [0.25, 0.25, 0.0, 0.0],
                color: [0.0, 0.0, 0.0, 1.0],
            },
            VertexPosUvColor {
                pos: [-0.9, -0.9, 0.8, 1.0],
                uv: [0.25, 0.25, 0.0, 0.0],
                color: [0.0, 0.0, 0.0, 1.0],
            },
        ];
        let indices: [u16; 12] = [0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7];
        // Matrice identité dans cb0[0..4].
        let mvp: [f32; 16] = [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ];
        // SAFETY: slices POD.
        let vb = unsafe {
            std::slice::from_raw_parts(verts.as_ptr().cast::<u8>(), std::mem::size_of_val(&verts))
                .to_vec()
        };
        let ib = unsafe {
            std::slice::from_raw_parts(
                indices.as_ptr().cast::<u8>(),
                std::mem::size_of_val(&indices),
            )
            .to_vec()
        };
        let cb = unsafe {
            std::slice::from_raw_parts(mvp.as_ptr().cast::<u8>(), std::mem::size_of_val(&mvp))
                .to_vec()
        };

        renderer
            .execute_commands(&[
                RenderCommand::Clear {
                    rgba: [0.0, 0.0, 0.0, 1.0],
                },
                RenderCommand::ClearDepth { depth: 1.0 },
                RenderCommand::UpdateConstantBuffer { slot: 0, data: cb },
                RenderCommand::BindTexture {
                    slot: 0,
                    texture_id: tex_id,
                },
                RenderCommand::SetPipeline { pipeline_id: pid },
                RenderCommand::SetVertexBuffer {
                    data: vb,
                    stride: std::mem::size_of::<VertexPosUvColor>() as u32,
                },
                RenderCommand::SetIndexBuffer {
                    data: ib,
                    index32: false,
                },
                RenderCommand::DrawIndexed {
                    index_count: 12,
                    instance_count: 1,
                    first_index: 0,
                    vertex_offset: 0,
                    first_instance: 0,
                },
            ])
            .expect("rendu 3D indexé + texture + depth");

        let rgba = renderer.readback_rgba().expect("readback GPU");
        let px = |x: usize, y: usize| -> [u8; 4] {
            let i = (y * 128 + x) * 4;
            [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
        };
        // Haut-gauche du quad proche (texel 0,0 = blanc) : NON écrasé par le quad lointain noir !
        let tl = px(20, 20);
        assert!(
            tl[0] > 200 && tl[1] > 200 && tl[2] > 200,
            "depth buffer ou texture haut-gauche incorrect: {tl:?}"
        );
        // Haut-droit (texel 1,0 = rouge)
        let tr = px(108, 20);
        assert!(
            tr[0] > 200 && tr[1] < 50 && tr[2] < 50,
            "texel rouge attendu: {tr:?}"
        );
        // Bas-gauche (texel 0,1 = vert)
        let bl = px(20, 108);
        assert!(
            bl[1] > 200 && bl[0] < 50 && bl[2] < 50,
            "texel vert attendu: {bl:?}"
        );
        // Bas-droit (texel 1,1 = bleu)
        let br = px(108, 108);
        assert!(
            br[2] > 200 && br[0] < 50 && br[1] < 50,
            "texel bleu attendu: {br:?}"
        );
    }

    #[test]
    fn executes_compute_shader_on_real_vulkan_gpu_and_verifies_ssbo_results() {
        let Ok(gpu) = GpuContext::new(false) else {
            return;
        };
        let mut renderer = VulkanRenderer::new(gpu, 64, 64).expect("init VulkanRenderer");

        let cs = sample_compute_transform_cs_dxbc();
        let cpid = renderer
            .create_compute_pipeline_from_dxbc(&cs)
            .expect("compilation Compute Shader DXBC -> SPIR-V -> Vulkan");

        // 256 éléments vec4 (4 groupes × 64 threads = 256 invocations GPU).
        let count = 256usize;
        let mut input_bytes = Vec::with_capacity(count * 16);
        for i in 0..count {
            let base = i as f32;
            for f in [base, base + 0.25, base + 0.5, 1.0] {
                input_bytes.extend_from_slice(&f.to_le_bytes());
            }
        }
        // cb0[0] = scale (2, 3, 4, 5), cb0[1] = bias (10, 20, 30, 40)
        let cb_floats: [f32; 8] = [2.0, 3.0, 4.0, 5.0, 10.0, 20.0, 30.0, 40.0];
        let mut cb_bytes = Vec::with_capacity(32);
        for f in cb_floats {
            cb_bytes.extend_from_slice(&f.to_le_bytes());
        }

        renderer
            .execute_commands(&[
                RenderCommand::UpdateConstantBuffer {
                    slot: 0,
                    data: cb_bytes,
                },
                RenderCommand::UpdateStorageBuffer {
                    slot: 0,
                    data: input_bytes,
                },
                RenderCommand::SetComputePipeline { pipeline_id: cpid },
                RenderCommand::Dispatch { x: 4, y: 1, z: 1 },
            ])
            .expect("exécution Compute Dispatch GPU");

        let out_bytes = renderer
            .readback_storage_buffer(1, count * 16)
            .expect("readback SSBO u1");
        assert_eq!(out_bytes.len(), count * 16);

        for i in 0..count {
            let off = i * 16;
            let got = [
                f32::from_le_bytes(out_bytes[off..off + 4].try_into().expect("f32")),
                f32::from_le_bytes(out_bytes[off + 4..off + 8].try_into().expect("f32")),
                f32::from_le_bytes(out_bytes[off + 8..off + 12].try_into().expect("f32")),
                f32::from_le_bytes(out_bytes[off + 12..off + 16].try_into().expect("f32")),
            ];
            let base = i as f32;
            let expected = [
                base * 2.0 + 10.0,
                (base + 0.25) * 3.0 + 20.0,
                (base + 0.5) * 4.0 + 30.0,
                1.0 * 5.0 + 40.0,
            ];
            for c in 0..4 {
                assert!(
                    (got[c] - expected[c]).abs() < 1e-3,
                    "mismatch Compute u1[{i}].{c}: got={} expected={}",
                    got[c],
                    expected[c]
                );
            }
        }
    }

    #[test]
    fn renders_bc1_compressed_texture_alpha_blending_scissor_and_instancing() {
        let Ok(gpu) = GpuContext::new(false) else {
            return;
        };
        if !gpu.caps.has_bc_compression {
            return;
        }
        let mut renderer = VulkanRenderer::new(gpu, 128, 128).expect("init VulkanRenderer");

        let vs = sample_instanced_vs_dxbc();
        let ps = sample_textured_cube_ps_dxbc();
        let cfg = GraphicsPipelineConfig {
            vertex_stride: std::mem::size_of::<VertexPosUvColor>() as u32,
            blend_enable: true,
            src_color_blend: vk::BlendFactor::SRC_ALPHA,
            dst_color_blend: vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
            ..Default::default()
        };
        let pid = renderer
            .create_pipeline_from_dxbc_with_config(&vs, &ps, &cfg)
            .expect("pipeline instancié + alpha blend");

        // Bloc 4×4 compressé BC1 matériel (DXGI_FORMAT_BC1_UNORM = 71) : vert pur RGB565 (0x07E0).
        let bc1_green: [u8; 8] = [0xE0, 0x07, 0xE0, 0x07, 0x00, 0x00, 0x00, 0x00];
        let tex_id = renderer
            .create_texture_dxgi(4, 4, 71, &bc1_green)
            .expect("upload texture BC1 matérielle");

        // Quad centré [-0.35..0.35] avec alpha = 0.5 (instance 0 décalée à -0.5, instance 1 à +0.5).
        let verts = [
            VertexPosUvColor {
                pos: [-0.35, 0.4, 0.2, 1.0],
                uv: [0.25, 0.25, 0.0, 0.0],
                color: [1.0, 1.0, 1.0, 0.5],
            },
            VertexPosUvColor {
                pos: [0.35, 0.4, 0.2, 1.0],
                uv: [0.75, 0.25, 0.0, 0.0],
                color: [1.0, 1.0, 1.0, 0.5],
            },
            VertexPosUvColor {
                pos: [0.35, -0.4, 0.2, 1.0],
                uv: [0.75, 0.75, 0.0, 0.0],
                color: [1.0, 1.0, 1.0, 0.5],
            },
            VertexPosUvColor {
                pos: [-0.35, -0.4, 0.2, 1.0],
                uv: [0.25, 0.75, 0.0, 0.0],
                color: [1.0, 1.0, 1.0, 0.5],
            },
        ];
        let indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
        // cb0[0..4] = identité, cb0[4] = offset inst 0 (-0.5, 0), cb0[5] = offset inst 1 (+0.5, 0).
        let cb_floats: [f32; 24] = [
            1.0, 0.0, 0.0, 0.0, // row 0
            0.0, 1.0, 0.0, 0.0, // row 1
            0.0, 0.0, 1.0, 0.0, // row 2
            0.0, 0.0, 0.0, 1.0, // row 3
            -0.5, 0.0, 0.0, 0.0, // inst 0
            0.5, 0.0, 0.0, 0.0, // inst 1
        ];
        // SAFETY: slices POD.
        let vb = unsafe {
            std::slice::from_raw_parts(verts.as_ptr().cast::<u8>(), std::mem::size_of_val(&verts))
                .to_vec()
        };
        let ib = unsafe {
            std::slice::from_raw_parts(
                indices.as_ptr().cast::<u8>(),
                std::mem::size_of_val(&indices),
            )
            .to_vec()
        };
        let cb = unsafe {
            std::slice::from_raw_parts(
                cb_floats.as_ptr().cast::<u8>(),
                std::mem::size_of_val(&cb_floats),
            )
            .to_vec()
        };

        renderer
            .execute_commands(&[
                RenderCommand::Clear {
                    rgba: [1.0, 0.0, 0.0, 1.0],
                },
                RenderCommand::ClearDepth { depth: 1.0 },
                RenderCommand::UpdateConstantBuffer { slot: 0, data: cb },
                RenderCommand::BindTexture {
                    slot: 0,
                    texture_id: tex_id,
                },
                RenderCommand::SetPipeline { pipeline_id: pid },
                // Scissor coupe la bande droite (x >= 100)
                RenderCommand::SetScissor {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 128,
                },
                RenderCommand::SetVertexBuffer {
                    data: vb,
                    stride: std::mem::size_of::<VertexPosUvColor>() as u32,
                },
                RenderCommand::SetIndexBuffer {
                    data: ib,
                    index32: false,
                },
                RenderCommand::DrawIndexed {
                    index_count: 6,
                    instance_count: 2,
                    first_index: 0,
                    vertex_offset: 0,
                    first_instance: 0,
                },
            ])
            .expect("rendu instancié + BC1 + alpha blend + scissor");

        let rgba = renderer.readback_rgba().expect("readback GPU");
        let px = |x: usize, y: usize| -> [u8; 4] {
            let i = (y * 128 + x) * 4;
            [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
        };
        // Instance 0 (x=32, y=64) : vert BC1 à 50% sur fond rouge -> R ~ 127, G ~ 127, B ~ 0.
        let inst0 = px(32, 64);
        assert!(
            inst0[0] > 95 && inst0[0] < 160 && inst0[1] > 95 && inst0[1] < 160 && inst0[2] < 20,
            "instance 0 BC1+alpha blend incorrect: {inst0:?}"
        );
        // Instance 1 dans le scissor (x=90, y=64) : également vert BC1 blendé sur fond rouge !
        let inst1 = px(90, 64);
        assert!(
            inst1[0] > 95 && inst1[0] < 160 && inst1[1] > 95 && inst1[1] < 160 && inst1[2] < 20,
            "instance 1 BC1+alpha blend incorrect: {inst1:?}"
        );
        // Hors scissor (x=112, y=64) : doit rester rouge pur (clippé par SetScissor) !
        let clipped = px(112, 64);
        assert!(
            clipped[0] > 240 && clipped[1] < 15 && clipped[2] < 15,
            "scissor clipping incorrect: {clipped:?}"
        );
    }
}
