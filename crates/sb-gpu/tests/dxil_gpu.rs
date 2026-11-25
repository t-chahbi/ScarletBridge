//! Exécution réelle sur GPU Vulkan de shaders DXIL (SM6.0) traduits en SPIR-V : les résultats sont
//! comparés à une référence CPU. Les tests sont sautés sans GPU Vulkan.

use std::path::{Path, PathBuf};

use sb_gpu::{GpuContext, RenderCommand, VertexPosColor, VulkanRenderer};

fn shader(name: &str) -> Vec<u8> {
    let p: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/shaders/dxil")
        .join(format!("{name}.dxil"));
    std::fs::read(&p).unwrap_or_else(|e| panic!("{p:?}: {e}"))
}

fn renderer(w: u32, h: u32) -> Option<VulkanRenderer> {
    let gpu = GpuContext::new(false).ok()?;
    VulkanRenderer::new(gpu, w, h).ok()
}

fn lcg(seed: &mut u32) -> u32 {
    *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
    *seed
}

fn bytes_u32(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn bytes_f32(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn read_u32(b: &[u8]) -> Vec<u32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect()
}

fn read_f32(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

#[test]
#[ignore = "GPU Vulkan requis : cargo test -p sb-gpu --test dxil_gpu -- --ignored"]
fn gpu_dxil_math_matches_cpu() {
    let Some(mut r) = renderer(64, 64) else {
        return;
    };
    let pid = r
        .create_compute_pipeline_from_dxbc(&shader("exec_math"))
        .expect("pipeline DXIL");
    let n = 64usize;
    let mut input = Vec::new();
    for i in 0..n {
        let f = i as f32;
        input.extend_from_slice(&[
            f * 0.37 + 0.1,
            f * 0.5 + 1.0,
            f * 0.11 + 0.5,
            (i % 13) as f32 + 0.25,
        ]);
    }
    let cb = [2.0f32, 3.0, 4.0, 5.0, 10.0, 20.0, 30.0, 40.0];
    r.execute_commands(&[
        RenderCommand::UpdateConstantBuffer {
            slot: 0,
            data: bytes_f32(&cb),
        },
        RenderCommand::UpdateStorageBuffer {
            slot: 0,
            data: bytes_f32(&input),
        },
        RenderCommand::SetComputePipeline { pipeline_id: pid },
        RenderCommand::Dispatch { x: 1, y: 1, z: 1 },
    ])
    .expect("dispatch");
    let out = read_f32(&r.readback_storage_buffer(1, n * 16).expect("readback"));
    for i in 0..n {
        let v = &input[i * 4..i * 4 + 4];
        let mut e = [
            v[0] * cb[0] + cb[4],
            v[1] * cb[1] + cb[5],
            v[2] * cb[2] + cb[6],
            v[3] * cb[3] + cb[7],
        ];
        e[0] = e[0].abs().sqrt() + v[1].floor();
        e[1] = v[0].max(v[2]) - v[1].min(v[3]);
        e[2] = v[0] + 0.25 * (v[2] - v[0]) + (v[3] as u32) as f32;
        e[3] = (v[3] * 0.1).clamp(0.0, 1.0)
            + v[0].fract()
            + (v[1] * 0.01).exp2()
            + 1.0 / (v[2] + 1.0).sqrt();
        for c in 0..4 {
            let g = out[i * 4 + c];
            assert!(
                (g - e[c]).abs() <= 1e-3 * e[c].abs().max(1.0),
                "math[{i}].{c}: gpu={g} cpu={}",
                e[c]
            );
        }
    }
}

fn control_ref(input: u32, n: u32, mode: u32) -> u32 {
    let x = input % 97 + 1;
    let mut c = x;
    let mut steps = 0u32;
    while c != 1 && steps < 200 {
        c = if c & 1 != 0 { 3 * c + 1 } else { c >> 1 };
        steps += 1;
    }
    let mut r = steps;
    match mode {
        0 => r += 1000,
        1 => r += 2000 + 3000,
        2 => r += 3000,
        _ => r = 7,
    }
    for a in 0..5u32 {
        if a == (x & 3) {
            continue;
        }
        if a > 3 && (x & 1) != 0 {
            break;
        }
        r = r.wrapping_add(a * x);
    }
    let mut k = 0u32;
    loop {
        k += 1;
        if k != 3 {
            r ^= k;
        }
        if k >= n {
            break;
        }
    }
    for i in 0..4u32 {
        for j in 0..4u32 {
            if i + j == (x & 7) {
                break;
            }
            r += 1;
        }
    }
    r
}

#[test]
#[ignore = "GPU Vulkan requis : cargo test -p sb-gpu --test dxil_gpu -- --ignored"]
fn gpu_dxil_control_flow_matches_cpu() {
    let Some(mut r) = renderer(64, 64) else {
        return;
    };
    let pid = r
        .create_compute_pipeline_from_dxbc(&shader("exec_control"))
        .expect("pipeline DXIL");
    let mut seed = 7u32;
    let input: Vec<u32> = (0..64).map(|_| lcg(&mut seed) >> 8).collect();
    for (n, mode) in [(5u32, 0u32), (9, 1), (4, 2), (7, 9)] {
        r.execute_commands(&[
            RenderCommand::UpdateConstantBuffer {
                slot: 0,
                data: bytes_u32(&[n, mode, 0, 0]),
            },
            RenderCommand::UpdateStorageBuffer {
                slot: 0,
                data: bytes_u32(&input),
            },
            RenderCommand::SetComputePipeline { pipeline_id: pid },
            RenderCommand::Dispatch { x: 1, y: 1, z: 1 },
        ])
        .expect("dispatch");
        let out = read_u32(&r.readback_storage_buffer(1, 64 * 4).expect("readback"));
        for i in 0..64 {
            assert_eq!(
                out[i],
                control_ref(input[i], n, mode),
                "control[{i}] n={n} mode={mode}"
            );
        }
    }
}

#[test]
#[ignore = "GPU Vulkan requis : cargo test -p sb-gpu --test dxil_gpu -- --ignored"]
fn gpu_dxil_groupshared_reduction_and_atomics() {
    let Some(mut r) = renderer(64, 64) else {
        return;
    };
    let pid = r
        .create_compute_pipeline_from_dxbc(&shader("exec_shared"))
        .expect("pipeline DXIL");
    let input: Vec<u32> = (0..256u32).map(|i| i * 3 + 1).collect();
    r.execute_commands(&[
        RenderCommand::UpdateStorageBuffer {
            slot: 0,
            data: bytes_u32(&input),
        },
        RenderCommand::UpdateStorageBuffer {
            slot: 1,
            data: bytes_u32(&[0u32; 256]),
        },
        RenderCommand::SetComputePipeline { pipeline_id: pid },
        RenderCommand::Dispatch { x: 4, y: 1, z: 1 },
    ])
    .expect("dispatch");
    let out = read_u32(&r.readback_storage_buffer(1, 256 * 4).expect("readback"));
    let mut total = 0u32;
    for g in 0..4usize {
        let s: u32 = input[g * 64..g * 64 + 64].iter().sum();
        assert_eq!(out[g], s, "somme du groupe {g}");
        total += s;
    }
    assert_eq!(out[255], total, "InterlockedAdd global");
}

fn fbh_s(x: i32) -> u32 {
    if x == 0 || x == -1 {
        return u32::MAX;
    }
    let y = if x < 0 { !x } else { x };
    31 - (y as u32).leading_zeros()
}

#[test]
#[ignore = "GPU Vulkan requis : cargo test -p sb-gpu --test dxil_gpu -- --ignored"]
fn gpu_dxil_bit_and_integer_ops_match_cpu() {
    let Some(mut r) = renderer(64, 64) else {
        return;
    };
    let pid = r
        .create_compute_pipeline_from_dxbc(&shader("exec_bits"))
        .expect("pipeline DXIL");
    let mut seed = 99u32;
    let mut input = Vec::new();
    for _ in 0..64 * 4 {
        input.push(lcg(&mut seed));
    }
    input[2] = 0;
    input[7] = 0;
    input[10] = 0x8000_0000;
    r.execute_commands(&[
        RenderCommand::UpdateStorageBuffer {
            slot: 0,
            data: bytes_u32(&input),
        },
        RenderCommand::SetComputePipeline { pipeline_id: pid },
        RenderCommand::Dispatch { x: 1, y: 1, z: 1 },
    ])
    .expect("dispatch");
    let out = read_u32(&r.readback_storage_buffer(1, 64 * 16).expect("readback"));
    let fbl = |x: u32| if x == 0 { u32::MAX } else { x.trailing_zeros() };
    let fbh = |x: u32| {
        if x == 0 {
            u32::MAX
        } else {
            31 - x.leading_zeros()
        }
    };
    for i in 0..64 {
        let v = &input[i * 4..i * 4 + 4];
        let s: Vec<i32> = v.iter().map(|&x| x as i32).collect();
        let e = [
            v[0].reverse_bits() ^ v[1].count_ones(),
            fbl(v[2]).wrapping_add(fbh(v[3]).wrapping_mul(3)),
            ((s[0] >> 3) as u32)
                .wrapping_add((s[1] / ((s[2] & 255) + 1)) as u32)
                .wrapping_add((s[1] % ((s[3] & 255) + 1)) as u32),
            (v[0] << (v[1] & 31)) ^ (v[2] >> (v[3] & 31)) ^ ((v[0] >> 4) & 0xFF) ^ fbh_s(s[2]),
        ];
        assert_eq!(&out[i * 4..i * 4 + 4], &e, "bits[{i}] in={v:?}");
    }
}

#[test]
#[ignore = "GPU Vulkan requis : cargo test -p sb-gpu --test dxil_gpu -- --ignored"]
fn gpu_dxil_renders_triangle_from_vs_ps() {
    let Some(mut r) = renderer(256, 256) else {
        return;
    };
    let pid = r
        .create_pipeline_from_dxbc(
            &shader("exec_vs_tri"),
            &shader("exec_ps_tri"),
            std::mem::size_of::<VertexPosColor>() as u32,
        )
        .expect("pipeline DXIL VS+PS");
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
    // SAFETY: `VertexPosColor` est `#[repr(C)]` et ne contient que des `f32`.
    let vb = unsafe {
        std::slice::from_raw_parts(verts.as_ptr().cast::<u8>(), std::mem::size_of_val(&verts))
            .to_vec()
    };
    r.execute_commands(&[
        RenderCommand::Clear {
            rgba: [0.05, 0.05, 0.2, 1.0],
        },
        RenderCommand::SetPipeline { pipeline_id: pid },
        RenderCommand::SetVertexBuffer {
            data: vb,
            stride: std::mem::size_of::<VertexPosColor>() as u32,
        },
        RenderCommand::Draw {
            vertex_count: 3,
            instance_count: 1,
            first_vertex: 0,
            first_instance: 0,
        },
    ])
    .expect("rendu");
    let rgba = r.readback_rgba().expect("readback");
    let px = |x: usize, y: usize| {
        let i = (y * 256 + x) * 4;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    };
    let bg = px(8, 8);
    assert!(bg[2] > 40 && bg[0] < 25, "fond: {bg:?}");
    let top = px(128, 48);
    assert!(top[0] > 180 && top[1] < 60, "rouge: {top:?}");
    let br = px(204, 204);
    assert!(br[1] > 180 && br[0] < 60, "vert: {br:?}");
    let bl = px(52, 204);
    assert!(bl[2] > 180 && bl[0] < 60, "bleu: {bl:?}");
}
