//! Harnais PE32+ x86-64 **programmes GPU natifs RDNA 2** (`Rdna2Native`, conteneur `SBR2`) via
//! `d3d12_xs.dll` : aucun jeu, aucun shader DXBC. Le harnais :
//!
//! 1. crée un périphérique, une file, une swapchain (64x64) et ses tas de descripteurs ;
//! 2. **compute** : un programme RDNA 2 écrit à la main (`out[i] = f32(in[i]) * échelle + biais`)
//!    lit deux V# (tas), un biais par CBV racine et une échelle par constante racine ;
//! 3. **graphique** : un VS + PS RDNA 2 dessinent un triangle plein écran texturé (V# des sommets,
//!    T# + S# lus dans les tas, teinte par constantes racine) ;
//! 4. **image de stockage** : deux programmes compute écrivent (`image_store`) puis relisent et
//!    modifient (`image_load` + `image_store`) une texture 2D RGBA32F 8x8 par un UAV (T#) ;
//! 5. relit le résultat (tampon de lecture / UAV) et vérifie les valeurs et les pixels.
//!
//! Les octets de code ont été assemblés avec `llvm-mc -triple=amdgcn -mcpu=gfx1030
//! -mattr=+wavefrontsize32` et sont figés dans ce fichier. Le format `SBR2` v1 est décrit dans
//! `docs/rdna2_pipeline.md` ; ce harnais en contient un constructeur minimal.

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "[rdna2_native] Compiler pour PE32+ x86-64 :\n\
         cargo build --target x86_64-pc-windows-gnu -p sb-harness-rdna2-native"
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
    /// `D3D12_DESCRIPTOR_RANGE` (v1.0).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct D3D12DescriptorRange {
        range_type: u32,
        num_descriptors: u32,
        base_shader_register: u32,
        register_space: u32,
        offset_in_descriptors_from_table_start: u32,
    }
    /// `D3D12_ROOT_PARAMETER` : type, union de 16 octets (à l'offset 8), visibilité.
    #[repr(C)]
    #[derive(Clone, Copy)]
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

    const _: () = {
        assert!(size_of::<D3D12GraphicsPipelineStateDesc>() == 656);
        assert!(offset_of!(D3D12GraphicsPipelineStateDesc, vs) == 8);
        assert!(size_of::<D3D12ResourceDesc>() == 56);
        assert!(size_of::<DxgiSwapChainDesc1>() == 48);
        assert!(size_of::<D3D12RootParameter>() == 32);
        assert!(size_of::<D3D12DescriptorRange>() == 20);
        assert!(offset_of!(D3D12RootParameter, data2) == 16);
        assert!(offset_of!(D3D12RootParameter, shader_visibility) == 24);
    };

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

    /// Récupère le pointeur de fonction au slot `index` de la vtable COM d'un objet.
    ///
    /// # Safety
    /// `obj` doit être un pointeur COM valide dont la vtable contient `index`.
    unsafe fn vtbl_fn<F: Copy>(obj: *mut c_void, index: usize) -> F {
        // SAFETY: déréférencement de vtable COM x86-64.
        unsafe {
            let vtbl = *(obj.cast::<*const *const c_void>());
            core::mem::transmute_copy(&*vtbl.add(index))
        }
    }

    // ==================== Conteneur SBR2 v1 (voir docs/rdna2_pipeline.md) ====================

    /// `(kind, loc_kind, loc_a, loc_b, stride)` : kind 0 = V#, 1 = T#, 2 = S# ;
    /// loc_kind 0 = Table(sgpr, offset), 1 = Sgprs, 2 = Pointer.
    type Res = (u32, u32, u32, u32, u32);

    struct Meta {
        stage: u32, // 0 VS, 1 PS, 2 CS
        local: [u32; 3],
        user_sgprs: u32,
        input_vgprs: u32,
        sys_flags: u32,
        res: &'static [Res],
    }

    fn sbr2(m: &Meta, code: &[u32]) -> Vec<u8> {
        let total = 16 + m.res.len() * 5 + code.len();
        let mut w: Vec<u32> = vec![
            u32::from_le_bytes(*b"SBR2"),
            1,  // version
            16, // header_dwords
            m.stage,
            32, // wave32
            m.local[0],
            m.local[1],
            m.local[2],
            0, // lds_dwords
            m.user_sgprs,
            m.input_vgprs,
            m.sys_flags,
            m.res.len() as u32,
            code.len() as u32,
            total as u32,
            0,
        ];
        for r in m.res {
            w.extend_from_slice(&[r.0, r.1, r.2, r.3, r.4]);
        }
        w.extend_from_slice(code);
        w.iter().flat_map(|d| d.to_le_bytes()).collect()
    }

    // llvm-mc -triple=amdgcn -mcpu=gfx1030 -mattr=+wavefrontsize32 (sources dans la doc).
    const VS_CODE: [u32; 14] = [
        0xf4080100, 0xfa000000, 0xbf8cc07f, 0x34020085, 0xe0381000, 0x80010401, 0xe0381010,
        0x80010801, 0xbf8c3f70, 0xf800020f, 0x0b0a0908, 0xf80008cf, 0x07060504, 0xbf810000,
    ];
    const PS_CODE: [u32; 21] = [
        0xf40c0200, 0xfa000020, 0xf4080401, 0xfa000000, 0xbf8cc07f, 0xc8080000, 0xc80c0100,
        0xc8090001, 0xc80d0101, 0xf0800f08, 0x00820402, 0xbf8c3f70, 0x10080804, 0x100a0a05,
        0x100c0c06, 0x100e0e07, 0x5e080b04, 0x5e0a0f06, 0xf8001c0f, 0x00000504, 0xbf810000,
    ];
    const CS_CODE: [u32; 19] = [
        0xf4080200, 0xfa000000, 0xf4080300, 0xfa000020, 0xf4000181, 0xfa000000, 0x8f078605,
        0xbf8cc07f, 0x4a000007, 0x34020082, 0xe0301000, 0x80020201, 0xbf8c3f70, 0x7e060d02,
        0xd54b0003, 0x00180903, 0xe0701000, 0x80030301, 0xbf810000,
    ];

    /// `cs_store_2d` : `image_store (x, y, 0.5, 1.0)` dans le texel `(x, y)` (T# : table s[0:1] + 0).
    const CS_STORE_2D: [u32; 13] = [
        0xf40c0200, 0xfa000000, 0xbf8cc07f, 0x36020087, 0x2c040083, 0x7e060202, 0x7e080d01,
        0x7e0a0d02, 0x7e0c02f0, 0x7e0e02f2, 0xf0201f08, 0x00020401, 0xbf810000,
    ];
    /// `cs_storage_rw_2d` : `image_load` puis `image_store (r + 1, g + 2, b, a)`.
    const CS_RW_2D: [u32; 14] = [
        0xf40c0200, 0xfa000000, 0xbf8cc07f, 0x36020087, 0x2c040083, 0x7e060202, 0xf0001f08,
        0x00020401, 0xbf8c3f70, 0x060808f2, 0x060a0af4, 0xf0201f08, 0x00020401, 0xbf810000,
    ];

    fn storage_blob(code: &[u32]) -> Vec<u8> {
        sbr2(
            &Meta {
                stage: 2,
                local: [64, 1, 1],
                user_sgprs: 2,
                input_vgprs: 1,
                sys_flags: 1,
                res: &[(1, 0, 0, 0, 0)], // T# image de stockage : table s[0:1] + 0
            },
            code,
        )
    }

    fn vs_blob() -> Vec<u8> {
        sbr2(
            &Meta {
                stage: 0,
                local: [1, 1, 1],
                user_sgprs: 2,
                input_vgprs: 1,
                sys_flags: 0,
                res: &[(0, 0, 0, 0, 0)], // V# vertex : table s[0:1] + 0
            },
            &VS_CODE,
        )
    }
    fn ps_blob() -> Vec<u8> {
        sbr2(
            &Meta {
                stage: 1,
                local: [1, 1, 1],
                user_sgprs: 8,
                input_vgprs: 2,
                sys_flags: 0,
                // T# : table s[0:1] + 0x20 ; S# : table s[2:3] + 0
                res: &[(1, 0, 0, 0x20, 0), (2, 0, 2, 0, 0)],
            },
            &PS_CODE,
        )
    }
    fn cs_blob() -> Vec<u8> {
        sbr2(
            &Meta {
                stage: 2,
                local: [64, 1, 1],
                user_sgprs: 5,
                input_vgprs: 1,
                sys_flags: 1, // identifiant de groupe x dans s5
                res: &[
                    (0, 0, 0, 0, 0),    // V# entrée : table s[0:1] + 0
                    (0, 0, 0, 0x20, 0), // V# sortie : table s[0:1] + 0x20
                    (0, 2, 2, 0, 0),    // pointeur racine s[2:3] (CBV : biais)
                ],
            },
            &CS_CODE,
        )
    }

    // ==================== Aides D3D12 ====================

    struct Ctx {
        dev: *mut c_void,
        queue: *mut c_void,
    }

    impl Ctx {
        unsafe fn heap(&self, ty: u32, n: u32) -> (usize, u64) {
            // SAFETY: vtables COM du bridge.
            unsafe {
                let f: extern "win64" fn(
                    *mut c_void,
                    *const D3D12DescriptorHeapDesc,
                    *const Guid,
                    *mut *mut c_void,
                ) -> i32 = vtbl_fn(self.dev, 14);
                let d = D3D12DescriptorHeapDesc {
                    heap_type: ty,
                    num_descriptors: n,
                    flags: 1,
                    node_mask: 1,
                };
                let mut h: *mut c_void = ptr::null_mut();
                assert_eq!(f(self.dev, &d, &DUMMY_IID, &mut h), 0);
                let cpu: extern "win64" fn(*mut c_void) -> usize = vtbl_fn(h, 9);
                let gpu: extern "win64" fn(*mut c_void) -> u64 = vtbl_fn(h, 10);
                (cpu(h), gpu(h))
            }
        }

        unsafe fn resource(&self, desc: &D3D12ResourceDesc) -> *mut c_void {
            // SAFETY: vtables COM du bridge.
            unsafe {
                let f: extern "win64" fn(
                    *mut c_void,
                    *const D3D12HeapProperties,
                    u32,
                    *const D3D12ResourceDesc,
                    u32,
                    *const c_void,
                    *const Guid,
                    *mut *mut c_void,
                ) -> i32 = vtbl_fn(self.dev, 27);
                let hp = D3D12HeapProperties {
                    heap_type: 2,
                    ..Default::default()
                };
                let mut r: *mut c_void = ptr::null_mut();
                assert_eq!(
                    f(self.dev, &hp, 0, desc, 0, ptr::null(), &DUMMY_IID, &mut r),
                    0
                );
                r
            }
        }

        unsafe fn buffer(&self, bytes: &[u8], min_len: usize) -> *mut c_void {
            // SAFETY: vtables COM du bridge ; `Map` renvoie la mémoire de la ressource.
            unsafe {
                let len = bytes.len().max(min_len);
                let r = self.resource(&D3D12ResourceDesc {
                    dimension: 1,
                    width: len as u64,
                    height: 1,
                    depth_or_array_size: 1,
                    mip_levels: 1,
                    ..Default::default()
                });
                if !bytes.is_empty() {
                    write_resource(r, bytes);
                }
                r
            }
        }

        unsafe fn root_sig(&self, params: &[(u32, u32)]) -> *mut c_void {
            // SAFETY: vtables COM du bridge ; les structures vivent pendant les appels.
            unsafe {
                let range = D3D12DescriptorRange {
                    num_descriptors: 4,
                    ..Default::default()
                };
                let ps: Vec<D3D12RootParameter> = params
                    .iter()
                    .map(|&(ty, n)| D3D12RootParameter {
                        parameter_type: ty,
                        _pad0: 0,
                        // table : NumDescriptorRanges ; constantes : ShaderRegister
                        data0: u32::from(ty == 0),
                        data1: 0,
                        // table : pDescriptorRanges ; constantes : Num32BitValues
                        data2: if ty == 0 {
                            ptr::from_ref(&range) as usize as u64
                        } else {
                            u64::from(n)
                        },
                        shader_visibility: 0,
                        _pad1: 0,
                    })
                    .collect();
                let desc = D3D12RootSignatureDesc {
                    num_parameters: ps.len() as u32,
                    _pad0: 0,
                    parameters: ps.as_ptr(),
                    num_static_samplers: 0,
                    _pad1: 0,
                    static_samplers: ptr::null(),
                    flags: 0,
                    _pad2: 0,
                };
                let mut blob: *mut c_void = ptr::null_mut();
                let mut err: *mut c_void = ptr::null_mut();
                assert_eq!(
                    D3D12SerializeRootSignature(
                        ptr::from_ref(&desc).cast(),
                        1,
                        &mut blob,
                        &mut err
                    ),
                    0
                );
                let blob_ptr: extern "win64" fn(*mut c_void) -> *mut c_void = vtbl_fn(blob, 3);
                let blob_size: extern "win64" fn(*mut c_void) -> usize = vtbl_fn(blob, 4);
                let create: extern "win64" fn(
                    *mut c_void,
                    u32,
                    *const c_void,
                    usize,
                    *const Guid,
                    *mut *mut c_void,
                ) -> i32 = vtbl_fn(self.dev, 16);
                let mut rs: *mut c_void = ptr::null_mut();
                assert_eq!(
                    create(
                        self.dev,
                        0,
                        blob_ptr(blob),
                        blob_size(blob),
                        &DUMMY_IID,
                        &mut rs
                    ),
                    0
                );
                rs
            }
        }

        unsafe fn cmd_list(&self) -> *mut c_void {
            // SAFETY: vtables COM du bridge.
            unsafe {
                let alloc: extern "win64" fn(
                    *mut c_void,
                    u32,
                    *const Guid,
                    *mut *mut c_void,
                ) -> i32 = vtbl_fn(self.dev, 9);
                let mut a: *mut c_void = ptr::null_mut();
                assert_eq!(alloc(self.dev, 0, &DUMMY_IID, &mut a), 0);
                let create: extern "win64" fn(
                    *mut c_void,
                    u32,
                    u32,
                    *mut c_void,
                    *mut c_void,
                    *const Guid,
                    *mut *mut c_void,
                ) -> i32 = vtbl_fn(self.dev, 12);
                let mut cl: *mut c_void = ptr::null_mut();
                assert_eq!(
                    create(self.dev, 0, 0, a, ptr::null_mut(), &DUMMY_IID, &mut cl),
                    0
                );
                cl
            }
        }

        unsafe fn execute_and_wait(&self, cl: *mut c_void) {
            // SAFETY: vtables COM du bridge.
            unsafe {
                let close: extern "win64" fn(*mut c_void) -> i32 = vtbl_fn(cl, 9);
                assert_eq!(close(cl), 0);
                let exec: extern "win64" fn(*mut c_void, u32, *const *mut c_void) =
                    vtbl_fn(self.queue, 10);
                exec(self.queue, 1, [cl].as_ptr());
                let signal: extern "win64" fn(*mut c_void, *mut c_void, u64) -> i32 =
                    vtbl_fn(self.queue, 14);
                let create_fence: extern "win64" fn(
                    *mut c_void,
                    u64,
                    u32,
                    *const Guid,
                    *mut *mut c_void,
                ) -> i32 = vtbl_fn(self.dev, 36);
                let mut fence: *mut c_void = ptr::null_mut();
                assert_eq!(create_fence(self.dev, 0, 0, &DUMMY_IID, &mut fence), 0);
                assert_eq!(signal(self.queue, fence, 1), 0);
                let completed: extern "win64" fn(*mut c_void) -> u64 = vtbl_fn(fence, 8);
                assert_eq!(completed(fence), 1, "ID3D12Fence::GetCompletedValue");
            }
        }
    }

    unsafe fn map_resource(res: *mut c_void) -> *mut u8 {
        // SAFETY: vtable COM `ID3D12Resource::Map`.
        unsafe {
            let map: extern "win64" fn(*mut c_void, u32, *const c_void, *mut *mut c_void) -> i32 =
                vtbl_fn(res, 8);
            let mut p: *mut c_void = ptr::null_mut();
            assert_eq!(map(res, 0, ptr::null(), &mut p), 0);
            p.cast()
        }
    }

    unsafe fn write_resource(res: *mut c_void, bytes: &[u8]) {
        // SAFETY: la ressource a au moins `bytes.len()` octets.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), map_resource(res), bytes.len()) };
    }

    unsafe fn read_resource(res: *mut c_void, len: usize) -> Vec<u8> {
        // SAFETY: la ressource a au moins `len` octets.
        unsafe { std::slice::from_raw_parts(map_resource(res), len).to_vec() }
    }

    fn u32s(v: &[u32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    fn f32s(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    /// V# brut sur un tampon (SRV si `uav == false`).
    unsafe fn buffer_view(ctx: &Ctx, res: *mut c_void, bytes: u32, uav: bool, dest: usize) {
        // SAFETY: vtables COM du bridge.
        unsafe {
            if uav {
                // D3D12_UNORDERED_ACCESS_VIEW_DESC : format, dim=BUFFER, Buffer{First, Num, Stride, Counter, Flags=RAW}
                let mut d = u32s(&[39, 1, 0, 0, bytes / 4, 0, 0, 0, 1]);
                d.resize(48, 0);
                let f: extern "win64" fn(*mut c_void, *mut c_void, *mut c_void, *const u8, usize) =
                    vtbl_fn(ctx.dev, 19);
                f(ctx.dev, res, ptr::null_mut(), d.as_ptr(), dest);
            } else {
                // D3D12_SHADER_RESOURCE_VIEW_DESC : format, dim=BUFFER, mapping, Buffer{First, Num, Stride, Flags=RAW}
                let mut d = u32s(&[39, 1, 0x1688, 0, 0, 0, bytes / 4, 0, 1]);
                d.resize(48, 0);
                let f: extern "win64" fn(*mut c_void, *mut c_void, *const u8, usize) =
                    vtbl_fn(ctx.dev, 18);
                f(ctx.dev, res, d.as_ptr(), dest);
            }
        }
    }

    // ==================== Test compute ====================

    unsafe fn test_compute(ctx: &Ctx) -> bool {
        // SAFETY: vtables COM du bridge.
        unsafe {
            let input: Vec<u32> = (0..128).collect();
            let in_res = ctx.buffer(&u32s(&input), 512);
            let out_res = ctx.buffer(&[], 512);
            let cbv_res = ctx.buffer(&f32s(&[3.0, 0.0, 0.0, 0.0]), 256);
            let (cpu, gpu) = ctx.heap(0, 4);
            buffer_view(ctx, in_res, 512, false, cpu);
            buffer_view(ctx, out_res, 512, true, cpu + 32);

            // Signature racine : p0 table (s0:s1), p1 CBV (s2:s3), p2 constante (s4).
            let rs = ctx.root_sig(&[(0, 0), (2, 0), (1, 1)]);
            let cs = cs_blob();
            let desc = D3D12ComputePipelineStateDesc {
                root_signature: rs,
                cs: D3D12ShaderBytecode {
                    shader_bytecode: cs.as_ptr(),
                    bytecode_length: cs.len(),
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
            let create_pso: extern "win64" fn(
                *mut c_void,
                *const D3D12ComputePipelineStateDesc,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(ctx.dev, 11);
            let mut pso: *mut c_void = ptr::null_mut();
            let hr = create_pso(ctx.dev, &desc, &DUMMY_IID, &mut pso);
            if hr != 0 {
                println!(
                    "[rdna2_native] ECHEC CreateComputePipelineState (Rdna2Native) hr={hr:#x}"
                );
                return false;
            }
            println!(
                "[rdna2_native] Pipeline compute RDNA 2 natif ({} octets SBR2) compilé",
                cs.len()
            );

            let cl = ctx.cmd_list();
            let set_pso: extern "win64" fn(*mut c_void, *mut c_void) = vtbl_fn(cl, 25);
            let set_rs: extern "win64" fn(*mut c_void, *mut c_void) = vtbl_fn(cl, 29);
            let set_table: extern "win64" fn(*mut c_void, u32, u64) = vtbl_fn(cl, 31);
            let set_consts: extern "win64" fn(*mut c_void, u32, u32, *const u32, u32) =
                vtbl_fn(cl, 35);
            let set_cbv: extern "win64" fn(*mut c_void, u32, u64) = vtbl_fn(cl, 37);
            let dispatch: extern "win64" fn(*mut c_void, u32, u32, u32) = vtbl_fn(cl, 14);
            let gpu_va: extern "win64" fn(*mut c_void) -> u64 = vtbl_fn(cbv_res, 11);
            set_pso(cl, pso);
            set_rs(cl, rs);
            set_table(cl, 0, gpu);
            set_cbv(cl, 1, gpu_va(cbv_res));
            set_consts(cl, 2, 1, [0.5f32.to_bits()].as_ptr(), 0);
            dispatch(cl, 2, 1, 1);
            ctx.execute_and_wait(cl);

            let out = read_resource(out_res, 512);
            let mut bad = 0;
            for i in 0..128usize {
                let v = f32::from_le_bytes([
                    out[4 * i],
                    out[4 * i + 1],
                    out[4 * i + 2],
                    out[4 * i + 3],
                ]);
                let want = i as f32 * 0.5 + 3.0;
                if v != want {
                    if bad < 4 {
                        println!("[rdna2_native] compute out[{i}] = {v}, attendu {want}");
                    }
                    bad += 1;
                }
            }
            if bad != 0 {
                println!("[rdna2_native] ECHEC compute : {bad}/128 valeurs fausses");
                return false;
            }
            println!(
                "[rdna2_native] Compute RDNA 2 : 128 valeurs out[i] = i*0.5+3 vérifiées (V# table, CBV racine, constante racine)"
            );
            true
        }
    }

    // ==================== Test image de stockage ====================

    unsafe fn run_cs(ctx: &Ctx, code: &[u32], tex_table: u64) -> bool {
        // SAFETY: vtables COM du bridge.
        unsafe {
            let rs = ctx.root_sig(&[(0, 0)]);
            let cs = storage_blob(code);
            let desc = D3D12ComputePipelineStateDesc {
                root_signature: rs,
                cs: D3D12ShaderBytecode {
                    shader_bytecode: cs.as_ptr(),
                    bytecode_length: cs.len(),
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
            let create_pso: extern "win64" fn(
                *mut c_void,
                *const D3D12ComputePipelineStateDesc,
                *const Guid,
                *mut *mut c_void,
            ) -> i32 = vtbl_fn(ctx.dev, 11);
            let mut pso: *mut c_void = ptr::null_mut();
            let hr = create_pso(ctx.dev, &desc, &DUMMY_IID, &mut pso);
            if hr != 0 {
                println!(
                    "[rdna2_native] ECHEC CreateComputePipelineState (image de stockage) hr={hr:#x}"
                );
                return false;
            }
            let cl = ctx.cmd_list();
            let set_pso: extern "win64" fn(*mut c_void, *mut c_void) = vtbl_fn(cl, 25);
            let set_rs: extern "win64" fn(*mut c_void, *mut c_void) = vtbl_fn(cl, 29);
            let set_table: extern "win64" fn(*mut c_void, u32, u64) = vtbl_fn(cl, 31);
            let dispatch: extern "win64" fn(*mut c_void, u32, u32, u32) = vtbl_fn(cl, 14);
            set_pso(cl, pso);
            set_rs(cl, rs);
            set_table(cl, 0, tex_table);
            dispatch(cl, 1, 1, 1);
            ctx.execute_and_wait(cl);
            true
        }
    }

    unsafe fn test_storage_image(ctx: &Ctx) -> bool {
        // SAFETY: vtables COM du bridge.
        unsafe {
            // Texture 2D RGBA32F 8x8 (dimension 3, DXGI_FORMAT_R32G32B32A32_FLOAT = 2).
            let tex = ctx.resource(&D3D12ResourceDesc {
                dimension: 3,
                width: 8,
                height: 8,
                depth_or_array_size: 1,
                mip_levels: 1,
                format: 2,
                sample_desc: DxgiSampleDesc {
                    count: 1,
                    quality: 0,
                },
                ..Default::default()
            });
            write_resource(tex, &vec![0u8; 8 * 8 * 16]);
            let (cpu, gpu) = ctx.heap(0, 2);
            // D3D12_UNORDERED_ACCESS_VIEW_DESC : format, dim = TEXTURE2D (4), MipSlice, PlaneSlice.
            let mut d = u32s(&[2, 4, 0, 0]);
            d.resize(48, 0);
            let create_uav: extern "win64" fn(
                *mut c_void,
                *mut c_void,
                *mut c_void,
                *const u8,
                usize,
            ) = vtbl_fn(ctx.dev, 19);
            create_uav(ctx.dev, tex, ptr::null_mut(), d.as_ptr(), cpu);

            if !run_cs(ctx, &CS_STORE_2D, gpu) {
                return false;
            }
            let rd = |out: &[u8], i: usize| {
                f32::from_le_bytes([out[4 * i], out[4 * i + 1], out[4 * i + 2], out[4 * i + 3]])
            };
            let out = read_resource(tex, 8 * 8 * 16);
            let mut bad = 0;
            for y in 0..8usize {
                for x in 0..8usize {
                    let o = (y * 8 + x) * 4;
                    let got = [
                        rd(&out, o),
                        rd(&out, o + 1),
                        rd(&out, o + 2),
                        rd(&out, o + 3),
                    ];
                    if got != [x as f32, y as f32, 0.5, 1.0] {
                        if bad < 4 {
                            println!("[rdna2_native] image_store ({x},{y}) = {got:?}");
                        }
                        bad += 1;
                    }
                }
            }
            if bad != 0 {
                println!("[rdna2_native] ECHEC image_store : {bad}/64 texels faux");
                return false;
            }
            println!(
                "[rdna2_native] Image de stockage : 64 texels écrits par image_store, relus en mémoire"
            );

            if !run_cs(ctx, &CS_RW_2D, gpu) {
                return false;
            }
            let out = read_resource(tex, 8 * 8 * 16);
            for y in 0..8usize {
                for x in 0..8usize {
                    let o = (y * 8 + x) * 4;
                    let got = [
                        rd(&out, o),
                        rd(&out, o + 1),
                        rd(&out, o + 2),
                        rd(&out, o + 3),
                    ];
                    if got != [x as f32 + 1.0, y as f32 + 2.0, 0.5, 1.0] {
                        println!("[rdna2_native] ECHEC image_load/store ({x},{y}) = {got:?}");
                        return false;
                    }
                }
            }
            println!(
                "[rdna2_native] Image de stockage : image_load + image_store (r+1, g+2) vérifiés"
            );
            true
        }
    }

    // ==================== Test graphique ====================

    unsafe fn test_graphics(ctx: &Ctx, backbuffer: *mut c_void) -> bool {
        // SAFETY: vtables COM du bridge.
        unsafe {
            const W: usize = 64;
            // Sommets : triangle plein écran, 32 octets (position.xyzw, uv.xyzw).
            let verts: [f32; 24] = [
                -1.0, -1.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, //
                3.0, -1.0, 0.0, 1.0, 2.0, 0.0, 0.0, 1.0, //
                -1.0, 3.0, 0.0, 1.0, 0.0, 2.0, 0.0, 1.0,
            ];
            let vb = ctx.buffer(&f32s(&verts), 96);
            let tex = ctx.resource(&D3D12ResourceDesc {
                dimension: 3,
                width: 2,
                height: 2,
                depth_or_array_size: 1,
                mip_levels: 1,
                format: 28,
                sample_desc: DxgiSampleDesc {
                    count: 1,
                    quality: 0,
                },
                ..Default::default()
            });
            let texels: [[u8; 4]; 4] = [
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 255, 255],
                [255, 255, 255, 255],
            ];
            write_resource(tex, &texels.iter().flatten().copied().collect::<Vec<u8>>());
            let rb = ctx.buffer(&[], W * W * 4);

            // Tas CBV/SRV/UAV : slot 0 = V# sommets, slot 1 = T#. Tas de samplers : slot 0 = S#.
            let (cpu, gpu) = ctx.heap(0, 4);
            let (scpu, sgpu) = ctx.heap(3, 2);
            buffer_view(ctx, vb, 96, false, cpu);
            let mut srv = u32s(&[28, 4, 0x1688, 0, 0, 1, 0]);
            srv.extend(0.0f32.to_le_bytes());
            srv.resize(48, 0);
            let create_srv: extern "win64" fn(*mut c_void, *mut c_void, *const u8, usize) =
                vtbl_fn(ctx.dev, 18);
            create_srv(ctx.dev, tex, srv.as_ptr(), cpu + 32);
            // D3D12_SAMPLER_DESC : MIN_MAG_MIP_POINT, CLAMP x3.
            let mut smp = u32s(&[0, 3, 3, 3]);
            smp.extend(0.0f32.to_le_bytes());
            smp.extend(u32s(&[1, 8]));
            for c in [0.0f32, 0.0, 0.0, 0.0, 0.0, 12.0] {
                smp.extend(c.to_le_bytes());
            }
            let create_smp: extern "win64" fn(*mut c_void, *const u8, usize) = vtbl_fn(ctx.dev, 22);
            create_smp(ctx.dev, smp.as_ptr(), scpu);

            // Signature racine : p0 table CBV/SRV/UAV (s0:s1), p1 table samplers (s2:s3), p2 4 constantes (s4..s7).
            let rs = ctx.root_sig(&[(0, 0), (0, 0), (1, 4)]);
            let (vs, ps) = (vs_blob(), ps_blob());
            let none = D3D12ShaderBytecode {
                shader_bytecode: ptr::null(),
                bytecode_length: 0,
            };
            let pso_desc = D3D12GraphicsPipelineStateDesc {
                root_signature: rs,
                vs: D3D12ShaderBytecode {
                    shader_bytecode: vs.as_ptr(),
                    bytecode_length: vs.len(),
                },
                ps: D3D12ShaderBytecode {
                    shader_bytecode: ps.as_ptr(),
                    bytecode_length: ps.len(),
                },
                ds: none,
                hs: none,
                gs: none,
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
                    num_elements: 0,
                    _pad: 0,
                },
                ib_strip_cut_value: 0,
                primitive_topology_type: 3,
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
            ) -> i32 = vtbl_fn(ctx.dev, 10);
            let mut pso: *mut c_void = ptr::null_mut();
            let hr = create_pso(ctx.dev, &pso_desc, &DUMMY_IID, &mut pso);
            if hr != 0 {
                println!(
                    "[rdna2_native] ECHEC CreateGraphicsPipelineState (Rdna2Native) hr={hr:#x}"
                );
                return false;
            }
            println!(
                "[rdna2_native] Pipeline graphique RDNA 2 natif (VS={}o, PS={}o) compilé",
                vs.len(),
                ps.len()
            );

            let cl = ctx.cmd_list();
            let set_pso: extern "win64" fn(*mut c_void, *mut c_void) = vtbl_fn(cl, 25);
            let set_rs: extern "win64" fn(*mut c_void, *mut c_void) = vtbl_fn(cl, 30);
            let set_table: extern "win64" fn(*mut c_void, u32, u64) = vtbl_fn(cl, 32);
            let set_consts: extern "win64" fn(*mut c_void, u32, u32, *const u32, u32) =
                vtbl_fn(cl, 36);
            let vp_fn: extern "win64" fn(*mut c_void, u32, *const D3D12Viewport) = vtbl_fn(cl, 21);
            let clear: extern "win64" fn(*mut c_void, usize, *const [f32; 4], u32, *const c_void) =
                vtbl_fn(cl, 48);
            let draw: extern "win64" fn(*mut c_void, u32, u32, u32, u32) = vtbl_fn(cl, 12);
            let copy: extern "win64" fn(*mut c_void, *mut c_void, *mut c_void) = vtbl_fn(cl, 17);
            let vp = D3D12Viewport {
                top_left_x: 0.0,
                top_left_y: 0.0,
                width: W as f32,
                height: W as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            };
            clear(cl, 0, &[0.0, 0.0, 0.0, 1.0], 0, ptr::null());
            set_pso(cl, pso);
            set_rs(cl, rs);
            set_table(cl, 0, gpu);
            set_table(cl, 1, sgpu);
            // Teinte (1, 1, 1, 1) : rendu fidèle à la texture.
            set_consts(cl, 2, 4, [1.0f32.to_bits(); 4].as_ptr(), 0);
            vp_fn(cl, 1, &vp);
            draw(cl, 3, 1, 0, 0);
            copy(cl, rb, backbuffer);
            ctx.execute_and_wait(cl);

            let pixels = read_resource(rb, W * W * 4);
            let px = |x: usize, y: usize| {
                let i = (y * W + x) * 4;
                [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
            };
            let checks = [
                ((16, 48), texels[0], "bas-gauche  (texel 0,0 rouge)"),
                ((48, 48), texels[1], "bas-droite  (texel 1,0 vert)"),
                ((16, 16), texels[2], "haut-gauche (texel 0,1 bleu)"),
                ((48, 16), texels[3], "haut-droite (texel 1,1 blanc)"),
            ];
            let mut ok = true;
            for ((x, y), want, what) in checks {
                let got = px(x, y);
                let close = got.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 1);
                println!(
                    "[rdna2_native] pixel ({x},{y}) {what} = {got:?}{}",
                    if close { "" } else { "  <-- ATTENDU " }
                );
                ok &= close;
            }
            if !ok {
                println!("[rdna2_native] ECHEC triangle texturé");
                return false;
            }

            // 2e passe : teinte (0.5, 0.25, 1, 1) -> constantes racine = user-SGPR du PS.
            let cl2 = ctx.cmd_list();
            let set_pso: extern "win64" fn(*mut c_void, *mut c_void) = vtbl_fn(cl2, 25);
            let set_rs: extern "win64" fn(*mut c_void, *mut c_void) = vtbl_fn(cl2, 30);
            let set_table: extern "win64" fn(*mut c_void, u32, u64) = vtbl_fn(cl2, 32);
            let set_consts: extern "win64" fn(*mut c_void, u32, u32, *const u32, u32) =
                vtbl_fn(cl2, 36);
            let clear: extern "win64" fn(*mut c_void, usize, *const [f32; 4], u32, *const c_void) =
                vtbl_fn(cl2, 48);
            let draw: extern "win64" fn(*mut c_void, u32, u32, u32, u32) = vtbl_fn(cl2, 12);
            let copy: extern "win64" fn(*mut c_void, *mut c_void, *mut c_void) = vtbl_fn(cl2, 17);
            clear(cl2, 0, &[0.0, 0.0, 0.0, 1.0], 0, ptr::null());
            set_pso(cl2, pso);
            set_rs(cl2, rs);
            set_table(cl2, 0, gpu);
            set_table(cl2, 1, sgpu);
            let tint = [0.5f32, 0.25, 1.0, 1.0].map(f32::to_bits);
            set_consts(cl2, 2, 4, tint.as_ptr(), 0);
            draw(cl2, 3, 1, 0, 0);
            copy(cl2, rb, backbuffer);
            ctx.execute_and_wait(cl2);
            let pixels = read_resource(rb, W * W * 4);
            let i = (48 * W + 16) * 4; // bas-gauche : rouge x teinte = (128, 0, 0)
            let j = (16 * W + 48) * 4; // haut-droite : blanc x teinte = (128, 64, 255)
            let (a, b) = (&pixels[i..i + 4], &pixels[j..j + 4]);
            let want_a = [128u8, 0, 0, 255];
            let want_b = [128u8, 64, 255, 255];
            let near = |g: &[u8], w: [u8; 4]| g.iter().zip(w).all(|(x, y)| x.abs_diff(y) <= 1);
            println!("[rdna2_native] teinte racine : bas-gauche = {a:?}, haut-droite = {b:?}");
            if !(near(a, want_a) && near(b, want_b)) {
                println!("[rdna2_native] ECHEC teinte par constantes racine");
                return false;
            }
            println!(
                "[rdna2_native] Triangle texturé RDNA 2 : pixels vérifiés (V#/T#/S# lus dans les tas, constantes racine)"
            );
            true
        }
    }

    pub fn run() -> i32 {
        println!(
            "[rdna2_native] === Harnais PE32+ programmes GPU natifs RDNA 2 (SBR2) -> Vulkan ==="
        );
        // SAFETY: appels COM `extern "win64"` résolus nativement par ScarletBridge (`sb-hle`).
        unsafe {
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
                width: 64,
                height: 64,
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
            let get_buf: extern "win64" fn(*mut c_void, u32, *const Guid, *mut *mut c_void) -> i32 =
                vtbl_fn(sc, 9);
            let mut bb: *mut c_void = ptr::null_mut();
            assert_eq!(get_buf(sc, 0, &DUMMY_IID, &mut bb), 0);

            let ctx = Ctx { dev, queue };
            let compute_ok = test_compute(&ctx);
            let storage_ok = test_storage_image(&ctx);
            let gfx_ok = test_graphics(&ctx, bb);
            if compute_ok && storage_ok && gfx_ok {
                println!("[rdna2_native] === PROGRAMMES GPU RDNA 2 NATIFS (VS+PS+CS) VÉRIFIÉS ===");
                0
            } else {
                println!(
                    "[rdna2_native] === ECHEC (compute={compute_ok}, image de stockage={storage_ok}, graphique={gfx_ok}) ==="
                );
                1
            }
        }
    }
}
