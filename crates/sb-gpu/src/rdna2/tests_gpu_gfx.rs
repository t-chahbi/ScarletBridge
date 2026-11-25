//! Rendu réel VS + PS RDNA 2 traduits (ignoré par défaut) : un triangle plein écran est rendu
//! dans une image 4x4 puis relue et comparée aux pixels attendus.
//!
//! `cargo test -p sb-gpu --lib rdna2::tests_gpu_gfx -- --ignored --nocapture`

use ash::vk;

use super::tests_gpu::{Gpu, program};
use super::*;

fn mem_type(g: &Gpu, bits: u32, want: vk::MemoryPropertyFlags) -> Option<u32> {
    (0..g.mem_props.memory_type_count).find(|&i| {
        bits & (1 << i) != 0
            && g.mem_props.memory_types[i as usize]
                .property_flags
                .contains(want)
    })
}

const HOST: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::from_raw(
    vk::MemoryPropertyFlags::HOST_VISIBLE.as_raw()
        | vk::MemoryPropertyFlags::HOST_COHERENT.as_raw(),
);

fn buffer_table(binding: u32, byte_offset: u32, kind: ResourceKind) -> Resource {
    Resource {
        kind,
        loc: ResourceLoc::Table {
            table_sgpr: 0,
            byte_offset,
        },
        set: 0,
        binding,
    }
}

/// Rend `vs` + `ps` ; `textured` active le T#/S# du PS et les UV du VS. Retourne les 16 pixels RGBA8.
fn render(g: &Gpu, vs: &str, ps: &str, textured: bool) -> Option<Vec<[u8; 4]>> {
    let (vs_p, ps_p) = (program(vs), program(ps));
    let mut vcfg = TranslateConfig::new(Stage::Vertex, WaveSize::W32);
    vcfg.vgprs = vec![(0, VgprInit::VertexId)];
    vcfg.resources = vec![buffer_table(2, 0, ResourceKind::Buffer { stride: 0 })];
    let mut pcfg = TranslateConfig::new(Stage::Fragment, WaveSize::W32);
    if textured {
        pcfg.resources = vec![
            buffer_table(0, 0, ResourceKind::Image),
            buffer_table(1, 0x20, ResourceKind::Sampler),
        ];
    }
    let vs_spv = translate_program(vs_p.words, &vcfg).ok()?.words;
    let ps_spv = translate_program(ps_p.words, &pcfg).ok()?.words;

    // SAFETY: tous les objets Vulkan sont créés puis détruits dans cette fonction, sur le
    // périphérique `g.dev` ; les pointeurs mappés ne sont accédés que pendant le mapping et
    // après `queue_wait_idle`.
    unsafe {
        let d = &g.dev;
        // --- sommets : position (+ uv) ---
        let stride = if textured { 32usize } else { 16 };
        let verts: [[f32; 8]; 3] = [
            [-1.0, -1.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            [3.0, -1.0, 0.0, 1.0, 2.0, 0.0, 0.0, 1.0],
            [-1.0, 3.0, 0.0, 1.0, 0.0, 2.0, 0.0, 1.0],
        ];
        let vb = d
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(256)
                    .usage(vk::BufferUsageFlags::STORAGE_BUFFER),
                None,
            )
            .ok()?;
        let req = d.get_buffer_memory_requirements(vb);
        let vmem = d
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(mem_type(g, req.memory_type_bits, HOST)?),
                None,
            )
            .ok()?;
        d.bind_buffer_memory(vb, vmem, 0).ok()?;
        let p = d
            .map_memory(vmem, 0, 256, vk::MemoryMapFlags::empty())
            .ok()? as *mut f32;
        for (i, v) in verts.iter().enumerate() {
            let n = stride / 4;
            std::ptr::copy_nonoverlapping(v.as_ptr(), p.add(i * n), n);
        }
        d.unmap_memory(vmem);

        // --- image cible 4x4 + buffer de relecture ---
        let fmt = vk::Format::R8G8B8A8_UNORM;
        let target = d
            .create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(fmt)
                    .extent(vk::Extent3D {
                        width: 4,
                        height: 4,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(
                        vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
                    )
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
            .ok()?;
        let req = d.get_image_memory_requirements(target);
        let tmem = d
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(mem_type(
                        g,
                        req.memory_type_bits,
                        vk::MemoryPropertyFlags::DEVICE_LOCAL,
                    )?),
                None,
            )
            .ok()?;
        d.bind_image_memory(target, tmem, 0).ok()?;
        let tview = d
            .create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(target)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(fmt)
                    .subresource_range(color_range()),
                None,
            )
            .ok()?;
        let rb = d
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(64)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                None,
            )
            .ok()?;
        let req = d.get_buffer_memory_requirements(rb);
        let rmem = d
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(mem_type(g, req.memory_type_bits, HOST)?),
                None,
            )
            .ok()?;
        d.bind_buffer_memory(rb, rmem, 0).ok()?;

        // --- texture 2x2 (tiling linéaire, remplie par l'hôte) ---
        let tex = d
            .create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(fmt)
                    .extent(vk::Extent3D {
                        width: 2,
                        height: 2,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::LINEAR)
                    .usage(vk::ImageUsageFlags::SAMPLED)
                    .initial_layout(vk::ImageLayout::PREINITIALIZED),
                None,
            )
            .ok()?;
        let req = d.get_image_memory_requirements(tex);
        let xmem = d
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(mem_type(g, req.memory_type_bits, HOST)?),
                None,
            )
            .ok()?;
        d.bind_image_memory(tex, xmem, 0).ok()?;
        let pitch = d
            .get_image_subresource_layout(
                tex,
                vk::ImageSubresource {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    array_layer: 0,
                },
            )
            .row_pitch as usize;
        let texels: [[u8; 4]; 4] = [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
        ];
        let xp = d
            .map_memory(xmem, 0, req.size, vk::MemoryMapFlags::empty())
            .ok()? as *mut u8;
        for (i, t) in texels.iter().enumerate() {
            std::ptr::copy_nonoverlapping(t.as_ptr(), xp.add((i / 2) * pitch + (i % 2) * 4), 4);
        }
        d.unmap_memory(xmem);
        let xview = d
            .create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(tex)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(fmt)
                    .subresource_range(color_range()),
                None,
            )
            .ok()?;
        let sampler = d
            .create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::NEAREST)
                    .min_filter(vk::Filter::NEAREST)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )
            .ok()?;

        // --- descripteurs ---
        let bind = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::VERTEX),
        ];
        let dsl = d
            .create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bind),
                None,
            )
            .ok()?;
        let layouts = [dsl];
        let pl = d
            .create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts),
                None,
            )
            .ok()?;
        let sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(1),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1),
        ];
        let pool = d
            .create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&sizes),
                None,
            )
            .ok()?;
        let set = d
            .allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts),
            )
            .ok()?[0];
        let ii = [vk::DescriptorImageInfo::default()
            .image_view(xview)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let si = [vk::DescriptorImageInfo::default().sampler(sampler)];
        let bi = [vk::DescriptorBufferInfo::default()
            .buffer(vb)
            .range(vk::WHOLE_SIZE)];
        d.update_descriptor_sets(
            &[
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(&ii),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .image_info(&si),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(2)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&bi),
            ],
            &[],
        );

        // --- pipeline graphique (rendu dynamique) ---
        let vsm = d
            .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&vs_spv), None)
            .ok()?;
        let fsm = d
            .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&ps_spv), None)
            .ok()?;
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vsm)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(fsm)
                .name(c"main"),
        ];
        let vi = vk::PipelineVertexInputStateCreateInfo::default();
        let ia = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let vp = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: 4.0,
            height: 4.0,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let area = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 4,
                height: 4,
            },
        };
        let sc = [area];
        let vps = vk::PipelineViewportStateCreateInfo::default()
            .viewports(&vp)
            .scissors(&sc);
        let rs = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .line_width(1.0);
        let ms = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let cba = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let cbs = vk::PipelineColorBlendStateCreateInfo::default().attachments(&cba);
        let fmts = [fmt];
        let mut rend = vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&fmts);
        let gp = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vi)
            .input_assembly_state(&ia)
            .viewport_state(&vps)
            .rasterization_state(&rs)
            .multisample_state(&ms)
            .color_blend_state(&cbs)
            .layout(pl)
            .push_next(&mut rend);
        let pipe = d
            .create_graphics_pipelines(vk::PipelineCache::null(), &[gp], None)
            .map_err(|(_, e)| e)
            .ok()?[0];

        // --- enregistrement ---
        let cp = d
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(g.qfam),
                None,
            )
            .ok()?;
        let cb = d
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(cp)
                    .command_buffer_count(1),
            )
            .ok()?[0];
        d.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default())
            .ok()?;
        let barrier = |img: vk::Image, old, new, sa, da, ss, ds| {
            let b = [vk::ImageMemoryBarrier::default()
                .image(img)
                .old_layout(old)
                .new_layout(new)
                .src_access_mask(sa)
                .dst_access_mask(da)
                .subresource_range(color_range())];
            d.cmd_pipeline_barrier(cb, ss, ds, vk::DependencyFlags::empty(), &[], &[], &b);
        };
        barrier(
            tex,
            vk::ImageLayout::PREINITIALIZED,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::AccessFlags::HOST_WRITE,
            vk::AccessFlags::SHADER_READ,
            vk::PipelineStageFlags::HOST,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
        );
        barrier(
            target,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::AccessFlags::empty(),
            vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
        );
        let att = [vk::RenderingAttachmentInfo::default()
            .image_view(tview)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue { float32: [0.0; 4] },
            })];
        d.cmd_begin_rendering(
            cb,
            &vk::RenderingInfo::default()
                .render_area(area)
                .layer_count(1)
                .color_attachments(&att),
        );
        d.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipe);
        d.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::GRAPHICS, pl, 0, &[set], &[]);
        d.cmd_draw(cb, 3, 1, 0, 0);
        d.cmd_end_rendering(cb);
        barrier(
            target,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            vk::AccessFlags::TRANSFER_READ,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::PipelineStageFlags::TRANSFER,
        );
        let region = [vk::BufferImageCopy::default()
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_extent(vk::Extent3D {
                width: 4,
                height: 4,
                depth: 1,
            })];
        d.cmd_copy_image_to_buffer(
            cb,
            target,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            rb,
            &region,
        );
        d.end_command_buffer(cb).ok()?;
        let cbs = [cb];
        d.queue_submit(
            g.queue,
            &[vk::SubmitInfo::default().command_buffers(&cbs)],
            vk::Fence::null(),
        )
        .ok()?;
        d.queue_wait_idle(g.queue).ok()?;

        let rp = d
            .map_memory(rmem, 0, 64, vk::MemoryMapFlags::empty())
            .ok()? as *const u8;
        let mut raw = [0u8; 64];
        std::ptr::copy_nonoverlapping(rp, raw.as_mut_ptr(), 64);
        d.unmap_memory(rmem);

        d.destroy_command_pool(cp, None);
        d.destroy_pipeline(pipe, None);
        d.destroy_shader_module(vsm, None);
        d.destroy_shader_module(fsm, None);
        d.destroy_descriptor_pool(pool, None);
        d.destroy_pipeline_layout(pl, None);
        d.destroy_descriptor_set_layout(dsl, None);
        d.destroy_sampler(sampler, None);
        d.destroy_image_view(xview, None);
        d.destroy_image(tex, None);
        d.free_memory(xmem, None);
        d.destroy_buffer(rb, None);
        d.free_memory(rmem, None);
        d.destroy_image_view(tview, None);
        d.destroy_image(target, None);
        d.free_memory(tmem, None);
        d.destroy_buffer(vb, None);
        d.free_memory(vmem, None);
        Some(raw.as_chunks::<4>().0.to_vec())
    }
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

#[test]
#[ignore = "nécessite un GPU Vulkan"]
fn gpu_graphics_vs_ps_render_expected_pixels() {
    let Some(gpu) = Gpu::new() else {
        eprintln!("aucun GPU Vulkan : test ignoré");
        return;
    };
    // 1. VS passe-plat + PS couleur constante (1.0, 0.5, 0.0, 1.0)
    let px =
        render(&gpu, "vs_passthrough", "ps_const_color", false).expect("rendu couleur constante");
    for (i, p) in px.iter().enumerate() {
        assert_eq!(p[0], 255, "pixel {i} R");
        assert!((i32::from(p[1]) - 128).abs() <= 1, "pixel {i} G = {}", p[1]);
        assert_eq!((p[2], p[3]), (0, 255), "pixel {i} B/A");
    }
    eprintln!("VS passe-plat + PS couleur constante : 16 pixels corrects");
    // 2. VS (UV) + PS texturé (interpolation v_interp_* + image_sample + export compressé)
    let px = render(&gpu, "vs_uv_passthrough", "ps_texture", true).expect("rendu texturé");
    let texels: [[u8; 4]; 4] = [
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 255, 255],
    ];
    for (i, p) in px.iter().enumerate() {
        let (x, y) = (i % 4, i / 4);
        let want = texels[(y / 2) * 2 + x / 2];
        for c in 0..4 {
            assert!(
                (i32::from(p[c]) - i32::from(want[c])).abs() <= 1,
                "pixel ({x},{y}) canal {c} : {p:?} != {want:?}"
            );
        }
    }
    eprintln!("VS (UV) + PS texturé (interp + image_sample + exp compr) : 16 pixels corrects");
}
