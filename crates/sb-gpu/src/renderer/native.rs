//! Pipelines graphiques / compute **natifs RDNA 2** (`ShaderFormat::Rdna2Native`).
//!
//! Un pipeline natif a son propre `VkDescriptorSetLayout`, dérivé des liaisons réellement
//! utilisées par les modules SPIR-V traduits (voir `rdna2_native` et `docs/rdna2_pipeline.md`) :
//!
//! * liaison `0` : SSBO « user data » (les mots des user-SGPR, communs à tous les étages) ;
//! * liaisons `1..=32` (VS, CS) / `33..=64` (PS) : une liaison par ressource déclarée
//!   (V# → `STORAGE_BUFFER`, T# → `SAMPLED_IMAGE`, S# → `SAMPLER`).
//!
//! Les ressources sont fournies **par commande** (`NativeDrawCmd` / `NativeDispatchCmd`) : le
//! HLE résout les V#/T#/S# lus en mémoire en contenus de tampons, images créées par le bridge
//! (`create_native_image`) et descriptions d'échantillonneurs. Les tampons sont des copies
//! (copie entrante) ; après un dispatch, leur contenu est renvoyé pour recopie sortante.

use std::collections::{BTreeMap, HashMap};

use super::*;
use crate::rdna2_native::{
    NativeBindingUse, NativeShader, NativeShaderMeta, NativeStage, USER_DATA_BINDING,
};

/// Description d'un échantillonneur Vulkan issue d'un S# (ou de valeurs par défaut).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NativeSamplerDesc {
    pub mag_filter: vk::Filter,
    pub min_filter: vk::Filter,
    pub mipmap_mode: vk::SamplerMipmapMode,
    pub address_u: vk::SamplerAddressMode,
    pub address_v: vk::SamplerAddressMode,
    pub address_w: vk::SamplerAddressMode,
    pub mip_lod_bias: f32,
    /// `<= 1.0` : anisotropie désactivée.
    pub max_anisotropy: f32,
    pub compare_op: Option<vk::CompareOp>,
    pub min_lod: f32,
    pub max_lod: f32,
    pub border_color: vk::BorderColor,
    pub unnormalized: bool,
}

impl Default for NativeSamplerDesc {
    fn default() -> Self {
        Self {
            mag_filter: vk::Filter::NEAREST,
            min_filter: vk::Filter::NEAREST,
            mipmap_mode: vk::SamplerMipmapMode::NEAREST,
            address_u: vk::SamplerAddressMode::CLAMP_TO_EDGE,
            address_v: vk::SamplerAddressMode::CLAMP_TO_EDGE,
            address_w: vk::SamplerAddressMode::CLAMP_TO_EDGE,
            mip_lod_bias: 0.0,
            max_anisotropy: 1.0,
            compare_op: None,
            min_lod: 0.0,
            max_lod: 15.0,
            border_color: vk::BorderColor::FLOAT_TRANSPARENT_BLACK,
            unnormalized: false,
        }
    }
}

/// Contenu d'une ressource résolue pour une commande.
#[derive(Debug, Clone)]
pub enum NativeBinding {
    /// Descripteur non résolu : une ressource de repli est liée (zéros / texture blanche).
    Unbound,
    /// Copie des octets du V# (`num_records * stride`, borné à la ressource).
    Buffer(Vec<u8>),
    /// Image créée par `create_native_image` + swizzle de composants issu du T#.
    Image {
        image_id: usize,
        swizzle: [vk::ComponentSwizzle; 4],
    },
    Sampler(NativeSamplerDesc),
}

/// État d'un étage pour une commande : mots des user-SGPR + ressources (indexées comme
/// `NativeShaderMeta::resources`).
#[derive(Debug, Clone, Default)]
pub struct NativeStageState {
    pub user_data: Vec<u32>,
    pub bindings: Vec<NativeBinding>,
}

/// `Draw` natif (VS + PS).
#[derive(Debug, Clone)]
pub struct NativeDrawCmd {
    pub pipeline_id: usize,
    pub vertex_count: u32,
    pub instance_count: u32,
    pub first_vertex: u32,
    pub first_instance: u32,
    pub vs: NativeStageState,
    pub ps: NativeStageState,
}

/// `Dispatch` natif (CS).
#[derive(Debug, Clone)]
pub struct NativeDispatchCmd {
    pub pipeline_id: usize,
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub cs: NativeStageState,
}

/// Image 2D créée à la demande du bridge pour un T# (contenu linéaire, mip 0).
pub struct NativeImage {
    gpu: Arc<GpuContext>,
    pub width: u32,
    pub height: u32,
    pub format: vk::Format,
    image: vk::Image,
    memory: vk::DeviceMemory,
    texel_bytes: u32,
    uploaded: bool,
}

impl Drop for NativeImage {
    fn drop(&mut self) {
        // SAFETY: destruction ordonnée de l'image Vulkan.
        unsafe {
            let _ = self.gpu.device.device_wait_idle();
            self.gpu.device.destroy_image(self.image, None);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}

struct NativeStageInfo {
    stage: NativeStage,
    meta: NativeShaderMeta,
    bindings: Vec<NativeBindingUse>,
}

/// Layout et liaisons d'un pipeline natif.
pub(crate) struct NativePipelineInfo {
    gpu: Arc<GpuContext>,
    set_layout: vk::DescriptorSetLayout,
    stages: Vec<NativeStageInfo>,
}

impl Drop for NativePipelineInfo {
    fn drop(&mut self) {
        // SAFETY: le layout n'est plus référencé (pipelines détruits avant, GPU inactif).
        unsafe {
            self.gpu
                .device
                .destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}

/// Objets Vulkan temporaires d'une commande, libérés après la fin d'exécution GPU.
#[derive(Default)]
pub(crate) struct NativeTemp {
    buffers: Vec<(vk::Buffer, vk::DeviceMemory)>,
    views: Vec<vk::ImageView>,
    samplers: Vec<vk::Sampler>,
    /// `(indice de ressource, mémoire, longueur d'origine)` des tampons à relire.
    readbacks: Vec<(usize, vk::DeviceMemory, usize)>,
}

/// Octets par texel des formats que le bridge sait téléverser.
pub fn native_format_texel_bytes(f: vk::Format) -> Option<u32> {
    use vk::Format as F;
    Some(match f {
        F::R8_UNORM | F::R8_SNORM | F::R8_UINT | F::R8_SINT => 1,
        F::R8G8_UNORM | F::R8G8_SNORM | F::R8G8_UINT | F::R8G8_SINT => 2,
        F::R16_SFLOAT | F::R16_UNORM | F::R16_UINT | F::R16_SINT => 2,
        F::R8G8B8A8_UNORM
        | F::R8G8B8A8_SRGB
        | F::R8G8B8A8_SNORM
        | F::R8G8B8A8_UINT
        | F::R8G8B8A8_SINT
        | F::B8G8R8A8_UNORM
        | F::B8G8R8A8_SRGB => 4,
        F::R16G16_SFLOAT | F::R16G16_UNORM | F::R32_SFLOAT | F::R32_UINT | F::R32_SINT => 4,
        F::R16G16B16A16_SFLOAT | F::R16G16B16A16_UNORM | F::R32G32_SFLOAT => 8,
        F::R32G32B32A32_SFLOAT => 16,
        _ => return None,
    })
}

fn stage_flags(s: NativeStage) -> vk::ShaderStageFlags {
    match s {
        NativeStage::Vertex => vk::ShaderStageFlags::VERTEX,
        NativeStage::Pixel => vk::ShaderStageFlags::FRAGMENT,
        NativeStage::Compute => vk::ShaderStageFlags::COMPUTE,
    }
}

fn descriptor_type(kind: &str) -> Result<vk::DescriptorType, String> {
    match kind {
        "storage_buffer" => Ok(vk::DescriptorType::STORAGE_BUFFER),
        "sampled_image" => Ok(vk::DescriptorType::SAMPLED_IMAGE),
        "sampler" => Ok(vk::DescriptorType::SAMPLER),
        other => Err(format!(
            "liaison `{other}` non supportée par les pipelines natifs (images de stockage : hors tranche)"
        )),
    }
}

fn load_stage(blob: &[u8], want: NativeStage) -> Result<(NativeStageInfo, Vec<u32>, bool), String> {
    let sh = NativeShader::parse(blob).map_err(|e| e.to_string())?;
    if sh.meta.stage != want {
        return Err(format!(
            "étage {:?} fourni, {want:?} attendu",
            sh.meta.stage
        ));
    }
    let t = sh.translate().map_err(|e| e.to_string())?;
    Ok((
        NativeStageInfo {
            stage: want,
            meta: sh.meta,
            bindings: t.bindings,
        },
        t.spirv,
        t.info.uses_exec_mask,
    ))
}

impl VulkanRenderer {
    // ------------------------------------------------------------ images

    /// Crée une image 2D (mip 0) de `format` et y téléverse `data` (pixels linéaires serrés,
    /// `width * height * texel_bytes` octets). Renvoie son identifiant.
    pub fn create_native_image(
        &mut self,
        width: u32,
        height: u32,
        format: vk::Format,
        data: &[u8],
    ) -> Result<usize, String> {
        let texel_bytes = native_format_texel_bytes(format)
            .ok_or_else(|| format!("format {format:?} non supporté pour les images natives"))?;
        if width == 0 || height == 0 || width > 16384 || height > 16384 {
            return Err(format!("dimensions d'image invalides {width}x{height}"));
        }
        let expected = (width as usize) * (height as usize) * texel_bytes as usize;
        if data.len() != expected {
            return Err(format!(
                "données d'image : {} octets fournis, {expected} attendus",
                data.len()
            ));
        }
        let gpu = Arc::clone(&self.gpu);
        let dev = &gpu.device;
        // SAFETY: propriétés de format du GPU courant.
        let feats = unsafe {
            gpu.instance
                .get_physical_device_format_properties(gpu.physical_device, format)
        }
        .optimal_tiling_features;
        if !feats
            .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE | vk::FormatFeatureFlags::TRANSFER_DST)
        {
            return Err(format!("format {format:?} non échantillonnable sur ce GPU"));
        }
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: création de l'image et de sa mémoire (DEVICE_LOCAL de préférence).
        let (image, memory) = unsafe {
            let image = dev.create_image(&info, None).map_err(|e| e.to_string())?;
            let reqs = dev.get_image_memory_requirements(image);
            let ty = gpu
                .find_memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                .or_else(|| {
                    gpu.find_memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::empty())
                });
            let Some(ty) = ty else {
                dev.destroy_image(image, None);
                return Err("aucun type mémoire pour l'image".into());
            };
            let memory = match dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(ty),
                None,
            ) {
                Ok(m) => m,
                Err(e) => {
                    dev.destroy_image(image, None);
                    return Err(e.to_string());
                }
            };
            if let Err(e) = dev.bind_image_memory(image, memory, 0) {
                dev.destroy_image(image, None);
                dev.free_memory(memory, None);
                return Err(e.to_string());
            }
            (image, memory)
        };
        let id = self.native_images.len();
        self.native_images.push(Some(NativeImage {
            gpu,
            width,
            height,
            format,
            image,
            memory,
            texel_bytes,
            uploaded: false,
        }));
        if let Err(e) = self.update_native_image(id, data) {
            self.native_images[id] = None;
            return Err(e);
        }
        sb_debug!(
            "renderer",
            "image native #{id} créée ({width}x{height} {format:?})"
        );
        Ok(id)
    }

    /// Remplace le contenu d'une image native (mêmes dimensions / format).
    pub fn update_native_image(&mut self, id: usize, data: &[u8]) -> Result<(), String> {
        let Some(Some(img)) = self.native_images.get(id) else {
            return Err(format!("image native #{id} inconnue"));
        };
        let expected = (img.width as usize) * (img.height as usize) * img.texel_bytes as usize;
        if data.len() != expected {
            return Err(format!(
                "données d'image : {} octets fournis, {expected} attendus",
                data.len()
            ));
        }
        let (image, w, h, old_layout) = (
            img.image,
            img.width,
            img.height,
            if img.uploaded {
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
            } else {
                vk::ImageLayout::UNDEFINED
            },
        );
        let (staging, staging_mem) = create_host_buffer_raw(
            &self.gpu,
            data.len() as u64,
            vk::BufferUsageFlags::TRANSFER_SRC,
            Some(data),
        )
        .map_err(|e| e.to_string())?;
        let dev = &self.gpu.device;
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        // SAFETY: soumission synchrone staging -> image ; barrières de layout explicites.
        let res = unsafe {
            (|| -> Result<(), vk::Result> {
                dev.reset_fences(&[self.fence])?;
                dev.reset_command_buffer(self.cmd_buf, vk::CommandBufferResetFlags::empty())?;
                dev.begin_command_buffer(
                    self.cmd_buf,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )?;
                let to_dst = vk::ImageMemoryBarrier::default()
                    .old_layout(old_layout)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .image(image)
                    .subresource_range(range)
                    .src_access_mask(vk::AccessFlags::SHADER_READ)
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
                dev.cmd_pipeline_barrier(
                    self.cmd_buf,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_dst],
                );
                let region = [vk::BufferImageCopy::default()
                    .image_subresource(vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    })
                    .image_extent(vk::Extent3D {
                        width: w,
                        height: h,
                        depth: 1,
                    })];
                dev.cmd_copy_buffer_to_image(
                    self.cmd_buf,
                    staging,
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
                    vk::PipelineStageFlags::FRAGMENT_SHADER
                        | vk::PipelineStageFlags::VERTEX_SHADER
                        | vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_read],
                );
                dev.end_command_buffer(self.cmd_buf)?;
                let cmds = [self.cmd_buf];
                let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
                dev.queue_submit(self.gpu.graphics_queue, &submit, self.fence)?;
                dev.wait_for_fences(&[self.fence], true, u64::MAX)
            })()
        };
        // SAFETY: le GPU a terminé (ou la soumission a échoué) : le staging n'est plus utilisé.
        unsafe {
            dev.destroy_buffer(staging, None);
            dev.free_memory(staging_mem, None);
        }
        res.map_err(|e| e.to_string())?;
        if let Some(Some(img)) = self.native_images.get_mut(id) {
            img.uploaded = true;
        }
        Ok(())
    }

    /// Dimensions et format d'une image native.
    pub fn native_image_info(&self, id: usize) -> Option<(u32, u32, vk::Format)> {
        self.native_images
            .get(id)
            .and_then(|i| i.as_ref())
            .map(|i| (i.width, i.height, i.format))
    }

    // ------------------------------------------------------------ pipelines

    fn native_set_layout(
        &self,
        stages: &[&NativeStageInfo],
    ) -> Result<vk::DescriptorSetLayout, String> {
        let mut map: BTreeMap<u32, (vk::DescriptorType, vk::ShaderStageFlags)> = BTreeMap::new();
        for st in stages {
            for b in &st.bindings {
                let ty = descriptor_type(b.kind)?;
                let e = map
                    .entry(b.binding)
                    .or_insert((ty, vk::ShaderStageFlags::empty()));
                if e.0 != ty {
                    return Err(format!(
                        "liaison {} utilisée avec deux types de descripteurs",
                        b.binding
                    ));
                }
                e.1 |= stage_flags(st.stage);
            }
        }
        let binds: Vec<vk::DescriptorSetLayoutBinding> = map
            .iter()
            .map(|(&binding, &(ty, flags))| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(binding)
                    .descriptor_type(ty)
                    .descriptor_count(1)
                    .stage_flags(flags)
            })
            .collect();
        // SAFETY: création du layout de set depuis des liaisons valides.
        unsafe {
            self.gpu.device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&binds),
                None,
            )
        }
        .map_err(|e| format!("vkCreateDescriptorSetLayout: {e}"))
    }

    /// Compile un pipeline graphique natif (VS + PS au format `SBR2`). Aucun attribut de sommet :
    /// le VS lit ses sommets par V# (`buffer_load`). Renvoie l'identifiant de pipeline graphique
    /// (même espace d'identifiants que les pipelines DXBC).
    pub fn create_native_graphics_pipeline(
        &mut self,
        vs_blob: &[u8],
        ps_blob: &[u8],
        cfg: &GraphicsPipelineConfig,
    ) -> Result<usize, String> {
        let (vs_info, vs_spv, vs_exec) =
            load_stage(vs_blob, NativeStage::Vertex).map_err(|e| format!("VS natif : {e}"))?;
        let (ps_info, ps_spv, ps_exec) =
            load_stage(ps_blob, NativeStage::Pixel).map_err(|e| format!("PS natif : {e}"))?;
        if vs_exec || ps_exec {
            sb_warn!(
                "renderer",
                "pipeline natif : un étage graphique modifie EXEC ; la taille de sous-groupe est choisie par le pilote (hypothèse : égale à la taille de vague)"
            );
        }
        let set_layout = self.native_set_layout(&[&vs_info, &ps_info])?;
        let info = NativePipelineInfo {
            gpu: Arc::clone(&self.gpu),
            set_layout,
            stages: vec![vs_info, ps_info],
        };
        let dev = &self.gpu.device;
        let set_layouts = [set_layout];
        // SAFETY: création du layout de pipeline, des modules et du pipeline ; tout est détruit
        // en cas d'erreur, les modules après création du pipeline.
        unsafe {
            let layout = dev
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts),
                    None,
                )
                .map_err(|e| format!("vkCreatePipelineLayout: {e}"))?;
            let vs_mod = dev
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&vs_spv), None)
                .map_err(|e| format!("vkCreateShaderModule(VS natif): {e}"))?;
            let ps_mod = match dev
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&ps_spv), None)
            {
                Ok(m) => m,
                Err(e) => {
                    dev.destroy_shader_module(vs_mod, None);
                    dev.destroy_pipeline_layout(layout, None);
                    return Err(format!("vkCreateShaderModule(PS natif): {e}"));
                }
            };
            let stages = [
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::VERTEX)
                    .module(vs_mod)
                    .name(c"main"),
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT)
                    .module(ps_mod)
                    .name(c"main"),
            ];
            let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
            let res = self.create_gfx_pipeline_raw(&stages, &vertex_input, layout, cfg);
            dev.destroy_shader_module(vs_mod, None);
            dev.destroy_shader_module(ps_mod, None);
            let pipeline = match res {
                Ok(p) => p,
                Err(e) => {
                    dev.destroy_pipeline_layout(layout, None);
                    return Err(e);
                }
            };
            let id = self.pipelines.len();
            self.pipelines.push(Some(GraphicsPipeline {
                gpu: Arc::clone(&self.gpu),
                layout,
                pipeline,
            }));
            self.native_gfx.insert(id, info);
            sb_info!(
                "renderer",
                "VkPipeline graphique natif RDNA 2 #{id} compilé sur '{}'",
                self.gpu.caps.device_name
            );
            Ok(id)
        }
    }

    /// Compile un pipeline compute natif (CS au format `SBR2`). Si le programme modifie EXEC,
    /// la taille de sous-groupe est imposée à la taille de vague (`subgroupSizeControl`) ; sans
    /// cette capacité le pipeline est refusé plutôt que de produire des résultats faux.
    pub fn create_native_compute_pipeline(&mut self, cs_blob: &[u8]) -> Result<usize, String> {
        let (cs_info, cs_spv, uses_exec) =
            load_stage(cs_blob, NativeStage::Compute).map_err(|e| format!("CS natif : {e}"))?;
        let wave = cs_info.meta.wave;
        let caps = &self.gpu.caps;
        let force_wave = if uses_exec {
            let ok = caps.has_subgroup_size_control
                && wave >= caps.min_subgroup_size
                && wave <= caps.max_subgroup_size
                && caps
                    .required_subgroup_size_stages
                    .contains(vk::ShaderStageFlags::COMPUTE);
            if !ok {
                return Err(format!(
                    "le programme dépend de la taille de vague ({wave}) mais le GPU ne permet pas de l'imposer"
                ));
            }
            true
        } else {
            false
        };
        let set_layout = self.native_set_layout(&[&cs_info])?;
        let info = NativePipelineInfo {
            gpu: Arc::clone(&self.gpu),
            set_layout,
            stages: vec![cs_info],
        };
        let dev = &self.gpu.device;
        let set_layouts = [set_layout];
        // SAFETY: création du layout, du module et du pipeline compute.
        unsafe {
            let layout = dev
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts),
                    None,
                )
                .map_err(|e| format!("vkCreatePipelineLayout(CS natif): {e}"))?;
            let module = match dev
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&cs_spv), None)
            {
                Ok(m) => m,
                Err(e) => {
                    dev.destroy_pipeline_layout(layout, None);
                    return Err(format!("vkCreateShaderModule(CS natif): {e}"));
                }
            };
            let mut rss = vk::PipelineShaderStageRequiredSubgroupSizeCreateInfo::default()
                .required_subgroup_size(wave);
            let mut stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(module)
                .name(c"main");
            if force_wave {
                stage = stage.push_next(&mut rss);
            }
            let ci = [vk::ComputePipelineCreateInfo::default()
                .stage(stage)
                .layout(layout)];
            let res = dev.create_compute_pipelines(self.pipeline_cache, &ci, None);
            dev.destroy_shader_module(module, None);
            let pipeline = match res {
                Ok(l) => l[0],
                Err((_, e)) => {
                    dev.destroy_pipeline_layout(layout, None);
                    return Err(format!("vkCreateComputePipelines(natif): {e}"));
                }
            };
            let id = self.compute_pipelines.len();
            self.compute_pipelines.push(Some(ComputePipeline {
                gpu: Arc::clone(&self.gpu),
                layout,
                pipeline,
            }));
            self.native_cs.insert(id, info);
            sb_info!(
                "renderer",
                "VkPipeline compute natif RDNA 2 #{id} compilé sur '{}' (wave{wave}{})",
                self.gpu.caps.device_name,
                if force_wave {
                    ", sous-groupe imposé"
                } else {
                    ""
                }
            );
            Ok(id)
        }
    }

    /// `true` si le pipeline graphique `id` est natif.
    pub fn is_native_graphics_pipeline(&self, id: usize) -> bool {
        self.native_gfx.contains_key(&id)
    }

    // ------------------------------------------------------------ liaison des ressources

    fn ensure_native_pool(&mut self) -> Result<vk::DescriptorPool, String> {
        if let Some(p) = self.native_pool {
            return Ok(p);
        }
        let sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 8192,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::SAMPLED_IMAGE,
                descriptor_count: 2048,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::SAMPLER,
                descriptor_count: 2048,
            },
        ];
        // SAFETY: création du pool de sets natifs (réinitialisé après chaque exécution).
        let pool = unsafe {
            self.gpu.device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1024)
                    .pool_sizes(&sizes),
                None,
            )
        }
        .map_err(|e| format!("vkCreateDescriptorPool(natif): {e}"))?;
        self.native_pool = Some(pool);
        Ok(pool)
    }

    /// Alloue le set d'une commande et y écrit user data + ressources. Les objets temporaires
    /// créés sont consignés dans `temp` (libérés par `native_release`, même en cas d'erreur).
    fn native_bind_resources(
        &self,
        pool: vk::DescriptorPool,
        info: &NativePipelineInfo,
        states: &[&NativeStageState],
        temp: &mut NativeTemp,
    ) -> Result<vk::DescriptorSet, String> {
        let dev = &self.gpu.device;
        let layouts = [info.set_layout];
        // SAFETY: allocation d'un set depuis le pool natif.
        let set = unsafe {
            dev.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts),
            )
        }
        .map_err(|e| format!("vkAllocateDescriptorSets(natif): {e}"))?[0];

        let make_buffer = |bytes: &[u8], temp: &mut NativeTemp| -> Result<vk::Buffer, String> {
            let padded = bytes.len().div_ceil(16).max(1) * 16;
            let mut data = vec![0u8; padded];
            data[..bytes.len()].copy_from_slice(bytes);
            let (buf, mem) = create_host_buffer_raw(
                &self.gpu,
                padded as u64,
                vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_SRC,
                Some(&data),
            )
            .map_err(|e| e.to_string())?;
            temp.buffers.push((buf, mem));
            Ok(buf)
        };

        enum Item {
            Buf(u32, usize),
            Img(u32, usize),
            Smp(u32, usize),
        }
        let mut items = Vec::new();
        let mut buf_infos: Vec<vk::DescriptorBufferInfo> = Vec::new();
        let mut img_infos: Vec<vk::DescriptorImageInfo> = Vec::new();
        let mut user_done = false;

        for (si, st) in info.stages.iter().enumerate() {
            let state = states
                .get(si)
                .ok_or_else(|| "état d'étage manquant".to_string())?;
            for b in &st.bindings {
                match b.resource {
                    None => {
                        debug_assert_eq!(b.binding, USER_DATA_BINDING);
                        if user_done {
                            continue;
                        }
                        // Le user data est commun aux étages : cohérence des préfixes, puis le
                        // plus long sert de contenu (chaque étage lit ses N premiers mots).
                        let longest = states
                            .iter()
                            .max_by_key(|s| s.user_data.len())
                            .copied()
                            .unwrap_or(state);
                        for other in states {
                            let n = longest.user_data.len().min(other.user_data.len());
                            if longest.user_data[..n] != other.user_data[..n] {
                                return Err("user data différent entre étages".into());
                            }
                        }
                        let bytes: Vec<u8> = longest
                            .user_data
                            .iter()
                            .flat_map(|w| w.to_le_bytes())
                            .collect();
                        let buf = make_buffer(&bytes, temp)?;
                        buf_infos.push(
                            vk::DescriptorBufferInfo::default()
                                .buffer(buf)
                                .range(vk::WHOLE_SIZE),
                        );
                        items.push(Item::Buf(b.binding, buf_infos.len() - 1));
                        user_done = true;
                    }
                    Some(r) => {
                        let bind = state.bindings.get(r).unwrap_or(&NativeBinding::Unbound);
                        match (b.kind, bind) {
                            ("storage_buffer", NativeBinding::Buffer(bytes)) => {
                                let buf = make_buffer(bytes, temp)?;
                                let mem = temp.buffers.last().map(|x| x.1).unwrap_or_default();
                                temp.readbacks.push((r, mem, bytes.len()));
                                buf_infos.push(
                                    vk::DescriptorBufferInfo::default()
                                        .buffer(buf)
                                        .range(vk::WHOLE_SIZE),
                                );
                                items.push(Item::Buf(b.binding, buf_infos.len() - 1));
                            }
                            ("storage_buffer", _) => {
                                sb_warn!(
                                    "renderer",
                                    "ressource {r} (V#) non résolue : tampon de zéros lié"
                                );
                                let buf = make_buffer(&[0u8; 16], temp)?;
                                buf_infos.push(
                                    vk::DescriptorBufferInfo::default()
                                        .buffer(buf)
                                        .range(vk::WHOLE_SIZE),
                                );
                                items.push(Item::Buf(b.binding, buf_infos.len() - 1));
                            }
                            ("sampled_image", NativeBinding::Image { image_id, swizzle }) => {
                                let Some(Some(img)) = self.native_images.get(*image_id) else {
                                    return Err(format!("image native #{image_id} inconnue"));
                                };
                                let view = self.native_view(img, *swizzle, temp)?;
                                img_infos.push(
                                    vk::DescriptorImageInfo::default()
                                        .image_view(view)
                                        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
                                );
                                items.push(Item::Img(b.binding, img_infos.len() - 1));
                            }
                            ("sampled_image", _) => {
                                sb_warn!(
                                    "renderer",
                                    "ressource {r} (T#) non résolue : texture blanche liée"
                                );
                                let view = self
                                    .textures
                                    .first()
                                    .and_then(|t| t.as_ref())
                                    .map(|t| t.view)
                                    .ok_or_else(|| "texture de repli absente".to_string())?;
                                img_infos.push(
                                    vk::DescriptorImageInfo::default()
                                        .image_view(view)
                                        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
                                );
                                items.push(Item::Img(b.binding, img_infos.len() - 1));
                            }
                            ("sampler", other) => {
                                let desc = match other {
                                    NativeBinding::Sampler(d) => *d,
                                    _ => {
                                        sb_warn!(
                                            "renderer",
                                            "ressource {r} (S#) non résolue : échantillonneur par défaut"
                                        );
                                        NativeSamplerDesc::default()
                                    }
                                };
                                let smp = self.native_sampler(&desc, temp)?;
                                img_infos.push(vk::DescriptorImageInfo::default().sampler(smp));
                                items.push(Item::Smp(b.binding, img_infos.len() - 1));
                            }
                            (k, _) => return Err(format!("type de liaison `{k}` non supporté")),
                        }
                    }
                }
            }
        }
        let writes: Vec<vk::WriteDescriptorSet> = items
            .iter()
            .map(|it| match *it {
                Item::Buf(binding, i) => vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(binding)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(std::slice::from_ref(&buf_infos[i])),
                Item::Img(binding, i) => vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(binding)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(std::slice::from_ref(&img_infos[i])),
                Item::Smp(binding, i) => vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(binding)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .image_info(std::slice::from_ref(&img_infos[i])),
            })
            .collect();
        // SAFETY: `writes` référence `buf_infos` / `img_infos`, vivants jusqu'ici.
        unsafe { dev.update_descriptor_sets(&writes, &[]) };
        Ok(set)
    }

    fn native_view(
        &self,
        img: &NativeImage,
        swizzle: [vk::ComponentSwizzle; 4],
        temp: &mut NativeTemp,
    ) -> Result<vk::ImageView, String> {
        let info = vk::ImageViewCreateInfo::default()
            .image(img.image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(img.format)
            .components(vk::ComponentMapping {
                r: swizzle[0],
                g: swizzle[1],
                b: swizzle[2],
                a: swizzle[3],
            })
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        // SAFETY: vue sur une image valide.
        let v = unsafe { self.gpu.device.create_image_view(&info, None) }
            .map_err(|e| format!("vkCreateImageView(natif): {e}"))?;
        temp.views.push(v);
        Ok(v)
    }

    fn native_sampler(
        &self,
        d: &NativeSamplerDesc,
        temp: &mut NativeTemp,
    ) -> Result<vk::Sampler, String> {
        let aniso = d.max_anisotropy > 1.0 && self.gpu_has_anisotropy();
        let mut info = vk::SamplerCreateInfo::default()
            .mag_filter(d.mag_filter)
            .min_filter(d.min_filter)
            .mipmap_mode(d.mipmap_mode)
            .address_mode_u(d.address_u)
            .address_mode_v(d.address_v)
            .address_mode_w(d.address_w)
            .mip_lod_bias(d.mip_lod_bias)
            .anisotropy_enable(aniso)
            .max_anisotropy(if aniso { d.max_anisotropy } else { 1.0 })
            .min_lod(d.min_lod)
            .max_lod(d.max_lod)
            .border_color(d.border_color)
            .unnormalized_coordinates(d.unnormalized);
        if let Some(op) = d.compare_op {
            info = info.compare_enable(true).compare_op(op);
        }
        // SAFETY: création d'un échantillonneur (valeurs validées par le pilote).
        let s = unsafe { self.gpu.device.create_sampler(&info, None) }
            .map_err(|e| format!("vkCreateSampler(natif): {e}"))?;
        temp.samplers.push(s);
        Ok(s)
    }

    fn gpu_has_anisotropy(&self) -> bool {
        // SAFETY: lecture des fonctionnalités du GPU courant.
        unsafe {
            self.gpu
                .instance
                .get_physical_device_features(self.gpu.physical_device)
        }
        .sampler_anisotropy
            == vk::TRUE
    }

    /// Libère les objets temporaires d'une commande et réinitialise le pool de sets.
    pub(crate) fn native_release(&self, temp: NativeTemp) {
        let dev = &self.gpu.device;
        // SAFETY: le GPU a terminé l'exécution (fence attendue) ou rien n'a été soumis.
        unsafe {
            for (b, m) in temp.buffers {
                dev.destroy_buffer(b, None);
                dev.free_memory(m, None);
            }
            for v in temp.views {
                dev.destroy_image_view(v, None);
            }
            for s in temp.samplers {
                dev.destroy_sampler(s, None);
            }
            if let Some(p) = self.native_pool {
                let _ = dev.reset_descriptor_pool(p, vk::DescriptorPoolResetFlags::empty());
            }
        }
    }

    /// Prépare les sets d'une série de `NativeDraw` d'un batch (appelé avant l'enregistrement).
    pub(crate) fn native_prepare_draws(
        &mut self,
        commands: &[RenderCommand],
        temp: &mut NativeTemp,
    ) -> Vec<Option<vk::DescriptorSet>> {
        let mut out: Vec<Option<vk::DescriptorSet>> = vec![None; commands.len()];
        if !commands
            .iter()
            .any(|c| matches!(c, RenderCommand::NativeDraw(_)))
        {
            return out;
        }
        let pool = match self.ensure_native_pool() {
            Ok(p) => p,
            Err(e) => {
                sb_warn!("renderer", "{e}");
                return out;
            }
        };
        for (i, c) in commands.iter().enumerate() {
            let RenderCommand::NativeDraw(nd) = c else {
                continue;
            };
            let Some(info) = self.native_gfx.get(&nd.pipeline_id) else {
                sb_warn!(
                    "renderer",
                    "NativeDraw : pipeline #{} n'est pas un pipeline natif",
                    nd.pipeline_id
                );
                continue;
            };
            match self.native_bind_resources(pool, info, &[&nd.vs, &nd.ps], temp) {
                Ok(set) => out[i] = Some(set),
                Err(e) => sb_warn!("renderer", "NativeDraw ignoré : {e}"),
            }
        }
        out
    }

    /// Exécute un `Dispatch` natif ; renvoie, par ressource, le contenu des tampons après
    /// exécution (`Some` pour les V#, `None` sinon) pour la recopie sortante.
    pub fn dispatch_native(
        &mut self,
        cmd: &NativeDispatchCmd,
    ) -> Result<Vec<Option<Vec<u8>>>, String> {
        let (Some(Some(cp)), Some(info)) = (
            self.compute_pipelines.get(cmd.pipeline_id),
            self.native_cs.get(&cmd.pipeline_id),
        ) else {
            return Err(format!(
                "pipeline compute natif #{} inconnu",
                cmd.pipeline_id
            ));
        };
        let (pipeline, layout) = (cp.pipeline, cp.layout);
        let n_res = info.stages[0].meta.resources.len();
        let pool = self.ensure_native_pool()?;
        let info = &self.native_cs[&cmd.pipeline_id];
        let mut temp = NativeTemp::default();
        let set = match self.native_bind_resources(pool, info, &[&cmd.cs], &mut temp) {
            Ok(s) => s,
            Err(e) => {
                self.native_release(temp);
                return Err(e);
            }
        };
        let dev = &self.gpu.device;
        // SAFETY: soumission synchrone d'un dispatch ; barrière SHADER_WRITE -> HOST_READ.
        let run = unsafe {
            (|| -> Result<(), vk::Result> {
                dev.reset_fences(&[self.fence])?;
                dev.reset_command_buffer(self.cmd_buf, vk::CommandBufferResetFlags::empty())?;
                dev.begin_command_buffer(
                    self.cmd_buf,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )?;
                dev.cmd_bind_pipeline(self.cmd_buf, vk::PipelineBindPoint::COMPUTE, pipeline);
                dev.cmd_bind_descriptor_sets(
                    self.cmd_buf,
                    vk::PipelineBindPoint::COMPUTE,
                    layout,
                    0,
                    &[set],
                    &[],
                );
                dev.cmd_dispatch(self.cmd_buf, cmd.x.max(1), cmd.y.max(1), cmd.z.max(1));
                let barrier = [vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                    .dst_access_mask(vk::AccessFlags::HOST_READ)];
                dev.cmd_pipeline_barrier(
                    self.cmd_buf,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::PipelineStageFlags::HOST,
                    vk::DependencyFlags::empty(),
                    &barrier,
                    &[],
                    &[],
                );
                dev.end_command_buffer(self.cmd_buf)?;
                let cmds = [self.cmd_buf];
                let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
                dev.queue_submit(self.gpu.graphics_queue, &submit, self.fence)?;
                dev.wait_for_fences(&[self.fence], true, u64::MAX)
            })()
        };
        let mut out: Vec<Option<Vec<u8>>> = vec![None; n_res];
        if run.is_ok() {
            for &(r, mem, len) in &temp.readbacks {
                // SAFETY: mémoire host-visible/coherent d'un tampon de `len` octets (arrondi à 16).
                unsafe {
                    if let Ok(p) = dev.map_memory(mem, 0, len as u64, vk::MemoryMapFlags::empty()) {
                        let mut v = vec![0u8; len];
                        ptr::copy_nonoverlapping(p.cast::<u8>(), v.as_mut_ptr(), len);
                        dev.unmap_memory(mem);
                        if r < out.len() {
                            out[r] = Some(v);
                        }
                    }
                }
            }
        }
        self.native_release(temp);
        run.map_err(|e| format!("dispatch natif : {e}"))?;
        sb_debug!(
            "renderer",
            "Dispatch natif ({}, {}, {}) exécuté sur pipeline #{}",
            cmd.x,
            cmd.y,
            cmd.z,
            cmd.pipeline_id
        );
        Ok(out)
    }

    /// Détruit les pipelines natifs et les images natives (appelé par `Drop`).
    pub(crate) fn native_teardown(&mut self) {
        self.native_gfx.clear();
        self.native_cs.clear();
        self.native_images.clear();
        if let Some(p) = self.native_pool.take() {
            // SAFETY: GPU inactif (appelé après `device_wait_idle`).
            unsafe { self.gpu.device.destroy_descriptor_pool(p, None) };
        }
    }
}

/// Tables de pipelines natifs (champs du renderer).
pub(crate) type NativePipelines = HashMap<usize, NativePipelineInfo>;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::rdna2_native::samples;

    fn renderer(w: u32, h: u32) -> Option<VulkanRenderer> {
        let gpu = GpuContext::new(false).ok()?;
        VulkanRenderer::new(gpu, w, h).ok()
    }

    fn f32s(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    fn u32s(v: &[u32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    #[test]
    fn native_compute_affine_with_table_pointer_and_root_constant() {
        let Some(mut r) = renderer(64, 64) else {
            eprintln!("aucun GPU Vulkan : test ignoré");
            return;
        };
        let cs = samples::cs_affine().to_bytes();
        let id = match r.create_native_compute_pipeline(&cs) {
            Ok(id) => id,
            Err(e) => panic!("pipeline compute natif : {e}"),
        };
        let input: Vec<u32> = (0..128).collect();
        // user data : s0:s1 table, s2:s3 CBV, s4 = échelle
        let user = vec![0, 0, 0, 0, 0.5f32.to_bits()];
        let cmd = NativeDispatchCmd {
            pipeline_id: id,
            x: 2,
            y: 1,
            z: 1,
            cs: NativeStageState {
                user_data: user,
                bindings: vec![
                    NativeBinding::Buffer(u32s(&input)),
                    NativeBinding::Buffer(vec![0u8; 512]),
                    NativeBinding::Buffer(f32s(&[3.0, 0.0, 0.0, 0.0])),
                ],
            },
        };
        let out = r.dispatch_native(&cmd).unwrap();
        let out_buf = out[1].as_ref().expect("tampon de sortie relu");
        assert_eq!(out_buf.len(), 512);
        for i in 0..128usize {
            let v = f32::from_le_bytes(out_buf[i * 4..i * 4 + 4].try_into().unwrap());
            assert_eq!(v, i as f32 * 0.5 + 3.0, "out[{i}]");
        }
        // L'entrée n'est pas modifiée.
        assert_eq!(out[0].as_ref().unwrap(), &u32s(&input));
        // Un identifiant inconnu est une erreur, pas un crash.
        let mut bad = cmd.clone();
        bad.pipeline_id = 99;
        assert!(r.dispatch_native(&bad).is_err());
    }

    fn textured_draw(r: &mut VulkanRenderer, pid: usize, tint: [f32; 4], img: usize) {
        let verts: [[f32; 8]; 3] = [
            [-1.0, -1.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            [3.0, -1.0, 0.0, 1.0, 2.0, 0.0, 0.0, 1.0],
            [-1.0, 3.0, 0.0, 1.0, 0.0, 2.0, 0.0, 1.0],
        ];
        let vb: Vec<u8> = verts.iter().flat_map(|v| f32s(v)).collect();
        let mut ps_user = vec![0u32; 4];
        ps_user.extend(tint.iter().map(|f| f.to_bits()));
        let id = pid;
        let cmd = NativeDrawCmd {
            pipeline_id: id,
            vertex_count: 3,
            instance_count: 1,
            first_vertex: 0,
            first_instance: 0,
            vs: NativeStageState {
                user_data: vec![0, 0],
                bindings: vec![NativeBinding::Buffer(vb)],
            },
            ps: NativeStageState {
                user_data: ps_user,
                bindings: vec![
                    NativeBinding::Image {
                        image_id: img,
                        swizzle: [vk::ComponentSwizzle::IDENTITY; 4],
                    },
                    NativeBinding::Sampler(NativeSamplerDesc::default()),
                ],
            },
        };
        r.execute_commands(&[
            RenderCommand::Clear {
                rgba: [0.0, 0.0, 0.0, 1.0],
            },
            RenderCommand::SetPipeline { pipeline_id: id },
            RenderCommand::NativeDraw(Box::new(cmd)),
        ])
        .unwrap();
    }

    #[test]
    fn native_graphics_textured_triangle_with_root_constant_tint() {
        let Some(mut r) = renderer(64, 64) else {
            eprintln!("aucun GPU Vulkan : test ignoré");
            return;
        };
        let pid = match r.create_native_graphics_pipeline(
            &samples::vs_textured().to_bytes(),
            &samples::ps_tint().to_bytes(),
            &GraphicsPipelineConfig::default(),
        ) {
            Ok(id) => id,
            Err(e) => panic!("pipeline graphique natif : {e}"),
        };
        assert!(r.is_native_graphics_pipeline(pid));
        let texels: [[u8; 4]; 4] = [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
        ];
        let data: Vec<u8> = texels.iter().flatten().copied().collect();
        let img = r
            .create_native_image(2, 2, vk::Format::R8G8B8A8_UNORM, &data)
            .unwrap();
        assert!(
            r.create_native_image(2, 2, vk::Format::R8G8B8A8_UNORM, &data[..8])
                .is_err(),
            "taille de données vérifiée"
        );
        let px = |buf: &[u8], x: usize, y: usize| {
            let i = (y * 64 + x) * 4;
            [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
        };
        // Orientation : convention D3D (v croît vers le haut de l'image avec le viewport inversé).
        textured_draw(&mut r, pid, [1.0; 4], img);
        let buf = r.readback_rgba().unwrap();
        assert_eq!(
            px(&buf, 16, 48),
            [255, 0, 0, 255],
            "bas-gauche = texel (0,0)"
        );
        assert_eq!(
            px(&buf, 48, 48),
            [0, 255, 0, 255],
            "bas-droite = texel (1,0)"
        );
        assert_eq!(
            px(&buf, 16, 16),
            [0, 0, 255, 255],
            "haut-gauche = texel (0,1)"
        );
        assert_eq!(
            px(&buf, 48, 16),
            [255, 255, 255, 255],
            "haut-droite = texel (1,1)"
        );
        // Teinte (constantes racine -> user SGPR) : R/2, G/4.
        textured_draw(&mut r, pid, [0.5, 0.25, 1.0, 1.0], img);
        let buf = r.readback_rgba().unwrap();
        let near = |a: [u8; 4], b: [u8; 4]| a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 1);
        assert!(
            near(px(&buf, 16, 48), [128, 0, 0, 255]),
            "{:?}",
            px(&buf, 16, 48)
        );
        assert!(
            near(px(&buf, 48, 16), [128, 64, 255, 255]),
            "{:?}",
            px(&buf, 48, 16)
        );
        // Mise à jour du contenu de l'image : pris en compte au dessin suivant.
        let green: Vec<u8> = [0u8, 255, 0, 255].repeat(4);
        r.update_native_image(img, &green).unwrap();
        textured_draw(&mut r, pid, [1.0; 4], img);
        let buf = r.readback_rgba().unwrap();
        assert_eq!(px(&buf, 16, 16), [0, 255, 0, 255]);
        // Swizzle du T# (R<-B) appliqué par la vue.
        // (couvert côté HLE ; ici on vérifie seulement que la vue est acceptée)
        assert_eq!(
            r.native_image_info(img),
            Some((2, 2, vk::Format::R8G8B8A8_UNORM))
        );
    }

    #[test]
    fn native_pipeline_rejects_wrong_stage_and_bad_blob() {
        let Some(mut r) = renderer(32, 32) else {
            eprintln!("aucun GPU Vulkan : test ignoré");
            return;
        };
        let cfg = GraphicsPipelineConfig::default();
        let vs = samples::vs_textured().to_bytes();
        let ps = samples::ps_tint().to_bytes();
        assert!(r.create_native_graphics_pipeline(&ps, &vs, &cfg).is_err());
        assert!(
            r.create_native_graphics_pipeline(b"DXBCxxxx", &ps, &cfg)
                .is_err()
        );
        assert!(r.create_native_compute_pipeline(&vs).is_err());
    }
}
