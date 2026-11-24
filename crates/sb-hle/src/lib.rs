//! Implémentations HLE natives Rust (`extern "win64"`) pour ScarletBridge (sans Wine) :
//! - `kernel32` / `kernelbase` / `api-ms-win-core-*` / `ntdll` / `bcryptprimitives`
//! - `ucrt` (`api-ms-win-crt-*`) & `vcruntime140*`
//! - `msvcp140*`, `vccorlib140_app`, WinRT (`CoreApplication`, `CoreWindow`, `ApplicationData`, `Gamepad`)
//! - `XAudio2_9` → SDL3/PipeWire, `d3d11` → Vulkan 1.4 (`sb-gpu`)
//! - Fondations Xbox Series X / Scarlett (`xmem`, `xgameruntime`, `d3d12_xs`, `xcurl`/`libHttpClient`)

pub mod com;
pub mod crt_math;
pub mod crt_str;
pub mod d3d11;
pub mod d3d12_xs;
pub mod exceptions;
pub mod file_io;
pub mod gdeflate;
#[macro_use]
pub mod gdk_classes;
pub mod gdk_xs;
pub mod kernel32;
pub mod kernel_ext;
pub mod msvc_rt;
pub mod msvcp_ios;
pub mod net;
pub mod process;
pub mod sync;
pub mod ucrt;
pub mod winrt;
pub mod winrt_objects;
pub mod xaudio2;
pub mod xbox_ext;
pub mod xbox_media;

use std::collections::HashMap;
use std::sync::Arc;

use sb_common::abi::E_NOTIMPL;
use sb_loader::{HleResolver, ImportSymbol};

/// Résolveur HLE par défaut reliant les imports PE32+ aux fonctions Rust `extern "win64"`.
pub struct DefaultHleResolver {
    kernel: HashMap<&'static str, usize>,
    crt: HashMap<&'static str, usize>,
    winrt: HashMap<&'static str, usize>,
    vccorlib: HashMap<&'static str, usize>,
    msvcp: HashMap<&'static str, usize>,
    atomic_wait: HashMap<&'static str, usize>,
    vcruntime1: HashMap<&'static str, usize>,
    concrt: HashMap<&'static str, usize>,
    vcomp: HashMap<&'static str, usize>,
    xaudio2_by_name: HashMap<&'static str, usize>,
    xaudio2_by_ord: HashMap<u16, usize>,
    d3d11: HashMap<&'static str, usize>,
    xmem: HashMap<&'static str, usize>,
    xgameruntime: HashMap<&'static str, usize>,
    d3d12_xs: HashMap<&'static str, usize>,
    dxgi: HashMap<&'static str, usize>,
    network: HashMap<&'static str, usize>,
    gameinput: HashMap<&'static str, usize>,
    fsr: HashMap<&'static str, usize>,
    bvh: HashMap<&'static str, usize>,
    bink: HashMap<&'static str, usize>,
    /// Exports par stem de DLL (réseau, Xbox/GDK, média) : prioritaires.
    extra: HashMap<&'static str, HashMap<&'static str, usize>>,
    extra_ord: HashMap<&'static str, HashMap<u16, usize>>,
}

/// Fusionne `src` dans `dst` sans écraser les entrées existantes.
fn merge_keep<K: std::hash::Hash + Eq, V>(dst: &mut HashMap<K, V>, src: HashMap<K, V>) {
    for (k, v) in src {
        dst.entry(k).or_insert(v);
    }
}

/// Fusionne des tables indexées par stem (les entrées déjà présentes gagnent).
fn merge_by_dll<K: std::hash::Hash + Eq>(
    dst: &mut HashMap<&'static str, HashMap<K, usize>>,
    src: HashMap<&'static str, HashMap<K, usize>>,
) {
    for (dll, syms) in src {
        merge_keep(dst.entry(dll).or_default(), syms);
    }
}

impl Default for DefaultHleResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl DefaultHleResolver {
    pub fn new() -> Self {
        let mut xaudio2_by_name = HashMap::new();
        xaudio2_by_name.insert(
            "XAudio2Create",
            xaudio2::XAudio2Create as *const () as usize,
        );
        let mut xaudio2_by_ord = HashMap::new();
        xaudio2_by_ord.insert(1, xaudio2::XAudio2Create as *const () as usize);

        let mut d3d11 = HashMap::new();
        d3d11.insert(
            "D3D11CreateDevice",
            d3d11::D3D11CreateDevice as *const () as usize,
        );

        let mut msvcp = winrt::msvcp_exports();
        msvcp.extend(msvc_rt::msvcp_threading_exports());
        merge_keep(&mut msvcp, msvcp_ios::exports());

        // Les implémentations historiques sont prioritaires sur les extensions.
        let mut crt = ucrt::exports();
        merge_keep(&mut crt, crt_str::exports());
        merge_keep(&mut crt, crt_math::exports());
        merge_keep(&mut crt, msvcp_ios::vcruntime_exports());

        let mut kernel = kernel32::exports();
        merge_keep(&mut kernel, kernel_ext::exports());

        let mut extra = HashMap::new();
        merge_by_dll(&mut extra, net::by_dll());
        merge_by_dll(&mut extra, xbox_ext::by_dll());
        merge_by_dll(&mut extra, xbox_media::by_dll());
        let mut extra_ord = HashMap::new();
        merge_by_dll(&mut extra_ord, net::ordinals_by_dll());
        merge_by_dll(&mut extra_ord, xbox_ext::ordinals_by_dll());
        merge_by_dll(&mut extra_ord, xbox_media::ordinals_by_dll());

        Self {
            kernel,
            crt,
            winrt: winrt::winrt_exports(),
            vccorlib: winrt::vccorlib_exports(),
            msvcp,
            atomic_wait: msvc_rt::atomic_wait_exports(),
            vcruntime1: msvc_rt::vcruntime_1_exports(),
            concrt: msvc_rt::concrt_exports(),
            vcomp: msvc_rt::vcomp_exports(),
            xaudio2_by_name,
            xaudio2_by_ord,
            d3d11,
            xmem: gdk_xs::xmem_exports(),
            xgameruntime: gdk_xs::xgameruntime_exports(),
            d3d12_xs: d3d12_xs::d3d12_xs_exports(),
            dxgi: d3d12_xs::dxgi_exports(),
            network: gdk_xs::network_offline_exports(),
            gameinput: gdk_xs::gameinput_exports(),
            fsr: gdk_xs::fsr_exports(),
            bvh: gdk_xs::bvh_exports(),
            bink: gdk_xs::bink_exports(),
            extra,
            extra_ord,
        }
    }

    pub fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}

impl HleResolver for DefaultHleResolver {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Option<usize> {
        let lower = dll.to_ascii_lowercase();
        let stem = lower.strip_suffix(".dll").unwrap_or(&lower);

        // Tables d'extension par stem (prioritaires, y compris par ordinal).
        let hit = match symbol {
            ImportSymbol::Name(n) => self.extra.get(stem).and_then(|m| m.get(n.as_str())),
            ImportSymbol::Ordinal(o) => self.extra_ord.get(stem).and_then(|m| m.get(o)),
        };
        if let Some(&addr) = hit {
            return Some(addr);
        }

        if stem == "xaudio2_9" || stem == "xaudio2_9redist" {
            return match symbol {
                ImportSymbol::Name(n) => self.xaudio2_by_name.get(n.as_str()).copied(),
                ImportSymbol::Ordinal(o) => self.xaudio2_by_ord.get(o).copied(),
            };
        }

        let ImportSymbol::Name(name) = symbol else {
            return None;
        };
        let n = name.as_str();

        if stem == "d3d11" {
            return self.d3d11.get(n).copied();
        }
        if stem == "d3d12_xs" || stem == "d3d12_x" || stem == "d3d12" {
            return self.d3d12_xs.get(n).copied();
        }
        if stem == "dxgi" {
            return self.dxgi.get(n).copied();
        }
        if stem == "xmem" {
            return self.xmem.get(n).copied();
        }
        if stem == "xgameruntime" || stem == "xgameplatform" {
            return self.xgameruntime.get(n).copied();
        }
        if stem == "xcurl" || stem == "libhttpclient.gdk" {
            return self.network.get(n).copied();
        }
        if stem == "gameinput" {
            return self.gameinput.get(n).copied();
        }
        if stem.starts_with("amd_fidelityfx_") {
            return self.fsr.get(n).copied();
        }
        if stem == "xboxbvhbuilder" {
            return self.bvh.get(n).copied();
        }
        if stem == "bink2scarlett" || stem == "bink2w64" {
            return self.bink.get(n).copied();
        }
        if stem.starts_with("vccorlib140") {
            return self.vccorlib.get(n).copied();
        }
        if stem == "msvcp140_atomic_wait" {
            return self.atomic_wait.get(n).copied();
        }
        if stem.starts_with("msvcp140") {
            return self
                .msvcp
                .get(n)
                .copied()
                .or_else(|| self.atomic_wait.get(n).copied());
        }
        if stem == "concrt140" {
            return self.concrt.get(n).copied();
        }
        if stem == "vcomp140" {
            return self.vcomp.get(n).copied();
        }
        if stem.starts_with("api-ms-win-core-winrt") || stem == "combase" {
            if let Some(&addr) = self.winrt.get(n) {
                return Some(addr);
            }
            return self.kernel.get(n).copied();
        }
        if stem.starts_with("api-ms-win-crt-")
            || stem == "ucrtbase"
            || stem == "msvcrt"
            || stem.starts_with("vcruntime140")
        {
            return self
                .crt
                .get(n)
                .copied()
                .or_else(|| self.vcruntime1.get(n).copied());
        }
        if stem == "kernel32"
            || stem == "kernelbase"
            || stem == "ntdll"
            || stem == "bcryptprimitives"
            || stem == "ole32"
            || stem == "advapi32"
            || stem == "bcrypt"
            || stem == "user32"
            || stem.starts_with("api-ms-win-core-")
            || stem.starts_with("ext-ms-win-")
        {
            return self
                .kernel
                .get(n)
                .copied()
                .or_else(|| self.winrt.get(n).copied());
        }

        None
    }

    fn is_hle_module(&self, dll: &str) -> bool {
        let lower = dll.to_ascii_lowercase();
        let stem = lower.strip_suffix(".dll").unwrap_or(&lower);
        if self.extra.contains_key(stem) || self.extra_ord.contains_key(stem) {
            return true;
        }
        stem == "kernel32"
            || stem == "bcrypt"
            || stem == "user32"
            || stem.starts_with("ext-ms-win-")
            || stem == "kernelbase"
            || stem == "ntdll"
            || stem == "ucrtbase"
            || stem == "msvcrt"
            || stem == "combase"
            || stem == "ole32"
            || stem == "advapi32"
            || stem == "bcryptprimitives"
            || stem == "d3d11"
            || stem == "d3d12_xs"
            || stem == "d3d12_x"
            || stem == "d3d12"
            || stem == "dxgi"
            || stem == "xgameruntime"
            || stem == "xcurl"
            || stem == "libhttpclient.gdk"
            || stem == "gameinput"
            || stem == "xboxbvhbuilder"
            || stem == "bink2scarlett"
            || stem.starts_with("amd_fidelityfx_")
            || stem == "xaudio2_9"
            || stem == "xaudio2_9redist"
            || stem.starts_with("api-ms-win-")
            || stem.starts_with("vcruntime140")
            || stem.starts_with("msvcp140")
            || stem.starts_with("vccorlib140")
            || stem == "concrt140"
            || stem == "vcomp140"
    }

    fn default_stub_return(&self, dll: &str, _symbol: &ImportSymbol) -> usize {
        let lower = dll.to_ascii_lowercase();
        if lower.contains("winrt")
            || lower.contains("xaudio")
            || lower.contains("d3d")
            || lower.contains("vccorlib")
        {
            E_NOTIMPL as usize
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_core_win32_crt_winrt_xaudio2_and_series_x_gdk() {
        let res = DefaultHleResolver::new();
        assert!(
            res.resolve("KERNEL32.dll", &ImportSymbol::Name("LoadLibraryW".into()))
                .is_some()
        );
        assert!(
            res.resolve(
                "api-ms-win-crt-heap-l1-1-0.dll",
                &ImportSymbol::Name("malloc".into())
            )
            .is_some()
        );
        assert!(
            res.resolve(
                "api-ms-win-core-winrt-l1-1-0.dll",
                &ImportSymbol::Name("RoInitialize".into())
            )
            .is_some()
        );
        assert!(
            res.resolve("XAudio2_9.dll", &ImportSymbol::Ordinal(1))
                .is_some()
        );
        assert!(
            res.resolve(
                "d3d12_xs.dll",
                &ImportSymbol::Name("D3D12CreateDevice".into())
            )
            .is_some()
        );
        assert!(
            res.resolve(
                "xgameruntime.dll",
                &ImportSymbol::Name("XSystemGetDeviceType".into())
            )
            .is_some()
        );
    }

    #[test]
    fn virtual_system_modules_have_handles_and_resolve_symbols() {
        let mm = sb_loader::ModuleManager::new();
        mm.set_resolver(std::sync::Arc::new(DefaultHleResolver::new()));
        let k = mm.virtual_module_handle("KERNEL32.dll").expect("kernel32");
        let a = mm
            .virtual_module_handle("api-ms-win-core-synch-l1-2-0.dll")
            .expect("api-ms synch");
        assert_ne!(k, a);
        assert_eq!(mm.virtual_module_handle("kernel32.dll"), Some(k));
        assert!(mm.virtual_module_handle("inexistante.dll").is_none());
        let sym = ImportSymbol::Name("SleepConditionVariableCS".into());
        assert!(mm.get_proc_address(a, &sym).is_some());
    }

    #[test]
    fn xgameruntime_dynamic_entry_points_resolve_through_virtual_module() {
        let mm = sb_loader::ModuleManager::new();
        mm.set_resolver(std::sync::Arc::new(DefaultHleResolver::new()));
        let h = mm
            .virtual_module_handle("xgameruntime.dll")
            .expect("xgameruntime virtuel");
        for n in [
            "QueryApiImpl",
            "InitializeApiImpl",
            "InitializeApiImplEx",
            "InitializeApiImplEx2",
            "XErrorReport",
            "UninitializeApiImpl",
        ] {
            assert!(
                mm.get_proc_address(h, &ImportSymbol::Name(n.into()))
                    .is_some(),
                "{n} non résolu"
            );
        }
    }
}
