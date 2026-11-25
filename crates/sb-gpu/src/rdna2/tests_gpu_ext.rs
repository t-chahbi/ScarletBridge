//! Tests GPU supplémentaires (ignorés par défaut) : accès mémoire 64 bits, f16, packed, DS,
//! sous-groupes, DPP/SDWA, f64, contrôle de bornes.
//! `cargo test -p sb-gpu --lib rdna2::tests_gpu_ext -- --ignored --nocapture`

use ash::vk;

use super::test_programs::PROGRAMS;
use super::tests_gpu::Gpu;
use super::*;

/// Buffer de stockage lié au binding d'indice identique (set 0).
pub(super) struct BufSpec {
    pub words: Vec<u32>,
    /// Adresse GPU (`bufferDeviceAddress`) requise.
    pub bda: bool,
}

impl BufSpec {
    pub fn zero(words: usize) -> Self {
        BufSpec {
            words: vec![0; words],
            bda: false,
        }
    }
}

impl Gpu {
    /// Exécute un shader de calcul avec plusieurs buffers (binding i = buffer i).
    /// `patch` : `(buffer_cible, mot, buffer_source)` écrit l'adresse GPU de `buffer_source`
    /// (2 mots) dans `buffer_cible` avant l'exécution.
    pub(super) fn run_bufs(
        &self,
        spv: &[u32],
        subgroup: u32,
        groups: u32,
        bufs: &[BufSpec],
        patch: &[(usize, usize, usize)],
    ) -> Option<Vec<Vec<u32>>> {
        // SAFETY: ressources créées puis détruites ici ; accès aux pointeurs mappés pendant le
        // mapping seulement, lecture après `queue_wait_idle`.
        unsafe {
            let d = &self.dev;
            let mut objs = Vec::new();
            for b in bufs {
                let bytes = (b.words.len() * 4) as u64;
                let mut usage = vk::BufferUsageFlags::STORAGE_BUFFER;
                if b.bda {
                    usage |= vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS;
                }
                let buf = d
                    .create_buffer(
                        &vk::BufferCreateInfo::default().size(bytes).usage(usage),
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
                let mut flags = vk::MemoryAllocateFlagsInfo::default()
                    .flags(vk::MemoryAllocateFlags::DEVICE_ADDRESS);
                let mut ai = vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(ty);
                if b.bda {
                    ai = ai.push_next(&mut flags);
                }
                let mem = d.allocate_memory(&ai, None).ok()?;
                d.bind_buffer_memory(buf, mem, 0).ok()?;
                let ptr = d
                    .map_memory(mem, 0, bytes, vk::MemoryMapFlags::empty())
                    .ok()? as *mut u32;
                std::ptr::copy_nonoverlapping(b.words.as_ptr(), ptr, b.words.len());
                objs.push((buf, mem, ptr));
            }
            for &(dst, word, src) in patch {
                let addr = d.get_buffer_device_address(
                    &vk::BufferDeviceAddressInfo::default().buffer(objs[src].0),
                );
                *objs[dst].2.add(word) = addr as u32;
                *objs[dst].2.add(word + 1) = (addr >> 32) as u32;
            }
            let binds: Vec<_> = (0..bufs.len() as u32)
                .map(|i| {
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(i)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::COMPUTE)
                })
                .collect();
            let dsl = d
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&binds),
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
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(spv), None)
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
                .descriptor_count(bufs.len() as u32)];
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
            let infos: Vec<[vk::DescriptorBufferInfo; 1]> = objs
                .iter()
                .map(|o| {
                    [vk::DescriptorBufferInfo::default()
                        .buffer(o.0)
                        .range(vk::WHOLE_SIZE)]
                })
                .collect();
            let writes: Vec<_> = infos
                .iter()
                .enumerate()
                .map(|(i, inf)| {
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(i as u32)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(inf)
                })
                .collect();
            d.update_descriptor_sets(&writes, &[]);
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
            let mut out = Vec::new();
            for (b, o) in bufs.iter().zip(&objs) {
                let mut v = vec![0u32; b.words.len()];
                std::ptr::copy_nonoverlapping(o.2 as *const u32, v.as_mut_ptr(), v.len());
                out.push(v);
            }
            d.destroy_command_pool(cp, None);
            d.destroy_descriptor_pool(pool, None);
            d.destroy_pipeline(pipe, None);
            d.destroy_shader_module(sm, None);
            d.destroy_pipeline_layout(pl, None);
            d.destroy_descriptor_set_layout(dsl, None);
            for (buf, mem, _) in objs {
                d.unmap_memory(mem);
                d.destroy_buffer(buf, None);
                d.free_memory(mem, None);
            }
            Some(out)
        }
    }
}

pub(super) fn prog(name: &str) -> &'static super::test_programs::Program {
    PROGRAMS.iter().find(|p| p.name == name).unwrap()
}

/// Traduit `name` (CS 64 threads) avec `cfg_fn` et l'exécute sur `bufs`.
pub(super) fn run_named(
    gpu: &Gpu,
    name: &str,
    cfg: TranslateConfig,
    bufs: &[BufSpec],
    patch: &[(usize, usize, usize)],
) -> Option<Vec<Vec<u32>>> {
    let p = prog(name);
    let t = translate_program(p.words, &cfg).unwrap_or_else(|e| panic!("{name}: {e}"));
    let out = gpu.run_bufs(&t.words, u32::from(p.wave), 1, bufs, patch);
    eprintln!(
        "{name} (wave{}) : {}",
        p.wave,
        if out.is_some() {
            "exécuté sur GPU"
        } else {
            "échec"
        }
    );
    out
}

pub(super) fn cs_cfg(wave: WaveSize) -> TranslateConfig {
    let mut c = TranslateConfig::new(Stage::Compute, wave);
    c.local_size = [64, 1, 1];
    c.vgprs = vec![(0, VgprInit::LocalInvocationId(0))];
    c
}

#[test]
#[ignore = "nécessite un GPU Vulkan"]
fn gpu_global_memory_64bit_addresses() {
    let Some(gpu) = Gpu::new() else {
        eprintln!("aucun GPU Vulkan : test ignoré");
        return;
    };
    let mut cfg = cs_cfg(WaveSize::W32);
    cfg.user_data_binding = Some((0, 1));
    cfg.sgprs = vec![(2, SgprInit::UserData(0)), (3, SgprInit::UserData(1))];
    // binding 0 : cible (adressable), binding 1 : pointeur
    let bufs = [
        BufSpec {
            words: vec![0; 1024],
            bda: true,
        },
        BufSpec::zero(4),
    ];
    let Some(o) = run_named(&gpu, "cs_global_mem", cfg, &bufs, &[(1, 0, 0)]) else {
        return;
    };
    let o = &o[0];
    for t in 0..64usize {
        assert_eq!(o[t], t as u32 + 7, "global_store_dword lane {t}");
        assert_eq!(o[64 + t], t as u32 + 8, "load -> +1 -> store lane {t}");
        // global_store_byte (RMW atomique) puis ubyte + sbyte : (b) + sext(b)
        let b = (0xab + t as u32) & 0xFF;
        let sx = (b as u8 as i8 as i32) as u32;
        assert_eq!(o[256 + t], b.wrapping_add(sx), "octets lane {t}");
        assert_eq!(o[384 + t], t as u32 + 7, "adresse 64 bits en VGPR lane {t}");
    }
    assert_eq!(o[450], 64, "atomique 64 lanes");
    let mut old: Vec<u32> = o[512..576].to_vec();
    old.sort_unstable();
    assert_eq!(
        old,
        (0..64).collect::<Vec<u32>>(),
        "valeurs de retour atomiques uniques"
    );
}

// ---------------------------------------------------------------------------- batterie étendue

enum E {
    Bits(u32),
    F(f32, f32, f32),
}

/// f32 -> bits f16 (arrondi au plus proche pair), domaine normal uniquement.
fn h16(x: f32) -> u32 {
    if x == 0.0 {
        return if x.is_sign_negative() { 0x8000 } else { 0 };
    }
    let b = x.to_bits();
    let (s, e, m) = (
        (b >> 31) << 15,
        ((b >> 23) & 0xFF) as i32 - 127 + 15,
        b & 0x7F_FFFF,
    );
    let mut h = ((e as u32) << 10) | (m >> 13);
    let (rem, half) = (m & 0x1FFF, 0x1000);
    if rem > half || (rem == half && h & 1 == 1) {
        h += 1;
    }
    s | h
}

fn oracle2(name: &str, t: u32) -> E {
    use E::{Bits, F};
    let (a_, b_) = (t & 15, ((t >> 2) & 7) + 1);
    let (a, b) = (a_ as f32, b_ as f32);
    let ha = t.wrapping_mul(0x9e37_79b1);
    let hb = ha ^ 0x5bd1_e995;
    let f5 = (t as i32 - 20) as f32 * f32::from_bits(0x3ebd_70a4);
    let f6 = 1.0 + t as f32 * f32::from_bits(0x3de1_47ae);
    let pk = |lo: u32, hi: u32| (lo & 0xFFFF) | (hi << 16);
    let sel = |c: bool| Bits(if c { hb } else { ha });
    let bytes = |v: u32, k: u32| (v >> (8 * k)) & 0xFF;
    let c64 = u64::from(ha) | (u64::from(hb) << 32);
    match name {
        "v_add_f16" => Bits(h16(a + b)),
        "v_sub_f16" => Bits(h16(a - b)),
        "v_mul_f16" => Bits(h16(a * b)),
        "v_max_f16" => Bits(h16(a.max(b))),
        "v_min_f16" => Bits(h16(a.min(b))),
        "v_fma_f16" => Bits(h16(a * b + a)),
        "v_cvt_u16_f16" => Bits(b_),
        "v_pk_add_f16" => Bits(pk(h16(a + b), h16(a + b))),
        "v_pk_sub_f16_neg" => Bits(pk(h16(a - b), h16(a + b))),
        "v_pk_mul_f16" => Bits(pk(h16(a * b), h16(a * b))),
        "v_pk_add_f16_opsel" => Bits(pk(h16(2.0 * b), h16(a + b))),
        "v_pk_add_u16" => Bits(pk(a_ + b_, a_ + b_)),
        "v_pk_mul_lo_u16" => Bits(pk(a_ * b_, a_ * b_)),
        "v_pk_sub_i16" => Bits(pk(a_.wrapping_sub(b_), b_.wrapping_sub(a_))),
        "v_pk_max_u16" => Bits(pk(a_.max(b_), a_.max(b_))),
        "v_pk_lshlrev_b16" => Bits(pk(b_ << (a_ & 15), a_ << (b_ & 15))),
        "v_pk_mad_u16" => Bits(pk(a_ * b_ + a_, b_ * a_ + b_)),
        "v_dot2_f32_f16" => Bits((2.0 * a * b + 1.0).to_bits()),
        "v_dot2_u32_u16" => Bits(2 * a_ * b_ + 5),
        "v_dot4_u32_u8" => {
            Bits((0..4).fold(7u32, |s, k| s.wrapping_add(bytes(ha, k) * bytes(hb, k))))
        }
        "v_dot4_i32_i8" => Bits((0..4).fold(7i32, |s, k| {
            s.wrapping_add(
                i32::from(bytes(ha, k) as u8 as i8) * i32::from(bytes(hb, k) as u8 as i8),
            )
        }) as u32),
        "v_fma_mix_f32" => F(a.mul_add(f6, 1.0), 1e-6, 1e-6),
        "v_add_nc_u16" => Bits((a_ + b_) & 0xFFFF),
        "v_mul_lo_u16" => Bits((a_ * b_) & 0xFFFF),
        "v_max_i16" => Bits(a_.max(b_)),
        "v_lshlrev_b16" => Bits((a_ << (b_ & 15)) & 0xFFFF),
        "v_mad_u16" => Bits((a_ * b_ + a_) & 0xFFFF),
        "v_cmp_class_neg_normal" => sel(t < 20),
        "v_cmp_class_zero" => sel(t == 20),
        "v_cmp_lt_f16" => sel(a < b),
        "v_cmp_gt_u16" => sel(a_ > b_),
        "v_cmp_lt_i16" => sel(b_ < a_),
        "v_frexp_mant_f32" | "v_frexp_exp_i32_f32" => {
            let (m, e) = if f5 == 0.0 {
                (0.0, 0)
            } else {
                let e = f5.abs().log2().floor() as i32 + 1;
                (f5 / 2f32.powi(e), e)
            };
            if name == "v_frexp_mant_f32" {
                Bits(m.to_bits())
            } else {
                Bits(e as u32)
            }
        }
        "v_mad_u64_u32_lo" => Bits((u64::from(ha) * u64::from(hb)).wrapping_add(c64) as u32),
        "v_mad_u64_u32_hi" => Bits(
            (u64::from(ha) * u64::from(hb))
                .wrapping_add(c64)
                .wrapping_shr(32) as u32,
        ),
        "v_mad_i64_i32_hi" => Bits(
            ((i64::from(ha as i32) * i64::from(hb as i32)).wrapping_add(c64 as i64) >> 32) as u32,
        ),
        "v_perm_b32" => Bits(bytes(hb, 2) | (bytes(ha, 0) << 8) | (bytes(ha, 3) << 16)),
        "v_perm_b32_ff" => {
            Bits(bytes(hb, 0) | (bytes(hb, 1) << 8) | (bytes(ha, 1) << 16) | (0xFF << 24))
        }
        "v_cvt_pk_u8_f32" => Bits((ha & !(0xFF << 16)) | ((f6.round_ties_even() as u32) << 16)),
        "v_mul_hi_u32_u24" => {
            Bits(((u64::from(ha & 0xFF_FFFF) * u64::from(hb & 0xFF_FFFF)) >> 32) as u32)
        }
        "v_ffbl_b32" => Bits(if ha == 0 {
            u32::MAX
        } else {
            ha.trailing_zeros()
        }),
        "v_cvt_flr_i32_f32" => Bits(f5.floor() as i32 as u32),
        "v_cvt_rpi_i32_f32" => Bits((f5 + 0.5).floor() as i32 as u32),
        "f64_fma_cvt" => Bits(((((t as i32 - 20) as f64).powi(2) + 1.0) as f32).to_bits()),
        "f64_sqrt" => F((f64::from(t)).sqrt() as f32, 1e-6, 0.0),
        "f64_floor_ldexp" => Bits(((f64::from(f5).floor() * 8.0) as f32).to_bits()),
        "v_cmp_lt_f64" => sel((t as i32) < 20),
        "v_cmp_gt_u64" => sel(c64 > (u64::from(hb) | (u64::from(ha) << 32))),
        "v_cmp_lt_i64" => sel((c64 as i64) < ((u64::from(hb) | (u64::from(ha) << 32)) as i64)),
        other => panic!("pas d'oracle pour {other}"),
    }
}

#[test]
#[ignore = "nécessite un GPU Vulkan"]
fn gpu_ext_battery_f16_packed_f64_matches_cpu_oracle() {
    let Some(gpu) = Gpu::new() else {
        eprintln!("aucun GPU Vulkan : test ignoré");
        return;
    };
    for prog_name in ["cs_ext_battery_w32", "cs_ext_battery_w64"] {
        let p = prog(prog_name);
        let segs: Vec<&str> = p
            .asm
            .lines()
            .filter_map(|l| l.strip_prefix("; seg "))
            .collect();
        let wave = if p.wave == 64 {
            WaveSize::W64
        } else {
            WaveSize::W32
        };
        let mut cfg = cs_cfg(wave);
        cfg.resources = vec![Resource {
            kind: ResourceKind::Buffer { stride: 0 },
            loc: ResourceLoc::Table {
                table_sgpr: 0,
                byte_offset: 0,
            },
            set: 0,
            binding: 0,
        }];
        let bufs = [BufSpec::zero(64 * segs.len() + 64)];
        let Some(o) = run_named(&gpu, prog_name, cfg, &bufs, &[]) else {
            continue;
        };
        let mut bad = Vec::new();
        for (k, name) in segs.iter().enumerate() {
            for t in 0..64u32 {
                let got = o[0][k * 64 + t as usize];
                let ok = match oracle2(name, t) {
                    E::Bits(v) => got == v,
                    E::F(v, rel, abs) => (f32::from_bits(got) - v).abs() <= rel * v.abs() + abs,
                };
                if !ok {
                    bad.push(format!("{prog_name}/{name} lane {t} : {got:#x}"));
                    break;
                }
            }
        }
        assert!(bad.is_empty(), "segments divergents :\n{}", bad.join("\n"));
        eprintln!("{prog_name} : {} segments x 64 lanes vérifiés", segs.len());
    }
}

// ---------------------------------------------------------------------------- DS / sous-groupes

fn oracle3(name: &str, t: u32, w: u32) -> Option<u32> {
    let base = t - t % w;
    let l = t % w;
    let lds_val = |lane_t: u32| 100 + lane_t;
    let (s10, s11) = (0x89ab_cdefu64, 0x0123_4567u64);
    let v64 = s10 | (s11 << 32);
    let n64 = (0x8123_4567u64 << 32 | s10) as i64;
    let ha = 100 + t;
    let hb = 0x1122_3344u32;
    let h64 = u64::from(ha) | (u64::from(hb) << 32);
    Some(match name {
        "ds_read_u8" => (0xa0 + t) & 0xFF,
        "ds_read_i8" => ((0xa0 + t) as u8 as i8) as i32 as u32,
        "ds_read_u16" => lds_val(t) & 0xFFFF,
        "ds_read2_b32_lo" => lds_val(t),
        "ds_read2_b32_hi" => 0x1122_3344,
        "ds_add_rtn_u32" | "ds_append" => return None, // permutation vérifiée à part
        "ds_bpermute_b32" => lds_val(base + (w - 1 - l)),
        "ds_permute_b32" => lds_val(base + (l + w - 1) % w),
        "ds_swizzle_xor1" => lds_val(base + (l ^ 1)),
        "ds_swizzle_quad" => lds_val(base + ((l & !3) | (3 - (l & 3)))),
        "v_mbcnt_lane" => l,
        "v_writelane_b32" => {
            if l == 5 {
                77
            } else {
                0
            }
        }
        "s_wqm_b32" => 0x000F_0F0F,
        "s_lshl_b64_lo" => (v64 << 36) as u32,
        "s_lshl_b64_hi" => ((v64 << 36) >> 32) as u32,
        "s_lshr_b64_lo" => (v64 >> 4) as u32,
        "s_ashr_i64_hi" => ((n64 >> 40) >> 32) as u32,
        "s_ashr_i64_lo" => (n64 >> 40) as u32,
        "s_bitset1_b32" => 0x10 | (1 << 7),
        "s_ff1_i32_b64" => 32 + 22,
        "v_lshlrev_b64_hi" => ((h64 << 36) >> 32) as u32,
        "v_lshrrev_b64_lo" => (h64 >> 36) as u32,
        "v_ashrrev_i64_hi" => (((h64 as i64) >> 4) >> 32) as u32,
        "v_bfm_b32" => ((1u32 << 7) - 1) << 9,
        "v_xor3_b32" => ha ^ hb ^ t,
        other => panic!("pas d'oracle pour {other}"),
    })
}

#[test]
#[ignore = "nécessite un GPU Vulkan"]
fn gpu_ds_and_subgroup_battery_matches_cpu_oracle() {
    let Some(gpu) = Gpu::new() else {
        eprintln!("aucun GPU Vulkan : test ignoré");
        return;
    };
    for prog_name in ["cs_ds_battery_w32", "cs_ds_battery_w64"] {
        let p = prog(prog_name);
        let segs: Vec<&str> = p
            .asm
            .lines()
            .filter_map(|l| l.strip_prefix("; seg "))
            .collect();
        let wave = if p.wave == 64 {
            WaveSize::W64
        } else {
            WaveSize::W32
        };
        let mut cfg = cs_cfg(wave);
        cfg.lds_dwords = 1024;
        cfg.resources = vec![Resource {
            kind: ResourceKind::Buffer { stride: 0 },
            loc: ResourceLoc::Table {
                table_sgpr: 0,
                byte_offset: 0,
            },
            set: 0,
            binding: 0,
        }];
        let bufs = [BufSpec::zero(64 * segs.len() + 64)];
        let Some(o) = run_named(&gpu, prog_name, cfg, &bufs, &[]) else {
            continue;
        };
        let mut bad = Vec::new();
        for (k, name) in segs.iter().enumerate() {
            let col = &o[0][k * 64..k * 64 + 64];
            match oracle3(name, 0, u32::from(p.wave)) {
                None => {
                    let mut v = col.to_vec();
                    v.sort_unstable();
                    if v != (0..64).collect::<Vec<u32>>() {
                        bad.push(format!(
                            "{prog_name}/{name} : pas une permutation {:?}",
                            &col[..8]
                        ));
                    }
                }
                Some(_) => {
                    for t in 0..64u32 {
                        let want = oracle3(name, t, u32::from(p.wave)).unwrap();
                        if col[t as usize] != want {
                            bad.push(format!(
                                "{prog_name}/{name} lane {t} : {:#x} != {want:#x}",
                                col[t as usize]
                            ));
                            break;
                        }
                    }
                }
            }
        }
        assert!(bad.is_empty(), "segments divergents :\n{}", bad.join("\n"));
        eprintln!("{prog_name} : {} segments vérifiés", segs.len());
    }
}

#[test]
#[ignore = "nécessite un GPU Vulkan"]
fn gpu_buffer_bounds_checking() {
    let Some(gpu) = Gpu::new() else {
        eprintln!("aucun GPU Vulkan : test ignoré");
        return;
    };
    let mut cfg = cs_cfg(WaveSize::W32);
    let res = |binding, off| Resource {
        kind: ResourceKind::Buffer { stride: 0 },
        loc: ResourceLoc::Table {
            table_sgpr: 0,
            byte_offset: off,
        },
        set: 0,
        binding,
    };
    cfg.resources = vec![res(0, 0), res(1, 0x10)];
    cfg.buffer_sizes = vec![(0, 0, 128)];
    let bufs = [
        BufSpec {
            words: vec![0xAAAA_AAAA; 64],
            bda: false,
        },
        BufSpec::zero(192),
    ];
    let Some(o) = run_named(&gpu, "cs_bounds", cfg, &bufs, &[]) else {
        return;
    };
    for t in 0..64usize {
        assert_eq!(
            o[0][t],
            if t < 32 { t as u32 + 1 } else { 0xAAAA_AAAA },
            "écriture hors bornes ignorée, mot {t}"
        );
        assert_eq!(
            o[1][t],
            if t < 32 { t as u32 + 1 } else { 0 },
            "lecture hors bornes = 0, lane {t}"
        );
        // course bénigne avec le store de la lane 31 : valeur initiale ou écrite
        assert!(
            matches!(o[1][64 + t], 32 | 0xAAAA_AAAA),
            "s_buffer_load dans les bornes : {:#x}",
            o[1][64 + t]
        );
        assert_eq!(o[1][128 + t], 0, "s_buffer_load hors bornes = 0");
    }
}

// ---------------------------------------------------------------------------- MIMG (CS)

/// Résultat d'une exécution avec images : buffer 0, image R32_UINT 2x2, image RGBA8 2x2.
struct ImgRun {
    buf: Vec<u32>,
    r32: [u32; 4],
    rgba: [[u8; 4]; 4],
}

const TEXELS: [[u8; 4]; 4] = [
    [255, 0, 0, 255],
    [0, 255, 0, 255],
    [0, 0, 255, 255],
    [255, 255, 0, 255],
];

impl Gpu {
    fn run_img(&self, spv: &[u32], subgroup: u32, words: usize) -> Option<ImgRun> {
        // SAFETY: ressources créées puis détruites ici ; mémoire hôte lue après `queue_wait_idle`.
        unsafe {
            let d = &self.dev;
            let host =
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
            let mt = |bits: u32| {
                (0..self.mem_props.memory_type_count).find(|&i| {
                    bits & (1 << i) != 0
                        && self.mem_props.memory_types[i as usize]
                            .property_flags
                            .contains(host)
                })
            };
            let bytes = (words * 4) as u64;
            let buf = d
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(bytes)
                        .usage(vk::BufferUsageFlags::STORAGE_BUFFER),
                    None,
                )
                .ok()?;
            let rq = d.get_buffer_memory_requirements(buf);
            let bmem = d
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(rq.size)
                        .memory_type_index(mt(rq.memory_type_bits)?),
                    None,
                )
                .ok()?;
            d.bind_buffer_memory(buf, bmem, 0).ok()?;
            let bp = d
                .map_memory(bmem, 0, bytes, vk::MemoryMapFlags::empty())
                .ok()? as *mut u8;
            std::ptr::write_bytes(bp, 0, bytes as usize);

            let mk_img = |fmt: vk::Format,
                          usage: vk::ImageUsageFlags|
             -> Option<(
                vk::Image,
                vk::DeviceMemory,
                vk::ImageView,
                usize,
                *mut u8,
            )> {
                let img = d
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
                            .usage(usage)
                            .initial_layout(vk::ImageLayout::PREINITIALIZED),
                        None,
                    )
                    .ok()?;
                let rq = d.get_image_memory_requirements(img);
                let mem = d
                    .allocate_memory(
                        &vk::MemoryAllocateInfo::default()
                            .allocation_size(rq.size)
                            .memory_type_index(mt(rq.memory_type_bits)?),
                        None,
                    )
                    .ok()?;
                d.bind_image_memory(img, mem, 0).ok()?;
                let pitch = d
                    .get_image_subresource_layout(
                        img,
                        vk::ImageSubresource {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            mip_level: 0,
                            array_layer: 0,
                        },
                    )
                    .row_pitch as usize;
                let p = d
                    .map_memory(mem, 0, rq.size, vk::MemoryMapFlags::empty())
                    .ok()? as *mut u8;
                std::ptr::write_bytes(p, 0, rq.size as usize);
                let view = d
                    .create_image_view(
                        &vk::ImageViewCreateInfo::default()
                            .image(img)
                            .view_type(vk::ImageViewType::TYPE_2D)
                            .format(fmt)
                            .subresource_range(vk::ImageSubresourceRange {
                                aspect_mask: vk::ImageAspectFlags::COLOR,
                                base_mip_level: 0,
                                level_count: 1,
                                base_array_layer: 0,
                                layer_count: 1,
                            }),
                        None,
                    )
                    .ok()?;
                Some((img, mem, view, pitch, p))
            };
            let (tex, tmem, tview, tpitch, tp) =
                mk_img(vk::Format::R8G8B8A8_UNORM, vk::ImageUsageFlags::SAMPLED)?;
            for (k, t) in TEXELS.iter().enumerate() {
                std::ptr::copy_nonoverlapping(
                    t.as_ptr(),
                    tp.add((k / 2) * tpitch + (k % 2) * 4),
                    4,
                );
            }
            let (sa, samem, saview, sapitch, sap) =
                mk_img(vk::Format::R32_UINT, vk::ImageUsageFlags::STORAGE)?;
            let (sb, sbmem, sbview, sbpitch, sbp) =
                mk_img(vk::Format::R8G8B8A8_UNORM, vk::ImageUsageFlags::STORAGE)?;
            let sampler = d
                .create_sampler(
                    &vk::SamplerCreateInfo::default()
                        .mag_filter(vk::Filter::NEAREST)
                        .min_filter(vk::Filter::NEAREST)
                        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                    None,
                )
                .ok()?;

            let cs = vk::ShaderStageFlags::COMPUTE;
            let bind = |b: u32, t: vk::DescriptorType| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(b)
                    .descriptor_type(t)
                    .descriptor_count(1)
                    .stage_flags(cs)
            };
            let binds = [
                bind(0, vk::DescriptorType::STORAGE_BUFFER),
                bind(4, vk::DescriptorType::SAMPLED_IMAGE),
                bind(5, vk::DescriptorType::SAMPLER),
                bind(6, vk::DescriptorType::STORAGE_IMAGE),
                bind(7, vk::DescriptorType::STORAGE_IMAGE),
            ];
            let dsl = d
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&binds),
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
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(spv), None)
                .ok()?;
            let mut rss = vk::PipelineShaderStageRequiredSubgroupSizeCreateInfo::default()
                .required_subgroup_size(subgroup);
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(cs)
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
            let sizes = [
                vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1),
                vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::SAMPLED_IMAGE)
                    .descriptor_count(1),
                vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::SAMPLER)
                    .descriptor_count(1),
                vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::STORAGE_IMAGE)
                    .descriptor_count(2),
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
            let bi = [vk::DescriptorBufferInfo::default()
                .buffer(buf)
                .range(vk::WHOLE_SIZE)];
            let ti = [vk::DescriptorImageInfo::default()
                .image_view(tview)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let si = [vk::DescriptorImageInfo::default().sampler(sampler)];
            let ai = [vk::DescriptorImageInfo::default()
                .image_view(saview)
                .image_layout(vk::ImageLayout::GENERAL)];
            let bi2 = [vk::DescriptorImageInfo::default()
                .image_view(sbview)
                .image_layout(vk::ImageLayout::GENERAL)];
            let w = |b: u32, t: vk::DescriptorType| {
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(b)
                    .descriptor_type(t)
            };
            d.update_descriptor_sets(
                &[
                    w(0, vk::DescriptorType::STORAGE_BUFFER).buffer_info(&bi),
                    w(4, vk::DescriptorType::SAMPLED_IMAGE).image_info(&ti),
                    w(5, vk::DescriptorType::SAMPLER).image_info(&si),
                    w(6, vk::DescriptorType::STORAGE_IMAGE).image_info(&ai),
                    w(7, vk::DescriptorType::STORAGE_IMAGE).image_info(&bi2),
                ],
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
            let range = vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            };
            let barrier = |img: vk::Image, new: vk::ImageLayout, dst: vk::AccessFlags| {
                let b = [vk::ImageMemoryBarrier::default()
                    .image(img)
                    .old_layout(vk::ImageLayout::PREINITIALIZED)
                    .new_layout(new)
                    .src_access_mask(vk::AccessFlags::HOST_WRITE)
                    .dst_access_mask(dst)
                    .subresource_range(range)];
                d.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::HOST,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &b,
                );
            };
            barrier(
                tex,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::AccessFlags::SHADER_READ,
            );
            barrier(
                sa,
                vk::ImageLayout::GENERAL,
                vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
            );
            barrier(
                sb,
                vk::ImageLayout::GENERAL,
                vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
            );
            d.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, pipe);
            d.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::COMPUTE, pl, 0, &[set], &[]);
            d.cmd_dispatch(cb, 1, 1, 1);
            let mb = [vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)];
            d.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &mb,
                &[],
                &[],
            );
            d.end_command_buffer(cb).ok()?;
            let cbs = [cb];
            d.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default().command_buffers(&cbs)],
                vk::Fence::null(),
            )
            .ok()?;
            d.queue_wait_idle(self.queue).ok()?;

            let mut out = ImgRun {
                buf: vec![0; words],
                r32: [0; 4],
                rgba: [[0; 4]; 4],
            };
            std::ptr::copy_nonoverlapping(bp as *const u32, out.buf.as_mut_ptr(), words);
            for k in 0..4 {
                let (x, y) = (k % 2, k / 2);
                out.r32[k] = *(sap.add(y * sapitch + x * 4) as *const u32);
                std::ptr::copy_nonoverlapping(
                    sbp.add(y * sbpitch + x * 4),
                    out.rgba[k].as_mut_ptr(),
                    4,
                );
            }
            d.destroy_command_pool(cp, None);
            d.destroy_descriptor_pool(pool, None);
            d.destroy_pipeline(pipe, None);
            d.destroy_shader_module(sm, None);
            d.destroy_pipeline_layout(pl, None);
            d.destroy_descriptor_set_layout(dsl, None);
            d.destroy_sampler(sampler, None);
            for (img, mem, view) in [(tex, tmem, tview), (sa, samem, saview), (sb, sbmem, sbview)] {
                d.destroy_image_view(view, None);
                d.destroy_image(img, None);
                d.unmap_memory(mem);
                d.free_memory(mem, None);
            }
            d.unmap_memory(bmem);
            d.destroy_buffer(buf, None);
            d.free_memory(bmem, None);
            Some(out)
        }
    }
}

fn texel_f(tx: i32, ty: i32, c: usize) -> f32 {
    let (x, y) = (tx.clamp(0, 1) as usize, ty.clamp(0, 1) as usize);
    f32::from(TEXELS[y * 2 + x][c]) / 255.0
}

#[test]
#[ignore = "nécessite un GPU Vulkan"]
fn gpu_mimg_battery_sample_gather_load_store_atomic() {
    let Some(gpu) = Gpu::new() else {
        eprintln!("aucun GPU Vulkan : test ignoré");
        return;
    };
    let p = prog("cs_mimg_battery");
    let segs: Vec<&str> = p
        .asm
        .lines()
        .filter_map(|l| l.strip_prefix("; seg "))
        .collect();
    let mut cfg = cs_cfg(WaveSize::W32);
    let res = |kind, binding, off| Resource {
        kind,
        loc: ResourceLoc::Table {
            table_sgpr: 0,
            byte_offset: off,
        },
        set: 0,
        binding,
    };
    cfg.resources = vec![
        res(ResourceKind::Buffer { stride: 0 }, 0, 0),
        res(ResourceKind::Image, 4, 0x20),
        res(ResourceKind::Sampler, 5, 0x40),
        res(ResourceKind::Image, 6, 0x60),
        res(ResourceKind::Image, 7, 0x80),
    ];
    let t = translate_program(p.words, &cfg).unwrap_or_else(|e| panic!("cs_mimg_battery: {e}"));
    let Some(o) = gpu.run_img(&t.words, 32, 64 * segs.len() + 64) else {
        eprintln!("exécution GPU impossible : test ignoré");
        return;
    };
    let mut bad = Vec::new();
    for (k, name) in segs.iter().enumerate() {
        for tid in 0..64u32 {
            let (x4, y4) = ((tid & 3) as f32, ((tid >> 2) & 3) as f32);
            let (u, v) = ((x4 + 0.5) / 4.0, (y4 + 0.5) / 4.0);
            let (tx, ty) = ((u * 2.0).floor() as i32, (v * 2.0).floor() as i32);
            let (lx, ly) = ((tid & 1) as i32, ((tid >> 1) & 1) as i32);
            // gather : empreinte 2x2 autour de (u*2-0.5, v*2-0.5) ; ordre (x0y1, x1y1, x1y0, x0y0)
            let (gx, gy) = (
                (u * 2.0 - 0.5).floor() as i32,
                (v * 2.0 - 0.5).floor() as i32,
            );
            let g = |c: usize, which: usize| match which {
                0 => texel_f(gx, gy + 1, c),
                1 => texel_f(gx + 1, gy + 1, c),
                2 => texel_f(gx + 1, gy, c),
                _ => texel_f(gx, gy, c),
            };
            let got = o.buf[k * 64 + tid as usize];
            let want: Option<u32> = match *name {
                "sample_lz_r" | "sample_l" => {
                    Some(texel_f(tx, ty, if *name == "sample_l" { 2 } else { 0 }).to_bits())
                }
                "sample_lz_g" => Some(texel_f(tx, ty, 1).to_bits()),
                "sample_lz_o_r" => Some(texel_f(tx + 1, ty, 0).to_bits()),
                "sample_d_r" => Some(texel_f(tx, ty, 1).to_bits()),
                "sample_d16_r" => Some(texel_f(tx, ty, 0).to_bits()),
                "gather4_r_w" => Some(g(0, 0).to_bits()),
                "gather4_r_x" => Some(g(0, 3).to_bits()),
                "gather4_g_z" => Some(g(1, 1).to_bits()),
                "load_r" => Some(texel_f(lx, ly, 0).to_bits()),
                "load_tfe_status" => Some(0),
                "resinfo_wh" => Some(2 << 8 | 2),
                "resinfo_levels" => Some(1),
                "store_rgba8" | "atomic_umax" => Some(0),
                "atomic_add_ret" => None,
                other => panic!("pas d'oracle pour {other}"),
            };
            if let Some(w) = want
                && got != w
            {
                bad.push(format!("{name} lane {tid} : {got:#x} != {w:#x}"));
                break;
            }
        }
        if *name == "atomic_add_ret" {
            let mut v = o.buf[k * 64..k * 64 + 64].to_vec();
            v.sort_unstable();
            if v != (0..64).collect::<Vec<u32>>() {
                bad.push("atomic_add_ret : retours non uniques".to_string());
            }
        }
    }
    assert!(bad.is_empty(), "segments divergents :\n{}", bad.join("\n"));
    assert_eq!(o.r32[0], 64, "image_atomic_add : 64 lanes");
    assert_eq!(o.r32[1], 63, "image_atomic_umax");
    for (k, px) in o.rgba.iter().enumerate() {
        let (x, y) = ((k % 2) as u8, (k / 2) as u8);
        assert_eq!(*px, [255 * x, 255 * y, 0, 255], "image_store texel {k}");
    }
    eprintln!(
        "cs_mimg_battery : {} segments + image_store/atomiques vérifiés",
        segs.len()
    );
}

// ---------------------------------------------------------------------------- DPP / SDWA

/// Oracle CPU (lecture du guide ISA) : valeur de la lane globale `tid` (vague de `w` lanes).
fn oracle_dpp(name: &str, tid: u32, w: u32) -> u32 {
    const OLD: u32 = 0xdead;
    let base = tid - tid % w;
    let t = tid % w; // lane dans la vague
    let v2 = |l: u32| 100 + base + l;
    let v3 = |l: u32| {
        (base + l)
            .wrapping_mul(0x0101_0101)
            .wrapping_add(0x1234_5678)
    };
    let (row, p) = (t & !15, t & 15);
    let src = |sp: Option<u32>| sp.map_or(OLD, |s| v2(row + s));
    match name {
        "dpp_quad_perm" => v2((t & !3) | [1, 0, 3, 2][(t & 3) as usize]),
        "dpp_row_shl3_bc" => {
            if p + 3 < 16 {
                v2(row + p + 3)
            } else {
                0
            }
        }
        "dpp_row_shl3" => src((p + 3 < 16).then_some(p + 3)),
        "dpp_row_shr1" => src(p.checked_sub(1)),
        "dpp_row_ror4" => src(Some((p + 16 - 4) & 15)),
        "dpp_row_mirror" => src(Some(15 - p)),
        "dpp_row_half_mirror" => src(Some((p & 8) | (7 - (p & 7)))),
        "dpp_row_share5" => src(Some(5)),
        "dpp_row_xmask3" => src(Some(p ^ 3)),
        "dpp_masked" => {
            // row_mask 0b0101 : rangées paires ; bank_mask 0b1010 : banques 1 et 3
            let (r, bank) = (t >> 4, (p >> 2) & 3);
            if (0x5 >> r) & 1 == 1 && (0xa >> bank) & 1 == 1 {
                v2((t & !3) | (3 - (t & 3)))
            } else {
                OLD
            }
        }
        "dpp8_reverse" => v2((t & !7) | (7 - (t & 7))),
        "dpp_add" => v2(t & !3).wrapping_add(v3(t)),
        "sdwa_add_sel" => ((v2(t) >> 8) & 0xFF).wrapping_add(v3(t) & 0xFFFF),
        "sdwa_dst_byte1" => ((v3(t) >> 16) & 0xFF) << 8,
        "sdwa_preserve" => (OLD & 0xFFFF) | ((v3(t) & 0xFFFF) << 16),
        "sdwa_sext" => ((v3(t) >> 24) as u8 as i8 as i32 as u32).wrapping_add(v2(t)),
        other => panic!("pas d'oracle pour {other}"),
    }
}

#[test]
#[ignore = "nécessite un GPU Vulkan"]
fn gpu_dpp_and_sdwa_battery() {
    let Some(gpu) = Gpu::new() else {
        eprintln!("aucun GPU Vulkan : test ignoré");
        return;
    };
    for prog_name in ["cs_dpp_battery_w32", "cs_dpp_battery_w64"] {
        let p = prog(prog_name);
        let segs: Vec<&str> = p
            .asm
            .lines()
            .filter_map(|l| l.strip_prefix("; seg "))
            .collect();
        let wave = if p.wave == 64 {
            WaveSize::W64
        } else {
            WaveSize::W32
        };
        let mut cfg = cs_cfg(wave);
        cfg.resources = vec![Resource {
            kind: ResourceKind::Buffer { stride: 0 },
            loc: ResourceLoc::Table {
                table_sgpr: 0,
                byte_offset: 0,
            },
            set: 0,
            binding: 0,
        }];
        let bufs = [BufSpec::zero(64 * segs.len() + 64)];
        let Some(o) = run_named(&gpu, prog_name, cfg, &bufs, &[]) else {
            continue;
        };
        let mut bad = Vec::new();
        for (k, name) in segs.iter().enumerate() {
            for t in 0..64u32 {
                let want = oracle_dpp(name, t, u32::from(p.wave));
                let got = o[0][k * 64 + t as usize];
                if got != want {
                    bad.push(format!(
                        "{prog_name}/{name} lane {t} : {got:#x} != {want:#x}"
                    ));
                    break;
                }
            }
        }
        assert!(bad.is_empty(), "segments divergents :\n{}", bad.join("\n"));
        eprintln!("{prog_name} : {} segments DPP/SDWA vérifiés", segs.len());
    }
}

#[test]
#[ignore = "nécessite un GPU Vulkan"]
fn gpu_buffer_byte_and_short_stores() {
    let Some(gpu) = Gpu::new() else {
        eprintln!("aucun GPU Vulkan : test ignoré");
        return;
    };
    let mut cfg = cs_cfg(WaveSize::W32);
    cfg.resources = vec![Resource {
        kind: ResourceKind::Buffer { stride: 0 },
        loc: ResourceLoc::Table {
            table_sgpr: 0,
            byte_offset: 0,
        },
        set: 0,
        binding: 0,
    }];
    let Some(o) = run_named(&gpu, "cs_buf_byte", cfg, &[BufSpec::zero(128)], &[]) else {
        return;
    };
    let bytes: Vec<u8> = o[0].iter().flat_map(|w| w.to_le_bytes()).collect();
    for t in 0..64usize {
        assert_eq!(bytes[t], (0x41 + t) as u8, "buffer_store_byte {t}");
        let s = u16::from_le_bytes([bytes[256 + 2 * t], bytes[257 + 2 * t]]);
        assert_eq!(s, 0x41 + t as u16, "buffer_store_short {t}");
    }
}
