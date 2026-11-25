//! FLAT / GLOBAL : accès mémoire à adresses 64 bits via `PhysicalStorageBuffer64`.
//!
//! Prérequis Vulkan : `bufferDeviceAddress` (Vulkan 1.2) ; capacités SPIR-V `Int64` et
//! `PhysicalStorageBufferAddresses`, extension `SPV_KHR_physical_storage_buffer`, modèle
//! d'adressage `PhysicalStorageBuffer64` (patché dans l'en-tête du module produit). Les adresses
//! GPU des buffers sont fournies au shader par les SGPR/VGPR (ex. `SgprInit::UserData`).
//! FLAT est traité comme GLOBAL (pas d'aperture LDS/scratch). SCRATCH n'est pas supporté.

use super::inst::{Encoding, Instruction, Operand, Src};
use super::translate::{TResult, Tr, TranslateError, unsupported};
use crate::dxbc_to_spirv::spv::{Sc, Val};

const OP_IADD: u16 = 128;
const OP_AND: u16 = 199;
const OP_OR: u16 = 197;
const OP_NOT: u16 = 200;
const OP_SHR: u16 = 194;
const OP_SHL: u16 = 196;
const OP_BFSEXT: u16 = 202;
const OP_BFUEXT: u16 = 203;

impl Tr<'_> {
    fn u64_ty(&mut self) -> u32 {
        self.spv.raw_type(21, &[64, 0])
    }

    /// Active les prérequis SPIR-V des pointeurs physiques.
    fn enable_psb(&mut self) {
        self.spv.caps.insert(11);
        self.spv.caps.insert(5347);
        self.spv.exts.insert("SPV_KHR_physical_storage_buffer");
        self.uses_psb = true;
    }

    fn make_u64(&mut self, lo: Val, hi: Val) -> u32 {
        let t = self.u64_ty();
        let l = self.spv.emit(113, t, &[lo.id]);
        let h = self.spv.emit(113, t, &[hi.id]);
        let c32 = self.spv.c_raw(t, &[32, 0]);
        let sh = self.spv.emit(OP_SHL, t, &[h, c32]);
        self.spv.emit(OP_OR, t, &[l, sh])
    }

    fn add_u64_imm(&mut self, a: u32, k: i64) -> u32 {
        let t = self.u64_ty();
        let c = self
            .spv
            .c_raw(t, &[k as u64 as u32, ((k as u64) >> 32) as u32]);
        self.spv.emit(OP_IADD, t, &[a, c])
    }

    fn psb_ptr(&mut self, addr: u32) -> u32 {
        let u = self.spv.ty(Sc::U32, 1);
        let pt = self.spv.ptr(5349, u);
        self.spv.emit(120, pt, &[addr])
    }

    fn psb_load(&mut self, ptr: u32) -> Val {
        let t = self.spv.ty(Sc::U32, 1);
        let id = self.spv.emit(61, t, &[ptr, 2, 4]);
        Val {
            id,
            sc: Sc::U32,
            n: 1,
        }
    }

    fn pair_regs(&mut self, o: &Operand) -> TResult<(Val, Val)> {
        self.src_pair(o)
    }

    pub(crate) fn flat(&mut self, i: &Instruction) -> TResult<()> {
        if i.encoding == Encoding::Scratch || i.mods.lds || i.name.ends_with("_addtid") {
            return unsupported(i);
        }
        let base = i
            .name
            .strip_prefix("global_")
            .or_else(|| i.name.strip_prefix("flat_"))
            .unwrap_or(i.name);
        let ops = &i.operands;
        let mut k = 0usize;
        let dst = if i.n_dst == 1 {
            k += 1;
            Some(ops[0])
        } else {
            None
        };
        let addr_op = ops[k];
        k += 1;
        let is_global = i.encoding == Encoding::Global;
        let has_data = base.starts_with("store") || base.starts_with("atomic");
        let data = if has_data {
            k += 1;
            Some(ops[k - 1])
        } else {
            None
        };
        let saddr = if is_global { ops.get(k).copied() } else { None };
        // --- adresse 64 bits
        self.enable_psb();
        let mut addr = match saddr {
            Some(s) if s.src != Src::Off => {
                let (sl, sh) = self.pair_regs(&Operand::new(s.src, 2))?;
                let b = self.make_u64(sl, sh);
                let off = self.src_u32(&addr_op)?;
                let t = self.u64_ty();
                let o64 = self.spv.emit(113, t, &[off.id]);
                self.spv.emit(OP_IADD, t, &[b, o64])
            }
            _ => {
                let (lo, hi) = self.pair_regs(&Operand::new(addr_op.src, 2))?;
                self.make_u64(lo, hi)
            }
        };
        addr = self.add_u64_imm(addr, i64::from(i.mods.offset));

        let name = base;
        let words = |n: &str| -> Option<usize> {
            match n {
                "dword" => Some(1),
                "dwordx2" => Some(2),
                "dwordx3" => Some(3),
                "dwordx4" => Some(4),
                _ => None,
            }
        };
        if let Some(rest) = name.strip_prefix("load_") {
            let d = dst.ok_or_else(|| TranslateError::Unsupported(i.name.into()))?;
            let Src::Vgpr(d0) = d.src else {
                return unsupported(i);
            };
            let d0 = usize::from(d0);
            if let Some(n) = words(rest) {
                return self.guarded(|t| {
                    for c in 0..n {
                        let a = t.add_u64_imm(addr, 4 * c as i64);
                        let p = t.psb_ptr(a);
                        let v = t.psb_load(p);
                        t.write_v(d0 + c, v);
                    }
                    Ok(())
                });
            }
            // sous-mots : ubyte/sbyte/ushort/sshort (+ _d16, _d16_hi)
            let (core, d16) = if let Some(c) = rest.strip_suffix("_d16_hi") {
                (c, 2)
            } else if let Some(c) = rest.strip_suffix("_d16") {
                (c, 1)
            } else {
                (rest, 0)
            };
            let (bits, signed) = match core {
                "ubyte" => (8u32, false),
                "sbyte" => (8, true),
                "ushort" => (16, false),
                "sshort" | "short" => (16, true),
                _ => return unsupported(i),
            };
            let signed = signed && d16 == 0;
            let old = {
                let v = self.vvar(d0);
                self.load_u32(v)
            };
            return self.guarded(|t| {
                let (w, shift) = t.psb_word_and_shift(addr);
                let c = t.cu(bits);
                let r = if signed {
                    let wi = t.spv.bitcast(w, Sc::I32);
                    let r = t.spv.val(OP_BFSEXT, Sc::I32, 1, &[wi.id, shift.id, c.id]);
                    t.spv.bitcast(r, Sc::U32)
                } else {
                    t.spv.val(OP_BFUEXT, Sc::U32, 1, &[w.id, shift.id, c.id])
                };
                let out = match d16 {
                    0 => r,
                    1 => {
                        let (hi, lo) = (t.cu(0xFFFF_0000), t.cu(0xFFFF));
                        let keep = t.op2(OP_AND, Sc::U32, old, hi);
                        let nv = t.op2(OP_AND, Sc::U32, r, lo);
                        t.op2(OP_OR, Sc::U32, keep, nv)
                    }
                    _ => {
                        let (lo, c16) = (t.cu(0xFFFF), t.cu(16));
                        let keep = t.op2(OP_AND, Sc::U32, old, lo);
                        let nv = t.op2(OP_SHL, Sc::U32, r, c16);
                        t.op2(OP_OR, Sc::U32, keep, nv)
                    }
                };
                t.write_v(d0, out);
                Ok(())
            });
        }
        if let Some(rest) = name.strip_prefix("store_") {
            let d = data.ok_or_else(|| TranslateError::Unsupported(i.name.into()))?;
            let Src::Vgpr(d0) = d.src else {
                return unsupported(i);
            };
            let d0 = usize::from(d0);
            if let Some(n) = words(rest) {
                let vals: Vec<Val> = (0..n)
                    .map(|c| {
                        let v = self.vvar(d0 + c);
                        self.load_u32(v)
                    })
                    .collect();
                return self.guarded(|t| {
                    for (c, v) in vals.iter().enumerate() {
                        let a = t.add_u64_imm(addr, 4 * c as i64);
                        let p = t.psb_ptr(a);
                        t.spv.emit_void(62, &[p, v.id, 2, 4]);
                    }
                    Ok(())
                });
            }
            let (core, hi) = match rest.strip_suffix("_d16_hi") {
                Some(c) => (c, true),
                None => (rest, false),
            };
            let bits = match core {
                "byte" => 8u32,
                "short" => 16,
                _ => return unsupported(i),
            };
            let v = self.vvar(d0);
            let mut val = self.load_u32(v);
            if hi {
                let c16 = self.cu(16);
                val = self.op2(OP_SHR, Sc::U32, val, c16);
            }
            return self.guarded(|t| {
                // lecture-modification-écriture atomique du mot contenant
                let (word_ptr, shift) = t.psb_word_ptr_and_shift(addr);
                let mask = t.cu(if bits == 8 { 0xFF } else { 0xFFFF });
                let m = t.op2(OP_AND, Sc::U32, val, mask);
                let vsh = t.op2(OP_SHL, Sc::U32, m, shift);
                let msh = t.op2(OP_SHL, Sc::U32, mask, shift);
                let inv = t.op1(OP_NOT, Sc::U32, msh);
                let (scope, sem) = (t.spv.c_u32(1), t.spv.c_u32(0));
                let u = t.spv.ty(Sc::U32, 1);
                t.spv.emit(240, u, &[word_ptr, scope, sem, inv.id]);
                t.spv.emit(241, u, &[word_ptr, scope, sem, vsh.id]);
                Ok(())
            });
        }
        if let Some(op) = name.strip_prefix("atomic_") {
            let d = data.ok_or_else(|| TranslateError::Unsupported(i.name.into()))?;
            let Src::Vgpr(d0) = d.src else {
                return unsupported(i);
            };
            let d0 = usize::from(d0);
            let code: u16 = match op {
                "add" => 234,
                "sub" => 235,
                "smin" => 236,
                "umin" => 237,
                "smax" => 238,
                "umax" => 239,
                "and" => 240,
                "or" => 241,
                "xor" => 242,
                "swap" => 229,
                "cmpswap" => 230,
                _ => return unsupported(i),
            };
            let v = self.vvar(d0);
            let val = self.load_u32(v);
            let cmp = if op == "cmpswap" {
                let c = self.vvar(d0 + 1);
                Some(self.load_u32(c))
            } else {
                None
            };
            let ret = dst;
            return self.guarded(|t| {
                let p = t.psb_ptr(addr);
                let (scope, sem) = (t.spv.c_u32(1), t.spv.c_u32(0));
                let u = t.spv.ty(Sc::U32, 1);
                let old = match cmp {
                    Some(c) => t.spv.emit(230, u, &[p, scope, sem, sem, val.id, c.id]),
                    None => t.spv.emit(code, u, &[p, scope, sem, val.id]),
                };
                if let Some(Operand {
                    src: Src::Vgpr(r), ..
                }) = ret
                {
                    t.write_v(
                        usize::from(r),
                        Val {
                            id: old,
                            sc: Sc::U32,
                            n: 1,
                        },
                    );
                }
                Ok(())
            });
        }
        unsupported(i)
    }

    /// Mot aligné contenant `addr` et décalage en bits de l'octet/demi-mot.
    fn psb_word_and_shift(&mut self, addr: u32) -> (Val, Val) {
        let (ptr, shift) = self.psb_word_ptr_and_shift(addr);
        (self.psb_load(ptr), shift)
    }

    fn psb_word_ptr_and_shift(&mut self, addr: u32) -> (u32, Val) {
        let t = self.u64_ty();
        let m = self.spv.c_raw(t, &[!3u32, u32::MAX]);
        let aligned = self.spv.emit(OP_AND, t, &[addr, m]);
        let ptr = self.psb_ptr(aligned);
        let u = self.spv.ty(Sc::U32, 1);
        let lo = self.spv.emit(113, u, &[addr]); // OpUConvert -> u32
        let c3 = self.cu(3);
        let low = self.op2(
            OP_AND,
            Sc::U32,
            Val {
                id: lo,
                sc: Sc::U32,
                n: 1,
            },
            c3,
        );
        let c8 = self.cu(8);
        let shift = self.op2(132, Sc::U32, low, c8);
        (ptr, shift)
    }
}
