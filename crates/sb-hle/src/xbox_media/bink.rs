//! Bink 2 (RAD Game Tools) — `Bink2Scarlett.dll` / `bink2w64.dll`.
//!
//! Le codec vidéo Bink 2 est propriétaire et n'est pas disponible : `BinkOpen` parse l'en-tête
//! public du conteneur `.bk2` (signature `KB2`/`BIK` + révision, nombre d'images, dimensions,
//! cadence) via le VFS, remplit la struct `BINK` publique puis « joue » la vidéo en noir
//! (Y=16, Cr=Cb=128, A=255) dans les frame buffers enregistrés, en avançant `FrameNum` au rythme
//! réel de la cadence du fichier. Les cinématiques se déroulent donc et se terminent sans bloquer.
//!
//! Signatures (bink.h public, `RADEXPLINK` = ABI Win64 standard) :
//! - `HBINK BinkOpen(char const* name, U32 flags)`
//! - `HBINK BinkOpenWithOptions(char const* name, BINK_OPEN_OPTIONS const* boo, U32 flags)` —
//!   *déduit* : la struct d'options n'est pas lue (options ignorées, flags honorés).
//! - `void BinkClose(HBINK)`, `S32 BinkDoFrame(HBINK)`, `void BinkNextFrame(HBINK)`,
//!   `S32 BinkWait(HBINK)`, `void BinkGoto(HBINK, U32 frame, S32 flags)`,
//!   `S32 BinkPause(HBINK, S32 pause)`, `U32 BinkGetKeyFrame(HBINK, U32 frame, S32 flags)`,
//!   `S32 BinkShouldSkip(HBINK)`, `void BinkSetVolume(HBINK, U32 trackid, S32 volume)`,
//!   `void BinkSetWillLoop(HBINK, S32 onoff)`, `S32 BinkSetSoundSystem(BINKSNDSYSOPEN, UINTa)`,
//!   `void BinkSetMemory(BINKMEMALLOC, BINKMEMFREE)`, `void BinkFreeGlobals(void)`,
//!   `char* BinkGetError(void)`,
//!   `void BinkGetFrameBuffersInfo(HBINK, BINKFRAMEBUFFERS*)`,
//!   `void BinkRegisterFrameBuffers(HBINK, BINKFRAMEBUFFERS*)`,
//!   `S32 BinkDoFrameAsyncMulti(HBINK, U32* thread_indices, S32 count)`,
//!   `S32 BinkDoFrameAsyncWait(HBINK, S32 us)`, `S32 BinkStartAsyncThread(S32 n, void const* p)`,
//!   `S32 BinkRequestStopAsyncThreadsMulti(U32* threads, S32 count)`,
//!   `S32 BinkWaitStopAsyncThreadsMulti(U32* threads, S32 count)`.
//! - `BinkSetOSFileCallbacks(...)` — *déduit* (variadique côté HLE) : callbacks mémorisés mais non
//!   utilisés, toutes les E/S passent par le VFS sandboxé.
//! - `S32 BinkGetPlatformInfo(S32 info, void* out)` — *déduit* : aucune requête plateforme n'est
//!   connue publiquement, retourne 0 (échec) sans écrire `out`.
//!
//! Le son n'est pas restitué (pistes muettes) : `BinkSetSoundSystem` mémorise le fournisseur.

#![allow(non_snake_case)]

use core::ffi::{CStr, c_char, c_void};
use core::mem::{offset_of, size_of};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use sb_common::{ffi_guard, sb_debug, sb_info};

const CHANNEL: &str = "bink2";

// Flags d'ouverture publics (bink.h).
pub const BINKNOFRAMEBUFFERS: u32 = 0x0000_0400;
pub const BINKALPHA: u32 = 0x0010_0000;
pub const BINKFILEHANDLE: u32 = 0x0080_0000;
pub const BINKFROMMEMORY: u32 = 0x0400_0000;

// BinkGetKeyFrame flags.
const BINKGETKEYPREVIOUS: i32 = 0;
const BINKGETKEYNEXT: i32 = 1;
const BINKGETKEYCLOSEST: i32 = 2;
const BINKGETKEYNOTEQUAL: i32 = 128;

const BINKMAXDIRTYRECTS: usize = 8;
const BINKMAXFRAMEBUFFERS: usize = 2;

// ==================== Structs publiques (bink.h) ====================

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BinkRect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

/// Struct `BINK` publique. Les champs `Width`..`OpenFlags` suivent bink.h ; les champs suivants
/// (`BinkType`..`FrameBuffers`) sont *déduits* de bink.h 2.x et le reste est réservé (zéro).
#[repr(C)]
pub struct Bink {
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub frame_num: u32,
    pub frame_rate: u32,
    pub frame_rate_div: u32,
    pub read_error: u32,
    pub open_flags: u32,
    pub bink_type: u32,
    pub largest_frame_size: u32,
    pub frame_size: u32,
    pub snd_size: u32,
    pub frame_change_percent: u32,
    pub frame_rects: [BinkRect; BINKMAXDIRTYRECTS],
    pub num_rects: i32,
    pub frame_buffers: *mut BinkFrameBuffers,
    pub reserved: [u8; 0x400 - 192],
}

const _: () = {
    assert!(offset_of!(Bink, width) == 0);
    assert!(offset_of!(Bink, height) == 4);
    assert!(offset_of!(Bink, frames) == 8);
    assert!(offset_of!(Bink, frame_num) == 12);
    assert!(offset_of!(Bink, frame_rate) == 16);
    assert!(offset_of!(Bink, frame_rate_div) == 20);
    assert!(offset_of!(Bink, read_error) == 24);
    assert!(offset_of!(Bink, open_flags) == 28);
    assert!(offset_of!(Bink, frame_rects) == 52);
    assert!(offset_of!(Bink, num_rects) == 180);
    assert!(offset_of!(Bink, frame_buffers) == 184);
    assert!(size_of::<Bink>() == 0x400);
};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinkPlane {
    pub allocate: i32,
    pub buffer: *mut u8,
    pub buffer_pitch: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinkFramePlaneSet {
    pub y_plane: BinkPlane,
    pub cr_plane: BinkPlane,
    pub cb_plane: BinkPlane,
    pub a_plane: BinkPlane,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinkFrameBuffers {
    pub total_frames: i32,
    pub ya_buffer_width: u32,
    pub ya_buffer_height: u32,
    pub crcb_buffer_width: u32,
    pub crcb_buffer_height: u32,
    pub frame_num: u32,
    pub frames: [BinkFramePlaneSet; BINKMAXFRAMEBUFFERS],
}

const _: () = {
    assert!(size_of::<BinkPlane>() == 24);
    assert!(offset_of!(BinkPlane, buffer) == 8);
    assert!(offset_of!(BinkPlane, buffer_pitch) == 16);
    assert!(size_of::<BinkFramePlaneSet>() == 96);
    assert!(offset_of!(BinkFrameBuffers, frame_num) == 20);
    assert!(offset_of!(BinkFrameBuffers, frames) == 24);
    assert!(size_of::<BinkFrameBuffers>() == 216);
};

const EMPTY_PLANE: BinkPlane = BinkPlane {
    allocate: 0,
    buffer: core::ptr::null_mut(),
    buffer_pitch: 0,
};

impl BinkFrameBuffers {
    const fn zeroed() -> Self {
        let set = BinkFramePlaneSet {
            y_plane: EMPTY_PLANE,
            cr_plane: EMPTY_PLANE,
            cb_plane: EMPTY_PLANE,
            a_plane: EMPTY_PLANE,
        };
        Self {
            total_frames: 0,
            ya_buffer_width: 0,
            ya_buffer_height: 0,
            crcb_buffer_width: 0,
            crcb_buffer_height: 0,
            frame_num: 0,
            frames: [set; BINKMAXFRAMEBUFFERS],
        }
    }
}

// ==================== En-tête conteneur .bk2 / .bik ====================

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BinkHeader {
    pub tag: [u8; 4],
    pub file_size: u32,
    pub frames: u32,
    pub largest_frame_size: u32,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub video_flags: u32,
    pub audio_tracks: u32,
    /// Images clés (index 0 = image 1). Vide si la table d'index est absente/tronquée.
    pub keyframes: Vec<bool>,
}

const HEADER_FIXED: usize = 44;
const MAX_FRAMES: u32 = 1_000_000;
const MAX_AUDIO_TRACKS: u32 = 256;
const MAX_DIM: u32 = 16_384;

fn rd32(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Nombre d'octets nécessaires pour lire l'en-tête *et* la table d'index.
fn header_span(fixed: &[u8]) -> Option<usize> {
    let frames = rd32(fixed, 8)? as usize;
    let tracks = rd32(fixed, 40)? as usize;
    Some(HEADER_FIXED + 4 + tracks * 12 + (frames + 1) * 4)
}

/// Parse l'en-tête documenté du conteneur Bink (identique au démultiplexeur FFmpeg `bink.c`).
pub(crate) fn parse_header(b: &[u8]) -> Result<BinkHeader, &'static str> {
    if b.len() < HEADER_FIXED {
        return Err("fichier trop court pour un en-tête Bink");
    }
    let tag = [b[0], b[1], b[2], b[3]];
    let is_bink2 = &tag[..3] == b"KB2";
    let is_bink1 = &tag[..3] == b"BIK";
    if !is_bink1 && !is_bink2 {
        return Err("signature Bink invalide (attendu 'KB2' ou 'BIK')");
    }
    let rd = |o| rd32(b, o).ok_or("en-tête Bink tronqué");
    let file_size = rd(4)?.saturating_add(8);
    let frames = rd(8)?;
    let largest_frame_size = rd(12)?;
    let width = rd(20)?;
    let height = rd(24)?;
    let fps_num = rd(28)?;
    let fps_den = rd(32)?;
    let video_flags = rd(36)?;
    let audio_tracks = rd(40)?;
    if frames == 0 || frames > MAX_FRAMES {
        return Err("nombre d'images Bink invalide");
    }
    if largest_frame_size > file_size {
        return Err("taille de plus grande image supérieure à la taille du fichier");
    }
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return Err("dimensions Bink invalides");
    }
    if fps_num == 0 || fps_den == 0 {
        return Err("cadence Bink invalide");
    }
    if audio_tracks > MAX_AUDIO_TRACKS {
        return Err("nombre de pistes audio Bink invalide");
    }
    // Table d'index : (frames + 1) offsets, bit 0 = image clé.
    let mut off = HEADER_FIXED;
    if is_bink2 && tag[3] >= b'i' {
        off += 4;
    }
    off += audio_tracks as usize * 12;
    let mut keyframes = Vec::new();
    if b.len() >= off + (frames as usize + 1) * 4 {
        keyframes = (0..frames as usize)
            .map(|i| rd32(b, off + i * 4).is_some_and(|v| v & 1 != 0))
            .collect();
    }
    Ok(BinkHeader {
        tag,
        file_size,
        frames,
        largest_frame_size,
        width,
        height,
        fps_num,
        fps_den,
        video_flags,
        audio_tracks,
        keyframes,
    })
}

fn read_header_from_path(path: &PathBuf) -> Result<BinkHeader, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("ouverture impossible : {e}"))?;
    let mut fixed = vec![0u8; HEADER_FIXED];
    f.read_exact(&mut fixed)
        .map_err(|_| "fichier trop court pour un en-tête Bink".to_string())?;
    let span = header_span(&fixed).unwrap_or(HEADER_FIXED);
    let mut all = fixed;
    let mut rest = Vec::new();
    // Lecture bornée : l'index n'est jamais plus grand que ~4 Mio (MAX_FRAMES).
    let max_span = HEADER_FIXED + 4 + 256 * 12 + (MAX_FRAMES as usize + 1) * 4;
    let _ = f
        .take(span.min(max_span).saturating_sub(HEADER_FIXED) as u64)
        .read_to_end(&mut rest);
    all.extend_from_slice(&rest);
    parse_header(&all).map_err(str::to_string)
}

// ==================== État global ====================

struct BinkState {
    header: BinkHeader,
    frame_dur: Duration,
    /// Instant où l'image `base_frame` est due.
    base: Instant,
    base_frame: u32,
    paused_at: Option<Instant>,
    will_loop: bool,
    volumes: HashMap<u32, i32>,
    /// Frame buffers enregistrés par l'invité (pointeur conservé) ou alloués en interne.
    fb: usize,
    internal_fb: Option<Box<BinkFrameBuffers>>,
    internal_planes: Vec<Box<[u8]>>,
    filled: HashSet<usize>,
    async_pending: bool,
}

// SAFETY: les pointeurs bruts (frame buffers invités/internes) ne sont déréférencés que sous
// le verrou global `BINKS` ; l'état n'est jamais partagé hors de ce verrou.
unsafe impl Send for BinkState {}

impl BinkState {
    fn due(&self, frame: u32) -> Instant {
        let n = frame.saturating_sub(self.base_frame);
        self.base + self.frame_dur * n
    }
}

#[derive(Default)]
struct Globals {
    sound_open: usize,
    sound_param: usize,
    mem_alloc: usize,
    mem_free: usize,
    file_callbacks: [usize; 4],
    async_threads: HashSet<u32>,
}

static BINKS: LazyLock<Mutex<HashMap<usize, BinkState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static GLOBALS: LazyLock<Mutex<Globals>> = LazyLock::new(|| Mutex::new(Globals::default()));
static ERROR_TEXT: Mutex<[u8; 256]> = Mutex::new([0u8; 256]);

fn set_error(msg: &str) {
    sb_debug!(CHANNEL, "erreur Bink : {msg}");
    let mut buf = ERROR_TEXT.lock();
    let bytes = msg.as_bytes();
    let n = bytes.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&bytes[..n]);
    buf[n] = 0;
}

fn align_up(v: u32, a: u32) -> u32 {
    v.div_ceil(a) * a
}

/// Remplit un plan de `rows` lignes de `width` octets à `value` (stride `pitch`).
///
/// # Safety
/// `buf` doit être valide en écriture pour `pitch * rows` octets.
unsafe fn fill_plane(buf: *mut u8, pitch: u32, width: u32, rows: u32, value: u8) {
    if buf.is_null() {
        return;
    }
    let pitch = if pitch == 0 { width } else { pitch } as usize;
    let w = (width as usize).min(pitch);
    for r in 0..rows as usize {
        // SAFETY: garanti par l'appelant (`pitch * rows` octets écrivables).
        unsafe { core::ptr::write_bytes(buf.add(r * pitch), value, w) };
    }
}

fn plane_info(st: &BinkState) -> (u32, u32, u32, u32) {
    let yw = align_up(st.header.width, 32);
    let yh = align_up(st.header.height, 32);
    (yw, yh, yw / 2, yh / 2)
}

fn fill_info(st: &BinkState, out: &mut BinkFrameBuffers, alpha: bool) {
    let (yw, yh, cw, ch) = plane_info(st);
    *out = BinkFrameBuffers::zeroed();
    out.total_frames = BINKMAXFRAMEBUFFERS as i32;
    out.ya_buffer_width = yw;
    out.ya_buffer_height = yh;
    out.crcb_buffer_width = cw;
    out.crcb_buffer_height = ch;
    out.frame_num = 0;
    for set in &mut out.frames {
        set.y_plane = BinkPlane {
            allocate: 1,
            buffer: core::ptr::null_mut(),
            buffer_pitch: yw,
        };
        set.cr_plane = BinkPlane {
            allocate: 1,
            buffer: core::ptr::null_mut(),
            buffer_pitch: cw,
        };
        set.cb_plane = set.cr_plane;
        set.a_plane = BinkPlane {
            allocate: i32::from(alpha),
            buffer: core::ptr::null_mut(),
            buffer_pitch: if alpha { yw } else { 0 },
        };
    }
}

/// « Décode » l'image courante : noir dans le prochain frame buffer, qui devient le courant.
fn render_black(st: &mut BinkState) {
    if st.fb == 0 {
        return;
    }
    let fb = st.fb as *mut BinkFrameBuffers;
    // SAFETY: `fb` est soit notre buffer interne, soit la struct enregistrée par l'invité,
    // qui doit rester valide tant que le HBINK est ouvert (contrat BinkRegisterFrameBuffers).
    unsafe {
        let total = (*fb).total_frames.clamp(1, BINKMAXFRAMEBUFFERS as i32) as u32;
        let next = ((*fb).frame_num + 1) % total;
        let set = (*fb).frames[next as usize];
        let (yh, ch) = ((*fb).ya_buffer_height, (*fb).crcb_buffer_height);
        let (yw, cw) = ((*fb).ya_buffer_width, (*fb).crcb_buffer_width);
        if st.filled.insert(set.y_plane.buffer as usize) {
            fill_plane(set.y_plane.buffer, set.y_plane.buffer_pitch, yw, yh, 16);
        }
        if st.filled.insert(set.cr_plane.buffer as usize) {
            fill_plane(set.cr_plane.buffer, set.cr_plane.buffer_pitch, cw, ch, 128);
        }
        if st.filled.insert(set.cb_plane.buffer as usize) {
            fill_plane(set.cb_plane.buffer, set.cb_plane.buffer_pitch, cw, ch, 128);
        }
        if st.filled.insert(set.a_plane.buffer as usize) {
            fill_plane(set.a_plane.buffer, set.a_plane.buffer_pitch, yw, yh, 255);
        }
        (*fb).frame_num = next;
    }
}

fn with_bink<R>(bink: *mut Bink, f: impl FnOnce(&mut Bink, &mut BinkState) -> R) -> Option<R> {
    if bink.is_null() {
        return None;
    }
    let mut map = BINKS.lock();
    let st = map.get_mut(&(bink as usize))?;
    // SAFETY: `bink` est une clé de `BINKS`, donc un `Box<Bink>` vivant alloué par `open_impl`.
    let b = unsafe { &mut *bink };
    Some(f(b, st))
}

fn open_impl(name: *const c_char, flags: u32) -> *mut Bink {
    let header = if flags & BINKFROMMEMORY != 0 {
        if name.is_null() {
            set_error("BinkOpen : pointeur mémoire NULL");
            return core::ptr::null_mut();
        }
        // SAFETY: BINKFROMMEMORY — `name` pointe sur l'image mémoire complète du fichier.
        let fixed = unsafe { core::slice::from_raw_parts(name.cast::<u8>(), HEADER_FIXED) };
        let total = rd32(fixed, 4).map_or(0, |s| s as usize + 8);
        let span = header_span(fixed).unwrap_or(HEADER_FIXED).min(total);
        // SAFETY: le fichier en mémoire fait `total` octets (champ taille de l'en-tête).
        let bytes = unsafe { core::slice::from_raw_parts(name.cast::<u8>(), span) };
        parse_header(bytes).map_err(str::to_string)
    } else {
        let path = if flags & BINKFILEHANDLE != 0 {
            crate::file_io::file_handle_path(name as usize)
                .ok_or_else(|| "BinkOpen : handle de fichier invalide".to_string())
        } else if name.is_null() {
            Err("BinkOpen : nom de fichier NULL".to_string())
        } else {
            // SAFETY: `name` est une chaîne C terminée par NUL (contrat BinkOpen).
            let guest = unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned();
            super::resolve_guest_path(&guest)
                .ok_or_else(|| format!("fichier '{guest}' introuvable (VFS)"))
        };
        match path {
            Ok(p) => read_header_from_path(&p),
            Err(e) => Err(e),
        }
    };
    let header = match header {
        Ok(h) => h,
        Err(e) => {
            set_error(&e);
            return core::ptr::null_mut();
        }
    };

    let nanos = u128::from(header.fps_den) * 1_000_000_000 / u128::from(header.fps_num);
    let frame_dur = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX).max(1));
    sb_info!(
        CHANNEL,
        "BinkOpen {}x{} {} images @ {}/{} (lecture noire, codec propriétaire)",
        header.width,
        header.height,
        header.frames,
        header.fps_num,
        header.fps_den
    );

    let mut b = Box::new(Bink {
        width: header.width,
        height: header.height,
        frames: header.frames,
        frame_num: 1,
        frame_rate: header.fps_num,
        frame_rate_div: header.fps_den,
        read_error: 0,
        open_flags: flags,
        bink_type: header.video_flags,
        largest_frame_size: header.largest_frame_size,
        frame_size: 0,
        snd_size: 0,
        frame_change_percent: 0,
        frame_rects: [BinkRect::default(); BINKMAXDIRTYRECTS],
        num_rects: 0,
        frame_buffers: core::ptr::null_mut(),
        reserved: [0; 0x400 - 192],
    });
    let mut st = BinkState {
        header,
        frame_dur,
        base: Instant::now(),
        base_frame: 1,
        paused_at: None,
        will_loop: false,
        volumes: HashMap::new(),
        fb: 0,
        internal_fb: None,
        internal_planes: Vec::new(),
        filled: HashSet::new(),
        async_pending: false,
    };
    if flags & BINKNOFRAMEBUFFERS == 0 {
        // Bink alloue lui-même ses frame buffers.
        let mut fbs = Box::new(BinkFrameBuffers::zeroed());
        fill_info(&st, &mut fbs, flags & BINKALPHA != 0);
        let (yh, ch) = (fbs.ya_buffer_height, fbs.crcb_buffer_height);
        for set in &mut fbs.frames {
            for (plane, rows) in [
                (&mut set.y_plane, yh),
                (&mut set.cr_plane, ch),
                (&mut set.cb_plane, ch),
                (&mut set.a_plane, yh),
            ] {
                if plane.allocate != 0 {
                    let mut mem =
                        vec![0u8; plane.buffer_pitch as usize * rows as usize].into_boxed_slice();
                    plane.buffer = mem.as_mut_ptr();
                    st.internal_planes.push(mem);
                }
            }
        }
        // Image 0 = dernier buffer affiché ; DoFrame écrit le suivant.
        fbs.frame_num = (BINKMAXFRAMEBUFFERS - 1) as u32;
        st.fb = (&raw mut *fbs) as usize;
        b.frame_buffers = &raw mut *fbs;
        st.internal_fb = Some(fbs);
    }
    let ptr = Box::into_raw(b);
    BINKS.lock().insert(ptr as usize, st);
    ptr
}

// ==================== Exports ====================

pub(crate) extern "win64" fn BinkOpen(name: *const c_char, flags: u32) -> *mut Bink {
    ffi_guard!(CHANNEL, "BinkOpen", core::ptr::null_mut(), {
        open_impl(name, flags)
    })
}

pub(crate) extern "win64" fn BinkOpenWithOptions(
    name: *const c_char,
    _options: *const c_void,
    flags: u32,
) -> *mut Bink {
    ffi_guard!(CHANNEL, "BinkOpenWithOptions", core::ptr::null_mut(), {
        open_impl(name, flags)
    })
}

pub(crate) extern "win64" fn BinkGetError() -> *const c_char {
    ffi_guard!(CHANNEL, "BinkGetError", core::ptr::null(), {
        ERROR_TEXT.lock().as_ptr().cast::<c_char>()
    })
}

pub(crate) extern "win64" fn BinkClose(bink: *mut Bink) {
    ffi_guard!(CHANNEL, "BinkClose", (), {
        if bink.is_null() {
            return;
        }
        if BINKS.lock().remove(&(bink as usize)).is_some() {
            // SAFETY: le pointeur provenait de `Box::into_raw` et vient d'être retiré du registre.
            drop(unsafe { Box::from_raw(bink) });
        }
    })
}

pub(crate) extern "win64" fn BinkDoFrame(bink: *mut Bink) -> i32 {
    ffi_guard!(CHANNEL, "BinkDoFrame", 0, {
        with_bink(bink, |b, st| {
            render_black(st);
            b.frame_size = 0;
            b.frame_change_percent = 0;
            b.num_rects = 0;
            0
        })
        .unwrap_or(0)
    })
}

pub(crate) extern "win64" fn BinkNextFrame(bink: *mut Bink) {
    ffi_guard!(CHANNEL, "BinkNextFrame", (), {
        with_bink(bink, |b, st| {
            if b.frame_num >= b.frames {
                // Bouclage vers l'image 1 en conservant une horloge continue.
                st.base = st.due(b.frames + 1);
                st.base_frame = 1;
                b.frame_num = 1;
            } else {
                b.frame_num += 1;
            }
        });
    })
}

pub(crate) extern "win64" fn BinkWait(bink: *mut Bink) -> i32 {
    ffi_guard!(CHANNEL, "BinkWait", 0, {
        with_bink(bink, |b, st| {
            if st.paused_at.is_some() || Instant::now() < st.due(b.frame_num) {
                1
            } else {
                0
            }
        })
        .unwrap_or(0)
    })
}

pub(crate) extern "win64" fn BinkShouldSkip(bink: *mut Bink) -> i32 {
    ffi_guard!(CHANNEL, "BinkShouldSkip", 0, {
        with_bink(bink, |b, st| {
            i32::from(
                st.paused_at.is_none()
                    && b.open_flags & 0x0008_0000 == 0 // BINKNOSKIP
                    && Instant::now() > st.due(b.frame_num) + st.frame_dur,
            )
        })
        .unwrap_or(0)
    })
}

pub(crate) extern "win64" fn BinkGoto(bink: *mut Bink, frame: u32, _flags: i32) {
    ffi_guard!(CHANNEL, "BinkGoto", (), {
        with_bink(bink, |b, st| {
            let f = frame.clamp(1, b.frames);
            b.frame_num = f;
            let now = Instant::now();
            st.base = now;
            st.base_frame = f;
            if st.paused_at.is_some() {
                st.paused_at = Some(now);
            }
        });
    })
}

pub(crate) extern "win64" fn BinkPause(bink: *mut Bink, pause: i32) -> i32 {
    ffi_guard!(CHANNEL, "BinkPause", 0, {
        with_bink(bink, |_, st| {
            let now = Instant::now();
            match (pause != 0, st.paused_at) {
                (true, None) => st.paused_at = Some(now),
                (false, Some(at)) => {
                    st.base += now - at;
                    st.paused_at = None;
                }
                _ => {}
            }
            i32::from(st.paused_at.is_some())
        })
        .unwrap_or(0)
    })
}

pub(crate) extern "win64" fn BinkGetKeyFrame(bink: *mut Bink, frame: u32, flags: i32) -> u32 {
    ffi_guard!(CHANNEL, "BinkGetKeyFrame", 0, {
        with_bink(bink, |b, st| {
            let total = b.frames;
            let is_key = |f: u32| -> bool {
                if f == 1 {
                    return true;
                }
                st.header
                    .keyframes
                    .get(f as usize - 1)
                    .copied()
                    .unwrap_or(false)
            };
            let f = frame.clamp(1, total);
            let not_equal = flags & BINKGETKEYNOTEQUAL != 0;
            let prev = || {
                let start = if not_equal { f.checked_sub(1)? } else { f };
                (1..=start).rev().find(|&i| is_key(i))
            };
            let next = || {
                let start = if not_equal { f + 1 } else { f };
                (start..=total).find(|&i| is_key(i))
            };
            let r = match flags & !BINKGETKEYNOTEQUAL {
                BINKGETKEYPREVIOUS => prev(),
                BINKGETKEYNEXT => next(),
                BINKGETKEYCLOSEST => match (prev(), next()) {
                    (Some(p), Some(n)) => Some(if f - p <= n - f { p } else { n }),
                    (p, n) => p.or(n),
                },
                _ => None,
            };
            r.unwrap_or(0)
        })
        .unwrap_or(0)
    })
}

pub(crate) extern "win64" fn BinkSetVolume(bink: *mut Bink, track: u32, volume: i32) {
    ffi_guard!(CHANNEL, "BinkSetVolume", (), {
        with_bink(bink, |_, st| {
            st.volumes.insert(track, volume.clamp(0, 65536));
        });
    })
}

pub(crate) extern "win64" fn BinkSetWillLoop(bink: *mut Bink, onoff: i32) {
    ffi_guard!(CHANNEL, "BinkSetWillLoop", (), {
        with_bink(bink, |_, st| st.will_loop = onoff != 0);
    })
}

pub(crate) extern "win64" fn BinkSetSoundSystem(open: *const c_void, param: usize) -> i32 {
    ffi_guard!(CHANNEL, "BinkSetSoundSystem", 0, {
        if open.is_null() {
            set_error("BinkSetSoundSystem : fonction d'ouverture NULL");
            return 0;
        }
        let mut g = GLOBALS.lock();
        g.sound_open = open as usize;
        g.sound_param = param;
        1
    })
}

/// `BINKSNDOPEN BinkOpenXAudio2(UINTa param)` et variantes : aucun pilote son Bink n'est fourni
/// (le flux audio Bink est propriétaire) — retourne NULL, sans faux succès.
pub(crate) extern "win64" fn BinkOpenXAudio2(_param: usize) -> *const c_void {
    ffi_guard!(CHANNEL, "BinkOpenXAudio2", core::ptr::null(), {
        core::ptr::null()
    })
}

pub(crate) extern "win64" fn BinkSetMemory(alloc: *const c_void, free: *const c_void) {
    ffi_guard!(CHANNEL, "BinkSetMemory", (), {
        let mut g = GLOBALS.lock();
        g.mem_alloc = alloc as usize;
        g.mem_free = free as usize;
    })
}

pub(crate) extern "win64" fn BinkSetOSFileCallbacks(
    open: *const c_void,
    read: *const c_void,
    seek: *const c_void,
    close: *const c_void,
) {
    ffi_guard!(CHANNEL, "BinkSetOSFileCallbacks", (), {
        GLOBALS.lock().file_callbacks =
            [open as usize, read as usize, seek as usize, close as usize];
    })
}

pub(crate) extern "win64" fn BinkFreeGlobals() {
    ffi_guard!(CHANNEL, "BinkFreeGlobals", (), {
        *GLOBALS.lock() = Globals::default();
    })
}

pub(crate) extern "win64" fn BinkGetFrameBuffersInfo(bink: *mut Bink, out: *mut BinkFrameBuffers) {
    ffi_guard!(CHANNEL, "BinkGetFrameBuffersInfo", (), {
        if out.is_null() {
            return;
        }
        with_bink(bink, |b, st| {
            // SAFETY: `out` vérifié non-nul, pointe sur un BINKFRAMEBUFFERS de l'invité.
            let out = unsafe { &mut *out };
            fill_info(st, out, b.open_flags & BINKALPHA != 0);
        });
    })
}

pub(crate) extern "win64" fn BinkRegisterFrameBuffers(bink: *mut Bink, fbs: *mut BinkFrameBuffers) {
    ffi_guard!(CHANNEL, "BinkRegisterFrameBuffers", (), {
        if fbs.is_null() {
            return;
        }
        with_bink(bink, |b, st| {
            st.internal_fb = None;
            st.internal_planes.clear();
            st.filled.clear();
            st.fb = fbs as usize;
            b.frame_buffers = fbs;
            // SAFETY: `fbs` vérifié non-nul (struct de l'invité, conservée par Bink).
            unsafe {
                let total = (*fbs).total_frames.clamp(1, BINKMAXFRAMEBUFFERS as i32) as u32;
                // Le prochain DoFrame écrit le buffer indiqué par FrameNum + 1 (mod total).
                (*fbs).frame_num = (*fbs).frame_num.min(total - 1);
            }
        });
    })
}

pub(crate) extern "win64" fn BinkGetPlatformInfo(_what: i32, _out: *mut c_void) -> i32 {
    ffi_guard!(CHANNEL, "BinkGetPlatformInfo", 0, {
        set_error("BinkGetPlatformInfo : requête plateforme non prise en charge");
        0
    })
}

pub(crate) extern "win64" fn BinkDoFrameAsyncMulti(
    bink: *mut Bink,
    _threads: *const u32,
    _count: i32,
) -> i32 {
    ffi_guard!(CHANNEL, "BinkDoFrameAsyncMulti", 0, {
        // Décodage (noir) synchrone : l'image est prête dès le retour.
        with_bink(bink, |b, st| {
            render_black(st);
            b.frame_size = 0;
            st.async_pending = true;
            1
        })
        .unwrap_or(0)
    })
}

pub(crate) extern "win64" fn BinkDoFrameAsyncWait(bink: *mut Bink, _us: i32) -> i32 {
    ffi_guard!(CHANNEL, "BinkDoFrameAsyncWait", 0, {
        with_bink(bink, |_, st| {
            st.async_pending = false;
            1
        })
        .unwrap_or(0)
    })
}

pub(crate) extern "win64" fn BinkStartAsyncThread(thread: i32, _param: *const c_void) -> i32 {
    ffi_guard!(CHANNEL, "BinkStartAsyncThread", 0, {
        if thread < 0 {
            return 0;
        }
        GLOBALS.lock().async_threads.insert(thread as u32);
        1
    })
}

fn thread_list(threads: *const u32, count: i32) -> Vec<u32> {
    if threads.is_null() || count <= 0 {
        return Vec::new();
    }
    // SAFETY: `threads` désigne `count` indices de threads (contrat Bink).
    unsafe { core::slice::from_raw_parts(threads, count as usize) }.to_vec()
}

pub(crate) extern "win64" fn BinkRequestStopAsyncThreadsMulti(
    threads: *const u32,
    count: i32,
) -> i32 {
    ffi_guard!(CHANNEL, "BinkRequestStopAsyncThreadsMulti", 0, {
        let list = thread_list(threads, count);
        let g = GLOBALS.lock();
        i32::from(list.iter().all(|t| g.async_threads.contains(t)))
    })
}

pub(crate) extern "win64" fn BinkWaitStopAsyncThreadsMulti(threads: *const u32, count: i32) -> i32 {
    ffi_guard!(CHANNEL, "BinkWaitStopAsyncThreadsMulti", 0, {
        let list = thread_list(threads, count);
        let mut g = GLOBALS.lock();
        for t in &list {
            g.async_threads.remove(t);
        }
        1
    })
}

pub(crate) fn exports() -> HashMap<&'static str, usize> {
    let mut m: HashMap<&'static str, usize> = HashMap::new();
    m.insert("BinkOpen", BinkOpen as *const () as usize);
    m.insert(
        "BinkOpenWithOptions",
        BinkOpenWithOptions as *const () as usize,
    );
    m.insert("BinkGetError", BinkGetError as *const () as usize);
    m.insert("BinkClose", BinkClose as *const () as usize);
    m.insert("BinkDoFrame", BinkDoFrame as *const () as usize);
    m.insert("BinkNextFrame", BinkNextFrame as *const () as usize);
    m.insert("BinkWait", BinkWait as *const () as usize);
    m.insert("BinkGoto", BinkGoto as *const () as usize);
    m.insert("BinkPause", BinkPause as *const () as usize);
    m.insert("BinkGetKeyFrame", BinkGetKeyFrame as *const () as usize);
    m.insert("BinkShouldSkip", BinkShouldSkip as *const () as usize);
    m.insert("BinkSetVolume", BinkSetVolume as *const () as usize);
    m.insert("BinkSetWillLoop", BinkSetWillLoop as *const () as usize);
    m.insert(
        "BinkSetSoundSystem",
        BinkSetSoundSystem as *const () as usize,
    );
    m.insert("BinkOpenXAudio2", BinkOpenXAudio2 as *const () as usize);
    m.insert("BinkOpenXAudio27", BinkOpenXAudio2 as *const () as usize);
    m.insert("BinkOpenXAudio28", BinkOpenXAudio2 as *const () as usize);
    m.insert("BinkOpenXAudio29", BinkOpenXAudio2 as *const () as usize);
    m.insert("BinkSetMemory", BinkSetMemory as *const () as usize);
    m.insert(
        "BinkSetOSFileCallbacks",
        BinkSetOSFileCallbacks as *const () as usize,
    );
    m.insert("BinkFreeGlobals", BinkFreeGlobals as *const () as usize);
    m.insert(
        "BinkGetFrameBuffersInfo",
        BinkGetFrameBuffersInfo as *const () as usize,
    );
    m.insert(
        "BinkRegisterFrameBuffers",
        BinkRegisterFrameBuffers as *const () as usize,
    );
    m.insert(
        "BinkGetPlatformInfo",
        BinkGetPlatformInfo as *const () as usize,
    );
    m.insert(
        "BinkDoFrameAsyncMulti",
        BinkDoFrameAsyncMulti as *const () as usize,
    );
    m.insert(
        "BinkDoFrameAsyncWait",
        BinkDoFrameAsyncWait as *const () as usize,
    );
    m.insert(
        "BinkStartAsyncThread",
        BinkStartAsyncThread as *const () as usize,
    );
    m.insert(
        "BinkRequestStopAsyncThreadsMulti",
        BinkRequestStopAsyncThreadsMulti as *const () as usize,
    );
    m.insert(
        "BinkWaitStopAsyncThreadsMulti",
        BinkWaitStopAsyncThreadsMulti as *const () as usize,
    );
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    /// En-tête `.bk2` synthétique : `frames` images, image clé toutes les 4 images.
    fn synth_bk2(w: u32, h: u32, frames: u32, fps: u32, div: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"KB2j");
        v.extend_from_slice(&0u32.to_le_bytes()); // taille - 8 (corrigée plus bas)
        v.extend_from_slice(&frames.to_le_bytes());
        v.extend_from_slice(&16u32.to_le_bytes()); // plus grande image
        v.extend_from_slice(&frames.to_le_bytes());
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&fps.to_le_bytes());
        v.extend_from_slice(&div.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // flags vidéo
        v.extend_from_slice(&0u32.to_le_bytes()); // pistes audio
        v.extend_from_slice(&0u32.to_le_bytes()); // révision >= 'i'
        let data_start = (v.len() + (frames as usize + 1) * 4) as u32;
        for i in 0..=frames {
            let key = u32::from(i % 4 == 0 && i < frames);
            v.extend_from_slice(&((data_start + i * 16) | key).to_le_bytes());
        }
        v.resize(v.len() + frames as usize * 16, 0);
        let sz = (v.len() - 8) as u32;
        v[4..8].copy_from_slice(&sz.to_le_bytes());
        v
    }

    #[test]
    fn parse_rejects_garbage_and_reads_dimensions() {
        assert!(parse_header(b"not a bink file at all, nope nope nope nope!!").is_err());
        let h = parse_header(&synth_bk2(1280, 720, 10, 30, 1)).expect("en-tête");
        assert_eq!((h.width, h.height, h.frames), (1280, 720, 10));
        assert_eq!(h.keyframes.len(), 10);
        assert!(h.keyframes[0] && h.keyframes[4] && !h.keyframes[1]);
    }

    #[test]
    fn open_missing_file_returns_null_with_error() {
        super::super::tests_vfs::install("bink-missing");
        let name = CString::new("movies/absent.bk2").expect("cstr");
        let b = BinkOpen(name.as_ptr(), 0);
        assert!(b.is_null());
        // SAFETY: BinkGetError retourne une chaîne C statique.
        let err = unsafe { CStr::from_ptr(BinkGetError()) }
            .to_string_lossy()
            .into_owned();
        // Tampon d'erreur global (comme Bink) : un autre test peut l'écraser en parallèle.
        assert!(!err.is_empty());
    }

    #[test]
    fn open_play_to_end_with_registered_black_buffers() {
        let root = super::super::tests_vfs::install("bink-play");
        std::fs::create_dir_all(root.join("movies")).expect("mkdir");
        std::fs::write(
            root.join("movies/intro.bk2"),
            synth_bk2(64, 48, 12, 1000, 1),
        )
        .expect("write");
        let name = CString::new("movies/intro.bk2").expect("cstr");
        let b = BinkOpen(name.as_ptr(), BINKNOFRAMEBUFFERS);
        assert!(!b.is_null());
        // SAFETY: HBINK valide retourné par BinkOpen.
        let (w, h, frames, num, rate, div) = unsafe {
            (
                (*b).width,
                (*b).height,
                (*b).frames,
                (*b).frame_num,
                (*b).frame_rate,
                (*b).frame_rate_div,
            )
        };
        assert_eq!((w, h, frames, num, rate, div), (64, 48, 12, 1, 1000, 1));

        let mut info = BinkFrameBuffers::zeroed();
        BinkGetFrameBuffersInfo(b, &mut info);
        assert_eq!(info.total_frames, 2);
        assert_eq!((info.ya_buffer_width, info.ya_buffer_height), (64, 64));
        let ysz = (info.ya_buffer_width * info.ya_buffer_height) as usize;
        let csz = (info.crcb_buffer_width * info.crcb_buffer_height) as usize;
        let mut planes: Vec<Vec<u8>> = (0..2)
            .flat_map(|_| [vec![0xAAu8; ysz], vec![0xAA; csz], vec![0xAA; csz]])
            .collect();
        for i in 0..2 {
            info.frames[i].y_plane.buffer = planes[i * 3].as_mut_ptr();
            info.frames[i].cr_plane.buffer = planes[i * 3 + 1].as_mut_ptr();
            info.frames[i].cb_plane.buffer = planes[i * 3 + 2].as_mut_ptr();
        }
        BinkRegisterFrameBuffers(b, &mut info);

        assert_eq!(BinkGetKeyFrame(b, 3, BINKGETKEYPREVIOUS), 1);
        assert_eq!(BinkGetKeyFrame(b, 3, BINKGETKEYNEXT), 5);
        assert_eq!(
            BinkGetKeyFrame(b, 5, BINKGETKEYNEXT | BINKGETKEYNOTEQUAL),
            9
        );

        let start = Instant::now();
        let mut decoded = 0;
        loop {
            if BinkWait(b) == 0 {
                assert_eq!(BinkDoFrame(b), 0);
                decoded += 1;
                // SAFETY: HBINK valide.
                if unsafe { (*b).frame_num == (*b).frames } {
                    break;
                }
                BinkNextFrame(b);
            } else {
                std::thread::sleep(Duration::from_micros(200));
            }
            assert!(start.elapsed() < Duration::from_secs(5), "lecture bloquée");
        }
        assert_eq!(decoded, 12);
        // 12 images à 1000 i/s : au moins ~11 ms de lecture réelle.
        assert!(start.elapsed() >= Duration::from_millis(10));
        for (i, p) in planes.iter().enumerate() {
            let want = if i % 3 == 0 { 16 } else { 128 };
            assert!(p.iter().all(|&v| v == want), "plan {i} non noir");
        }
        // Bouclage après la dernière image.
        BinkNextFrame(b);
        // SAFETY: HBINK valide.
        assert_eq!(unsafe { (*b).frame_num }, 1);
        assert_eq!(BinkPause(b, 1), 1);
        assert_eq!(BinkWait(b), 1);
        assert_eq!(BinkPause(b, 0), 0);
        BinkGoto(b, 7, 0);
        // SAFETY: HBINK valide.
        assert_eq!(unsafe { (*b).frame_num }, 7);
        BinkClose(b);
        assert_eq!(BinkWait(b), 0); // handle fermé : ignoré
    }

    #[test]
    fn internal_buffers_and_async_paths() {
        let data = synth_bk2(32, 32, 3, 30, 1);
        let b = BinkOpen(data.as_ptr().cast(), BINKFROMMEMORY);
        assert!(!b.is_null());
        // SAFETY: HBINK valide ; buffers internes alloués (pas de BINKNOFRAMEBUFFERS).
        let fb = unsafe { (*b).frame_buffers };
        assert!(!fb.is_null());
        assert_eq!(BinkStartAsyncThread(0, core::ptr::null()), 1);
        assert_eq!(BinkDoFrameAsyncMulti(b, [0u32].as_ptr(), 1), 1);
        assert_eq!(BinkDoFrameAsyncWait(b, -1), 1);
        // SAFETY: buffers internes valides tant que le HBINK est ouvert.
        unsafe {
            let cur = (*fb).frames[(*fb).frame_num as usize];
            assert_eq!(*cur.y_plane.buffer, 16);
            assert_eq!(*cur.cr_plane.buffer, 128);
        }
        assert_eq!(BinkRequestStopAsyncThreadsMulti([0u32].as_ptr(), 1), 1);
        assert_eq!(BinkWaitStopAsyncThreadsMulti([0u32].as_ptr(), 1), 1);
        assert_eq!(BinkGetPlatformInfo(0, core::ptr::null_mut()), 0);
        assert_eq!(BinkSetSoundSystem(core::ptr::null(), 0), 0);
        BinkSetVolume(b, 0, 32768);
        BinkSetWillLoop(b, 1);
        BinkClose(b);
        BinkFreeGlobals();
    }
}
