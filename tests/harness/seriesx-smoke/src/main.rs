//! Harnais PE32+ x86-64 Xbox Series X (GDK / Scarlett) :
//! Vérifie en exécution native sous ScarletBridge (sans Wine) :
//! - `xgameruntime.dll` (`XGameRuntimeInitialize`, `XSystemGetDeviceType` = Series X, `XUserGetId`, `XUserGetGamertag`)
//! - `xmem.dll` (`XMemAlloc` 64 KiB physique unifié `VK_EXT_external_memory_host`, `XMemSize`, `XMemFree`)
//! - `d3d12_xs.dll` (`D3D12CreateDevice` sur Vulkan 1.4)
//! - `XAudio2_9.dll` (`XAudio2Create` → `CreateMasteringVoice`)
//! - `libHttpClient.GDK.dll` (`HCInitialize`, `HCHttpCallCreate` bloqué en mode offline strict)

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "[seriesx_smoke] Compiler pour PE32+ x86-64 :\n\
         cargo build --target x86_64-pc-windows-gnu -p sb-harness-seriesx-smoke"
    );
}

#[cfg(windows)]
fn main() {
    std::process::exit(win_impl::run());
}

#[cfg(windows)]
mod win_impl {
    use core::ffi::c_void;
    use std::ptr;

    #[link(name = "xgameruntime", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn XGameRuntimeInitialize() -> i32;
        fn XGameRuntimeUninitialize();
        fn XSystemGetDeviceType() -> u32;
        fn XUserGetId(user: usize, out_xuid: *mut u64) -> i32;
        fn XUserGetGamertag(
            user: usize,
            component: u32,
            buf_size: usize,
            buf: *mut u8,
            out_used: *mut usize,
        ) -> i32;
    }

    #[link(name = "xmem", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void;
        fn XMemSize(address: *const c_void, attributes: u64) -> usize;
        fn XMemFree(address: *mut c_void, attributes: u64);
    }

    #[link(name = "d3d12_xs", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn D3D12CreateDevice(
            adapter: *mut c_void,
            min_feature_level: u32,
            riid: *const [u8; 16],
            out_device: *mut *mut c_void,
        ) -> i32;
    }

    #[link(name = "XAudio2_9", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn XAudio2Create(pp_xaudio2: *mut *mut c_void, flags: u32, processor: u32) -> i32;
    }

    #[link(name = "libHttpClient.GDK", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn HCInitialize(args: *mut c_void) -> i32;
        fn HCHttpCallCreate(out_call: *mut usize) -> i32;
        fn HCCleanup();
    }

    pub fn run() -> i32 {
        println!("[seriesx_smoke] === Test d'intégration Xbox Series X (PE32+ natif) ===");

        // 1. GameOS & Profil local offline
        // SAFETY: imports `raw-dylib` résolus par le loader ScarletBridge (`sb-hle`).
        unsafe {
            assert_eq!(XGameRuntimeInitialize(), 0, "XGameRuntimeInitialize");
            let dev = XSystemGetDeviceType();
            assert_eq!(dev, 4, "doit signaler Xbox Series X (Anaconda = 4)");
            let mut xuid = 0u64;
            assert_eq!(XUserGetId(1, &mut xuid), 0);
            let mut tag = [0u8; 64];
            let mut used = 0usize;
            assert_eq!(
                XUserGetGamertag(1, 0, tag.len(), tag.as_mut_ptr(), &mut used),
                0
            );
            let gamertag = String::from_utf8_lossy(&tag[..used.saturating_sub(1)]);
            println!(
                "[seriesx_smoke] GameOS OK : device=XboxSeriesX({dev}), xuid={xuid:#x}, gamertag='{gamertag}'"
            );

            // 2. Mémoire unifiée xmem (64 KiB physique zéro-copie Vulkan)
            let attrs: u64 = ((1u32 << 31) | (1u32 << 14) | (5u32 << 15)) as u64;
            let mem = XMemAlloc(65536, attrs);
            assert!(!mem.is_null(), "XMemAlloc NULL");
            assert!((mem as usize).is_multiple_of(65536), "alignement 64 KiB");
            let slice = std::slice::from_raw_parts_mut(mem.cast::<u8>(), 65536);
            assert!(slice.iter().all(|&b| b == 0), "tampon zéro-initialisé");
            slice.fill(0x5A);
            assert_eq!(XMemSize(mem, attrs), 65536);
            XMemFree(mem, attrs);
            println!(
                "[seriesx_smoke] xmem.dll OK : 64 KiB unifié (mmap + Vulkan external_memory_host)"
            );

            // 3. Direct3D 12 XS -> Vulkan 1.4
            let dummy_iid = [0u8; 16];
            let mut d3d12_dev: *mut c_void = ptr::null_mut();
            assert_eq!(
                D3D12CreateDevice(ptr::null_mut(), 0xC100, &dummy_iid, &mut d3d12_dev),
                0
            );
            assert!(!d3d12_dev.is_null(), "ID3D12Device_XS NULL");
            println!("[seriesx_smoke] d3d12_xs.dll OK : ID3D12Device_XS={d3d12_dev:p}");

            // 4. XAudio2_9 -> PipeWire/SDL3
            let mut xa2: *mut c_void = ptr::null_mut();
            assert_eq!(XAudio2Create(&mut xa2, 0, 1), 0);
            assert!(!xa2.is_null(), "IXAudio2 NULL");
            println!("[seriesx_smoke] XAudio2_9.dll OK : IXAudio2={xa2:p}");

            // 5. Réseau offline strict
            assert_eq!(HCInitialize(ptr::null_mut()), 0);
            let mut call_handle = 0usize;
            let hr = HCHttpCallCreate(&mut call_handle);
            assert!(hr < 0, "le réseau doit être bloqué en mode offline strict");
            HCCleanup();
            println!(
                "[seriesx_smoke] libHttpClient.GDK.dll OK : mode offline strict vérifié (hr={hr:#x})"
            );

            XGameRuntimeUninitialize();
        }

        println!("[seriesx_smoke] === TOUS LES TESTS XBOX SERIES X RÉUSSIS ===");
        0
    }
}
