//! Harnais PE32+ x86-64 **Cube 3D texturé rotatif Direct3D 12 Xbox Series X** :
//! Exécute un pipeline 3D de jeu complet depuis un vrai binaire Windows PE32+ sous ScarletBridge (sans Wine) :
//! - `d3d12_xs.dll` + `dxgi.dll` (SwapChain 512×512, Depth Buffer D32F, Index Buffer R16_UINT,
//!   Constant Buffer MVP 4×4, Texture 2D 64×64 damier téléversée via `GetCopyableFootprints` +
//!   `CopyTextureRegion`, SRV + Descriptor Table, `DrawIndexedInstanced(36)` sur 3 frames rotatives)
//! - `gameinput.dll` (`GameInputCreate` → `GetCurrentReading` → `GetGamepadState` via SDL3)
//! - `XboxBvhBuilder.dll` (`XboxBvhGetPrebuildInfo`) & `amd_fidelityfx_fsr2_xbox.dll` (`ffxFsr2ContextCreate`)
//! - Vérification pixel par pixel côté invité via `CopyResource` + `Map` (fond, face avant 3D, motif damier).

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "[d3d12_cube] Compiler pour PE32+ x86-64 :\n\
         cargo build --target x86_64-pc-windows-gnu -p sb-harness-d3d12-cube"
    );
}

#[cfg(windows)]
fn main() {
    std::process::exit(win_impl::run());
}

#[cfg(windows)]
mod win_impl {
    use core::ffi::c_void;
    use core::mem::{offset_of, size_of};
    use std::ptr;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Guid(u32, u16, u16, [u8; 8]);

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct DxgiSampleDesc {
        count: u32,
        quality: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct D3D12ShaderBytecode {
        shader_bytecode: *const u8,
        bytecode_length: usize,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct D3D12StreamOutputDesc {
        so_declaration: *const c_void,
        num_entries: u32,
        _pad0: u32,
        buffer_strides: *const u32,
        num_strides: u32,
        rasterized_stream: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct D3D12RenderTargetBlendDesc {
        blend_enable: i32,
        logic_op_enable: i32,
        src_blend: u32,
        dest_blend: u32,
        blend_op: u32,
        src_blend_alpha: u32,
        dest_blend_alpha: u32,
        blend_op_alpha: u32,
        logic_op: u32,
        render_target_write_mask: u8,
        _pad: [u8; 3],
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct D3D12BlendDesc {
        alpha_to_coverage_enable: i32,
        independent_blend_enable: i32,
        render_target: [D3D12RenderTargetBlendDesc; 8],
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct D3D12RasterizerDesc {
        fill_mode: u32,
        cull_mode: u32,
        front_counter_clockwise: i32,
        depth_bias: i32,
        depth_bias_clamp: f32,
        slope_scaled_depth_bias: f32,
        depth_clip_enable: i32,
        multisample_enable: i32,
        antialiased_line_enable: i32,
        forced_sample_count: u32,
        conservative_raster: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct D3D12DepthStencilOpDesc {
        stencil_fail_op: u32,
        stencil_depth_fail_op: u32,
        stencil_pass_op: u32,
        stencil_func: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct D3D12DepthStencilDesc {
        depth_enable: i32,
        depth_write_mask: u32,
        depth_func: u32,
        stencil_enable: i32,
        stencil_read_mask: u8,
        stencil_write_mask: u8,
        _pad: [u8; 2],
        front_face: D3D12DepthStencilOpDesc,
        back_face: D3D12DepthStencilOpDesc,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct D3D12InputLayoutDesc {
        input_element_descs: *const c_void,
        num_elements: u32,
        _pad: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct D3D12CachedPipelineState {
        cached_blob: *const c_void,
        cached_blob_size_in_bytes: usize,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct D3D12GraphicsPipelineStateDesc {
        root_signature: *mut c_void,
        vs: D3D12ShaderBytecode,
        ps: D3D12ShaderBytecode,
        ds: D3D12ShaderBytecode,
        hs: D3D12ShaderBytecode,
        gs: D3D12ShaderBytecode,
        stream_output: D3D12StreamOutputDesc,
        blend_state: D3D12BlendDesc,
        sample_mask: u32,
        rasterizer_state: D3D12RasterizerDesc,
        depth_stencil_state: D3D12DepthStencilDesc,
        input_layout: D3D12InputLayoutDesc,
        ib_strip_cut_value: u32,
        primitive_topology_type: u32,
        num_render_targets: u32,
        rtv_formats: [u32; 8],
        dsv_format: u32,
        sample_desc: DxgiSampleDesc,
        node_mask: u32,
        cached_pso: D3D12CachedPipelineState,
        flags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct D3D12CommandQueueDesc {
        queue_type: u32,
        priority: i32,
        flags: u32,
        node_mask: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct D3D12DescriptorHeapDesc {
        heap_type: u32,
        num_descriptors: u32,
        flags: u32,
        node_mask: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct D3D12HeapProperties {
        heap_type: u32,
        cpu_page_property: u32,
        memory_pool_preference: u32,
        creation_node_mask: u32,
        visible_node_mask: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct D3D12ResourceDesc {
        dimension: u32,
        _pad0: u32,
        alignment: u64,
        width: u64,
        height: u32,
        depth_or_array_size: u16,
        mip_levels: u16,
        format: u32,
        sample_desc: DxgiSampleDesc,
        layout: u32,
        flags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct D3D12VertexBufferView {
        buffer_location: u64,
        size_in_bytes: u32,
        stride_in_bytes: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct D3D12IndexBufferView {
        buffer_location: u64,
        size_in_bytes: u32,
        format: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct D3D12SubresourceFootprint {
        format: u32,
        width: u32,
        height: u32,
        depth: u32,
        row_pitch: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct D3D12PlacedSubresourceFootprint {
        offset: u64,
        footprint: D3D12SubresourceFootprint,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct D3D12TextureCopyLocation {
        resource: *mut c_void,
        copy_type: u32,
        _pad: u32,
        placed_footprint: D3D12PlacedSubresourceFootprint,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct D3D12Viewport {
        top_left_x: f32,
        top_left_y: f32,
        width: f32,
        height: f32,
        min_depth: f32,
        max_depth: f32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct DxgiSwapChainDesc1 {
        width: u32,
        height: u32,
        format: u32,
        stereo: i32,
        sample_desc: DxgiSampleDesc,
        buffer_usage: u32,
        buffer_count: u32,
        scaling: u32,
        swap_effect: u32,
        alpha_mode: u32,
        flags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct GameInputGamepadState {
        buttons: u32,
        left_trigger: f32,
        right_trigger: f32,
        left_thumbstick_x: f32,
        left_thumbstick_y: f32,
        right_thumbstick_x: f32,
        right_thumbstick_y: f32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct D3D12RaytracingPrebuildInfo {
        result_data_max_size: u64,
        scratch_data_size: u64,
        update_scratch_data_size: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct VertexPosUvColor {
        pos: [f32; 4],
        uv: [f32; 4],
        color: [f32; 4],
    }

    const _: () = {
        assert!(size_of::<D3D12GraphicsPipelineStateDesc>() == 656);
        assert!(size_of::<D3D12TextureCopyLocation>() == 48);
        assert!(offset_of!(D3D12TextureCopyLocation, placed_footprint) == 16);
        assert!(size_of::<VertexPosUvColor>() == 48);
    };

    const DUMMY_IID: Guid = Guid(0, 0, 0, [0; 8]);

    #[link(name = "d3d12_xs", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn D3D12CreateDevice(
            adapter: *mut c_void,
            min_feature_level: u32,
            riid: *const Guid,
            out_device: *mut *mut c_void,
        ) -> i32;
        fn D3D12SerializeRootSignature(
            desc: *const c_void,
            version: u32,
            out_blob: *mut *mut c_void,
            out_err: *mut *mut c_void,
        ) -> i32;
    }

    #[link(name = "dxgi", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn CreateDXGIFactory2(flags: u32, riid: *const Guid, out_factory: *mut *mut c_void) -> i32;
    }

    #[link(name = "gameinput", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn GameInputCreate(out_gi: *mut *mut c_void) -> i32;
    }

    #[link(name = "XboxBvhBuilder", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn XboxBvhGetPrebuildInfo(desc: *const c_void, out_info: *mut D3D12RaytracingPrebuildInfo);
    }

    #[link(name = "amd_fidelityfx_fsr2_xbox", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn ffxFsr2ContextCreate(ctx: *mut c_void, desc: *const c_void) -> i32;
        fn ffxFsr2ContextDestroy(ctx: *mut c_void) -> i32;
    }

    unsafe fn vtbl_fn<F: Copy>(obj: *mut c_void, index: usize) -> F {
        // SAFETY: déréférencement vtable COM x86-64.
        unsafe {
            let vtbl = *(obj.cast::<*const *const c_void>());
            let fn_ptr = *vtbl.add(index);
            core::mem::transmute_copy(&fn_ptr)
        }
    }

    // ==================== Shaders DXBC SM5.0 (MVP cb0[0..4] + Texture t0/s0) ====================

    fn build_sig_chunk(entries: &[(&str, u32, u32, u32)]) -> Vec<u8> {
        let count = entries.len() as u32;
        let header_size = 8 + entries.len() * 24;
        let mut names = Vec::new();
        let mut name_offsets = Vec::new();
        for &(name, _, _, _) in entries {
            name_offsets.push((header_size + names.len()) as u32);
            names.extend_from_slice(name.as_bytes());
            names.push(0);
        }
        while !names.len().is_multiple_of(4) {
            names.push(0);
        }
        let mut out = Vec::with_capacity(header_size + names.len());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&8u32.to_le_bytes());
        for (idx, &(_, sem_idx, sys_val, reg)) in entries.iter().enumerate() {
            out.extend_from_slice(&name_offsets[idx].to_le_bytes());
            out.extend_from_slice(&sem_idx.to_le_bytes());
            out.extend_from_slice(&sys_val.to_le_bytes());
            out.extend_from_slice(&3u32.to_le_bytes());
            out.extend_from_slice(&reg.to_le_bytes());
            out.extend_from_slice(&[0x0F, 0x0F, 0, 0]);
        }
        out.extend_from_slice(&names);
        out
    }

    fn build_dxbc(isgn: &[u8], osgn: &[u8], shex_words: &[u32]) -> Vec<u8> {
        let mut shex = Vec::with_capacity(shex_words.len() * 4);
        for &w in shex_words {
            shex.extend_from_slice(&w.to_le_bytes());
        }
        let chunks: [(&[u8; 4], &[u8]); 3] = [(b"ISGN", isgn), (b"OSGN", osgn), (b"SHEX", &shex)];
        let header_len = 32 + 4 * chunks.len();
        let mut total = header_len;
        let mut offsets = Vec::new();
        for &(_, payload) in &chunks {
            offsets.push(total as u32);
            total += 8 + payload.len();
        }
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(b"DXBC");
        out.extend_from_slice(&[0u8; 16]);
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
        for off in offsets {
            out.extend_from_slice(&off.to_le_bytes());
        }
        for &(tag, payload) in &chunks {
            out.extend_from_slice(tag);
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(payload);
        }
        out
    }

    fn make_cube_vs_dxbc() -> Vec<u8> {
        let isgn = build_sig_chunk(&[
            ("POSITION", 0, 0, 0),
            ("TEXCOORD", 0, 0, 1),
            ("COLOR", 0, 0, 2),
        ]);
        let osgn = build_sig_chunk(&[
            ("SV_POSITION", 0, 1, 0),
            ("TEXCOORD", 0, 0, 1),
            ("COLOR", 0, 0, 2),
        ]);
        let dst_o = |mask: u32| -> u32 { 2 | (mask << 4) | (2u32 << 12) | (1 << 20) };
        let src_v = 2 | (1 << 2) | (0xE4 << 4) | (1u32 << 12) | (1 << 20);
        let src_cb = 2 | (1 << 2) | (0xE4 << 4) | (8u32 << 12) | (2 << 20);
        let op_dp4 = (8u32 << 24) | 0x11;
        let op_mov = (5u32 << 24) | 0x36;
        let op_ret = (1u32 << 24) | 0x3E;

        let mut words = vec![
            0x0001_0050,
            0,
            op_dp4,
            dst_o(1),
            0,
            src_v,
            0,
            src_cb,
            0,
            0,
            op_dp4,
            dst_o(2),
            0,
            src_v,
            0,
            src_cb,
            0,
            1,
            op_dp4,
            dst_o(4),
            0,
            src_v,
            0,
            src_cb,
            0,
            2,
            op_dp4,
            dst_o(8),
            0,
            src_v,
            0,
            src_cb,
            0,
            3,
            op_mov,
            dst_o(0xF),
            1,
            src_v,
            1,
            op_mov,
            dst_o(0xF),
            2,
            src_v,
            2,
            op_ret,
        ];
        words[1] = words.len() as u32;
        build_dxbc(&isgn, &osgn, &words)
    }

    fn make_cube_ps_dxbc() -> Vec<u8> {
        let isgn = build_sig_chunk(&[
            ("SV_POSITION", 0, 1, 0),
            ("TEXCOORD", 0, 0, 1),
            ("COLOR", 0, 0, 2),
        ]);
        let osgn = build_sig_chunk(&[("SV_TARGET", 0, 0, 0)]);
        let dst_r = |mask: u32| -> u32 { 2 | (mask << 4) | (1 << 20) };
        let dst_o = |mask: u32| -> u32 { 2 | (mask << 4) | (2u32 << 12) | (1 << 20) };
        let src_v = 2 | (1 << 2) | (0xE4 << 4) | (1u32 << 12) | (1 << 20);
        let src_r = 2 | (1 << 2) | (0xE4 << 4) | (1 << 20);
        let src_t = 2 | (1 << 2) | (0xE4 << 4) | (7u32 << 12) | (1 << 20);
        let src_s = (6u32 << 12) | (1 << 20);

        let mut words = vec![
            0x0000_0050,
            0,
            (2u32 << 24) | 0x68,
            1, // dcl_temps 1
            (9u32 << 24) | 0x45,
            dst_r(0xF),
            0,
            src_v,
            1,
            src_t,
            0,
            src_s,
            0, // sample r0, v1, t0, s0
            (7u32 << 24) | 0x38,
            dst_o(0xF),
            0,
            src_r,
            0,
            src_v,
            2, // mul o0, r0, v2
            (1u32 << 24) | 0x3E,
        ];
        words[1] = words.len() as u32;
        build_dxbc(&isgn, &osgn, &words)
    }

    // ==================== Math 3D (Matrice MVP 4×4 row-major pour `dp4`) ====================

    fn mat4_mul(a: [[f32; 4]; 4], b: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
        let mut r = [[0.0f32; 4]; 4];
        for i in 0..4 {
            for j in 0..4 {
                r[i][j] =
                    a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j] + a[i][3] * b[3][j];
            }
        }
        r
    }

    fn build_mvp(angle_y: f32, angle_x: f32) -> [f32; 16] {
        let (sy, cy) = angle_y.sin_cos();
        let (sx, cx) = angle_x.sin_cos();
        let rot_y = [
            [cy, 0.0, sy, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [-sy, 0.0, cy, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let rot_x = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, cx, -sx, 0.0],
            [0.0, sx, cx, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        // Caméra reculée à z = +3.2 (repère main gauche D3D : +Z devant la caméra)
        let trans = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 3.2],
            [0.0, 0.0, 0.0, 1.0],
        ];
        // Perspective D3D (FOV 60°, aspect 1.0, near=0.1, far=50.0, z dans [0..1])
        let f = 1.0 / (std::f32::consts::FRAC_PI_6).tan();
        let near = 0.1f32;
        let far = 50.0f32;
        let proj = [
            [f, 0.0, 0.0, 0.0],
            [0.0, f, 0.0, 0.0],
            [0.0, 0.0, far / (far - near), -(far * near) / (far - near)],
            [0.0, 0.0, 1.0, 0.0],
        ];
        let mv = mat4_mul(trans, mat4_mul(rot_x, rot_y));
        let mvp = mat4_mul(proj, mv);
        let mut flat = [0.0f32; 16];
        for r in 0..4 {
            for c in 0..4 {
                flat[r * 4 + c] = mvp[r][c];
            }
        }
        flat
    }

    fn build_cube_mesh() -> (Vec<VertexPosUvColor>, Vec<u16>) {
        let mut verts = Vec::with_capacity(24);
        let mut indices = Vec::with_capacity(36);

        let faces: [([[f32; 3]; 4], [f32; 4]); 6] = [
            // Face avant (z = -1) : Cyan lumineux
            (
                [
                    [-1.0, -1.0, -1.0],
                    [1.0, -1.0, -1.0],
                    [1.0, 1.0, -1.0],
                    [-1.0, 1.0, -1.0],
                ],
                [0.25, 0.95, 1.0, 1.0],
            ),
            // Face arrière (z = +1) : Magenta
            (
                [
                    [1.0, -1.0, 1.0],
                    [-1.0, -1.0, 1.0],
                    [-1.0, 1.0, 1.0],
                    [1.0, 1.0, 1.0],
                ],
                [1.0, 0.25, 0.85, 1.0],
            ),
            // Face droite (x = +1) : Or
            (
                [
                    [1.0, -1.0, -1.0],
                    [1.0, -1.0, 1.0],
                    [1.0, 1.0, 1.0],
                    [1.0, 1.0, -1.0],
                ],
                [1.0, 0.80, 0.20, 1.0],
            ),
            // Face gauche (x = -1) : Vert émeraude
            (
                [
                    [-1.0, -1.0, 1.0],
                    [-1.0, -1.0, -1.0],
                    [-1.0, 1.0, -1.0],
                    [-1.0, 1.0, 1.0],
                ],
                [0.20, 1.0, 0.45, 1.0],
            ),
            // Face haut (y = +1) : Bleu ciel
            (
                [
                    [-1.0, 1.0, -1.0],
                    [1.0, 1.0, -1.0],
                    [1.0, 1.0, 1.0],
                    [-1.0, 1.0, 1.0],
                ],
                [0.45, 0.70, 1.0, 1.0],
            ),
            // Face bas (y = -1) : Corail
            (
                [
                    [-1.0, -1.0, 1.0],
                    [1.0, -1.0, 1.0],
                    [1.0, -1.0, -1.0],
                    [-1.0, -1.0, -1.0],
                ],
                [1.0, 0.35, 0.30, 1.0],
            ),
        ];
        let uvs = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];

        for (corners, color) in faces {
            let base = verts.len() as u16;
            for i in 0..4 {
                verts.push(VertexPosUvColor {
                    pos: [corners[i][0], corners[i][1], corners[i][2], 1.0],
                    uv: [uvs[i][0], uvs[i][1], 0.0, 0.0],
                    color,
                });
            }
            indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        (verts, indices)
    }

    pub fn run() -> i32 {
        println!("[d3d12_cube] === Harnais PE32+ Cube 3D Texturé Direct3D 12 XS -> Vulkan 1.4 ===");

        const W: u32 = 512;
        const H: u32 = 512;
        const TEX_SIZE: u32 = 64;

        // SAFETY: appels COM `extern "win64"` résolus nativement par ScarletBridge (`sb-hle`).
        unsafe {
            // 0. Vérification GameInput + BVH Raytracing + FSR2 Xbox
            let mut gi: *mut c_void = ptr::null_mut();
            assert_eq!(GameInputCreate(&mut gi), 0);
            let gi_get_reading: extern "win64" fn(
                *mut c_void,
                u32,
                *mut c_void,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(gi, 4);
            let mut reading: *mut c_void = ptr::null_mut();
            assert_eq!(
                gi_get_reading(gi, 0x20000, ptr::null_mut(), &mut reading),
                0
            );
            let get_pad: extern "win64" fn(*mut c_void, *mut GameInputGamepadState) -> i32 =
                vtbl_fn(reading, 14);
            let mut pad = GameInputGamepadState::default();
            assert_eq!(get_pad(reading, &mut pad), 1);

            // Sans lancer de rayons côté GPU (`rt=false`, voir `sb-gpu`), le bridge ne prétend plus
            // réserver de mémoire BVH : tailles nulles, constructions sans effet. L'appel doit
            // réussir sans planter et laisser la structure de sortie cohérente (zéro).
            let mut bvh_info = D3D12RaytracingPrebuildInfo {
                result_data_max_size: u64::MAX,
                ..Default::default()
            };
            XboxBvhGetPrebuildInfo(ptr::null(), &mut bvh_info);
            assert_eq!(bvh_info.result_data_max_size, 0);
            assert_eq!(ffxFsr2ContextCreate(ptr::null_mut(), ptr::null()), 0);
            assert_eq!(ffxFsr2ContextDestroy(ptr::null_mut()), 0);
            println!(
                "[d3d12_cube] Services Series X OK (GameInput SDL3, BVH={}o, FSR2)",
                bvh_info.result_data_max_size
            );

            // 1. Device + Queue + SwapChain 512×512
            let mut dev: *mut c_void = ptr::null_mut();
            assert_eq!(
                D3D12CreateDevice(ptr::null_mut(), 0xC100, &DUMMY_IID, &mut dev),
                0
            );
            let create_queue: extern "win64" fn(
                *mut c_void,
                *const D3D12CommandQueueDesc,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 8);
            let mut queue: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_queue(
                    dev,
                    &D3D12CommandQueueDesc::default(),
                    &DUMMY_IID,
                    &mut queue
                ),
                0
            );

            let mut factory: *mut c_void = ptr::null_mut();
            assert_eq!(CreateDXGIFactory2(0, &DUMMY_IID, &mut factory), 0);
            let create_sc: extern "win64" fn(
                *mut c_void,
                *mut c_void,
                usize,
                *const DxgiSwapChainDesc1,
                *const c_void,
                *mut c_void,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(factory, 15);
            let sc_desc = DxgiSwapChainDesc1 {
                width: W,
                height: H,
                format: 28,
                buffer_count: 2,
                sample_desc: DxgiSampleDesc {
                    count: 1,
                    quality: 0,
                },
                ..Default::default()
            };
            let mut sc: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_sc(
                    factory,
                    queue,
                    0,
                    &sc_desc,
                    ptr::null(),
                    ptr::null_mut(),
                    &mut sc
                ),
                0
            );
            let sc_get_buf: extern "win64" fn(
                *mut c_void,
                u32,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(sc, 9);
            let mut backbuffer: *mut c_void = ptr::null_mut();
            assert_eq!(sc_get_buf(sc, 0, &DUMMY_IID, &mut backbuffer), 0);

            // 2. RootSignature + Pipeline 3D (MVP cb0 + Texture t0/s0)
            let mut rs_blob: *mut c_void = ptr::null_mut();
            assert_eq!(
                D3D12SerializeRootSignature(ptr::null(), 1, &mut rs_blob, ptr::null_mut()),
                0
            );
            let create_rs: extern "win64" fn(
                *mut c_void,
                u32,
                *const c_void,
                usize,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 16);
            let mut root_sig: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_rs(dev, 0, ptr::null(), 0, &DUMMY_IID, &mut root_sig),
                0
            );

            let vs_dxbc = make_cube_vs_dxbc();
            let ps_dxbc = make_cube_ps_dxbc();
            let pso_desc = D3D12GraphicsPipelineStateDesc {
                root_signature: root_sig,
                vs: D3D12ShaderBytecode {
                    shader_bytecode: vs_dxbc.as_ptr(),
                    bytecode_length: vs_dxbc.len(),
                },
                ps: D3D12ShaderBytecode {
                    shader_bytecode: ps_dxbc.as_ptr(),
                    bytecode_length: ps_dxbc.len(),
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
                depth_stencil_state: D3D12DepthStencilDesc {
                    depth_enable: 1,
                    depth_write_mask: 1,
                    depth_func: 4, // LESS_EQUAL
                    ..Default::default()
                },
                input_layout: D3D12InputLayoutDesc {
                    input_element_descs: ptr::null(),
                    num_elements: 3,
                    _pad: 0,
                },
                ib_strip_cut_value: 0,
                primitive_topology_type: 3,
                num_render_targets: 1,
                rtv_formats: [28, 0, 0, 0, 0, 0, 0, 0],
                dsv_format: 40, // D32_FLOAT
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
            let create_pso: extern "win64" fn(
                *mut c_void,
                *const D3D12GraphicsPipelineStateDesc,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 10);
            let mut pso: *mut c_void = ptr::null_mut();
            assert_eq!(create_pso(dev, &pso_desc, &DUMMY_IID, &mut pso), 0);

            // 3. Ressources : VB (24 sommets), IB (36 indices), CB (MVP), Texture 64×64 + Staging, Readback
            let (verts, indices) = build_cube_mesh();
            let create_res: extern "win64" fn(
                *mut c_void,
                *const D3D12HeapProperties,
                u32,
                *const D3D12ResourceDesc,
                u32,
                *const c_void,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 27);
            let upload_hp = D3D12HeapProperties {
                heap_type: 2,
                ..Default::default()
            };
            let default_hp = D3D12HeapProperties {
                heap_type: 1,
                ..Default::default()
            };
            let readback_hp = D3D12HeapProperties {
                heap_type: 3,
                ..Default::default()
            };

            let make_buf = |hp: &D3D12HeapProperties, size: u64| -> *mut c_void {
                let d = D3D12ResourceDesc {
                    dimension: 1,
                    width: size,
                    height: 1,
                    depth_or_array_size: 1,
                    mip_levels: 1,
                    ..Default::default()
                };
                let mut out: *mut c_void = ptr::null_mut();
                assert_eq!(
                    create_res(dev, hp, 0, &d, 0, ptr::null(), &DUMMY_IID, &mut out),
                    0
                );
                out
            };

            let vb = make_buf(
                &upload_hp,
                (verts.len() * size_of::<VertexPosUvColor>()) as u64,
            );
            let ib = make_buf(&upload_hp, (indices.len() * size_of::<u16>()) as u64);
            let cb = make_buf(&upload_hp, 256);
            let rb = make_buf(&readback_hp, u64::from(W * H * 4));

            let res_map: extern "win64" fn(
                *mut c_void,
                u32,
                *const c_void,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(vb, 8);
            let res_gpu_va: extern "win64" fn(*mut c_void) -> u64 = vtbl_fn(vb, 11);

            let mut p: *mut c_void = ptr::null_mut();
            assert_eq!(res_map(vb, 0, ptr::null(), &mut p), 0);
            ptr::copy_nonoverlapping(
                verts.as_ptr().cast::<u8>(),
                p.cast::<u8>(),
                verts.len() * size_of::<VertexPosUvColor>(),
            );
            assert_eq!(res_map(ib, 0, ptr::null(), &mut p), 0);
            ptr::copy_nonoverlapping(
                indices.as_ptr().cast::<u8>(),
                p.cast::<u8>(),
                indices.len() * size_of::<u16>(),
            );

            // Texture 2D 64×64 (damier 8×8 blanc / gris foncé) + CopyableFootprints + Staging
            let tex_desc = D3D12ResourceDesc {
                dimension: 3, // TEXTURE2D
                width: u64::from(TEX_SIZE),
                height: TEX_SIZE,
                depth_or_array_size: 1,
                mip_levels: 1,
                format: 28,
                sample_desc: DxgiSampleDesc {
                    count: 1,
                    quality: 0,
                },
                ..Default::default()
            };
            let mut tex: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_res(
                    dev,
                    &default_hp,
                    0,
                    &tex_desc,
                    0,
                    ptr::null(),
                    &DUMMY_IID,
                    &mut tex
                ),
                0
            );

            let get_fp: extern "win64" fn(
                *mut c_void,
                *const D3D12ResourceDesc,
                u32,
                u32,
                u64,
                *mut D3D12PlacedSubresourceFootprint,
                *mut u32,
                *mut u64,
                *mut u64,
            ) = vtbl_fn(dev, 38);
            let mut footprint = D3D12PlacedSubresourceFootprint::default();
            let mut total_tex_bytes = 0u64;
            get_fp(
                dev,
                &tex_desc,
                0,
                1,
                0,
                &mut footprint,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut total_tex_bytes,
            );
            let tex_staging = make_buf(&upload_hp, total_tex_bytes);
            assert_eq!(res_map(tex_staging, 0, ptr::null(), &mut p), 0);
            let pitch = footprint.footprint.row_pitch as usize;
            let dst_stage =
                std::slice::from_raw_parts_mut(p.cast::<u8>(), total_tex_bytes as usize);
            for y in 0..TEX_SIZE as usize {
                for x in 0..TEX_SIZE as usize {
                    let dark = ((x / 8) + (y / 8)) % 2 == 0;
                    let c: [u8; 4] = if dark {
                        [55, 55, 65, 255]
                    } else {
                        [250, 250, 250, 255]
                    };
                    let off = y * pitch + x * 4;
                    dst_stage[off..off + 4].copy_from_slice(&c);
                }
            }

            // Heap SRV + CreateShaderResourceView
            let create_heap: extern "win64" fn(
                *mut c_void,
                *const D3D12DescriptorHeapDesc,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 14);
            let mut srv_heap: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_heap(
                    dev,
                    &D3D12DescriptorHeapDesc {
                        heap_type: 0, // CBV_SRV_UAV
                        num_descriptors: 1,
                        flags: 1, // SHADER_VISIBLE
                        node_mask: 0,
                    },
                    &DUMMY_IID,
                    &mut srv_heap
                ),
                0
            );
            let heap_cpu: extern "win64" fn(*mut c_void) -> usize = vtbl_fn(srv_heap, 9);
            let heap_gpu: extern "win64" fn(*mut c_void) -> u64 = vtbl_fn(srv_heap, 10);
            let create_srv: extern "win64" fn(*mut c_void, *mut c_void, *const c_void, usize) =
                vtbl_fn(dev, 18);
            create_srv(dev, tex, ptr::null(), heap_cpu(srv_heap));

            // 4. CommandAllocator + CommandList + 3 frames rotatives du cube 3D !
            let create_alloc: extern "win64" fn(
                *mut c_void,
                u32,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 9);
            let mut alloc: *mut c_void = ptr::null_mut();
            assert_eq!(create_alloc(dev, 0, &DUMMY_IID, &mut alloc), 0);

            let create_cl: extern "win64" fn(
                *mut c_void,
                u32,
                u32,
                *mut c_void,
                *mut c_void,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 12);
            let mut cl: *mut c_void = ptr::null_mut();
            assert_eq!(create_cl(dev, 0, 0, alloc, pso, &DUMMY_IID, &mut cl), 0);

            let cl_close: extern "win64" fn(*mut c_void) -> i32 = vtbl_fn(cl, 9);
            let cl_reset: extern "win64" fn(*mut c_void, *mut c_void, *mut c_void) -> i32 =
                vtbl_fn(cl, 10);
            let cl_draw_idx: extern "win64" fn(*mut c_void, u32, u32, u32, i32, u32) =
                vtbl_fn(cl, 13);
            let cl_copy_tex: extern "win64" fn(
                *mut c_void,
                *const D3D12TextureCopyLocation,
                u32,
                u32,
                u32,
                *const D3D12TextureCopyLocation,
                *const c_void,
            ) = vtbl_fn(cl, 16);
            let cl_copy_res: extern "win64" fn(*mut c_void, *mut c_void, *mut c_void) =
                vtbl_fn(cl, 17);
            let cl_vp: extern "win64" fn(*mut c_void, u32, *const D3D12Viewport) = vtbl_fn(cl, 21);
            let cl_set_table: extern "win64" fn(*mut c_void, u32, u64) = vtbl_fn(cl, 32);
            let cl_set_cbv: extern "win64" fn(*mut c_void, u32, u64) = vtbl_fn(cl, 38);
            let cl_set_ib: extern "win64" fn(*mut c_void, *const D3D12IndexBufferView) =
                vtbl_fn(cl, 43);
            let cl_set_vb: extern "win64" fn(*mut c_void, u32, u32, *const D3D12VertexBufferView) =
                vtbl_fn(cl, 44);
            let cl_clear_dsv: extern "win64" fn(
                *mut c_void,
                usize,
                u32,
                f32,
                u8,
                u32,
                *const c_void,
            ) = vtbl_fn(cl, 47);
            let cl_clear_rtv: extern "win64" fn(
                *mut c_void,
                usize,
                *const [f32; 4],
                u32,
                *const c_void,
            ) = vtbl_fn(cl, 48);
            let q_exec: extern "win64" fn(*mut c_void, u32, *const *mut c_void) =
                vtbl_fn(queue, 10);
            let sc_present: extern "win64" fn(*mut c_void, u32, u32) -> i32 = vtbl_fn(sc, 8);

            let vp = D3D12Viewport {
                top_left_x: 0.0,
                top_left_y: 0.0,
                width: W as f32,
                height: H as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            };
            let vbv = D3D12VertexBufferView {
                buffer_location: res_gpu_va(vb),
                size_in_bytes: (verts.len() * size_of::<VertexPosUvColor>()) as u32,
                stride_in_bytes: size_of::<VertexPosUvColor>() as u32,
            };
            let ibv = D3D12IndexBufferView {
                buffer_location: res_gpu_va(ib),
                size_in_bytes: (indices.len() * size_of::<u16>()) as u32,
                format: 57, // DXGI_FORMAT_R16_UINT
            };
            let clear = [0.06f32, 0.06, 0.14, 1.0];

            let frames = 3u32;
            for frame in 0..frames {
                if frame > 0 {
                    assert_eq!(cl_reset(cl, alloc, pso), 0);
                } else {
                    // Frame 0 : téléversement GPU de la texture damier 64×64 par `CopyTextureRegion`.
                    let dst_loc = D3D12TextureCopyLocation {
                        resource: tex,
                        copy_type: 0,
                        _pad: 0,
                        placed_footprint: D3D12PlacedSubresourceFootprint::default(),
                    };
                    let src_loc = D3D12TextureCopyLocation {
                        resource: tex_staging,
                        copy_type: 1,
                        _pad: 0,
                        placed_footprint: footprint,
                    };
                    cl_copy_tex(cl, &dst_loc, 0, 0, 0, &src_loc, ptr::null());
                }

                // Met à jour la matrice MVP 4×4 (rotation Y + X).
                let angle_y = 0.45 + (frame as f32) * 0.18;
                let angle_x = 0.35;
                let mvp = build_mvp(angle_y, angle_x);
                assert_eq!(res_map(cb, 0, ptr::null(), &mut p), 0);
                ptr::copy_nonoverlapping(
                    mvp.as_ptr().cast::<u8>(),
                    p.cast::<u8>(),
                    size_of_val(&mvp),
                );

                cl_vp(cl, 1, &vp);
                cl_clear_rtv(cl, 0, &clear, 0, ptr::null());
                cl_clear_dsv(cl, 0, 1, 1.0, 0, 0, ptr::null());
                cl_set_cbv(cl, 0, res_gpu_va(cb));
                cl_set_table(cl, 1, heap_gpu(srv_heap));
                cl_set_vb(cl, 0, 1, &vbv);
                cl_set_ib(cl, &ibv);
                cl_draw_idx(cl, indices.len() as u32, 1, 0, 0, 0);
                if frame == frames - 1 {
                    cl_copy_res(cl, rb, backbuffer);
                }
                assert_eq!(cl_close(cl), 0);
                let arr = [cl];
                q_exec(queue, 1, arr.as_ptr());
                assert_eq!(sc_present(sc, 1, 0), 0);
            }

            // 5. Vérification pixel par pixel du cube 3D texturé + depth buffer !
            assert_eq!(res_map(rb, 0, ptr::null(), &mut p), 0);
            let pixels = std::slice::from_raw_parts(p.cast::<u8>(), (W * H * 4) as usize);
            let px = |x: usize, y: usize| -> [u8; 4] {
                let i = (y * (W as usize) + x) * 4;
                [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
            };

            let bg = px(12, 12);
            assert!(
                bg[2] > 25 && bg[0] < 30 && bg[1] < 30,
                "fond inattendu: {bg:?}"
            );

            // Mesure la zone centrale du cube (192..320) : doit contenir des pixels brillants ET sombres (damier) !
            let mut min_luma = 255u32;
            let mut max_luma = 0u32;
            let mut non_bg_pixels = 0usize;
            for y in 192..320 {
                for x in 192..320 {
                    let c = px(x, y);
                    let luma = (u32::from(c[0]) + u32::from(c[1]) + u32::from(c[2])) / 3;
                    if c[0] > 35 || c[1] > 35 || c[2] > 55 {
                        non_bg_pixels += 1;
                        min_luma = min_luma.min(luma);
                        max_luma = max_luma.max(luma);
                    }
                }
            }
            let center = px(256, 256);
            println!(
                "[d3d12_cube] Readback GPU 512x512 (3 frames 3D) :\n\
                 - Fond (12,12)        = RGBA{bg:?}\n\
                 - Centre (256,256)    = RGBA{center:?}\n\
                 - Pixels cube centre  = {non_bg_pixels} / 16384\n\
                 - Contraste damier    = min_luma={min_luma}, max_luma={max_luma}"
            );
            assert!(
                non_bg_pixels > 10_000,
                "le cube 3D doit couvrir la zone centrale (trouvé {non_bg_pixels})"
            );
            assert!(
                max_luma > min_luma + 60,
                "le motif damier texturé doit présenter un vrai contraste (min={min_luma}, max={max_luma})"
            );
        }

        println!("[d3d12_cube] === CUBE 3D TEXTURÉ + DEPTH + MVP + INDEX BUFFER VÉRIFIÉ ===");
        0
    }
}
