//! Tests d'exécution réelle sur GPU Vulkan (ignorés par défaut).
//!
//! `cargo test -p sb-gpu --lib rdna2::tests_gpu -- --ignored --nocapture`
//!
//! Les shaders de calcul sont traduits depuis leur code RDNA 2, exécutés sur le premier GPU
//! discret avec la taille de sous-groupe requise (32 pour wave32, 64 pour wave64) et leur sortie
//! est comparée à la valeur attendue calculée sur CPU. Si aucun GPU compatible n'est trouvé, le
//! test s'arrête avec un message.

use ash::vk;

use super::test_programs::PROGRAMS;
use super::*;

pub(super) struct Gpu {
    _entry: ash::Entry,
    pub(super) instance: ash::Instance,
    pub(super) pd: vk::PhysicalDevice,
    pub(super) dev: ash::Device,
    pub(super) queue: vk::Queue,
    pub(super) qfam: u32,
    pub(super) mem_props: vk::PhysicalDeviceMemoryProperties,
}

impl Gpu {
    pub(super) fn new() -> Option<Gpu> {
        // SAFETY: chargement dynamique de la bibliothèque Vulkan ; toutes les structures passées
        // aux appels ci-dessous vivent jusqu'à la fin de chaque appel.
        unsafe {
            let entry = ash::Entry::load().ok()?;
            let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
            let instance = entry
                .create_instance(
                    &vk::InstanceCreateInfo::default().application_info(&app),
                    None,
                )
                .ok()?;
            let pds = instance.enumerate_physical_devices().ok()?;
            let pd = pds
                .iter()
                .copied()
                .find(|&p| {
                    instance.get_physical_device_properties(p).device_type
                        == vk::PhysicalDeviceType::DISCRETE_GPU
                })
                .or_else(|| pds.first().copied())?;
            let qfam = instance
                .get_physical_device_queue_family_properties(pd)
                .iter()
                .position(|q| q.queue_flags.contains(vk::QueueFlags::COMPUTE))?
                as u32;
            let mut f13 = vk::PhysicalDeviceVulkan13Features::default().subgroup_size_control(true);
            let prio = [1.0f32];
            let qi = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(qfam)
                .queue_priorities(&prio)];
            let mut f12 = vk::PhysicalDeviceVulkan12Features::default().buffer_device_address(true);
            let feats = instance.get_physical_device_features(pd);
            let ci = vk::DeviceCreateInfo::default()
                .queue_create_infos(&qi)
                .enabled_features(&feats)
                .push_next(&mut f12)
                .push_next(&mut f13);
            let dev = instance.create_device(pd, &ci, None).ok()?;
            let queue = dev.get_device_queue(qfam, 0);
            let mem_props = instance.get_physical_device_memory_properties(pd);
            Some(Gpu {
                _entry: entry,
                instance,
                pd,
                dev,
                queue,
                qfam,
                mem_props,
            })
        }
    }

    /// Exécute un shader de calcul sur un buffer de `bytes` octets initialisé à zéro.
    fn run(&self, words: &[u32], subgroup: u32, groups: u32, bytes: u64) -> Option<Vec<u32>> {
        // SAFETY: ressources Vulkan créées puis détruites dans cette fonction ; les pointeurs
        // mappés ne sont lus qu'après `queue_wait_idle` et pendant que la mémoire est mappée.
        unsafe {
            let d = &self.dev;
            let buf = d
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(bytes)
                        .usage(vk::BufferUsageFlags::STORAGE_BUFFER),
                    None,
                )
                .ok()?;
            let req = d.get_buffer_memory_requirements(buf);
            let want =
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
            let ty = (0..self.mem_props.memory_type_count).find(|&i| {
                req.memory_type_bits & (1 << i) != 0
                    && self.mem_props.memory_types[i as usize]
                        .property_flags
                        .contains(want)
            })?;
            let mem = d
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(ty),
                    None,
                )
                .ok()?;
            d.bind_buffer_memory(buf, mem, 0).ok()?;
            let ptr = d
                .map_memory(mem, 0, bytes, vk::MemoryMapFlags::empty())
                .ok()? as *mut u8;
            std::ptr::write_bytes(ptr, 0, bytes as usize);

            let bind = [vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)];
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
            let sm = d
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(words), None)
                .ok()?;
            let mut rss = vk::PipelineShaderStageRequiredSubgroupSizeCreateInfo::default()
                .required_subgroup_size(subgroup);
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(sm)
                .name(c"main")
                .push_next(&mut rss);
            let pipe = d
                .create_compute_pipelines(
                    vk::PipelineCache::null(),
                    &[vk::ComputePipelineCreateInfo::default()
                        .stage(stage)
                        .layout(pl)],
                    None,
                )
                .ok()?[0];
            let sizes = [vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)];
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
            let bi = [vk::DescriptorBufferInfo::default()
                .buffer(buf)
                .offset(0)
                .range(vk::WHOLE_SIZE)];
            d.update_descriptor_sets(
                &[vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&bi)],
                &[],
            );
            let cp = d
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default().queue_family_index(self.qfam),
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
            d.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, pipe);
            d.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::COMPUTE, pl, 0, &[set], &[]);
            d.cmd_dispatch(cb, groups, 1, 1);
            d.end_command_buffer(cb).ok()?;
            let cbs = [cb];
            d.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default().command_buffers(&cbs)],
                vk::Fence::null(),
            )
            .ok()?;
            d.queue_wait_idle(self.queue).ok()?;
            let mut out = vec![0u32; (bytes / 4) as usize];
            std::ptr::copy_nonoverlapping(ptr as *const u32, out.as_mut_ptr(), out.len());

            d.destroy_command_pool(cp, None);
            d.destroy_descriptor_pool(pool, None);
            d.destroy_pipeline(pipe, None);
            d.destroy_shader_module(sm, None);
            d.destroy_pipeline_layout(pl, None);
            d.destroy_descriptor_set_layout(dsl, None);
            d.unmap_memory(mem);
            d.destroy_buffer(buf, None);
            d.free_memory(mem, None);
            Some(out)
        }
    }

    fn supports_subgroup(&self, size: u32) -> bool {
        // SAFETY: requête sur le périphérique physique valide `self.pd` de `self.instance`.
        unsafe {
            let mut sg = vk::PhysicalDeviceSubgroupSizeControlProperties::default();
            let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut sg);
            self.instance
                .get_physical_device_properties2(self.pd, &mut p2);
            sg.min_subgroup_size <= size
                && size <= sg.max_subgroup_size
                && sg
                    .required_subgroup_size_stages
                    .contains(vk::ShaderStageFlags::COMPUTE)
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: le périphérique et l'instance ne sont plus utilisés.
        unsafe {
            let _ = self.dev.device_wait_idle();
            self.dev.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

pub(super) fn program(name: &str) -> &'static super::test_programs::Program {
    PROGRAMS.iter().find(|p| p.name == name).unwrap()
}

fn cfg(p: &super::test_programs::Program) -> TranslateConfig {
    let wave = if p.wave == 64 {
        WaveSize::W64
    } else {
        WaveSize::W32
    };
    let mut c = TranslateConfig::new(Stage::Compute, wave);
    c.local_size = [64, 1, 1];
    c.sgprs = vec![(2, SgprInit::WorkgroupId(0))];
    c.vgprs = vec![(0, VgprInit::LocalInvocationId(0))];
    c.resources = vec![Resource {
        kind: ResourceKind::Buffer { stride: 0 },
        loc: ResourceLoc::Table {
            table_sgpr: 0,
            byte_offset: 0,
        },
        set: 0,
        binding: 0,
    }];
    c.lds_dwords = 64;
    c
}

fn run(gpu: &Gpu, name: &str, groups: u32, bytes: u64) -> Option<Vec<u32>> {
    let p = program(name);
    let t = translate_program(p.words, &cfg(p)).unwrap();
    if !gpu.supports_subgroup(u32::from(p.wave)) {
        eprintln!(
            "taille de sous-groupe {} non supportée : {name} ignoré",
            p.wave
        );
        return None;
    }
    let out = gpu.run(&t.words, u32::from(p.wave), groups, bytes);
    eprintln!(
        "{name} (wave{}) : {}",
        p.wave,
        if out.is_some() {
            "exécuté sur GPU"
        } else {
            "échec d'exécution"
        }
    );
    out
}

#[test]
#[ignore = "nécessite un GPU Vulkan"]
fn gpu_cs_programs_produce_expected_buffers() {
    let Some(gpu) = Gpu::new() else {
        eprintln!("aucun GPU Vulkan : test ignoré");
        return;
    };
    let f = f32::from_bits;

    if let Some(o) = run(&gpu, "cs_write_buffer", 2, 4096) {
        for i in 0..128u32 {
            assert_eq!(f(o[i as usize]), 2.0 * i as f32, "cs_write_buffer[{i}]");
        }
    }
    for name in ["cs_divergent_w32", "cs_divergent_w64"] {
        if let Some(o) = run(&gpu, name, 1, 4096) {
            for i in 0..64usize {
                assert_eq!(
                    o[i],
                    if i < 16 { 7 } else { 0 },
                    "{name}[{i}] (branche divergente)"
                );
                assert_eq!(o[64 + i], 9, "{name}[{}] (après reconvergence)", 64 + i);
            }
        }
    }
    if let Some(o) = run(&gpu, "cs_scalar_loop", 1, 4096) {
        assert!(
            o[..64].iter().all(|&v| v == 45),
            "somme 0..10 par boucle scalaire : {:?}",
            &o[..8]
        );
    }
    if let Some(o) = run(&gpu, "cs_divergent_loop", 1, 4096) {
        for (i, &v) in o[..64].iter().enumerate() {
            assert_eq!(v, 10 * (i as u32 & 3).max(1), "boucle divergente lane {i}");
        }
    }
    if let Some(o) = run(&gpu, "cs_lds_barrier", 1, 4096) {
        for (i, &v) in o[..64].iter().enumerate() {
            assert_eq!(v, 100 + 63 - i as u32, "LDS + barrière lane {i}");
        }
    }
    if let Some(o) = run(&gpu, "cs_valu_mods", 1, 8192) {
        for t in 0..64usize {
            let tf = t as f32;
            let v2 = (2.0 * (1.0 - tf * tf)).clamp(0.0, 1.0);
            let v3 = if 0.5 < v2 { 1.0 } else { 0.0 };
            assert_eq!(f(o[4 * t]), v2, "v2 lane {t}");
            assert_eq!(f(o[4 * t + 1]), v3, "v3 lane {t}");
            assert_eq!(o[4 * t + 2], 2 * t as u32, "v_add_co_u32 lane {t}");
            assert_eq!(o[4 * t + 3], 0, "carry-in lane {t}");
            let b = 256 + 4 * t;
            assert_eq!(o[b], 3 * t as u32 + 5, "v_mad_u32_u24 lane {t}");
            assert_eq!(o[b + 1], (t as u32 >> 1) & 7, "v_bfe_u32 lane {t}");
            assert_eq!(f(o[b + 2]), v2.min(v3), "v_min_f32 lane {t}");
            assert!(
                (f(o[b + 3]) - tf.sqrt()).abs() <= 1e-5 * tf.sqrt().max(1.0),
                "v_sqrt_f32 lane {t}"
            );
        }
    }
    if let Some(o) = run(&gpu, "cs_atomic_add", 1, 4096) {
        assert_eq!(o[0], 64, "somme atomique");
        let mut seen: Vec<u32> = o[1..65].to_vec();
        seen.sort_unstable();
        assert_eq!(
            seen,
            (0..64).collect::<Vec<u32>>(),
            "valeurs de retour uniques"
        );
    }
}

// ---------------------------------------------------------------------------------- batterie ALU

/// Résultat attendu d'un segment de la batterie.
enum Exp {
    Bits(u32),
    /// valeur, tolérance relative, tolérance absolue
    F(f32, f32, f32),
    /// deux demi-flottants empaquetés (bas, haut)
    Pk(f32, f32),
}

fn f16_to_f32(h: u32) -> f32 {
    let (s, e, m) = ((h >> 15) & 1, (h >> 10) & 0x1F, h & 0x3FF);
    let v = match e {
        0 => (m as f32) * 2f32.powi(-24),
        31 => f32::INFINITY,
        _ => (1.0 + m as f32 / 1024.0) * 2f32.powi(e as i32 - 15),
    };
    if s == 1 { -v } else { v }
}

/// Oracle CPU indépendant (sémantique du guide ISA RDNA 2) pour le segment `name` de la lane `t`.
fn oracle(name: &str, t: u32, wave: u32) -> Exp {
    use Exp::{Bits, F};
    let (a, b) = (
        t.wrapping_mul(0x9e37_79b1),
        t.wrapping_mul(0x9e37_79b1) ^ 0x5bd1_e995,
    );
    let c = t.wrapping_sub(20);
    let f5 = (t as i32 - 20) as f32 * f32::from_bits(0x3ebd_70a4);
    let f6 = 1.0 + t as f32 * f32::from_bits(0x3de1_47ae);
    let (s30, s31) = (0xffff_fff0u32, 0x123u32);
    let sext24 = |v: u32| (((v & 0xFF_FFFF) << 8) as i32) >> 8;
    match name {
        "v_add_nc_u32" => Bits(a.wrapping_add(b)),
        "v_sub_nc_u32" => Bits(a.wrapping_sub(b)),
        "v_subrev_nc_u32" => Bits(b.wrapping_sub(a)),
        "v_and_b32" => Bits(a & b),
        "v_or_b32" => Bits(a | b),
        "v_xor_b32" => Bits(a ^ b),
        "v_xnor_b32" => Bits(!(a ^ b)),
        "v_lshlrev_b32" => Bits(a << (b & 31)),
        "v_lshrrev_b32" => Bits(a >> (b & 31)),
        "v_ashrrev_i32" => Bits(((a as i32) >> (b & 31)) as u32),
        "v_mul_u32_u24" => Bits((a & 0xFF_FFFF).wrapping_mul(b & 0xFF_FFFF)),
        "v_mul_i32_i24" => Bits(sext24(a).wrapping_mul(sext24(b)) as u32),
        "v_min_i32" => Bits((a as i32).min(b as i32) as u32),
        "v_max_i32" => Bits((a as i32).max(b as i32) as u32),
        "v_min_u32" => Bits(a.min(b)),
        "v_max_u32" => Bits(a.max(b)),
        "v_mul_hi_u32" => Bits(((u64::from(a) * u64::from(b)) >> 32) as u32),
        "v_mul_hi_i32" => Bits(((i64::from(a as i32) * i64::from(b as i32)) >> 32) as u32),
        "v_mul_lo_u32" => Bits(a.wrapping_mul(b)),
        "v_alignbit_b32" => Bits((((u64::from(a) << 32) | u64::from(b)) >> (c & 31)) as u32),
        "v_bfi_b32" => Bits((a & b) | (!a & c)),
        "v_med3_i32" => {
            let mut v = [a as i32, b as i32, c as i32];
            v.sort_unstable();
            Bits(v[1] as u32)
        }
        "v_lshl_add_u32" => Bits((a << (c & 31)).wrapping_add(b)),
        "v_not_b32" => Bits(!a),
        "v_bfrev_b32" => Bits(a.reverse_bits()),
        "v_ffbh_u32" => Bits(if a == 0 { u32::MAX } else { a.leading_zeros() }),
        "v_bfe_u32" => Bits((a >> 7) & 0x1FF),
        "v_bfe_i32" => Bits(((((a >> 3) & 0x1FFF) << 19) as i32 >> 19) as u32),
        "v_add3_u32" => Bits(a.wrapping_add(b).wrapping_add(c)),
        "v_and_or_b32" => Bits((a & b) | c),
        "v_mad_u32_u24" => Bits((a & 0xFF_FFFF).wrapping_mul(b & 0xFF_FFFF).wrapping_add(c)),
        "v_mad_i32_i24" => Bits(sext24(a).wrapping_mul(sext24(b)).wrapping_add(c as i32) as u32),
        "v_add_f32" => Bits((0.5 + f5).to_bits()),
        "v_sub_f32" => Bits((f5 - f6).to_bits()),
        "v_mul_f32" => Bits((f5 * f6).to_bits()),
        "v_min_f32" => Bits(f5.min(f6).to_bits()),
        "v_max_f32" => Bits(f5.max(f6).to_bits()),
        "v_fma_f32" => F(f5.mul_add(f6, f5), 1e-6, 1e-6),
        "v_floor_f32" => Bits(f5.floor().to_bits()),
        "v_ceil_f32" => Bits(f5.ceil().to_bits()),
        "v_trunc_f32" => Bits(f5.trunc().to_bits()),
        "v_fract_f32" => F(f5 - f5.floor(), 1e-6, 1e-6),
        "v_rndne_f32" => Bits(f5.round_ties_even().to_bits()),
        "v_cvt_i32_f32" => Bits(f5 as i32 as u32),
        "v_cvt_u32_f32" => Bits(f6 as u32),
        "v_rcp_f32" => F(1.0 / f6, 2e-6, 0.0),
        "v_rsq_f32" => F(1.0 / f6.sqrt(), 2e-6, 0.0),
        "v_sqrt_f32" => F(f6.sqrt(), 2e-6, 0.0),
        "v_exp_f32" => F(f5.exp2(), 2e-5, 0.0),
        "v_log_f32" => F(f6.log2(), 1e-5, 2e-5),
        "v_sin_f32" => F((f6 * std::f32::consts::TAU).sin(), 0.0, 2e-3),
        "v_cvt_f16_f32_roundtrip" => F(f5, 1e-3, 1e-6),
        "v_cvt_pkrtz_f16_f32" => Exp::Pk(f5, f6),
        "v_cndmask_lt_i32" => Bits(if (c as i32) < 0 { b } else { a }),
        "v_cndmask_gt_u32" => Bits(if a > b { b } else { a }),
        "v_cndmask_ge_f32" => Bits(if f5 >= f6 { b } else { a }),
        "v_add_co_u32_sum" => Bits(a.wrapping_add(b)),
        "v_add_co_u32_carry" => Bits(u32::from(a.checked_add(b).is_none())),
        "v_sub_co_ci_u32" => Bits(
            b.wrapping_sub(a)
                .wrapping_sub(u32::from(a.checked_add(b).is_none())),
        ),
        "s_add_u32" => Bits(s30.wrapping_add(s31)),
        "s_add_u32_scc" => Bits(u32::from(s30.checked_add(s31).is_none())),
        "s_sub_u32_scc" => Bits(if s31 < s30 { 0x100 } else { 7 }),
        "s_mul_i32" => Bits(s30.wrapping_mul(s31)),
        "s_bfe_u32" => Bits((s30 >> 4) & 0xFF),
        "s_lshl_b32" => Bits(s31 << 5),
        "s_ashr_i32" => Bits(((s30 as i32) >> 2) as u32),
        "s_min_i32" => Bits((s30 as i32).min(s31 as i32) as u32),
        "s_max_u32" => Bits(s30.max(s31)),
        "s_cmp_lt_i32_cselect" => Bits(if (s30 as i32) < (s31 as i32) { 11 } else { 22 }),
        "s_addc_u32" => Bits(
            s31.wrapping_add(5)
                .wrapping_add(u32::from(s30.checked_add(s30).is_none())),
        ),
        "s_bcnt1_i32_b32" => Bits(s30.count_ones()),
        "s_ff1_i32_b32" => Bits(s30.trailing_zeros()),
        "s_flbit_i32_b32" => Bits(s31.leading_zeros()),
        "s_brev_b32" => Bits(s31.reverse_bits()),
        "s_abs_i32" => Bits((s30 as i32).unsigned_abs()),
        "s_sext_i32_i16" => Bits(0x1fff8u32 as u16 as i16 as i32 as u32),
        "s_pack_ll_b32_b16" => Bits((s30 & 0xFFFF) | (s31 << 16)),
        "s_absdiff_i32" => Bits((s30 as i32).abs_diff(s31 as i32)),
        "s_bfm_b32" => Bits(((1u32 << 7) - 1) << 9),
        "s_mul_hi_u32" => Bits(((u64::from(s30) * u64::from(s30)) >> 32) as u32),
        "s_andn2_b32" => Bits(s30 & !s31),
        "s_xnor_b32" => Bits(!(s30 ^ s31)),
        "s_movk_addk_mulk" => Bits(((-16i32 + 0x20) * 7) as u32),
        "v_readfirstlane_b32" => {
            // première lane active *de la vague* de `t`
            let first = t - t % wave;
            Bits(first.wrapping_mul(0x9e37_79b1) ^ 0x5bd1_e995)
        }
        other => panic!("pas d'oracle pour le segment {other}"),
    }
}

fn check(name: &str, t: u32, got: u32, exp: &Exp, prog: &str) {
    match *exp {
        Exp::Bits(v) => assert_eq!(got, v, "{prog}/{name} lane {t} : {got:#x} != {v:#x}"),
        Exp::F(v, rel, abs) => {
            let g = f32::from_bits(got);
            assert!(
                (g - v).abs() <= rel * v.abs() + abs,
                "{prog}/{name} lane {t} : {g} != {v}"
            );
        }
        Exp::Pk(lo, hi) => {
            let (gl, gh) = (f16_to_f32(got & 0xFFFF), f16_to_f32(got >> 16));
            assert!(
                (gl - lo).abs() <= 1e-3 * lo.abs() + 1e-5,
                "{prog}/{name} lane {t} bas : {gl} != {lo}"
            );
            assert!(
                (gh - hi).abs() <= 1e-3 * hi.abs() + 1e-5,
                "{prog}/{name} lane {t} haut : {gh} != {hi}"
            );
        }
    }
}

#[test]
#[ignore = "nécessite un GPU Vulkan"]
fn gpu_alu_battery_matches_cpu_oracle() {
    let Some(gpu) = Gpu::new() else {
        eprintln!("aucun GPU Vulkan : test ignoré");
        return;
    };
    for prog in ["cs_alu_battery_w32", "cs_alu_battery_w64"] {
        let p = program(prog);
        let segs: Vec<&str> = p
            .asm
            .lines()
            .filter_map(|l| l.strip_prefix("; seg "))
            .collect();
        let Some(out) = run(&gpu, prog, 1, 256 * segs.len() as u64 + 256) else {
            continue;
        };
        let mut checked = 0usize;
        for (k, name) in segs.iter().enumerate() {
            for t in 0..64u32 {
                let got = out[k * 64 + t as usize];
                check(name, t, got, &oracle(name, t, u32::from(p.wave)), prog);
                checked += 1;
            }
        }
        eprintln!(
            "{prog} : {} segments x 64 lanes vérifiés ({checked} valeurs)",
            segs.len()
        );
    }
}
