//! Loader PE32+ natif Linux : mapping `mmap`, relocations `.reloc`, initialisation `.tls`,
//! résolution IAT (HLE Rust / DLL invitées / trampolines JIT), exports & gestionnaire de modules.

use std::collections::HashMap;
use std::ffi::CStr;
use std::fs;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use goblin::pe::PE;
use goblin::pe::header::COFF_MACHINE_X86_64;
use parking_lot::RwLock;
use sb_common::abi::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH, E_NOTIMPL};
use sb_common::{sb_debug, sb_info, sb_trace, sb_warn};
use thiserror::Error;

use crate::teb;
use crate::trampoline::allocate_stub_trampoline;
use crate::unwind::{RuntimeFunction, register_function_table};

const PAGE_SIZE: usize = 4096;

// Flags de caractéristiques des sections PE.
const IMAGE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const IMAGE_SCN_MEM_READ: u32 = 0x4000_0000;
const IMAGE_SCN_MEM_WRITE: u32 = 0x8000_0000;

// Types de relocation x86-64.
const IMAGE_REL_BASED_ABSOLUTE: u16 = 0;
const IMAGE_REL_BASED_HIGHLOW: u16 = 3;
const IMAGE_REL_BASED_DIR64: u16 = 10;

// Bit ordinal dans une entrée ILT 64 bits.
const IMAGE_ORDINAL_FLAG64: u64 = 1u64 << 63;

#[derive(Debug, Error)]
pub enum LoaderError {
    #[error("erreur I/O lors de la lecture de {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("fichier PE invalide ({path}): {reason}")]
    InvalidPe { path: PathBuf, reason: String },
    #[error("architecture non supportée (seul PE32+ x86-64 est supporté) : {0}")]
    UnsupportedArch(PathBuf),
    #[error("échec d'allocation mmap pour {path} ({size} octets)")]
    MmapFailed { path: PathBuf, size: usize },
    #[error("échec d'initialisation du TEB Windows (arch_prctl) : {0}")]
    TebInit(#[from] std::io::Error),
    #[error("le module {0} ne possède aucun point d'entrée")]
    NoEntryPoint(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ImportSymbol {
    Name(String),
    Ordinal(u16),
}

impl std::fmt::Display for ImportSymbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Name(s) => write!(f, "{s}"),
            Self::Ordinal(ord) => write!(f, "@{ord}"),
        }
    }
}

/// Résolveur HLE fourni par `sb-hle` pour lier les imports Win32/CRT/WinRT/Xbox vers du Rust natif.
pub trait HleResolver: Send + Sync {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Option<usize>;

    /// Indique si cette DLL est prise en charge par le HLE natif (les symboles non encore
    /// implémentés reçoivent un trampoline JIT plutôt que de charger une DLL PE32+ externe).
    fn is_hle_module(&self, _dll: &str) -> bool {
        false
    }

    /// Valeur de retour par défaut pour un trampoline JIT non implémenté (ex: `0` ou `E_NOTIMPL`).
    fn default_stub_return(&self, dll: &str, symbol: &ImportSymbol) -> usize {
        let lower = dll.to_ascii_lowercase();
        if lower.contains("winrt") || lower.contains("xaudio") || lower.contains("d3d") {
            E_NOTIMPL as usize
        } else {
            let _ = symbol;
            0
        }
    }
}

#[derive(Debug, Clone)]
pub enum ExportTarget {
    Address(usize),
    Forwarder(String),
}

#[derive(Debug)]
pub struct LoadedModule {
    pub name: String,
    pub path: PathBuf,
    pub base: usize,
    pub image_size: usize,
    pub is_dll: bool,
    pub entry_point: Option<usize>,
    pub exports_by_name: HashMap<String, ExportTarget>,
    pub exports_by_ordinal: HashMap<u16, ExportTarget>,
    pub tls_callbacks: Vec<usize>,
    pub resolved_hle: usize,
    pub resolved_guest: usize,
    pub stubbed_jit: usize,
}

impl LoadedModule {
    #[inline]
    pub const fn contains(&self, addr: usize) -> bool {
        addr >= self.base && addr < self.base.saturating_add(self.image_size)
    }
}

static NEXT_TLS_SLOT: AtomicUsize = AtomicUsize::new(0);

/// Modèle `.tls` d'un module PE chargé, cloné pour chaque nouveau thread invité (`CreateThread`).
#[derive(Clone)]
struct TlsTemplate {
    slot: usize,
    raw_template: Vec<u8>,
    total_len: usize,
    module_base: usize,
    callbacks: Vec<usize>,
}

/// Gestionnaire global de tous les modules PE32+ chargés dans le processus ScarletBridge.
pub struct ModuleManager {
    resolver: RwLock<Option<Arc<dyn HleResolver>>>,
    search_paths: RwLock<Vec<PathBuf>>,
    modules: RwLock<Vec<Arc<LoadedModule>>>,
    tls_templates: RwLock<Vec<TlsTemplate>>,
    main_module_base: AtomicUsize,
    /// Modules système virtuels (kernel32, api-ms-win-*, ...) pris en charge par le HLE.
    virtual_modules: RwLock<Vec<(String, usize)>>,
}

/// Base des handles de modules virtuels (plage non mappée, jamais déréférencée par le HLE).
const VIRTUAL_MODULE_BASE: usize = 0x7ffd_0000_0000;
const VIRTUAL_MODULE_STRIDE: usize = 0x10_0000;

impl Default for ModuleManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleManager {
    pub fn new() -> Self {
        Self {
            resolver: RwLock::new(None),
            search_paths: RwLock::new(Vec::new()),
            modules: RwLock::new(Vec::new()),
            tls_templates: RwLock::new(Vec::new()),
            main_module_base: AtomicUsize::new(0),
            virtual_modules: RwLock::new(Vec::new()),
        }
    }

    /// Retourne (en le créant au besoin) le handle d'un module système virtuel géré par le HLE.
    ///
    /// Permet à `GetModuleHandleW(L"kernel32.dll")` / `LoadLibrary` de réussir, comme sous Windows,
    /// pour les DLL qui n'existent pas sur disque. Retourne `None` si la DLL n'est pas HLE.
    pub fn virtual_module_handle(&self, name: &str) -> Option<usize> {
        let norm = normalize_dll_name(name);
        let key = if norm.contains('.') {
            norm
        } else {
            format!("{norm}.dll")
        };
        if let Some((_, h)) = self.virtual_modules.read().iter().find(|(n, _)| *n == key) {
            return Some(*h);
        }
        let resolver = self.resolver.read().clone()?;
        if !resolver.is_hle_module(&key) {
            return None;
        }
        let mut mods = self.virtual_modules.write();
        if let Some((_, h)) = mods.iter().find(|(n, _)| *n == key) {
            return Some(*h);
        }
        let handle = VIRTUAL_MODULE_BASE + (mods.len() + 1) * VIRTUAL_MODULE_STRIDE;
        mods.push((key, handle));
        Some(handle)
    }

    /// Nom de la DLL virtuelle associée à un handle, le cas échéant.
    pub fn virtual_module_name(&self, handle: usize) -> Option<String> {
        self.virtual_modules
            .read()
            .iter()
            .find(|(_, h)| *h == handle)
            .map(|(n, _)| n.clone())
    }

    /// Clone les sections `.tls` de tous les modules chargés pour le thread invité courant
    /// et invoque les callbacks TLS avec `DLL_THREAD_ATTACH (2)`.
    pub fn attach_current_thread_tls(&self) {
        let templates = self.tls_templates.read().clone();
        for tpl in templates {
            let mut buf = vec![0u8; tpl.total_len].into_boxed_slice();
            let copy_len = tpl.raw_template.len().min(buf.len());
            buf[..copy_len].copy_from_slice(&tpl.raw_template[..copy_len]);
            teb::attach_owned_module_tls(tpl.slot, buf);
            for &cb in &tpl.callbacks {
                // SAFETY: `cb` est un `PIMAGE_TLS_CALLBACK` Win64 valide.
                let f: unsafe extern "win64" fn(usize, u32, usize) =
                    unsafe { core::mem::transmute(cb) };
                unsafe {
                    f(tpl.module_base, 2, 0); // DLL_THREAD_ATTACH
                }
            }
        }
    }

    /// Invoque les callbacks TLS avec `DLL_THREAD_DETACH (3)` avant la fin d'un thread invité.
    pub fn detach_current_thread_tls(&self) {
        let templates = self.tls_templates.read().clone();
        for tpl in templates.iter().rev() {
            for &cb in &tpl.callbacks {
                // SAFETY: `cb` est un `PIMAGE_TLS_CALLBACK` Win64 valide.
                let f: unsafe extern "win64" fn(usize, u32, usize) =
                    unsafe { core::mem::transmute(cb) };
                unsafe {
                    f(tpl.module_base, 3, 0); // DLL_THREAD_DETACH
                }
            }
        }
    }

    pub fn set_resolver(&self, resolver: Arc<dyn HleResolver>) {
        *self.resolver.write() = Some(resolver);
    }

    pub fn add_search_path(&self, dir: PathBuf) {
        let mut paths = self.search_paths.write();
        if !paths.contains(&dir) {
            paths.push(dir);
        }
    }

    pub fn main_module_base(&self) -> usize {
        self.main_module_base.load(Ordering::Acquire)
    }

    pub fn find_by_name(&self, name: &str) -> Option<Arc<LoadedModule>> {
        let norm = normalize_dll_name(name);
        self.modules.read().iter().find(|m| m.name == norm).cloned()
    }

    pub fn find_by_base(&self, base: usize) -> Option<Arc<LoadedModule>> {
        self.modules.read().iter().find(|m| m.base == base).cloned()
    }

    /// Module PE chargé contenant l'adresse `addr` (équivalent de `RtlPcToFileHeader`).
    pub fn find_by_address(&self, addr: usize) -> Option<Arc<LoadedModule>> {
        self.modules
            .read()
            .iter()
            .find(|m| m.contains(addr))
            .cloned()
    }

    /// Décrit une adresse d'exécution sous la forme `module.exe+0xRVA` pour le diagnostic de signaux.
    pub fn describe_address(&self, addr: usize) -> Option<String> {
        let mods = self.modules.read();
        for m in mods.iter() {
            if m.contains(addr) {
                return Some(format!("{}+{:#x}", m.name, addr - m.base));
            }
        }
        None
    }

    /// Charge et mappe un exécutable PE32+ principal (ainsi que ses dépendances), sans l'exécuter.
    pub fn load_executable(&self, path: &Path) -> Result<Arc<LoadedModule>, LoaderError> {
        // Initialise `GS` (TEB) avant tout chargement récursif de DLL qui pourrait exécuter `DllMain`.
        teb::init_thread_teb(0)?;

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            self.add_search_path(parent.to_path_buf());
        }

        let module = self.load_pe_internal(path, false)?;
        self.main_module_base.store(module.base, Ordering::Release);
        teb::set_peb_image_base(module.base);
        self.init_tls_for_module(&module);
        Ok(module)
    }

    /// Charge une DLL PE32+ par nom ou chemin (utilisé par IAT et par `LoadLibraryW`/`LoadLibraryA`).
    pub fn load_library(&self, name_or_path: &str) -> Result<Arc<LoadedModule>, LoaderError> {
        let norm = normalize_dll_name(name_or_path);
        if let Some(existing) = self.find_by_name(&norm) {
            return Ok(existing);
        }

        let disk_path = self
            .locate_dll_on_disk(name_or_path)
            .ok_or_else(|| LoaderError::Io {
                path: PathBuf::from(name_or_path),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("DLL introuvable : {name_or_path}"),
                ),
            })?;

        let module = self.load_pe_internal(&disk_path, true)?;
        self.init_tls_for_module(&module);
        self.call_dll_main(&module, DLL_PROCESS_ATTACH);
        Ok(module)
    }

    /// Résout un symbole exporté d'un module chargé (ou d'un module virtuel HLE).
    pub fn get_proc_address(&self, base: usize, symbol: &ImportSymbol) -> Option<usize> {
        // Recherche dans les modules PE chargés.
        if let Some(module) = self.find_by_base(base) {
            let target = match symbol {
                ImportSymbol::Name(name) => module.exports_by_name.get(name),
                ImportSymbol::Ordinal(ord) => module.exports_by_ordinal.get(ord),
            }?;
            return match target {
                ExportTarget::Address(addr) => Some(*addr),
                ExportTarget::Forwarder(fwd) => self.resolve_forwarder(fwd, 0),
            };
        }
        // Module système virtuel (kernel32, api-ms-win-*, ...) : résolution HLE.
        if let Some(name) = self.virtual_module_name(base) {
            return self.resolve_hle(&name, symbol);
        }
        None
    }

    /// Résout un symbole HLE directement par `(dll, symbol)` via le `HleResolver` enregistré.
    pub fn resolve_hle(&self, dll: &str, symbol: &ImportSymbol) -> Option<usize> {
        let resolver = self.resolver.read().clone()?;
        resolver.resolve(dll, symbol)
    }

    /// Exécute le point d'entrée d'un exécutable PE32+ déjà chargé.
    ///
    /// # Safety
    /// Exécute du code machine x86-64 arbitraire issu du binaire PE32+ mappé.
    pub unsafe fn execute_module(&self, module: &LoadedModule) -> Result<i32, LoaderError> {
        let entry_addr = module
            .entry_point
            .ok_or_else(|| LoaderError::NoEntryPoint(module.path.clone()))?;

        // Appelle les callbacks TLS éventuels avant le point d'entrée principal.
        for &cb in &module.tls_callbacks {
            sb_debug!(
                "loader",
                "invocation callback TLS {:#x} pour {}",
                cb,
                module.name
            );
            // SAFETY: signature standard `PIMAGE_TLS_CALLBACK` en ABI `win64`.
            let func: unsafe extern "win64" fn(*mut libc::c_void, u32, *mut libc::c_void) =
                unsafe { core::mem::transmute(cb) };
            unsafe {
                func(
                    module.base as *mut libc::c_void,
                    DLL_PROCESS_ATTACH,
                    ptr::null_mut(),
                );
            }
        }

        sb_info!(
            "loader",
            "saut vers le point d'entrée de {} à {:#x} (base={:#x}, HLE={}, guest={}, JIT_stubs={})",
            module.name,
            entry_addr,
            module.base,
            module.resolved_hle,
            module.resolved_guest,
            module.stubbed_jit
        );

        // SAFETY: `entry_addr` pointe vers le code exécutable du binaire PE32+ chargé.
        let entry_fn: unsafe extern "win64" fn() -> i32 =
            unsafe { core::mem::transmute(entry_addr) };
        let code = unsafe { entry_fn() };
        Ok(code)
    }

    /// Appelle `DllMain(DLL_PROCESS_DETACH)` sur un module DLL (utilisé par `FreeLibrary`).
    pub fn unload_library(&self, base: usize) -> bool {
        if let Some(module) = self.find_by_base(base) {
            if module.is_dll {
                self.call_dll_main(&module, DLL_PROCESS_DETACH);
            }
            return true;
        }
        false
    }

    fn call_dll_main(&self, module: &LoadedModule, reason: u32) {
        let Some(entry_addr) = module.entry_point else {
            return;
        };
        sb_debug!(
            "loader",
            "appel DllMain({}, reason={reason}) à {:#x}",
            module.name,
            entry_addr
        );
        // SAFETY: `DllMain` suit la signature `BOOL WINAPI DllMain(HINSTANCE, DWORD, LPVOID)`.
        let dll_main: unsafe extern "win64" fn(*mut libc::c_void, u32, *mut libc::c_void) -> i32 =
            unsafe { core::mem::transmute(entry_addr) };
        unsafe {
            let _ = dll_main(module.base as *mut libc::c_void, reason, ptr::null_mut());
        }
    }

    fn locate_dll_on_disk(&self, name_or_path: &str) -> Option<PathBuf> {
        let candidate = PathBuf::from(name_or_path);
        if candidate.is_file() {
            return Some(candidate);
        }
        let wanted = Path::new(name_or_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())?;

        for dir in self.search_paths.read().iter() {
            let direct = dir.join(&wanted);
            if direct.is_file() {
                return Some(direct);
            }
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_file()
                        && let Some(fname) = p.file_name().and_then(|n| n.to_str())
                        && fname.eq_ignore_ascii_case(&wanted)
                    {
                        return Some(p);
                    }
                }
            }
        }
        None
    }

    fn load_pe_internal(
        &self,
        path: &Path,
        expect_dll: bool,
    ) -> Result<Arc<LoadedModule>, LoaderError> {
        let raw = fs::read(path).map_err(|source| LoaderError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let pe = PE::parse(&raw).map_err(|err| LoaderError::InvalidPe {
            path: path.to_path_buf(),
            reason: err.to_string(),
        })?;

        if !pe.is_64 || pe.header.coff_header.machine != COFF_MACHINE_X86_64 {
            return Err(LoaderError::UnsupportedArch(path.to_path_buf()));
        }

        let opt = pe
            .header
            .optional_header
            .ok_or_else(|| LoaderError::InvalidPe {
                path: path.to_path_buf(),
                reason: "OptionalHeader absent".to_string(),
            })?;

        let preferred_base = opt.windows_fields.image_base as usize;
        let image_size = align_up(opt.windows_fields.size_of_image as usize, PAGE_SIZE);
        let headers_size = opt.windows_fields.size_of_headers as usize;
        let entry_rva = opt.standard_fields.address_of_entry_point as usize;

        // 1. Allocation mémoire virtuelle RW (hint = ImageBase si raisonnable).
        let hint = if (0x1000_0000..0x0000_7000_0000_0000).contains(&preferred_base) {
            preferred_base as *mut libc::c_void
        } else {
            ptr::null_mut()
        };
        // Adresse préférée d'abord, de façon stricte : `MAP_FIXED_NOREPLACE` n'écrase jamais un
        // mapping existant et échoue plutôt que de placer l'image ailleurs (un simple `hint` peut
        // être ignoré en silence, ce qui est fatal pour une image sans relocations).
        let mut mapped = libc::MAP_FAILED;
        if !hint.is_null() {
            // SAFETY: allocation anonyme RW ; `MAP_FIXED_NOREPLACE` ne remplace aucun mapping.
            mapped = unsafe {
                libc::mmap(
                    hint,
                    image_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
                    -1,
                    0,
                )
            };
            if mapped != libc::MAP_FAILED && mapped != hint {
                // Noyau ancien qui ignore le drapeau : on libère et on retombe sur le repli.
                // SAFETY: mapping que nous venons de créer.
                unsafe { libc::munmap(mapped, image_size) };
                mapped = libc::MAP_FAILED;
            }
        }
        if mapped == libc::MAP_FAILED {
            // Repli : n'importe quelle adresse (l'image sera relogée si elle le permet).
            // SAFETY: allocation anonyme `mmap` RW pour charger le PE32+.
            mapped = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    image_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
        }
        if mapped == libc::MAP_FAILED {
            return Err(LoaderError::MmapFailed {
                path: path.to_path_buf(),
                size: image_size,
            });
        }
        if mapped as usize != preferred_base
            && opt.data_directories.get_base_relocation_table().is_none()
        {
            // Sans table de relocation, les adresses absolues du code seraient fausses : échec net.
            // SAFETY: libération de l'image que nous venons d'allouer.
            unsafe { libc::munmap(mapped, image_size) };
            return Err(LoaderError::InvalidPe {
                path: path.to_path_buf(),
                reason: format!(
                    "base préférée {preferred_base:#x} indisponible dans ce processus et l'image n'a \
                     aucune table de relocation : chargement impossible (libérez cette plage \
                     d'adresses, ex. désactivez un préchargement `LD_PRELOAD`)"
                ),
            });
        }
        let base = mapped as usize;

        // 2. Copie des en-têtes PE et de chaque section.
        let copy_hdr = headers_size.min(raw.len()).min(image_size);
        // SAFETY: `base..base+image_size` vient d'être alloué RW par `mmap`.
        unsafe {
            ptr::copy_nonoverlapping(raw.as_ptr(), base as *mut u8, copy_hdr);
        }

        for sec in &pe.sections {
            let va = sec.virtual_address as usize;
            let raw_ptr = sec.pointer_to_raw_data as usize;
            let raw_size = sec.size_of_raw_data as usize;
            if raw_size == 0 || raw_ptr >= raw.len() {
                continue;
            }
            let avail = (raw.len() - raw_ptr).min(raw_size);
            if va + avail > image_size {
                continue;
            }
            // SAFETY: plage `[va, va+avail)` vérifiée dans `[0, image_size)`.
            unsafe {
                ptr::copy_nonoverlapping(raw.as_ptr().add(raw_ptr), (base + va) as *mut u8, avail);
            }
        }

        // 3. Relocations de base (`.reloc`).
        let delta = base.wrapping_sub(preferred_base);
        if delta != 0
            && let Some(reloc_dir) = opt.data_directories.get_base_relocation_table()
        {
            apply_relocations(
                base,
                image_size,
                reloc_dir.virtual_address as usize,
                reloc_dir.size as usize,
                delta,
            );
        }

        // 4. Parsing des exports avant résolution IAT (permet de résoudre les dépendances cycliques).
        let (exports_by_name, exports_by_ordinal) =
            if let Some(exp_dir) = opt.data_directories.get_export_table() {
                parse_exports_from_mapped(
                    base,
                    image_size,
                    exp_dir.virtual_address as usize,
                    exp_dir.size as usize,
                )
            } else {
                (HashMap::new(), HashMap::new())
            };

        // 5. Résolution de la table d'imports (IAT).
        let (resolved_hle, resolved_guest, stubbed_jit) =
            if let Some(imp_dir) = opt.data_directories.get_import_table() {
                self.resolve_imports(base, image_size, imp_dir.virtual_address as usize)
            } else {
                (0, 0, 0)
            };

        // 6. Extraction des callbacks `.tls` (l'indexation est effectuée après création du TEB).
        let tls_callbacks = if let Some(tls_dir) = opt.data_directories.get_tls_table() {
            read_tls_callbacks(base, image_size, tls_dir.virtual_address as usize)
        } else {
            Vec::new()
        };

        // 7. Enregistrement de la table `.pdata` (IMAGE_DIRECTORY_ENTRY_EXCEPTION) pour l'unwind.
        if let Some(exc_dir) = opt.data_directories.get_exception_table() {
            let rva = exc_dir.virtual_address as usize;
            let size = exc_dir.size as usize;
            let count = size / size_of::<RuntimeFunction>();
            if rva != 0 && count > 0 && rva + count * size_of::<RuntimeFunction>() <= image_size {
                // SAFETY: la table `.pdata` réside dans l'image mappée, qui n'est jamais démappée.
                unsafe {
                    register_function_table(
                        base,
                        image_size,
                        (base + rva) as *const RuntimeFunction,
                        count,
                    );
                }
            }
        }

        // 8. Application des permissions par page (`mprotect`).
        apply_section_protections(base, image_size, &pe.sections);

        let name = normalize_dll_name(&path.display().to_string());
        let entry_point = if entry_rva != 0 {
            Some(base + entry_rva)
        } else {
            None
        };

        sb_info!(
            "loader",
            "PE32+ chargé : {} ({:#x}..{:#x}, delta={:#x}, exports={}, HLE={}, guest={}, JIT={})",
            name,
            base,
            base + image_size,
            delta,
            exports_by_name.len(),
            resolved_hle,
            resolved_guest,
            stubbed_jit
        );

        let module = Arc::new(LoadedModule {
            name,
            path: path.to_path_buf(),
            base,
            image_size,
            is_dll: expect_dll || pe.is_lib,
            entry_point,
            exports_by_name,
            exports_by_ordinal,
            tls_callbacks,
            resolved_hle,
            resolved_guest,
            stubbed_jit,
        });

        self.modules.write().push(Arc::clone(&module));
        Ok(module)
    }

    fn init_tls_for_module(&self, module: &LoadedModule) {
        let base = module.base;
        let Some((tls_rva, _tls_size)) = read_data_dir_from_mapped(base, module.image_size, 9)
        else {
            return;
        };
        if tls_rva == 0 || tls_rva + 40 > module.image_size {
            return;
        }
        // SAFETY: `tls_rva..tls_rva+40` est dans l'image mappée.
        let (start_raw, end_raw, addr_of_index, zero_fill) = unsafe {
            let p = (base + tls_rva) as *const u8;
            let start = ptr::read_unaligned(p.cast::<u64>()) as usize;
            let end = ptr::read_unaligned(p.add(8).cast::<u64>()) as usize;
            let idx_ptr = ptr::read_unaligned(p.add(16).cast::<u64>()) as usize;
            let zfill = ptr::read_unaligned(p.add(32).cast::<u32>()) as usize;
            (start, end, idx_ptr, zfill)
        };

        let raw_len = end_raw.saturating_sub(start_raw);
        let total_len = raw_len.saturating_add(zero_fill).max(64);
        let slot = NEXT_TLS_SLOT.fetch_add(1, Ordering::AcqRel);

        let mut raw_template = Vec::new();
        let mut buf = vec![0u8; total_len].into_boxed_slice();
        if raw_len > 0 && start_raw >= base && end_raw <= base + module.image_size {
            // SAFETY: plage TLS source vérifiée dans l'image mappée.
            unsafe {
                let slice = std::slice::from_raw_parts(start_raw as *const u8, raw_len);
                buf[..raw_len].copy_from_slice(slice);
                raw_template.extend_from_slice(slice);
            }
        }
        let leaked = Box::leak(buf).as_mut_ptr();
        teb::set_module_tls_slot(slot, leaked);
        self.tls_templates.write().push(TlsTemplate {
            slot,
            raw_template,
            total_len,
            module_base: base,
            callbacks: module.tls_callbacks.clone(),
        });

        if addr_of_index >= base && addr_of_index + 4 <= base + module.image_size {
            // SAFETY: `AddressOfIndex` est une variable globale RW `u32` dans `.data`.
            unsafe {
                ptr::write_unaligned(addr_of_index as *mut u32, slot as u32);
            }
        }
        sb_debug!(
            "loader",
            "TLS initialisé pour {} : slot={slot}, raw={raw_len}o, zero={zero_fill}o",
            module.name
        );
    }

    fn resolve_imports(
        &self,
        base: usize,
        image_size: usize,
        imp_rva: usize,
    ) -> (usize, usize, usize) {
        if imp_rva == 0 {
            return (0, 0, 0);
        }

        let mut hle_count = 0usize;
        let mut guest_count = 0usize;
        let mut jit_count = 0usize;

        let mut desc_offset = imp_rva;
        while desc_offset + 20 <= image_size {
            // SAFETY: descripteur `IMAGE_IMPORT_DESCRIPTOR` de 20 octets dans l'image.
            let (orig_first_thunk, name_rva, first_thunk) = unsafe {
                let p = (base + desc_offset) as *const u32;
                let oft = ptr::read_unaligned(p) as usize;
                let name = ptr::read_unaligned(p.add(3)) as usize;
                let ft = ptr::read_unaligned(p.add(4)) as usize;
                (oft, name, ft)
            };

            if name_rva == 0 && first_thunk == 0 {
                break;
            }
            desc_offset += 20;

            let dll_name = read_c_string(base, image_size, name_rva).unwrap_or_default();
            if dll_name.is_empty() || first_thunk == 0 {
                continue;
            }

            let ilt_rva = if orig_first_thunk != 0 {
                orig_first_thunk
            } else {
                first_thunk
            };

            let mut entry_idx = 0usize;
            loop {
                let ilt_entry_rva = ilt_rva + entry_idx * 8;
                let iat_entry_rva = first_thunk + entry_idx * 8;
                if ilt_entry_rva + 8 > image_size || iat_entry_rva + 8 > image_size {
                    break;
                }

                // SAFETY: entrée ILT 64 bits dans l'image mappée.
                let thunk = unsafe { ptr::read_unaligned((base + ilt_entry_rva) as *const u64) };
                if thunk == 0 {
                    break;
                }

                let sym = if (thunk & IMAGE_ORDINAL_FLAG64) != 0 {
                    ImportSymbol::Ordinal((thunk & 0xFFFF) as u16)
                } else {
                    let hint_name_rva = (thunk as usize).saturating_add(2);
                    let name = read_c_string(base, image_size, hint_name_rva).unwrap_or_default();
                    ImportSymbol::Name(name)
                };

                let target_addr = if let Some(addr) = self
                    .prefers_guest_dll(&dll_name)
                    .then(|| self.resolve_from_guest_dll_forced(&dll_name, &sym))
                    .flatten()
                {
                    guest_count += 1;
                    addr
                } else if let Some(addr) = self.resolve_hle(&dll_name, &sym) {
                    hle_count += 1;
                    addr
                } else if let Some(addr) = self.resolve_from_guest_dll(&dll_name, &sym) {
                    guest_count += 1;
                    addr
                } else {
                    let ret_val = self
                        .resolver
                        .read()
                        .as_ref()
                        .map(|r| r.default_stub_return(&dll_name, &sym))
                        .unwrap_or(0);
                    let stub_addr = allocate_stub_trampoline(&dll_name, &sym.to_string(), ret_val);
                    sb_trace!(
                        "loader",
                        "trampoline JIT généré pour {}!{} -> {:#x}",
                        dll_name,
                        sym,
                        stub_addr
                    );
                    jit_count += 1;
                    stub_addr
                };

                // `SB_TRACE_API` : les imports HLE / invités passent par un thunk qui note l'appel
                // (les trampolines JIT de symboles absents journalisent déjà eux-mêmes).
                let target_addr = if crate::trampoline::api_trace_mode()
                    != crate::trampoline::ApiTraceMode::Off
                    && !crate::trampoline::is_stub_trampoline(target_addr)
                {
                    crate::trampoline::allocate_trace_thunk(
                        &dll_name,
                        &sym.to_string(),
                        target_addr,
                    )
                } else {
                    target_addr
                };

                // SAFETY: écriture dans l'IAT avant le verrouillage `mprotect`.
                unsafe {
                    ptr::write_unaligned((base + iat_entry_rva) as *mut u64, target_addr as u64);
                }

                entry_idx += 1;
            }
        }

        (hle_count, guest_count, jit_count)
    }

    /// `SB_GUEST_DLLS=msvcp140,concrt140` : DLL redistribuables fournies avec l'application à
    /// charger comme DLL invitées (code réel) plutôt que via le HLE. Opt-in : un symbole absent
    /// de la DLL retombe sur le HLE puis sur les trampolines.
    fn prefers_guest_dll(&self, dll_name: &str) -> bool {
        static LIST: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        let list = LIST.get_or_init(|| parse_guest_dll_list(std::env::var("SB_GUEST_DLLS").ok()));
        let norm = normalize_dll_name(dll_name);
        let stem = norm.strip_suffix(".dll").unwrap_or(&norm);
        list.iter().any(|n| n == stem)
    }

    fn resolve_from_guest_dll_forced(&self, dll_name: &str, sym: &ImportSymbol) -> Option<usize> {
        let module = self.load_library(dll_name).ok()?;
        self.get_proc_address(module.base, sym)
    }

    fn resolve_from_guest_dll(&self, dll_name: &str, sym: &ImportSymbol) -> Option<usize> {
        if self
            .resolver
            .read()
            .as_ref()
            .is_some_and(|r| r.is_hle_module(dll_name))
        {
            return None;
        }
        let module = self.load_library(dll_name).ok()?;
        self.get_proc_address(module.base, sym)
    }

    fn resolve_forwarder(&self, forwarder: &str, depth: usize) -> Option<usize> {
        if depth > 8 {
            return None;
        }
        let (target_dll, target_sym) = forwarder.split_once('.')?;
        let dll_with_ext = if target_dll.to_ascii_lowercase().ends_with(".dll") {
            target_dll.to_string()
        } else {
            format!("{target_dll}.dll")
        };
        let sym = if let Some(ord_str) = target_sym.strip_prefix('#') {
            ImportSymbol::Ordinal(ord_str.parse::<u16>().ok()?)
        } else {
            ImportSymbol::Name(target_sym.to_string())
        };

        if let Some(addr) = self.resolve_hle(&dll_with_ext, &sym) {
            return Some(addr);
        }
        if let Ok(module) = self.load_library(&dll_with_ext) {
            let entry = match &sym {
                ImportSymbol::Name(n) => module.exports_by_name.get(n),
                ImportSymbol::Ordinal(o) => module.exports_by_ordinal.get(o),
            }?;
            return match entry {
                ExportTarget::Address(a) => Some(*a),
                ExportTarget::Forwarder(next) => self.resolve_forwarder(next, depth + 1),
            };
        }
        None
    }
}

static MODULE_MANAGER: OnceLock<ModuleManager> = OnceLock::new();

pub fn module_manager() -> &'static ModuleManager {
    MODULE_MANAGER.get_or_init(ModuleManager::new)
}

/// Découpe `SB_GUEST_DLLS` (séparateurs `,` `;` espace) en noms minuscules sans `.dll`.
fn parse_guest_dll_list(value: Option<String>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split([',', ';', ' '])
        .map(|n| n.trim().to_ascii_lowercase())
        .map(|n| n.strip_suffix(".dll").map(str::to_string).unwrap_or(n))
        .filter(|n| !n.is_empty())
        .collect()
}

fn normalize_dll_name(name: &str) -> String {
    Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| name.to_ascii_lowercase())
}

const fn align_up(value: usize, align: usize) -> usize {
    let rem = value % align;
    if rem == 0 {
        value
    } else {
        value + (align - rem)
    }
}

fn read_c_string(base: usize, image_size: usize, rva: usize) -> Option<String> {
    if rva >= image_size {
        return None;
    }
    let max_len = image_size - rva;
    // SAFETY: borné à `max_len` octets dans l'image mappée.
    let slice = unsafe { std::slice::from_raw_parts((base + rva) as *const u8, max_len) };
    let nul_pos = slice.iter().position(|&b| b == 0)?;
    CStr::from_bytes_with_nul(&slice[..=nul_pos])
        .ok()
        .map(|c| c.to_string_lossy().into_owned())
}

fn read_data_dir_from_mapped(
    base: usize,
    image_size: usize,
    index: usize,
) -> Option<(usize, usize)> {
    if image_size < 0x100 {
        return None;
    }
    // SAFETY: en-tête DOS + PE32+ présents au début de l'image mappée.
    unsafe {
        let pe_off = ptr::read_unaligned((base + 0x3C) as *const u32) as usize;
        // PE sig (4) + COFF header (20) + OptionalHeader64 DataDirectories offset (112) = 136
        let dir_off = pe_off + 24 + 112 + index * 8;
        if dir_off + 8 > image_size {
            return None;
        }
        let rva = ptr::read_unaligned((base + dir_off) as *const u32) as usize;
        let size = ptr::read_unaligned((base + dir_off + 4) as *const u32) as usize;
        Some((rva, size))
    }
}

fn apply_relocations(
    base: usize,
    image_size: usize,
    reloc_rva: usize,
    reloc_size: usize,
    delta: usize,
) {
    let mut offset = 0usize;
    let mut patched = 0usize;

    while offset + 8 <= reloc_size && reloc_rva + offset + 8 <= image_size {
        // SAFETY: bloc `IMAGE_BASE_RELOCATION` dans l'image mappée.
        let (page_rva, block_size) = unsafe {
            let p = (base + reloc_rva + offset) as *const u32;
            (
                ptr::read_unaligned(p) as usize,
                ptr::read_unaligned(p.add(1)) as usize,
            )
        };
        if block_size < 8 || offset + block_size > reloc_size {
            break;
        }

        let count = (block_size - 8) / 2;
        for i in 0..count {
            let entry_off = reloc_rva + offset + 8 + i * 2;
            if entry_off + 2 > image_size {
                break;
            }
            // SAFETY: entrée de relocation 16 bits valide dans le bloc.
            let entry = unsafe { ptr::read_unaligned((base + entry_off) as *const u16) };
            let kind = entry >> 12;
            let rel_off = (entry & 0x0FFF) as usize;
            let target_rva = page_rva + rel_off;

            match kind {
                IMAGE_REL_BASED_ABSOLUTE => {}
                IMAGE_REL_BASED_DIR64 if target_rva + 8 <= image_size => {
                    // SAFETY: `target_rva + 8 <= image_size` dans la zone RW avant `mprotect`.
                    unsafe {
                        let ptr64 = (base + target_rva) as *mut u64;
                        let val = ptr::read_unaligned(ptr64);
                        ptr::write_unaligned(ptr64, val.wrapping_add(delta as u64));
                    }
                    patched += 1;
                }
                IMAGE_REL_BASED_HIGHLOW if target_rva + 4 <= image_size => {
                    // SAFETY: `target_rva + 4 <= image_size` dans la zone RW.
                    unsafe {
                        let ptr32 = (base + target_rva) as *mut u32;
                        let val = ptr::read_unaligned(ptr32);
                        ptr::write_unaligned(ptr32, val.wrapping_add(delta as u32));
                    }
                    patched += 1;
                }
                other => {
                    sb_warn!("loader", "type de relocation PE non géré : {other}");
                }
            }
        }
        offset += block_size;
    }

    sb_debug!(
        "loader",
        "relocations appliquées : {patched} entrées (delta={delta:#x})"
    );
}

fn parse_exports_from_mapped(
    base: usize,
    image_size: usize,
    exp_rva: usize,
    exp_size: usize,
) -> (HashMap<String, ExportTarget>, HashMap<u16, ExportTarget>) {
    let mut by_name = HashMap::new();
    let mut by_ordinal = HashMap::new();

    if exp_rva == 0 || exp_rva + 40 > image_size {
        return (by_name, by_ordinal);
    }

    // SAFETY: `IMAGE_EXPORT_DIRECTORY` de 40 octets dans l'image mappée.
    let (ordinal_base, num_funcs, num_names, funcs_rva, names_rva, name_ords_rva) = unsafe {
        let p = (base + exp_rva) as *const u32;
        (
            ptr::read_unaligned(p.add(4)),
            ptr::read_unaligned(p.add(5)) as usize,
            ptr::read_unaligned(p.add(6)) as usize,
            ptr::read_unaligned(p.add(7)) as usize,
            ptr::read_unaligned(p.add(8)) as usize,
            ptr::read_unaligned(p.add(9)) as usize,
        )
    };

    let make_target = |func_rva: usize| -> Option<ExportTarget> {
        if func_rva == 0 {
            return None;
        }
        if func_rva >= exp_rva && func_rva < exp_rva + exp_size {
            read_c_string(base, image_size, func_rva).map(ExportTarget::Forwarder)
        } else if func_rva < image_size {
            Some(ExportTarget::Address(base + func_rva))
        } else {
            None
        }
    };

    for i in 0..num_funcs {
        let off = funcs_rva + i * 4;
        if off + 4 > image_size {
            break;
        }
        // SAFETY: index borné dans l'image mappée.
        let func_rva = unsafe { ptr::read_unaligned((base + off) as *const u32) } as usize;
        if let Some(target) = make_target(func_rva) {
            let ord = (ordinal_base as usize + i) as u16;
            by_ordinal.insert(ord, target);
        }
    }

    for i in 0..num_names {
        let name_off = names_rva + i * 4;
        let ord_off = name_ords_rva + i * 2;
        if name_off + 4 > image_size || ord_off + 2 > image_size {
            break;
        }
        // SAFETY: indices bornés dans l'image mappée.
        let (name_rva, func_idx) = unsafe {
            (
                ptr::read_unaligned((base + name_off) as *const u32) as usize,
                ptr::read_unaligned((base + ord_off) as *const u16) as usize,
            )
        };
        if let Some(name) = read_c_string(base, image_size, name_rva) {
            let func_off = funcs_rva + func_idx * 4;
            if func_off + 4 <= image_size {
                // SAFETY: `func_off + 4 <= image_size`.
                let func_rva =
                    unsafe { ptr::read_unaligned((base + func_off) as *const u32) } as usize;
                if let Some(target) = make_target(func_rva) {
                    by_name.insert(name, target);
                }
            }
        }
    }

    (by_name, by_ordinal)
}

fn read_tls_callbacks(base: usize, image_size: usize, tls_rva: usize) -> Vec<usize> {
    let mut callbacks = Vec::new();
    if tls_rva == 0 || tls_rva + 32 > image_size {
        return callbacks;
    }
    // SAFETY: `IMAGE_TLS_DIRECTORY64::AddressOfCallBacks` à l'offset 24.
    let cb_array_va = unsafe { ptr::read_unaligned((base + tls_rva + 24) as *const u64) } as usize;
    if cb_array_va < base || cb_array_va >= base + image_size {
        return callbacks;
    }
    let mut cur = cb_array_va;
    while cur + 8 <= base + image_size {
        // SAFETY: lecture du tableau de pointeurs terminé par 0 dans l'image.
        let cb = unsafe { ptr::read_unaligned(cur as *const u64) } as usize;
        if cb == 0 {
            break;
        }
        callbacks.push(cb);
        cur += 8;
    }
    callbacks
}

fn apply_section_protections(
    base: usize,
    image_size: usize,
    sections: &[goblin::pe::section_table::SectionTable],
) {
    let page_count = image_size / PAGE_SIZE;
    let mut page_prots = vec![libc::PROT_READ; page_count];

    for sec in sections {
        let va = sec.virtual_address as usize;
        let len = (sec.virtual_size as usize).max(sec.size_of_raw_data as usize);
        if len == 0 || va >= image_size {
            continue;
        }
        let end = (va + len).min(image_size);
        let start_page = va / PAGE_SIZE;
        let end_page = align_up(end, PAGE_SIZE) / PAGE_SIZE;

        let mut prot = 0;
        if (sec.characteristics & IMAGE_SCN_MEM_READ) != 0 {
            prot |= libc::PROT_READ;
        }
        if (sec.characteristics & IMAGE_SCN_MEM_WRITE) != 0 {
            prot |= libc::PROT_WRITE;
        }
        if (sec.characteristics & IMAGE_SCN_MEM_EXECUTE) != 0 {
            prot |= libc::PROT_EXEC;
        }

        for p in page_prots
            .iter_mut()
            .take(end_page.min(page_count))
            .skip(start_page)
        {
            *p |= prot;
        }
    }

    // Applique `mprotect` par plages contiguës de permissions identiques.
    let mut idx = 0usize;
    while idx < page_count {
        let prot = page_prots[idx];
        let start = idx;
        while idx < page_count && page_prots[idx] == prot {
            idx += 1;
        }
        let addr = (base + start * PAGE_SIZE) as *mut libc::c_void;
        let len = (idx - start) * PAGE_SIZE;
        // SAFETY: plage alignée sur `PAGE_SIZE` à l'intérieur de notre allocation `mmap`.
        let rc = unsafe { libc::mprotect(addr, len, prot) };
        if rc != 0 {
            sb_warn!(
                "loader",
                "mprotect({addr:p}, {len:#x}, {prot:#x}) a échoué : {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

#[cfg(test)]
mod guest_list_tests {
    use super::parse_guest_dll_list;

    #[test]
    fn guest_dll_list_is_normalized() {
        assert!(parse_guest_dll_list(None).is_empty());
        assert_eq!(
            parse_guest_dll_list(Some("MSVCP140.dll, concrt140;vcomp140".into())),
            ["msvcp140", "concrt140", "vcomp140"]
        );
    }
}
