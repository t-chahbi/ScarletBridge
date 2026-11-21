//! Implémentation HLE COM de `XAudio2_9.dll` (`IXAudio2`, `IXAudio2MasteringVoice`,
//! `IXAudio2SourceVoice`) branchée sur **SDL3 Audio → PipeWire**.

#![allow(non_snake_case, unused_variables)]

use core::ffi::c_void;
use std::ptr;

use parking_lot::Mutex;
use sb_common::abi::{E_POINTER, E_UNEXPECTED, HResult, S_OK};
use sb_common::{ffi_guard, sb_debug, sb_info};
use sb_gpu::Sdl3AudioStream;

use crate::com::{ComHeader, Guid, com_add_ref, com_query_interface_list, com_release};

const CHANNEL: &str = "xaudio2";

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct WaveFormatEx {
    format_tag: u16,
    channels: u16,
    samples_per_sec: u32,
    avg_bytes_per_sec: u32,
    block_align: u16,
    bits_per_sample: u16,
    cb_size: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct XAudio2Buffer {
    flags: u32,
    audio_bytes: u32,
    audio_data: *const u8,
    play_begin: u32,
    play_length: u32,
    loop_begin: u32,
    loop_length: u32,
    loop_count: u32,
    context: *mut c_void,
}

#[repr(C)]
struct XAudio2VoiceState {
    current_buffer_context: *mut c_void,
    buffers_queued: u32,
    samples_played: u64,
}

// ==================== IXAudio2 ====================

#[repr(C)]
struct IXAudio2Vtbl {
    query_interface: extern "win64" fn(*mut ComHeader, *const Guid, *mut *mut c_void) -> HResult,
    add_ref: extern "win64" fn(*mut ComHeader) -> u32,
    release: extern "win64" fn(*mut ComHeader) -> u32,
    register_for_callbacks: extern "win64" fn(*mut XAudio2Object, *mut c_void) -> HResult,
    unregister_for_callbacks: extern "win64" fn(*mut XAudio2Object, *mut c_void),
    create_source_voice: extern "win64" fn(
        *mut XAudio2Object,
        *mut *mut c_void,
        *const WaveFormatEx,
        u32,
        f32,
        *mut c_void,
        *const c_void,
        *const c_void,
    ) -> HResult,
    create_submix_voice: extern "win64" fn(
        *mut XAudio2Object,
        *mut *mut c_void,
        u32,
        u32,
        u32,
        u32,
        *const c_void,
        *const c_void,
    ) -> HResult,
    create_mastering_voice: extern "win64" fn(
        *mut XAudio2Object,
        *mut *mut c_void,
        u32,
        u32,
        u32,
        *const u16,
        *const c_void,
        u32,
    ) -> HResult,
    start_engine: extern "win64" fn(*mut XAudio2Object) -> HResult,
    stop_engine: extern "win64" fn(*mut XAudio2Object),
    commit_changes: extern "win64" fn(*mut XAudio2Object, u32) -> HResult,
    get_performance_data: extern "win64" fn(*mut XAudio2Object, *mut u8),
    set_debug_configuration: extern "win64" fn(*mut XAudio2Object, *const c_void, *mut c_void),
}

#[repr(C)]
struct XAudio2Object {
    header: ComHeader,
    running: bool,
}

static IXAUDIO2_VTBL: IXAudio2Vtbl = IXAudio2Vtbl {
    query_interface: xa2_qi,
    add_ref: com_add_ref,
    release: com_release,
    register_for_callbacks: xa2_register_callbacks,
    unregister_for_callbacks: xa2_unregister_callbacks,
    create_source_voice: xa2_create_source_voice,
    create_submix_voice: xa2_create_submix_voice,
    create_mastering_voice: xa2_create_mastering_voice,
    start_engine: xa2_start_engine,
    stop_engine: xa2_stop_engine,
    commit_changes: xa2_commit_changes,
    get_performance_data: xa2_get_perf,
    set_debug_configuration: xa2_set_debug,
};

extern "win64" fn xa2_qi(
    this: *mut ComHeader,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    com_query_interface_list("IXAudio2", this, riid, ppv, &[])
}

extern "win64" fn xa2_register_callbacks(_this: *mut XAudio2Object, _cb: *mut c_void) -> HResult {
    S_OK
}

extern "win64" fn xa2_unregister_callbacks(_this: *mut XAudio2Object, _cb: *mut c_void) {}

extern "win64" fn xa2_create_mastering_voice(
    _this: *mut XAudio2Object,
    out_voice: *mut *mut c_void,
    channels: u32,
    sample_rate: u32,
    _flags: u32,
    _device_id: *const u16,
    _effect_chain: *const c_void,
    _stream_cat: u32,
) -> HResult {
    ffi_guard!(CHANNEL, "CreateMasteringVoice", E_UNEXPECTED, {
        if out_voice.is_null() {
            return E_POINTER;
        }
        sb_info!(
            CHANNEL,
            "IXAudio2::CreateMasteringVoice(channels={channels}, rate={sample_rate})"
        );
        let obj = Box::new(VoiceObject {
            header: ComHeader::new((&raw const VOICE_VTBL).cast()),
            stream: Mutex::new(None),
            callback: ptr::null_mut(),
            samples_played: Mutex::new(0),
            volume: Mutex::new(1.0),
        });
        // SAFETY: `out_voice` vérifié non-nul.
        unsafe {
            *out_voice = Box::into_raw(obj).cast();
        }
        S_OK
    })
}

extern "win64" fn xa2_create_submix_voice(
    this: *mut XAudio2Object,
    out_voice: *mut *mut c_void,
    channels: u32,
    sample_rate: u32,
    _flags: u32,
    _stage: u32,
    _send_list: *const c_void,
    _effect_chain: *const c_void,
) -> HResult {
    xa2_create_mastering_voice(
        this,
        out_voice,
        channels,
        sample_rate,
        0,
        ptr::null(),
        ptr::null(),
        0,
    )
}

extern "win64" fn xa2_create_source_voice(
    _this: *mut XAudio2Object,
    out_voice: *mut *mut c_void,
    fmt: *const WaveFormatEx,
    _flags: u32,
    _max_freq_ratio: f32,
    callback: *mut c_void,
    _send_list: *const c_void,
    _effect_chain: *const c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CreateSourceVoice", E_UNEXPECTED, {
        if out_voice.is_null() {
            return E_POINTER;
        }
        let (channels, sample_rate, is_float) = if !fmt.is_null() {
            // SAFETY: `fmt` vérifié non-nul.
            let wf = unsafe { *fmt };
            (
                u32::from(wf.channels.max(1)),
                wf.samples_per_sec.max(8000),
                wf.format_tag == 3 || wf.bits_per_sample == 32,
            )
        } else {
            (2, 44100, false)
        };

        let headless = std::env::var("SB_HEADLESS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let stream = if headless {
            None
        } else {
            Sdl3AudioStream::open_pcm(channels, sample_rate, is_float)
        };

        sb_info!(
            CHANNEL,
            "IXAudio2::CreateSourceVoice({channels}ch, {sample_rate}Hz, float={is_float}, pipewire={})",
            stream.is_some()
        );

        let obj = Box::new(VoiceObject {
            header: ComHeader::new((&raw const VOICE_VTBL).cast()),
            stream: Mutex::new(stream),
            callback,
            samples_played: Mutex::new(0),
            volume: Mutex::new(1.0),
        });
        // SAFETY: `out_voice` vérifié non-nul.
        unsafe {
            *out_voice = Box::into_raw(obj).cast();
        }
        S_OK
    })
}

extern "win64" fn xa2_start_engine(this: *mut XAudio2Object) -> HResult {
    if !this.is_null() {
        // SAFETY: `this` non-nul.
        unsafe {
            (*this).running = true;
        }
    }
    S_OK
}

extern "win64" fn xa2_stop_engine(this: *mut XAudio2Object) {
    if !this.is_null() {
        // SAFETY: `this` non-nul.
        unsafe {
            (*this).running = false;
        }
    }
}

extern "win64" fn xa2_commit_changes(_this: *mut XAudio2Object, _op_set: u32) -> HResult {
    S_OK
}

extern "win64" fn xa2_get_perf(_this: *mut XAudio2Object, out_perf: *mut u8) {
    if !out_perf.is_null() {
        // SAFETY: `XAUDIO2_PERFORMANCE_DATA` fait 64 octets.
        unsafe {
            ptr::write_bytes(out_perf, 0, 64);
        }
    }
}

extern "win64" fn xa2_set_debug(_this: *mut XAudio2Object, _cfg: *const c_void, _res: *mut c_void) {
}

// ==================== IXAudio2Voice / IXAudio2SourceVoice ====================
// Remarque ABI : `IXAudio2Voice` N'hérite PAS de `IUnknown` ! Son slot 0 est `GetVoiceDetails`.

#[repr(C)]
struct VoiceVtbl {
    get_voice_details: extern "win64" fn(*mut VoiceObject, *mut u32), // 0
    set_output_voices: extern "win64" fn(*mut VoiceObject, *const c_void) -> HResult, // 1
    set_effect_chain: extern "win64" fn(*mut VoiceObject, *const c_void) -> HResult, // 2
    enable_effect: extern "win64" fn(*mut VoiceObject, u32, u32) -> HResult, // 3
    disable_effect: extern "win64" fn(*mut VoiceObject, u32, u32) -> HResult, // 4
    get_effect_state: extern "win64" fn(*mut VoiceObject, u32, *mut i32), // 5
    set_effect_parameters:
        extern "win64" fn(*mut VoiceObject, u32, *const c_void, u32, u32) -> HResult, // 6
    get_effect_parameters: extern "win64" fn(*mut VoiceObject, u32, *mut c_void, u32) -> HResult, // 7
    set_filter_parameters: extern "win64" fn(*mut VoiceObject, *const c_void, u32) -> HResult, // 8
    get_filter_parameters: extern "win64" fn(*mut VoiceObject, *mut c_void),                   // 9
    set_output_filter_parameters:
        extern "win64" fn(*mut VoiceObject, *mut c_void, *const c_void, u32) -> HResult, // 10
    get_output_filter_parameters: extern "win64" fn(*mut VoiceObject, *mut c_void, *mut c_void), // 11
    set_volume: extern "win64" fn(*mut VoiceObject, f32, u32) -> HResult, // 12
    get_volume: extern "win64" fn(*mut VoiceObject, *mut f32),            // 13
    set_channel_volumes: extern "win64" fn(*mut VoiceObject, u32, *const f32, u32) -> HResult, // 14
    get_channel_volumes: extern "win64" fn(*mut VoiceObject, u32, *mut f32), // 15
    set_output_matrix:
        extern "win64" fn(*mut VoiceObject, *mut c_void, u32, u32, *const f32, u32) -> HResult, // 16
    get_output_matrix: extern "win64" fn(*mut VoiceObject, *mut c_void, u32, u32, *mut f32), // 17
    destroy_voice: extern "win64" fn(*mut VoiceObject),                                      // 18
    // Méthodes spécifiques `IXAudio2SourceVoice` (slots 19..27) :
    start: extern "win64" fn(*mut VoiceObject, u32, u32) -> HResult, // 19
    stop: extern "win64" fn(*mut VoiceObject, u32, u32) -> HResult,  // 20
    submit_source_buffer:
        extern "win64" fn(*mut VoiceObject, *const XAudio2Buffer, *const c_void) -> HResult, // 21
    flush_source_buffers: extern "win64" fn(*mut VoiceObject) -> HResult, // 22
    discontinuity: extern "win64" fn(*mut VoiceObject) -> HResult,   // 23
    exit_loop: extern "win64" fn(*mut VoiceObject, u32) -> HResult,  // 24
    get_state: extern "win64" fn(*mut VoiceObject, *mut XAudio2VoiceState, u32), // 25
    set_frequency_ratio: extern "win64" fn(*mut VoiceObject, f32, u32) -> HResult, // 26
    get_frequency_ratio: extern "win64" fn(*mut VoiceObject, *mut f32), // 27
    set_source_sample_rate: extern "win64" fn(*mut VoiceObject, u32) -> HResult, // 28
}

#[repr(C)]
struct VoiceObject {
    header: ComHeader,
    stream: Mutex<Option<Sdl3AudioStream>>,
    callback: *mut c_void,
    samples_played: Mutex<u64>,
    volume: Mutex<f32>,
}

static VOICE_VTBL: VoiceVtbl = VoiceVtbl {
    get_voice_details: voice_get_details,
    set_output_voices: voice_ok_1,
    set_effect_chain: voice_ok_1,
    enable_effect: voice_ok_2,
    disable_effect: voice_ok_2,
    get_effect_state: voice_get_effect_state,
    set_effect_parameters: voice_ok_4,
    get_effect_parameters: voice_ok_3,
    set_filter_parameters: voice_ok_2p,
    get_filter_parameters: voice_void_1,
    set_output_filter_parameters: voice_ok_3p,
    get_output_filter_parameters: voice_void_2,
    set_volume: voice_set_volume,
    get_volume: voice_get_volume,
    set_channel_volumes: voice_ok_3f,
    get_channel_volumes: voice_void_2u,
    set_output_matrix: voice_ok_5,
    get_output_matrix: voice_void_4,
    destroy_voice: voice_destroy,
    start: voice_start,
    stop: voice_stop,
    submit_source_buffer: voice_submit_source_buffer,
    flush_source_buffers: voice_ok_0,
    discontinuity: voice_ok_0,
    exit_loop: voice_ok_1u,
    get_state: voice_get_state,
    set_frequency_ratio: voice_set_volume,
    get_frequency_ratio: voice_get_volume,
    set_source_sample_rate: voice_ok_1u,
};

extern "win64" fn voice_get_details(_this: *mut VoiceObject, out: *mut u32) {
    if !out.is_null() {
        // SAFETY: `XAUDIO2_VOICE_DETAILS` = 4x `u32` (flags, active, channels, sample_rate).
        unsafe {
            *out = 0;
            *out.add(1) = 0;
            *out.add(2) = 2;
            *out.add(3) = 44100;
        }
    }
}
extern "win64" fn voice_ok_0(_this: *mut VoiceObject) -> HResult {
    S_OK
}
extern "win64" fn voice_ok_1(_this: *mut VoiceObject, _a: *const c_void) -> HResult {
    S_OK
}
extern "win64" fn voice_ok_1u(_this: *mut VoiceObject, _a: u32) -> HResult {
    S_OK
}
extern "win64" fn voice_ok_2(_this: *mut VoiceObject, _a: u32, _b: u32) -> HResult {
    S_OK
}
extern "win64" fn voice_ok_2p(_this: *mut VoiceObject, _a: *const c_void, _b: u32) -> HResult {
    S_OK
}
extern "win64" fn voice_ok_3(
    _this: *mut VoiceObject,
    _a: u32,
    _b: *mut c_void,
    _c: u32,
) -> HResult {
    S_OK
}
extern "win64" fn voice_ok_3p(
    _this: *mut VoiceObject,
    _a: *mut c_void,
    _b: *const c_void,
    _c: u32,
) -> HResult {
    S_OK
}
extern "win64" fn voice_ok_3f(
    _this: *mut VoiceObject,
    _a: u32,
    _b: *const f32,
    _c: u32,
) -> HResult {
    S_OK
}
extern "win64" fn voice_ok_4(
    _this: *mut VoiceObject,
    _a: u32,
    _b: *const c_void,
    _c: u32,
    _d: u32,
) -> HResult {
    S_OK
}
extern "win64" fn voice_ok_5(
    _this: *mut VoiceObject,
    _a: *mut c_void,
    _b: u32,
    _c: u32,
    _d: *const f32,
    _e: u32,
) -> HResult {
    S_OK
}
extern "win64" fn voice_get_effect_state(_this: *mut VoiceObject, _idx: u32, out: *mut i32) {
    if !out.is_null() {
        // SAFETY: `out` vérifié non-nul.
        unsafe {
            *out = 0;
        }
    }
}
extern "win64" fn voice_void_1(_this: *mut VoiceObject, _a: *mut c_void) {}
extern "win64" fn voice_void_2(_this: *mut VoiceObject, _a: *mut c_void, _b: *mut c_void) {}
extern "win64" fn voice_void_2u(_this: *mut VoiceObject, _a: u32, _b: *mut f32) {}
extern "win64" fn voice_void_4(
    _this: *mut VoiceObject,
    _a: *mut c_void,
    _b: u32,
    _c: u32,
    _d: *mut f32,
) {
}

extern "win64" fn voice_set_volume(this: *mut VoiceObject, vol: f32, _op_set: u32) -> HResult {
    if !this.is_null() {
        // SAFETY: `this` non-nul.
        unsafe {
            *(*this).volume.lock() = vol;
        }
    }
    S_OK
}

extern "win64" fn voice_get_volume(this: *mut VoiceObject, out: *mut f32) {
    if !this.is_null() && !out.is_null() {
        // SAFETY: `this` et `out` non-nuls.
        unsafe {
            *out = *(*this).volume.lock();
        }
    }
}

extern "win64" fn voice_destroy(this: *mut VoiceObject) {
    if !this.is_null() {
        // SAFETY: libération du flux SDL3 interne.
        unsafe {
            *(*this).stream.lock() = None;
        }
    }
}

extern "win64" fn voice_start(_this: *mut VoiceObject, _flags: u32, _op_set: u32) -> HResult {
    sb_debug!(CHANNEL, "IXAudio2SourceVoice::Start()");
    S_OK
}

extern "win64" fn voice_stop(_this: *mut VoiceObject, _flags: u32, _op_set: u32) -> HResult {
    sb_debug!(CHANNEL, "IXAudio2SourceVoice::Stop()");
    S_OK
}

extern "win64" fn voice_submit_source_buffer(
    this: *mut VoiceObject,
    buf: *const XAudio2Buffer,
    _wma: *const c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "SubmitSourceBuffer", E_UNEXPECTED, {
        if this.is_null() || buf.is_null() {
            return E_POINTER;
        }
        // SAFETY: `this` et `buf` sont valides.
        let (b, voice) = unsafe { (*buf, &*this) };
        if !b.audio_data.is_null() && b.audio_bytes > 0 {
            // SAFETY: `audio_data` pointe vers `audio_bytes` octets PCM.
            let pcm = unsafe { std::slice::from_raw_parts(b.audio_data, b.audio_bytes as usize) };
            if let Some(ref stream) = *voice.stream.lock() {
                stream.push_bytes(pcm);
            }
            *voice.samples_played.lock() += u64::from(b.audio_bytes / 4);
        }

        // Notifie immédiatement `IXAudio2VoiceCallback::OnBufferEnd(context)` (slot 4) si fourni.
        if !voice.callback.is_null() {
            // SAFETY: `callback` est un pointeur COM `IXAudio2VoiceCallback*` ; slot 4 = `OnBufferEnd`.
            unsafe {
                let vtbl = *(voice.callback.cast::<*const usize>());
                if !vtbl.is_null() {
                    let on_buffer_end_addr = *vtbl.add(4);
                    if on_buffer_end_addr != 0 {
                        let on_buffer_end: unsafe extern "win64" fn(*mut c_void, *mut c_void) =
                            core::mem::transmute(on_buffer_end_addr);
                        on_buffer_end(voice.callback, b.context);
                    }
                }
            }
        }

        S_OK
    })
}

extern "win64" fn voice_get_state(
    this: *mut VoiceObject,
    out_state: *mut XAudio2VoiceState,
    _flags: u32,
) {
    if !this.is_null() && !out_state.is_null() {
        // SAFETY: `this` et `out_state` vérifiés non-nuls.
        unsafe {
            let played = *(*this).samples_played.lock();
            *out_state = XAudio2VoiceState {
                current_buffer_context: ptr::null_mut(),
                buffers_queued: 0,
                samples_played: played,
            };
        }
    }
}

/// Point d'entrée `XAudio2Create` (exporté par nom et à l'ordinal `@1`).
pub(crate) extern "win64" fn XAudio2Create(
    pp_xaudio2: *mut *mut c_void,
    flags: u32,
    processor: u32,
) -> HResult {
    ffi_guard!(CHANNEL, "XAudio2Create", E_UNEXPECTED, {
        if pp_xaudio2.is_null() {
            return E_POINTER;
        }
        sb_info!(
            CHANNEL,
            "XAudio2Create(flags={flags:#x}, processor={processor:#x}) -> IXAudio2 (PipeWire/SDL3)"
        );
        let obj = Box::new(XAudio2Object {
            header: ComHeader::new((&raw const IXAUDIO2_VTBL).cast()),
            running: true,
        });
        // SAFETY: `pp_xaudio2` vérifié non-nul.
        unsafe {
            *pp_xaudio2 = Box::into_raw(obj).cast();
        }
        S_OK
    })
}
