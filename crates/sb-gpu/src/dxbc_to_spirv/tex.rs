//! Textures, échantillonnage, buffers bruts/structurés, UAV, atomiques, barrières.

use super::ShaderTranslateError;
use super::codegen::{BufKind, BufVar, Gen, R, ResDim, invalid, mask_comps, unsupported};
use super::decode::{Inst, Operand};
use super::opcodes::*;
use super::spv::{Sc, Val};
use crate::shader::ShaderStage;

fn slot_of(op: &Operand) -> R<u32> {
    if op.dims != 1 || op.idx[0].rel.is_some() {
        return unsupported(
            "tableaux de descripteurs / indexation dynamique de ressources (SM5.1)",
        );
    }
    Ok(op.idx[0].imm)
}

impl Gen {
    fn need_ops(inst: &Inst, k: usize) -> R<()> {
        if inst.operands.len() < k {
            return invalid(format!(
                "{} : {} opérandes attendus",
                opcode_name(inst.opcode),
                k
            ));
        }
        Ok(())
    }

    fn texel_offset(&mut self, inst: &Inst, dim: ResDim) -> Option<u32> {
        let off = inst.offset?;
        let n = dim.offset_comps() as usize;
        if n == 0 || off.iter().take(n).all(|&o| o == 0) {
            return None;
        }
        let bits: Vec<u32> = off.iter().take(n).map(|&o| o as u32).collect();
        Some(self.s.c_vec(Sc::I32, &bits).id)
    }

    /// Écrit `res4` (vec4 de résultats) en appliquant le swizzle de la ressource.
    fn write_swizzled(&mut self, dst: &Operand, res4: Val, swz_op: &Operand, sat: bool) -> R<()> {
        let comps = mask_comps(dst.mask);
        if comps.is_empty() {
            return Ok(());
        }
        let swz: Vec<u32> = comps.iter().map(|&c| swz_op.swizzle[c as usize]).collect();
        let v = self.s.pick(res4, &swz);
        self.write(dst, v, sat)
    }

    fn zeros_pad(&mut self, v: Val, total: u32) -> Val {
        let z = match v.sc {
            Sc::F32 => self.s.c_f32(0.0),
            _ => self.s.c_bits(v.sc, 0),
        };
        let mut comps = Vec::new();
        for i in 0..total {
            comps.push(if i < v.n { self.s.extract(v, i).id } else { z });
        }
        self.s.construct(v.sc, &comps)
    }

    fn sampled_image(&mut self, tex_slot: u32, samp_slot: u32) -> R<(u32, super::codegen::TexVar)> {
        let tex = self.texture(tex_slot)?;
        let sv = self.sampler(samp_slot)?;
        let samp_ty = self.s.raw_type(26, &[]);
        let img = self.s.emit(61, tex.img_ty, &[tex.var]);
        let smp = self.s.emit(61, samp_ty, &[sv]);
        let si_ty = self.s.raw_type(27, &[tex.img_ty]);
        let si = self.s.emit(86, si_ty, &[img, smp]);
        Ok((si, tex))
    }

    fn sample_like(&mut self, inst: &Inst) -> R<()> {
        Self::need_ops(inst, 4)?;
        let ops = &inst.operands;
        let (dst, res_op) = (&ops[0], &ops[2]);
        if mask_comps(dst.mask).is_empty() {
            return Ok(());
        }
        let (si, tex) = self.sampled_image(slot_of(res_op)?, slot_of(&ops[3])?)?;
        let nc = tex.dim.coord_comps();
        let all: Vec<u32> = (0..nc).collect();
        let coord = self.read(&ops[1], &all, Sc::F32)?;
        let pixel = self.stage == ShaderStage::Pixel;
        let op = inst.opcode;
        let dref = matches!(op, OP_SAMPLE_C | OP_SAMPLE_C_LZ);
        let mut mask = 0u32;
        let mut words: Vec<u32> = Vec::new();
        let mut implicit = pixel && matches!(op, OP_SAMPLE | OP_SAMPLE_B | OP_SAMPLE_C);
        match op {
            OP_SAMPLE_L => {
                Self::need_ops(inst, 5)?;
                mask |= 2;
                words.push(self.read(&ops[4], &[0], Sc::F32)?.id);
            }
            OP_SAMPLE_B if pixel => {
                Self::need_ops(inst, 5)?;
                mask |= 1;
                words.push(self.read(&ops[4], &[0], Sc::F32)?.id);
            }
            OP_SAMPLE_D => {
                Self::need_ops(inst, 6)?;
                let g = nc - u32::from(tex.dim.arrayed());
                let gc: Vec<u32> = (0..g).collect();
                let dx = self.read(&ops[4], &gc, Sc::F32)?;
                let dy = self.read(&ops[5], &gc, Sc::F32)?;
                mask |= 4;
                words.push(dx.id);
                words.push(dy.id);
                implicit = false;
            }
            OP_SAMPLE_C_LZ => {
                mask |= 2;
                let z = self.s.c_f32(0.0);
                words.push(z);
            }
            _ => {}
        }
        if !implicit && mask == 0 {
            mask |= 2;
            words.push(self.s.c_f32(0.0));
        }
        // Ordre des opérandes d'image : Bias, Lod, Grad, ConstOffset.
        if let Some(off) = self.texel_offset(inst, tex.dim) {
            mask |= 8;
            words.push(off);
        }
        let (rty, spv_op, mut args): (Sc, u16, Vec<u32>) = if dref {
            Self::need_ops(inst, 5)?;
            let r = self.read(&ops[4], &[0], Sc::F32)?;
            (
                Sc::F32,
                if implicit { 89 } else { 90 },
                vec![si, coord.id, r.id],
            )
        } else {
            (tex.sc, if implicit { 87 } else { 88 }, vec![si, coord.id])
        };
        if mask != 0 {
            args.push(mask);
            args.extend_from_slice(&words);
        }
        let rn = if dref { 1 } else { 4 };
        let r = self.s.val(spv_op, rty, rn, &args);
        let r4 = self.s.splat(r, 4);
        self.write_swizzled(dst, r4, res_op, inst.saturate())
    }

    fn fetch(&mut self, inst: &Inst) -> R<()> {
        Self::need_ops(inst, 3)?;
        let ops = &inst.operands;
        let (dst, res_op) = (&ops[0], &ops[2]);
        if mask_comps(dst.mask).is_empty() {
            return Ok(());
        }
        let tex = self.texture(slot_of(res_op)?)?;
        let nc = tex.dim.coord_comps();
        let coords: Vec<u32> = (0..nc).collect();
        let c = self.read(&ops[1], &coords, Sc::I32)?;
        let img = self.s.emit(61, tex.img_ty, &[tex.var]);
        let mut args = vec![img, c.id];
        let mut mask = 0u32;
        let mut words = Vec::new();
        if inst.opcode == OP_LD_MS {
            Self::need_ops(inst, 4)?;
            mask |= 0x40;
            words.push(self.read(&ops[3], &[0], Sc::I32)?.id);
        } else if tex.dim != ResDim::Buffer && !tex.dim.ms() {
            mask |= 2;
            words.push(self.read(&ops[1], &[nc], Sc::I32)?.id);
        }
        if let Some(off) = self.texel_offset(inst, tex.dim) {
            mask |= 8;
            words.insert(words.len().min(1), off);
            if mask & 0x40 != 0 {
                // Ordre : ConstOffset (8) avant Sample (0x40).
                words.swap(0, 1);
            }
        }
        if mask != 0 {
            args.push(mask);
            args.extend_from_slice(&words);
        }
        let r = self.s.val(95, tex.sc, 4, &args);
        self.write_swizzled(dst, r, res_op, inst.saturate())
    }

    fn gather(&mut self, inst: &Inst) -> R<()> {
        let po = matches!(inst.opcode, OP_GATHER4_PO | OP_GATHER4_PO_C);
        let dref = matches!(inst.opcode, OP_GATHER4_C | OP_GATHER4_PO_C);
        let base = if po { 3 } else { 2 };
        Self::need_ops(inst, base + 2 + usize::from(dref))?;
        let ops = &inst.operands;
        let dst = &ops[0];
        if mask_comps(dst.mask).is_empty() {
            return Ok(());
        }
        let res_op = &ops[base];
        let samp_op = &ops[base + 1];
        let (si, tex) = self.sampled_image(slot_of(res_op)?, slot_of(samp_op)?)?;
        let nc = tex.dim.coord_comps();
        let all: Vec<u32> = (0..nc).collect();
        let coord = self.read(&ops[1], &all, Sc::F32)?;
        let mut args = vec![si, coord.id];
        let spv_op = if dref {
            let r = self.read(&ops[base + 2], &[0], Sc::F32)?;
            args.push(r.id);
            97
        } else {
            let comp = self.s.c_u32(samp_op.swizzle[0]);
            args.push(comp);
            96
        };
        if po {
            self.s.caps.insert(25);
            let o = self.read(&ops[2], &[0, 1], Sc::I32)?;
            args.push(0x10);
            args.push(o.id);
        } else if let Some(off) = self.texel_offset(inst, tex.dim) {
            args.push(8);
            args.push(off);
        }
        let r = self.s.val(spv_op, tex.sc, 4, &args);
        self.write_swizzled(dst, r, res_op, inst.saturate())
    }

    fn lod(&mut self, inst: &Inst) -> R<()> {
        Self::need_ops(inst, 4)?;
        let ops = &inst.operands;
        let (si, tex) = self.sampled_image(slot_of(&ops[2])?, slot_of(&ops[3])?)?;
        self.s.caps.insert(50);
        let nc = tex.dim.coord_comps() - u32::from(tex.dim.arrayed());
        let all: Vec<u32> = (0..nc).collect();
        let coord = self.read(&ops[1], &all, Sc::F32)?;
        let r = self.s.val(105, Sc::F32, 2, &[si, coord.id]);
        let r4 = self.zeros_pad(r, 4);
        self.write_swizzled(&ops[0], r4, &ops[2], false)
    }

    fn resinfo(&mut self, inst: &Inst) -> R<()> {
        Self::need_ops(inst, 3)?;
        let ops = &inst.operands;
        let res_op = &ops[2];
        self.s.caps.insert(50);
        let slot = slot_of(res_op)?;
        let (img, dim, storage) = if res_op.ty == OPERAND_UNORDERED_ACCESS_VIEW {
            let t = self.uav_image(slot)?;
            (self.s.emit(61, t.img_ty, &[t.var]), t.dim, true)
        } else {
            let t = self.texture(slot)?;
            (self.s.emit(61, t.img_ty, &[t.var]), t.dim, false)
        };
        let sn = dim.size_comps();
        let (size, levels) = if storage || dim.ms() || dim == ResDim::Buffer {
            let size = self.s.val(104, Sc::U32, sn, &[img]);
            let lv = if dim.ms() { 1 } else { 0 };
            (size, self.s.c_u32(lv))
        } else {
            let lod = self.read(&ops[1], &[0], Sc::U32)?;
            let size = self.s.val(103, Sc::U32, sn, &[img, lod.id]);
            let lv = self.s.val(106, Sc::U32, 1, &[img]);
            (size, lv.id)
        };
        let padded = self.zeros_pad(size, 3);
        let mut comps: Vec<u32> = (0..3).map(|i| self.s.extract(padded, i).id).collect();
        comps.push(levels);
        let v4 = self.s.construct(Sc::U32, &comps);
        let kind = (inst.token >> 11) & 3;
        let out = if kind == 2 {
            v4
        } else {
            let f = self.s.val(112, Sc::F32, 4, &[v4.id]);
            if kind == 1 {
                let one = self.s.c_vec(Sc::F32, &[1.0f32.to_bits(); 4]);
                let rcp = self.s.val(136, Sc::F32, 4, &[one.id, f.id]);
                let layer = if dim.arrayed() {
                    dim.size_comps() - 1
                } else {
                    9
                };
                let mut cs = Vec::new();
                for i in 0..4u32 {
                    let src = if i == layer || i == 3 { f } else { rcp };
                    cs.push(self.s.extract(src, i).id);
                }
                self.s.construct(Sc::F32, &cs)
            } else {
                f
            }
        };
        self.write_swizzled(&ops[0], out, res_op, false)
    }

    fn bufinfo(&mut self, inst: &Inst) -> R<()> {
        Self::need_ops(inst, 2)?;
        let ops = &inst.operands;
        let src = &ops[1];
        let slot = slot_of(src)?;
        let value = if src.ty == OPERAND_UNORDERED_ACCESS_VIEW
            && self.d.uav.get(&slot).map(|d| d.kind) == Some(BufKind::Typed)
        {
            self.s.caps.insert(50);
            let t = self.uav_image(slot)?;
            let img = self.s.emit(61, t.img_ty, &[t.var]);
            self.s.val(104, Sc::U32, 1, &[img])
        } else if src.ty == OPERAND_RESOURCE
            && self.d.res.get(&slot).map(|d| d.kind) == Some(BufKind::Typed)
        {
            self.s.caps.insert(50);
            let t = self.texture(slot)?;
            let img = self.s.emit(61, t.img_ty, &[t.var]);
            self.s.val(104, Sc::U32, 1, &[img])
        } else {
            let b = self.buffer_of(src)?;
            let words = self.s.val(68, Sc::U32, 1, &[b.var, 0]);
            let two = self.s.c_u32(2);
            let bytes = self.s.val(196, Sc::U32, 1, &[words.id, two]);
            match b.kind {
                BufKind::Structured(stride) if stride > 0 => {
                    let st = self.s.c_u32(stride);
                    self.s.val(134, Sc::U32, 1, &[bytes.id, st])
                }
                _ => bytes,
            }
        };
        let z = self.s.c_u32(0);
        let v4 = self.s.construct(Sc::U32, &[value.id, z, z, z]);
        self.write_swizzled(&ops[0], v4, src, false)
    }

    fn sampleinfo(&mut self, inst: &Inst) -> R<()> {
        Self::need_ops(inst, 2)?;
        let ops = &inst.operands;
        let src = &ops[1];
        let count = if src.ty == OPERAND_RESOURCE {
            self.s.caps.insert(50);
            let t = self.texture(slot_of(src)?)?;
            let img = self.s.emit(61, t.img_ty, &[t.var]);
            self.s.val(107, Sc::U32, 1, &[img])
        } else {
            // Rasterizer : le nombre d'échantillons du pipeline est inconnu ici -> 1 (documenté).
            Val {
                id: self.s.c_u32(1),
                sc: Sc::U32,
                n: 1,
            }
        };
        let z = self.s.c_u32(0);
        let v4 = self.s.construct(Sc::U32, &[count.id, z, z, z]);
        let v4 = if (inst.token >> 11) & 3 == 1 {
            v4
        } else {
            self.s.val(112, Sc::F32, 4, &[v4.id])
        };
        self.write_swizzled(&ops[0], v4, src, false)
    }

    // ---- UAV typés
    fn ld_uav_typed(&mut self, inst: &Inst) -> R<()> {
        Self::need_ops(inst, 3)?;
        let ops = &inst.operands;
        let t = self.uav_image(slot_of(&ops[2])?)?;
        let nc: Vec<u32> = (0..t.dim.coord_comps()).collect();
        let c = self.read(&ops[1], &nc, Sc::U32)?;
        let img = self.s.emit(61, t.img_ty, &[t.var]);
        let r = self.s.val(98, t.sc, 4, &[img, c.id]);
        self.write_swizzled(&ops[0], r, &ops[2], false)
    }

    fn store_uav_typed(&mut self, inst: &Inst) -> R<()> {
        Self::need_ops(inst, 3)?;
        let ops = &inst.operands;
        let t = self.uav_image(slot_of(&ops[0])?)?;
        let nc: Vec<u32> = (0..t.dim.coord_comps()).collect();
        let c = self.read(&ops[1], &nc, Sc::U32)?;
        let texel = self.read(&ops[2], &[0, 1, 2, 3], t.sc)?;
        let img = self.s.emit(61, t.img_ty, &[t.var]);
        self.s.emit_void(99, &[img, c.id, texel.id]);
        Ok(())
    }

    // ---- buffers bruts / structurés / TGSM
    fn buffer_of(&mut self, op: &Operand) -> R<BufVar> {
        match op.ty {
            OPERAND_UNORDERED_ACCESS_VIEW => self.uav_buffer(slot_of(op)?),
            OPERAND_RESOURCE => self.srv_buffer(slot_of(op)?),
            t => invalid(format!("opérande 0x{t:X} n'est pas un buffer")),
        }
    }

    /// Pointeur vers le mot d'indice `word` (U32) de la cible ; `(ptr, stride)`.
    fn word_ptr(&mut self, target: &Operand, word: Val) -> R<u32> {
        if target.ty == OPERAND_THREAD_GROUP_SHARED_MEMORY {
            let (var, _) = self.tgsm(slot_of(target)?)?;
            let p = self.pty(4, Sc::U32, 1);
            return Ok(self.s.emit(65, p, &[var, word.id]));
        }
        let b = self.buffer_of(target)?;
        let z = self.s.c_u32(0);
        let p = self.pty(12, Sc::U32, 1);
        Ok(self.s.emit(65, p, &[b.var, z, word.id]))
    }

    fn stride_of(&mut self, target: &Operand) -> R<u32> {
        if target.ty == OPERAND_THREAD_GROUP_SHARED_MEMORY {
            return Ok(self.tgsm(slot_of(target)?)?.1);
        }
        Ok(match self.buffer_of(target)?.kind {
            BufKind::Structured(s) => s,
            _ => 0,
        })
    }

    /// Adresse en octets : `byte_op` seul (brut) ou `idx * stride + off` (structuré).
    fn byte_addr(
        &mut self,
        target: &Operand,
        idx_op: Option<&Operand>,
        off_op: &Operand,
    ) -> R<Val> {
        let off = self.read(off_op, &[0], Sc::U32)?;
        let Some(idx_op) = idx_op else {
            return Ok(off);
        };
        let stride = self.stride_of(target)?;
        let i = self.read(idx_op, &[0], Sc::U32)?;
        let st = self.s.c_u32(stride);
        let m = self.s.val(132, Sc::U32, 1, &[i.id, st]);
        Ok(self.s.val(128, Sc::U32, 1, &[m.id, off.id]))
    }

    fn word_at(&mut self, bytes: Val, k: u32) -> Val {
        let two = self.s.c_u32(2);
        let w = self.s.val(194, Sc::U32, 1, &[bytes.id, two]);
        if k == 0 {
            w
        } else {
            let kc = self.s.c_u32(k);
            self.s.val(128, Sc::U32, 1, &[w.id, kc])
        }
    }

    fn ld_buf(&mut self, inst: &Inst) -> R<()> {
        let structured = inst.opcode == OP_LD_STRUCTURED;
        Self::need_ops(inst, if structured { 4 } else { 3 })?;
        let ops = &inst.operands;
        let dst = &ops[0];
        let comps = mask_comps(dst.mask);
        if comps.is_empty() {
            return Ok(());
        }
        let src = if structured { &ops[3] } else { &ops[2] };
        let bytes = if structured {
            self.byte_addr(src, Some(&ops[1]), &ops[2])?
        } else {
            self.byte_addr(src, None, &ops[1])?
        };
        let need = comps
            .iter()
            .map(|&c| src.swizzle[c as usize])
            .max()
            .unwrap_or(0)
            + 1;
        let mut words = Vec::new();
        for k in 0..need {
            let w = self.word_at(bytes, k);
            let p = self.word_ptr(src, w)?;
            words.push(self.s.val(61, Sc::U32, 1, &[p]).id);
        }
        let mut full = words.clone();
        while full.len() < 4 {
            full.push(self.s.c_u32(0));
        }
        let v4 = self.s.construct(Sc::U32, &full);
        self.write_swizzled(dst, v4, src, false)
    }

    fn st_buf(&mut self, inst: &Inst) -> R<()> {
        let structured = inst.opcode == OP_STORE_STRUCTURED;
        Self::need_ops(inst, if structured { 4 } else { 3 })?;
        let ops = &inst.operands;
        let dst = &ops[0];
        let comps = mask_comps(dst.mask);
        if comps.is_empty() {
            return Ok(());
        }
        let (bytes, src) = if structured {
            (self.byte_addr(dst, Some(&ops[1]), &ops[2])?, &ops[3])
        } else {
            (self.byte_addr(dst, None, &ops[1])?, &ops[2])
        };
        let v = self.read(src, &comps, Sc::U32)?;
        for k in 0..comps.len() as u32 {
            let w = self.word_at(bytes, k);
            let p = self.word_ptr(dst, w)?;
            let e = self.s.extract(v, k);
            self.s.emit_void(62, &[p, e.id]);
        }
        Ok(())
    }

    fn atomic(&mut self, inst: &Inst) -> R<()> {
        let imm = inst.opcode >= OP_IMM_ATOMIC_IADD;
        let base = usize::from(imm);
        let cmp = matches!(inst.opcode, OP_ATOMIC_CMP_STORE | OP_IMM_ATOMIC_CMP_EXCH);
        Self::need_ops(inst, base + 3 + usize::from(cmp))?;
        let ops = &inst.operands;
        let target = &ops[base];
        let addr_op = &ops[base + 1];
        let (spv_op, _) = match inst.opcode {
            OP_ATOMIC_AND | OP_IMM_ATOMIC_AND => (240u16, ()),
            OP_ATOMIC_OR | OP_IMM_ATOMIC_OR => (241, ()),
            OP_ATOMIC_XOR | OP_IMM_ATOMIC_XOR => (242, ()),
            OP_ATOMIC_IADD | OP_IMM_ATOMIC_IADD => (234, ()),
            OP_ATOMIC_IMAX | OP_IMM_ATOMIC_IMAX => (238, ()),
            OP_ATOMIC_IMIN | OP_IMM_ATOMIC_IMIN => (236, ()),
            OP_ATOMIC_UMAX | OP_IMM_ATOMIC_UMAX => (239, ()),
            OP_ATOMIC_UMIN | OP_IMM_ATOMIC_UMIN => (237, ()),
            OP_IMM_ATOMIC_EXCH => (229, ()),
            _ => (230, ()),
        };
        let slot = slot_of(target)?;
        let shared = target.ty == OPERAND_THREAD_GROUP_SHARED_MEMORY;
        let scope = self.s.c_u32(if shared { 2 } else { 1 });
        let sem = self.s.c_u32(0);
        let ptr = if target.ty == OPERAND_UNORDERED_ACCESS_VIEW
            && self.d.uav.get(&slot).map(|d| d.kind) == Some(BufKind::Typed)
        {
            let t = self.uav_image(slot)?;
            let nc: Vec<u32> = (0..t.dim.coord_comps()).collect();
            let c = self.read(addr_op, &nc, Sc::U32)?;
            let pt = self.pty(11, t.sc, 1);
            let s0 = self.s.c_u32(0);
            self.s.emit(60, pt, &[t.var, c.id, s0])
        } else {
            let structured = self.stride_of(target)? > 0;
            let bytes = if structured {
                let idx = self.read(addr_op, &[0], Sc::U32)?;
                let off = self.read(addr_op, &[1], Sc::U32)?;
                let stride = self.stride_of(target)?;
                let st = self.s.c_u32(stride);
                let m = self.s.val(132, Sc::U32, 1, &[idx.id, st]);
                self.s.val(128, Sc::U32, 1, &[m.id, off.id])
            } else {
                self.read(addr_op, &[0], Sc::U32)?
            };
            let w = self.word_at(bytes, 0);
            self.word_ptr(target, w)?
        };
        let v0 = self.read(&ops[base + 2], &[0], Sc::U32)?;
        let u = self.s.ty(Sc::U32, 1);
        let id = if cmp {
            let v1 = self.read(&ops[base + 3], &[0], Sc::U32)?;
            // D3D : (cmp_value, new_value) ; SPIR-V : Value = nouvelle valeur, Comparator = cmp.
            self.s.emit(230, u, &[ptr, scope, sem, sem, v1.id, v0.id])
        } else if spv_op == 229 {
            self.s.emit(229, u, &[ptr, scope, sem, v0.id])
        } else {
            self.s.emit(spv_op, u, &[ptr, scope, sem, v0.id])
        };
        if imm {
            let r = Val {
                id,
                sc: Sc::U32,
                n: 1,
            };
            self.write(&ops[0], r, false)?;
        }
        Ok(())
    }

    fn sync(&mut self, inst: &Inst) -> R<()> {
        if self.stage != ShaderStage::Compute {
            return Ok(());
        }
        let f = (inst.token >> 11) & 0xF;
        let mut sem = 0u32;
        if f & 2 != 0 {
            sem |= 0x100;
        }
        if f & 0xC != 0 {
            sem |= 0x40 | 0x800;
        }
        let scope = self.s.c_u32(if f & 8 != 0 { 1 } else { 2 });
        if f & 1 != 0 {
            let exec = self.s.c_u32(2);
            let sm = self.s.c_u32(sem | 8);
            self.s.emit_void(224, &[exec, scope, sm]);
        } else if sem != 0 {
            let sm = self.s.c_u32(sem | 8);
            self.s.emit_void(225, &[scope, sm]);
        }
        Ok(())
    }

    fn eval(&mut self, inst: &Inst) -> R<()> {
        Self::need_ops(inst, 2)?;
        let ops = &inst.operands;
        let src = &ops[1];
        if src.ty != OPERAND_INPUT || self.stage != ShaderStage::Pixel {
            return invalid("eval_* attend une entrée de pixel shader");
        }
        let ptr = self.input_ptr(src.idx[0].imm)?;
        self.s.caps.insert(52);
        let r = match inst.opcode {
            OP_EVAL_CENTROID => self.s.glsl_op(Sc::F32, 4, 76, &[ptr]),
            OP_EVAL_SAMPLE_INDEX => {
                Self::need_ops(inst, 3)?;
                let i = self.read(&ops[2], &[0], Sc::I32)?;
                self.s.glsl_op(Sc::F32, 4, 77, &[ptr, i.id])
            }
            _ => {
                Self::need_ops(inst, 3)?;
                let o = self.read(&ops[2], &[0, 1], Sc::I32)?;
                let of = self.s.val(111, Sc::F32, 2, &[o.id]);
                let k = self.s.c_vec(Sc::F32, &[(1.0f32 / 16.0).to_bits(); 2]);
                let sc = self.s.val(133, Sc::F32, 2, &[of.id, k.id]);
                self.s.glsl_op(Sc::F32, 4, 78, &[ptr, sc.id])
            }
        };
        self.write_swizzled(&ops[0], r, src, inst.saturate())
    }

    pub(super) fn exec_tex(&mut self, inst: &Inst) -> R<()> {
        match inst.opcode {
            OP_SAMPLE | OP_SAMPLE_L | OP_SAMPLE_B | OP_SAMPLE_D | OP_SAMPLE_C | OP_SAMPLE_C_LZ => {
                self.sample_like(inst)
            }
            OP_LD | OP_LD_MS => self.fetch(inst),
            OP_GATHER4 | OP_GATHER4_C | OP_GATHER4_PO | OP_GATHER4_PO_C => self.gather(inst),
            OP_LOD => self.lod(inst),
            OP_RESINFO => self.resinfo(inst),
            OP_BUFINFO => self.bufinfo(inst),
            OP_SAMPLEINFO => self.sampleinfo(inst),
            OP_LD_UAV_TYPED => self.ld_uav_typed(inst),
            OP_STORE_UAV_TYPED => self.store_uav_typed(inst),
            OP_LD_RAW | OP_LD_STRUCTURED => self.ld_buf(inst),
            OP_STORE_RAW | OP_STORE_STRUCTURED => self.st_buf(inst),
            OP_ATOMIC_AND..=OP_ATOMIC_UMIN | OP_IMM_ATOMIC_IADD..=OP_IMM_ATOMIC_UMIN => {
                if inst.opcode == OP_ATOMIC_CMP_STORE
                    || !matches!(inst.opcode, OP_IMM_ATOMIC_ALLOC | OP_IMM_ATOMIC_CONSUME)
                {
                    self.atomic(inst)
                } else {
                    Err(self.unsupported_op(inst))
                }
            }
            OP_SYNC => self.sync(inst),
            OP_EVAL_CENTROID | OP_EVAL_SAMPLE_INDEX | OP_EVAL_SNAPPED => self.eval(inst),
            _ => Err(self.unsupported_op(inst)),
        }
    }

    fn unsupported_op(&self, inst: &Inst) -> ShaderTranslateError {
        ShaderTranslateError::UnsupportedOpcode {
            opcode: inst.opcode,
            name: opcode_name(inst.opcode),
        }
    }
}
