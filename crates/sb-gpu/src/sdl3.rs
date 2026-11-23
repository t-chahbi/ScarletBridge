//! Liaison dynamique sûre vers `libSDL3.so.0` (fenêtre Vulkan Wayland/Hyprland,
//! flux audio PipeWire et manettes Xbox avec vibrations).

use core::ffi::{c_char, c_int, c_void};
use std::ffi::{CStr, CString};
use std::ptr;
use std::sync::OnceLock;

use sb_common::{sb_debug, sb_info, sb_warn};

pub const SDL_INIT_AUDIO: u32 = 0x0000_0010;
pub const SDL_INIT_VIDEO: u32 = 0x0000_0020;
pub const SDL_INIT_GAMEPAD: u32 = 0x0000_2000;

pub const SDL_WINDOW_RESIZABLE: u64 = 0x0000_0000_0000_0020;
pub const SDL_WINDOW_VULKAN: u64 = 0x0000_0000_1000_0000;

pub const SDL_EVENT_QUIT: u32 = 0x100;
pub const SDL_EVENT_WINDOW_CLOSE_REQUESTED: u32 = 0x203;
pub const SDL_EVENT_KEY_DOWN: u32 = 0x300;
pub const SDL_EVENT_KEY_UP: u32 = 0x301;

pub const SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK: u32 = 0xFFFF_FFFF;
pub const SDL_AUDIO_S16LE: u32 = 0x8010;
pub const SDL_AUDIO_F32LE: u32 = 0x8120;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SdlAudioSpec {
    pub format: u32,
    pub channels: c_int,
    pub freq: c_int,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct GamepadSnapshot {
    pub connected: bool,
    pub buttons_mask: u32,
    pub left_trigger: f64,
    pub right_trigger: f64,
    pub left_thumb_x: f64,
    pub left_thumb_y: f64,
    pub right_thumb_x: f64,
    pub right_thumb_y: f64,
}

type SdlInitFn = unsafe extern "C" fn(u32) -> bool;
type SdlGetErrorFn = unsafe extern "C" fn() -> *const c_char;
type SdlCreateWindowFn = unsafe extern "C" fn(*const c_char, c_int, c_int, u64) -> *mut c_void;
type SdlDestroyWindowFn = unsafe extern "C" fn(*mut c_void);
type SdlPollEventFn = unsafe extern "C" fn(*mut u8) -> bool;
type SdlVkLoadLibraryFn = unsafe extern "C" fn(*const c_char) -> bool;
type SdlVkGetExtsFn = unsafe extern "C" fn(*mut u32) -> *const *const c_char;
type SdlVkCreateSurfaceFn = unsafe extern "C" fn(*mut c_void, u64, *const c_void, *mut u64) -> bool;
type SdlOpenAudioStreamFn =
    unsafe extern "C" fn(u32, *const SdlAudioSpec, *const c_void, *mut c_void) -> *mut c_void;
type SdlResumeAudioFn = unsafe extern "C" fn(*mut c_void) -> bool;
type SdlPutAudioFn = unsafe extern "C" fn(*mut c_void, *const c_void, c_int) -> bool;
type SdlDestroyAudioFn = unsafe extern "C" fn(*mut c_void);
type SdlGetGamepadsFn = unsafe extern "C" fn(*mut c_int) -> *mut u32;
type SdlOpenGamepadFn = unsafe extern "C" fn(u32) -> *mut c_void;
type SdlGetGamepadButtonFn = unsafe extern "C" fn(*mut c_void, c_int) -> bool;
type SdlGetGamepadAxisFn = unsafe extern "C" fn(*mut c_void, c_int) -> i16;
type SdlRumbleGamepadFn = unsafe extern "C" fn(*mut c_void, u16, u16, u32) -> bool;
type SdlFreeFn = unsafe extern "C" fn(*mut c_void);

pub struct Sdl3Lib {
    _handle: *mut libc::c_void,
    init: SdlInitFn,
    get_error: SdlGetErrorFn,
    create_window: SdlCreateWindowFn,
    destroy_window: SdlDestroyWindowFn,
    poll_event: SdlPollEventFn,
    vk_load_library: SdlVkLoadLibraryFn,
    vk_get_extensions: SdlVkGetExtsFn,
    vk_create_surface: SdlVkCreateSurfaceFn,
    open_audio_stream: SdlOpenAudioStreamFn,
    resume_audio_device: SdlResumeAudioFn,
    put_audio_data: SdlPutAudioFn,
    destroy_audio_stream: SdlDestroyAudioFn,
    get_gamepads: SdlGetGamepadsFn,
    open_gamepad: SdlOpenGamepadFn,
    get_gamepad_button: SdlGetGamepadButtonFn,
    get_gamepad_axis: SdlGetGamepadAxisFn,
    rumble_gamepad: SdlRumbleGamepadFn,
    free: SdlFreeFn,
}

// SAFETY: les pointeurs de symboles résolus via `dlsym` sur `libSDL3.so.0` sont statiques.
unsafe impl Send for Sdl3Lib {}
unsafe impl Sync for Sdl3Lib {}

static SDL3: OnceLock<Option<Sdl3Lib>> = OnceLock::new();

impl Sdl3Lib {
    /// Charge `libSDL3.so.0` dynamiquement (singleton global, sans dépendance compile-time C).
    pub fn load() -> Option<&'static Self> {
        SDL3.get_or_init(Self::load_internal).as_ref()
    }

    fn load_internal() -> Option<Self> {
        let candidates = [c"libSDL3.so.0", c"libSDL3.so"];
        let mut handle = ptr::null_mut();
        for name in candidates {
            // SAFETY: `dlopen` standard POSIX sur une bibliothèque système connue.
            handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if !handle.is_null() {
                break;
            }
        }
        if handle.is_null() {
            sb_warn!(
                "sdl3",
                "libSDL3.so.0 introuvable ; bascule en mode headless"
            );
            return None;
        }

        unsafe fn sym<T: Copy>(h: *mut libc::c_void, name: &CStr) -> Option<T> {
            // SAFETY: `h` est un handle `dlopen` valide et `name` est terminé par nul.
            let p = unsafe { libc::dlsym(h, name.as_ptr()) };
            if p.is_null() {
                None
            } else {
                // SAFETY: la signature cible `T` correspond au symbole C SDL3 documenté.
                Some(unsafe { core::mem::transmute_copy(&p) })
            }
        }

        // SAFETY: chargement de chaque point d'entrée officiel SDL3.
        unsafe {
            let lib = Self {
                _handle: handle,
                init: sym(handle, c"SDL_Init")?,
                get_error: sym(handle, c"SDL_GetError")?,
                create_window: sym(handle, c"SDL_CreateWindow")?,
                destroy_window: sym(handle, c"SDL_DestroyWindow")?,
                poll_event: sym(handle, c"SDL_PollEvent")?,
                vk_load_library: sym(handle, c"SDL_Vulkan_LoadLibrary")?,
                vk_get_extensions: sym(handle, c"SDL_Vulkan_GetInstanceExtensions")?,
                vk_create_surface: sym(handle, c"SDL_Vulkan_CreateSurface")?,
                open_audio_stream: sym(handle, c"SDL_OpenAudioDeviceStream")?,
                resume_audio_device: sym(handle, c"SDL_ResumeAudioStreamDevice")?,
                put_audio_data: sym(handle, c"SDL_PutAudioStreamData")?,
                destroy_audio_stream: sym(handle, c"SDL_DestroyAudioStream")?,
                get_gamepads: sym(handle, c"SDL_GetGamepads")?,
                open_gamepad: sym(handle, c"SDL_OpenGamepad")?,
                get_gamepad_button: sym(handle, c"SDL_GetGamepadButton")?,
                get_gamepad_axis: sym(handle, c"SDL_GetGamepadAxis")?,
                rumble_gamepad: sym(handle, c"SDL_RumbleGamepad")?,
                free: sym(handle, c"SDL_free")?,
            };
            sb_info!("sdl3", "libSDL3.so.0 chargée dynamiquement");
            Some(lib)
        }
    }

    pub fn last_error(&self) -> String {
        // SAFETY: `SDL_GetError` retourne toujours une chaîne C valide.
        let p = unsafe { (self.get_error)() };
        if p.is_null() {
            String::new()
        } else {
            // SAFETY: chaîne C non-nulle.
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        }
    }

    pub fn init_subsystems(&self, flags: u32) -> bool {
        // SAFETY: `SDL_Init` officiel SDL3.
        let ok = unsafe { (self.init)(flags) };
        if !ok {
            sb_warn!(
                "sdl3",
                "SDL_Init({flags:#x}) a échoué : {}",
                self.last_error()
            );
        }
        ok
    }

    /// Extensions d'instance Vulkan requises par SDL3 pour créer une surface.
    ///
    /// `SDL_Vulkan_GetInstanceExtensions` suppose le sous-système vidéo initialisé et la
    /// bibliothèque Vulkan chargée ; sans cela SDL déréférence un périphérique vidéo NULL et le
    /// processus plante. On initialise donc la vidéo (comptée par références : `SDL_Init` est sans
    /// danger si la fenêtre l'initialise aussi plus tard) et on charge Vulkan d'abord. Sans
    /// affichage disponible, retourne une liste vide : le rendu reste possible hors fenêtre.
    pub fn vulkan_instance_extensions(&self) -> Vec<CString> {
        if !self.init_subsystems(SDL_INIT_VIDEO) {
            sb_warn!(
                "sdl3",
                "vidéo SDL3 indisponible : aucune extension de surface Vulkan (rendu hors fenêtre)"
            );
            return Vec::new();
        }
        // SAFETY: `SDL_Vulkan_LoadLibrary(NULL)` charge le chargeur Vulkan par défaut ; la vidéo
        // est initialisée.
        if !unsafe { (self.vk_load_library)(ptr::null()) } {
            sb_warn!(
                "sdl3",
                "SDL_Vulkan_LoadLibrary a échoué : {} (aucune extension de surface Vulkan)",
                self.last_error()
            );
            return Vec::new();
        }
        let mut count = 0u32;
        // SAFETY: `SDL_Vulkan_GetInstanceExtensions` remplit `count` et retourne un tableau statique.
        let ptrs = unsafe { (self.vk_get_extensions)(&mut count) };
        if ptrs.is_null() || count == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            // SAFETY: `i < count`.
            let s_ptr = unsafe { *ptrs.add(i) };
            if !s_ptr.is_null() {
                // SAFETY: chaîne C valide fournie par SDL3.
                out.push(unsafe { CStr::from_ptr(s_ptr) }.to_owned());
            }
        }
        out
    }

    /// Initialise une fois le sous-système manette (`SDL_Init` est refcompté et sans danger si la
    /// fenêtre l'initialise aussi). Retourne `false` si SDL refuse : aucun accès manette alors.
    fn ensure_gamepad_subsystem(&self) -> bool {
        static READY: OnceLock<bool> = OnceLock::new();
        *READY.get_or_init(|| self.init_subsystems(SDL_INIT_GAMEPAD))
    }

    /// Lit l'état instantané de la première manette connectée (Wayland/evdev via SDL3).
    pub fn poll_primary_gamepad(&self) -> GamepadSnapshot {
        if !self.ensure_gamepad_subsystem() {
            return GamepadSnapshot::default();
        }
        let mut count: c_int = 0;
        // SAFETY: `SDL_GetGamepads` retourne un tableau alloué par SDL à libérer via `SDL_free`.
        let ids = unsafe { (self.get_gamepads)(&mut count) };
        if ids.is_null() || count <= 0 {
            if !ids.is_null() {
                // SAFETY: libération du tampon `SDL_GetGamepads`.
                unsafe { (self.free)(ids.cast()) };
            }
            return GamepadSnapshot::default();
        }

        // SAFETY: `count > 0`.
        let first_id = unsafe { *ids };
        // SAFETY: libération du tampon `SDL_GetGamepads`.
        unsafe { (self.free)(ids.cast()) };

        // SAFETY: ouvre/récupère la manette identifiée par `first_id`.
        let pad = unsafe { (self.open_gamepad)(first_id) };
        if pad.is_null() {
            return GamepadSnapshot::default();
        }

        let norm_axis = |axis: c_int| -> f64 {
            // SAFETY: `pad` est un `SDL_Gamepad*` valide.
            let raw = unsafe { (self.get_gamepad_axis)(pad, axis) };
            (f64::from(raw) / 32767.0).clamp(-1.0, 1.0)
        };
        let norm_trigger = |axis: c_int| -> f64 {
            // SAFETY: `pad` est un `SDL_Gamepad*` valide.
            let raw = unsafe { (self.get_gamepad_axis)(pad, axis) };
            (f64::from(raw) / 32767.0).clamp(0.0, 1.0)
        };

        // Mapping vers les bits `Windows.Gaming.Input.GamepadButtons`.
        let mut mask = 0u32;
        let map = [
            (0, 0x0004),  // South (A)
            (1, 0x0008),  // East (B)
            (2, 0x0010),  // West (X)
            (3, 0x0020),  // North (Y)
            (4, 0x0002),  // Back / View
            (6, 0x0001),  // Start / Menu
            (7, 0x0400),  // LeftStick
            (8, 0x0800),  // RightStick
            (9, 0x0100),  // LeftShoulder
            (10, 0x0200), // RightShoulder
            (11, 0x0040), // DPadUp
            (12, 0x0080), // DPadDown
            (13, 0x0100), // DPadLeft
            (14, 0x0200), // DPadRight
        ];
        for (sdl_btn, winrt_bit) in map {
            // SAFETY: `pad` valide.
            if unsafe { (self.get_gamepad_button)(pad, sdl_btn) } {
                mask |= winrt_bit;
            }
        }

        GamepadSnapshot {
            connected: true,
            buttons_mask: mask,
            left_thumb_x: norm_axis(0),
            left_thumb_y: -norm_axis(1),
            right_thumb_x: norm_axis(2),
            right_thumb_y: -norm_axis(3),
            left_trigger: norm_trigger(4),
            right_trigger: norm_trigger(5),
        }
    }

    pub fn rumble_primary_gamepad(&self, low_freq: u16, high_freq: u16, duration_ms: u32) {
        if !self.ensure_gamepad_subsystem() {
            return;
        }
        let mut count: c_int = 0;
        // SAFETY: `SDL_GetGamepads`.
        let ids = unsafe { (self.get_gamepads)(&mut count) };
        if ids.is_null() {
            return;
        }
        if count > 0 {
            // SAFETY: `count > 0`.
            let first_id = unsafe { *ids };
            let pad = unsafe { (self.open_gamepad)(first_id) };
            if !pad.is_null() {
                // SAFETY: `pad` valide.
                let _ = unsafe { (self.rumble_gamepad)(pad, low_freq, high_freq, duration_ms) };
            }
        }
        // SAFETY: libération du tableau `SDL_GetGamepads`.
        unsafe { (self.free)(ids.cast()) };
    }
}

pub struct Sdl3Window {
    lib: &'static Sdl3Lib,
    raw: *mut c_void,
    pub width: u32,
    pub height: u32,
    pub should_close: bool,
}

// SAFETY: accédé sous verrou depuis le thread principal de rendu.
unsafe impl Send for Sdl3Window {}

impl Sdl3Window {
    pub fn new(title: &str, width: u32, height: u32) -> Option<Self> {
        let lib = Sdl3Lib::load()?;
        if !lib.init_subsystems(SDL_INIT_VIDEO | SDL_INIT_GAMEPAD) {
            return None;
        }
        let c_title = CString::new(title).unwrap_or_default();
        // SAFETY: `SDL_CreateWindow` avec drapeau `SDL_WINDOW_VULKAN`.
        let raw = unsafe {
            (lib.create_window)(
                c_title.as_ptr(),
                width as c_int,
                height as c_int,
                SDL_WINDOW_VULKAN | SDL_WINDOW_RESIZABLE,
            )
        };
        if raw.is_null() {
            sb_warn!("sdl3", "SDL_CreateWindow a échoué : {}", lib.last_error());
            return None;
        }
        sb_info!(
            "sdl3",
            "fenêtre Vulkan Wayland/SDL3 créée ({width}x{height}) : '{title}'"
        );
        Some(Self {
            lib,
            raw,
            width,
            height,
            should_close: false,
        })
    }

    pub fn create_vulkan_surface(
        &self,
        vk_instance: ash::vk::Instance,
    ) -> Option<ash::vk::SurfaceKHR> {
        use ash::vk::Handle;
        let mut raw_surface: u64 = 0;
        // SAFETY: `self.raw` est un `SDL_Window*` Vulkan valide et `vk_instance` est une `VkInstance`.
        let ok = unsafe {
            (self.lib.vk_create_surface)(
                self.raw,
                vk_instance.as_raw(),
                ptr::null(),
                &mut raw_surface,
            )
        };
        if !ok || raw_surface == 0 {
            sb_warn!(
                "sdl3",
                "SDL_Vulkan_CreateSurface a échoué : {}",
                self.lib.last_error()
            );
            return None;
        }
        Some(ash::vk::SurfaceKHR::from_raw(raw_surface))
    }

    /// Pompe les événements SDL3 (clavier / fermeture de fenêtre). Retourne `false` si fermeture demandée.
    pub fn poll_events(&mut self) -> bool {
        let mut event = [0u8; 128];
        // SAFETY: `SDL_Event` fait au maximum 128 octets en SDL3.
        while unsafe { (self.lib.poll_event)(event.as_mut_ptr()) } {
            let ev_type = u32::from_ne_bytes([event[0], event[1], event[2], event[3]]);
            if ev_type == SDL_EVENT_QUIT || ev_type == SDL_EVENT_WINDOW_CLOSE_REQUESTED {
                sb_debug!("sdl3", "événement fermeture fenêtre reçu ({ev_type:#x})");
                self.should_close = true;
            }
        }
        !self.should_close
    }
}

impl Drop for Sdl3Window {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: `self.raw` provient de `SDL_CreateWindow`.
            unsafe { (self.lib.destroy_window)(self.raw) };
            self.raw = ptr::null_mut();
        }
    }
}

/// Flux audio temps réel SDL3 → PipeWire pour `XAudio2_9`.
pub struct Sdl3AudioStream {
    lib: &'static Sdl3Lib,
    raw: *mut c_void,
}

// SAFETY: utilisé sous verrou depuis les appels `IXAudio2SourceVoice::SubmitSourceBuffer`.
unsafe impl Send for Sdl3AudioStream {}

impl Sdl3AudioStream {
    pub fn open_pcm(channels: u32, sample_rate: u32, is_float: bool) -> Option<Self> {
        let lib = Sdl3Lib::load()?;
        if !lib.init_subsystems(SDL_INIT_AUDIO) {
            return None;
        }
        let spec = SdlAudioSpec {
            format: if is_float {
                SDL_AUDIO_F32LE
            } else {
                SDL_AUDIO_S16LE
            },
            channels: channels.clamp(1, 8) as c_int,
            freq: sample_rate.clamp(8000, 192_000) as c_int,
        };
        // SAFETY: `SDL_OpenAudioDeviceStream` officiel SDL3.
        let raw = unsafe {
            (lib.open_audio_stream)(
                SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK,
                &spec,
                ptr::null(),
                ptr::null_mut(),
            )
        };
        if raw.is_null() {
            sb_warn!(
                "sdl3",
                "SDL_OpenAudioDeviceStream a échoué : {}",
                lib.last_error()
            );
            return None;
        }
        // SAFETY: démarre immédiatement la lecture sur le périphérique PipeWire par défaut.
        let _ = unsafe { (lib.resume_audio_device)(raw) };
        sb_info!(
            "sdl3",
            "flux audio PipeWire/SDL3 ouvert ({channels} ch, {sample_rate} Hz, float={is_float})"
        );
        Some(Self { lib, raw })
    }

    pub fn push_bytes(&self, data: &[u8]) {
        if data.is_empty() || self.raw.is_null() {
            return;
        }
        // SAFETY: `self.raw` est un `SDL_AudioStream*` valide et `data` est lisible.
        let _ = unsafe {
            (self.lib.put_audio_data)(self.raw, data.as_ptr().cast(), data.len() as c_int)
        };
    }
}

impl Drop for Sdl3AudioStream {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: `self.raw` provient de `SDL_OpenAudioDeviceStream`.
            unsafe { (self.lib.destroy_audio_stream)(self.raw) };
            self.raw = ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Régression : interroger les extensions Vulkan avant toute initialisation de SDL faisait
    /// planter le processus (périphérique vidéo NULL dans SDL). Le pilote vidéo `dummy` rend le test
    /// indépendant de tout affichage ; le résultat peut être vide, mais ne doit jamais planter.
    #[test]
    fn vulkan_instance_extensions_before_any_sdl_init_does_not_crash() {
        // SAFETY: variable lue uniquement par SDL, positionnée avant tout appel SDL de ce test.
        unsafe { std::env::set_var("SDL_VIDEODRIVER", "dummy") };
        let Some(lib) = Sdl3Lib::load() else {
            return; // libSDL3 absente : rien à vérifier
        };
        let _exts = lib.vulkan_instance_extensions();
        // Un second appel (vidéo déjà initialisée) doit aussi réussir.
        let _again = lib.vulkan_instance_extensions();
    }

    /// Les accès manette initialisent eux-mêmes le sous-système : aucun plantage sans `SDL_Init`.
    #[test]
    fn gamepad_access_without_prior_init_does_not_crash() {
        // SAFETY: variable lue uniquement par SDL avant son initialisation.
        unsafe { std::env::set_var("SDL_VIDEODRIVER", "dummy") };
        let Some(lib) = Sdl3Lib::load() else {
            return;
        };
        let snap = lib.poll_primary_gamepad();
        lib.rumble_primary_gamepad(0, 0, 1);
        let _ = snap.connected;
    }
}
