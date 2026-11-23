//! `XGameSaveImpl` (clsid `704c3f58-e629-4cc2-b197-30511b996fe2`) : sauvegardes de jeu hors ligne.
//!
//! Vtable : 3 (IUnknown) + 30 méthodes = 33 entrées (`IXGameSaveImpl` = slots 3-29,
//! `IXGameSaveImpl2` ajoute les 3 slots `XGameSaveFiles*` 30-32, `IXGameSaveImpl3` aucun).
//!
//! # Stockage
//!
//! Tout passe par le VFS (`save:/`) sous `save:/.xgamesave/<config>/<conteneur>/<blob>`. Les noms
//! de configuration, de conteneur et de blob sont encodés en `%XX` (tout octet hors
//! `[A-Za-z0-9_-]`, et le `.` initial) : aucun séparateur de chemin, aucun `..`, aucun nom commençant
//! par `@` (préfixe réservé aux fichiers internes `@display` et `@tmp-*`). Une mise à jour écrit
//! chaque blob dans un fichier temporaire voisin puis le renomme (atomique par blob).
//!
//! # Règles documentées
//!
//! Nom de conteneur : partie chemin `[A-Za-z0-9_/]*` suivie d'un nom `[A-Za-z0-9_.-]+` ne finissant
//! pas par `.` et sans `..`, 256 caractères au plus (`E_GS_INVALID_CONTAINER_NAME`). Un blob au
//! plus par mise à jour (écriture ou suppression). Mise à jour > `GS_MAX_BLOB_SIZE` (16 Mio)
//! → `E_GS_UPDATE_TOO_BIG`. Tampon de lecture trop petit → `E_GS_PROVIDED_BUFFER_TOO_SMALL`,
//! blob absent → `E_GS_BLOB_NOT_FOUND`.
//!
//! Limite d'implémentation : le nom encodé (un fichier/dossier hôte) tient en 255 octets, donc un
//! conteneur de 256 caractères tous « sûrs » est refusé et un nom avec beaucoup de `/` ou `.` encore plus tôt.
//!
//! ASSUMPTION : nom de blob 256 octets au plus (`GS_MAX_BLOB_NAME_SIZE`), quota de 1 Gio quand le
//! VFS n'en définit pas, `blobNames == NULL` lit tous les blobs, `GetContainerInfo` d'un conteneur
//! absent renvoie `S_OK` sans rappel, tampon de lecture requis = tableau de `XGameSaveBlob` + octets
//! des blobs (les noms pointent vers des chaînes internes au runtime, valables toute la session).

use core::ffi::c_void;
use core::mem::{offset_of, size_of};
use std::collections::{BTreeMap, HashMap};
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, OnceLock};
use std::time::UNIX_EPOCH;

use parking_lot::Mutex;
use sb_common::abi::{E_FAIL, E_INVALIDARG, HResult, S_OK};
use sb_common::{ffi_guard, sb_debug};

use super::{
    AsyncPayload, CHANNEL, HandleTable, async_check, async_complete, async_peek_size, async_take,
    guid, read_cstr, singleton, write_cstr_buf, write_out,
};
use crate::com::{
    ComHeader, Guid, VtblSlot, com_add_ref, com_query_interface_list, com_release, vslot,
};
use crate::gdk_classes::ClassDef;
use crate::gdk_xs::XAsyncBlock;

const NAME: &str = "XGameSaveImpl";
pub(super) const CLSID: Guid = guid("704c3f58-e629-4cc2-b197-30511b996fe2");
const IID_IMPL2: Guid = guid("704c3f58-e629-4cc2-b197-30511b996ee2");
const IID_IMPL3: Guid = guid("1bfff3af-f14a-40a3-8e35-9ada906593f9");
const IIDS: [Guid; 3] = [CLSID, IID_IMPL2, IID_IMPL3];

/// Nombre d'entrées de la vtable (IUnknown + 30 méthodes).
pub(super) const VTBL_SLOTS: usize = 33;

pub(super) const E_GS_INVALID_CONTAINER_NAME: HResult = 0x8083_0001_u32 as i32;
pub(super) const E_GS_NO_ACCESS: HResult = 0x8083_0002_u32 as i32;
pub(super) const E_GS_OUT_OF_LOCAL_STORAGE: HResult = 0x8083_0003_u32 as i32;
pub(super) const E_GS_UPDATE_TOO_BIG: HResult = 0x8083_0005_u32 as i32;
pub(super) const E_GS_QUOTA_EXCEEDED: HResult = 0x8083_0006_u32 as i32;
pub(super) const E_GS_PROVIDED_BUFFER_TOO_SMALL: HResult = 0x8083_0007_u32 as i32;
pub(super) const E_GS_BLOB_NOT_FOUND: HResult = 0x8083_0008_u32 as i32;
pub(super) const E_GS_NO_SERVICE_CONFIGURATION: HResult = 0x8083_0009_u32 as i32;

const ROOT: &str = ".xgamesave";
const FILES_ROOT: &str = ".xgamesave-files";
/// `GS_MAX_BLOB_SIZE` : taille maximale d'une mise à jour.
pub(super) const GS_MAX_BLOB_SIZE: usize = 16 * 1024 * 1024;
const GS_MAX_BLOB_NAME_SIZE: usize = 256;
const MAX_CONTAINER_NAME: usize = 256;
/// Longueur maximale d'un nom de fichier encodé (`NAME_MAX` = 255).
const MAX_ENCODED: usize = 255;
const DEFAULT_QUOTA: i64 = 1 << 30;
const DISPLAY_FILE: &str = "@display";
const TMP_PREFIX: &str = "@tmp-";

// ---------------------------------------------------------------------------------------------
// Structures partagées avec l'invité
// ---------------------------------------------------------------------------------------------

#[repr(C)]
struct BlobInfo {
    name: *const u8,
    size: u32,
}

#[repr(C)]
struct Blob {
    info: BlobInfo,
    data: *mut u8,
}

#[repr(C)]
struct ContainerInfo {
    name: *const u8,
    display_name: *const u8,
    blob_count: u32,
    total_size: u64,
    last_modified_time: u64,
    needs_sync: u8,
}

const _: () = assert!(size_of::<BlobInfo>() == 16);
const _: () = assert!(size_of::<Blob>() == 24);
const _: () = assert!(offset_of!(Blob, data) == 16);
const _: () = assert!(size_of::<ContainerInfo>() == 48);
const _: () = assert!(offset_of!(ContainerInfo, blob_count) == 16);
const _: () = assert!(offset_of!(ContainerInfo, total_size) == 24);
const _: () = assert!(offset_of!(ContainerInfo, last_modified_time) == 32);
const _: () = assert!(offset_of!(ContainerInfo, needs_sync) == 40);

type ContainerCb = unsafe extern "win64" fn(*const ContainerInfo, *mut c_void) -> u8;
type BlobCb = unsafe extern "win64" fn(*const BlobInfo, *mut c_void) -> u8;

// ---------------------------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------------------------

struct Provider {
    /// Identifiant de configuration encodé (SCID).
    cfg: String,
}

#[derive(Clone)]
struct Container {
    cfg: String,
    name: String,
}

struct Update {
    container: Container,
    display_name: String,
    /// `Some(octets)` = écriture, `None` = suppression ; un seul enregistrement par blob.
    ops: BTreeMap<String, Option<Vec<u8>>>,
    total: usize,
}

static PROVIDERS: LazyLock<HandleTable<Provider>> = LazyLock::new(|| HandleTable::new(0x6A00_0010));
static CONTAINERS: LazyLock<HandleTable<Container>> =
    LazyLock::new(|| HandleTable::new(0x6B00_0010));
static UPDATES: LazyLock<HandleTable<Update>> = LazyLock::new(|| HandleTable::new(0x6C00_0010));

// ---------------------------------------------------------------------------------------------
// Noms et chemins
// ---------------------------------------------------------------------------------------------

/// Encode `raw` en `%XX` : seuls `[A-Za-z0-9_-]` (et `.` hors première position) restent tels quels.
fn encode_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for (i, b) in raw.bytes().enumerate() {
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || (b == b'.' && i > 0) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Inverse de [`encode_name`] ; `None` si l'entrée n'est pas un nom encodé valide.
fn decode_name(enc: &str) -> Option<String> {
    let b = enc.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = enc.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn container_name_valid(n: &str) -> bool {
    if n.is_empty() || n.len() > MAX_CONTAINER_NAME || encode_name(n).len() > MAX_ENCODED {
        return false;
    }
    let (path, file) = match n.rfind('/') {
        Some(i) => (&n[..=i], &n[i + 1..]),
        None => ("", n),
    };
    path.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'/')
        && !file.is_empty()
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        && !file.ends_with('.')
        && !file.contains("..")
}

fn blob_name_valid(n: &str) -> bool {
    !n.is_empty() && n.len() <= GS_MAX_BLOB_NAME_SIZE && encode_name(n).len() <= MAX_ENCODED
}

fn host_path(guest: &str) -> Result<PathBuf, HResult> {
    crate::file_io::resolve_vfs_path(guest).ok_or(E_GS_NO_ACCESS)
}

fn cfg_dir(cfg: &str) -> Result<PathBuf, HResult> {
    host_path(&format!("save:/{ROOT}/{cfg}"))
}

fn container_dir(c: &Container) -> Result<PathBuf, HResult> {
    host_path(&format!("save:/{ROOT}/{}/{}", c.cfg, encode_name(&c.name)))
}

fn io_to_hr(e: &std::io::Error) -> HResult {
    match e.kind() {
        std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded => {
            E_GS_OUT_OF_LOCAL_STORAGE
        }
        std::io::ErrorKind::PermissionDenied => E_GS_NO_ACCESS,
        _ => E_FAIL,
    }
}

/// Quota restant : celui du VFS (`save:/`) s'il en définit un, 1 Gio sinon.
fn quota_remaining() -> i64 {
    let st = crate::process::process_state().read();
    match st.vfs.as_ref() {
        Some(v) => match v.save_quota_bytes() {
            Some(q) => q.saturating_sub(v.save_usage_bytes().unwrap_or(0)) as i64,
            None => DEFAULT_QUOTA,
        },
        None => DEFAULT_QUOTA,
    }
}

/// Chaîne interne au runtime (valable toute la session) pour les `const char*` rendus au titre.
fn intern(s: &str) -> *const u8 {
    static POOL: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    let mut pool = POOL.get_or_init(|| Mutex::new(HashMap::new())).lock();
    if let Some(p) = pool.get(s) {
        return *p as *const u8;
    }
    let c = CString::new(s.replace('\0', "")).unwrap_or_default();
    let p = Box::leak(c.into_boxed_c_str()).as_ptr() as *const u8;
    pool.insert(s.to_string(), p as usize);
    p
}

// ---------------------------------------------------------------------------------------------
// Parcours des conteneurs et des blobs
// ---------------------------------------------------------------------------------------------

struct BlobEntry {
    name: String,
    size: u64,
    modified: u64,
}

fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

fn list_blobs(dir: &Path) -> Vec<BlobEntry> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let fname = e.file_name().to_string_lossy().into_owned();
            if fname.starts_with('@') {
                continue;
            }
            let (Some(name), Ok(meta)) = (decode_name(&fname), e.metadata()) else {
                continue;
            };
            if meta.is_file() {
                v.push(BlobEntry {
                    name,
                    size: meta.len(),
                    modified: mtime_secs(&meta),
                });
            }
        }
    }
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

fn list_container_names(cfg: &str) -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(dir) = cfg_dir(cfg)
        && let Ok(rd) = std::fs::read_dir(dir)
    {
        for e in rd.flatten() {
            let fname = e.file_name().to_string_lossy().into_owned();
            if fname.starts_with('@') || !e.metadata().is_ok_and(|m| m.is_dir()) {
                continue;
            }
            if let Some(n) = decode_name(&fname) {
                v.push(n);
            }
        }
    }
    v.sort();
    v
}

/// Appelle `cb` avec les informations du conteneur ; renvoie `false` si le rappel demande l'arrêt.
fn emit_container_info(c: &Container, cb: ContainerCb, ctx: *mut c_void) -> Result<bool, HResult> {
    let dir = container_dir(c)?;
    let blobs = list_blobs(&dir);
    let display = std::fs::read(dir.join(DISPLAY_FILE))
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let modified = blobs
        .iter()
        .map(|b| b.modified)
        .max()
        .or_else(|| std::fs::metadata(&dir).ok().map(|m| mtime_secs(&m)))
        .unwrap_or(0);
    let info = ContainerInfo {
        name: intern(&c.name),
        display_name: intern(&display),
        blob_count: blobs.len() as u32,
        total_size: blobs.iter().map(|b| b.size).sum(),
        last_modified_time: modified,
        needs_sync: 0,
    };
    // SAFETY: rappel `BOOLEAN CALLBACK(const XGameSaveContainerInfo*, void*)` fourni par le titre ;
    // `info` et les chaînes internes restent valides pendant l'appel.
    let keep_going = unsafe { cb(&info, ctx) };
    Ok(keep_going != 0)
}

// ---------------------------------------------------------------------------------------------
// Fournisseur
// ---------------------------------------------------------------------------------------------

fn init_provider(user: usize, cfg_ptr: *const u8) -> Result<usize, HResult> {
    if user == 0 {
        return Err(E_INVALIDARG);
    }
    let cfg = read_cstr(cfg_ptr).ok_or(E_INVALIDARG)?;
    if cfg.is_empty() {
        return Err(E_GS_NO_SERVICE_CONFIGURATION);
    }
    let enc = encode_name(&cfg);
    if enc.len() > MAX_ENCODED {
        return Err(E_INVALIDARG);
    }
    let dir = cfg_dir(&enc)?;
    std::fs::create_dir_all(&dir).map_err(|e| io_to_hr(&e))?;
    sb_debug!(
        CHANNEL,
        "XGameSave : fournisseur pour la configuration '{cfg}'"
    );
    Ok(PROVIDERS.insert(Provider { cfg: enc }))
}

fn provider_cfg(h: usize) -> Result<String, HResult> {
    PROVIDERS.with(h, |p| p.cfg.clone()).ok_or(E_INVALIDARG)
}

fn quota_for(h: usize) -> Result<i64, HResult> {
    provider_cfg(h)?;
    Ok(quota_remaining())
}

fn delete_container(h: usize, name_ptr: *const u8) -> Result<(), HResult> {
    let cfg = provider_cfg(h)?;
    let name = read_cstr(name_ptr).ok_or(E_INVALIDARG)?;
    if !container_name_valid(&name) {
        return Err(E_GS_INVALID_CONTAINER_NAME);
    }
    let dir = container_dir(&Container { cfg, name })?;
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_to_hr(&e)),
    }
}

/// Une erreur d'arguments (`E_INVALIDARG`) est renvoyée de façon synchrone par l'appel `…Async` ;
/// toute autre issue complète le bloc.
fn finish<T>(
    block: *mut XAsyncBlock,
    res: Result<T, HResult>,
    to_payload: impl FnOnce(T) -> (usize, AsyncPayload),
) -> HResult {
    if res.as_ref().err() == Some(&E_INVALIDARG) {
        return E_INVALIDARG;
    }
    async_complete(block, res.map(to_payload))
}

// ---------------------------------------------------------------------------------------------
// Lecture
// ---------------------------------------------------------------------------------------------

fn read_names(names: *const *const u8, count: u32) -> Result<Option<Vec<String>>, HResult> {
    if names.is_null() {
        return Ok(None);
    }
    let mut v = Vec::new();
    for i in 0..count as usize {
        // SAFETY: `names` pointe vers `count` chaînes C (`_In_opt_z_count_(countOfBlobs)`).
        let p = unsafe { *names.add(i) };
        v.push(read_cstr(p).ok_or(E_INVALIDARG)?);
    }
    Ok(Some(v))
}

fn load_blobs(
    c: &Container,
    names: Option<Vec<String>>,
) -> Result<Vec<(String, Vec<u8>)>, HResult> {
    let dir = container_dir(c)?;
    let wanted: Vec<String> = match names {
        Some(n) => n,
        None => list_blobs(&dir).into_iter().map(|b| b.name).collect(),
    };
    let mut out = Vec::with_capacity(wanted.len());
    for n in wanted {
        if !blob_name_valid(&n) {
            return Err(E_INVALIDARG);
        }
        match std::fs::read(dir.join(encode_name(&n))) {
            Ok(data) => out.push((n, data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(E_GS_BLOB_NOT_FOUND),
            Err(e) => return Err(io_to_hr(&e)),
        }
    }
    Ok(out)
}

fn required_size(blobs: &[(String, Vec<u8>)]) -> usize {
    blobs.len() * size_of::<Blob>() + blobs.iter().map(|(_, d)| d.len()).sum::<usize>()
}

/// Remplit le tampon du titre : tableau de `XGameSaveBlob` puis octets de chaque blob.
fn fill_blobs(blobs: &[(String, Vec<u8>)], size: usize, out: *mut u8) -> Result<u32, HResult> {
    if blobs.is_empty() {
        return Ok(0);
    }
    if out.is_null() {
        return Err(E_INVALIDARG);
    }
    if size < required_size(blobs) {
        return Err(E_GS_PROVIDED_BUFFER_TOO_SMALL);
    }
    let mut data_off = blobs.len() * size_of::<Blob>();
    for (i, (name, data)) in blobs.iter().enumerate() {
        // SAFETY: `size >= required_size` : le tableau puis les données tiennent dans `out`.
        unsafe {
            let data_ptr = out.add(data_off);
            core::ptr::copy_nonoverlapping(data.as_ptr(), data_ptr, data.len());
            let blob = Blob {
                info: BlobInfo {
                    name: intern(name),
                    size: data.len() as u32,
                },
                data: data_ptr,
            };
            out.add(i * size_of::<Blob>())
                .cast::<Blob>()
                .write_unaligned(blob);
        }
        data_off += data.len();
    }
    Ok(blobs.len() as u32)
}

// ---------------------------------------------------------------------------------------------
// Mise à jour
// ---------------------------------------------------------------------------------------------

fn create_update(cont: usize, display_ptr: *const u8) -> Result<usize, HResult> {
    let container = CONTAINERS.with(cont, |c| c.clone()).ok_or(E_INVALIDARG)?;
    let display_name = read_cstr(display_ptr).ok_or(E_INVALIDARG)?;
    Ok(UPDATES.insert(Update {
        container,
        display_name,
        ops: BTreeMap::new(),
        total: 0,
    }))
}

fn blob_write(upd: usize, name_ptr: *const u8, data: *const u8, len: usize) -> Result<(), HResult> {
    let name = read_cstr(name_ptr).ok_or(E_INVALIDARG)?;
    if !blob_name_valid(&name) || (data.is_null() && len > 0) {
        return Err(E_INVALIDARG);
    }
    if len > GS_MAX_BLOB_SIZE {
        return Err(E_GS_UPDATE_TOO_BIG);
    }
    UPDATES
        .with(upd, |u| {
            if u.ops.contains_key(&name) {
                return Err(E_INVALIDARG); // une seule modification par blob et par mise à jour
            }
            if u.total + len > GS_MAX_BLOB_SIZE {
                return Err(E_GS_UPDATE_TOO_BIG);
            }
            let bytes = if len == 0 {
                Vec::new()
            } else {
                // SAFETY: `data` pointe vers `len` octets (`_In_reads_bytes_(byteCount)`).
                unsafe { core::slice::from_raw_parts(data, len) }.to_vec()
            };
            u.total += len;
            u.ops.insert(name, Some(bytes));
            Ok(())
        })
        .ok_or(E_INVALIDARG)?
}

fn blob_delete(upd: usize, name_ptr: *const u8) -> Result<(), HResult> {
    let name = read_cstr(name_ptr).ok_or(E_INVALIDARG)?;
    if !blob_name_valid(&name) {
        return Err(E_INVALIDARG);
    }
    UPDATES
        .with(upd, |u| {
            if u.ops.contains_key(&name) {
                return Err(E_INVALIDARG);
            }
            u.ops.insert(name, None);
            Ok(())
        })
        .ok_or(E_INVALIDARG)?
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn write_temp(dir: &Path, data: &[u8]) -> Result<PathBuf, HResult> {
    use std::io::Write;
    let tmp = dir.join(format!(
        "{TMP_PREFIX}{}-{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let res = std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(data)?;
        f.sync_all()
    });
    match res {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(io_to_hr(&e))
        }
    }
}

fn submit_update(upd: usize) -> Result<(), HResult> {
    let (container, display, ops, total) = UPDATES
        .with(upd, |u| {
            (
                u.container.clone(),
                u.display_name.clone(),
                u.ops.clone(),
                u.total,
            )
        })
        .ok_or(E_INVALIDARG)?;
    if total > GS_MAX_BLOB_SIZE {
        return Err(E_GS_UPDATE_TOO_BIG);
    }
    let dir = container_dir(&container)?;
    let replaced: u64 = ops
        .iter()
        .filter(|(_, v)| v.is_some())
        .filter_map(|(n, _)| std::fs::metadata(dir.join(encode_name(n))).ok())
        .map(|m| m.len())
        .sum();
    let additional = (total as u64).saturating_sub(replaced);
    if additional as i64 > quota_remaining() {
        return Err(E_GS_QUOTA_EXCEEDED);
    }
    std::fs::create_dir_all(&dir).map_err(|e| io_to_hr(&e))?;

    // Phase 1 : tout est écrit dans des fichiers temporaires voisins.
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    let cleanup = |staged: &[(PathBuf, PathBuf)]| {
        for (tmp, _) in staged {
            let _ = std::fs::remove_file(tmp);
        }
    };
    for (name, op) in &ops {
        if let Some(data) = op {
            match write_temp(&dir, data) {
                Ok(tmp) => staged.push((tmp, dir.join(encode_name(name)))),
                Err(hr) => {
                    cleanup(&staged);
                    return Err(hr);
                }
            }
        }
    }
    match write_temp(&dir, display.as_bytes()) {
        Ok(tmp) => staged.push((tmp, dir.join(DISPLAY_FILE))),
        Err(hr) => {
            cleanup(&staged);
            return Err(hr);
        }
    }
    // Phase 2 : renommages (atomiques par fichier), puis suppressions.
    for (i, (tmp, dest)) in staged.iter().enumerate() {
        if let Err(e) = std::fs::rename(tmp, dest) {
            cleanup(&staged[i..]);
            return Err(io_to_hr(&e));
        }
    }
    for (name, op) in &ops {
        if op.is_none() {
            match std::fs::remove_file(dir.join(encode_name(name))) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_to_hr(&e)),
            }
        }
    }
    UPDATES.with(upd, |u| {
        u.ops.clear();
        u.total = 0;
    });
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// XGameSaveFiles*
// ---------------------------------------------------------------------------------------------

/// Dossier invité des fichiers de sauvegarde pour une configuration (créé dans le VFS).
fn files_folder(user: usize, cfg_ptr: *const u8) -> Result<String, HResult> {
    if user == 0 {
        return Err(E_INVALIDARG);
    }
    let cfg = read_cstr(cfg_ptr).ok_or(E_INVALIDARG)?;
    if cfg.is_empty() {
        return Err(E_GS_NO_SERVICE_CONFIGURATION);
    }
    let enc = encode_name(&cfg);
    if enc.len() > MAX_ENCODED {
        return Err(E_INVALIDARG);
    }
    let host = host_path(&format!("save:/{FILES_ROOT}/{enc}"))?;
    std::fs::create_dir_all(&host).map_err(|e| io_to_hr(&e))?;
    Ok(format!("S:\\{FILES_ROOT}\\{enc}"))
}

// ---------------------------------------------------------------------------------------------
// Slots
// ---------------------------------------------------------------------------------------------

extern "win64" fn qi(this: *mut ComHeader, riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    com_query_interface_list(NAME, this, riid, ppv, &IIDS)
}

// 3
extern "win64" fn initialize_provider(
    _this: *mut c_void,
    user: usize,
    cfg: *const u8,
    _sync_on_demand: u8,
    out: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveInitializeProvider", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, 0);
        match init_provider(user, cfg) {
            Ok(h) => {
                write_out(out, h);
                S_OK
            }
            Err(hr) => hr,
        }
    })
}

// 4
extern "win64" fn initialize_provider_async(
    _this: *mut c_void,
    user: usize,
    cfg: *const u8,
    _sync_on_demand: u8,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveInitializeProviderAsync", E_FAIL, {
        if let Err(hr) = async_check(block) {
            return hr;
        }
        finish(block, init_provider(user, cfg), |h| {
            (size_of::<usize>(), AsyncPayload::Handle(h))
        })
    })
}

// 5
extern "win64" fn initialize_provider_result(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    out: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveInitializeProviderResult", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, 0);
        match async_take(block, |p| matches!(p, AsyncPayload::Handle(_))) {
            Ok(rec) => {
                if let AsyncPayload::Handle(h) = rec.payload {
                    write_out(out, h);
                }
                S_OK
            }
            Err(hr) => hr,
        }
    })
}

// 6
extern "win64" fn close_provider(_this: *mut c_void, provider: usize) {
    ffi_guard!(CHANNEL, "XGameSaveCloseProvider", (), {
        PROVIDERS.remove(provider);
    })
}

// 7
extern "win64" fn get_remaining_quota(
    _this: *mut c_void,
    provider: usize,
    out: *mut i64,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveGetRemainingQuota", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        match quota_for(provider) {
            Ok(q) => {
                write_out(out, q);
                S_OK
            }
            Err(hr) => hr,
        }
    })
}

// 8
extern "win64" fn get_remaining_quota_async(
    _this: *mut c_void,
    provider: usize,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveGetRemainingQuotaAsync", E_FAIL, {
        if let Err(hr) = async_check(block) {
            return hr;
        }
        finish(block, quota_for(provider), |q| {
            (size_of::<i64>(), AsyncPayload::Quota(q))
        })
    })
}

// 9
extern "win64" fn get_remaining_quota_result(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    out: *mut i64,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveGetRemainingQuotaResult", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        match async_take(block, |p| matches!(p, AsyncPayload::Quota(_))) {
            Ok(rec) => {
                if let AsyncPayload::Quota(q) = rec.payload {
                    write_out(out, q);
                }
                S_OK
            }
            Err(hr) => hr,
        }
    })
}

// 10
extern "win64" fn delete_container_slot(
    _this: *mut c_void,
    provider: usize,
    name: *const u8,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveDeleteContainer", E_FAIL, {
        delete_container(provider, name).err().unwrap_or(S_OK)
    })
}

// 11
extern "win64" fn delete_container_async(
    _this: *mut c_void,
    provider: usize,
    name: *const u8,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveDeleteContainerAsync", E_FAIL, {
        if let Err(hr) = async_check(block) {
            return hr;
        }
        finish(block, delete_container(provider, name), |()| {
            (0, AsyncPayload::None)
        })
    })
}

// 12
extern "win64" fn delete_container_result(_this: *mut c_void, block: *mut XAsyncBlock) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveDeleteContainerResult", E_FAIL, {
        async_take(block, |_| true).err().unwrap_or(S_OK)
    })
}

// 13
extern "win64" fn get_container_info(
    _this: *mut c_void,
    provider: usize,
    name: *const u8,
    ctx: *mut c_void,
    cb: Option<ContainerCb>,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveGetContainerInfo", E_FAIL, {
        let Some(cb) = cb else {
            return E_INVALIDARG;
        };
        let cfg = match provider_cfg(provider) {
            Ok(c) => c,
            Err(hr) => return hr,
        };
        let Some(n) = read_cstr(name) else {
            return E_INVALIDARG;
        };
        if !container_name_valid(&n) {
            return E_GS_INVALID_CONTAINER_NAME;
        }
        let c = Container { cfg, name: n };
        match container_dir(&c) {
            Ok(dir) if dir.is_dir() => emit_container_info(&c, cb, ctx).err().unwrap_or(S_OK),
            Ok(_) => S_OK,
            Err(hr) => hr,
        }
    })
}

fn enumerate_containers(
    provider: usize,
    prefix: Option<String>,
    ctx: *mut c_void,
    cb: Option<ContainerCb>,
) -> HResult {
    let Some(cb) = cb else {
        return E_INVALIDARG;
    };
    let cfg = match provider_cfg(provider) {
        Ok(c) => c,
        Err(hr) => return hr,
    };
    for name in list_container_names(&cfg) {
        if prefix.as_deref().is_some_and(|p| !name.starts_with(p)) {
            continue;
        }
        let c = Container {
            cfg: cfg.clone(),
            name,
        };
        match emit_container_info(&c, cb, ctx) {
            Ok(true) => {}
            Ok(false) => break,
            Err(hr) => return hr,
        }
    }
    S_OK
}

// 14
extern "win64" fn enumerate_container_info(
    _this: *mut c_void,
    provider: usize,
    ctx: *mut c_void,
    cb: Option<ContainerCb>,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveEnumerateContainerInfo", E_FAIL, {
        enumerate_containers(provider, None, ctx, cb)
    })
}

// 15
extern "win64" fn enumerate_container_info_by_name(
    _this: *mut c_void,
    provider: usize,
    prefix: *const u8,
    ctx: *mut c_void,
    cb: Option<ContainerCb>,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveEnumerateContainerInfoByName", E_FAIL, {
        let prefix = if prefix.is_null() {
            None
        } else {
            match read_cstr(prefix) {
                Some(p) => Some(p),
                None => return E_INVALIDARG,
            }
        };
        enumerate_containers(provider, prefix, ctx, cb)
    })
}

// 16
extern "win64" fn create_container(
    _this: *mut c_void,
    provider: usize,
    name: *const u8,
    out: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveCreateContainer", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, 0);
        let cfg = match provider_cfg(provider) {
            Ok(c) => c,
            Err(hr) => return hr,
        };
        let Some(n) = read_cstr(name) else {
            return E_INVALIDARG;
        };
        if !container_name_valid(&n) {
            return E_GS_INVALID_CONTAINER_NAME;
        }
        write_out(out, CONTAINERS.insert(Container { cfg, name: n }));
        S_OK
    })
}

// 17
extern "win64" fn close_container(_this: *mut c_void, container: usize) {
    ffi_guard!(CHANNEL, "XGameSaveCloseContainer", (), {
        CONTAINERS.remove(container);
    })
}

fn enumerate_blobs(
    container: usize,
    prefix: Option<String>,
    ctx: *mut c_void,
    cb: Option<BlobCb>,
) -> HResult {
    let Some(cb) = cb else {
        return E_INVALIDARG;
    };
    let Some(c) = CONTAINERS.with(container, |c| c.clone()) else {
        return E_INVALIDARG;
    };
    let dir = match container_dir(&c) {
        Ok(d) => d,
        Err(hr) => return hr,
    };
    for b in list_blobs(&dir) {
        if prefix.as_deref().is_some_and(|p| !b.name.starts_with(p)) {
            continue;
        }
        let info = BlobInfo {
            name: intern(&b.name),
            size: b.size as u32,
        };
        // SAFETY: rappel `BOOLEAN CALLBACK(const XGameSaveBlobInfo*, void*)` fourni par le titre.
        if unsafe { cb(&info, ctx) } == 0 {
            break;
        }
    }
    S_OK
}

// 18
extern "win64" fn enumerate_blob_info(
    _this: *mut c_void,
    container: usize,
    ctx: *mut c_void,
    cb: Option<BlobCb>,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveEnumerateBlobInfo", E_FAIL, {
        enumerate_blobs(container, None, ctx, cb)
    })
}

// 19
extern "win64" fn enumerate_blob_info_by_name(
    _this: *mut c_void,
    container: usize,
    prefix: *const u8,
    ctx: *mut c_void,
    cb: Option<BlobCb>,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveEnumerateBlobInfoByName", E_FAIL, {
        let prefix = if prefix.is_null() {
            None
        } else {
            match read_cstr(prefix) {
                Some(p) => Some(p),
                None => return E_INVALIDARG,
            }
        };
        enumerate_blobs(container, prefix, ctx, cb)
    })
}

// 20
extern "win64" fn read_blob_data(
    _this: *mut c_void,
    container: usize,
    names: *const *const u8,
    count: *mut u32,
    blobs_size: usize,
    out: *mut u8,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveReadBlobData", E_FAIL, {
        if count.is_null() {
            return E_INVALIDARG;
        }
        let Some(c) = CONTAINERS.with(container, |c| c.clone()) else {
            return E_INVALIDARG;
        };
        // SAFETY: `count` non nul (`_Inout_`).
        let wanted = unsafe { count.read_unaligned() };
        let names = match read_names(names, wanted) {
            Ok(n) => n,
            Err(hr) => return hr,
        };
        let blobs = match load_blobs(&c, names) {
            Ok(b) => b,
            Err(hr) => return hr,
        };
        match fill_blobs(&blobs, blobs_size, out) {
            Ok(n) => {
                write_out(count, n);
                S_OK
            }
            Err(hr) => hr,
        }
    })
}

// 21
extern "win64" fn read_blob_data_async(
    _this: *mut c_void,
    container: usize,
    names: *const *const u8,
    count: u32,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveReadBlobDataAsync", E_FAIL, {
        if let Err(hr) = async_check(block) {
            return hr;
        }
        let Some(c) = CONTAINERS.with(container, |c| c.clone()) else {
            return E_INVALIDARG;
        };
        let names = match read_names(names, count) {
            Ok(n) => n,
            Err(hr) => return hr,
        };
        finish(block, load_blobs(&c, names), |blobs| {
            (required_size(&blobs), AsyncPayload::Blobs(blobs))
        })
    })
}

// 22
extern "win64" fn read_blob_data_result(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    blobs_size: usize,
    out: *mut u8,
    count: *mut u32,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveReadBlobDataResult", E_FAIL, {
        if count.is_null() {
            return E_INVALIDARG;
        }
        match async_peek_size(block) {
            // Tampon trop petit : l'entrée est conservée pour permettre un nouvel essai.
            Ok(needed) if blobs_size < needed => return E_GS_PROVIDED_BUFFER_TOO_SMALL,
            Ok(_) => {}
            Err(hr) => {
                // Échec mémorisé : restitué (et consommé) par `async_take`.
                let _ = async_take(block, |_| true);
                return hr;
            }
        }
        match async_take(block, |p| matches!(p, AsyncPayload::Blobs(_))) {
            Ok(rec) => {
                let AsyncPayload::Blobs(blobs) = rec.payload else {
                    return E_INVALIDARG;
                };
                match fill_blobs(&blobs, blobs_size, out) {
                    Ok(n) => {
                        write_out(count, n);
                        S_OK
                    }
                    Err(hr) => hr,
                }
            }
            Err(hr) => hr,
        }
    })
}

// 23
extern "win64" fn create_update_slot(
    _this: *mut c_void,
    container: usize,
    display: *const u8,
    out: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveCreateUpdate", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, 0);
        match create_update(container, display) {
            Ok(h) => {
                write_out(out, h);
                S_OK
            }
            Err(hr) => hr,
        }
    })
}

// 24
extern "win64" fn close_update(_this: *mut c_void, update: usize) {
    ffi_guard!(CHANNEL, "XGameSaveCloseUpdate", (), {
        UPDATES.remove(update);
    })
}

// 25
extern "win64" fn submit_blob_write(
    _this: *mut c_void,
    update: usize,
    name: *const u8,
    data: *const u8,
    len: usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveSubmitBlobWrite", E_FAIL, {
        blob_write(update, name, data, len).err().unwrap_or(S_OK)
    })
}

// 26
extern "win64" fn submit_blob_delete(
    _this: *mut c_void,
    update: usize,
    name: *const u8,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveSubmitBlobDelete", E_FAIL, {
        blob_delete(update, name).err().unwrap_or(S_OK)
    })
}

// 27
extern "win64" fn submit_update_slot(_this: *mut c_void, update: usize) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveSubmitUpdate", E_FAIL, {
        submit_update(update).err().unwrap_or(S_OK)
    })
}

// 28
extern "win64" fn submit_update_async(
    _this: *mut c_void,
    update: usize,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveSubmitUpdateAsync", E_FAIL, {
        if let Err(hr) = async_check(block) {
            return hr;
        }
        finish(block, submit_update(update), |()| (0, AsyncPayload::None))
    })
}

// 29
extern "win64" fn submit_update_result(_this: *mut c_void, block: *mut XAsyncBlock) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveSubmitUpdateResult", E_FAIL, {
        async_take(block, |_| true).err().unwrap_or(S_OK)
    })
}

// 30
extern "win64" fn files_get_folder_with_ui_async(
    _this: *mut c_void,
    user: usize,
    cfg: *const u8,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveFilesGetFolderWithUiAsync", E_FAIL, {
        if let Err(hr) = async_check(block) {
            return hr;
        }
        finish(block, files_folder(user, cfg), |p| {
            (p.len() + 1, AsyncPayload::Text(p))
        })
    })
}

// 31
extern "win64" fn files_get_folder_with_ui_result(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    folder_size: usize,
    out: *mut u8,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveFilesGetFolderWithUiResult", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        match async_peek_size(block) {
            Ok(needed) if folder_size < needed => return E_GS_PROVIDED_BUFFER_TOO_SMALL,
            Ok(_) => {}
            Err(hr) => {
                let _ = async_take(block, |_| true);
                return hr;
            }
        }
        match async_take(block, |p| matches!(p, AsyncPayload::Text(_))) {
            Ok(rec) => {
                if let AsyncPayload::Text(path) = rec.payload {
                    write_cstr_buf(out, &path);
                }
                S_OK
            }
            Err(hr) => hr,
        }
    })
}

// 32
extern "win64" fn files_get_remaining_quota(
    _this: *mut c_void,
    user: usize,
    cfg: *const u8,
    out: *mut i64,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameSaveFilesGetRemainingQuota", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        if user == 0 {
            return E_INVALIDARG;
        }
        match read_cstr(cfg) {
            None => E_INVALIDARG,
            Some(c) if c.is_empty() => E_GS_NO_SERVICE_CONFIGURATION,
            Some(_) => {
                write_out(out, quota_remaining());
                S_OK
            }
        }
    })
}

static VTBL: [VtblSlot; VTBL_SLOTS] = [
    vslot(qi as *const c_void),
    vslot(com_add_ref as *const c_void),
    vslot(com_release as *const c_void),
    vslot(initialize_provider as *const c_void),
    vslot(initialize_provider_async as *const c_void),
    vslot(initialize_provider_result as *const c_void),
    vslot(close_provider as *const c_void),
    vslot(get_remaining_quota as *const c_void),
    vslot(get_remaining_quota_async as *const c_void),
    vslot(get_remaining_quota_result as *const c_void),
    vslot(delete_container_slot as *const c_void),
    vslot(delete_container_async as *const c_void),
    vslot(delete_container_result as *const c_void),
    vslot(get_container_info as *const c_void),
    vslot(enumerate_container_info as *const c_void),
    vslot(enumerate_container_info_by_name as *const c_void),
    vslot(create_container as *const c_void),
    vslot(close_container as *const c_void),
    vslot(enumerate_blob_info as *const c_void),
    vslot(enumerate_blob_info_by_name as *const c_void),
    vslot(read_blob_data as *const c_void),
    vslot(read_blob_data_async as *const c_void),
    vslot(read_blob_data_result as *const c_void),
    vslot(create_update_slot as *const c_void),
    vslot(close_update as *const c_void),
    vslot(submit_blob_write as *const c_void),
    vslot(submit_blob_delete as *const c_void),
    vslot(submit_update_slot as *const c_void),
    vslot(submit_update_async as *const c_void),
    vslot(submit_update_result as *const c_void),
    vslot(files_get_folder_with_ui_async as *const c_void),
    vslot(files_get_folder_with_ui_result as *const c_void),
    vslot(files_get_remaining_quota as *const c_void),
];

const _: () = assert!(VTBL.len() == 33);

fn create() -> *mut ComHeader {
    static OBJ: OnceLock<usize> = OnceLock::new();
    singleton(&OBJ, VTBL.as_ptr().cast())
}

pub(super) fn class() -> ClassDef {
    ClassDef {
        name: NAME,
        clsid: CLSID,
        extra_iids: &[IID_IMPL2, IID_IMPL3],
        create,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests_support::*;
    use super::*;
    use sb_common::abi::E_NOINTERFACE;

    fn cs(s: &str) -> CString {
        CString::new(s).expect("cstring")
    }

    fn provider(obj: *mut c_void, cfg: &str) -> usize {
        let c = cs(cfg);
        let mut p = 0usize;
        assert_eq!(
            vcall!(obj, 3, (usize, *const u8, u8, *mut usize) -> HResult, 1, c.as_ptr().cast(), 0, &mut p),
            S_OK
        );
        assert_ne!(p, 0);
        p
    }

    fn container(obj: *mut c_void, prov: usize, name: &str) -> usize {
        let n = cs(name);
        let mut h = 0usize;
        assert_eq!(
            vcall!(obj, 16, (usize, *const u8, *mut usize) -> HResult, prov, n.as_ptr().cast(), &mut h),
            S_OK
        );
        h
    }

    fn write_blobs(obj: *mut c_void, cont: usize, blobs: &[(&str, &[u8])]) -> HResult {
        let d = cs("Display");
        let mut u = 0usize;
        assert_eq!(
            vcall!(obj, 23, (usize, *const u8, *mut usize) -> HResult, cont, d.as_ptr().cast(), &mut u),
            S_OK
        );
        for (n, data) in blobs {
            let n = cs(n);
            let hr = vcall!(obj, 25, (usize, *const u8, *const u8, usize) -> HResult, u, n.as_ptr().cast(), data.as_ptr(), data.len());
            if hr != S_OK {
                vcall!(obj, 24, (usize) -> (), u);
                return hr;
            }
        }
        let hr = vcall!(obj, 27, (usize) -> HResult, u);
        vcall!(obj, 24, (usize) -> (), u);
        hr
    }

    #[test]
    fn vtable_has_33_slots_and_iunknown_works() {
        ensure_vfs();
        const _: () = assert!(VTBL_SLOTS == 33);
        let obj = query_class(&CLSID);
        // Tests parallèles : le singleton est partagé, on ne vérifie que la cohérence locale.
        let added = vcall!(obj, 1, () -> u32);
        assert!(added >= 2 && refcount(obj) >= 1);
        assert!(vcall!(obj, 2, () -> u32) >= 1);
        for iid in [CLSID, IID_IMPL2, IID_IMPL3] {
            let mut out: *mut c_void = core::ptr::null_mut();
            assert_eq!(
                vcall!(obj, 0, (*const Guid, *mut *mut c_void) -> HResult, &iid, &mut out),
                S_OK
            );
            assert_eq!(out, obj);
            vcall!(out, 2, () -> u32);
        }
        let bad = guid("00000000-1111-2222-3333-444444444444");
        let mut out: *mut c_void = core::ptr::null_mut();
        assert_eq!(
            vcall!(obj, 0, (*const Guid, *mut *mut c_void) -> HResult, &bad, &mut out),
            E_NOINTERFACE
        );
        let mut o2: *mut c_void = core::ptr::null_mut();
        assert_eq!(
            crate::gdk_classes::query(&CLSID, &bad, &mut o2),
            E_NOINTERFACE
        );
        assert_eq!(crate::gdk_classes::query(&CLSID, &IID_IMPL3, &mut o2), S_OK);
    }

    #[test]
    fn name_encoding_roundtrip_and_rules() {
        for n in ["Save/slot_1", "a b", "..", ".hidden", "é€", "x%y", "@tmp"] {
            let enc = encode_name(n);
            assert!(!enc.contains('/') && !enc.contains('\\') && !enc.starts_with('.'));
            assert!(!enc.starts_with('@'));
            assert_eq!(decode_name(&enc).as_deref(), Some(n));
        }
        assert!(container_name_valid("slot_1"));
        assert!(container_name_valid("dir/sub_/file.v-2"));
        assert!(!container_name_valid(""));
        assert!(!container_name_valid("dir/"));
        assert!(!container_name_valid("a..b"));
        assert!(!container_name_valid("end."));
        assert!(!container_name_valid("../x"));
        assert!(!container_name_valid("dir.x/file"));
        assert!(!container_name_valid("sp ace"));
        assert!(!container_name_valid(&"a".repeat(257)));
        // 256 caractères : accepté par la doc, refusé ici car le nom de fichier encodé dépasse NAME_MAX.
        assert!(!container_name_valid(&"a".repeat(256)));
        assert!(container_name_valid(&"a".repeat(255)));
        // le `/` encodé (%2F) compte triple dans le nom de fichier
        assert!(!container_name_valid(&format!("{}/x", "a/".repeat(100))));
    }

    #[test]
    fn provider_errors_and_quota() {
        ensure_vfs();
        let obj = query_class(&CLSID);
        let mut p = 7usize;
        let c = cs("cfg_err");
        // sortie NULL, utilisateur NULL, config NULL / vide.
        assert_eq!(
            vcall!(obj, 3, (usize, *const u8, u8, *mut usize) -> HResult, 1, c.as_ptr().cast(), 0, core::ptr::null_mut()),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 3, (usize, *const u8, u8, *mut usize) -> HResult, 0, c.as_ptr().cast(), 0, &mut p),
            E_INVALIDARG
        );
        assert_eq!(p, 0);
        assert_eq!(
            vcall!(obj, 3, (usize, *const u8, u8, *mut usize) -> HResult, 1, core::ptr::null(), 0, &mut p),
            E_INVALIDARG
        );
        let empty = cs("");
        assert_eq!(
            vcall!(obj, 3, (usize, *const u8, u8, *mut usize) -> HResult, 1, empty.as_ptr().cast(), 0, &mut p),
            E_GS_NO_SERVICE_CONFIGURATION
        );
        let prov = provider(obj, "cfg_err");
        let mut q = 0i64;
        assert_eq!(
            vcall!(obj, 7, (usize, *mut i64) -> HResult, prov, &mut q),
            S_OK
        );
        assert!(q >= 0);
        assert_eq!(
            vcall!(obj, 7, (usize, *mut i64) -> HResult, prov, core::ptr::null_mut()),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 7, (usize, *mut i64) -> HResult, 0xBAD, &mut q),
            E_INVALIDARG
        );
        vcall!(obj, 6, (usize) -> (), prov);
        assert_eq!(
            vcall!(obj, 7, (usize, *mut i64) -> HResult, prov, &mut q),
            E_INVALIDARG
        );
    }

    #[test]
    fn container_names_validated() {
        ensure_vfs();
        let obj = query_class(&CLSID);
        let prov = provider(obj, "cfg_names");
        for bad in ["", "a..b", "x/", "bad name", "../escape", "a/../b"] {
            let n = cs(bad);
            let mut h = 5usize;
            assert_eq!(
                vcall!(obj, 16, (usize, *const u8, *mut usize) -> HResult, prov, n.as_ptr().cast(), &mut h),
                E_GS_INVALID_CONTAINER_NAME,
                "{bad:?}"
            );
            assert_eq!(h, 0);
        }
        let n = cs("ok");
        assert_eq!(
            vcall!(obj, 16, (usize, *const u8, *mut usize) -> HResult, prov, n.as_ptr().cast(), core::ptr::null_mut()),
            E_INVALIDARG
        );
        let del = cs("bad name");
        assert_eq!(
            vcall!(obj, 10, (usize, *const u8) -> HResult, prov, del.as_ptr().cast()),
            E_GS_INVALID_CONTAINER_NAME
        );
    }

    #[test]
    fn write_read_enumerate_delete_roundtrip() {
        ensure_vfs();
        let obj = query_class(&CLSID);
        let prov = provider(obj, "cfg_roundtrip");
        let cont = container(obj, prov, "dir/slot_1");
        assert_eq!(
            write_blobs(
                obj,
                cont,
                &[("World", b"hello"), ("Player/1", b"xyz"), ("..", b"dots")]
            ),
            S_OK
        );

        // Aucun temporaire ne subsiste, aucun nom brut de blob dans le chemin hôte.
        let host = container_dir(&Container {
            cfg: encode_name("cfg_roundtrip"),
            name: "dir/slot_1".into(),
        })
        .expect("dir");
        let files: Vec<String> = std::fs::read_dir(&host)
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            files.iter().all(|f| !f.starts_with(TMP_PREFIX)),
            "{files:?}"
        );
        assert!(files.contains(&"Player%2F1".to_string()));
        assert!(files.contains(&"%2E.".to_string()));

        // Énumération des blobs.
        extern "win64" fn blob_cb(info: *const BlobInfo, ctx: *mut c_void) -> u8 {
            // SAFETY: appelé par le slot avec une info valide et le `Vec` du test comme contexte.
            unsafe {
                let v = &mut *(ctx as *mut Vec<(String, u32)>);
                let i = &*info;
                v.push((
                    std::ffi::CStr::from_ptr(i.name.cast())
                        .to_string_lossy()
                        .into_owned(),
                    i.size,
                ));
            }
            1
        }
        let mut seen: Vec<(String, u32)> = Vec::new();
        assert_eq!(
            vcall!(obj, 18, (usize, *mut c_void, Option<BlobCb>) -> HResult, cont, (&raw mut seen).cast(), Some(blob_cb as BlobCb)),
            S_OK
        );
        assert_eq!(
            seen,
            [
                ("..".into(), 4),
                ("Player/1".into(), 3),
                ("World".into(), 5)
            ]
        );
        seen.clear();
        let pre = cs("Pl");
        assert_eq!(
            vcall!(obj, 19, (usize, *const u8, *mut c_void, Option<BlobCb>) -> HResult, cont, pre.as_ptr().cast(), (&raw mut seen).cast(), Some(blob_cb as BlobCb)),
            S_OK
        );
        assert_eq!(seen.len(), 1);
        assert_eq!(
            vcall!(obj, 18, (usize, *mut c_void, Option<BlobCb>) -> HResult, cont, core::ptr::null_mut(), None),
            E_INVALIDARG
        );

        // Lecture nommée dans un tampon exact, puis trop petit.
        let (n1, n2) = (cs("World"), cs("Player/1"));
        let names = [n1.as_ptr().cast::<u8>(), n2.as_ptr().cast::<u8>()];
        let need = 2 * size_of::<Blob>() + 5 + 3;
        let mut buf = vec![0u8; need];
        let mut count = 2u32;
        assert_eq!(
            vcall_read(
                obj,
                cont,
                names.as_ptr(),
                &mut count,
                need - 1,
                buf.as_mut_ptr()
            ),
            E_GS_PROVIDED_BUFFER_TOO_SMALL
        );
        assert_eq!(
            vcall_read(
                obj,
                cont,
                names.as_ptr(),
                &mut count,
                need,
                buf.as_mut_ptr()
            ),
            S_OK
        );
        assert_eq!(count, 2);
        // SAFETY: le slot a rempli `buf` avec deux `XGameSaveBlob` puis les données.
        unsafe {
            let b0 = (buf.as_ptr() as *const Blob).read_unaligned();
            let b1 = (buf.as_ptr().add(size_of::<Blob>()) as *const Blob).read_unaligned();
            assert_eq!(b0.info.size, 5);
            assert_eq!(core::slice::from_raw_parts(b0.data, 5), b"hello");
            assert_eq!(
                std::ffi::CStr::from_ptr(b1.info.name.cast()).to_bytes(),
                b"Player/1"
            );
            assert_eq!(core::slice::from_raw_parts(b1.data, 3), b"xyz");
        }
        // Blob absent / nom NULL / compteur NULL.
        let missing = cs("Nope");
        let mn = [missing.as_ptr().cast::<u8>()];
        let mut one = 1u32;
        assert_eq!(
            vcall_read(obj, cont, mn.as_ptr(), &mut one, 4096, buf.as_mut_ptr()),
            E_GS_BLOB_NOT_FOUND
        );
        assert_eq!(
            vcall_read(
                obj,
                cont,
                names.as_ptr(),
                core::ptr::null_mut(),
                need,
                buf.as_mut_ptr()
            ),
            E_INVALIDARG
        );
        // Lecture de tous les blobs (blobNames == NULL).
        let mut all = 0u32;
        let mut big = vec![0u8; 4096];
        assert_eq!(
            vcall_read(
                obj,
                cont,
                core::ptr::null(),
                &mut all,
                4096,
                big.as_mut_ptr()
            ),
            S_OK
        );
        assert_eq!(all, 3);

        // Informations de conteneur.
        extern "win64" fn cont_cb(info: *const ContainerInfo, ctx: *mut c_void) -> u8 {
            // SAFETY: info valide, contexte = `Vec` du test.
            unsafe {
                let v = &mut *(ctx as *mut Vec<(String, String, u32, u64)>);
                let i = &*info;
                v.push((
                    std::ffi::CStr::from_ptr(i.name.cast())
                        .to_string_lossy()
                        .into_owned(),
                    std::ffi::CStr::from_ptr(i.display_name.cast())
                        .to_string_lossy()
                        .into_owned(),
                    i.blob_count,
                    i.total_size,
                ));
            }
            1
        }
        let mut infos: Vec<(String, String, u32, u64)> = Vec::new();
        let cn = cs("dir/slot_1");
        assert_eq!(
            vcall!(obj, 13, (usize, *const u8, *mut c_void, Option<ContainerCb>) -> HResult, prov, cn.as_ptr().cast(), (&raw mut infos).cast(), Some(cont_cb as ContainerCb)),
            S_OK
        );
        assert_eq!(infos, [("dir/slot_1".into(), "Display".into(), 3, 12)]);
        infos.clear();
        assert_eq!(
            vcall!(obj, 14, (usize, *mut c_void, Option<ContainerCb>) -> HResult, prov, (&raw mut infos).cast(), Some(cont_cb as ContainerCb)),
            S_OK
        );
        assert_eq!(infos.len(), 1);
        infos.clear();
        let nopre = cs("zzz");
        assert_eq!(
            vcall!(obj, 15, (usize, *const u8, *mut c_void, Option<ContainerCb>) -> HResult, prov, nopre.as_ptr().cast(), (&raw mut infos).cast(), Some(cont_cb as ContainerCb)),
            S_OK
        );
        assert!(infos.is_empty());
        assert_eq!(
            vcall!(obj, 14, (usize, *mut c_void, Option<ContainerCb>) -> HResult, prov, core::ptr::null_mut(), None),
            E_INVALIDARG
        );

        // Suppression de blob par mise à jour, puis du conteneur.
        let d = cs("Display2");
        let mut u = 0usize;
        assert_eq!(
            vcall!(obj, 23, (usize, *const u8, *mut usize) -> HResult, cont, d.as_ptr().cast(), &mut u),
            S_OK
        );
        let w = cs("World");
        assert_eq!(
            vcall!(obj, 26, (usize, *const u8) -> HResult, u, w.as_ptr().cast()),
            S_OK
        );
        assert_eq!(vcall!(obj, 27, (usize) -> HResult, u), S_OK);
        vcall!(obj, 24, (usize) -> (), u);
        let mut one = 1u32;
        assert_eq!(
            vcall_read(
                obj,
                cont,
                [w.as_ptr().cast::<u8>()].as_ptr(),
                &mut one,
                4096,
                big.as_mut_ptr()
            ),
            E_GS_BLOB_NOT_FOUND
        );
        assert_eq!(
            vcall!(obj, 10, (usize, *const u8) -> HResult, prov, cn.as_ptr().cast()),
            S_OK
        );
        assert!(!host.exists());
        // Suppression d'un conteneur absent : S_OK.
        assert_eq!(
            vcall!(obj, 10, (usize, *const u8) -> HResult, prov, cn.as_ptr().cast()),
            S_OK
        );
        vcall!(obj, 17, (usize) -> (), cont);
    }

    /// `XGameSaveReadBlobData(container, names, &count, size, out)` (slot 20).
    fn vcall_read(
        obj: *mut c_void,
        cont: usize,
        names: *const *const u8,
        count: *mut u32,
        size: usize,
        out: *mut u8,
    ) -> HResult {
        vcall!(obj, 20, (usize, *const *const u8, *mut u32, usize, *mut u8) -> HResult, cont, names, count, size, out)
    }

    #[test]
    fn update_rules() {
        ensure_vfs();
        let obj = query_class(&CLSID);
        let prov = provider(obj, "cfg_rules");
        let cont = container(obj, prov, "rules");
        let d = cs("D");
        let mut u = 0usize;
        assert_eq!(
            vcall!(obj, 23, (usize, *const u8, *mut usize) -> HResult, cont, core::ptr::null(), &mut u),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 23, (usize, *const u8, *mut usize) -> HResult, 0xBAD, d.as_ptr().cast(), &mut u),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 23, (usize, *const u8, *mut usize) -> HResult, cont, d.as_ptr().cast(), core::ptr::null_mut()),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 23, (usize, *const u8, *mut usize) -> HResult, cont, d.as_ptr().cast(), &mut u),
            S_OK
        );
        let a = cs("A");
        let data = [1u8, 2, 3];
        let w = |name: *const u8, data: *const u8, len: usize| vcall!(obj, 25, (usize, *const u8, *const u8, usize) -> HResult, u, name, data, len);
        assert_eq!(w(a.as_ptr().cast(), data.as_ptr(), 3), S_OK);
        // un seul enregistrement par blob (écriture ou suppression)
        assert_eq!(w(a.as_ptr().cast(), data.as_ptr(), 3), E_INVALIDARG);
        assert_eq!(
            vcall!(obj, 26, (usize, *const u8) -> HResult, u, a.as_ptr().cast()),
            E_INVALIDARG
        );
        // nom vide / NULL / données NULL
        let empty = cs("");
        assert_eq!(w(empty.as_ptr().cast(), data.as_ptr(), 3), E_INVALIDARG);
        assert_eq!(w(core::ptr::null(), data.as_ptr(), 3), E_INVALIDARG);
        let b = cs("B");
        assert_eq!(w(b.as_ptr().cast(), core::ptr::null(), 3), E_INVALIDARG);
        // nom trop long
        let long = cs(&"n".repeat(257));
        assert_eq!(w(long.as_ptr().cast(), data.as_ptr(), 3), E_INVALIDARG);
        // blob trop gros, puis cumul de la mise à jour trop gros
        assert_eq!(
            w(b.as_ptr().cast(), data.as_ptr(), GS_MAX_BLOB_SIZE + 1),
            E_GS_UPDATE_TOO_BIG
        );
        let half = vec![0u8; GS_MAX_BLOB_SIZE / 2 + 1];
        let c = cs("C");
        let d2 = cs("D2");
        assert_eq!(w(c.as_ptr().cast(), half.as_ptr(), half.len()), S_OK);
        assert_eq!(
            w(d2.as_ptr().cast(), half.as_ptr(), half.len()),
            E_GS_UPDATE_TOO_BIG
        );
        // handle de mise à jour inconnu
        assert_eq!(vcall!(obj, 27, (usize) -> HResult, 0xBAD), E_INVALIDARG);
        vcall!(obj, 24, (usize) -> (), u);
        assert_eq!(vcall!(obj, 27, (usize) -> HResult, u), E_INVALIDARG);
    }

    #[test]
    fn async_variants_complete_through_xasync_block() {
        ensure_vfs();
        let obj = query_class(&CLSID);

        static FIRED: AtomicU64 = AtomicU64::new(0);
        unsafe extern "win64" fn on_done(_b: *mut XAsyncBlock) {
            FIRED.fetch_add(1, Ordering::SeqCst);
        }
        let new_block = || XAsyncBlock {
            queue: 0,
            context: core::ptr::null_mut(),
            callback: Some(on_done),
            internal: [0; 32],
        };

        // InitializeProviderAsync / Result
        let cfg = cs("cfg_async");
        let mut blk = new_block();
        let before = FIRED.load(Ordering::SeqCst);
        assert_eq!(
            vcall!(obj, 4, (usize, *const u8, u8, *mut XAsyncBlock) -> HResult, 1, cfg.as_ptr().cast(), 0, &mut blk),
            S_OK
        );
        assert_eq!(FIRED.load(Ordering::SeqCst), before + 1);
        assert_eq!(blk.internal[0], 1);
        let mut prov = 0usize;
        assert_eq!(
            vcall!(obj, 5, (*mut XAsyncBlock, *mut usize) -> HResult, &mut blk, &mut prov),
            S_OK
        );
        assert_ne!(prov, 0);
        // résultat déjà consommé
        let mut again = 99usize;
        assert_eq!(
            vcall!(obj, 5, (*mut XAsyncBlock, *mut usize) -> HResult, &mut blk, &mut again),
            E_INVALIDARG
        );
        assert_eq!(again, 0);
        assert_eq!(
            vcall!(obj, 4, (usize, *const u8, u8, *mut XAsyncBlock) -> HResult, 1, cfg.as_ptr().cast(), 0, core::ptr::null_mut()),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 4, (usize, *const u8, u8, *mut XAsyncBlock) -> HResult, 0, cfg.as_ptr().cast(), 0, &mut blk),
            E_INVALIDARG
        );

        // Quota async
        let mut blk = new_block();
        assert_eq!(
            vcall!(obj, 8, (usize, *mut XAsyncBlock) -> HResult, prov, &mut blk),
            S_OK
        );
        let mut q = -1i64;
        assert_eq!(
            vcall!(obj, 9, (*mut XAsyncBlock, *mut i64) -> HResult, &mut blk, &mut q),
            S_OK
        );
        assert!(q >= 0);

        // Écriture asynchrone puis lecture asynchrone avec ResultSize.
        let cont = container(obj, prov, "asv");
        let d = cs("D");
        let mut u = 0usize;
        assert_eq!(
            vcall!(obj, 23, (usize, *const u8, *mut usize) -> HResult, cont, d.as_ptr().cast(), &mut u),
            S_OK
        );
        let bn = cs("Blob");
        let payload = [9u8; 10];
        assert_eq!(
            vcall!(obj, 25, (usize, *const u8, *const u8, usize) -> HResult, u, bn.as_ptr().cast(), payload.as_ptr(), 10),
            S_OK
        );
        let mut blk = new_block();
        assert_eq!(
            vcall!(obj, 28, (usize, *mut XAsyncBlock) -> HResult, u, &mut blk),
            S_OK
        );
        assert_eq!(
            vcall!(obj, 29, (*mut XAsyncBlock) -> HResult, &mut blk),
            S_OK
        );
        vcall!(obj, 24, (usize) -> (), u);

        let mut blk = new_block();
        let names = [bn.as_ptr().cast::<u8>()];
        assert_eq!(
            vcall!(obj, 21, (usize, *const *const u8, u32, *mut XAsyncBlock) -> HResult, cont, names.as_ptr(), 1, &mut blk),
            S_OK
        );
        let size = u64::from_le_bytes(blk.internal[8..16].try_into().expect("8 octets")) as usize;
        assert_eq!(size, size_of::<Blob>() + 10);
        let mut buf = vec![0u8; size];
        let mut count = 0u32;
        assert_eq!(
            vcall!(obj, 22, (*mut XAsyncBlock, usize, *mut u8, *mut u32) -> HResult, &mut blk, size - 1, buf.as_mut_ptr(), &mut count),
            E_GS_PROVIDED_BUFFER_TOO_SMALL
        );
        assert_eq!(
            vcall!(obj, 22, (*mut XAsyncBlock, usize, *mut u8, *mut u32) -> HResult, &mut blk, size, buf.as_mut_ptr(), &mut count),
            S_OK
        );
        assert_eq!(count, 1);
        assert_eq!(&buf[size_of::<Blob>()..], &payload);

        // Lecture d'un blob absent : échec mémorisé, restitué par Result.
        let miss = cs("Missing");
        let mn = [miss.as_ptr().cast::<u8>()];
        let mut blk = new_block();
        assert_eq!(
            vcall!(obj, 21, (usize, *const *const u8, u32, *mut XAsyncBlock) -> HResult, cont, mn.as_ptr(), 1, &mut blk),
            S_OK
        );
        assert_eq!(
            vcall!(obj, 22, (*mut XAsyncBlock, usize, *mut u8, *mut u32) -> HResult, &mut blk, 4096, buf.as_mut_ptr(), &mut count),
            E_GS_BLOB_NOT_FOUND
        );

        // Suppression asynchrone d'un nom invalide : échec d'exécution mémorisé.
        let bad = cs("bad name");
        let mut blk = new_block();
        assert_eq!(
            vcall!(obj, 11, (usize, *const u8, *mut XAsyncBlock) -> HResult, prov, bad.as_ptr().cast(), &mut blk),
            S_OK
        );
        assert_eq!(
            vcall!(obj, 12, (*mut XAsyncBlock) -> HResult, &mut blk),
            E_GS_INVALID_CONTAINER_NAME
        );
        assert_eq!(
            vcall!(obj, 11, (usize, *const u8, *mut XAsyncBlock) -> HResult, prov, bad.as_ptr().cast(), core::ptr::null_mut()),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 29, (*mut XAsyncBlock) -> HResult, core::ptr::null_mut()),
            E_INVALIDARG
        );
    }

    #[test]
    fn files_slots() {
        ensure_vfs();
        let obj = query_class(&CLSID);
        let cfg = cs("cfg_files");
        let mut blk = XAsyncBlock {
            queue: 0,
            context: core::ptr::null_mut(),
            callback: None,
            internal: [0; 32],
        };
        assert_eq!(
            vcall!(obj, 30, (usize, *const u8, *mut XAsyncBlock) -> HResult, 0, cfg.as_ptr().cast(), &mut blk),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 30, (usize, *const u8, *mut XAsyncBlock) -> HResult, 1, cfg.as_ptr().cast(), &mut blk),
            S_OK
        );
        let size = u64::from_le_bytes(blk.internal[8..16].try_into().expect("8 octets")) as usize;
        let mut buf = vec![0u8; size];
        assert_eq!(
            vcall!(obj, 31, (*mut XAsyncBlock, usize, *mut u8) -> HResult, &mut blk, size - 1, buf.as_mut_ptr()),
            E_GS_PROVIDED_BUFFER_TOO_SMALL
        );
        assert_eq!(
            vcall!(obj, 31, (*mut XAsyncBlock, usize, *mut u8) -> HResult, &mut blk, size, buf.as_mut_ptr()),
            S_OK
        );
        let path = std::str::from_utf8(&buf[..size - 1]).expect("utf8");
        assert!(path.starts_with("S:\\"));
        assert!(!path.contains(".."));
        assert!(crate::file_io::resolve_vfs_path(path).is_some_and(|p| p.is_dir()));

        let mut q = 0i64;
        assert_eq!(
            vcall!(obj, 32, (usize, *const u8, *mut i64) -> HResult, 1, cfg.as_ptr().cast(), &mut q),
            S_OK
        );
        assert!(q >= 0);
        assert_eq!(
            vcall!(obj, 32, (usize, *const u8, *mut i64) -> HResult, 0, cfg.as_ptr().cast(), &mut q),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 32, (usize, *const u8, *mut i64) -> HResult, 1, cfg.as_ptr().cast(), core::ptr::null_mut()),
            E_INVALIDARG
        );
    }
}
