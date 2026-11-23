//! Classes du runtime GDK : `XGameSaveImpl`, `XStoreImpl`, `XPackageImpl` (voir `gdk_classes.rs`).
//!
//! Le module regroupe les briques partagées par les trois classes (GUID, lecture de chaînes
//! invitées, enregistrement des résultats asynchrones, handles opaques) ; chaque classe vit dans
//! son sous-fichier (`gamesave.rs`, `store.rs`, `package.rs`).
//!
//! # Convention asynchrone
//!
//! Comme `XUserAddAsync` (`gdk_xs.rs`), un appel `…Async` s'exécute immédiatement : le résultat est
//! conservé dans une table indexée par l'adresse du `XAsyncBlock`, `internal[0]` vaut 1 (terminé),
//! le statut est recopié en `internal[4..8]` et la taille du résultat en `internal[8..16]`, puis le
//! rappel (`callback`) du bloc est invoqué. L'appel `…Result` correspondant consomme l'entrée.
//! Une erreur d'arguments est renvoyée de façon synchrone par l'appel `…Async` (aucun bloc ne
//! reste en attente) ; une erreur d'exécution est mémorisée et rendue par `…Result`.

#[cfg(test)]
macro_rules! vcall {
    ($obj:expr, $n:expr, ($($t:ty),*) -> $r:ty $(, $a:expr)*) => {{
        let obj: *mut core::ffi::c_void = $obj;
        // SAFETY: `obj` vient de `QueryApiImpl` ; le slot `$n` a la signature décrite par le test.
        let f: extern "win64" fn(*mut core::ffi::c_void $(, $t)*) -> $r = unsafe {
            core::mem::transmute(
                $crate::gdk_classes::rt_save_store::tests_support::slot_ptr(obj, $n),
            )
        };
        f(obj $(, $a)*)
    }};
}

mod gamesave;
mod package;
mod store;

/// Aides de test partagées (VFS temporaire, requête de classe, appel de slot par la vtable).
#[cfg(test)]
pub(crate) mod tests_support {
    use core::ffi::c_void;

    use crate::com::{ComHeader, Guid};

    /// Installe un VFS temporaire si aucun n'est configuré dans le processus de test.
    pub(crate) fn ensure_vfs() {
        let mut st = crate::process::process_state().write();
        if st.vfs.is_none() {
            let root =
                std::env::temp_dir().join(format!("sb-h3-save-store-vfs-{}", std::process::id()));
            let game = root.join("game");
            std::fs::create_dir_all(&game).expect("racine du jeu");
            st.vfs = Some(
                sb_vfs::VirtualFs::new(&game, &root.join("save"), &root.join("temp"), None)
                    .expect("vfs"),
            );
        }
    }

    /// `QueryApiImpl(clsid, clsid, &obj)` ; l'objet est un singleton jamais libéré.
    pub(crate) fn query_class(clsid: &Guid) -> *mut c_void {
        let mut obj: *mut c_void = core::ptr::null_mut();
        assert_eq!(crate::gdk_classes::query(clsid, clsid, &mut obj), 0);
        assert!(!obj.is_null());
        obj
    }

    pub(crate) fn refcount(obj: *mut c_void) -> u32 {
        // SAFETY: `obj` est un objet de classe GDK (commence par `ComHeader`).
        unsafe {
            (*(obj as *mut ComHeader))
                .ref_count
                .load(core::sync::atomic::Ordering::Acquire)
        }
    }

    /// Pointeur de fonction du slot `n` de la vtable de `obj`.
    pub(crate) fn slot_ptr(obj: *mut c_void, n: usize) -> *const c_void {
        // SAFETY: le premier mot de l'objet est le pointeur de vtable ; le test reste dans ses bornes.
        unsafe {
            let vtbl = *(obj as *const *const *const c_void);
            *vtbl.add(n)
        }
    }
}

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use sb_common::abi::{E_INVALIDARG, HResult, S_OK};
use sb_common::sb_warn;

use super::ClassDef;
use crate::com::{ComHeader, Guid};
use crate::gdk_xs::XAsyncBlock;

pub(crate) const CHANNEL: &str = "gdk_save_store";

/// Classes fournies par ce module (enregistrées dans `gdk_classes::registry`).
pub fn classes() -> Vec<ClassDef> {
    vec![gamesave::class(), store::class(), package::class()]
}

// ---------------------------------------------------------------------------------------------
// GUID
// ---------------------------------------------------------------------------------------------

const fn hex_digit(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => panic!("chiffre hexadécimal invalide"),
    }
}

const fn hex_n(s: &[u8], start: usize, digits: usize) -> u64 {
    let mut v = 0u64;
    let mut i = 0;
    while i < digits {
        v = (v << 4) | hex_digit(s[start + i]) as u64;
        i += 1;
    }
    v
}

/// Construit un GUID à la compilation depuis `"xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"`.
pub(crate) const fn guid(s: &str) -> Guid {
    let b = s.as_bytes();
    assert!(b.len() == 36, "GUID : 36 caractères attendus");
    let d4_hi = hex_n(b, 19, 4);
    let d4_lo = hex_n(b, 24, 12);
    let d4 = (d4_hi << 48) | d4_lo;
    Guid::new(
        hex_n(b, 0, 8) as u32,
        hex_n(b, 9, 4) as u16,
        hex_n(b, 14, 4) as u16,
        d4.to_be_bytes(),
    )
}

// ---------------------------------------------------------------------------------------------
// Objets COM singleton
// ---------------------------------------------------------------------------------------------

/// Retourne (en le créant au besoin) l'objet singleton d'une classe : un `ComHeader` seul, car
/// l'état des classes vit dans des tables globales indexées par handle.
pub(crate) fn singleton(cell: &'static OnceLock<usize>, vtbl: *const c_void) -> *mut ComHeader {
    *cell.get_or_init(|| Box::into_raw(Box::new(ComHeader::new(vtbl))) as usize) as *mut ComHeader
}

// ---------------------------------------------------------------------------------------------
// Chaînes invitées
// ---------------------------------------------------------------------------------------------

/// Longueur maximale (octets) acceptée pour une chaîne C invitée.
const MAX_GUEST_STRING: usize = 64 * 1024;

/// Lit une chaîne C invitée UTF-8 (`None` si NULL, non terminée dans la limite, ou non UTF-8).
pub(crate) fn read_cstr(p: *const u8) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut bytes = Vec::new();
    for i in 0..MAX_GUEST_STRING {
        // SAFETY: l'invité fournit une chaîne terminée par NUL (`_In_z_`) ; lecture octet par octet
        // bornée par `MAX_GUEST_STRING`.
        let b = unsafe { *p.add(i) };
        if b == 0 {
            return String::from_utf8(bytes).ok();
        }
        bytes.push(b);
    }
    None
}

/// Écrit `value` dans `*out` si `out` est non nul.
pub(crate) fn write_out<T>(out: *mut T, value: T) {
    if !out.is_null() {
        // SAFETY: `out` non nul, fourni par l'invité comme pointeur de sortie (`_Out_`).
        unsafe { out.write_unaligned(value) };
    }
}

/// Copie `text` suivi d'un NUL dans `buf` (taille `size` octets, déjà validée par l'appelant).
pub(crate) fn write_cstr_buf(buf: *mut u8, text: &str) {
    // SAFETY: l'appelant a vérifié `buf` non nul et `size >= text.len() + 1`.
    unsafe {
        core::ptr::copy_nonoverlapping(text.as_ptr(), buf, text.len());
        *buf.add(text.len()) = 0;
    }
}

// ---------------------------------------------------------------------------------------------
// Handles opaques
// ---------------------------------------------------------------------------------------------

/// Table de handles opaques (valeurs non nulles, jamais déréférencées).
pub(crate) struct HandleTable<T> {
    next: AtomicUsize,
    map: Mutex<HashMap<usize, T>>,
}

impl<T> HandleTable<T> {
    pub(crate) fn new(base: usize) -> Self {
        Self {
            next: AtomicUsize::new(base),
            map: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn insert(&self, value: T) -> usize {
        let h = self.next.fetch_add(0x10, Ordering::Relaxed);
        self.map.lock().insert(h, value);
        h
    }

    pub(crate) fn remove(&self, h: usize) -> Option<T> {
        self.map.lock().remove(&h)
    }

    pub(crate) fn contains(&self, h: usize) -> bool {
        self.map.lock().contains_key(&h)
    }

    pub(crate) fn with<R>(&self, h: usize, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        self.map.lock().get_mut(&h).map(f)
    }
}

// ---------------------------------------------------------------------------------------------
// Résultats asynchrones
// ---------------------------------------------------------------------------------------------

/// Charge utile d'un résultat asynchrone.
pub(crate) enum AsyncPayload {
    None,
    Handle(usize),
    Quota(i64),
    Text(String),
    Blobs(Vec<(String, Vec<u8>)>),
}

pub(crate) struct AsyncRecord {
    pub(crate) hr: HResult,
    pub(crate) size: usize,
    pub(crate) payload: AsyncPayload,
}

const _: () = assert!(size_of::<XAsyncBlock>() == 56);

fn async_results() -> &'static Mutex<HashMap<usize, AsyncRecord>> {
    static T: OnceLock<Mutex<HashMap<usize, AsyncRecord>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Valide le bloc asynchrone d'un appel `…Async` (NULL → `E_INVALIDARG`).
pub(crate) fn async_check(block: *mut XAsyncBlock) -> Result<(), HResult> {
    if block.is_null() {
        Err(E_INVALIDARG)
    } else {
        Ok(())
    }
}

/// Termine l'opération asynchrone : mémorise le résultat, marque le bloc terminé et appelle son
/// rappel. Renvoie `S_OK` (l'erreur d'exécution est rendue par l'appel `…Result`).
pub(crate) fn async_finish(
    block: *mut XAsyncBlock,
    hr: HResult,
    size: usize,
    payload: AsyncPayload,
) -> HResult {
    async_results()
        .lock()
        .insert(block as usize, AsyncRecord { hr, size, payload });
    // SAFETY: `block` non nul (validé par `async_check`) et vivant jusqu'à la fin de l'opération.
    let callback = unsafe {
        let ab = &mut *block;
        ab.internal[0] = 1;
        ab.internal[4..8].copy_from_slice(&hr.to_le_bytes());
        ab.internal[8..16].copy_from_slice(&(size as u64).to_le_bytes());
        ab.callback
    };
    if let Some(cb) = callback {
        // SAFETY: rappel `void CALLBACK(XAsyncBlock*)` fourni par le titre.
        unsafe { cb(block) };
    }
    S_OK
}

/// Complète un appel asynchrone à partir du résultat `Result<(size, payload), HResult>` calculé.
pub(crate) fn async_complete(
    block: *mut XAsyncBlock,
    outcome: Result<(usize, AsyncPayload), HResult>,
) -> HResult {
    match outcome {
        Ok((size, payload)) => async_finish(block, S_OK, size, payload),
        Err(hr) => async_finish(block, hr, 0, AsyncPayload::None),
    }
}

/// Taille de résultat mémorisée (sans consommer l'entrée), pour les `…ResultSize`/`…ResultCount`.
pub(crate) fn async_peek_size(block: *mut XAsyncBlock) -> Result<usize, HResult> {
    if block.is_null() {
        return Err(E_INVALIDARG);
    }
    match async_results().lock().get(&(block as usize)) {
        Some(r) if r.hr < 0 => Err(r.hr),
        Some(r) => Ok(r.size),
        None => Err(E_INVALIDARG),
    }
}

/// Consomme le résultat d'un bloc. `accept` dit si la charge utile convient à l'appelant ;
/// sinon l'entrée est conservée et `E_INVALIDARG` renvoyé. Un statut d'échec est restitué tel quel.
pub(crate) fn async_take(
    block: *mut XAsyncBlock,
    accept: impl Fn(&AsyncPayload) -> bool,
) -> Result<AsyncRecord, HResult> {
    if block.is_null() {
        return Err(E_INVALIDARG);
    }
    let mut table = async_results().lock();
    let key = block as usize;
    match table.get(&key) {
        None => Err(E_INVALIDARG),
        Some(r) if r.hr < 0 => {
            let hr = r.hr;
            table.remove(&key);
            Err(hr)
        }
        Some(r) if !accept(&r.payload) => Err(E_INVALIDARG),
        Some(_) => table.remove(&key).ok_or(E_INVALIDARG),
    }
}

/// Journalise une seule fois par (classe, slot) un échec documenté ou un slot sans signature.
pub(crate) fn warn_once(class: &'static str, slot: usize, what: &str) {
    static SEEN: OnceLock<Mutex<std::collections::HashSet<(usize, usize)>>> = OnceLock::new();
    let fresh = SEEN
        .get_or_init(|| Mutex::new(Default::default()))
        .lock()
        .insert((class.as_ptr() as usize, slot));
    if fresh {
        sb_warn!(CHANNEL, "{class}::slot {slot} ({what}) -> échec hors ligne");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_parser_matches_literal() {
        let g = guid("704c3f58-e629-4cc2-b197-30511b996fe2");
        assert_eq!(g.data1, 0x704c_3f58);
        assert_eq!(g.data2, 0xe629);
        assert_eq!(g.data3, 0x4cc2);
        assert_eq!(g.data4, [0xb1, 0x97, 0x30, 0x51, 0x1b, 0x99, 0x6f, 0xe2]);
    }

    #[test]
    fn three_classes_are_registered() {
        let names: Vec<_> = classes().iter().map(|c| c.name).collect();
        assert_eq!(names, ["XGameSaveImpl", "XStoreImpl", "XPackageImpl"]);
    }

    #[test]
    fn async_table_roundtrip() {
        let mut block = XAsyncBlock {
            queue: 0,
            context: core::ptr::null_mut(),
            callback: None,
            internal: [0; 32],
        };
        let p = &raw mut block;
        assert_eq!(async_check(core::ptr::null_mut()), Err(E_INVALIDARG));
        assert_eq!(async_finish(p, S_OK, 7, AsyncPayload::Quota(5)), S_OK);
        assert_eq!(block.internal[0], 1);
        assert_eq!(async_peek_size(p), Ok(7));
        assert!(async_take(p, |pl| matches!(pl, AsyncPayload::Handle(_))).is_err());
        let rec = async_take(p, |pl| matches!(pl, AsyncPayload::Quota(_))).expect("record");
        assert_eq!(rec.size, 7);
        assert_eq!(async_take(p, |_| true).err(), Some(E_INVALIDARG));
        async_finish(p, -5, 0, AsyncPayload::None);
        assert_eq!(async_take(p, |_| true).err(), Some(-5));
    }
}
