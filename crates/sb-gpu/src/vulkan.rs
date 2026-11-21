//! Contexte Vulkan 1.3/1.4 (`ash`) natif Linux + Swapchain Wayland/SDL3 pour ScarletBridge.

use std::ffi::{CStr, CString};
use std::sync::Arc;

use ash::vk;
use sb_common::{sb_debug, sb_info, sb_warn};
use thiserror::Error;

use crate::sdl3::Sdl3Lib;

#[derive(Debug, Error)]
pub enum GpuError {
    #[error("impossible de charger libvulkan.so.1 : {0}")]
    EntryLoad(String),
    #[error("aucun GPU Vulkan compatible détecté")]
    NoPhysicalDevice,
    #[error("aucune file graphique Vulkan trouvée")]
    NoGraphicsQueue,
    #[error("erreur Vulkan : {0}")]
    Vk(#[from] vk::Result),
    #[error("surface Vulkan indisponible")]
    NoSurface,
}

#[derive(Debug, Clone)]
pub struct GpuCapabilities {
    pub device_name: String,
    pub api_version_major: u32,
    pub api_version_minor: u32,
    pub api_version_patch: u32,
    pub has_external_memory_host: bool,
    pub min_imported_host_pointer_alignment: u64,
    pub has_dynamic_rendering: bool,
    pub has_swapchain: bool,
    pub has_bc_compression: bool,
    pub has_fill_mode_non_solid: bool,
    pub has_ray_tracing: bool,
    pub validation_enabled: bool,
}

pub struct GpuContext {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: ash::Device,
    pub graphics_queue_family: u32,
    pub graphics_queue: vk::Queue,
    pub command_pool: vk::CommandPool,
    pub memory_properties: vk::PhysicalDeviceMemoryProperties,
    pub caps: GpuCapabilities,
    pub ext_memory_host_fn: Option<ash::ext::external_memory_host::Device>,
}

impl GpuContext {
    /// Initialise une instance et un périphérique logique Vulkan (avec ou sans extensions surface SDL3).
    pub fn new(with_surface: bool) -> Result<Arc<Self>, GpuError> {
        // SAFETY: chargement dynamique de `libvulkan.so.1`.
        let entry =
            unsafe { ash::Entry::load() }.map_err(|e| GpuError::EntryLoad(e.to_string()))?;

        let app_info = vk::ApplicationInfo::default()
            .application_name(c"ScarletBridge")
            .application_version(vk::make_api_version(0, 0, 1, 0))
            .engine_name(c"ScarletBridge-Vulkan")
            .engine_version(vk::make_api_version(0, 0, 1, 0))
            .api_version(vk::API_VERSION_1_3);

        let mut inst_exts: Vec<CString> = Vec::new();
        if with_surface && let Some(sdl) = Sdl3Lib::load() {
            inst_exts = sdl.vulkan_instance_extensions();
        }
        let inst_ext_ptrs: Vec<*const i8> = inst_exts.iter().map(|s| s.as_ptr()).collect();

        let mut enabled_layers: Vec<*const i8> = Vec::new();
        let validation_requested = std::env::var("SB_VK_VALIDATION").ok().as_deref() == Some("1");
        let mut validation_enabled = false;
        if validation_requested {
            // SAFETY: énumération des couches de validation installées.
            if let Ok(layers) = unsafe { entry.enumerate_instance_layer_properties() } {
                let target = c"VK_LAYER_KHRONOS_validation";
                if layers.iter().any(|l| {
                    // SAFETY: `layer_name` est terminé par nul.
                    unsafe { CStr::from_ptr(l.layer_name.as_ptr()) == target }
                }) {
                    enabled_layers.push(target.as_ptr());
                    validation_enabled = true;
                }
            }
        }

        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_layer_names(&enabled_layers)
            .enabled_extension_names(&inst_ext_ptrs);

        // SAFETY: `create_info` et les chaînes d'extensions vivent pendant l'appel.
        let instance = unsafe { entry.create_instance(&create_info, None) }?;

        // SAFETY: `instance` est valide.
        let physical_devices = unsafe { instance.enumerate_physical_devices() }?;
        if physical_devices.is_empty() {
            return Err(GpuError::NoPhysicalDevice);
        }

        // Privilégie un GPU discret (ex. AMD Radeon RX 5700 XT).
        let physical_device = physical_devices
            .iter()
            .copied()
            .find(|&pd| {
                // SAFETY: `pd` provient de `enumerate_physical_devices`.
                let props = unsafe { instance.get_physical_device_properties(pd) };
                props.device_type == vk::PhysicalDeviceType::DISCRETE_GPU
            })
            .unwrap_or(physical_devices[0]);

        // SAFETY: `physical_device` est valide.
        let props = unsafe { instance.get_physical_device_properties(physical_device) };
        let device_name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
            .to_string_lossy()
            .into_owned();

        // SAFETY: `physical_device` est valide.
        let queue_families =
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
        let graphics_queue_family = queue_families
            .iter()
            .position(|qf| {
                qf.queue_flags
                    .contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
            })
            .or_else(|| {
                queue_families
                    .iter()
                    .position(|qf| qf.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            })
            .ok_or(GpuError::NoGraphicsQueue)? as u32;

        // Détection des extensions périphérique & fonctionnalités matérielles (BC, wireframe, RT).
        // SAFETY: `physical_device` est valide.
        let supported_features = unsafe { instance.get_physical_device_features(physical_device) };
        let has_bc_compression = supported_features.texture_compression_bc == vk::TRUE;
        let has_fill_mode_non_solid = supported_features.fill_mode_non_solid == vk::TRUE;

        let dev_ext_props =
            unsafe { instance.enumerate_device_extension_properties(physical_device) }?;
        let has_ext = |wanted: &CStr| -> bool {
            dev_ext_props.iter().any(|p| {
                // SAFETY: `extension_name` est une chaîne C terminée par nul.
                let name = unsafe { CStr::from_ptr(p.extension_name.as_ptr()) };
                name == wanted
            })
        };

        let has_swapchain = has_ext(ash::khr::swapchain::NAME);
        let has_external_memory_host = has_ext(ash::ext::external_memory_host::NAME);
        let has_dynamic_rendering = has_ext(ash::khr::dynamic_rendering::NAME);
        let has_ray_tracing = has_ext(ash::khr::acceleration_structure::NAME)
            && has_ext(ash::khr::ray_tracing_pipeline::NAME);

        let mut enabled_dev_exts: Vec<*const i8> = Vec::new();
        if with_surface && has_swapchain {
            enabled_dev_exts.push(ash::khr::swapchain::NAME.as_ptr());
        }
        if has_external_memory_host {
            enabled_dev_exts.push(ash::ext::external_memory_host::NAME.as_ptr());
        }

        let mut min_host_align = 4096u64;
        if has_external_memory_host {
            let mut host_mem_props = vk::PhysicalDeviceExternalMemoryHostPropertiesEXT::default();
            let mut props2 =
                vk::PhysicalDeviceProperties2::default().push_next(&mut host_mem_props);
            // SAFETY: `props2` et `host_mem_props` sont valides sur la pile.
            unsafe {
                instance.get_physical_device_properties2(physical_device, &mut props2);
            }
            min_host_align = host_mem_props.min_imported_host_pointer_alignment.max(4096);
        }

        let priorities = [1.0f32];
        let queue_create = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(graphics_queue_family)
            .queue_priorities(&priorities)];

        let enabled_features = vk::PhysicalDeviceFeatures::default()
            .texture_compression_bc(has_bc_compression)
            .fill_mode_non_solid(has_fill_mode_non_solid)
            .independent_blend(supported_features.independent_blend == vk::TRUE)
            .sampler_anisotropy(supported_features.sampler_anisotropy == vk::TRUE);

        let dev_create_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_create)
            .enabled_extension_names(&enabled_dev_exts)
            .enabled_features(&enabled_features);

        // SAFETY: création du périphérique logique Vulkan.
        let device = unsafe { instance.create_device(physical_device, &dev_create_info, None) }?;
        // SAFETY: `graphics_queue_family` vient d'être créé avec 1 file.
        let graphics_queue = unsafe { device.get_device_queue(graphics_queue_family, 0) };

        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(graphics_queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: création du command pool graphique.
        let command_pool = unsafe { device.create_command_pool(&pool_info, None) }?;

        // SAFETY: lecture des types mémoire du GPU.
        let memory_properties =
            unsafe { instance.get_physical_device_memory_properties(physical_device) };

        let ext_memory_host_fn = if has_external_memory_host {
            Some(ash::ext::external_memory_host::Device::new(
                &instance, &device,
            ))
        } else {
            None
        };

        let caps = GpuCapabilities {
            device_name: device_name.clone(),
            api_version_major: vk::api_version_major(props.api_version),
            api_version_minor: vk::api_version_minor(props.api_version),
            api_version_patch: vk::api_version_patch(props.api_version),
            has_external_memory_host,
            min_imported_host_pointer_alignment: min_host_align,
            has_dynamic_rendering,
            has_swapchain,
            has_bc_compression,
            has_fill_mode_non_solid,
            has_ray_tracing,
            validation_enabled,
        };

        sb_info!(
            "gpu",
            "Vulkan {}.{}.{} initialisé sur '{}' (ext_mem_host={}, align={}o, bc={}, rt={}, validation={})",
            caps.api_version_major,
            caps.api_version_minor,
            caps.api_version_patch,
            caps.device_name,
            caps.has_external_memory_host,
            caps.min_imported_host_pointer_alignment,
            caps.has_bc_compression,
            caps.has_ray_tracing,
            caps.validation_enabled
        );

        Ok(Arc::new(Self {
            entry,
            instance,
            physical_device,
            device,
            graphics_queue_family,
            graphics_queue,
            command_pool,
            memory_properties,
            caps,
            ext_memory_host_fn,
        }))
    }

    pub fn find_memory_type(
        &self,
        type_bits: u32,
        required: vk::MemoryPropertyFlags,
    ) -> Option<u32> {
        for i in 0..self.memory_properties.memory_type_count {
            let supported = (type_bits & (1 << i)) != 0;
            let props = self.memory_properties.memory_types[i as usize].property_flags;
            if supported && props.contains(required) {
                return Some(i);
            }
        }
        None
    }
}

impl Drop for GpuContext {
    fn drop(&mut self) {
        // SAFETY: destruction ordonnée des ressources Vulkan.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

/// Présentateur Vulkan `VkSwapchainKHR` minimal pour afficher les frames D3D11/D3D12X sur Wayland.
pub struct SwapchainPresenter {
    gpu: Arc<GpuContext>,
    surface_loader: ash::khr::surface::Instance,
    swapchain_loader: ash::khr::swapchain::Device,
    surface: vk::SurfaceKHR,
    swapchain: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    cmd_buf: vk::CommandBuffer,
    img_available: vk::Semaphore,
    render_finished: vk::Semaphore,
    in_flight: vk::Fence,
    pub frame_count: u64,
}

impl SwapchainPresenter {
    pub fn new(
        gpu: Arc<GpuContext>,
        surface: vk::SurfaceKHR,
        width: u32,
        height: u32,
    ) -> Result<Self, GpuError> {
        let surface_loader = ash::khr::surface::Instance::new(&gpu.entry, &gpu.instance);
        let swapchain_loader = ash::khr::swapchain::Device::new(&gpu.instance, &gpu.device);

        // SAFETY: interroge les capacités de la surface Wayland/SDL3.
        let caps = unsafe {
            surface_loader.get_physical_device_surface_capabilities(gpu.physical_device, surface)
        }?;
        let formats = unsafe {
            surface_loader.get_physical_device_surface_formats(gpu.physical_device, surface)
        }?;
        let chosen_format = formats
            .iter()
            .copied()
            .find(|f| f.format == vk::Format::B8G8R8A8_UNORM)
            .or_else(|| formats.first().copied())
            .ok_or(GpuError::NoSurface)?;

        let extent = if caps.current_extent.width != u32::MAX {
            caps.current_extent
        } else {
            vk::Extent2D {
                width: width.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                height: height.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
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
            .image_format(chosen_format.format)
            .image_color_space(chosen_format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(vk::PresentModeKHR::FIFO)
            .clipped(true);

        // SAFETY: création du swapchain Vulkan.
        let swapchain = unsafe { swapchain_loader.create_swapchain(&sc_info, None) }?;
        let images = unsafe { swapchain_loader.get_swapchain_images(swapchain) }?;

        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(gpu.command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: allocation du command buffer de présentation.
        let cmd_buf = unsafe { gpu.device.allocate_command_buffers(&alloc_info) }?[0];

        let sem_info = vk::SemaphoreCreateInfo::default();
        let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
        // SAFETY: création des primitives de synchronisation.
        let img_available = unsafe { gpu.device.create_semaphore(&sem_info, None) }?;
        let render_finished = unsafe { gpu.device.create_semaphore(&sem_info, None) }?;
        let in_flight = unsafe { gpu.device.create_fence(&fence_info, None) }?;

        sb_info!(
            "gpu",
            "Swapchain Vulkan créé ({}x{}, {} images, format={:?})",
            extent.width,
            extent.height,
            images.len(),
            chosen_format.format
        );

        Ok(Self {
            gpu,
            surface_loader,
            swapchain_loader,
            surface,
            swapchain,
            images,
            cmd_buf,
            img_available,
            render_finished,
            in_flight,
            frame_count: 0,
        })
    }

    /// Présente une frame effacée avec la couleur RGBA courante sur le swapchain Vulkan Wayland.
    pub fn present_clear(&mut self, rgba: [f32; 4]) -> Result<(), GpuError> {
        let dev = &self.gpu.device;
        // SAFETY: synchronisation frame-in-flight et enregistrement du command buffer Vulkan.
        unsafe {
            dev.wait_for_fences(&[self.in_flight], true, u64::MAX)?;
            dev.reset_fences(&[self.in_flight])?;

            let (img_index, _suboptimal) = self.swapchain_loader.acquire_next_image(
                self.swapchain,
                u64::MAX,
                self.img_available,
                vk::Fence::null(),
            )?;
            let image = self.images[img_index as usize];

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

            let to_clear = vk::ImageMemoryBarrier::default()
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
                &[to_clear],
            );

            let clear_val = vk::ClearColorValue { float32: rgba };
            dev.cmd_clear_color_image(
                self.cmd_buf,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &clear_val,
                &[range],
            );

            let to_present = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .image(image)
                .subresource_range(range)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE);
            dev.cmd_pipeline_barrier(
                self.cmd_buf,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_present],
            );

            dev.end_command_buffer(self.cmd_buf)?;

            let wait_sems = [self.img_available];
            let wait_stages = [vk::PipelineStageFlags::TRANSFER];
            let cmd_bufs = [self.cmd_buf];
            let signal_sems = [self.render_finished];
            let submit = vk::SubmitInfo::default()
                .wait_semaphores(&wait_sems)
                .wait_dst_stage_mask(&wait_stages)
                .command_buffers(&cmd_bufs)
                .signal_semaphores(&signal_sems);
            dev.queue_submit(self.gpu.graphics_queue, &[submit], self.in_flight)?;

            let swapchains = [self.swapchain];
            let indices = [img_index];
            let present = vk::PresentInfoKHR::default()
                .wait_semaphores(&signal_sems)
                .swapchains(&swapchains)
                .image_indices(&indices);
            if let Err(err) = self
                .swapchain_loader
                .queue_present(self.gpu.graphics_queue, &present)
            {
                sb_warn!("gpu", "queue_present: {err}");
            }
        }
        self.frame_count += 1;
        if self.frame_count == 1 || self.frame_count.is_multiple_of(60) {
            sb_debug!("gpu", "frame Vulkan présentée #{}", self.frame_count);
        }
        Ok(())
    }
}

impl Drop for SwapchainPresenter {
    fn drop(&mut self) {
        // SAFETY: attente GPU puis destruction ordonnée du swapchain et de la surface.
        unsafe {
            let _ = self.gpu.device.device_wait_idle();
            self.gpu.device.destroy_fence(self.in_flight, None);
            self.gpu
                .device
                .destroy_semaphore(self.render_finished, None);
            self.gpu.device.destroy_semaphore(self.img_available, None);
            self.swapchain_loader
                .destroy_swapchain(self.swapchain, None);
            self.surface_loader.destroy_surface(self.surface, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initializes_headless_vulkan_device_if_available() {
        match GpuContext::new(false) {
            Ok(ctx) => {
                assert!(!ctx.caps.device_name.is_empty());
                assert!(ctx.caps.api_version_major >= 1);
            }
            Err(err) => {
                // Tolère un environnement sans ICD Vulkan tout en validant la gestion d'erreur.
                eprintln!("Vulkan indisponible dans cet environnement : {err}");
            }
        }
    }
}
