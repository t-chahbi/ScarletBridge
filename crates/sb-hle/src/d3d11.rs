//! Implémentation HLE COM de Direct3D 11 + DXGI (`ID3D11Device`, `ID3D11DeviceContext`,
//! `IDXGIDevice`, `IDXGIAdapter`, `IDXGIFactory2`, `IDXGISwapChain1`, ressources & shaders)
//! branchée sur notre backend **Vulkan 1.4 (`sb-gpu`) + fenêtre SDL3 Wayland**.

#![allow(non_snake_case, unused_variables)]

use core::ffi::c_void;
use std::collections::HashMap;
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;
use sb_common::abi::{E_POINTER, E_UNEXPECTED, HResult, S_OK};
use sb_common::{ffi_guard, sb_debug, sb_info, sb_trace, sb_warn};
use sb_gpu::{GpuContext, RenderCommand, ShaderContainer, SwapchainPresenter, VulkanRenderer};

use crate::com::{
    ComHeader, Guid, IID_ID3D11TEXTURE2D, IID_IDXGIADAPTER, IID_IDXGIDEVICE, IID_IDXGIDEVICE1,
    IID_IDXGIDEVICE2, IID_IDXGIDEVICE3, IID_IDXGIFACTORY2, VtblSlot, com_add_ref,
    com_query_interface_list, com_release, vslot,
};
use crate::winrt_objects;

const CHANNEL: &str = "d3d11";
const D3D_FEATURE_LEVEL_11_1: u32 = 0xB100;

static PRESENT_FRAMES: AtomicU64 = AtomicU64::new(0);
static LAST_CLEAR_RGBA: Mutex<[f32; 4]> = Mutex::new([0.08, 0.05, 0.14, 1.0]);
static PRESENT_COUNT_LIMIT: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();

pub fn max_frames_limit() -> Option<u64> {
    *PRESENT_COUNT_LIMIT.get_or_init(|| {
        std::env::var("SB_MAX_FRAMES")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
    })
}

pub fn presented_frames() -> u64 {
    PRESENT_FRAMES.load(Ordering::Acquire)
}

#[repr(C)]
struct D3D11BufferDesc {
    byte_width: u32,
    usage: u32,
    bind_flags: u32,
    cpu_access_flags: u32,
    misc_flags: u32,
    structure_byte_stride: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct D3D11Texture2DDesc {
    width: u32,
    height: u32,
    mip_levels: u32,
    array_size: u32,
    format: u32,
    sample_count: u32,
    sample_quality: u32,
    usage: u32,
    bind_flags: u32,
    cpu_access_flags: u32,
    misc_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct D3D11SubresourceData {
    sys_mem: *const u8,
    sys_mem_pitch: u32,
    sys_mem_slice_pitch: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct D3D11MappedSubresource {
    data: *mut u8,
    row_pitch: u32,
    depth_pitch: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DxgiSwapChainDesc1 {
    width: u32,
    height: u32,
    format: u32,
    stereo: i32,
    sample_count: u32,
    sample_quality: u32,
    buffer_usage: u32,
    buffer_count: u32,
    scaling: u32,
    swap_effect: u32,
    alpha_mode: u32,
    flags: u32,
}

// ==================== Objet Ressource Générique D3D11 (Buffer / Texture / Shader / State) ====================

#[repr(C)]
struct GenericD3D11Object {
    header: ComHeader,
    backing: Mutex<Vec<u8>>,
    row_pitch: u32,
    desc_tex2d: D3D11Texture2DDesc,
    shader_dxbc: Mutex<Option<Vec<u8>>>,
    parent_resource: usize,
    gpu_texture_id: AtomicUsize,
}

#[repr(C)]
struct GenericD3D11Vtbl {
    slots: [VtblSlot; 32],
}

extern "win64" fn generic_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("ID3D11Object", this, riid, ppv, &[IID_ID3D11TEXTURE2D])
}

extern "win64" fn generic_get_desc_tex2d(
    this: *mut GenericD3D11Object,
    out_desc: *mut D3D11Texture2DDesc,
) {
    if !this.is_null() && !out_desc.is_null() {
        // SAFETY: `this` et `out_desc` vérifiés non-nuls.
        unsafe {
            *out_desc = (*this).desc_tex2d;
        }
    }
}

extern "win64" fn generic_noop() -> HResult {
    S_OK
}

const fn build_generic_vtbl() -> GenericD3D11Vtbl {
    let mut slots = [vslot(generic_noop as *const c_void); 32];
    slots[0] = vslot(generic_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[10] = vslot(generic_get_desc_tex2d as *const c_void);
    GenericD3D11Vtbl { slots }
}

static GENERIC_D3D11_VTBL: GenericD3D11Vtbl = build_generic_vtbl();

fn alloc_generic_obj(
    backing_size: usize,
    row_pitch: u32,
    tex_desc: Option<D3D11Texture2DDesc>,
) -> *mut c_void {
    let default_tex = D3D11Texture2DDesc {
        width: 1280,
        height: 720,
        mip_levels: 1,
        array_size: 1,
        format: 28, // DXGI_FORMAT_R8G8B8A8_UNORM
        sample_count: 1,
        sample_quality: 0,
        usage: 0,
        bind_flags: 0x20,
        cpu_access_flags: 0,
        misc_flags: 0,
    };
    let obj = Box::new(GenericD3D11Object {
        header: ComHeader::new((&raw const GENERIC_D3D11_VTBL).cast()),
        backing: Mutex::new(vec![0u8; backing_size.max(64)]),
        row_pitch: row_pitch.max(4),
        desc_tex2d: tex_desc.unwrap_or(default_tex),
        shader_dxbc: Mutex::new(None),
        parent_resource: 0,
        gpu_texture_id: AtomicUsize::new(usize::MAX),
    });
    Box::into_raw(obj).cast()
}

// ==================== ID3D11Device (43 slots exacts `d3d11.h`) ====================

#[repr(C)]
struct D3D11DeviceVtbl {
    slots: [VtblSlot; 43],
}

#[repr(C)]
struct D3D11DeviceObject {
    header: ComHeader,
    gpu: Option<Arc<GpuContext>>,
    immediate_ctx: *mut c_void,
    dxgi_device: *mut c_void,
}

static D3D11_DEVICE_VTBL: D3D11DeviceVtbl = D3D11DeviceVtbl {
    slots: [
        vslot(d3d11_dev_qi as *const c_void),        // 0: QueryInterface
        vslot(com_add_ref as *const c_void),         // 1: AddRef
        vslot(com_release as *const c_void),         // 2: Release
        vslot(d3d11_create_buffer as *const c_void), // 3: CreateBuffer
        vslot(d3d11_create_tex1d as *const c_void),  // 4: CreateTexture1D
        vslot(d3d11_create_tex2d as *const c_void),  // 5: CreateTexture2D
        vslot(d3d11_create_tex3d as *const c_void),  // 6: CreateTexture3D
        vslot(d3d11_create_out_3 as *const c_void),  // 7: CreateShaderResourceView
        vslot(d3d11_create_out_3 as *const c_void),  // 8: CreateUnorderedAccessView
        vslot(d3d11_create_out_3 as *const c_void),  // 9: CreateRenderTargetView
        vslot(d3d11_create_out_3 as *const c_void),  // 10: CreateDepthStencilView
        vslot(d3d11_create_input_layout as *const c_void), // 11: CreateInputLayout
        vslot(d3d11_create_vertex_shader as *const c_void), // 12: CreateVertexShader
        vslot(d3d11_create_shader_other as *const c_void), // 13: CreateGeometryShader
        vslot(d3d11_create_gs_so as *const c_void),  // 14: CreateGeometryShaderWithStreamOutput
        vslot(d3d11_create_pixel_shader as *const c_void), // 15: CreatePixelShader
        vslot(d3d11_create_shader_other as *const c_void), // 16: CreateHullShader
        vslot(d3d11_create_shader_other as *const c_void), // 17: CreateDomainShader
        vslot(d3d11_create_shader_other as *const c_void), // 18: CreateComputeShader
        vslot(d3d11_create_out_1 as *const c_void),  // 19: CreateClassLinkage
        vslot(d3d11_create_out_2 as *const c_void),  // 20: CreateBlendState
        vslot(d3d11_create_out_2 as *const c_void),  // 21: CreateDepthStencilState
        vslot(d3d11_create_out_2 as *const c_void),  // 22: CreateRasterizerState
        vslot(d3d11_create_out_2 as *const c_void),  // 23: CreateSamplerState
        vslot(d3d11_create_out_2 as *const c_void),  // 24: CreateQuery
        vslot(d3d11_create_out_2 as *const c_void),  // 25: CreatePredicate
        vslot(d3d11_create_out_2 as *const c_void),  // 26: CreateCounter
        vslot(d3d11_create_out_2 as *const c_void),  // 27: CreateDeferredContext
        vslot(d3d11_create_out_3 as *const c_void),  // 28: OpenSharedResource
        vslot(d3d11_check_format_support as *const c_void), // 29: CheckFormatSupport
        vslot(d3d11_check_msaa as *const c_void),    // 30: CheckMultisampleQualityLevels
        vslot(generic_noop as *const c_void),        // 31: CheckCounterInfo
        vslot(generic_noop as *const c_void),        // 32: CheckCounter
        vslot(d3d11_check_feature_support as *const c_void), // 33: CheckFeatureSupport
        vslot(generic_noop as *const c_void),        // 34: GetPrivateData
        vslot(generic_noop as *const c_void),        // 35: SetPrivateData
        vslot(generic_noop as *const c_void),        // 36: SetPrivateDataInterface
        vslot(d3d11_get_feature_level as *const c_void), // 37: GetFeatureLevel
        vslot(d3d11_get_zero_u32 as *const c_void),  // 38: GetCreationFlags
        vslot(generic_noop as *const c_void),        // 39: GetDeviceRemovedReason
        vslot(d3d11_get_immediate_ctx as *const c_void), // 40: GetImmediateContext
        vslot(generic_noop as *const c_void),        // 41: SetExceptionMode
        vslot(d3d11_get_zero_u32 as *const c_void),  // 42: GetExceptionMode
    ],
};

extern "win64" fn d3d11_dev_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    if ppv.is_null() || this.is_null() || riid.is_null() {
        return E_POINTER;
    }
    // SAFETY: `riid` et `this` non-nuls.
    let wanted = unsafe { *riid };
    if wanted == IID_IDXGIDEVICE
        || wanted == IID_IDXGIDEVICE1
        || wanted == IID_IDXGIDEVICE2
        || wanted == IID_IDXGIDEVICE3
    {
        // SAFETY: `this` est un `D3D11DeviceObject`.
        let dev = unsafe { &*(this.cast::<D3D11DeviceObject>()) };
        unsafe {
            *ppv = dev.dxgi_device;
        }
        return S_OK;
    }
    com_query_interface_list("ID3D11Device", this, riid, ppv, &[])
}

extern "win64" fn d3d11_create_buffer(
    _this: *mut D3D11DeviceObject,
    desc: *const D3D11BufferDesc,
    init: *const D3D11SubresourceData,
    out_buf: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CreateBuffer", E_UNEXPECTED, {
        let size = if !desc.is_null() {
            // SAFETY: `desc` non-nul.
            unsafe { (*desc).byte_width as usize }
        } else {
            256
        };
        let ptr_obj = alloc_generic_obj(size, size as u32, None);
        if !init.is_null() {
            // SAFETY: copie initiale si `pSysMem` est fourni.
            let sub = unsafe { *init };
            if !sub.sys_mem.is_null() && size > 0 {
                let obj = unsafe { &*(ptr_obj.cast::<GenericD3D11Object>()) };
                let mut guard = obj.backing.lock();
                let copy_len = size.min(guard.len());
                unsafe {
                    ptr::copy_nonoverlapping(sub.sys_mem, guard.as_mut_ptr(), copy_len);
                }
            }
        }
        if !out_buf.is_null() {
            // SAFETY: `out_buf` non-nul.
            unsafe {
                *out_buf = ptr_obj;
            }
        }
        S_OK
    })
}

extern "win64" fn d3d11_create_tex1d(
    _this: *mut D3D11DeviceObject,
    _desc: *const c_void,
    _init: *const c_void,
    out_tex: *mut *mut c_void,
) -> HResult {
    if !out_tex.is_null() {
        // SAFETY: `out_tex` non-nul.
        unsafe {
            *out_tex = alloc_generic_obj(4096, 256, None);
        }
    }
    S_OK
}

extern "win64" fn d3d11_create_tex2d(
    _this: *mut D3D11DeviceObject,
    desc: *const D3D11Texture2DDesc,
    _init: *const c_void,
    out_tex: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CreateTexture2D", E_UNEXPECTED, {
        let d = if !desc.is_null() {
            // SAFETY: `desc` non-nul.
            unsafe { *desc }
        } else {
            D3D11Texture2DDesc {
                width: 256,
                height: 256,
                mip_levels: 1,
                array_size: 1,
                format: 28,
                sample_count: 1,
                sample_quality: 0,
                usage: 0,
                bind_flags: 0,
                cpu_access_flags: 0,
                misc_flags: 0,
            }
        };
        let row_pitch = d.width.max(1).saturating_mul(4);
        let bytes = (row_pitch as usize)
            .saturating_mul(d.height.max(1) as usize)
            .min(64 * 1024 * 1024);
        sb_trace!(
            CHANNEL,
            "ID3D11Device::CreateTexture2D({}x{}, fmt={}, mips={})",
            d.width,
            d.height,
            d.format,
            d.mip_levels
        );
        if !out_tex.is_null() {
            // SAFETY: `out_tex` non-nul.
            unsafe {
                *out_tex = alloc_generic_obj(bytes, row_pitch, Some(d));
            }
        }
        S_OK
    })
}

extern "win64" fn d3d11_create_tex3d(
    _this: *mut D3D11DeviceObject,
    _desc: *const c_void,
    _init: *const c_void,
    out_tex: *mut *mut c_void,
) -> HResult {
    if !out_tex.is_null() {
        // SAFETY: `out_tex` non-nul.
        unsafe {
            *out_tex = alloc_generic_obj(4096, 256, None);
        }
    }
    S_OK
}

extern "win64" fn d3d11_create_out_1(
    _this: *mut D3D11DeviceObject,
    out: *mut *mut c_void,
) -> HResult {
    if !out.is_null() {
        // SAFETY: `out` non-nul.
        unsafe {
            *out = alloc_generic_obj(64, 64, None);
        }
    }
    S_OK
}

extern "win64" fn d3d11_create_out_2(
    _this: *mut D3D11DeviceObject,
    _a: *const c_void,
    out: *mut *mut c_void,
) -> HResult {
    if !out.is_null() {
        // SAFETY: `out` non-nul.
        unsafe {
            *out = alloc_generic_obj(64, 64, None);
        }
    }
    S_OK
}

extern "win64" fn d3d11_create_out_3(
    _this: *mut D3D11DeviceObject,
    a: *const c_void,
    _b: *const c_void,
    out: *mut *mut c_void,
) -> HResult {
    if !out.is_null() {
        let ptr_obj = alloc_generic_obj(64, 64, None);
        if !a.is_null() {
            // SAFETY: conserve le pointeur vers la ressource parente (ex. SRV -> Texture2D).
            unsafe {
                (*(ptr_obj.cast::<GenericD3D11Object>())).parent_resource = a as usize;
            }
        }
        // SAFETY: `out` non-nul.
        unsafe {
            *out = ptr_obj;
        }
    }
    S_OK
}

extern "win64" fn d3d11_create_input_layout(
    _this: *mut D3D11DeviceObject,
    _descs: *const c_void,
    _num: u32,
    _bytecode: *const u8,
    _bytecode_len: usize,
    out_layout: *mut *mut c_void,
) -> HResult {
    if !out_layout.is_null() {
        // SAFETY: `out_layout` non-nul.
        unsafe {
            *out_layout = alloc_generic_obj(64, 64, None);
        }
    }
    S_OK
}

fn inspect_and_create_shader(
    kind: &str,
    bytecode: *const u8,
    len: usize,
    out: *mut *mut c_void,
) -> HResult {
    let mut stored_dxbc = None;
    if !bytecode.is_null() && len >= 32 {
        // SAFETY: `bytecode` pointe vers `len` octets DXBC/DXIL.
        let slice = unsafe { std::slice::from_raw_parts(bytecode, len) };
        stored_dxbc = Some(slice.to_vec());
        if let Some(info) = ShaderContainer::parse(slice) {
            let spv_words = sb_gpu::translate_dxbc_to_spirv(slice)
                .map(|s| s.spirv.len())
                .unwrap_or(0);
            sb_debug!(
                CHANNEL,
                "ID3D11Device::{kind} ({:?} {:?} SM {}.{}, {} octets, spirv={} mots)",
                info.format,
                info.stage,
                info.major_version,
                info.minor_version,
                len,
                spv_words
            );
        }
    }
    if !out.is_null() {
        let obj = alloc_generic_obj(64, 64, None);
        if let Some(bc) = stored_dxbc {
            // SAFETY: `obj` provient de `alloc_generic_obj`.
            unsafe {
                *(*(obj.cast::<GenericD3D11Object>())).shader_dxbc.lock() = Some(bc);
            }
        }
        // SAFETY: `out` non-nul.
        unsafe {
            *out = obj;
        }
    }
    S_OK
}

extern "win64" fn d3d11_create_vertex_shader(
    _this: *mut D3D11DeviceObject,
    bytecode: *const u8,
    len: usize,
    _linkage: *mut c_void,
    out: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CreateVertexShader", E_UNEXPECTED, {
        inspect_and_create_shader("CreateVertexShader", bytecode, len, out)
    })
}

extern "win64" fn d3d11_create_pixel_shader(
    _this: *mut D3D11DeviceObject,
    bytecode: *const u8,
    len: usize,
    _linkage: *mut c_void,
    out: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CreatePixelShader", E_UNEXPECTED, {
        inspect_and_create_shader("CreatePixelShader", bytecode, len, out)
    })
}

extern "win64" fn d3d11_create_shader_other(
    _this: *mut D3D11DeviceObject,
    bytecode: *const u8,
    len: usize,
    _linkage: *mut c_void,
    out: *mut *mut c_void,
) -> HResult {
    inspect_and_create_shader("CreateShader", bytecode, len, out)
}

#[allow(clippy::too_many_arguments)]
extern "win64" fn d3d11_create_gs_so(
    _this: *mut D3D11DeviceObject,
    _bc: *const u8,
    _len: usize,
    _so: *const c_void,
    _num_entries: u32,
    _strides: *const u32,
    _num_strides: u32,
    _raster_stream: u32,
    _linkage: *mut c_void,
    out: *mut *mut c_void,
) -> HResult {
    if !out.is_null() {
        // SAFETY: `out` non-nul.
        unsafe {
            *out = alloc_generic_obj(64, 64, None);
        }
    }
    S_OK
}

extern "win64" fn d3d11_check_format_support(
    _this: *mut D3D11DeviceObject,
    _fmt: u32,
    out_support: *mut u32,
) -> HResult {
    if !out_support.is_null() {
        // SAFETY: `out_support` non-nul (signale support large).
        unsafe {
            *out_support = 0xFFFF_FFFF;
        }
    }
    S_OK
}

extern "win64" fn d3d11_check_msaa(
    _this: *mut D3D11DeviceObject,
    _fmt: u32,
    _samples: u32,
    out_levels: *mut u32,
) -> HResult {
    if !out_levels.is_null() {
        // SAFETY: `out_levels` non-nul.
        unsafe {
            *out_levels = 1;
        }
    }
    S_OK
}

extern "win64" fn d3d11_check_feature_support(
    _this: *mut D3D11DeviceObject,
    _feature: u32,
    data: *mut u8,
    size: u32,
) -> HResult {
    if !data.is_null() && size > 0 {
        // SAFETY: zéro-initialisation propre de la structure de sortie.
        unsafe {
            ptr::write_bytes(data, 0, size as usize);
        }
    }
    S_OK
}

extern "win64" fn d3d11_get_feature_level(_this: *mut D3D11DeviceObject) -> u32 {
    D3D_FEATURE_LEVEL_11_1
}

extern "win64" fn d3d11_get_zero_u32(_this: *mut D3D11DeviceObject) -> u32 {
    0
}

extern "win64" fn d3d11_get_immediate_ctx(this: *mut D3D11DeviceObject, out_ctx: *mut *mut c_void) {
    if !this.is_null() && !out_ctx.is_null() {
        // SAFETY: `this` et `out_ctx` non-nuls.
        unsafe {
            *out_ctx = (*this).immediate_ctx;
        }
    }
}

// ==================== ID3D11DeviceContext (115 slots exacts `d3d11.h`) ====================

#[repr(C)]
struct D3D11ContextVtbl {
    slots: [VtblSlot; 115],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct D3D11Viewport {
    top_left_x: f32,
    top_left_y: f32,
    width: f32,
    height: f32,
    min_depth: f32,
    max_depth: f32,
}

#[repr(C)]
struct D3D11ContextObject {
    header: ComHeader,
    draw_calls: AtomicU64,
    renderer: Option<Arc<Mutex<VulkanRenderer>>>,
    bound_vs: AtomicUsize,
    bound_ps: AtomicUsize,
    vb_stride: AtomicU32,
    pipeline_cache: Mutex<HashMap<(usize, usize), usize>>,
    pending_cmds: Mutex<Vec<RenderCommand>>,
}

static D3D11_ACTIVE_CTX: Mutex<usize> = Mutex::new(0);

fn d3d11_ensure_pipeline(ctx: &D3D11ContextObject) -> Option<usize> {
    let vs_ptr = ctx.bound_vs.load(Ordering::Acquire);
    let ps_ptr = ctx.bound_ps.load(Ordering::Acquire);
    if vs_ptr == 0 || ps_ptr == 0 {
        return None;
    }
    if let Some(&pid) = ctx.pipeline_cache.lock().get(&(vs_ptr, ps_ptr)) {
        return Some(pid);
    }
    let renderer_arc = ctx.renderer.as_ref()?;
    // SAFETY: `vs_ptr` et `ps_ptr` proviennent de `CreateVertexShader` / `CreatePixelShader`.
    let vs_bc = unsafe {
        (*(vs_ptr as *const GenericD3D11Object))
            .shader_dxbc
            .lock()
            .clone()
    }?;
    let ps_bc = unsafe {
        (*(ps_ptr as *const GenericD3D11Object))
            .shader_dxbc
            .lock()
            .clone()
    }?;
    let stride = ctx.vb_stride.load(Ordering::Acquire).max(32);
    let mut renderer = renderer_arc.lock();
    match renderer.create_pipeline_from_dxbc(&vs_bc, &ps_bc, stride) {
        Ok(pid) => {
            ctx.pipeline_cache.lock().insert((vs_ptr, ps_ptr), pid);
            Some(pid)
        }
        Err(err) => {
            sb_debug!(CHANNEL, "D3D11 lazy VkPipeline ignoré: {err}");
            None
        }
    }
}

extern "win64" fn ctx_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("ID3D11DeviceContext", this, riid, ppv, &[])
}

extern "win64" fn ctx_noop() -> HResult {
    S_OK
}

extern "win64" fn ctx_set_cbuffers(
    this: *mut D3D11ContextObject,
    start_slot: u32,
    num_buffers: u32,
    buffers: *const *mut GenericD3D11Object,
) {
    if this.is_null() || buffers.is_null() || num_buffers == 0 {
        return;
    }
    // SAFETY: `this` et `buffers[..num_buffers]` valides.
    let ctx = unsafe { &*this };
    let ptrs = unsafe { std::slice::from_raw_parts(buffers, num_buffers as usize) };
    let mut cmds = ctx.pending_cmds.lock();
    for (i, &buf_ptr) in ptrs.iter().enumerate() {
        if buf_ptr.is_null() {
            continue;
        }
        // SAFETY: `buf_ptr` est un `GenericD3D11Object` valide.
        let data = unsafe { (*buf_ptr).backing.lock().clone() };
        cmds.push(RenderCommand::UpdateConstantBuffer {
            slot: (start_slot + i as u32) % 4,
            data,
        });
    }
}

extern "win64" fn ctx_ps_set_srvs(
    this: *mut D3D11ContextObject,
    start_slot: u32,
    num_views: u32,
    views: *const *mut GenericD3D11Object,
) {
    if this.is_null() || views.is_null() || num_views == 0 {
        return;
    }
    // SAFETY: `this` et `views[..num_views]` valides.
    let ctx = unsafe { &*this };
    let Some(ref renderer_arc) = ctx.renderer else {
        return;
    };
    let ptrs = unsafe { std::slice::from_raw_parts(views, num_views as usize) };
    let mut cmds = ctx.pending_cmds.lock();
    for (i, &srv_ptr) in ptrs.iter().enumerate() {
        if srv_ptr.is_null() {
            continue;
        }
        // SAFETY: `srv_ptr` provient de `CreateShaderResourceView`.
        let parent = unsafe { (*srv_ptr).parent_resource };
        if parent == 0 {
            continue;
        }
        let tex_obj = unsafe { &*(parent as *const GenericD3D11Object) };
        let mut tid = tex_obj.gpu_texture_id.load(Ordering::Acquire);
        if tid == usize::MAX {
            let data = tex_obj.backing.lock().clone();
            let w = tex_obj.desc_tex2d.width.max(1);
            let h = tex_obj.desc_tex2d.height.max(1);
            if let Ok(new_id) = renderer_arc.lock().create_texture_rgba8(w, h, &data) {
                tex_obj.gpu_texture_id.store(new_id, Ordering::Release);
                tid = new_id;
            }
        }
        if tid != usize::MAX {
            cmds.push(RenderCommand::BindTexture {
                slot: (start_slot + i as u32) % 4,
                texture_id: tid,
            });
        }
    }
}

extern "win64" fn ctx_ps_set_shader(
    this: *mut D3D11ContextObject,
    shader: *mut c_void,
    _instances: *const *mut c_void,
    _num_instances: u32,
) {
    if !this.is_null() {
        // SAFETY: `this` non-nul.
        unsafe {
            (*this).bound_ps.store(shader as usize, Ordering::Release);
        }
    }
}

extern "win64" fn ctx_vs_set_shader(
    this: *mut D3D11ContextObject,
    shader: *mut c_void,
    _instances: *const *mut c_void,
    _num_instances: u32,
) {
    if !this.is_null() {
        // SAFETY: `this` non-nul.
        unsafe {
            (*this).bound_vs.store(shader as usize, Ordering::Release);
        }
    }
}

extern "win64" fn ctx_draw(this: *mut D3D11ContextObject, vcount: u32, start: u32) {
    if this.is_null() {
        return;
    }
    // SAFETY: `this` non-nul.
    let ctx = unsafe { &*this };
    ctx.draw_calls.fetch_add(1, Ordering::Relaxed);
    if let Some(pid) = d3d11_ensure_pipeline(ctx) {
        let mut cmds = ctx.pending_cmds.lock();
        cmds.push(RenderCommand::SetPipeline { pipeline_id: pid });
        cmds.push(RenderCommand::Draw {
            vertex_count: vcount,
            instance_count: 1,
            first_vertex: start,
            first_instance: 0,
        });
    }
}

extern "win64" fn ctx_draw_indexed(
    this: *mut D3D11ContextObject,
    icount: u32,
    start: u32,
    base: i32,
) {
    if this.is_null() {
        return;
    }
    // SAFETY: `this` non-nul.
    let ctx = unsafe { &*this };
    ctx.draw_calls.fetch_add(1, Ordering::Relaxed);
    if let Some(pid) = d3d11_ensure_pipeline(ctx) {
        let mut cmds = ctx.pending_cmds.lock();
        cmds.push(RenderCommand::SetPipeline { pipeline_id: pid });
        cmds.push(RenderCommand::DrawIndexed {
            index_count: icount,
            instance_count: 1,
            first_index: start,
            vertex_offset: base,
            first_instance: 0,
        });
    }
}

extern "win64" fn ctx_ia_set_vertex_buffers(
    this: *mut D3D11ContextObject,
    _start_slot: u32,
    num_buffers: u32,
    buffers: *const *mut GenericD3D11Object,
    strides: *const u32,
    offsets: *const u32,
) {
    if this.is_null() || num_buffers == 0 || buffers.is_null() {
        return;
    }
    // SAFETY: `this` et `buffers[0]` valides.
    let ctx = unsafe { &*this };
    let buf_ptr = unsafe { *buffers };
    if buf_ptr.is_null() {
        return;
    }
    let stride = if !strides.is_null() {
        // SAFETY: `strides` non-nul.
        unsafe { *strides }
    } else {
        32
    };
    let offset = if !offsets.is_null() {
        // SAFETY: `offsets` non-nul.
        unsafe { *offsets as usize }
    } else {
        0
    };
    ctx.vb_stride.store(stride, Ordering::Release);
    // SAFETY: `buf_ptr` est un `GenericD3D11Object` valide.
    let raw = unsafe { (*buf_ptr).backing.lock() };
    if offset < raw.len() {
        let data = raw[offset..].to_vec();
        ctx.pending_cmds
            .lock()
            .push(RenderCommand::SetVertexBuffer { data, stride });
    }
}

extern "win64" fn ctx_ia_set_index_buffer(
    this: *mut D3D11ContextObject,
    buffer: *mut GenericD3D11Object,
    format: u32,
    offset: u32,
) {
    if this.is_null() || buffer.is_null() {
        return;
    }
    // SAFETY: `this` et `buffer` non-nuls.
    let ctx = unsafe { &*this };
    let raw = unsafe { (*buffer).backing.lock() };
    let off = offset as usize;
    if off < raw.len() {
        ctx.pending_cmds.lock().push(RenderCommand::SetIndexBuffer {
            data: raw[off..].to_vec(),
            index32: format == 42,
        });
    }
}

extern "win64" fn ctx_rs_set_viewports(
    this: *mut D3D11ContextObject,
    num_viewports: u32,
    viewports: *const D3D11Viewport,
) {
    if this.is_null() || num_viewports == 0 || viewports.is_null() {
        return;
    }
    // SAFETY: `this` et `viewports` valides.
    let ctx = unsafe { &*this };
    let vp = unsafe { *viewports };
    ctx.pending_cmds.lock().push(RenderCommand::SetViewport {
        x: vp.top_left_x,
        y: vp.top_left_y,
        width: vp.width,
        height: vp.height,
    });
}

extern "win64" fn ctx_update_subresource(
    _this: *mut D3D11ContextObject,
    dst_resource: *mut GenericD3D11Object,
    _dst_subresource: u32,
    _dst_box: *const c_void,
    src_data: *const u8,
    src_row_pitch: u32,
    _src_depth_pitch: u32,
) {
    if dst_resource.is_null() || src_data.is_null() {
        return;
    }
    // SAFETY: `dst_resource` et `src_data` non-nuls.
    let res = unsafe { &*dst_resource };
    let mut dst = res.backing.lock();
    let len = if src_row_pitch > 0 {
        (src_row_pitch as usize).min(dst.len())
    } else {
        dst.len()
    };
    if len > 0 {
        // SAFETY: `src_data` contient au moins `len` octets.
        let src = unsafe { std::slice::from_raw_parts(src_data, len) };
        dst[..len].copy_from_slice(src);
    }
    res.gpu_texture_id.store(usize::MAX, Ordering::Release);
}

extern "win64" fn ctx_map(
    _this: *mut D3D11ContextObject,
    resource: *mut GenericD3D11Object,
    _subresource: u32,
    _map_type: u32,
    _map_flags: u32,
    out_mapped: *mut D3D11MappedSubresource,
) -> HResult {
    ffi_guard!(CHANNEL, "Map", E_UNEXPECTED, {
        if resource.is_null() || out_mapped.is_null() {
            return E_POINTER;
        }
        // SAFETY: `resource` provient de nos `CreateBuffer` / `CreateTexture2D`.
        let res = unsafe { &*resource };
        let mut buf = res.backing.lock();
        let data_ptr = buf.as_mut_ptr();
        let depth_pitch = buf.len() as u32;
        drop(buf);
        // SAFETY: `out_mapped` non-nul.
        unsafe {
            *out_mapped = D3D11MappedSubresource {
                data: data_ptr,
                row_pitch: res.row_pitch,
                depth_pitch,
            };
        }
        S_OK
    })
}

extern "win64" fn ctx_unmap(
    _this: *mut D3D11ContextObject,
    _resource: *mut GenericD3D11Object,
    _subresource: u32,
) {
}

extern "win64" fn ctx_clear_rtv(
    this: *mut D3D11ContextObject,
    _rtv: *mut c_void,
    color: *const [f32; 4],
) {
    if !color.is_null() {
        // SAFETY: `color` pointe vers 4 floats RGBA.
        let rgba = unsafe { *color };
        *LAST_CLEAR_RGBA.lock() = rgba;
        if !this.is_null() {
            // SAFETY: `this` non-nul.
            unsafe { &*this }
                .pending_cmds
                .lock()
                .push(RenderCommand::Clear { rgba });
        }
    }
}

extern "win64" fn ctx_clear_dsv(
    this: *mut D3D11ContextObject,
    _dsv: *mut c_void,
    _clear_flags: u32,
    depth: f32,
    _stencil: u8,
) {
    if !this.is_null() {
        // SAFETY: `this` non-nul.
        unsafe { &*this }
            .pending_cmds
            .lock()
            .push(RenderCommand::ClearDepth { depth });
    }
}

fn build_context_vtbl() -> D3D11ContextVtbl {
    let mut slots = [vslot(ctx_noop as *const c_void); 115];
    slots[0] = vslot(ctx_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[7] = vslot(ctx_set_cbuffers as *const c_void);
    slots[8] = vslot(ctx_ps_set_srvs as *const c_void);
    slots[9] = vslot(ctx_ps_set_shader as *const c_void);
    slots[11] = vslot(ctx_vs_set_shader as *const c_void);
    slots[12] = vslot(ctx_draw_indexed as *const c_void);
    slots[13] = vslot(ctx_draw as *const c_void);
    slots[14] = vslot(ctx_map as *const c_void);
    slots[15] = vslot(ctx_unmap as *const c_void);
    slots[16] = vslot(ctx_set_cbuffers as *const c_void);
    slots[18] = vslot(ctx_ia_set_vertex_buffers as *const c_void);
    slots[19] = vslot(ctx_ia_set_index_buffer as *const c_void);
    slots[44] = vslot(ctx_rs_set_viewports as *const c_void);
    slots[48] = vslot(ctx_update_subresource as *const c_void);
    slots[50] = vslot(ctx_clear_rtv as *const c_void);
    slots[53] = vslot(ctx_clear_dsv as *const c_void);
    D3D11ContextVtbl { slots }
}

static D3D11_CONTEXT_VTBL: std::sync::OnceLock<D3D11ContextVtbl> = std::sync::OnceLock::new();

// ==================== Chaîne DXGI : IDXGIDevice → IDXGIAdapter → IDXGIFactory2 → IDXGISwapChain1 ====================

#[repr(C)]
struct DxgiDeviceVtbl {
    slots: [VtblSlot; 20],
}

#[repr(C)]
struct DxgiDeviceObject {
    header: ComHeader,
    adapter: *mut c_void,
}

extern "win64" fn dxgi_dev_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list(
        "IDXGIDevice",
        this,
        riid,
        ppv,
        &[
            IID_IDXGIDEVICE,
            IID_IDXGIDEVICE1,
            IID_IDXGIDEVICE2,
            IID_IDXGIDEVICE3,
        ],
    )
}

extern "win64" fn dxgi_dev_get_adapter(
    this: *mut DxgiDeviceObject,
    out_adapter: *mut *mut c_void,
) -> HResult {
    if this.is_null() || out_adapter.is_null() {
        return E_POINTER;
    }
    // SAFETY: `this` et `out_adapter` non-nuls.
    unsafe {
        *out_adapter = (*this).adapter;
    }
    S_OK
}

const fn build_dxgi_device_vtbl() -> DxgiDeviceVtbl {
    let mut slots = [vslot(generic_noop as *const c_void); 20];
    slots[0] = vslot(dxgi_dev_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[6] = vslot(dxgi_dev_get_adapter as *const c_void); // GetParent
    slots[7] = vslot(dxgi_dev_get_adapter as *const c_void); // GetAdapter
    // 12 = SetMaximumFrameLatency, 13 = GetMaximumFrameLatency, 18 = Trim -> S_OK
    DxgiDeviceVtbl { slots }
}

static DXGI_DEVICE_VTBL: DxgiDeviceVtbl = build_dxgi_device_vtbl();

#[repr(C)]
struct DxgiAdapterVtbl {
    slots: [VtblSlot; 20],
}

#[repr(C)]
struct DxgiAdapterObject {
    header: ComHeader,
    factory: *mut c_void,
}

extern "win64" fn dxgi_adapter_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("IDXGIAdapter", this, riid, ppv, &[IID_IDXGIADAPTER])
}

extern "win64" fn dxgi_adapter_get_parent(
    this: *mut DxgiAdapterObject,
    _riid: *const Guid,
    out_parent: *mut *mut c_void,
) -> HResult {
    if this.is_null() || out_parent.is_null() {
        return E_POINTER;
    }
    // SAFETY: `this` et `out_parent` non-nuls.
    unsafe {
        *out_parent = (*this).factory;
    }
    S_OK
}

const fn build_dxgi_adapter_vtbl() -> DxgiAdapterVtbl {
    let mut slots = [vslot(generic_noop as *const c_void); 20];
    slots[0] = vslot(dxgi_adapter_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[6] = vslot(dxgi_adapter_get_parent as *const c_void); // GetParent -> IDXGIFactory2
    DxgiAdapterVtbl { slots }
}

static DXGI_ADAPTER_VTBL: DxgiAdapterVtbl = build_dxgi_adapter_vtbl();

#[repr(C)]
struct DxgiFactory2Vtbl {
    slots: [VtblSlot; 32],
}

#[repr(C)]
struct DxgiFactory2Object {
    header: ComHeader,
    gpu: Option<Arc<GpuContext>>,
}

extern "win64" fn dxgi_factory_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("IDXGIFactory2", this, riid, ppv, &[IID_IDXGIFACTORY2])
}

extern "win64" fn dxgi_create_swapchain_for_core_window(
    this: *mut DxgiFactory2Object,
    _device: *mut c_void,
    _window: *mut c_void,
    desc: *const DxgiSwapChainDesc1,
    _restrict_output: *mut c_void,
    out_swapchain: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CreateSwapChainForCoreWindow", E_UNEXPECTED, {
        if out_swapchain.is_null() {
            return E_POINTER;
        }
        let (w, h) = if !desc.is_null() {
            // SAFETY: `desc` non-nul.
            let d = unsafe { *desc };
            (d.width.max(1280), d.height.max(720))
        } else {
            (1280, 720)
        };

        let presenter = if !this.is_null() {
            // SAFETY: `this` non-nul.
            let gpu_opt = unsafe { (*this).gpu.clone() };
            if let Some(gpu) = gpu_opt {
                winrt_objects::create_vulkan_surface_for_window(gpu.instance.handle()).and_then(
                    |surf| match SwapchainPresenter::new(gpu, surf, w, h) {
                        Ok(p) => Some(p),
                        Err(err) => {
                            sb_warn!(CHANNEL, "SwapchainPresenter Vulkan indisponible : {err}");
                            None
                        }
                    },
                )
            } else {
                None
            }
        } else {
            None
        };

        sb_info!(
            CHANNEL,
            "IDXGIFactory2::CreateSwapChainForCoreWindow({w}x{h}, vulkan_swapchain={})",
            presenter.is_some()
        );

        let backbuffer = alloc_generic_obj(
            (w as usize) * (h as usize) * 4,
            w * 4,
            Some(D3D11Texture2DDesc {
                width: w,
                height: h,
                mip_levels: 1,
                array_size: 1,
                format: 28,
                sample_count: 1,
                sample_quality: 0,
                usage: 0,
                bind_flags: 0x20,
                cpu_access_flags: 0,
                misc_flags: 0,
            }),
        );

        let sc = Box::new(DxgiSwapChainObject {
            header: ComHeader::new((&raw const DXGI_SWAPCHAIN_VTBL).cast()),
            backbuffer,
            presenter: Mutex::new(presenter),
        });
        // SAFETY: `out_swapchain` vérifié non-nul.
        unsafe {
            *out_swapchain = Box::into_raw(sc).cast();
        }
        S_OK
    })
}

fn build_factory_vtbl() -> DxgiFactory2Vtbl {
    let mut slots = [vslot(generic_noop as *const c_void); 32];
    slots[0] = vslot(dxgi_factory_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[15] = vslot(dxgi_create_swapchain_for_core_window as *const c_void); // CreateSwapChainForHwnd
    slots[16] = vslot(dxgi_create_swapchain_for_core_window as *const c_void); // CreateSwapChainForCoreWindow
    DxgiFactory2Vtbl { slots }
}

static DXGI_FACTORY_VTBL: std::sync::OnceLock<DxgiFactory2Vtbl> = std::sync::OnceLock::new();

#[repr(C)]
struct DxgiSwapChainVtbl {
    slots: [VtblSlot; 42],
}

#[repr(C)]
struct DxgiSwapChainObject {
    header: ComHeader,
    backbuffer: *mut c_void,
    presenter: Mutex<Option<SwapchainPresenter>>,
}

extern "win64" fn sc_qi(this: *mut ComHeader, riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    com_query_interface_list("IDXGISwapChain1", this, riid, ppv, &[])
}

extern "win64" fn sc_present(
    this: *mut DxgiSwapChainObject,
    _sync_interval: u32,
    _flags: u32,
) -> HResult {
    ffi_guard!(CHANNEL, "SwapChain::Present", E_UNEXPECTED, {
        let frame = PRESENT_FRAMES.fetch_add(1, Ordering::AcqRel) + 1;
        let rgba = *LAST_CLEAR_RGBA.lock();

        // Exécute les commandes 3D D3D11 enregistrées sur `VulkanRenderer` si présentes.
        let ctx_addr = *D3D11_ACTIVE_CTX.lock();
        if ctx_addr != 0 {
            // SAFETY: `ctx_addr` est le `D3D11ContextObject` créé dans `D3D11CreateDevice`.
            let ctx = unsafe { &*(ctx_addr as *const D3D11ContextObject) };
            let cmds: Vec<RenderCommand> = ctx.pending_cmds.lock().drain(..).collect();
            if !cmds.is_empty()
                && let Some(ref ren_arc) = ctx.renderer
            {
                let mut renderer = ren_arc.lock();
                if let Err(err) = renderer.execute_commands(&cmds) {
                    sb_warn!(CHANNEL, "D3D11 -> VulkanRenderer frame #{frame}: {err}");
                } else if let Ok(path_str) = std::env::var("SB_DUMP_FRAME")
                    && !path_str.is_empty()
                {
                    let _ = renderer.save_ppm(&PathBuf::from(path_str));
                }
            }
        }

        if !this.is_null() {
            // SAFETY: `this` non-nul.
            let sc = unsafe { &*this };
            if let Some(ref mut pres) = *sc.presenter.lock()
                && let Err(err) = pres.present_clear(rgba)
            {
                sb_warn!(CHANNEL, "present_clear frame #{frame}: {err}");
            }
        }
        if frame == 1 || frame.is_multiple_of(60) {
            sb_info!(
                CHANNEL,
                "IDXGISwapChain1::Present() frame #{frame} (clear RGBA={rgba:?})"
            );
        }
        if let Some(limit) = max_frames_limit()
            && frame >= limit
        {
            sb_info!(
                CHANNEL,
                "limite SB_MAX_FRAMES={limit} atteinte — fermeture propre demandée"
            );
            winrt_objects::request_window_close();
        }
        S_OK
    })
}

extern "win64" fn sc_present1(
    this: *mut DxgiSwapChainObject,
    sync_interval: u32,
    flags: u32,
    _params: *const c_void,
) -> HResult {
    sc_present(this, sync_interval, flags)
}

extern "win64" fn sc_get_buffer(
    this: *mut DxgiSwapChainObject,
    _index: u32,
    _riid: *const Guid,
    out_surface: *mut *mut c_void,
) -> HResult {
    if this.is_null() || out_surface.is_null() {
        return E_POINTER;
    }
    // SAFETY: `this` et `out_surface` non-nuls.
    unsafe {
        *out_surface = (*this).backbuffer;
    }
    S_OK
}

const fn build_swapchain_vtbl() -> DxgiSwapChainVtbl {
    let mut slots = [vslot(generic_noop as *const c_void); 42];
    slots[0] = vslot(sc_qi as *const c_void);
    slots[1] = vslot(com_add_ref as *const c_void);
    slots[2] = vslot(com_release as *const c_void);
    slots[8] = vslot(sc_present as *const c_void);
    slots[9] = vslot(sc_get_buffer as *const c_void);
    slots[22] = vslot(sc_present1 as *const c_void);
    DxgiSwapChainVtbl { slots }
}

static DXGI_SWAPCHAIN_VTBL: DxgiSwapChainVtbl = build_swapchain_vtbl();

// ==================== Point d'entrée `d3d11.dll!D3D11CreateDevice` ====================

#[allow(clippy::too_many_arguments)]
pub(crate) extern "win64" fn D3D11CreateDevice(
    _adapter: *mut c_void,
    driver_type: u32,
    _software: *mut c_void,
    flags: u32,
    _feature_levels: *const u32,
    _num_levels: u32,
    sdk_version: u32,
    out_device: *mut *mut c_void,
    out_feature_level: *mut u32,
    out_context: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "D3D11CreateDevice", E_UNEXPECTED, {
        let with_surface = !winrt_objects::is_headless();
        let gpu = match GpuContext::new(with_surface) {
            Ok(g) => Some(g),
            Err(err) => {
                sb_warn!(
                    CHANNEL,
                    "Vulkan surface indisponible ({err}), tentative headless"
                );
                GpuContext::new(false).ok()
            }
        };

        sb_info!(
            CHANNEL,
            "D3D11CreateDevice(driver={driver_type}, flags={flags:#x}, sdk={sdk_version}, vulkan={})",
            gpu.as_ref()
                .map(|g| g.caps.device_name.as_str())
                .unwrap_or("logiciel")
        );

        let renderer = gpu
            .as_ref()
            .and_then(|g| VulkanRenderer::new(Arc::clone(g), 1280, 720).ok())
            .map(|r| Arc::new(Mutex::new(r)));

        let ctx_vtbl = D3D11_CONTEXT_VTBL.get_or_init(build_context_vtbl);
        let ctx_ptr = Box::into_raw(Box::new(D3D11ContextObject {
            header: ComHeader::new((ctx_vtbl as *const D3D11ContextVtbl).cast()),
            draw_calls: AtomicU64::new(0),
            renderer,
            bound_vs: AtomicUsize::new(0),
            bound_ps: AtomicUsize::new(0),
            vb_stride: AtomicU32::new(32),
            pipeline_cache: Mutex::new(HashMap::new()),
            pending_cmds: Mutex::new(Vec::new()),
        }))
        .cast::<c_void>();
        *D3D11_ACTIVE_CTX.lock() = ctx_ptr as usize;

        let factory_vtbl = DXGI_FACTORY_VTBL.get_or_init(build_factory_vtbl);
        let factory_ptr = Box::into_raw(Box::new(DxgiFactory2Object {
            header: ComHeader::new((factory_vtbl as *const DxgiFactory2Vtbl).cast()),
            gpu: gpu.clone(),
        }))
        .cast::<c_void>();

        let adapter_ptr = Box::into_raw(Box::new(DxgiAdapterObject {
            header: ComHeader::new((&raw const DXGI_ADAPTER_VTBL).cast()),
            factory: factory_ptr,
        }))
        .cast::<c_void>();

        let dxgi_dev_ptr = Box::into_raw(Box::new(DxgiDeviceObject {
            header: ComHeader::new((&raw const DXGI_DEVICE_VTBL).cast()),
            adapter: adapter_ptr,
        }))
        .cast::<c_void>();

        let dev_ptr = Box::into_raw(Box::new(D3D11DeviceObject {
            header: ComHeader::new((&raw const D3D11_DEVICE_VTBL).cast()),
            gpu,
            immediate_ctx: ctx_ptr,
            dxgi_device: dxgi_dev_ptr,
        }))
        .cast::<c_void>();

        if !out_device.is_null() {
            // SAFETY: `out_device` non-nul.
            unsafe {
                *out_device = dev_ptr;
            }
        }
        if !out_context.is_null() {
            // SAFETY: `out_context` non-nul.
            unsafe {
                *out_context = ctx_ptr;
            }
        }
        if !out_feature_level.is_null() {
            // SAFETY: `out_feature_level` non-nul.
            unsafe {
                *out_feature_level = D3D_FEATURE_LEVEL_11_1;
            }
        }
        S_OK
    })
}
