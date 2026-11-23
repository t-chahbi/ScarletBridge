//! Émetteur dynamique de trampolines JIT x86-64 (ABI `win64`) pour les imports non implémentés.
//!
//! Chaque symbole manquant reçoit un stub exécutable de 36 octets respectant strictement la
//! convention d'appel Windows x86-64 (alignement 16 octets, shadow space 32 octets, préservation
//! des registres callee-saved) et redirigeant vers un dispatcher Rust sûr qui loggue `dll!symbole`.
//!
//! Politique pilotée par `SB_UNIMPLEMENTED` :
//! - `log` (défaut) : trace l'appel et retourne la valeur par défaut du symbole ;
//! - `stop` : trace `dll!symbole`, l'appelant (`module+RVA`) et rcx/rdx/r8/r9, puis termine le
//!   processus (code 3) — utile pour trouver le premier import manquant réellement atteint.

use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::{Mutex, RwLock};
use sb_common::abi::E_NOTIMPL;
use sb_common::{ffi_guard, sb_err, sb_info, sb_stub};

const STUB_CODE_SIZE: usize = 46;
const STUB_STRIDE: usize = 64; // Aligné sur ligne de cache 64 octets.
const POOL_SIZE: usize = 256 * 1024; // 4096 trampolines max par pool.

#[derive(Debug, Clone)]
pub struct StubEntry {
    pub dll: String,
    pub symbol: String,
    pub return_value: usize,
}

static STUB_ENTRIES: OnceLock<RwLock<Vec<StubEntry>>> = OnceLock::new();

fn stub_entries() -> &'static RwLock<Vec<StubEntry>> {
    STUB_ENTRIES.get_or_init(|| RwLock::new(Vec::new()))
}

struct TrampolinePool {
    base: *mut u8,
    len: usize,
    offset: usize,
}

// SAFETY: le pool est protégé par un `Mutex` et sa mémoire `mmap` vit jusqu'à la fin du processus.
unsafe impl Send for TrampolinePool {}

static POOL: OnceLock<Mutex<TrampolinePool>> = OnceLock::new();

fn pool() -> &'static Mutex<TrampolinePool> {
    POOL.get_or_init(|| {
        // SAFETY: allocation anonyme `mmap` de `POOL_SIZE` octets.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                POOL_SIZE,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(
            ptr,
            libc::MAP_FAILED,
            "échec d'allocation mmap du pool de trampolines JIT"
        );
        Mutex::new(TrampolinePool {
            base: ptr.cast::<u8>(),
            len: POOL_SIZE,
            offset: 0,
        })
    })
}

/// Comportement d'un import non implémenté (`SB_UNIMPLEMENTED`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnimplementedPolicy {
    /// Trace et retourne la valeur par défaut.
    Log,
    /// Trace l'appel complet puis termine le processus.
    Stop,
}

impl UnimplementedPolicy {
    /// Interprète la valeur de `SB_UNIMPLEMENTED` (insensible à la casse, défaut `log`).
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("stop") | Some("abort") => Self::Stop,
            _ => Self::Log,
        }
    }
}

fn policy() -> UnimplementedPolicy {
    static POLICY: OnceLock<UnimplementedPolicy> = OnceLock::new();
    *POLICY.get_or_init(|| {
        UnimplementedPolicy::parse(std::env::var("SB_UNIMPLEMENTED").ok().as_deref())
    })
}

/// Dispatcher Rust appelé par tous les trampolines JIT x86-64.
/// `caller` est l'adresse de retour dans le code invité qui a appelé l'import.
extern "win64" fn missing_import_dispatch(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    stub_id: u64,
    caller: usize,
) -> usize {
    ffi_guard!("jit", "missing_import_dispatch", E_NOTIMPL as usize, {
        let entry = stub_entries().read().get(stub_id as usize).cloned();
        match entry {
            Some(meta) => {
                let full_name = format!("{}!{}", meta.dll, meta.symbol);
                if policy() == UnimplementedPolicy::Stop {
                    let site = crate::module_manager()
                        .describe_address(caller)
                        .unwrap_or_else(|| format!("{caller:#x}"));
                    sb_err!(
                        "jit",
                        "import non implémenté atteint (SB_UNIMPLEMENTED=stop) : {full_name} appelé depuis {site} (rcx={a1:#x}, rdx={a2:#x}, r8={a3:#x}, r9={a4:#x})"
                    );
                    std::process::exit(3);
                }
                sb_stub!(
                    "jit",
                    &full_name,
                    "a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x} -> {:#x}",
                    meta.return_value
                );
                meta.return_value
            }
            None => {
                sb_stub!(
                    "jit",
                    "unknown_stub",
                    "id={stub_id}, a1={a1:#x}, a2={a2:#x}, a3={a3:#x}, a4={a4:#x}"
                );
                0
            }
        }
    })
}

/// Alloue un trampoline JIT x86-64 pour `dll!symbol` et retourne son adresse d'entrée appelable en `win64`.
#[cfg(target_arch = "x86_64")]
pub fn allocate_stub_trampoline(dll: &str, symbol: &str, return_value: usize) -> usize {
    let stub_id = {
        let mut entries = stub_entries().write();
        let id = entries.len() as u64;
        entries.push(StubEntry {
            dll: dll.to_string(),
            symbol: symbol.to_string(),
            return_value,
        });
        id
    };

    let mut guard = pool().lock();
    assert!(
        guard.offset + STUB_STRIDE <= guard.len,
        "pool de trampolines JIT saturé (> {} stubs)",
        guard.len / STUB_STRIDE
    );

    let slot_ptr = unsafe { guard.base.add(guard.offset) };
    guard.offset += STUB_STRIDE;

    let dispatch_addr = missing_import_dispatch as *const () as usize as u64;
    let mut code = [0x90u8; STUB_STRIDE]; // NOP padding entre les stubs.

    // sub rsp, 0x38 (shadow 32o + 5e arg 8o + alignement 16o ; entrée rsp%16==8 -> call rsp%16==0)
    code[0..4].copy_from_slice(&[0x48, 0x83, 0xEC, 0x38]);
    // movabs rax, stub_id
    code[4..6].copy_from_slice(&[0x48, 0xB8]);
    code[6..14].copy_from_slice(&stub_id.to_le_bytes());
    // mov [rsp + 0x20], rax (5e argument Win64)
    code[14..19].copy_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x20]);
    // mov rax, [rsp + 0x38] (adresse de retour de l'appelant invité)
    code[19..24].copy_from_slice(&[0x48, 0x8B, 0x44, 0x24, 0x38]);
    // mov [rsp + 0x28], rax (6e argument Win64)
    code[24..29].copy_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x28]);
    // movabs rax, dispatch_addr
    code[29..31].copy_from_slice(&[0x48, 0xB8]);
    code[31..39].copy_from_slice(&dispatch_addr.to_le_bytes());
    // call rax
    code[39..41].copy_from_slice(&[0xFF, 0xD0]);
    // add rsp, 0x38
    code[41..45].copy_from_slice(&[0x48, 0x83, 0xC4, 0x38]);
    // ret
    code[45] = 0xC3;

    const _: () = assert!(STUB_CODE_SIZE <= STUB_STRIDE);

    // SAFETY: `slot_ptr` pointe vers `STUB_STRIDE` octets valides et mappés RWX dans `POOL`.
    unsafe {
        ptr::copy_nonoverlapping(code.as_ptr(), slot_ptr, STUB_STRIDE);
    }

    slot_ptr as usize
}

// ---------------------------------------------------------------------------------------------
// Traceur d'API HLE (`SB_TRACE_API`)
// ---------------------------------------------------------------------------------------------

/// Mode du traceur d'appels d'API (variable d'environnement `SB_TRACE_API`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiTraceMode {
    /// Désactivé (défaut) : les imports pointent directement vers l'implémentation.
    Off,
    /// Garde les derniers appels dans un tampon circulaire, affiché dans le rapport de crash.
    Ring,
    /// Comme `Ring`, et journalise chaque appel au fil de l'eau.
    Live,
}

impl ApiTraceMode {
    /// `1` / `ring` / `true` → `Ring` ; `live` / `log` → `Live` ; autre → `Off`.
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("1") | Some("ring") | Some("true") | Some("on") => Self::Ring,
            Some("live") | Some("log") => Self::Live,
            _ => Self::Off,
        }
    }
}

/// Mode courant, lu une fois depuis `SB_TRACE_API`.
pub fn api_trace_mode() -> ApiTraceMode {
    static MODE: OnceLock<ApiTraceMode> = OnceLock::new();
    *MODE.get_or_init(|| ApiTraceMode::parse(std::env::var("SB_TRACE_API").ok().as_deref()))
}

const TRACE_RING: usize = 256;
const TRACE_STRIDE: usize = 192;
const TRACE_POOL_SIZE: usize = 1024 * 1024;

/// Octets de texte conservés par argument (aperçu de chaîne pointée).
const PREVIEW_BYTES: usize = 40;

#[derive(Default)]
struct TraceSlot {
    seq: AtomicU64,
    id: AtomicU64,
    args: [AtomicU64; 4],
    /// Par argument : mot 0 = `genre | longueur << 8` (0 = aucun, 1 = ASCII, 2 = UTF-16), mots
    /// 1..6 = octets de texte (UTF-8 déjà décodé, `PREVIEW_BYTES` au plus).
    preview: [[AtomicU64; 6]; 4],
}

/// Lit `buf.len()` octets de la mémoire du processus à `addr` sans jamais faire de faute :
/// `process_vm_readv` sur soi-même renvoie une erreur (ou une lecture partielle) au lieu d'un
/// SIGSEGV si la page n'est pas mappée.
fn read_self(addr: usize, buf: &mut [u8]) -> usize {
    if !(0x1_0000..0x8000_0000_0000).contains(&addr) {
        return 0;
    }
    let local = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let remote = libc::iovec {
        iov_base: addr as *mut libc::c_void,
        iov_len: buf.len(),
    };
    // SAFETY: les deux `iovec` décrivent un tampon local valide et une plage distante lue par le
    // noyau ; l'appel échoue proprement (EFAULT) si la plage n'est pas lisible.
    let n = unsafe { libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0) };
    if n < 0 { 0 } else { n as usize }
}

/// Aperçu textuel (ASCII ou UTF-16LE imprimable, au moins 3 caractères) de la mémoire à `addr`.
fn text_preview(addr: usize) -> Option<(u8, String)> {
    let mut buf = [0u8; PREVIEW_BYTES * 2];
    let mut n = read_self(addr, &mut buf);
    if n < 8 {
        // Fin de page : réessaie avec une lecture courte.
        n = read_self(addr, &mut buf[..8]);
    }
    let data = &buf[..n];
    let printable = |b: u8| (0x20..0x7F).contains(&b);
    // ASCII / UTF-8 simple.
    let ascii: Vec<u8> = data.iter().copied().take_while(|b| printable(*b)).collect();
    if ascii.len() >= 3 {
        let text: String = ascii
            .iter()
            .take(PREVIEW_BYTES)
            .map(|b| *b as char)
            .collect();
        return Some((1, text));
    }
    // UTF-16LE (caractères ASCII sur 16 bits).
    let wide: String = data
        .as_chunks::<2>()
        .0
        .iter()
        .take_while(|c| c[1] == 0 && printable(c[0]))
        .take(PREVIEW_BYTES)
        .map(|c| c[0] as char)
        .collect();
    (wide.len() >= 3).then_some((2, wide))
}

static TRACE_SEQ: AtomicU64 = AtomicU64::new(0);
static TRACE_SLOTS: OnceLock<Vec<TraceSlot>> = OnceLock::new();
static TRACE_NAMES: OnceLock<RwLock<Vec<(String, String)>>> = OnceLock::new();

fn trace_slots() -> &'static Vec<TraceSlot> {
    TRACE_SLOTS.get_or_init(|| (0..TRACE_RING).map(|_| TraceSlot::default()).collect())
}

fn trace_names() -> &'static RwLock<Vec<(String, String)>> {
    TRACE_NAMES.get_or_init(|| RwLock::new(Vec::new()))
}

/// Appelé par chaque thunk de trace avec `regs = [rcx, rdx, r8, r9]` (sauvegardés sur la pile).
extern "win64" fn api_trace_dispatch(stub_id: u64, regs: *const u64) {
    ffi_guard!("trace", "api_trace_dispatch", (), {
        if regs.is_null() {
            return;
        }
        // SAFETY: `regs` pointe vers 4 `u64` écrits par le thunk juste avant l'appel.
        let a = unsafe { core::slice::from_raw_parts(regs, 4) };
        let seq = TRACE_SEQ.fetch_add(1, Ordering::AcqRel) + 1;
        let slot = &trace_slots()[(seq as usize) % TRACE_RING];
        slot.id.store(stub_id, Ordering::Relaxed);
        for (dst, v) in slot.args.iter().zip(a) {
            dst.store(*v, Ordering::Relaxed);
        }
        // Aperçu des chaînes pointées, capturé maintenant : le tampon peut être réécrit plus tard.
        for (i, v) in a.iter().enumerate() {
            let words = &slot.preview[i];
            match text_preview(*v as usize) {
                Some((kind, text)) => {
                    let bytes = text.as_bytes();
                    let len = bytes.len().min(PREVIEW_BYTES);
                    words[0].store(u64::from(kind) | ((len as u64) << 8), Ordering::Relaxed);
                    for (w, chunk) in words[1..].iter().zip(bytes[..len].chunks(8)) {
                        let mut b = [0u8; 8];
                        b[..chunk.len()].copy_from_slice(chunk);
                        w.store(u64::from_le_bytes(b), Ordering::Relaxed);
                    }
                }
                None => words[0].store(0, Ordering::Relaxed),
            }
        }
        slot.seq.store(seq, Ordering::Release);
        if api_trace_mode() == ApiTraceMode::Live
            && let Some(names) = trace_names().try_read()
            && let Some((dll, sym)) = names.get(stub_id as usize)
        {
            sb_info!(
                "api",
                "{dll}!{sym}({:#x}, {:#x}, {:#x}, {:#x})",
                a[0],
                a[1],
                a[2],
                a[3]
            );
        }
    })
}

/// Alloue un thunk qui note l'appel de `dll!symbol` (4 premiers arguments) puis saute vers
/// `target` : les arguments (registres, XMM0-3 et pile) et l'adresse de retour restent intacts,
/// le thunk est donc transparent pour l'appelant et pour l'implémentation.
#[cfg(target_arch = "x86_64")]
pub fn allocate_trace_thunk(dll: &str, symbol: &str, target: usize) -> usize {
    static POOL: OnceLock<Mutex<TrampolinePool>> = OnceLock::new();
    let stub_id = {
        let mut names = trace_names().write();
        names.push((dll.to_string(), symbol.to_string()));
        (names.len() - 1) as u64
    };
    let pool = POOL.get_or_init(|| {
        // SAFETY: allocation anonyme `mmap` de `TRACE_POOL_SIZE` octets.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                TRACE_POOL_SIZE,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(
            ptr,
            libc::MAP_FAILED,
            "échec mmap du pool de thunks de trace"
        );
        Mutex::new(TrampolinePool {
            base: ptr.cast::<u8>(),
            len: TRACE_POOL_SIZE,
            offset: 0,
        })
    });
    let mut guard = pool.lock();
    assert!(
        guard.offset + TRACE_STRIDE <= guard.len,
        "pool de thunks de trace saturé"
    );
    // SAFETY: `offset + TRACE_STRIDE <= len` : le slot est dans la zone mappée RWX.
    let slot_ptr = unsafe { guard.base.add(guard.offset) };
    guard.offset += TRACE_STRIDE;

    let mut c: Vec<u8> = Vec::with_capacity(TRACE_STRIDE);
    // Cadre 0x88 : entrée rsp ≡ 8 (mod 16) → après `sub`, rsp ≡ 0 (mod 16) pour le `call`.
    // [rsp+0x00..0x20] ombre ; [0x20..0x40] rcx, rdx, r8, r9 ; [0x40..0x80] xmm0-3.
    c.extend_from_slice(&[0x48, 0x81, 0xEC, 0x88, 0x00, 0x00, 0x00]); // sub rsp, 0x88
    c.extend_from_slice(&[0x48, 0x89, 0x4C, 0x24, 0x20]); // mov [rsp+0x20], rcx
    c.extend_from_slice(&[0x48, 0x89, 0x54, 0x24, 0x28]); // mov [rsp+0x28], rdx
    c.extend_from_slice(&[0x4C, 0x89, 0x44, 0x24, 0x30]); // mov [rsp+0x30], r8
    c.extend_from_slice(&[0x4C, 0x89, 0x4C, 0x24, 0x38]); // mov [rsp+0x38], r9
    c.extend_from_slice(&[0x0F, 0x29, 0x44, 0x24, 0x40]); // movaps [rsp+0x40], xmm0
    c.extend_from_slice(&[0x0F, 0x29, 0x4C, 0x24, 0x50]); // movaps [rsp+0x50], xmm1
    c.extend_from_slice(&[0x0F, 0x29, 0x54, 0x24, 0x60]); // movaps [rsp+0x60], xmm2
    c.extend_from_slice(&[0x0F, 0x29, 0x5C, 0x24, 0x70]); // movaps [rsp+0x70], xmm3
    c.extend_from_slice(&[0x48, 0xB9]); // mov rcx, stub_id
    c.extend_from_slice(&stub_id.to_le_bytes());
    c.extend_from_slice(&[0x48, 0x8D, 0x54, 0x24, 0x20]); // lea rdx, [rsp+0x20]
    c.extend_from_slice(&[0x48, 0xB8]); // mov rax, dispatcher
    c.extend_from_slice(&(api_trace_dispatch as *const () as usize as u64).to_le_bytes());
    c.extend_from_slice(&[0xFF, 0xD0]); // call rax
    c.extend_from_slice(&[0x48, 0x8B, 0x4C, 0x24, 0x20]); // mov rcx, [rsp+0x20]
    c.extend_from_slice(&[0x48, 0x8B, 0x54, 0x24, 0x28]); // mov rdx, [rsp+0x28]
    c.extend_from_slice(&[0x4C, 0x8B, 0x44, 0x24, 0x30]); // mov r8, [rsp+0x30]
    c.extend_from_slice(&[0x4C, 0x8B, 0x4C, 0x24, 0x38]); // mov r9, [rsp+0x38]
    c.extend_from_slice(&[0x0F, 0x28, 0x44, 0x24, 0x40]); // movaps xmm0, [rsp+0x40]
    c.extend_from_slice(&[0x0F, 0x28, 0x4C, 0x24, 0x50]); // movaps xmm1, [rsp+0x50]
    c.extend_from_slice(&[0x0F, 0x28, 0x54, 0x24, 0x60]); // movaps xmm2, [rsp+0x60]
    c.extend_from_slice(&[0x0F, 0x28, 0x5C, 0x24, 0x70]); // movaps xmm3, [rsp+0x70]
    c.extend_from_slice(&[0x48, 0x81, 0xC4, 0x88, 0x00, 0x00, 0x00]); // add rsp, 0x88
    c.extend_from_slice(&[0x48, 0xB8]); // mov rax, target
    c.extend_from_slice(&(target as u64).to_le_bytes());
    c.extend_from_slice(&[0xFF, 0xE0]); // jmp rax
    assert!(
        c.len() <= TRACE_STRIDE,
        "thunk de trace trop long : {}",
        c.len()
    );
    c.resize(TRACE_STRIDE, 0x90);

    // SAFETY: `slot_ptr` pointe vers `TRACE_STRIDE` octets valides et mappés RWX.
    unsafe { ptr::copy_nonoverlapping(c.as_ptr(), slot_ptr, TRACE_STRIDE) };
    slot_ptr as usize
}

/// Derniers appels d'API tracés (les plus anciens en premier), au plus `max`, formatés
/// `dll!symbole(a1, a2, a3, a4)`. Vide si le traceur est désactivé. Lecture sans verrou bloquant
/// (utilisable depuis un handler de signal).
pub fn recent_api_calls(max: usize) -> Vec<String> {
    let Some(slots) = TRACE_SLOTS.get() else {
        return Vec::new();
    };
    let Some(names) = TRACE_NAMES.get().and_then(|n| n.try_read()) else {
        return Vec::new();
    };
    let last = TRACE_SEQ.load(Ordering::Acquire);
    let first = last.saturating_sub(max.min(TRACE_RING) as u64 - 1).max(1);
    let mut out = Vec::new();
    for seq in first..=last {
        let slot = &slots[(seq as usize) % TRACE_RING];
        if slot.seq.load(Ordering::Acquire) != seq {
            continue; // entrée écrasée ou en cours d'écriture
        }
        let id = slot.id.load(Ordering::Relaxed) as usize;
        let (dll, sym) = names
            .get(id)
            .map(|(d, s)| (d.as_str(), s.as_str()))
            .unwrap_or(("?", "?"));
        let args: Vec<String> = (0..4)
            .map(|i| {
                let v = slot.args[i].load(Ordering::Relaxed);
                let head = slot.preview[i][0].load(Ordering::Relaxed);
                let (kind, len) = ((head & 0xFF) as u8, ((head >> 8) & 0xFF) as usize);
                if kind == 0 || len == 0 {
                    return format!("{v:#x}");
                }
                let mut bytes = Vec::with_capacity(len);
                for w in &slot.preview[i][1..] {
                    bytes.extend_from_slice(&w.load(Ordering::Relaxed).to_le_bytes());
                }
                bytes.truncate(len);
                let text = String::from_utf8_lossy(&bytes);
                let tag = if kind == 2 { "L" } else { "" };
                format!("{v:#x} {tag}\"{text}\"")
            })
            .collect();
        out.push(format!("#{seq} {dll}!{sym}({})", args.join(", ")));
    }
    out
}

/// Vrai si `addr` est un trampoline JIT de symbole non implémenté (déjà tracé par lui-même).
pub fn is_stub_trampoline(addr: usize) -> bool {
    let guard = pool().lock();
    let base = guard.base as usize;
    addr >= base && addr < base + guard.offset
}

/// Retourne une description lisible si `addr` appartient au pool JIT (utile pour le diagnostic de crash).
pub fn describe_trampoline(addr: usize) -> Option<String> {
    let guard = pool().lock();
    let base = guard.base as usize;
    if addr >= base && addr < base + guard.offset {
        let index = (addr - base) / STUB_STRIDE;
        let entries = stub_entries().read();
        if let Some(entry) = entries.get(index) {
            return Some(format!("jit_stub({}!{})", entry.dll, entry.symbol));
        }
    }
    None
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    #[test]
    fn jit_trampoline_executes_win64_abi_and_returns_configured_value() {
        let addr = allocate_stub_trampoline("fake_xbox.dll", "FakeApiCall", 0x1337_CAFE);
        // SAFETY: `addr` est un trampoline x86-64 conforme `extern "win64"`.
        let func: extern "win64" fn(usize, usize, usize, usize) -> usize =
            unsafe { core::mem::transmute(addr) };

        let ret = func(0x11, 0x22, 0x33, 0x44);
        assert_eq!(ret, 0x1337_CAFE);
        assert_eq!(
            describe_trampoline(addr).as_deref(),
            Some("jit_stub(fake_xbox.dll!FakeApiCall)")
        );
    }

    #[test]
    fn unimplemented_policy_parsing() {
        assert_eq!(UnimplementedPolicy::parse(None), UnimplementedPolicy::Log);
        assert_eq!(
            UnimplementedPolicy::parse(Some("log")),
            UnimplementedPolicy::Log
        );
        assert_eq!(
            UnimplementedPolicy::parse(Some(" STOP ")),
            UnimplementedPolicy::Stop
        );
        assert_eq!(
            UnimplementedPolicy::parse(Some("abort")),
            UnimplementedPolicy::Stop
        );
        assert_eq!(
            UnimplementedPolicy::parse(Some("n'importe")),
            UnimplementedPolicy::Log
        );
    }

    extern "win64" fn target_sum(a: usize, b: usize, c: usize, d: usize) -> usize {
        a + b * 10 + c * 100 + d * 1000
    }

    extern "win64" fn target_fmul(a: f64, b: f64) -> f64 {
        a * b
    }

    #[test]
    fn api_trace_thunk_is_transparent_and_records_calls() {
        let t = allocate_trace_thunk("fake.dll", "Sum", target_sum as *const () as usize);
        // SAFETY: `t` est un thunk x86-64 win64 qui saute vers `target_sum`.
        let f: extern "win64" fn(usize, usize, usize, usize) -> usize =
            unsafe { core::mem::transmute(t) };
        assert_eq!(f(1, 2, 3, 4), 1 + 20 + 300 + 4000);

        // Arguments flottants (XMM0-1) préservés.
        let t2 = allocate_trace_thunk("fake.dll", "Fmul", target_fmul as *const () as usize);
        // SAFETY: idem pour `target_fmul`.
        let g: extern "win64" fn(f64, f64) -> f64 = unsafe { core::mem::transmute(t2) };
        assert_eq!(g(1.5, 4.0), 6.0);

        let calls = recent_api_calls(8);
        assert!(
            calls
                .iter()
                .any(|c| c.contains("fake.dll!Sum(0x1, 0x2, 0x3, 0x4)")),
            "{calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.contains("fake.dll!Fmul")),
            "{calls:?}"
        );
    }

    #[test]
    fn api_trace_records_string_arguments_ascii_and_utf16() {
        let ascii = b"GAME:\\Media\\a.bin\0";
        let wide: Vec<u16> = "G:\\x\\b.bin\0".encode_utf16().collect();
        let t = allocate_trace_thunk("fake.dll", "OpenPath", target_sum as *const () as usize);
        // SAFETY: thunk x86-64 win64 vers `target_sum` (les arguments ne sont pas déréférencés).
        let f: extern "win64" fn(usize, usize, usize, usize) -> usize =
            unsafe { core::mem::transmute(t) };
        f(ascii.as_ptr() as usize, wide.as_ptr() as usize, 0x1234, 0);
        let calls = recent_api_calls(4);
        let line = calls
            .iter()
            .rev()
            .find(|c| c.contains("fake.dll!OpenPath"))
            .expect("appel tracé");
        assert!(line.contains(r#""GAME:\Media\a.bin""#), "{line}");
        assert!(line.contains(r#"L"G:\x\b.bin""#), "{line}");
        assert!(
            line.contains("0x1234,") || line.contains("0x1234"),
            "{line}"
        );
        // Pointeur invalide : aucune faute, pas d'aperçu.
        f(0xDEAD_0000, 0x10, 0, 0);
        assert!(recent_api_calls(1)[0].contains("0xdead0000,"));
    }

    #[test]
    fn api_trace_mode_parsing() {
        assert_eq!(ApiTraceMode::parse(None), ApiTraceMode::Off);
        assert_eq!(ApiTraceMode::parse(Some("1")), ApiTraceMode::Ring);
        assert_eq!(ApiTraceMode::parse(Some(" LIVE ")), ApiTraceMode::Live);
        assert_eq!(ApiTraceMode::parse(Some("non")), ApiTraceMode::Off);
    }
}
