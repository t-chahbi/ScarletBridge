//! Support RTL x64 (ntdll / api-ms-win-core-rtlsupport) : `RtlCaptureContext`,
//! `RtlLookupFunctionEntry`, `RtlVirtualUnwind`, `RtlPcToFileHeader`,
//! `RtlAddFunctionTable` / `RtlDeleteFunctionTable`.
//!
//! `RtlUnwind` / `RtlUnwindEx` ne sont pas fournis : un déroulage réel nécessiterait la
//! répartition SEH complète (handlers de langage, `__C_specific_handler`, consolidation).

use core::ffi::c_void;

use sb_common::ffi_guard;
use sb_loader::module_manager;
use sb_loader::unwind::{
    Context, KNonvolatileContextPointers, RuntimeFunction, add_dynamic_function_table,
    image_base_for_pc, lookup_function_entry, remove_function_table, virtual_unwind,
};

use super::{CHANNEL, write_out};

/// `CONTEXT_AMD64 | CONTROL | INTEGER | SEGMENTS | FLOATING_POINT` (valeur posée par Windows).
pub(crate) const CONTEXT_CAPTURE_FLAGS: u32 = 0x0010_000F;

/// `RtlCaptureContext` : capture l'état de l'appelant (RIP = adresse de retour, RSP = pile
/// de l'appelant après retour), registres entiers, segments, EFLAGS, `fxsave` et MXCSR.
#[unsafe(naked)]
pub(crate) extern "win64" fn RtlCaptureContext(ctx: *mut Context) {
    core::arch::naked_asm!(
        "pushfq",
        "mov [rcx + 0x78], rax",
        "mov [rcx + 0x80], rcx",
        "mov [rcx + 0x88], rdx",
        "mov [rcx + 0x90], rbx",
        "lea rax, [rsp + 0x10]",
        "mov [rcx + 0x98], rax",
        "mov [rcx + 0xA0], rbp",
        "mov [rcx + 0xA8], rsi",
        "mov [rcx + 0xB0], rdi",
        "mov [rcx + 0xB8], r8",
        "mov [rcx + 0xC0], r9",
        "mov [rcx + 0xC8], r10",
        "mov [rcx + 0xD0], r11",
        "mov [rcx + 0xD8], r12",
        "mov [rcx + 0xE0], r13",
        "mov [rcx + 0xE8], r14",
        "mov [rcx + 0xF0], r15",
        "mov word ptr [rcx + 0x38], cs",
        "mov word ptr [rcx + 0x3A], ds",
        "mov word ptr [rcx + 0x3C], es",
        "mov word ptr [rcx + 0x3E], fs",
        "mov word ptr [rcx + 0x40], gs",
        "mov word ptr [rcx + 0x42], ss",
        "fxsave64 [rcx + 0x100]",
        "stmxcsr [rcx + 0x34]",
        "mov rax, [rsp + 8]",
        "mov [rcx + 0xF8], rax",
        "mov eax, [rsp]",
        "mov [rcx + 0x44], eax",
        "mov dword ptr [rcx + 0x30], {flags}",
        "mov rax, [rcx + 0x78]",
        "popfq",
        "ret",
        flags = const CONTEXT_CAPTURE_FLAGS,
    )
}

pub(super) extern "win64" fn RtlLookupFunctionEntry(
    pc: u64,
    image_base: *mut u64,
    _history: *mut c_void,
) -> *const RuntimeFunction {
    ffi_guard!(CHANNEL, "RtlLookupFunctionEntry", core::ptr::null(), {
        match lookup_function_entry(pc as usize) {
            Some((base, f)) => {
                write_out(image_base, base as u64);
                f
            }
            None => {
                // Fonction feuille ou adresse hors image : base renseignée si connue.
                if let Some(b) = image_base_for_pc(pc as usize) {
                    write_out(image_base, b as u64);
                }
                core::ptr::null()
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) extern "win64" fn RtlVirtualUnwind(
    handler_type: u32,
    image_base: u64,
    control_pc: u64,
    function: *const RuntimeFunction,
    ctx: *mut Context,
    handler_data: *mut usize,
    establisher_frame: *mut u64,
    ctx_ptrs: *mut KNonvolatileContextPointers,
) -> usize {
    ffi_guard!(CHANNEL, "RtlVirtualUnwind", 0, {
        if ctx.is_null() || function.is_null() {
            return 0;
        }
        // SAFETY: `ctx` et `ctx_ptrs` sont fournis par l'appelant (CONTEXT / pointeurs valides).
        let (c, p) = unsafe { (&mut *ctx, ctx_ptrs.as_mut()) };
        // SAFETY: `function` provient de `RtlLookupFunctionEntry` pour `image_base` ; la pile
        // de `ctx` appartient à l'invité.
        let r = unsafe {
            virtual_unwind(
                handler_type,
                image_base as usize,
                control_pc as usize,
                function,
                c,
                p,
            )
        };
        match r {
            Some(r) => {
                write_out(establisher_frame, r.establisher_frame);
                write_out(handler_data, r.handler_data);
                r.handler.unwrap_or(0)
            }
            None => 0,
        }
    })
}

pub(super) extern "win64" fn RtlPcToFileHeader(pc: usize, base_out: *mut usize) -> usize {
    ffi_guard!(CHANNEL, "RtlPcToFileHeader", 0, {
        let base = module_manager()
            .find_by_address(pc)
            .map(|m| m.base)
            .or_else(|| image_base_for_pc(pc))
            .unwrap_or(0);
        write_out(base_out, base);
        base
    })
}

pub(super) extern "win64" fn RtlAddFunctionTable(
    table: *const RuntimeFunction,
    count: u32,
    base: u64,
) -> u8 {
    ffi_guard!(CHANNEL, "RtlAddFunctionTable", 0, {
        // SAFETY: l'invité garantit `count` entrées valides tant que la table est enregistrée.
        u8::from(unsafe { add_dynamic_function_table(table, count as usize, base as usize) })
    })
}

pub(super) extern "win64" fn RtlDeleteFunctionTable(table: *const RuntimeFunction) -> u8 {
    ffi_guard!(CHANNEL, "RtlDeleteFunctionTable", 0, {
        u8::from(remove_function_table(table))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_context_fills_caller_state() {
        let mut ctx = Context::default();
        let local = 0u64;
        RtlCaptureContext(&mut ctx);
        assert_eq!(ctx.context_flags, CONTEXT_CAPTURE_FLAGS);
        assert_ne!(ctx.rip, 0);
        let here = &raw const local as u64;
        assert!(
            ctx.rsp.abs_diff(here) < 0x10000,
            "rsp={:#x} local={here:#x}",
            ctx.rsp
        );
        assert_eq!(ctx.rsp % 8, 0);
        assert_eq!(ctx.mx_csr, ctx.flt_save.mx_csr);
        assert_ne!(ctx.mx_csr, 0);
        assert_ne!(ctx.seg_cs, 0);
        let addr = &raw const ctx as u64;
        assert_eq!(ctx.rcx, addr);
    }

    #[test]
    fn lookup_and_virtual_unwind_synthetic_image() {
        // Image : code à 0x100 (push rbx ; sub rsp,0x20 ; corps), UNWIND_INFO à 0x200,
        // .pdata à 0x300, handler à 0x180.
        let mut img = vec![0x90u8; 0x1000];
        img[0x100..0x105].copy_from_slice(&[0x53, 0x48, 0x83, 0xEC, 0x20]);
        img[0x200..0x214].copy_from_slice(&[
            1 | (1 << 3), // v1, UNW_FLAG_EHANDLER
            5,
            2,
            0,
            0x05,
            2 | (3 << 4), // ALLOC_SMALL 0x20
            0x01,
            3 << 4, // PUSH_NONVOL rbx
            0x80,
            0x01,
            0,
            0, // handler RVA 0x180
            0xAA,
            0xBB,
            0xCC,
            0xDD, // handler data
            0,
            0,
            0,
            0,
        ]);
        img[0x300..0x30C].copy_from_slice(
            &[0x100u32, 0x140, 0x200]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        );
        let base = img.as_ptr() as usize;
        let table = (base + 0x300) as *const RuntimeFunction;
        assert_eq!(RtlAddFunctionTable(table, 1, base as u64), 1);

        let pc = (base + 0x120) as u64;
        let mut ib = 0u64;
        let f = RtlLookupFunctionEntry(pc, &mut ib, core::ptr::null_mut());
        assert!(!f.is_null());
        assert_eq!(ib, base as u64);
        let mut pcb = 0usize;
        assert_eq!(RtlPcToFileHeader(pc as usize, &mut pcb), base);

        let mut stack = [0u64; 8];
        stack[4] = 0x1234_5678; // rbx sauvegardé
        stack[5] = 0xCAFE_0000; // adresse de retour
        let rsp = stack.as_ptr() as u64;
        let mut ctx = Context {
            rsp,
            rip: pc,
            ..Context::default()
        };
        let mut data = 0usize;
        let mut frame = 0u64;
        let handler = RtlVirtualUnwind(
            1,
            ib,
            pc,
            f,
            &mut ctx,
            &mut data,
            &mut frame,
            core::ptr::null_mut(),
        );
        assert_eq!(handler, base + 0x180);
        // SAFETY: `data` pointe dans l'image synthétique.
        assert_eq!(unsafe { *(data as *const u32) }, 0xDDCC_BBAA);
        assert_eq!(frame, rsp);
        assert_eq!(
            (ctx.rbx, ctx.rip, ctx.rsp),
            (0x1234_5678, 0xCAFE_0000, rsp + 48)
        );

        assert_eq!(RtlDeleteFunctionTable(table), 1);
        assert!(RtlLookupFunctionEntry(pc, &mut ib, core::ptr::null_mut()).is_null());
        assert_eq!(RtlPcToFileHeader(pc as usize, &mut pcb), 0);
        assert_eq!(pcb, 0);
    }
}
