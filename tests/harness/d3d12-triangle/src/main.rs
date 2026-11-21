//! Harnais PE32+ x86-64 **Direct3D 12 Xbox Series X (`d3d12_xs.dll` + `dxgi.dll`)** :
//! Exécute un pipeline 3D complet depuis un vrai binaire Windows PE32+ sous ScarletBridge (sans Wine) :
//! 1. Crée `ID3D12Device`, `ID3D12CommandQueue`, `IDXGIFactory4`, `IDXGISwapChain3` (256x256).
//! 2. Construit deux shaders DXBC SM5.0 (Vertex Shader + Pixel Shader) et appelle `CreateGraphicsPipelineState`.
//! 3. Alloue un Vertex Buffer (3 sommets RGB) et un Readback Buffer via `CreateCommittedResource` + `Map`.
//! 4. Enregistre `ClearRenderTargetView` + `RSSetViewports` + `IASetVertexBuffers` + `DrawInstanced(3)` + `CopyResource`.
//! 5. Soumet sur `ExecuteCommandLists`, synchronise par `ID3D12Fence` + `WaitForSingleObject`, appelle `Present`.
//! 6. Relit les pixels RGBA8 réellement rendus par le GPU Vulkan et vérifie chaque sommet (Rouge/Vert/Bleu + fond).

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "[d3d12_triangle] Compiler pour PE32+ x86-64 :\n\
         cargo build --target x86_64-pc-windows-gnu -p sb-harness-d3d12-triangle"
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
    #[derive(Clone, Copy)]
    struct VertexPosColor {
        pos: [f32; 4],
        color: [f32; 4],
    }

    const _: () = {
        assert!(size_of::<D3D12GraphicsPipelineStateDesc>() == 656);
        assert!(offset_of!(D3D12GraphicsPipelineStateDesc, vs) == 8);
        assert!(offset_of!(D3D12GraphicsPipelineStateDesc, ps) == 24);
        assert!(offset_of!(D3D12GraphicsPipelineStateDesc, input_layout) == 552);
        assert!(size_of::<D3D12ResourceDesc>() == 56);
        assert!(size_of::<D3D12VertexBufferView>() == 16);
        assert!(size_of::<DxgiSwapChainDesc1>() == 48);
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

    #[link(name = "kernel32", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn CreateEventW(
            sec: *mut c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> usize;
        fn WaitForSingleObject(handle: usize, timeout_ms: u32) -> u32;
        fn CloseHandle(handle: usize) -> i32;
    }

    /// Récupère le pointeur de fonction au slot `index` de la vtable COM d'un objet `obj`.
    ///
    /// # Safety
    /// `obj` doit être un pointeur COM valide dont le premier champ pointe vers une vtable contenant `index`.
    unsafe fn vtbl_fn<F: Copy>(obj: *mut c_void, index: usize) -> F {
        // SAFETY: déréférencement vtable COM x86-64.
        unsafe {
            let vtbl = *(obj.cast::<*const *const c_void>());
            let fn_ptr = *vtbl.add(index);
            core::mem::transmute_copy(&fn_ptr)
        }
    }

    // ==================== Constructeur DXBC SM5.0 embarqué ====================

    fn build_sig_chunk(entries: &[(&str, u32, u32, u32, u8)]) -> Vec<u8> {
        let count = entries.len() as u32;
        let header_size = 8 + (entries.len() * 24);
        let mut names = Vec::new();
        let mut name_offsets = Vec::new();
        for &(name, _, _, _, _) in entries {
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
        for (idx, &(_, sem_idx, sys_val, reg, mask)) in entries.iter().enumerate() {
            out.extend_from_slice(&name_offsets[idx].to_le_bytes());
            out.extend_from_slice(&sem_idx.to_le_bytes());
            out.extend_from_slice(&sys_val.to_le_bytes());
            out.extend_from_slice(&3u32.to_le_bytes()); // FLOAT32
            out.extend_from_slice(&reg.to_le_bytes());
            out.push(mask);
            out.push(mask);
            out.extend_from_slice(&[0u8, 0u8]);
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

    fn make_vs_dxbc() -> Vec<u8> {
        let isgn = build_sig_chunk(&[("POSITION", 0, 0, 0, 0x0F), ("COLOR", 0, 0, 1, 0x0F)]);
        let osgn = build_sig_chunk(&[("SV_POSITION", 0, 1, 0, 0x0F), ("COLOR", 0, 0, 1, 0x0F)]);
        // SM5.0 Vertex Shader (0x0001_0050) :
        //   mov o0.xyzw, v0.xyzw
        //   mov o1.xyzw, v1.xyzw
        //   ret
        let dst_mask = (2u32) | (0xFu32 << 4) | (1u32 << 20);
        let src_swiz = (2u32) | (1u32 << 2) | (0xE4u32 << 4) | (1u32 << 20);
        let op_mov = 0x36u32 | (5u32 << 24);
        let op_ret = 0x3Eu32 | (1u32 << 24);
        let mut words = vec![
            0x0001_0050,
            0, // taille remplie ci-dessous
            op_mov,
            dst_mask | (2u32 << 12),
            0, // o0
            src_swiz | (1u32 << 12),
            0, // v0
            op_mov,
            dst_mask | (2u32 << 12),
            1, // o1
            src_swiz | (1u32 << 12),
            1, // v1
            op_ret,
        ];
        words[1] = words.len() as u32;
        build_dxbc(&isgn, &osgn, &words)
    }

    fn make_ps_dxbc() -> Vec<u8> {
        let isgn = build_sig_chunk(&[("SV_POSITION", 0, 1, 0, 0x0F), ("COLOR", 0, 0, 1, 0x0F)]);
        let osgn = build_sig_chunk(&[("SV_TARGET", 0, 0, 0, 0x0F)]);
        // SM5.0 Pixel Shader (0x0000_0050) :
        //   mov o0.xyzw, v1.xyzw
        //   ret
        let dst_mask = (2u32) | (0xFu32 << 4) | (1u32 << 20);
        let src_swiz = (2u32) | (1u32 << 2) | (0xE4u32 << 4) | (1u32 << 20);
        let op_mov = 0x36u32 | (5u32 << 24);
        let op_ret = 0x3Eu32 | (1u32 << 24);
        let mut words = vec![
            0x0000_0050,
            0,
            op_mov,
            dst_mask | (2u32 << 12),
            0, // o0
            src_swiz | (1u32 << 12),
            1, // v1
            op_ret,
        ];
        words[1] = words.len() as u32;
        build_dxbc(&isgn, &osgn, &words)
    }

    pub fn run() -> i32 {
        println!("[d3d12_triangle] === Harnais PE32+ 3D Direct3D 12 XS -> Vulkan 1.4 ===");

        const W: u32 = 256;
        const H: u32 = 256;

        // SAFETY: appels COM `extern "win64"` résolus nativement par ScarletBridge (`sb-hle`).
        unsafe {
            // 1. Device + CommandQueue + DXGI Factory4 + SwapChain3
            let mut dev: *mut c_void = ptr::null_mut();
            assert_eq!(
                D3D12CreateDevice(ptr::null_mut(), 0xC100, &DUMMY_IID, &mut dev),
                0,
                "D3D12CreateDevice"
            );
            assert!(!dev.is_null());

            let create_queue: extern "win64" fn(
                *mut c_void,
                *const D3D12CommandQueueDesc,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 8);
            let q_desc = D3D12CommandQueueDesc::default();
            let mut queue: *mut c_void = ptr::null_mut();
            assert_eq!(create_queue(dev, &q_desc, &DUMMY_IID, &mut queue), 0);

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
                format: 28, // DXGI_FORMAT_R8G8B8A8_UNORM
                buffer_count: 2,
                sample_desc: DxgiSampleDesc {
                    count: 1,
                    quality: 0,
                },
                ..Default::default()
            };
            let mut swapchain: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_sc(
                    factory,
                    queue,
                    0,
                    &sc_desc,
                    ptr::null(),
                    ptr::null_mut(),
                    &mut swapchain
                ),
                0
            );

            let sc_get_buf: extern "win64" fn(
                *mut c_void,
                u32,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(swapchain, 9);
            let sc_cur_idx: extern "win64" fn(*mut c_void) -> u32 = vtbl_fn(swapchain, 36);
            let bb_idx = sc_cur_idx(swapchain);
            let mut backbuffer: *mut c_void = ptr::null_mut();
            assert_eq!(
                sc_get_buf(swapchain, bb_idx, &DUMMY_IID, &mut backbuffer),
                0
            );

            // 2. RTV Descriptor Heap + Render Target View
            let create_heap: extern "win64" fn(
                *mut c_void,
                *const D3D12DescriptorHeapDesc,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 14);
            let heap_desc = D3D12DescriptorHeapDesc {
                heap_type: 2, // RTV
                num_descriptors: 2,
                ..Default::default()
            };
            let mut rtv_heap: *mut c_void = ptr::null_mut();
            assert_eq!(create_heap(dev, &heap_desc, &DUMMY_IID, &mut rtv_heap), 0);

            let get_cpu_start: extern "win64" fn(*mut c_void) -> usize = vtbl_fn(rtv_heap, 9);
            let rtv_handle = get_cpu_start(rtv_heap);
            assert!(rtv_handle != 0);

            let create_rtv: extern "win64" fn(*mut c_void, *mut c_void, *const c_void, usize) =
                vtbl_fn(dev, 20);
            create_rtv(dev, backbuffer, ptr::null(), rtv_handle);

            // 3. RootSignature + GraphicsPipelineState (DXBC SM5.0 -> SPIR-V -> VkPipeline)
            let mut rs_blob: *mut c_void = ptr::null_mut();
            let mut err_blob: *mut c_void = ptr::null_mut();
            assert_eq!(
                D3D12SerializeRootSignature(ptr::null(), 1, &mut rs_blob, &mut err_blob),
                0
            );
            let blob_ptr: extern "win64" fn(*mut c_void) -> *mut c_void = vtbl_fn(rs_blob, 3);
            let blob_size: extern "win64" fn(*mut c_void) -> usize = vtbl_fn(rs_blob, 4);
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
                create_rs(
                    dev,
                    0,
                    blob_ptr(rs_blob),
                    blob_size(rs_blob),
                    &DUMMY_IID,
                    &mut root_sig
                ),
                0
            );

            let vs_dxbc = make_vs_dxbc();
            let ps_dxbc = make_ps_dxbc();
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
                depth_stencil_state: D3D12DepthStencilDesc::default(),
                input_layout: D3D12InputLayoutDesc {
                    input_element_descs: ptr::null(),
                    num_elements: 2,
                    _pad: 0,
                },
                ib_strip_cut_value: 0,
                primitive_topology_type: 3, // TRIANGLE
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
            let create_pso: extern "win64" fn(
                *mut c_void,
                *const D3D12GraphicsPipelineStateDesc,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 10);
            let mut pso: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_pso(dev, &pso_desc, &DUMMY_IID, &mut pso),
                0,
                "CreateGraphicsPipelineState"
            );
            println!(
                "[d3d12_triangle] Pipeline DXBC SM5.0 (VS={}o, PS={}o) compilé sur GPU Vulkan !",
                vs_dxbc.len(),
                ps_dxbc.len()
            );

            // 4. Vertex Buffer (3 sommets RGB en NDC Direct3D : +Y en haut) + Readback Buffer
            let vertices = [
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

            let upload_heap = D3D12HeapProperties {
                heap_type: 2, // UPLOAD
                ..Default::default()
            };
            let vb_desc = D3D12ResourceDesc {
                dimension: 1, // BUFFER
                width: size_of_val(&vertices) as u64,
                height: 1,
                depth_or_array_size: 1,
                mip_levels: 1,
                ..Default::default()
            };
            let mut vb: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_res(
                    dev,
                    &upload_heap,
                    0,
                    &vb_desc,
                    0,
                    ptr::null(),
                    &DUMMY_IID,
                    &mut vb
                ),
                0
            );

            let res_map: extern "win64" fn(
                *mut c_void,
                u32,
                *const c_void,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(vb, 8);
            let res_unmap: extern "win64" fn(*mut c_void, u32, *const c_void) = vtbl_fn(vb, 9);
            let res_gpu_va: extern "win64" fn(*mut c_void) -> u64 = vtbl_fn(vb, 11);

            let mut mapped_vb: *mut c_void = ptr::null_mut();
            assert_eq!(res_map(vb, 0, ptr::null(), &mut mapped_vb), 0);
            ptr::copy_nonoverlapping(
                vertices.as_ptr().cast::<u8>(),
                mapped_vb.cast::<u8>(),
                size_of_val(&vertices),
            );
            res_unmap(vb, 0, ptr::null());

            let rb_heap = D3D12HeapProperties {
                heap_type: 3, // READBACK
                ..Default::default()
            };
            let rb_desc = D3D12ResourceDesc {
                dimension: 1,
                width: u64::from(W * H * 4),
                height: 1,
                depth_or_array_size: 1,
                mip_levels: 1,
                ..Default::default()
            };
            let mut rb: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_res(
                    dev,
                    &rb_heap,
                    0,
                    &rb_desc,
                    0,
                    ptr::null(),
                    &DUMMY_IID,
                    &mut rb
                ),
                0
            );

            // 5. CommandAllocator + CommandList + Enregistrement des commandes 3D
            let create_alloc: extern "win64" fn(
                *mut c_void,
                u32,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 9);
            let mut cmd_alloc: *mut c_void = ptr::null_mut();
            assert_eq!(create_alloc(dev, 0, &DUMMY_IID, &mut cmd_alloc), 0);

            let create_cl: extern "win64" fn(
                *mut c_void,
                u32,
                u32,
                *mut c_void,
                *mut c_void,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 12);
            let mut cmd_list: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_cl(dev, 0, 0, cmd_alloc, pso, &DUMMY_IID, &mut cmd_list),
                0
            );

            let cl_close: extern "win64" fn(*mut c_void) -> i32 = vtbl_fn(cmd_list, 9);
            let cl_draw: extern "win64" fn(*mut c_void, u32, u32, u32, u32) = vtbl_fn(cmd_list, 12);
            let cl_copy: extern "win64" fn(*mut c_void, *mut c_void, *mut c_void) =
                vtbl_fn(cmd_list, 17);
            let cl_topo: extern "win64" fn(*mut c_void, u32) = vtbl_fn(cmd_list, 20);
            let cl_vp: extern "win64" fn(*mut c_void, u32, *const D3D12Viewport) =
                vtbl_fn(cmd_list, 21);
            let cl_set_rs: extern "win64" fn(*mut c_void, *mut c_void) = vtbl_fn(cmd_list, 30);
            let cl_set_vb: extern "win64" fn(*mut c_void, u32, u32, *const D3D12VertexBufferView) =
                vtbl_fn(cmd_list, 44);
            let cl_clear: extern "win64" fn(
                *mut c_void,
                usize,
                *const [f32; 4],
                u32,
                *const c_void,
            ) = vtbl_fn(cmd_list, 48);

            let vp = D3D12Viewport {
                top_left_x: 0.0,
                top_left_y: 0.0,
                width: W as f32,
                height: H as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            };
            let clear_color = [0.05f32, 0.05, 0.20, 1.0];
            let vbv = D3D12VertexBufferView {
                buffer_location: res_gpu_va(vb),
                size_in_bytes: size_of_val(&vertices) as u32,
                stride_in_bytes: size_of::<VertexPosColor>() as u32,
            };

            cl_set_rs(cmd_list, root_sig);
            cl_vp(cmd_list, 1, &vp);
            cl_clear(cmd_list, rtv_handle, &clear_color, 0, ptr::null());
            cl_topo(cmd_list, 4); // D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST
            cl_set_vb(cmd_list, 0, 1, &vbv);
            cl_draw(cmd_list, 3, 1, 0, 0);
            cl_copy(cmd_list, rb, backbuffer);
            assert_eq!(cl_close(cmd_list), 0);

            // 6. Exécution GPU Vulkan + Fence + Present
            let q_exec: extern "win64" fn(*mut c_void, u32, *const *mut c_void) =
                vtbl_fn(queue, 10);
            let q_signal: extern "win64" fn(*mut c_void, *mut c_void, u64) -> i32 =
                vtbl_fn(queue, 14);
            let cl_arr = [cmd_list];
            q_exec(queue, 1, cl_arr.as_ptr());

            let create_fence: extern "win64" fn(
                *mut c_void,
                u64,
                u32,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 36);
            let mut fence: *mut c_void = ptr::null_mut();
            assert_eq!(create_fence(dev, 0, 0, &DUMMY_IID, &mut fence), 0);
            assert_eq!(q_signal(queue, fence, 1), 0);

            let fence_val: extern "win64" fn(*mut c_void) -> u64 = vtbl_fn(fence, 8);
            let fence_ev: extern "win64" fn(*mut c_void, u64, usize) -> i32 = vtbl_fn(fence, 9);
            assert_eq!(fence_val(fence), 1, "ID3D12Fence::GetCompletedValue");
            let ev = CreateEventW(ptr::null_mut(), 0, 0, ptr::null());
            assert!(ev != 0);
            assert_eq!(fence_ev(fence, 1, ev), 0);
            assert_eq!(WaitForSingleObject(ev, 5000), 0);
            CloseHandle(ev);

            let sc_present: extern "win64" fn(*mut c_void, u32, u32) -> i32 = vtbl_fn(swapchain, 8);
            assert_eq!(sc_present(swapchain, 1, 0), 0);

            // 7. Vérification pixel par pixel depuis l'intérieur du binaire invité PE32+ !
            let mut mapped_rb: *mut c_void = ptr::null_mut();
            assert_eq!(res_map(rb, 0, ptr::null(), &mut mapped_rb), 0);
            let pixels = std::slice::from_raw_parts(mapped_rb.cast::<u8>(), (W * H * 4) as usize);
            let px = |x: usize, y: usize| -> [u8; 4] {
                let i = (y * (W as usize) + x) * 4;
                [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
            };

            let bg = px(8, 8);
            let center = px(128, 128);
            let top = px(128, 48);
            let br = px(204, 204);
            let bl = px(52, 204);

            println!(
                "[d3d12_triangle] Readback GPU 256x256 :\n\
                 - Fond (8,8)          = RGBA{bg:?}\n\
                 - Centre (128,128)    = RGBA{center:?}\n\
                 - Sommet haut (R)     = RGBA{top:?}\n\
                 - Bas-droit (G)       = RGBA{br:?}\n\
                 - Bas-gauche (B)      = RGBA{bl:?}"
            );

            assert!(bg[2] > 40 && bg[0] < 25, "fond bleu nuit attendu: {bg:?}");
            assert!(
                center[0] > 60 && center[1] > 40 && center[2] > 40,
                "mélange RGB centre attendu: {center:?}"
            );
            assert!(top[0] > 180 && top[1] < 60, "rouge attendu: {top:?}");
            assert!(br[1] > 180 && br[0] < 60, "vert attendu: {br:?}");
            assert!(bl[2] > 180 && bl[0] < 60, "bleu attendu: {bl:?}");
        }

        println!("[d3d12_triangle] === RENDU 3D GPU VULKAN VÉRIFIÉ PIXEL PAR PIXEL ===");
        0
    }
}
