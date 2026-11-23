//! Déroulage de pile Win64 (SEH x64) : registre des tables `.pdata`
//! (`IMAGE_DIRECTORY_ENTRY_EXCEPTION`) par module, recherche binaire de `RUNTIME_FUNCTION`
//! et interprétation des `UNWIND_INFO` (équivalent de `RtlLookupFunctionEntry` /
//! `RtlVirtualUnwind`).
//!
//! Référence : documentation publique Microsoft « x64 exception handling ».

use core::mem::{offset_of, size_of};
use std::ptr;

use parking_lot::RwLock;

/// `RUNTIME_FUNCTION` x64 (12 octets, RVAs relatives à l'image).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeFunction {
    pub begin_address: u32,
    pub end_address: u32,
    pub unwind_data: u32,
}

/// `M128A` (registre XMM 128 bits).
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct M128A {
    pub low: u64,
    pub high: u64,
}

/// `CONTEXT` x64 Windows (0x4D0 octets, aligné sur 16).
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct Context {
    pub p1_home: u64,
    pub p2_home: u64,
    pub p3_home: u64,
    pub p4_home: u64,
    pub p5_home: u64,
    pub p6_home: u64,
    pub context_flags: u32,
    pub mx_csr: u32,
    pub seg_cs: u16,
    pub seg_ds: u16,
    pub seg_es: u16,
    pub seg_fs: u16,
    pub seg_gs: u16,
    pub seg_ss: u16,
    pub eflags: u32,
    pub dr0: u64,
    pub dr1: u64,
    pub dr2: u64,
    pub dr3: u64,
    pub dr6: u64,
    pub dr7: u64,
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rbx: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    /// `XMM_SAVE_AREA32` (format `fxsave`, 512 octets).
    pub flt_save: XmmSaveArea32,
    pub vector_register: [M128A; 26],
    pub vector_control: u64,
    pub debug_control: u64,
    pub last_branch_to_rip: u64,
    pub last_branch_from_rip: u64,
    pub last_exception_to_rip: u64,
    pub last_exception_from_rip: u64,
}

/// `XMM_SAVE_AREA32` (image mémoire de `fxsave64`).
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct XmmSaveArea32 {
    pub control_word: u16,
    pub status_word: u16,
    pub tag_word: u8,
    pub reserved1: u8,
    pub error_opcode: u16,
    pub error_offset: u32,
    pub error_selector: u16,
    pub reserved2: u16,
    pub data_offset: u32,
    pub data_selector: u16,
    pub reserved3: u16,
    pub mx_csr: u32,
    pub mx_csr_mask: u32,
    pub float_registers: [M128A; 8],
    pub xmm_registers: [M128A; 16],
    pub reserved4: [u8; 96],
}

/// `KNONVOLATILE_CONTEXT_POINTERS` x64 (256 octets).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KNonvolatileContextPointers {
    pub floating_context: [*mut M128A; 16],
    pub integer_context: [*mut u64; 16],
}

const _: () = {
    assert!(size_of::<RuntimeFunction>() == 12);
    assert!(size_of::<M128A>() == 16);
    assert!(size_of::<XmmSaveArea32>() == 512);
    assert!(offset_of!(XmmSaveArea32, mx_csr) == 24);
    assert!(offset_of!(XmmSaveArea32, xmm_registers) == 160);
    assert!(size_of::<Context>() == 0x4D0);
    assert!(core::mem::align_of::<Context>() == 16);
    assert!(offset_of!(Context, context_flags) == 0x30);
    assert!(offset_of!(Context, mx_csr) == 0x34);
    assert!(offset_of!(Context, seg_cs) == 0x38);
    assert!(offset_of!(Context, seg_ss) == 0x42);
    assert!(offset_of!(Context, eflags) == 0x44);
    assert!(offset_of!(Context, dr0) == 0x48);
    assert!(offset_of!(Context, rax) == 0x78);
    assert!(offset_of!(Context, rsp) == 0x98);
    assert!(offset_of!(Context, r15) == 0xF0);
    assert!(offset_of!(Context, rip) == 0xF8);
    assert!(offset_of!(Context, flt_save) == 0x100);
    assert!(offset_of!(Context, vector_register) == 0x300);
    assert!(offset_of!(Context, vector_control) == 0x4A0);
    assert!(offset_of!(Context, last_exception_from_rip) == 0x4C8);
    assert!(size_of::<KNonvolatileContextPointers>() == 256);
};

impl Default for Context {
    fn default() -> Self {
        // SAFETY: `Context` est une struct POD (entiers et tableaux d'entiers) : le motif
        // tout-à-zéro est une valeur valide.
        unsafe { core::mem::zeroed() }
    }
}

impl Context {
    /// Registre entier par numéro d'opérande d'unwind (0=RAX … 15=R15).
    pub const fn gpr(&self, index: u8) -> u64 {
        match index & 15 {
            0 => self.rax,
            1 => self.rcx,
            2 => self.rdx,
            3 => self.rbx,
            4 => self.rsp,
            5 => self.rbp,
            6 => self.rsi,
            7 => self.rdi,
            8 => self.r8,
            9 => self.r9,
            10 => self.r10,
            11 => self.r11,
            12 => self.r12,
            13 => self.r13,
            14 => self.r14,
            _ => self.r15,
        }
    }

    /// Accès mutable au registre entier numéro `index` (0=RAX … 15=R15).
    pub const fn gpr_mut(&mut self, index: u8) -> &mut u64 {
        match index & 15 {
            0 => &mut self.rax,
            1 => &mut self.rcx,
            2 => &mut self.rdx,
            3 => &mut self.rbx,
            4 => &mut self.rsp,
            5 => &mut self.rbp,
            6 => &mut self.rsi,
            7 => &mut self.rdi,
            8 => &mut self.r8,
            9 => &mut self.r9,
            10 => &mut self.r10,
            11 => &mut self.r11,
            12 => &mut self.r12,
            13 => &mut self.r13,
            14 => &mut self.r14,
            _ => &mut self.r15,
        }
    }
}

// Flags `UNWIND_INFO`.
pub const UNW_FLAG_NHANDLER: u32 = 0;
pub const UNW_FLAG_EHANDLER: u32 = 1;
pub const UNW_FLAG_UHANDLER: u32 = 2;
pub const UNW_FLAG_CHAININFO: u32 = 4;

// Opcodes d'unwind.
const UWOP_PUSH_NONVOL: u8 = 0;
const UWOP_ALLOC_LARGE: u8 = 1;
const UWOP_ALLOC_SMALL: u8 = 2;
const UWOP_SET_FPREG: u8 = 3;
const UWOP_SAVE_NONVOL: u8 = 4;
const UWOP_SAVE_NONVOL_FAR: u8 = 5;
const UWOP_EPILOG: u8 = 6;
const UWOP_SPARE_CODE: u8 = 7;
const UWOP_SAVE_XMM128: u8 = 8;
const UWOP_SAVE_XMM128_FAR: u8 = 9;
const UWOP_PUSH_MACHFRAME: u8 = 10;

/// Table de fonctions enregistrée (image PE ou `RtlAddFunctionTable`).
#[derive(Debug, Clone, Copy)]
struct FunctionTable {
    image_base: usize,
    /// Plage d'adresses couverte `[start, end)`.
    start: usize,
    end: usize,
    table: usize,
    count: usize,
}

static FUNCTION_TABLES: RwLock<Vec<FunctionTable>> = RwLock::new(Vec::new());

/// Enregistre la table `.pdata` d'une image mappée couvrant `[image_base, image_base + image_size)`.
///
/// # Safety
/// `table` doit pointer vers `count` entrées `RUNTIME_FUNCTION` triées par `BeginAddress`,
/// valides tant que la table reste enregistrée.
pub unsafe fn register_function_table(
    image_base: usize,
    image_size: usize,
    table: *const RuntimeFunction,
    count: usize,
) {
    if table.is_null() || count == 0 {
        return;
    }
    let mut tables = FUNCTION_TABLES.write();
    tables.retain(|t| t.table != table as usize);
    tables.push(FunctionTable {
        image_base,
        start: image_base,
        end: image_base.saturating_add(image_size),
        table: table as usize,
        count,
    });
}

/// Équivalent de `RtlAddFunctionTable` : la plage couverte est déduite des entrées.
///
/// # Safety
/// Voir [`register_function_table`].
pub unsafe fn add_dynamic_function_table(
    table: *const RuntimeFunction,
    count: usize,
    image_base: usize,
) -> bool {
    if table.is_null() || count == 0 {
        return false;
    }
    // SAFETY: contrat de l'appelant (`count` entrées lisibles).
    let entries = unsafe { std::slice::from_raw_parts(table, count) };
    let lo = entries.iter().map(|e| e.begin_address).min().unwrap_or(0) as usize;
    let hi = entries.iter().map(|e| e.end_address).max().unwrap_or(0) as usize;
    let mut tables = FUNCTION_TABLES.write();
    tables.retain(|t| t.table != table as usize);
    tables.push(FunctionTable {
        image_base,
        start: image_base.saturating_add(lo),
        end: image_base.saturating_add(hi),
        table: table as usize,
        count,
    });
    true
}

/// Équivalent de `RtlDeleteFunctionTable`.
pub fn remove_function_table(table: *const RuntimeFunction) -> bool {
    let mut tables = FUNCTION_TABLES.write();
    let before = tables.len();
    tables.retain(|t| t.table != table as usize);
    tables.len() != before
}

/// Recherche la `RUNTIME_FUNCTION` couvrant `pc`. Retourne `(image_base, entrée)`.
///
/// Une fonction feuille (sans entrée) retourne `None`, comme `RtlLookupFunctionEntry`.
pub fn lookup_function_entry(pc: usize) -> Option<(usize, *const RuntimeFunction)> {
    let tables = FUNCTION_TABLES.read();
    let t = tables.iter().find(|t| pc >= t.start && pc < t.end)?;
    let rva = pc.checked_sub(t.image_base)?;
    let rva = u32::try_from(rva).ok()?;
    // SAFETY: contrat de `register_function_table` : `count` entrées triées lisibles.
    let entries = unsafe { std::slice::from_raw_parts(t.table as *const RuntimeFunction, t.count) };
    let idx = entries.partition_point(|e| e.begin_address <= rva);
    if idx == 0 {
        return None;
    }
    let e = &entries[idx - 1];
    if rva < e.end_address {
        Some((t.image_base, ptr::from_ref(e)))
    } else {
        None
    }
}

/// Remonte la pile invitée à partir de `ctx` (état à l'instant d'une faute) et retourne le PC de
/// chaque trame, la première étant `ctx.rip`, jusqu'à `max` trames.
///
/// S'arrête dès qu'une adresse n'appartient plus à une image PE enregistrée (trame hôte, trampoline
/// JIT, pile corrompue) ou que la pile ne progresse plus. Une fonction sans entrée `.pdata` dans
/// une image connue est traitée comme une feuille (adresse de retour au sommet de pile), comme
/// `RtlVirtualUnwind` le fait pour le code sans métadonnées.
///
/// # Safety
/// La pile décrite par `ctx.rsp` et le code des images enregistrées doivent être lisibles.
pub unsafe fn guest_backtrace(mut ctx: Context, max: usize) -> Vec<usize> {
    let mut frames = Vec::new();
    while frames.len() < max {
        let pc = ctx.rip as usize;
        if pc == 0 || image_base_for_pc(pc).is_none() {
            break;
        }
        frames.push(pc);
        // Pour les trames appelantes, `pc` est une adresse de retour : on cherche l'instruction
        // d'appel (`pc - 1`) pour ne pas tomber dans la fonction suivante.
        let lookup_pc = if frames.len() == 1 { pc } else { pc - 1 };
        let prev_rsp = ctx.rsp;
        match lookup_function_entry(lookup_pc) {
            Some((image_base, func)) => {
                // SAFETY: contrat de l'appelant ; `func` vient de la table de l'image.
                if unsafe { virtual_unwind(0, image_base, lookup_pc, func, &mut ctx, None) }
                    .is_none()
                {
                    break;
                }
            }
            None => {
                if ctx.rsp == 0 {
                    break;
                }
                // SAFETY: contrat de l'appelant (sommet de pile lisible).
                ctx.rip = unsafe { rd_u64(ctx.rsp) };
                ctx.rsp += 8;
            }
        }
        if ctx.rsp <= prev_rsp {
            break;
        }
    }
    frames
}

/// Base de l'image dont une table couvre `pc` (utile pour `RtlPcToFileHeader`).
pub fn image_base_for_pc(pc: usize) -> Option<usize> {
    FUNCTION_TABLES
        .read()
        .iter()
        .find(|t| pc >= t.start && pc < t.end)
        .map(|t| t.image_base)
}

/// Résultat de [`virtual_unwind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnwindResult {
    /// Adresse du handler de langage (`EXCEPTION_ROUTINE`), si demandé et présent.
    pub handler: Option<usize>,
    /// Pointeur vers les données spécifiques du handler (`HandlerData`).
    pub handler_data: usize,
    /// Cadre d'établissement (`EstablisherFrame`).
    pub establisher_frame: u64,
}

#[inline]
unsafe fn rd_u16(addr: usize) -> u16 {
    // SAFETY: contrat de l'appelant (adresse lisible).
    unsafe { ptr::read_unaligned(addr as *const u16) }
}

#[inline]
unsafe fn rd_u32(addr: usize) -> u32 {
    // SAFETY: contrat de l'appelant (adresse lisible).
    unsafe { ptr::read_unaligned(addr as *const u32) }
}

#[inline]
unsafe fn rd_u64(addr: u64) -> u64 {
    // SAFETY: contrat de l'appelant (adresse lisible).
    unsafe { ptr::read_unaligned(addr as usize as *const u64) }
}

#[inline]
unsafe fn rd_u8(addr: usize) -> u8 {
    // SAFETY: contrat de l'appelant (adresse lisible).
    unsafe { ptr::read(addr as *const u8) }
}

fn restore_gpr(
    ctx: &mut Context,
    ptrs: Option<&mut KNonvolatileContextPointers>,
    reg: u8,
    slot: u64,
) {
    // SAFETY: `slot` est une adresse de pile de l'invité décrite par l'UNWIND_INFO.
    *ctx.gpr_mut(reg) = unsafe { rd_u64(slot) };
    if let Some(p) = ptrs {
        p.integer_context[usize::from(reg & 15)] = slot as usize as *mut u64;
    }
}

fn restore_xmm(
    ctx: &mut Context,
    ptrs: Option<&mut KNonvolatileContextPointers>,
    reg: u8,
    slot: u64,
) {
    // SAFETY: `slot` est une adresse de pile de l'invité décrite par l'UNWIND_INFO.
    let v = unsafe {
        M128A {
            low: rd_u64(slot),
            high: rd_u64(slot + 8),
        }
    };
    ctx.flt_save.xmm_registers[usize::from(reg & 15)] = v;
    if let Some(p) = ptrs {
        p.floating_context[usize::from(reg & 15)] = slot as usize as *mut M128A;
    }
}

/// Nombre de slots `UNWIND_CODE` occupés par un opcode.
const fn opcode_slots(op: u8, info: u8) -> usize {
    match op {
        UWOP_ALLOC_LARGE => {
            if info == 0 {
                2
            } else {
                3
            }
        }
        UWOP_SAVE_NONVOL | UWOP_SAVE_XMM128 | UWOP_EPILOG => 2,
        UWOP_SAVE_NONVOL_FAR | UWOP_SAVE_XMM128_FAR | UWOP_SPARE_CODE => 3,
        _ => 1,
    }
}

/// Détecte si `pc` se trouve dans un épilogue standard x64
/// (`add/lea rsp` optionnel, suite de `pop`, puis `ret` ou `jmp` intra-fonction).
///
/// # Safety
/// Le code machine à partir de `pc` doit être lisible.
unsafe fn is_inside_epilog(mut pc: usize, image_base: usize, func: &RuntimeFunction) -> bool {
    // SAFETY: lectures d'octets de code (contrat de l'appelant).
    unsafe {
        let b0 = rd_u8(pc);
        if (b0 & 0xF8) == 0x48 {
            let b1 = rd_u8(pc + 1);
            let b2 = rd_u8(pc + 2);
            match b1 {
                0x81 => {
                    if b0 == 0x48 && b2 == 0xC4 {
                        pc += 7;
                    } else {
                        return false;
                    }
                }
                0x83 => {
                    if b0 == 0x48 && b2 == 0xC4 {
                        pc += 4;
                    } else {
                        return false;
                    }
                }
                0x8D => {
                    if (b0 & 0x06) != 0 || ((b2 >> 3) & 7) != 4 || (b2 & 7) == 4 {
                        return false;
                    }
                    match b2 >> 6 {
                        1 => pc += 4,
                        2 => pc += 7,
                        _ => return false,
                    }
                }
                _ => {}
            }
        }
        let in_func = |p: usize| {
            let rva = p.wrapping_sub(image_base);
            rva >= func.begin_address as usize && rva < func.end_address as usize
        };
        for _ in 0..64 {
            if (rd_u8(pc) & 0xF0) == 0x40 {
                pc += 1;
            }
            match rd_u8(pc) {
                0x58..=0x5F => pc += 1,
                0xC2 | 0xC3 => return true,
                0xE9 => {
                    let rel = rd_u32(pc + 1) as i32 as isize;
                    pc = pc.wrapping_add(5).wrapping_add_signed(rel);
                    if !in_func(pc) {
                        return false;
                    }
                }
                0xEB => {
                    let rel = rd_u8(pc + 1) as i8 as isize;
                    pc = pc.wrapping_add(2).wrapping_add_signed(rel);
                    if !in_func(pc) {
                        return false;
                    }
                }
                0xF3 => return rd_u8(pc + 1) == 0xC3,
                _ => return false,
            }
        }
        false
    }
}

/// Simule l'exécution de l'épilogue à partir de `pc` sur `ctx`.
///
/// # Safety
/// Code et pile de l'invité lisibles.
unsafe fn interpret_epilog(
    mut pc: usize,
    ctx: &mut Context,
    mut ptrs: Option<&mut KNonvolatileContextPointers>,
) {
    // SAFETY: contrat de l'appelant ; `is_inside_epilog` a validé la séquence.
    unsafe {
        for _ in 0..64 {
            let mut rex = 0u8;
            if (rd_u8(pc) & 0xF0) == 0x40 {
                rex = rd_u8(pc) & 0x0F;
                pc += 1;
            }
            match rd_u8(pc) {
                op @ 0x58..=0x5F => {
                    let reg = op - 0x58 + (rex & 1) * 8;
                    let slot = ctx.rsp;
                    restore_gpr(ctx, ptrs.as_deref_mut(), reg, slot);
                    ctx.rsp = ctx.rsp.wrapping_add(8);
                    pc += 1;
                }
                0x81 => {
                    let imm = rd_u32(pc + 2) as i32 as i64;
                    ctx.rsp = ctx.rsp.wrapping_add_signed(imm);
                    pc += 6;
                }
                0x83 => {
                    let imm = i64::from(rd_u8(pc + 2) as i8);
                    ctx.rsp = ctx.rsp.wrapping_add_signed(imm);
                    pc += 3;
                }
                0x8D => {
                    let modrm = rd_u8(pc + 1);
                    let base = ctx.gpr((modrm & 7) + (rex & 1) * 8);
                    if (modrm >> 6) == 1 {
                        ctx.rsp = base.wrapping_add_signed(i64::from(rd_u8(pc + 2) as i8));
                        pc += 3;
                    } else {
                        ctx.rsp = base.wrapping_add_signed(i64::from(rd_u32(pc + 2) as i32));
                        pc += 6;
                    }
                }
                0xC2 => {
                    ctx.rip = rd_u64(ctx.rsp);
                    ctx.rsp = ctx
                        .rsp
                        .wrapping_add(8)
                        .wrapping_add(u64::from(rd_u16(pc + 1)));
                    return;
                }
                0xC3 | 0xF3 => {
                    ctx.rip = rd_u64(ctx.rsp);
                    ctx.rsp = ctx.rsp.wrapping_add(8);
                    return;
                }
                0xE9 => {
                    let rel = rd_u32(pc + 1) as i32 as isize;
                    pc = pc.wrapping_add(5).wrapping_add_signed(rel);
                }
                0xEB => {
                    let rel = rd_u8(pc + 1) as i8 as isize;
                    pc = pc.wrapping_add(2).wrapping_add_signed(rel);
                }
                _ => return,
            }
        }
    }
}

/// Équivalent de `RtlVirtualUnwind` : déroule une trame de `ctx` (qui contient l'état à `control_pc`).
///
/// Retourne `None` si l'`UNWIND_INFO` est d'une version non supportée.
///
/// # Safety
/// `func` doit pointer vers une `RUNTIME_FUNCTION` valide de l'image `image_base` ; la pile
/// décrite par `ctx` et le code de la fonction doivent être lisibles.
pub unsafe fn virtual_unwind(
    handler_type: u32,
    image_base: usize,
    control_pc: usize,
    func: *const RuntimeFunction,
    ctx: &mut Context,
    mut ptrs: Option<&mut KNonvolatileContextPointers>,
) -> Option<UnwindResult> {
    if func.is_null() {
        return None;
    }
    // SAFETY: contrat de l'appelant.
    let mut function = unsafe { *func };
    let mut frame = ctx.rsp;
    let mut establisher = frame;
    let mut mach_frame = false;
    let mut last_flags;
    let mut last_info;
    let mut prolog_offset: Option<usize>;

    // Limite défensive sur la profondeur de chaînage.
    let mut depth = 0;
    loop {
        depth += 1;
        if depth > 32 {
            return None;
        }
        let info = image_base + function.unwind_data as usize;
        // SAFETY: `UNWIND_INFO` dans l'image (contrat de l'appelant).
        let (ver_flags, prolog_size, count, frame_byte) = unsafe {
            (
                rd_u8(info),
                rd_u8(info + 1),
                rd_u8(info + 2),
                rd_u8(info + 3),
            )
        };
        let version = ver_flags & 7;
        let flags = u32::from(ver_flags >> 3);
        if version != 1 && version != 2 {
            return None;
        }
        let frame_reg = frame_byte & 0x0F;
        let frame_off = u64::from(frame_byte >> 4) * 16;
        let codes = info + 4;
        let count = usize::from(count);
        last_flags = flags;
        last_info = info;

        if frame_reg != 0 {
            frame = ctx.gpr(frame_reg).wrapping_sub(frame_off);
        }

        let begin = image_base + function.begin_address as usize;
        if control_pc >= begin && control_pc < begin + usize::from(prolog_size) {
            prolog_offset = Some(control_pc - begin);
        } else {
            prolog_offset = None;
            // SAFETY: code de la fonction lisible (contrat de l'appelant).
            if count != 0 && unsafe { is_inside_epilog(control_pc, image_base, &function) } {
                // SAFETY: idem.
                unsafe { interpret_epilog(control_pc, ctx, ptrs.as_deref_mut()) };
                return Some(UnwindResult {
                    handler: None,
                    handler_data: 0,
                    establisher_frame: frame,
                });
            }
        }

        let mut i = 0usize;
        while i < count {
            let slot = codes + i * 2;
            // SAFETY: slots `UNWIND_CODE` de l'UNWIND_INFO.
            let (code_offset, op_byte) = unsafe { (rd_u8(slot), rd_u8(slot + 1)) };
            let op = op_byte & 0x0F;
            let op_info = op_byte >> 4;
            let size = opcode_slots(op, op_info);
            if let Some(po) = prolog_offset
                && po < usize::from(code_offset)
            {
                i += size;
                continue;
            }
            // SAFETY: lectures des slots suivants de l'opcode courant (bornés par `count`).
            let next_u16 = || unsafe { u64::from(rd_u16(slot + 2)) };
            let next_u32 = || unsafe { u64::from(rd_u32(slot + 2)) };
            match op {
                UWOP_PUSH_NONVOL => {
                    let at = ctx.rsp;
                    restore_gpr(ctx, ptrs.as_deref_mut(), op_info, at);
                    ctx.rsp = ctx.rsp.wrapping_add(8);
                }
                UWOP_ALLOC_LARGE => {
                    let amount = if op_info == 0 {
                        next_u16() * 8
                    } else {
                        next_u32()
                    };
                    ctx.rsp = ctx.rsp.wrapping_add(amount);
                }
                UWOP_ALLOC_SMALL => {
                    ctx.rsp = ctx.rsp.wrapping_add(u64::from(op_info) * 8 + 8);
                }
                UWOP_SET_FPREG => {
                    ctx.rsp = frame;
                    establisher = frame;
                }
                UWOP_SAVE_NONVOL => {
                    let at = frame.wrapping_add(next_u16() * 8);
                    restore_gpr(ctx, ptrs.as_deref_mut(), op_info, at);
                }
                UWOP_SAVE_NONVOL_FAR => {
                    let at = frame.wrapping_add(next_u32());
                    restore_gpr(ctx, ptrs.as_deref_mut(), op_info, at);
                }
                UWOP_SAVE_XMM128 => {
                    let at = frame.wrapping_add(next_u16() * 16);
                    restore_xmm(ctx, ptrs.as_deref_mut(), op_info, at);
                }
                UWOP_SAVE_XMM128_FAR => {
                    let at = frame.wrapping_add(next_u32());
                    restore_xmm(ctx, ptrs.as_deref_mut(), op_info, at);
                }
                UWOP_PUSH_MACHFRAME if (flags & UNW_FLAG_CHAININFO) == 0 && i + size >= count => {
                    if op_info != 0 {
                        ctx.rsp = ctx.rsp.wrapping_add(8);
                    }
                    // SAFETY: trame machine (RIP, CS, EFLAGS, RSP, SS) sur la pile invitée.
                    unsafe {
                        ctx.rip = rd_u64(ctx.rsp);
                        ctx.rsp = rd_u64(ctx.rsp.wrapping_add(24));
                    }
                    mach_frame = true;
                }
                _ => {} // UWOP_EPILOG (v2) / codes réservés : rien à faire.
            }
            i += size;
        }

        if (flags & UNW_FLAG_CHAININFO) == 0 {
            break;
        }
        // RUNTIME_FUNCTION chaînée après les codes (nombre aligné sur 2).
        let chain = codes + ((count + 1) & !1) * 2;
        // SAFETY: entrée chaînée dans l'UNWIND_INFO.
        function = unsafe { ptr::read_unaligned(chain as *const RuntimeFunction) };
    }

    if !mach_frame {
        // SAFETY: adresse de retour au sommet de la pile déroulée.
        ctx.rip = unsafe { rd_u64(ctx.rsp) };
        ctx.rsp = ctx.rsp.wrapping_add(8);
    }

    let establisher_frame = establisher;
    if (last_flags & handler_type) == 0 || prolog_offset.is_some() {
        return Some(UnwindResult {
            handler: None,
            handler_data: 0,
            establisher_frame,
        });
    }
    // SAFETY: relecture du compteur de codes pour localiser le handler.
    let count = usize::from(unsafe { rd_u8(last_info + 2) });
    let handler_slot = last_info + 4 + ((count + 1) & !1) * 2;
    // SAFETY: RVA du handler suivie des données du handler.
    let handler_rva = unsafe { rd_u32(handler_slot) } as usize;
    Some(UnwindResult {
        handler: Some(image_base + handler_rva),
        handler_data: handler_slot + 4,
        establisher_frame,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Image synthétique : code à 0x100, UNWIND_INFO à 0x200, .pdata à 0x300.
    struct Image {
        mem: Vec<u8>,
    }

    impl Image {
        fn new() -> Self {
            Self {
                mem: vec![0xCC; 0x1000],
            }
        }
        fn base(&self) -> usize {
            self.mem.as_ptr() as usize
        }
        fn put(&mut self, off: usize, bytes: &[u8]) {
            self.mem[off..off + bytes.len()].copy_from_slice(bytes);
        }
        fn put_fn(&mut self, off: usize, f: RuntimeFunction) {
            self.put(off, &f.begin_address.to_le_bytes());
            self.put(off + 4, &f.end_address.to_le_bytes());
            self.put(off + 8, &f.unwind_data.to_le_bytes());
        }
    }

    // Prologue : push rbp ; push rbx ; sub rsp, 0x28 ; (corps) ; épilogue :
    // add rsp, 0x28 ; pop rbx ; pop rbp ; ret
    const CODE: &[u8] = &[
        0x55, // 0x00 push rbp
        0x53, // 0x01 push rbx
        0x48, 0x83, 0xEC, 0x28, // 0x02 sub rsp,0x28
        0x90, 0x90, // 0x06 corps
        0x48, 0x83, 0xC4, 0x28, // 0x08 add rsp,0x28
        0x5B, // 0x0C pop rbx
        0x5D, // 0x0D pop rbp
        0xC3, // 0x0E ret
    ];

    fn build_simple(flags: u8) -> Image {
        let mut img = Image::new();
        img.put(0x100, CODE);
        // UNWIND_INFO v1 : prolog=6, 3 codes, pas de frame reg.
        img.put(
            0x200,
            &[
                1 | (flags << 3),
                6,
                3,
                0,
                0x06,
                (UWOP_ALLOC_SMALL) | (4 << 4), // 0x28 = 4*8+8
                0x02,
                UWOP_PUSH_NONVOL | (3 << 4), // rbx
                0x01,
                UWOP_PUSH_NONVOL | (5 << 4), // rbp
                0,
                0, // padding (nombre impair)
            ],
        );
        if flags != 0 {
            img.put(0x20C, &0x180u32.to_le_bytes()); // handler RVA
            img.put(0x210, &0xDEAD_BEEFu32.to_le_bytes()); // handler data
        }
        img.put_fn(
            0x300,
            RuntimeFunction {
                begin_address: 0x100,
                end_address: 0x100 + CODE.len() as u32,
                unwind_data: 0x200,
            },
        );
        img
    }

    fn stack_with(values: &[(usize, u64)]) -> Vec<u64> {
        let mut s = vec![0u64; 64];
        for &(i, v) in values {
            s[i] = v;
        }
        s
    }

    #[test]
    fn lookup_and_unwind_body() {
        let img = build_simple(UNW_FLAG_EHANDLER as u8);
        let base = img.base();
        // SAFETY: la table synthétique vit pendant tout le test.
        unsafe {
            register_function_table(
                base,
                img.mem.len(),
                (base + 0x300) as *const RuntimeFunction,
                1,
            );
        }
        // Pile dans le corps : rsp -> [0x28 locaux][rbx][rbp][ret]
        let stack = stack_with(&[(5, 0x1111), (6, 0x2222), (7, 0x4000_1234)]);
        let rsp = stack.as_ptr() as u64;
        let pc = base + 0x106;
        let (ib, f) = lookup_function_entry(pc).expect("entrée");
        assert_eq!(ib, base);
        assert!(lookup_function_entry(base + 0x80).is_none());
        let mut ctx = Context {
            rsp,
            rip: pc as u64,
            ..Context::default()
        };
        let mut ptrs = KNonvolatileContextPointers {
            floating_context: [ptr::null_mut(); 16],
            integer_context: [ptr::null_mut(); 16],
        };
        // SAFETY: image et pile synthétiques valides.
        let r = unsafe { virtual_unwind(UNW_FLAG_EHANDLER, ib, pc, f, &mut ctx, Some(&mut ptrs)) }
            .expect("unwind");
        assert_eq!(ctx.rbx, 0x1111);
        assert_eq!(ctx.rbp, 0x2222);
        assert_eq!(ctx.rip, 0x4000_1234);
        assert_eq!(ctx.rsp, rsp + 64);
        assert_eq!(r.establisher_frame, rsp);
        assert_eq!(r.handler, Some(base + 0x180));
        // SAFETY: `handler_data` pointe dans l'image synthétique.
        assert_eq!(unsafe { rd_u32(r.handler_data) }, 0xDEAD_BEEF);
        assert_eq!(ptrs.integer_context[3] as u64, rsp + 40);
        assert!(remove_function_table(
            (base + 0x300) as *const RuntimeFunction
        ));
        assert!(lookup_function_entry(pc).is_none());
    }

    #[test]
    fn unwind_in_prolog_and_epilog() {
        let img = build_simple(UNW_FLAG_EHANDLER as u8);
        let base = img.base();
        let f = (base + 0x300) as *const RuntimeFunction;

        // Dans le prologue après `push rbp` (offset 1) : seul rbp est sur la pile.
        let stack = stack_with(&[(0, 0x2222), (1, 0x5555)]);
        let rsp = stack.as_ptr() as u64;
        let mut ctx = Context {
            rsp,
            ..Context::default()
        };
        // SAFETY: image et pile synthétiques valides.
        let r = unsafe { virtual_unwind(UNW_FLAG_EHANDLER, base, base + 0x101, f, &mut ctx, None) }
            .expect("unwind");
        assert_eq!(ctx.rbp, 0x2222);
        assert_eq!(ctx.rip, 0x5555);
        assert_eq!(ctx.rsp, rsp + 16);
        assert_eq!(r.handler, None, "pas de handler dans le prologue");

        // Dans l'épilogue après `add rsp` (offset 0x0C) : [rbx][rbp][ret].
        let stack = stack_with(&[(0, 0x1111), (1, 0x2222), (2, 0x7777)]);
        let rsp = stack.as_ptr() as u64;
        let mut ctx = Context {
            rsp,
            ..Context::default()
        };
        // SAFETY: idem.
        let r = unsafe { virtual_unwind(UNW_FLAG_EHANDLER, base, base + 0x10C, f, &mut ctx, None) }
            .expect("unwind");
        assert_eq!((ctx.rbx, ctx.rbp, ctx.rip), (0x1111, 0x2222, 0x7777));
        assert_eq!(ctx.rsp, rsp + 24);
        assert_eq!(r.handler, None);

        // Sur `add rsp, 0x28` (offset 0x08).
        let stack = stack_with(&[(5, 0x1111), (6, 0x2222), (7, 0x8888)]);
        let rsp = stack.as_ptr() as u64;
        let mut ctx = Context {
            rsp,
            ..Context::default()
        };
        // SAFETY: idem.
        unsafe { virtual_unwind(UNW_FLAG_EHANDLER, base, base + 0x108, f, &mut ctx, None) }
            .expect("unwind");
        assert_eq!((ctx.rbx, ctx.rbp, ctx.rip), (0x1111, 0x2222, 0x8888));
        assert_eq!(ctx.rsp, rsp + 64);
    }

    #[test]
    fn unwind_fpreg_save_nonvol_xmm_and_chain() {
        let mut img = Image::new();
        let base = img.base();
        // Fonction principale (0x100..0x140) : push rbp ; sub rsp,0x40 ; lea rbp,[rsp+0x20] ;
        // mov [rsp+0x30],rsi ; movaps [rsp+0x10],xmm6. Codes décrits seulement (le corps n'est
        // pas exécuté), pas d'épilogue au pc testé.
        img.put(0x100, &[0x90; 0x40]);
        img.put(
            0x200,
            &[
                1,
                0x14,
                7,
                5 | (2 << 4), // frame reg = rbp, offset = 2*16
                0x14,
                UWOP_SAVE_XMM128 | (6 << 4),
                1,
                0, // offset 1*16 = 0x10
                0x10,
                UWOP_SAVE_NONVOL | (6 << 4),
                6,
                0, // offset 6*8 = 0x30
                0x0C,
                UWOP_SET_FPREG,
                0x08,
                UWOP_ALLOC_SMALL | (7 << 4), // 0x40
                0x01,
                UWOP_PUSH_NONVOL | (5 << 4),
                0,
                0,
            ],
        );
        img.put_fn(
            0x300,
            RuntimeFunction {
                begin_address: 0x100,
                end_address: 0x120,
                unwind_data: 0x200,
            },
        );
        // Fragment chaîné (0x120..0x140) : aucun code propre, chaîne vers la principale.
        img.put(0x240, &[1 | ((UNW_FLAG_CHAININFO as u8) << 3), 0, 0, 0]);
        img.put_fn(
            0x244,
            RuntimeFunction {
                begin_address: 0x100,
                end_address: 0x120,
                unwind_data: 0x200,
            },
        );
        img.put_fn(
            0x30C,
            RuntimeFunction {
                begin_address: 0x120,
                end_address: 0x140,
                unwind_data: 0x240,
            },
        );

        // Pile du corps : rsp=S ; [S+0x10]=xmm6 ; [S+0x30]=rsi ; [S+0x40]=rbp ; [S+0x48]=ret.
        let mut stack = vec![0u64; 32];
        stack[2] = 0xAAAA;
        stack[3] = 0xBBBB;
        stack[6] = 0x6666;
        stack[8] = 0x2222;
        stack[9] = 0x9999;
        let s = stack.as_ptr() as u64;
        for (pc_off, f_off) in [(0x118usize, 0x300usize), (0x128, 0x30C)] {
            let mut ctx = Context {
                rsp: s,
                rbp: s + 0x20,
                ..Context::default()
            };
            let f = (base + f_off) as *const RuntimeFunction;
            // SAFETY: image et pile synthétiques valides.
            let r = unsafe { virtual_unwind(0, base, base + pc_off, f, &mut ctx, None) }
                .expect("unwind");
            assert_eq!(ctx.rsi, 0x6666);
            assert_eq!(ctx.rbp, 0x2222);
            assert_eq!(ctx.rip, 0x9999);
            assert_eq!(ctx.rsp, s + 0x50);
            assert_eq!(ctx.flt_save.xmm_registers[6].low, 0xAAAA);
            assert_eq!(ctx.flt_save.xmm_registers[6].high, 0xBBBB);
            assert_eq!(r.establisher_frame, s);
            assert_eq!(r.handler, None);
        }
    }

    #[test]
    fn unwind_machframe() {
        let mut img = Image::new();
        let base = img.base();
        img.put(
            0x200,
            &[1, 0, 1, 0, 0, UWOP_PUSH_MACHFRAME | (1 << 4), 0, 0],
        );
        img.put_fn(
            0x300,
            RuntimeFunction {
                begin_address: 0x100,
                end_address: 0x110,
                unwind_data: 0x200,
            },
        );
        img.put(0x100, &[0x90; 0x10]);
        // [err][rip][cs][eflags][rsp][ss]
        let stack = stack_with(&[(0, 0xE), (1, 0x1234), (4, 0xABCD_0000)]);
        let mut ctx = Context {
            rsp: stack.as_ptr() as u64,
            ..Context::default()
        };
        let f = (base + 0x300) as *const RuntimeFunction;
        // SAFETY: image et pile synthétiques valides.
        unsafe { virtual_unwind(0, base, base + 0x104, f, &mut ctx, None) }.expect("unwind");
        assert_eq!(ctx.rip, 0x1234);
        assert_eq!(ctx.rsp, 0xABCD_0000);
    }

    #[test]
    fn guest_backtrace_walks_frames_and_stops_outside_images() {
        let img = build_simple(0);
        let base = img.base();
        // SAFETY: la table synthétique vit pendant tout le test.
        unsafe {
            register_function_table(
                base,
                img.mem.len(),
                (base + 0x300) as *const RuntimeFunction,
                1,
            );
        }
        // Trame 1 (corps, rsp = pile[0]) : retour pile[7] -> base+0x107 (même fonction).
        // Trame 2 : rsp = pile[8], retour pile[15] = 0 -> fin.
        let stack = stack_with(&[(7, (base + 0x107) as u64), (15, 0)]);
        let ctx = Context {
            rsp: stack.as_ptr() as u64,
            rip: (base + 0x106) as u64,
            ..Context::default()
        };
        // SAFETY: image et pile synthétiques valides.
        let frames = unsafe { guest_backtrace(ctx, 16) };
        assert_eq!(frames, vec![base + 0x106, base + 0x107]);
        // La profondeur est bornée.
        let ctx = Context {
            rsp: stack.as_ptr() as u64,
            rip: (base + 0x106) as u64,
            ..Context::default()
        };
        // SAFETY: idem.
        assert_eq!(unsafe { guest_backtrace(ctx, 1) }.len(), 1);
        // Hors image : aucune trame.
        let ctx = Context {
            rsp: stack.as_ptr() as u64,
            rip: 0x1234,
            ..Context::default()
        };
        // SAFETY: idem.
        assert!(unsafe { guest_backtrace(ctx, 16) }.is_empty());
        assert!(remove_function_table(
            (base + 0x300) as *const RuntimeFunction
        ));
    }
}
