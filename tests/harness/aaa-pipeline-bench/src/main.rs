//! Harnais PE32+ x86-64 natif **AAA Pipeline Benchmark (`aaa_pipeline_bench.exe`)** pour ScarletBridge.
//!
//! Valide bout-en-bout un vrai pipeline graphique + Compute AAA Xbox Series X (`d3d12_xs.dll` → Vulkan 1.4) :
//! 1. `ID3D12Device::CheckFeatureSupport` (FL 12_1, SM 6.x, ResourceBindingTier 3, RenderPassesTier 2)
//! 2. `CreateHeap` (256 KiB) + `CreatePlacedResource` avec **aliasing mémoire** à offset 65536
//! 3. `D3D12SerializeRootSignature` + `CreateComputePipelineState` + `Dispatch(4, 1, 1)` (256 threads GPU)
//! 4. Texture 64×64 compressée matériel **BC1 (`DXGI_FORMAT_BC1_UNORM = 71`)** téléversée via `CopyTextureRegion`
//! 5. Rendu 3D **instancié (`DrawIndexedInstanced` × 4 cubes)** alimenté par le résultat du Compute Shader,
//!    avec `D3D12_INPUT_ELEMENT_DESC` compact (`float3 pos + float2 uv + float4 color`, 36 octets) + Depth Buffer
//! 6. Passe transparente **Alpha Blending (`SRC_ALPHA / INV_SRC_ALPHA`)** + **Scissor Clipping (`RSSetScissorRects`)**
//! 7. Readback GPU→CPU vérifié float par float et pixel par pixel + `IDXGISwapChain3::Present`.

#[cfg(not(target_os = "windows"))]
fn main() {
    println!("aaa_pipeline_bench est un binaire PE32+ cible `x86_64-pc-windows-gnu`.");
}

#[cfg(target_os = "windows")]
fn main() {
    win_main::run();
}

#[cfg(target_os = "windows")]
mod win_main {
    use core::ffi::c_void;
    use core::mem::{size_of, size_of_val};
    use core::ptr;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Guid(u32, u16, u16, [u8; 8]);

    const DUMMY_IID: Guid = Guid(0, 0, 0, [0; 8]);

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
    struct D3D12InputElementDesc {
        semantic_name: *const u8,
        semantic_index: u32,
        format: u32,
        input_slot: u32,
        aligned_byte_offset: u32,
        input_slot_class: u32,
        instance_data_step_rate: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct D3D12InputLayoutDesc {
        input_element_descs: *const D3D12InputElementDesc,
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
    #[derive(Clone, Copy)]
    struct D3D12ComputePipelineStateDesc {
        root_signature: *mut c_void,
        cs: D3D12ShaderBytecode,
        node_mask: u32,
        _pad0: u32,
        cached_pso: D3D12CachedPipelineState,
        flags: u32,
        _pad1: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct D3D12RootParameter {
        parameter_type: u32,
        _pad0: u32,
        data0: u32,
        data1: u32,
        data2: u64,
        shader_visibility: u32,
        _pad1: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct D3D12RootSignatureDesc {
        num_parameters: u32,
        _pad0: u32,
        parameters: *const D3D12RootParameter,
        num_static_samplers: u32,
        _pad1: u32,
        static_samplers: *const c_void,
        flags: u32,
        _pad2: u32,
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
    struct D3D12HeapDesc {
        size_in_bytes: u64,
        properties: D3D12HeapProperties,
        _pad0: u32,
        alignment: u64,
        flags: u32,
        _pad1: u32,
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
    struct D3D12Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
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

    /// Sommet AAA compact (36 octets : `R32G32B32_FLOAT` + `R32G32_FLOAT` + `R32G32B32A32_FLOAT`).
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct VertexAaa {
        pos: [f32; 3],
        uv: [f32; 2],
        color: [f32; 4],
    }

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
            out_error_blob: *mut *mut c_void,
        ) -> i32;
    }

    #[link(name = "dxgi", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn CreateDXGIFactory2(flags: u32, riid: *const Guid, out_factory: *mut *mut c_void) -> i32;
    }

    unsafe fn vtbl_fn<T: Copy>(obj: *mut c_void, slot: usize) -> T {
        unsafe {
            let vtbl = *(obj.cast::<*const *const c_void>());
            let func_ptr = *vtbl.add(slot);
            core::mem::transmute_copy(&func_ptr)
        }
    }

    // ==================== Générateurs DXBC SM5.0 (Compute, Instanced VS, Textured PS, Solid PS) ====================

    const OP_ADD: u32 = 0x00;
    const OP_DP4: u32 = 0x11;
    const OP_MAD: u32 = 0x32;
    const OP_MOV: u32 = 0x36;
    const OP_MUL: u32 = 0x38;
    const OP_RET: u32 = 0x3E;
    const OP_SAMPLE: u32 = 0x45;
    const OP_DCL_TEMPS: u32 = 0x68;
    const OP_DCL_THREAD_GROUP: u32 = 0x9B;
    const OP_LD_STRUCTURED: u32 = 0xA7;
    const OP_STORE_STRUCTURED: u32 = 0xA8;

    const OPERAND_TEMP: u32 = 0;
    const OPERAND_INPUT: u32 = 1;
    const OPERAND_OUTPUT: u32 = 2;
    const OPERAND_IMMEDIATE32: u32 = 4;
    const OPERAND_SAMPLER: u32 = 6;
    const OPERAND_RESOURCE: u32 = 7;
    const OPERAND_CONSTANT_BUFFER: u32 = 8;
    const OPERAND_UAV: u32 = 30;
    const OPERAND_THREAD_ID: u32 = 32;

    fn build_sig_chunk(elems: &[(&str, u32, u32, u32)]) -> Vec<u8> {
        let table_end = 8 + elems.len() * 24;
        let mut strings = Vec::new();
        let mut offsets = Vec::with_capacity(elems.len());
        for &(name, _, _, _) in elems {
            offsets.push((table_end + strings.len()) as u32);
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
        }
        while !strings.len().is_multiple_of(4) {
            strings.push(0);
        }
        let mut out = Vec::with_capacity(table_end + strings.len());
        out.extend_from_slice(&(elems.len() as u32).to_le_bytes());
        out.extend_from_slice(&8u32.to_le_bytes());
        for (i, &(_, sidx, sv, reg)) in elems.iter().enumerate() {
            out.extend_from_slice(&offsets[i].to_le_bytes());
            out.extend_from_slice(&sidx.to_le_bytes());
            out.extend_from_slice(&sv.to_le_bytes());
            out.extend_from_slice(&3u32.to_le_bytes());
            out.extend_from_slice(&reg.to_le_bytes());
            out.extend_from_slice(&[0x0F, 0x0F, 0, 0]);
        }
        out.extend_from_slice(&strings);
        out
    }

    fn build_dxbc(
        kind: u32,
        inputs: &[(&str, u32, u32, u32)],
        outputs: &[(&str, u32, u32, u32)],
        instructions: &[u32],
    ) -> Vec<u8> {
        let isgn = build_sig_chunk(inputs);
        let osgn = build_sig_chunk(outputs);
        let version = (kind << 16) | 0x0050;
        let mut shex = Vec::with_capacity((2 + instructions.len()) * 4);
        shex.extend_from_slice(&version.to_le_bytes());
        shex.extend_from_slice(&((2 + instructions.len()) as u32).to_le_bytes());
        for &w in instructions {
            shex.extend_from_slice(&w.to_le_bytes());
        }
        let chunks: [(&[u8; 4], &[u8]); 3] = [(b"ISGN", &isgn), (b"OSGN", &osgn), (b"SHEX", &shex)];
        let mut cur = 32 + chunks.len() * 4;
        let mut offs = Vec::new();
        for (_, d) in &chunks {
            offs.push(cur as u32);
            cur += 8 + d.len();
        }
        let mut out = Vec::with_capacity(cur);
        out.extend_from_slice(b"DXBC");
        out.extend_from_slice(&[0u8; 16]);
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(cur as u32).to_le_bytes());
        out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
        for o in offs {
            out.extend_from_slice(&o.to_le_bytes());
        }
        for (fourcc, d) in chunks {
            out.extend_from_slice(fourcc);
            out.extend_from_slice(&(d.len() as u32).to_le_bytes());
            out.extend_from_slice(d);
        }
        out
    }

    fn make_compute_cs_dxbc() -> Vec<u8> {
        let dst_r = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_TEMP << 12) | (1 << 20) };
        let src_r = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_TEMP << 12) | (1 << 20);
        let src_cb = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_CONSTANT_BUFFER << 12) | (2 << 20);
        let src_u = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_UAV << 12) | (1 << 20);
        let dst_u = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_UAV << 12) | (1 << 20) };
        let src_tid_x = 2 | (2 << 2) | (OPERAND_THREAD_ID << 12);
        let imm1 = 1 | (OPERAND_IMMEDIATE32 << 12);
        let code = [
            (2u32 << 24) | OP_DCL_TEMPS,
            2,
            (4u32 << 24) | OP_DCL_THREAD_GROUP,
            64,
            1,
            1,
            (8u32 << 24) | OP_LD_STRUCTURED,
            dst_r(0xF),
            0,
            src_tid_x,
            imm1,
            0,
            src_u,
            0,
            (11u32 << 24) | OP_MAD,
            dst_r(0xF),
            1,
            src_r,
            0,
            src_cb,
            0,
            0,
            src_cb,
            0,
            1,
            (8u32 << 24) | OP_STORE_STRUCTURED,
            dst_u(0xF),
            1,
            src_tid_x,
            imm1,
            0,
            src_r,
            1,
            (1u32 << 24) | OP_RET,
        ];
        build_dxbc(5, &[], &[], &code)
    }

    fn make_instanced_vs_dxbc() -> Vec<u8> {
        let dst_r = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_TEMP << 12) | (1 << 20) };
        let dst_o = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_OUTPUT << 12) | (1 << 20) };
        let src_v = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_INPUT << 12) | (1 << 20);
        let src_r = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_TEMP << 12) | (1 << 20);
        let src_r_x = 2 | (2 << 2) | (OPERAND_TEMP << 12) | (1 << 20);
        let src_cb = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_CONSTANT_BUFFER << 12) | (2 << 20);
        let src_cb_rel = src_cb | (3 << 25);
        let code = [
            (2u32 << 24) | OP_DCL_TEMPS,
            2,
            (5u32 << 24) | OP_MOV,
            dst_r(0xF),
            0,
            src_v,
            3,
            (10u32 << 24) | OP_ADD,
            dst_r(0xF),
            1,
            src_v,
            0,
            src_cb_rel,
            0,
            4,
            src_r_x,
            0,
            (8u32 << 24) | OP_DP4,
            dst_o(0x1),
            0,
            src_r,
            1,
            src_cb,
            0,
            0,
            (8u32 << 24) | OP_DP4,
            dst_o(0x2),
            0,
            src_r,
            1,
            src_cb,
            0,
            1,
            (8u32 << 24) | OP_DP4,
            dst_o(0x4),
            0,
            src_r,
            1,
            src_cb,
            0,
            2,
            (8u32 << 24) | OP_DP4,
            dst_o(0x8),
            0,
            src_r,
            1,
            src_cb,
            0,
            3,
            (5u32 << 24) | OP_MOV,
            dst_o(0xF),
            1,
            src_v,
            1,
            (5u32 << 24) | OP_MOV,
            dst_o(0xF),
            2,
            src_v,
            2,
            (1u32 << 24) | OP_RET,
        ];
        build_dxbc(
            1,
            &[
                ("POSITION", 0, 0, 0),
                ("TEXCOORD", 0, 0, 1),
                ("COLOR", 0, 0, 2),
                ("SV_INSTANCEID", 0, 8, 3),
            ],
            &[
                ("SV_POSITION", 0, 1, 0),
                ("TEXCOORD", 0, 0, 1),
                ("COLOR", 0, 0, 2),
            ],
            &code,
        )
    }

    fn make_textured_ps_dxbc() -> Vec<u8> {
        let dst_r = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_TEMP << 12) | (1 << 20) };
        let dst_o = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_OUTPUT << 12) | (1 << 20) };
        let src_v = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_INPUT << 12) | (1 << 20);
        let src_r = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_TEMP << 12) | (1 << 20);
        let src_t = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_RESOURCE << 12) | (1 << 20);
        let src_s = (OPERAND_SAMPLER << 12) | (1 << 20);
        let code = [
            (2u32 << 24) | OP_DCL_TEMPS,
            1,
            (9u32 << 24) | OP_SAMPLE,
            dst_r(0xF),
            0,
            src_v,
            1,
            src_t,
            0,
            src_s,
            0,
            (7u32 << 24) | OP_MUL,
            dst_o(0xF),
            0,
            src_r,
            0,
            src_v,
            2,
            (1u32 << 24) | OP_RET,
        ];
        build_dxbc(
            0,
            &[
                ("SV_POSITION", 0, 1, 0),
                ("TEXCOORD", 0, 0, 1),
                ("COLOR", 0, 0, 2),
            ],
            &[("SV_Target", 0, 0, 0)],
            &code,
        )
    }

    fn make_hud_vs_dxbc() -> Vec<u8> {
        let dst_o = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_OUTPUT << 12) | (1 << 20) };
        let src_v = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_INPUT << 12) | (1 << 20);
        let code = [
            (5u32 << 24) | OP_MOV,
            dst_o(0xF),
            0,
            src_v,
            0,
            (5u32 << 24) | OP_MOV,
            dst_o(0xF),
            1,
            src_v,
            2,
            (1u32 << 24) | OP_RET,
        ];
        build_dxbc(
            1,
            &[
                ("POSITION", 0, 0, 0),
                ("TEXCOORD", 0, 0, 1),
                ("COLOR", 0, 0, 2),
            ],
            &[("SV_POSITION", 0, 1, 0), ("COLOR", 0, 0, 1)],
            &code,
        )
    }

    fn make_hud_ps_dxbc() -> Vec<u8> {
        let dst_o = |mask: u32| -> u32 { 2 | (mask << 4) | (OPERAND_OUTPUT << 12) | (1 << 20) };
        let src_v = 2 | (1 << 2) | (0xE4 << 4) | (OPERAND_INPUT << 12) | (1 << 20);
        let code = [
            (5u32 << 24) | OP_MOV,
            dst_o(0xF),
            0,
            src_v,
            1,
            (1u32 << 24) | OP_RET,
        ];
        build_dxbc(
            0,
            &[("SV_POSITION", 0, 1, 0), ("COLOR", 0, 0, 1)],
            &[("SV_Target", 0, 0, 0)],
            &code,
        )
    }

    // ==================== Mathématiques 3D + Géométrie Cube + Texture BC1 ====================

    fn mat4_mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
        let mut out = [0.0f32; 16];
        for r in 0..4 {
            for c in 0..4 {
                out[r * 4 + c] = a[r * 4] * b[c]
                    + a[r * 4 + 1] * b[4 + c]
                    + a[r * 4 + 2] * b[8 + c]
                    + a[r * 4 + 3] * b[12 + c];
            }
        }
        out
    }

    fn build_mvp(angle_y: f32, angle_x: f32) -> [f32; 16] {
        let (sy, cy) = angle_y.sin_cos();
        let (sx, cx) = angle_x.sin_cos();
        let rot_y = [
            cy, 0.0, sy, 0.0, 0.0, 1.0, 0.0, 0.0, -sy, 0.0, cy, 0.0, 0.0, 0.0, 0.0, 1.0,
        ];
        let rot_x = [
            1.0, 0.0, 0.0, 0.0, 0.0, cx, -sx, 0.0, 0.0, sx, cx, 0.0, 0.0, 0.0, 0.0, 1.0,
        ];
        let trans = [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 5.2, 0.0, 0.0, 0.0, 1.0,
        ];
        let f = 1.65f32;
        let near = 0.5f32;
        let far = 20.0f32;
        let proj = [
            f,
            0.0,
            0.0,
            0.0,
            0.0,
            f,
            0.0,
            0.0,
            0.0,
            0.0,
            far / (far - near),
            -(far * near) / (far - near),
            0.0,
            0.0,
            1.0,
            0.0,
        ];
        let model = mat4_mul(&rot_x, &rot_y);
        let mv = mat4_mul(&trans, &model);
        mat4_mul(&proj, &mv)
    }

    fn build_cube_mesh(half: f32) -> (Vec<VertexAaa>, Vec<u16>) {
        let s = half;
        let faces: [([[f32; 3]; 4], [f32; 4]); 6] = [
            (
                [[-s, s, -s], [s, s, -s], [s, -s, -s], [-s, -s, -s]],
                [1.0, 0.95, 0.88, 1.0],
            ),
            (
                [[s, s, s], [-s, s, s], [-s, -s, s], [s, -s, s]],
                [0.45, 0.92, 1.0, 1.0],
            ),
            (
                [[s, s, -s], [s, s, s], [s, -s, s], [s, -s, -s]],
                [1.0, 0.65, 0.42, 1.0],
            ),
            (
                [[-s, s, s], [-s, s, -s], [-s, -s, -s], [-s, -s, s]],
                [0.55, 1.0, 0.65, 1.0],
            ),
            (
                [[-s, s, s], [s, s, s], [s, s, -s], [-s, s, -s]],
                [0.95, 0.92, 1.0, 1.0],
            ),
            (
                [[-s, -s, -s], [s, -s, -s], [s, -s, s], [-s, -s, s]],
                [0.65, 0.55, 1.0, 1.0],
            ),
        ];
        let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let mut verts = Vec::with_capacity(24);
        let mut indices = Vec::with_capacity(36);
        for (fi, (corners, col)) in faces.iter().enumerate() {
            let base = (fi * 4) as u16;
            for vi in 0..4 {
                verts.push(VertexAaa {
                    pos: corners[vi],
                    uv: uvs[vi],
                    color: *col,
                });
            }
            indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        (verts, indices)
    }

    fn rgb565(r: u8, g: u8, b: u8) -> u16 {
        ((u16::from(r) >> 3) << 11) | ((u16::from(g) >> 2) << 5) | (u16::from(b) >> 3)
    }

    /// Génère une texture 64×64 compressée nativement en **blocs BC1 (DXT1, 8 octets / bloc 4×4)**.
    fn build_bc1_checkerboard_64x64() -> Vec<u8> {
        let blocks_x = 16usize;
        let blocks_y = 16usize;
        let mut out = Vec::with_capacity(blocks_x * blocks_y * 8);
        let cyan = rgb565(32, 235, 255);
        let gold = rgb565(255, 200, 48);
        let navy = rgb565(28, 46, 98);
        for by in 0..blocks_y {
            for bx in 0..blocks_x {
                let tile = ((bx / 2) + (by / 2)) & 1;
                let (c0, c1) = if tile == 0 {
                    (cyan, navy)
                } else {
                    (gold, navy)
                };
                // Dégradé 2-bit interne sur chaque bloc 4×4 (bords sombres + centre lumineux).
                let idx_bits: u32 = 0x5540_0055;
                out.extend_from_slice(&c0.to_le_bytes());
                out.extend_from_slice(&c1.to_le_bytes());
                out.extend_from_slice(&idx_bits.to_le_bytes());
            }
        }
        out
    }

    pub fn run() {
        const W: u32 = 512;
        const H: u32 = 512;
        println!("=== ScarletBridge PE32+ AAA Pipeline Benchmark ({W}x{H}) ===");

        unsafe {
            // 1. Device D3D12_XS + Feature Support (FL 12_1, SM 6.x, ResourceBindingTier 3)
            let mut dev: *mut c_void = ptr::null_mut();
            assert_eq!(
                D3D12CreateDevice(ptr::null_mut(), 0xC100, &DUMMY_IID, &mut dev),
                0
            );

            let dev_check_feat: extern "win64" fn(*mut c_void, u32, *mut c_void, u32) -> i32 =
                vtbl_fn(dev, 13);
            let mut sm = 0x66u32;
            assert_eq!(
                dev_check_feat(dev, 7, (&raw mut sm).cast(), size_of::<u32>() as u32),
                0
            );
            assert!(sm >= 0x60, "Shader Model 6.x requis, reçu {sm:#x}");
            let mut opts = [0u32; 16];
            assert_eq!(
                dev_check_feat(dev, 0, opts.as_mut_ptr().cast(), size_of_val(&opts) as u32),
                0
            );
            assert_eq!(opts[4], 3, "ResourceBindingTier::TIER_3 requis");
            println!(
                "[AAA] CheckFeatureSupport OK (SM={sm:#x}, ResourceBindingTier={}, HeapTier={})",
                opts[4], opts[14]
            );

            // Command Queue + SwapChain 512×512 + Allocator
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
                format: 28,
                sample_desc: DxgiSampleDesc {
                    count: 1,
                    quality: 0,
                },
                buffer_count: 2,
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

            let create_alloc: extern "win64" fn(
                *mut c_void,
                u32,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 9);
            let mut alloc: *mut c_void = ptr::null_mut();
            assert_eq!(create_alloc(dev, 0, &DUMMY_IID, &mut alloc), 0);

            // 2. Heap 256 KiB + Placed Resources aliasés (in_buf @ 0, out_buf @ 65536, out_alias @ 65536)
            let create_heap: extern "win64" fn(
                *mut c_void,
                *const D3D12HeapDesc,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 28);
            let create_placed: extern "win64" fn(
                *mut c_void,
                *mut c_void,
                u64,
                *const D3D12ResourceDesc,
                u32,
                *const c_void,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 29);
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

            let heap_desc = D3D12HeapDesc {
                size_in_bytes: 256 * 1024,
                alignment: 65536,
                ..Default::default()
            };
            let mut gpu_heap: *mut c_void = ptr::null_mut();
            assert_eq!(create_heap(dev, &heap_desc, &DUMMY_IID, &mut gpu_heap), 0);

            let buf_desc = D3D12ResourceDesc {
                dimension: 1,
                width: 256 * 16,
                height: 1,
                depth_or_array_size: 1,
                mip_levels: 1,
                ..Default::default()
            };
            let mut cs_in_buf: *mut c_void = ptr::null_mut();
            let mut cs_out_buf: *mut c_void = ptr::null_mut();
            let mut cs_out_alias: *mut c_void = ptr::null_mut();
            let mut cs_readback: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_placed(
                    dev,
                    gpu_heap,
                    0,
                    &buf_desc,
                    0,
                    ptr::null(),
                    &DUMMY_IID,
                    &mut cs_in_buf
                ),
                0
            );
            assert_eq!(
                create_placed(
                    dev,
                    gpu_heap,
                    65536,
                    &buf_desc,
                    0,
                    ptr::null(),
                    &DUMMY_IID,
                    &mut cs_out_buf
                ),
                0
            );
            assert_eq!(
                create_placed(
                    dev,
                    gpu_heap,
                    65536,
                    &buf_desc,
                    0,
                    ptr::null(),
                    &DUMMY_IID,
                    &mut cs_out_alias
                ),
                0
            );
            let upload_hp = D3D12HeapProperties {
                heap_type: 2,
                ..Default::default()
            };
            assert_eq!(
                create_res(
                    dev,
                    &upload_hp,
                    0,
                    &buf_desc,
                    0,
                    ptr::null(),
                    &DUMMY_IID,
                    &mut cs_readback
                ),
                0
            );

            let res_map: extern "win64" fn(
                *mut c_void,
                u32,
                *const c_void,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(cs_in_buf, 8);
            let res_gpu_va: extern "win64" fn(*mut c_void) -> u64 = vtbl_fn(cs_in_buf, 11);
            assert_eq!(res_gpu_va(cs_out_buf), res_gpu_va(cs_in_buf) + 65536);
            assert_eq!(res_gpu_va(cs_out_alias), res_gpu_va(cs_out_buf));
            println!(
                "[AAA] Placed Heap Aliasing OK (in={:#x}, out=alias={:#x})",
                res_gpu_va(cs_in_buf),
                res_gpu_va(cs_out_alias)
            );

            // 3. RootSignature Compute réelle (param 0=CBV b0, param 1=UAV u0, param 2=UAV u1)
            let cs_params = [
                D3D12RootParameter {
                    parameter_type: 2,
                    data0: 0,
                    ..Default::default()
                },
                D3D12RootParameter {
                    parameter_type: 4,
                    data0: 0,
                    ..Default::default()
                },
                D3D12RootParameter {
                    parameter_type: 4,
                    data0: 1,
                    ..Default::default()
                },
            ];
            let cs_rs_desc = D3D12RootSignatureDesc {
                num_parameters: cs_params.len() as u32,
                _pad0: 0,
                parameters: cs_params.as_ptr(),
                num_static_samplers: 0,
                _pad1: 0,
                static_samplers: ptr::null(),
                flags: 0,
                _pad2: 0,
            };
            let mut cs_rs_blob: *mut c_void = ptr::null_mut();
            assert_eq!(
                D3D12SerializeRootSignature(
                    (&raw const cs_rs_desc).cast(),
                    1,
                    &mut cs_rs_blob,
                    ptr::null_mut()
                ),
                0
            );
            let blob_ptr: extern "win64" fn(*mut c_void) -> *mut c_void = vtbl_fn(cs_rs_blob, 3);
            let blob_len: extern "win64" fn(*mut c_void) -> usize = vtbl_fn(cs_rs_blob, 4);
            let create_rs: extern "win64" fn(
                *mut c_void,
                u32,
                *const c_void,
                usize,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 16);
            let mut cs_root_sig: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_rs(
                    dev,
                    0,
                    blob_ptr(cs_rs_blob),
                    blob_len(cs_rs_blob),
                    &DUMMY_IID,
                    &mut cs_root_sig
                ),
                0
            );

            // Compute PSO (256 threads = 4×64)
            let cs_dxbc = make_compute_cs_dxbc();
            let cs_pso_desc = D3D12ComputePipelineStateDesc {
                root_signature: cs_root_sig,
                cs: D3D12ShaderBytecode {
                    shader_bytecode: cs_dxbc.as_ptr(),
                    bytecode_length: cs_dxbc.len(),
                },
                node_mask: 0,
                _pad0: 0,
                cached_pso: D3D12CachedPipelineState {
                    cached_blob: ptr::null(),
                    cached_blob_size_in_bytes: 0,
                },
                flags: 0,
                _pad1: 0,
            };
            let create_cpso: extern "win64" fn(
                *mut c_void,
                *const D3D12ComputePipelineStateDesc,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 11);
            let mut cs_pso: *mut c_void = ptr::null_mut();
            assert_eq!(create_cpso(dev, &cs_pso_desc, &DUMMY_IID, &mut cs_pso), 0);

            // Remplit cs_in_buf : les 4 premiers vec4 encodent les coordonnées grille des 4 cubes instanciés !
            let mut p: *mut c_void = ptr::null_mut();
            assert_eq!(res_map(cs_in_buf, 0, ptr::null(), &mut p), 0);
            let in_slice = core::slice::from_raw_parts_mut(p.cast::<f32>(), 256 * 4);
            let base_grid: [[f32; 4]; 4] = [
                [-1.0, 0.7, 0.0, 0.0],
                [1.0, 0.7, 0.0, 0.0],
                [-1.0, -1.0, 0.0, 0.0],
                [1.0, -1.0, 0.0, 0.0],
            ];
            for (i, g) in base_grid.iter().enumerate() {
                in_slice[i * 4..i * 4 + 4].copy_from_slice(g);
            }
            for i in 4..256 {
                let f = i as f32;
                in_slice[i * 4] = f;
                in_slice[i * 4 + 1] = f * 0.5;
                in_slice[i * 4 + 2] = 1.0;
                in_slice[i * 4 + 3] = 0.0;
            }

            let cb_desc = D3D12ResourceDesc {
                dimension: 1,
                width: 512,
                height: 1,
                depth_or_array_size: 1,
                mip_levels: 1,
                ..Default::default()
            };
            let mut cs_cb: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_res(
                    dev,
                    &upload_hp,
                    0,
                    &cb_desc,
                    0,
                    ptr::null(),
                    &DUMMY_IID,
                    &mut cs_cb
                ),
                0
            );
            assert_eq!(res_map(cs_cb, 0, ptr::null(), &mut p), 0);
            // scale = (1.35, 1.20, 1.0, 1.0), bias = (0.0, -0.15, 0.0, 0.0)
            let cs_cb_vals: [f32; 8] = [1.35, 1.20, 1.0, 1.0, 0.0, -0.15, 0.0, 0.0];
            ptr::copy_nonoverlapping(cs_cb_vals.as_ptr(), p.cast::<f32>(), cs_cb_vals.len());

            // Exécution Compute Shader sur GPU AMD RX 5700 XT + CopyResource vers readback !
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
            assert_eq!(create_cl(dev, 0, 0, alloc, cs_pso, &DUMMY_IID, &mut cl), 0);

            let cl_close: extern "win64" fn(*mut c_void) -> i32 = vtbl_fn(cl, 9);
            let cl_reset: extern "win64" fn(*mut c_void, *mut c_void, *mut c_void) -> i32 =
                vtbl_fn(cl, 10);
            let cl_draw_idx: extern "win64" fn(*mut c_void, u32, u32, u32, i32, u32) =
                vtbl_fn(cl, 13);
            let cl_dispatch: extern "win64" fn(*mut c_void, u32, u32, u32) = vtbl_fn(cl, 14);
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
            let cl_scissor: extern "win64" fn(*mut c_void, u32, *const D3D12Rect) = vtbl_fn(cl, 22);
            let cl_set_pso: extern "win64" fn(*mut c_void, *mut c_void) = vtbl_fn(cl, 25);
            let cl_set_cs_rs: extern "win64" fn(*mut c_void, *mut c_void) = vtbl_fn(cl, 29);
            let cl_set_gfx_table: extern "win64" fn(*mut c_void, u32, u64) = vtbl_fn(cl, 32);
            let cl_set_cs_cbv: extern "win64" fn(*mut c_void, u32, u64) = vtbl_fn(cl, 37);
            let cl_set_gfx_cbv: extern "win64" fn(*mut c_void, u32, u64) = vtbl_fn(cl, 38);
            let cl_set_cs_uav: extern "win64" fn(*mut c_void, u32, u64) = vtbl_fn(cl, 41);
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

            cl_set_cs_rs(cl, cs_root_sig);
            cl_set_cs_cbv(cl, 0, res_gpu_va(cs_cb));
            cl_set_cs_uav(cl, 1, res_gpu_va(cs_in_buf));
            cl_set_cs_uav(cl, 2, res_gpu_va(cs_out_buf));
            cl_dispatch(cl, 4, 1, 1);
            cl_copy_res(cl, cs_readback, cs_out_buf);
            assert_eq!(cl_close(cl), 0);
            let lists = [cl];
            q_exec(queue, 1, lists.as_ptr());

            // Vérifie les 256×4 floats via le buffer aliasé ET via le buffer CopyResource !
            assert_eq!(res_map(cs_out_alias, 0, ptr::null(), &mut p), 0);
            let alias_f32 = core::slice::from_raw_parts(p.cast::<f32>(), 256 * 4);
            let mut p_rb: *mut c_void = ptr::null_mut();
            assert_eq!(res_map(cs_readback, 0, ptr::null(), &mut p_rb), 0);
            let rb_f32 = core::slice::from_raw_parts(p_rb.cast::<f32>(), 256 * 4);

            let mut inst_offsets = [[0.0f32; 4]; 4];
            for i in 0..4 {
                let expected_x = base_grid[i][0] * 1.35;
                let expected_y = base_grid[i][1] * 1.20 - 0.15;
                assert!((alias_f32[i * 4] - expected_x).abs() < 1e-3);
                assert!((alias_f32[i * 4 + 1] - expected_y).abs() < 1e-3);
                assert!((rb_f32[i * 4] - expected_x).abs() < 1e-3);
                inst_offsets[i] = [
                    alias_f32[i * 4],
                    alias_f32[i * 4 + 1],
                    alias_f32[i * 4 + 2],
                    0.0,
                ];
            }
            for i in 4..256 {
                let f = i as f32;
                assert!((rb_f32[i * 4] - (f * 1.35)).abs() < 1e-3);
                assert!((rb_f32[i * 4 + 1] - (f * 0.5 * 1.20 - 0.15)).abs() < 1e-3);
            }
            println!(
                "[AAA] Compute Dispatch(4,1,1) = 256 threads GPU OK (inst0=({:.2},{:.2}), inst3=({:.2},{:.2}))",
                inst_offsets[0][0], inst_offsets[0][1], inst_offsets[3][0], inst_offsets[3][1]
            );

            // 4. Pipelines Graphiques AAA avec InputLayout personnalisé (36o) :
            //    - PSO #1 : Cubes 3D instanciés (BC1 texture, Depth ON)
            //    - PSO #2 : Bandeau HUD Alpha-Blended (SRC_ALPHA / INV_SRC_ALPHA, Depth OFF)
            let input_elems = [
                D3D12InputElementDesc {
                    semantic_name: c"POSITION".as_ptr().cast(),
                    semantic_index: 0,
                    format: 6, // DXGI_FORMAT_R32G32B32_FLOAT (12o)
                    input_slot: 0,
                    aligned_byte_offset: 0,
                    input_slot_class: 0,
                    instance_data_step_rate: 0,
                },
                D3D12InputElementDesc {
                    semantic_name: c"TEXCOORD".as_ptr().cast(),
                    semantic_index: 0,
                    format: 16, // DXGI_FORMAT_R32G32_FLOAT (8o)
                    input_slot: 0,
                    aligned_byte_offset: 12,
                    input_slot_class: 0,
                    instance_data_step_rate: 0,
                },
                D3D12InputElementDesc {
                    semantic_name: c"COLOR".as_ptr().cast(),
                    semantic_index: 0,
                    format: 2, // DXGI_FORMAT_R32G32B32A32_FLOAT (16o)
                    input_slot: 0,
                    aligned_byte_offset: 20,
                    input_slot_class: 0,
                    instance_data_step_rate: 0,
                },
            ];
            let mut gfx_rs: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_rs(dev, 0, ptr::null(), 0, &DUMMY_IID, &mut gfx_rs),
                0
            );
            let create_gpso: extern "win64" fn(
                *mut c_void,
                *const D3D12GraphicsPipelineStateDesc,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 10);

            let inst_vs = make_instanced_vs_dxbc();
            let tex_ps = make_textured_ps_dxbc();
            let cube_pso_desc = D3D12GraphicsPipelineStateDesc {
                root_signature: gfx_rs,
                vs: D3D12ShaderBytecode {
                    shader_bytecode: inst_vs.as_ptr(),
                    bytecode_length: inst_vs.len(),
                },
                ps: D3D12ShaderBytecode {
                    shader_bytecode: tex_ps.as_ptr(),
                    bytecode_length: tex_ps.len(),
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
                rasterizer_state: D3D12RasterizerDesc {
                    fill_mode: 3, // SOLID
                    cull_mode: 1, // NONE
                    ..Default::default()
                },
                depth_stencil_state: D3D12DepthStencilDesc {
                    depth_enable: 1,
                    depth_write_mask: 1,
                    depth_func: 4, // LESS_EQUAL
                    ..Default::default()
                },
                input_layout: D3D12InputLayoutDesc {
                    input_element_descs: input_elems.as_ptr(),
                    num_elements: input_elems.len() as u32,
                    _pad: 0,
                },
                ib_strip_cut_value: 0,
                primitive_topology_type: 3,
                num_render_targets: 1,
                rtv_formats: [28, 0, 0, 0, 0, 0, 0, 0],
                dsv_format: 40,
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
            let mut cube_pso: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_gpso(dev, &cube_pso_desc, &DUMMY_IID, &mut cube_pso),
                0
            );

            let hud_vs = make_hud_vs_dxbc();
            let hud_ps = make_hud_ps_dxbc();
            let mut blend_desc = D3D12BlendDesc::default();
            blend_desc.render_target[0] = D3D12RenderTargetBlendDesc {
                blend_enable: 1,
                src_blend: 5,       // SRC_ALPHA
                dest_blend: 6,      // INV_SRC_ALPHA
                blend_op: 1,        // ADD
                src_blend_alpha: 2, // ONE
                dest_blend_alpha: 1,
                blend_op_alpha: 1,
                render_target_write_mask: 0x0F,
                ..Default::default()
            };
            let hud_pso_desc = D3D12GraphicsPipelineStateDesc {
                vs: D3D12ShaderBytecode {
                    shader_bytecode: hud_vs.as_ptr(),
                    bytecode_length: hud_vs.len(),
                },
                ps: D3D12ShaderBytecode {
                    shader_bytecode: hud_ps.as_ptr(),
                    bytecode_length: hud_ps.len(),
                },
                blend_state: blend_desc,
                depth_stencil_state: D3D12DepthStencilDesc {
                    depth_enable: 0,
                    depth_write_mask: 0,
                    depth_func: 8,
                    ..Default::default()
                },
                dsv_format: 0,
                ..cube_pso_desc
            };
            let mut hud_pso: *mut c_void = ptr::null_mut();
            assert_eq!(create_gpso(dev, &hud_pso_desc, &DUMMY_IID, &mut hud_pso), 0);

            // 5. Texture compressée matérielle BC1 64×64 (`DXGI_FORMAT_BC1_UNORM = 71`)
            let bc1_data = build_bc1_checkerboard_64x64();
            let tex_desc = D3D12ResourceDesc {
                dimension: 3, // TEXTURE2D
                width: 64,
                height: 64,
                depth_or_array_size: 1,
                mip_levels: 1,
                format: 71, // DXGI_FORMAT_BC1_UNORM
                sample_desc: DxgiSampleDesc {
                    count: 1,
                    quality: 0,
                },
                ..Default::default()
            };
            let default_hp = D3D12HeapProperties {
                heap_type: 1,
                ..Default::default()
            };
            let mut bc1_tex: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_res(
                    dev,
                    &default_hp,
                    0,
                    &tex_desc,
                    0,
                    ptr::null(),
                    &DUMMY_IID,
                    &mut bc1_tex
                ),
                0
            );
            let get_footprints: extern "win64" fn(
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
            let mut fp = D3D12PlacedSubresourceFootprint::default();
            let mut num_rows = 0u32;
            let mut row_bytes = 0u64;
            let mut total_bytes = 0u64;
            get_footprints(
                dev,
                &tex_desc,
                0,
                1,
                0,
                &mut fp,
                &mut num_rows,
                &mut row_bytes,
                &mut total_bytes,
            );
            assert_eq!(num_rows, 16, "64x64 BC1 doit avoir 16 lignes de blocs 4x4");
            assert_eq!(row_bytes, 128, "16 blocs BC1 * 8 octets = 128 octets/ligne");

            let staging_desc = D3D12ResourceDesc {
                dimension: 1,
                width: total_bytes,
                height: 1,
                depth_or_array_size: 1,
                mip_levels: 1,
                ..Default::default()
            };
            let mut bc1_staging: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_res(
                    dev,
                    &upload_hp,
                    0,
                    &staging_desc,
                    0,
                    ptr::null(),
                    &DUMMY_IID,
                    &mut bc1_staging
                ),
                0
            );
            assert_eq!(res_map(bc1_staging, 0, ptr::null(), &mut p), 0);
            for row in 0..(num_rows as usize) {
                let src = &bc1_data[row * 128..(row + 1) * 128];
                ptr::copy_nonoverlapping(
                    src.as_ptr(),
                    p.cast::<u8>().add(row * (fp.footprint.row_pitch as usize)),
                    128,
                );
            }

            let create_desc_heap: extern "win64" fn(
                *mut c_void,
                *const D3D12DescriptorHeapDesc,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(dev, 14);
            let create_srv: extern "win64" fn(*mut c_void, *mut c_void, *const c_void, usize) =
                vtbl_fn(dev, 18);
            let srv_heap_desc = D3D12DescriptorHeapDesc {
                heap_type: 0,
                num_descriptors: 1,
                flags: 1,
                node_mask: 0,
            };
            let mut srv_heap: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_desc_heap(dev, &srv_heap_desc, &DUMMY_IID, &mut srv_heap),
                0
            );
            let heap_cpu: extern "win64" fn(*mut c_void) -> usize = vtbl_fn(srv_heap, 9);
            let heap_gpu: extern "win64" fn(*mut c_void) -> u64 = vtbl_fn(srv_heap, 10);
            create_srv(dev, bc1_tex, ptr::null(), heap_cpu(srv_heap));

            // 6. Géométrie Cube 3D (stride 36o) + Quad HUD + CB (MVP + 4 offsets Compute) + Readback
            let (cube_verts, cube_indices) = build_cube_mesh(0.62);
            let hud_verts = [
                VertexAaa {
                    pos: [-1.0, 1.0, 0.0],
                    uv: [0.0, 0.0],
                    color: [0.95, 0.12, 0.42, 0.55],
                },
                VertexAaa {
                    pos: [1.0, 1.0, 0.0],
                    uv: [1.0, 0.0],
                    color: [0.95, 0.12, 0.42, 0.55],
                },
                VertexAaa {
                    pos: [1.0, 0.52, 0.0],
                    uv: [1.0, 1.0],
                    color: [0.95, 0.12, 0.42, 0.55],
                },
                VertexAaa {
                    pos: [-1.0, 0.52, 0.0],
                    uv: [0.0, 1.0],
                    color: [0.95, 0.12, 0.42, 0.55],
                },
            ];
            let hud_indices: [u16; 6] = [0, 1, 2, 0, 2, 3];

            let alloc_buf = |bytes: &[u8]| -> *mut c_void {
                let d = D3D12ResourceDesc {
                    dimension: 1,
                    width: bytes.len().max(64) as u64,
                    height: 1,
                    depth_or_array_size: 1,
                    mip_levels: 1,
                    ..Default::default()
                };
                let mut r: *mut c_void = ptr::null_mut();
                assert_eq!(
                    create_res(dev, &upload_hp, 0, &d, 0, ptr::null(), &DUMMY_IID, &mut r),
                    0
                );
                let mut mp: *mut c_void = ptr::null_mut();
                assert_eq!(res_map(r, 0, ptr::null(), &mut mp), 0);
                ptr::copy_nonoverlapping(bytes.as_ptr(), mp.cast::<u8>(), bytes.len());
                r
            };

            let cube_vb = alloc_buf(core::slice::from_raw_parts(
                cube_verts.as_ptr().cast::<u8>(),
                cube_verts.len() * size_of::<VertexAaa>(),
            ));
            let cube_ib = alloc_buf(core::slice::from_raw_parts(
                cube_indices.as_ptr().cast::<u8>(),
                cube_indices.len() * size_of::<u16>(),
            ));
            let hud_vb = alloc_buf(core::slice::from_raw_parts(
                hud_verts.as_ptr().cast::<u8>(),
                size_of_val(&hud_verts),
            ));
            let hud_ib = alloc_buf(core::slice::from_raw_parts(
                hud_indices.as_ptr().cast::<u8>(),
                size_of_val(&hud_indices),
            ));

            // CB graphique : cb0[0..4] = MVP, cb0[4..8] = 4 offsets calculés par le Compute Shader !
            let mvp = build_mvp(0.55, 0.38);
            let mut gfx_cb_floats = [0.0f32; 32];
            gfx_cb_floats[..16].copy_from_slice(&mvp);
            for (i, off) in inst_offsets.iter().enumerate() {
                gfx_cb_floats[16 + i * 4..16 + (i + 1) * 4].copy_from_slice(off);
            }
            let gfx_cb = alloc_buf(core::slice::from_raw_parts(
                gfx_cb_floats.as_ptr().cast::<u8>(),
                size_of_val(&gfx_cb_floats),
            ));

            let rb_desc = D3D12ResourceDesc {
                dimension: 1,
                width: u64::from(W * H * 4),
                height: 1,
                depth_or_array_size: 1,
                mip_levels: 1,
                ..Default::default()
            };
            let rb_hp = D3D12HeapProperties {
                heap_type: 3,
                ..Default::default()
            };
            let mut frame_rb: *mut c_void = ptr::null_mut();
            assert_eq!(
                create_res(
                    dev,
                    &rb_hp,
                    0,
                    &rb_desc,
                    0,
                    ptr::null(),
                    &DUMMY_IID,
                    &mut frame_rb
                ),
                0
            );

            // 7. Enregistrement de la frame AAA : Upload BC1 + 4 Cubes Instanciés + HUD Blend/Scissor + Readback
            assert_eq!(cl_reset(cl, alloc, cube_pso), 0);
            let dst_loc = D3D12TextureCopyLocation {
                resource: bc1_tex,
                copy_type: 0,
                _pad: 0,
                placed_footprint: D3D12PlacedSubresourceFootprint::default(),
            };
            let src_loc = D3D12TextureCopyLocation {
                resource: bc1_staging,
                copy_type: 1,
                _pad: 0,
                placed_footprint: fp,
            };
            cl_copy_tex(cl, &dst_loc, 0, 0, 0, &src_loc, ptr::null());

            let vp = D3D12Viewport {
                top_left_x: 0.0,
                top_left_y: 0.0,
                width: W as f32,
                height: H as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            };
            let full_sc = D3D12Rect {
                left: 0,
                top: 0,
                right: W as i32,
                bottom: H as i32,
            };
            let clear = [0.05f32, 0.07, 0.16, 1.0];
            cl_vp(cl, 1, &vp);
            cl_scissor(cl, 1, &full_sc);
            cl_clear_rtv(cl, 0, &clear, 0, ptr::null());
            cl_clear_dsv(cl, 0, 1, 1.0, 0, 0, ptr::null());
            cl_set_gfx_cbv(cl, 0, res_gpu_va(gfx_cb));
            cl_set_gfx_table(cl, 1, heap_gpu(srv_heap));
            let cube_vbv = D3D12VertexBufferView {
                buffer_location: res_gpu_va(cube_vb),
                size_in_bytes: (cube_verts.len() * size_of::<VertexAaa>()) as u32,
                stride_in_bytes: size_of::<VertexAaa>() as u32,
            };
            let cube_ibv = D3D12IndexBufferView {
                buffer_location: res_gpu_va(cube_ib),
                size_in_bytes: (cube_indices.len() * size_of::<u16>()) as u32,
                format: 57,
            };
            cl_set_vb(cl, 0, 1, &cube_vbv);
            cl_set_ib(cl, &cube_ibv);
            // 1 seul draw call instancié dessine les 4 cubes 3D texturés BC1 !
            cl_draw_idx(cl, cube_indices.len() as u32, 4, 0, 0, 0);

            // Passe 2 : HUD translucide (Alpha Blending) + Scissor Rect [40..472, 24..104]
            let hud_sc = D3D12Rect {
                left: 40,
                top: 24,
                right: 472,
                bottom: 104,
            };
            cl_set_pso(cl, hud_pso);
            cl_scissor(cl, 1, &hud_sc);
            let hud_vbv = D3D12VertexBufferView {
                buffer_location: res_gpu_va(hud_vb),
                size_in_bytes: size_of_val(&hud_verts) as u32,
                stride_in_bytes: size_of::<VertexAaa>() as u32,
            };
            let hud_ibv = D3D12IndexBufferView {
                buffer_location: res_gpu_va(hud_ib),
                size_in_bytes: size_of_val(&hud_indices) as u32,
                format: 57,
            };
            cl_set_vb(cl, 0, 1, &hud_vbv);
            cl_set_ib(cl, &hud_ibv);
            cl_draw_idx(cl, hud_indices.len() as u32, 1, 0, 0, 0);

            cl_copy_res(cl, frame_rb, backbuffer);
            assert_eq!(cl_close(cl), 0);
            q_exec(queue, 1, lists.as_ptr());

            let sc_present: extern "win64" fn(*mut c_void, u32, u32) -> i32 = vtbl_fn(sc, 8);
            assert_eq!(sc_present(sc, 1, 0), 0);

            // 8. Vérification GPU pixel par pixel : fond, 4 cubes instanciés BC1, HUD alpha-blended, scissor !
            assert_eq!(res_map(frame_rb, 0, ptr::null(), &mut p), 0);
            let pixels = core::slice::from_raw_parts(p.cast::<u8>(), (W * H * 4) as usize);
            let px = |x: usize, y: usize| -> [u8; 4] {
                let i = (y * (W as usize) + x) * 4;
                [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
            };

            let bg = px(10, 60);
            assert!(
                bg[0] < 30 && bg[1] < 35 && bg[2] > 25 && bg[2] < 60,
                "bord hors scissor doit rester fond bleu nuit: {bg:?}"
            );
            let hud = px(256, 60);
            assert!(
                hud[0] > 110 && hud[0] < 190 && hud[2] > 50,
                "bandeau HUD alpha-blended dans scissor inattendu: {hud:?}"
            );

            // Vérifie que les 4 quadrants contiennent chacun des pixels lumineux des 4 cubes instanciés BC1 !
            let quads = [
                ("haut-gauche", 40usize..240, 120usize..280),
                ("haut-droit", 272usize..472, 120usize..280),
                ("bas-gauche", 40usize..240, 290usize..470),
                ("bas-droit", 272usize..472, 290usize..470),
            ];
            for (name, xr, yr) in quads {
                let mut lit = 0usize;
                for y in yr {
                    for x in xr.clone() {
                        let c = px(x, y);
                        if usize::from(c[0]) + usize::from(c[1]) + usize::from(c[2]) > 180 {
                            lit += 1;
                        }
                    }
                }
                assert!(
                    lit > 500,
                    "quadrant {name} devrait contenir une instance de cube 3D BC1 (lit={lit})"
                );
                println!("[AAA] Quadrant {name} vérifié ({lit} pixels 3D BC1 lumineux)");
            }

            println!("[AAA] Vérification HUD Alpha-Blend={hud:?} & Scissor Hors-Zone={bg:?} OK !");
            println!(
                "=== SUCCES : Pipeline AAA Series X (Compute + Placed Alias + BC1 + Instancing + Blend/Scissor) ==="
            );
        }
    }
}
