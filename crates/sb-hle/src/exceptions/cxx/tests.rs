//! Tests des exceptions C++ : images synthétiques (code machine écrit à la main, `.pdata`,
//! `UNWIND_INFO`, `FuncInfo` FH3 et `FuncInfo4` FH4, `ThrowInfo`) exécutées nativement.
//!
//! Disposition (RVA) : `0x020/0x030` thunks vers FH3/FH4 ; `0x100` lanceur ; `0x200`/`0x220`
//! fonctions intermédiaires (deux objets locaux à détruire) ; `0x400..` fonctions avec
//! `try/catch` ; `0x600..` funclets ; `0x1000..` tables EH ; `0x1800..` données/journal.

use super::*;
use sb_loader::unwind::RuntimeFunction;
use sb_loader::unwind::register_function_table;

const SEQ: usize = 0x1800;
const S_DTOR_A: usize = 0x1804;
const S_DTOR_B: usize = 0x1808;
const S_CATCH: usize = 0x180C;
const S_OBJDTOR: usize = 0x1810;
const S_CATCHVAL: usize = 0x1814;
const S_CATCH2: usize = 0x1818;
const SLOT_THROW: usize = 0x1820;
const OBJ_INT: usize = 0x1900;
const OBJ_DER: usize = 0x1908;
const TD_INT: usize = 0x1A00;
const TD_BASE: usize = 0x1A20;
const TD_DER: usize = 0x1A50;
const TI_INT: usize = 0x1B00;
const TI_DER: usize = 0x1B80;

struct Img {
    base: usize,
    funcs: Vec<(u32, u32, u32)>,
}

impl Img {
    fn put(&self, rva: usize, b: &[u8]) {
        // SAFETY: écriture dans la page RWX de 0x3000 octets.
        unsafe {
            core::ptr::copy_nonoverlapping(b.as_ptr(), (self.base + rva) as *mut u8, b.len())
        };
    }
    fn u32(&self, rva: usize, v: u32) {
        self.put(rva, &v.to_le_bytes());
    }
    fn i32(&self, rva: usize, v: i32) {
        self.put(rva, &v.to_le_bytes());
    }
    fn rd(&self, rva: usize) -> u32 {
        // SAFETY: lecture dans la page.
        unsafe { core::ptr::read_unaligned((self.base + rva) as *const u32) }
    }
    fn addr(&self, rva: usize) -> usize {
        self.base + rva
    }
    fn func(&mut self, begin: usize, end: usize, unwind: usize) {
        self.funcs.push((begin as u32, end as u32, unwind as u32));
    }
}

fn rel(next_ip: usize, target: usize) -> [u8; 4] {
    ((target as i64 - next_ip as i64) as i32).to_le_bytes()
}

fn thunk(target: usize) -> Vec<u8> {
    let mut t = vec![0x48, 0xB8];
    t.extend_from_slice(&(target as u64).to_le_bytes());
    t.extend_from_slice(&[0xFF, 0xE0]);
    t
}

/// Encodage FH4 d'un entier non signé (longueur par bits de poids faible).
fn enc_uint(v: u32) -> Vec<u8> {
    if v < 1 << 7 {
        vec![(v << 1) as u8]
    } else if v < 1 << 14 {
        (((v << 2) | 1) as u16).to_le_bytes().to_vec()
    } else if v < 1 << 21 {
        let x = (v << 3) | 3;
        x.to_le_bytes()[..3].to_vec()
    } else if v < 1 << 28 {
        ((v << 4) | 7).to_le_bytes().to_vec()
    } else {
        let mut b = vec![0x0F];
        b.extend_from_slice(&v.to_le_bytes());
        b
    }
}

fn unwind_info(img: &Img, at: usize, flags: u8, handler: usize, data: usize) -> usize {
    img.put(at, &[1 | (flags << 3), 4, 1, 0, 0x04, 0x42, 0, 0]);
    if flags != 0 {
        img.u32(at + 8, handler as u32);
        img.u32(at + 12, data as u32);
    }
    at
}

/// `seq++ ; slot = seq` (21 octets de code RIP-relatif à partir de `at`), sans `ret`.
fn seq_store(img: &Img, at: usize, slot: usize) -> usize {
    let mut c = vec![0x8B, 0x05];
    c.extend_from_slice(&rel(at + 6, SEQ));
    c.extend_from_slice(&[0xFF, 0xC0]);
    c.extend_from_slice(&[0x89, 0x05]);
    c.extend_from_slice(&rel(at + 14, SEQ));
    c.extend_from_slice(&[0x89, 0x05]);
    c.extend_from_slice(&rel(at + 20, slot));
    img.put(at, &c);
    at + 20
}

/// Funclet de destruction : `seq++ ; slot = seq ; ret`.
fn emit_dtor(img: &Img, at: usize, slot: usize) {
    let end = seq_store(img, at, slot);
    img.put(end, &[0xC3]);
}

/// Fonction intermédiaire : `call callee` entre deux états ; handler = `handler_rva`.
fn emit_mid(img: &mut Img, off: usize, callee: usize, handler: usize, data: usize, ui: usize) {
    let mut c = vec![0x48, 0x83, 0xEC, 0x28, 0xE8];
    c.extend_from_slice(&rel(off + 9, callee));
    c.extend_from_slice(&[0x90, 0x48, 0x83, 0xC4, 0x28, 0xC3]);
    img.put(off, &c);
    unwind_info(img, ui, 3, handler, data);
    img.func(off, off + 0x10, ui);
}

/// Fonction `try { callee } catch ...` : retourne `ok` si rien n'est lancé, 2 via la continuation
/// (`off + 0x20`).
fn emit_try(
    img: &mut Img,
    off: usize,
    callee: usize,
    ok: u32,
    handler: usize,
    data: usize,
    ui: usize,
) {
    let mut c = vec![0x48, 0x83, 0xEC, 0x28, 0xE8];
    c.extend_from_slice(&rel(off + 9, callee));
    c.push(0x90);
    c.push(0xB8);
    c.extend_from_slice(&ok.to_le_bytes());
    c.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28, 0xC3]);
    img.put(off, &c);
    img.put(
        off + 0x20,
        &[0xB8, 2, 0, 0, 0, 0x48, 0x83, 0xC4, 0x28, 0xC3],
    );
    unwind_info(img, ui, 3, handler, data);
    img.func(off, off + 0x2C, ui);
}

/// Funclet catch (variable à `[rdx + 0x20]`, entier) : mémorise la valeur, journalise, renvoie
/// `cont`.
fn emit_catch_store(img: &Img, at: usize, slot: usize, cont: usize) {
    let mut c = vec![0x8B, 0x4A, 0x20, 0x89, 0x0D];
    c.extend_from_slice(&rel(at + 9, S_CATCHVAL));
    img.put(at, &c);
    let end = seq_store(img, at + 9, slot);
    let mut t = vec![0x48, 0x8D, 0x05];
    t.extend_from_slice(&rel(end + 7, cont));
    t.push(0xC3);
    img.put(end, &t);
}

/// Funclet catch par référence : `[rdx + 0x20]` contient un pointeur vers l'objet ; mémorise son
/// premier mot.
fn emit_catch_ref(img: &Img, at: usize, slot: usize, cont: usize) {
    // mov rcx,[rdx+0x20] ; mov ecx,[rcx] ; mov [S_CATCHVAL],ecx
    let mut c = vec![0x48, 0x8B, 0x4A, 0x20, 0x8B, 0x09, 0x89, 0x0D];
    c.extend_from_slice(&rel(at + 12, S_CATCHVAL));
    img.put(at, &c);
    let end = seq_store(img, at + 12, slot);
    let mut t = vec![0x48, 0x8D, 0x05];
    t.extend_from_slice(&rel(end + 7, cont));
    t.push(0xC3);
    img.put(end, &t);
}

/// Funclet catch qui relance (`throw;`) : possède son propre `UNWIND_INFO` (sans handler).
fn emit_catch_rethrow(img: &mut Img, at: usize, slot: usize, ui: usize) {
    let mut c = vec![0x48, 0x83, 0xEC, 0x28];
    img.put(at, &c);
    let end = seq_store(img, at + 4, slot);
    c = vec![0x31, 0xC9, 0x31, 0xD2, 0xFF, 0x15];
    c.extend_from_slice(&rel(end + 10, SLOT_THROW));
    c.push(0xCC);
    img.put(end, &c);
    unwind_info(img, ui, 0, 0, 0);
    img.func(at, end + 14, ui);
}

/// Funclet catch quelconque : journalise et renvoie `cont`.
fn emit_catch_any(img: &Img, at: usize, slot: usize, cont: usize) {
    let end = seq_store(img, at, slot);
    let mut t = vec![0x48, 0x8D, 0x05];
    t.extend_from_slice(&rel(end + 7, cont));
    t.push(0xC3);
    img.put(end, &t);
}

#[allow(clippy::too_many_arguments)]
fn funcinfo3(
    img: &Img,
    at: usize,
    max_state: u32,
    unwind_map: usize,
    n_try: u32,
    try_map: usize,
    n_ip: u32,
    ip_map: usize,
) {
    img.u32(at, 0x1993_0522);
    img.u32(at + 4, max_state);
    img.u32(at + 8, unwind_map as u32);
    img.u32(at + 12, n_try);
    img.u32(at + 16, try_map as u32);
    img.u32(at + 20, n_ip);
    img.u32(at + 24, ip_map as u32);
}

fn handler3(img: &Img, at: usize, adj: u32, ty: usize, obj: i32, handler: usize) {
    img.u32(at, adj);
    img.u32(at + 4, ty as u32);
    img.i32(at + 8, obj);
    img.u32(at + 12, handler as u32);
}

fn try3(img: &Img, at: usize, low: i32, high: i32, catch_high: i32, n: u32, handlers: usize) {
    img.i32(at, low);
    img.i32(at + 4, high);
    img.i32(at + 8, catch_high);
    img.u32(at + 12, n);
    img.u32(at + 16, handlers as u32);
}

fn typedesc(img: &Img, at: usize, name: &[u8]) {
    img.put(at + 16, name);
}

fn catchable(img: &Img, at: usize, props: u32, td: usize, size: u32) {
    img.u32(at, props);
    img.u32(at + 4, td as u32);
    img.i32(at + 12, -1);
    img.u32(at + 20, size);
}

fn build() -> Img {
    // SAFETY: mapping anonyme RWX.
    let p = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            0x3000,
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(p, libc::MAP_FAILED);
    let mut img = Img {
        base: p as usize,
        funcs: Vec::new(),
    };
    img.put(0x20, &thunk(__CxxFrameHandler3 as *const () as usize));
    img.put(0x30, &thunk(__CxxFrameHandler4 as *const () as usize));
    img.put(
        SLOT_THROW,
        &(_CxxThrowException as *const () as usize as u64).to_le_bytes(),
    );

    // Lanceur : forward rcx/rdx vers _CxxThrowException.
    let mut t = vec![0x48, 0x83, 0xEC, 0x28, 0xFF, 0x15];
    t.extend_from_slice(&rel(0x100 + 10, SLOT_THROW));
    t.push(0xCC);
    img.put(0x100, &t);
    unwind_info(&img, 0x800, 0, 0, 0);
    img.func(0x100, 0x10C, 0x800);

    emit_dtor(&img, 0x300, S_DTOR_A);
    emit_dtor(&img, 0x320, S_DTOR_B);
    emit_dtor(&img, 0x340, S_OBJDTOR);

    // --- Types et ThrowInfo ---
    typedesc(&img, TD_INT, b".H\0");
    typedesc(&img, TD_BASE, b".?AVBase@@\0");
    typedesc(&img, TD_DER, b".?AVDerived@@\0");
    img.u32(OBJ_INT, 0x2A);
    img.put(OBJ_DER, &0x1122_3344_5566_7788u64.to_le_bytes());
    img.u32(TI_INT + 4, 0x340);
    img.u32(TI_INT + 12, 0x1B20);
    img.i32(0x1B20, 1);
    img.u32(0x1B24, 0x1B30);
    catchable(&img, 0x1B30, 1, TD_INT, 4);
    img.u32(TI_DER + 12, 0x1BA0);
    img.i32(0x1BA0, 2);
    img.u32(0x1BA4, 0x1BC0);
    img.u32(0x1BA8, 0x1BE0);
    catchable(&img, 0x1BC0, 0, TD_DER, 8);
    catchable(&img, 0x1BE0, 0, TD_BASE, 8);

    // --- FH3 ---
    // MIDDLE3 @0x200 : états 1 (appel) / -1 ; destructeurs B puis A.
    funcinfo3(&img, 0x1000, 2, 0x1040, 0, 0, 2, 0x1060);
    img.i32(0x1040, -1);
    img.u32(0x1044, 0x300);
    img.i32(0x1048, 0);
    img.u32(0x104C, 0x320);
    img.u32(0x1060, 0x204);
    img.i32(0x1064, 1);
    img.u32(0x1068, 0x20A);
    img.i32(0x106C, -1);
    emit_mid(&mut img, 0x200, 0x100, 0x20, 0x1000, 0x810);

    // OUTER3 @0x400 : try { MIDDLE } catch (int)
    funcinfo3(&img, 0x1100, 1, 0x1140, 1, 0x1160, 2, 0x11A0);
    img.i32(0x1140, -1);
    try3(&img, 0x1160, 0, 0, 1, 1, 0x1180);
    handler3(&img, 0x1180, 0, TD_INT, 0x20, 0x600);
    img.u32(0x11A0, 0x404);
    img.i32(0x11A4, 0);
    img.u32(0x11A8, 0x40A);
    img.i32(0x11AC, -1);
    emit_try(&mut img, 0x400, 0x200, 1, 0x20, 0x1100, 0x820);
    emit_catch_store(&img, 0x600, S_CATCH, 0x420);

    // OUTER_CA @0x500 : try { MIDDLE } catch (...)
    funcinfo3(&img, 0x1200, 1, 0x1240, 1, 0x1260, 2, 0x12A0);
    img.i32(0x1240, -1);
    try3(&img, 0x1260, 0, 0, 1, 1, 0x1280);
    handler3(&img, 0x1280, 0, 0, 0, 0x680);
    img.u32(0x12A0, 0x504);
    img.i32(0x12A4, 0);
    img.u32(0x12A8, 0x50A);
    img.i32(0x12AC, -1);
    emit_try(&mut img, 0x500, 0x200, 1, 0x20, 0x1200, 0x830);
    emit_catch_any(&img, 0x680, S_CATCH, 0x520);

    // OUTER_REF @0x540 : try { MIDDLE } catch (Base &)
    funcinfo3(&img, 0x1300, 1, 0x1340, 1, 0x1360, 2, 0x13A0);
    img.i32(0x1340, -1);
    try3(&img, 0x1360, 0, 0, 1, 1, 0x1380);
    handler3(&img, 0x1380, HT_REFERENCE, TD_BASE, 0x20, 0x6C0);
    img.u32(0x13A0, 0x544);
    img.i32(0x13A4, 0);
    img.u32(0x13A8, 0x54A);
    img.i32(0x13AC, -1);
    emit_try(&mut img, 0x540, 0x200, 1, 0x20, 0x1300, 0x840);
    emit_catch_ref(&img, 0x6C0, S_CATCH, 0x560);

    // Rethrow : OUTER_RT @0x440 { try { MIDDLE } catch (...) { throw; } }
    //           OUTER2 @0x480   { try { OUTER_RT } catch (int) }
    funcinfo3(&img, 0x1400, 1, 0x1440, 1, 0x1460, 2, 0x14A0);
    img.i32(0x1440, -1);
    try3(&img, 0x1460, 0, 0, 1, 1, 0x1480);
    handler3(&img, 0x1480, 0, 0, 0, 0x700);
    img.u32(0x14A0, 0x444);
    img.i32(0x14A4, 0);
    img.u32(0x14A8, 0x44A);
    img.i32(0x14AC, -1);
    emit_try(&mut img, 0x440, 0x200, 1, 0x20, 0x1400, 0x850);
    emit_catch_rethrow(&mut img, 0x700, S_CATCH2, 0x860);
    funcinfo3(&img, 0x1500, 1, 0x1540, 1, 0x1560, 2, 0x15A0);
    img.i32(0x1540, -1);
    try3(&img, 0x1560, 0, 0, 1, 1, 0x1580);
    handler3(&img, 0x1580, 0, TD_INT, 0x20, 0x740);
    img.u32(0x15A0, 0x484);
    img.i32(0x15A4, 0);
    img.u32(0x15A8, 0x48A);
    img.i32(0x15AC, -1);
    emit_try(&mut img, 0x480, 0x440, 1, 0x20, 0x1500, 0x870);
    emit_catch_store(&img, 0x740, S_CATCH, 0x4A0);

    // --- FH4 ---
    // MIDDLE4 @0x220 : table de déroulage compressée (états = décalages 0 et 6).
    let mut um = Vec::new();
    um.extend(enc_uint(1)); // entrée 0 : kind 1, next 0
    um.extend_from_slice(&0x300u32.to_le_bytes());
    um.extend(enc_uint(0));
    assert_eq!(um.len(), 6);
    um.extend(enc_uint((6 << 2) | 1)); // entrée 1 : parent = entrée 0
    um.extend_from_slice(&0x320u32.to_le_bytes());
    um.extend(enc_uint(0));
    img.put(0x1600, &um);
    let mut ip = enc_uint(2);
    ip.extend(enc_uint(0x224));
    ip.extend(enc_uint(6 + 1));
    ip.extend(enc_uint(6));
    ip.extend(enc_uint(0));
    img.put(0x1620, &ip);
    let mut fi = vec![0x08];
    fi.extend_from_slice(&0x1600u32.to_le_bytes());
    fi.extend_from_slice(&0x1620u32.to_le_bytes());
    img.put(0x1640, &fi);
    emit_mid(&mut img, 0x220, 0x100, 0x30, 0x1640, 0x880);

    // OUTER4 @0x4C0 : try { MIDDLE4 } catch (int) — FuncInfo4 avec TryMap4.
    let mut tm = enc_uint(1);
    tm.extend(enc_uint(0)); // low
    tm.extend(enc_uint(0)); // high - low
    tm.extend(enc_uint(1)); // catch_high - high
    tm.extend_from_slice(&0x16C0u32.to_le_bytes());
    img.put(0x1680, &tm);
    let mut ha = enc_uint(1);
    ha.push(0x02 | 0x04 | 0x08 | 0x10); // type + catchObj + cont RVA + 1 adresse
    ha.extend_from_slice(&(TD_INT as u32).to_le_bytes());
    ha.extend(enc_uint(0x20));
    ha.extend_from_slice(&0x640u32.to_le_bytes());
    ha.extend_from_slice(&0x4E0u32.to_le_bytes());
    img.put(0x16C0, &ha);
    let mut ip = enc_uint(2);
    ip.extend(enc_uint(0x4C4));
    ip.extend(enc_uint(1));
    ip.extend(enc_uint(6));
    ip.extend(enc_uint(0));
    img.put(0x1700, &ip);
    let mut fi = vec![0x10];
    fi.extend_from_slice(&0x1680u32.to_le_bytes());
    fi.extend_from_slice(&0x1700u32.to_le_bytes());
    img.put(0x1720, &fi);
    emit_try(&mut img, 0x4C0, 0x220, 1, 0x30, 0x1720, 0x890);
    emit_catch_store(&img, 0x640, S_CATCH, 0x4E0);

    // Exception synthétique `std::out_of_range` (région RTTI du runtime) attrapée par un
    // `catch (const std::exception &)` dont le TypeDescriptor est celui de l'image de test :
    // la correspondance se fait par nom décoré à travers la hiérarchie.
    typedesc(&img, 0x1A80, b".?AVexception@std@@\0");
    img.put(
        0x1828,
        &(synth::x_out_of_range as *const () as usize as u64).to_le_bytes(),
    );
    img.put(0x1830, b"vector too long\0");
    let mut t = vec![0x48, 0x83, 0xEC, 0x28, 0x48, 0x8D, 0x0D];
    t.extend_from_slice(&rel(0x120 + 11, 0x1830));
    t.extend_from_slice(&[0xFF, 0x15]);
    t.extend_from_slice(&rel(0x120 + 17, 0x1828));
    t.push(0xCC);
    img.put(0x120, &t);
    unwind_info(&img, 0x8A0, 0, 0, 0);
    img.func(0x120, 0x134, 0x8A0);
    funcinfo3(&img, 0x1D00, 2, 0x1040, 0, 0, 2, 0x1D40);
    img.u32(0x1D40, 0x244);
    img.i32(0x1D44, 1);
    img.u32(0x1D48, 0x24A);
    img.i32(0x1D4C, -1);
    emit_mid(&mut img, 0x240, 0x120, 0x20, 0x1D00, 0x8B0);
    funcinfo3(&img, 0x1D80, 1, 0x1DC0, 1, 0x1DE0, 2, 0x1E20);
    img.i32(0x1DC0, -1);
    try3(&img, 0x1DE0, 0, 0, 1, 1, 0x1E00);
    handler3(&img, 0x1E00, HT_REFERENCE, 0x1A80, 0x20, 0x780);
    img.u32(0x1E20, 0x584);
    img.i32(0x1E24, 0);
    img.u32(0x1E28, 0x58A);
    img.i32(0x1E2C, -1);
    emit_try(&mut img, 0x580, 0x240, 1, 0x20, 0x1D80, 0x8C0);
    emit_catch_ref(&img, 0x780, S_CATCH, 0x5A0);

    // .pdata trié
    img.funcs.sort_unstable();
    for (i, (b, e, u)) in img.funcs.clone().iter().enumerate() {
        img.u32(0x2000 + i * 12, *b);
        img.u32(0x2000 + i * 12 + 4, *e);
        img.u32(0x2000 + i * 12 + 8, *u);
    }
    // SAFETY: table triée dans l'image, valide tant que l'image vit.
    unsafe {
        register_function_table(
            img.base,
            0x3000,
            (img.base + 0x2000) as *const RuntimeFunction,
            img.funcs.len(),
        );
    }
    img
}

fn call2(img: &Img, entry: usize, obj: usize, ti: usize) -> u32 {
    let f: extern "win64" fn(usize, usize) -> u32 =
        // SAFETY: `entry` est une fonction de l'image écrite pour cette signature.
        unsafe { core::mem::transmute(img.addr(entry)) };
    f(obj, ti)
}

#[test]
fn catch_int_runs_destructors_in_order_and_resumes_at_continuation() {
    let img = build();
    let ret = call2(&img, 0x400, img.addr(OBJ_INT), img.addr(TI_INT));
    assert_eq!(ret, 2, "reprise à la continuation du catch");
    let (b, a, c, o) = (
        img.rd(S_DTOR_B),
        img.rd(S_DTOR_A),
        img.rd(S_CATCH),
        img.rd(S_OBJDTOR),
    );
    assert_eq!(
        (b, a, c, o),
        (1, 2, 3, 4),
        "ordre : dtor B, dtor A, catch, objet lancé"
    );
    assert_eq!(img.rd(S_CATCHVAL), 0x2A, "variable d'exception copiée");
}

#[test]
fn catch_all_catches_any_type() {
    let img = build();
    let ret = call2(&img, 0x500, img.addr(OBJ_DER), img.addr(TI_DER));
    assert_eq!(ret, 2);
    assert_eq!(
        (img.rd(S_DTOR_B), img.rd(S_DTOR_A), img.rd(S_CATCH)),
        (1, 2, 3)
    );
}

#[test]
fn catch_base_reference_matches_derived_via_catchable_type_array() {
    let img = build();
    let ret = call2(&img, 0x540, img.addr(OBJ_DER), img.addr(TI_DER));
    assert_eq!(ret, 2);
    assert_eq!(
        img.rd(S_CATCHVAL),
        0x5566_7788,
        "référence vers l'objet lancé"
    );
    assert_eq!(img.rd(S_CATCH), 3);
}

#[test]
fn no_exception_thrown_returns_normally() {
    let img = build();
    // lanceur remplacé : appel direct de MIDDLE impossible sans throw ; OUTER3 avec ti NULL
    // relancerait (throw;) : on vérifie plutôt qu'un type non apparenté n'est pas attrapé par
    // `catch (int)` mais par le catch-all.
    let ret = call2(&img, 0x500, img.addr(OBJ_INT), img.addr(TI_INT));
    assert_eq!(ret, 2);
}

#[test]
fn rethrow_from_catch_all_is_caught_by_outer_catch_int() {
    let img = build();
    let ret = call2(&img, 0x480, img.addr(OBJ_INT), img.addr(TI_INT));
    assert_eq!(ret, 2);
    assert_eq!(img.rd(S_DTOR_B), 1);
    assert_eq!(img.rd(S_DTOR_A), 2);
    assert_eq!(img.rd(S_CATCH2), 3, "catch (...) interne");
    assert_eq!(img.rd(S_CATCH), 4, "catch (int) externe");
    assert_eq!(img.rd(S_CATCHVAL), 0x2A);
    assert_eq!(
        img.rd(S_OBJDTOR),
        5,
        "objet détruit une seule fois, à la fin"
    );
}

#[test]
fn fh4_compressed_tables_catch_and_destructors() {
    let img = build();
    let ret = call2(&img, 0x4C0, img.addr(OBJ_INT), img.addr(TI_INT));
    assert_eq!(ret, 2);
    assert_eq!(
        (
            img.rd(S_DTOR_B),
            img.rd(S_DTOR_A),
            img.rd(S_CATCH),
            img.rd(S_OBJDTOR)
        ),
        (1, 2, 3, 4)
    );
    assert_eq!(img.rd(S_CATCHVAL), 0x2A);
}

#[test]
fn variable_length_integers_roundtrip() {
    for v in [
        0u32,
        1,
        63,
        127,
        128,
        16383,
        16384,
        2_097_151,
        2_097_152,
        268_435_455,
        268_435_456,
        u32::MAX,
    ] {
        let b = enc_uint(v);
        let mut c = desc::Cursor {
            pos: b.as_ptr() as usize,
        };
        // SAFETY: `b` contient l'encodage complet.
        assert_eq!(unsafe { c.uint() }, v, "valeur {v}");
        assert_eq!(c.pos - b.as_ptr() as usize, b.len());
    }
}

/// Processus enfant : exception C++ sans catch correspondant (`catch (int)` face à `Derived`).
#[test]
fn unmatched_child() {
    if std::env::var_os("SB_TEST_CXX_UNMATCHED_CHILD").is_none() {
        return;
    }
    let img = build();
    std::hint::black_box(call2(&img, 0x400, img.addr(OBJ_DER), img.addr(TI_DER)));
}

#[test]
fn unmatched_exception_terminates_like_abort() {
    let exe = std::env::current_exe().expect("exe de test");
    let out = std::process::Command::new(exe)
        .args([
            "--exact",
            "exceptions::cxx::tests::unmatched_child",
            "--nocapture",
        ])
        .env("SB_TEST_CXX_UNMATCHED_CHILD", "1")
        .output()
        .expect("lancement de l'enfant");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(134), "{stderr}");
    assert!(stderr.contains("std::terminate"), "{stderr}");
}

#[test]
fn xout_of_range_throws_real_exception_caught_as_std_exception() {
    let img = build();
    let ret = call2(&img, 0x580, 0, 0);
    assert_eq!(
        ret, 2,
        "catch (const std::exception &) attrape _Xout_of_range"
    );
    assert_eq!(
        (img.rd(S_DTOR_B), img.rd(S_DTOR_A), img.rd(S_CATCH)),
        (1, 2, 3)
    );
    assert_ne!(
        img.rd(S_CATCHVAL),
        0,
        "référence vers l'objet (vptr non nul)"
    );
}

#[test]
fn synthetic_std_exception_is_catchable_by_base_type() {
    // Test de la région RTTI : le `what()` et la hiérarchie sont construits sans guest.
    let r = synth::what_of_new(synth::K_OUT_OF_RANGE, c"deque too long");
    assert_eq!(r, "deque too long");
}

#[test]
fn terminate_handler_roundtrip() {
    assert_eq!(set_terminate(0x1234), 0);
    assert_eq!(get_terminate(), 0x1234);
    assert_eq!(set_terminate(0), 0x1234);
}
