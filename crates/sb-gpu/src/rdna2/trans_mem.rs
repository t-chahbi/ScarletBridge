//! Traduction des accès mémoire, exports et interpolation : SMEM, MUBUF/MTBUF, MIMG (échantillonnage
//! de base), DS (LDS), `exp` et `v_interp_*`.

use super::inst::{Instruction, Operand, Src};
use super::tables::TBUF_FORMATS;
use super::translate::{
    BindingUse, ResRef, ResourceKind, ResourceLoc, Stage, TResult, Tr, TranslateError, res_ref,
    unsupported,
};
use crate::dxbc_to_spirv::spv::{Sc, Val};

const OP_IADD: u16 = 128;
const OP_IMUL: u16 = 132;
const OP_AND: u16 = 199;
const OP_SHR: u16 = 194;
const OP_BFSEXT: u16 = 202;
const OP_BFUEXT: u16 = 203;

fn err<T>(msg: impl Into<String>) -> TResult<T> {
    Err(TranslateError::Unresolved(msg.into()))
}

/// Opérande registre vectoriel : index du VGPR de base.
fn vgpr_of(o: &Operand) -> TResult<usize> {
    match o.src {
        Src::Vgpr(n) => Ok(usize::from(n)),
        other => Err(TranslateError::Unsupported(format!(
            "registre vectoriel attendu, trouvé {other:?}"
        ))),
    }
}

/// Nombre de composantes d'un suffixe `x`/`xy`/`xyz`/`xyzw`.
fn format_comps(name: &str) -> Option<usize> {
    let tail = name.rsplit('_').next()?;
    matches!(tail, "x" | "xy" | "xyz" | "xyzw").then_some(tail.len())
}

/// Nombre de dwords d'un suffixe `dword`/`dwordx2`/`dwordx3`/`dwordx4`.
fn dword_count(name: &str) -> Option<usize> {
    match name.rsplit('_').next()? {
        "dword" => Some(1),
        "dwordx2" => Some(2),
        "dwordx3" => Some(3),
        "dwordx4" => Some(4),
        _ => None,
    }
}

impl Tr<'_> {
    // ------------------------------------------------------------ résolution de descripteurs
    fn sgpr_index(o: &Operand) -> TResult<u8> {
        match o.src {
            Src::Sgpr(n) => Ok(n),
            Src::Ttmp(n) => Ok(108 + n),
            other => err(format!(
                "descripteur attendu dans des SGPR, trouvé {other:?}"
            )),
        }
    }

    fn buffer_res(&mut self, o: &Operand) -> TResult<(ResRef, u32)> {
        let n = Self::sgpr_index(o)?;
        match self.res_map.get(&n).copied() {
            Some(r) => match r.kind {
                ResourceKind::Buffer { stride } => Ok((r, stride)),
                _ => err(format!("s{n} ne désigne pas un buffer")),
            },
            None => err(format!(
                "aucun descripteur de buffer connu dans s[{n}:{}]",
                n + 3
            )),
        }
    }

    // ------------------------------------------------------------ SMEM
    pub(crate) fn smem(&mut self, i: &Instruction) -> TResult<()> {
        let name = i.name;
        if matches!(name, "s_dcache_inv" | "s_gl1_inv" | "s_dcache_wb") {
            return Ok(());
        }
        let count =
            dword_count_smem(name).ok_or_else(|| TranslateError::Unsupported(name.into()))?;
        let (dst, base) = (i.operands[0], i.operands[1]);
        let dcode =
            Self::scalar_code(&dst)?.ok_or_else(|| TranslateError::Unsupported(name.into()))?;
        let is_buffer = name.starts_with("s_buffer_load");
        // offset en octets (statique si possible)
        let imm = i.mods.offset;
        let soff = if i.mods.smem_soffset {
            Some(self.src_u32(&i.operands[2])?)
        } else {
            None
        };

        if is_buffer {
            let (r, _) = self.buffer_res(&base)?;
            return self.smem_read_words(r.set, r.binding, imm, soff, dcode, count);
        }
        // s_load : base = paire de SGPR
        let n = Self::sgpr_index(&base)?;
        let cfg = self.cfg;
        // 1. pointeur vers un buffer de données
        for r in &cfg.resources {
            if let (ResourceLoc::Pointer(p), ResourceKind::Buffer { .. }) = (r.loc, r.kind)
                && p == n
            {
                return self.smem_read_words(r.set, r.binding, imm, soff, dcode, count);
            }
        }
        // 2. table de descripteurs : on associe les SGPR de destination aux ressources chargées
        let lo = i64::from(imm);
        let hi = lo + 4 * count as i64;
        let mut found = false;
        self.clobber(dcode, count);
        for r in &cfg.resources {
            if let ResourceLoc::Table {
                table_sgpr,
                byte_offset,
            } = r.loc
            {
                let sz = i64::from(res_ref(r).size) * 4;
                let off = i64::from(byte_offset);
                if table_sgpr == n && off >= lo && off + sz <= hi && soff.is_none() {
                    let sgpr = dcode + ((off - lo) / 4) as usize;
                    self.res_map.insert(sgpr as u8, res_ref(r));
                    found = true;
                }
            }
        }
        if found {
            return Ok(());
        }
        err(format!(
            "s_load depuis s[{n}:{}] : ni pointeur de buffer ni table de descripteurs configuré",
            n + 1
        ))
    }

    /// Lit `count` dwords consécutifs d'un buffer vers des SGPR.
    fn smem_read_words(
        &mut self,
        set: u32,
        binding: u32,
        imm: i32,
        soff: Option<Val>,
        dcode: usize,
        count: usize,
    ) -> TResult<()> {
        let mut byte = self.cu(imm as u32);
        if let Some(s) = soff {
            byte = self.op2(OP_IADD, Sc::U32, byte, s);
        }
        let c2 = self.cu(2);
        let word0 = self.op2(OP_SHR, Sc::U32, byte, c2);
        self.clobber(dcode, count);
        for k in 0..count {
            let kc = self.cu(k as u32);
            let idx = self.op2(OP_IADD, Sc::U32, word0, kc);
            let ptr = self.buffer_word_ptr(set, binding, idx);
            let v = self.load_ptr_u32(ptr);
            let var = self.svar(dcode + k);
            self.store_u32(var, v);
        }
        Ok(())
    }

    // ------------------------------------------------------------ MUBUF / MTBUF
    pub(crate) fn vbuf(&mut self, i: &Instruction) -> TResult<()> {
        let name = i.name;
        if i.operands.is_empty() {
            return Ok(()); // buffer_gl*_inv
        }
        if i.mods.tfe {
            return unsupported(i);
        }
        let ops = &i.operands;
        let n = ops.len();
        let (data, vaddr, srsrc, soff) = if n == 4 {
            (Some(ops[0]), ops[1], ops[2], ops[3])
        } else {
            (None, ops[n - 3], ops[n - 2], ops[n - 1])
        };
        let (res, stride) = self.buffer_res(&srsrc)?;
        // adresse en octets
        let mut addr = self.cu(i.mods.offset as u32);
        let so = self.src_u32(&soff)?;
        addr = self.op2(OP_IADD, Sc::U32, addr, so);
        if i.mods.idxen || i.mods.offen {
            let a0 = vgpr_of(&vaddr)?;
            let (idx_reg, off_reg) = match (i.mods.idxen, i.mods.offen) {
                (true, true) => (Some(a0), Some(a0 + 1)),
                (true, false) => (Some(a0), None),
                _ => (None, Some(a0)),
            };
            if let Some(r) = off_reg {
                let var = self.vvar(r);
                let v = self.load_u32(var);
                addr = self.op2(OP_IADD, Sc::U32, addr, v);
            }
            if let Some(r) = idx_reg {
                if stride == 0 {
                    return err("accès idxen sur un buffer de stride 0");
                }
                let var = self.vvar(r);
                let v = self.load_u32(var);
                let s = self.cu(stride);
                let scaled = self.op2(OP_IMUL, Sc::U32, v, s);
                addr = self.op2(OP_IADD, Sc::U32, addr, scaled);
            }
        }
        let c2 = self.cu(2);
        let widx = self.op2(OP_SHR, Sc::U32, addr, c2);
        let (set, binding) = (res.set, res.binding);
        let base = name
            .strip_prefix("tbuffer_")
            .or_else(|| name.strip_prefix("buffer_"))
            .unwrap_or(name);

        // --- atomiques
        if let Some(op) = base.strip_prefix("atomic_") {
            return self.buffer_atomic(i, op, data, set, binding, widx);
        }
        // --- composantes
        let is_t = name.starts_with("tbuffer_");
        let (is_load, is_store) = (base.starts_with("load"), base.starts_with("store"));
        if !(is_load || is_store) {
            return unsupported(i);
        }
        let kind = base
            .trim_start_matches("load_")
            .trim_start_matches("store_");
        // nombre de composantes écrites/lues dans le VGPR
        let (ncomp, mem_comps, small) = if kind.starts_with("format") {
            let nc = format_comps(kind).ok_or_else(|| TranslateError::Unsupported(name.into()))?;
            if is_t {
                let fname = TBUF_FORMATS[usize::from(i.mods.format & 0x7F)];
                let fname = if i.mods.format == 1 {
                    "BUF_FMT_8_UNORM"
                } else {
                    fname
                };
                let mem = fname
                    .strip_prefix("BUF_FMT_")
                    .map(|f| {
                        f.split('_')
                            .take_while(|t| t.parse::<u32>().is_ok())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let all32 = !mem.is_empty()
                    && mem.iter().all(|t| *t == "32")
                    && !fname.contains("UNORM")
                    && !fname.contains("SNORM")
                    && !fname.contains("SCALED");
                if !all32 {
                    return Err(TranslateError::Unsupported(format!(
                        "{name} format {fname} (seuls les formats 32 bits par canal sont supportés)"
                    )));
                }
                (nc, mem.len(), None)
            } else {
                (nc, nc, None) // hypothèse : canaux 32 bits flottants
            }
        } else if let Some(c) = dword_count(kind) {
            (c, c, None)
        } else {
            match kind {
                "ubyte" => (1, 1, Some((8u32, false))),
                "sbyte" => (1, 1, Some((8, true))),
                "ushort" => (1, 1, Some((16, false))),
                "sshort" => (1, 1, Some((16, true))),
                _ => return unsupported(i),
            }
        };
        let d0 = match data {
            Some(d) => vgpr_of(&d)?,
            None => return unsupported(i),
        };
        if is_store {
            if small.is_some() || kind == "byte" || kind == "short" {
                return unsupported(i);
            }
            let mut vals = Vec::new();
            for c in 0..ncomp.min(mem_comps) {
                let var = self.vvar(d0 + c);
                vals.push(self.load_u32(var));
            }
            return self.guarded(|t| {
                for (c, v) in vals.iter().enumerate() {
                    let kc = t.cu(c as u32);
                    let idx = t.op2(OP_IADD, Sc::U32, widx, kc);
                    let ptr = t.buffer_word_ptr(set, binding, idx);
                    t.spv.emit_void(62, &[ptr, v.id]);
                }
                Ok(())
            });
        }
        // charge
        self.guarded(|t| {
            for c in 0..ncomp {
                let v = if c < mem_comps {
                    let kc = t.cu(c as u32);
                    let idx = t.op2(OP_IADD, Sc::U32, widx, kc);
                    let ptr = t.buffer_word_ptr(set, binding, idx);
                    let w = t.load_ptr_u32(ptr);
                    match small {
                        Some((bits, signed)) => {
                            // octet/demi-mot à l'intérieur du dword
                            let mask = t.cu(if bits == 8 { 3 } else { 2 });
                            let byte = t.op2(OP_AND, Sc::U32, addr, mask);
                            let c8 = t.cu(8);
                            let off = t.op2(OP_IMUL, Sc::U32, byte, c8);
                            let wd = t.cu(bits);
                            if signed {
                                let wi = t.spv.bitcast(w, Sc::I32);
                                let r = t.spv.val(OP_BFSEXT, Sc::I32, 1, &[wi.id, off.id, wd.id]);
                                t.spv.bitcast(r, Sc::U32)
                            } else {
                                t.spv.val(OP_BFUEXT, Sc::U32, 1, &[w.id, off.id, wd.id])
                            }
                        }
                        None => w,
                    }
                } else if c == 3 {
                    t.cu(1.0f32.to_bits())
                } else {
                    t.cu(0)
                };
                t.write_v(d0 + c, v);
            }
            Ok(())
        })
    }

    fn buffer_atomic(
        &mut self,
        i: &Instruction,
        op: &str,
        data: Option<Operand>,
        set: u32,
        binding: u32,
        widx: Val,
    ) -> TResult<()> {
        let Some(d) = data else { return unsupported(i) };
        if op.ends_with("_x2") {
            return unsupported(i);
        }
        let d0 = vgpr_of(&d)?;
        let spirv_op: u16 = match op {
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
        let returns = i.mods.glc;
        let var = self.vvar(d0);
        let val = self.load_u32(var);
        let cmp = if op == "cmpswap" {
            let v = self.vvar(d0 + 1);
            Some(self.load_u32(v))
        } else {
            None
        };
        self.guarded(|t| {
            let ptr = t.buffer_word_ptr(set, binding, widx);
            let (scope, sem) = (t.spv.c_u32(1), t.spv.c_u32(0));
            let u = t.spv.ty(Sc::U32, 1);
            let old = if let Some(c) = cmp {
                // OpAtomicCompareExchange : ptr, scope, eq, neq, valeur, comparateur
                // (v[n] = nouvelle valeur, v[n+1] = comparateur)
                t.spv.emit(230, u, &[ptr, scope, sem, sem, val.id, c.id])
            } else {
                t.spv.emit(spirv_op, u, &[ptr, scope, sem, val.id])
            };
            if returns {
                t.write_v(
                    d0,
                    Val {
                        id: old,
                        sc: Sc::U32,
                        n: 1,
                    },
                );
            }
            Ok(())
        })
    }

    // ------------------------------------------------------------ DS (LDS)
    pub(crate) fn ds(&mut self, i: &Instruction) -> TResult<()> {
        let name = i.name;
        if self.cfg.lds_dwords == 0 {
            return Err(TranslateError::Config(
                "instruction ds_* sans lds_dwords".into(),
            ));
        }
        let (is_read, width) = match name {
            "ds_read_b32" => (true, 1),
            "ds_read_b64" => (true, 2),
            "ds_read_b96" => (true, 3),
            "ds_read_b128" => (true, 4),
            "ds_write_b32" => (false, 1),
            "ds_write_b64" => (false, 2),
            "ds_write_b96" => (false, 3),
            "ds_write_b128" => (false, 4),
            _ => return unsupported(i),
        };
        let ops = &i.operands;
        let lds = self.lds_var();
        let (addr_reg, data_reg) = if is_read {
            (vgpr_of(&ops[1])?, vgpr_of(&ops[0])?)
        } else {
            (vgpr_of(&ops[0])?, vgpr_of(&ops[1])?)
        };
        let av = self.vvar(addr_reg);
        let a = self.load_u32(av);
        let off = self.cu(i.mods.offset as u32);
        let byte = self.op2(OP_IADD, Sc::U32, a, off);
        let c2 = self.cu(2);
        let widx = self.op2(OP_SHR, Sc::U32, byte, c2);
        let u = self.spv.ty(Sc::U32, 1);
        let pt = self.spv.ptr(4, u);
        let mut vals = Vec::new();
        if !is_read {
            for k in 0..width {
                let v = self.vvar(data_reg + k);
                vals.push(self.load_u32(v));
            }
        }
        self.guarded(|t| {
            for k in 0..width {
                let kc = t.cu(k as u32);
                let idx = t.op2(OP_IADD, Sc::U32, widx, kc);
                let ptr = t.spv.emit(65, pt, &[lds, idx.id]);
                match vals.get(k) {
                    Some(v) => t.spv.emit_void(62, &[ptr, v.id]),
                    None => {
                        let w = t.load_ptr_u32(ptr);
                        t.write_v(data_reg + k, w);
                    }
                }
            }
            Ok(())
        })
    }

    fn lds_var(&mut self) -> u32 {
        if let Some(v) = self.lds {
            return v;
        }
        let u = self.spv.ty(Sc::U32, 1);
        let n = self.spv.c_u32(self.cfg.lds_dwords);
        let arr = self.spv.array(u, n);
        let pt = self.spv.ptr(4, arr);
        let var = self.spv.global_var(pt, 4, None);
        self.lds = Some(var);
        var
    }

    // ------------------------------------------------------------ exports
    fn output_vec4(&mut self, class_loc: Option<u32>, builtin: Option<u32>) -> u32 {
        let t = self.spv.ty(Sc::F32, 4);
        let pt = self.spv.ptr(3, t);
        let init = self.spv.c_null(t);
        let var = self.spv.global_var(pt, 3, Some(init));
        if let Some(b) = builtin {
            self.spv.decorate(var, 11, &[b]);
        }
        if let Some(l) = class_loc {
            self.spv.decorate(var, 30, &[l]);
        }
        self.interface.push(var);
        var
    }

    pub(crate) fn export(&mut self, i: &Instruction) -> TResult<()> {
        let m = &i.mods;
        let target = u32::from(m.target);
        let stage = self.cfg.stage;
        // quatre composantes f32 optionnelles
        let mut comps: [Option<Val>; 4] = [None; 4];
        if m.compr {
            for pair in 0..2usize {
                let Src::Vgpr(r) = i.operands[pair * 2].src else {
                    continue;
                };
                let var = self.vvar(usize::from(r));
                let w = self.load_u32(var);
                let v2 = self.spv.glsl_op(Sc::F32, 2, 62, &[w.id]);
                for half in 0..2usize {
                    let c = pair * 2 + half;
                    if m.en >> c & 1 != 0 {
                        comps[c] = Some(self.spv.extract(v2, half as u32));
                    }
                }
            }
        } else {
            for (c, slot) in comps.iter_mut().enumerate() {
                if m.en >> c & 1 != 0
                    && let Src::Vgpr(r) = i.operands[c].src
                {
                    let var = self.vvar(usize::from(r));
                    let w = self.load_u32(var);
                    *slot = Some(self.spv.bitcast(w, Sc::F32));
                }
            }
        }
        match target {
            9 => Ok(()), // null
            12 => {
                if stage != Stage::Vertex {
                    return unsupported(i);
                }
                let var = match self.position {
                    Some(v) => v,
                    None => {
                        let v = self.output_vec4(None, Some(0));
                        self.position = Some(v);
                        v
                    }
                };
                self.store_vec4(var, comps)
            }
            13..=15 => Ok(()), // pos1..3 (point size, clip distances) : ignorés
            32..=63 => {
                if stage != Stage::Vertex {
                    return unsupported(i);
                }
                let loc = target - 32;
                let var = match self.params.get(&loc) {
                    Some(&v) => v,
                    None => {
                        let v = self.output_vec4(Some(loc), None);
                        self.params.insert(loc, v);
                        v
                    }
                };
                self.store_vec4(var, comps)
            }
            0..=7 => {
                if stage != Stage::Fragment {
                    return unsupported(i);
                }
                let var = match self.mrts.get(&target) {
                    Some(&v) => v,
                    None => {
                        let v = self.output_vec4(Some(target), None);
                        self.mrts.insert(target, v);
                        v
                    }
                };
                self.store_vec4(var, comps)
            }
            8 => {
                // mrtz : profondeur dans la composante x
                if stage != Stage::Fragment {
                    return unsupported(i);
                }
                let Some(z) = comps[0] else { return Ok(()) };
                let var = match self.depth {
                    Some(v) => v,
                    None => {
                        let t = self.spv.ty(Sc::F32, 1);
                        let pt = self.spv.ptr(3, t);
                        let v = self.spv.global_var(pt, 3, None);
                        self.spv.decorate(v, 11, &[22]);
                        self.interface.push(v);
                        self.depth = Some(v);
                        v
                    }
                };
                self.guarded(|t| {
                    t.spv.emit_void(62, &[var, z.id]);
                    Ok(())
                })
            }
            _ => unsupported(i),
        }
    }

    /// Écrit les composantes actives dans un `vec4` de sortie en préservant les autres.
    fn store_vec4(&mut self, var: u32, comps: [Option<Val>; 4]) -> TResult<()> {
        self.guarded(|t| {
            let ty = t.spv.ty(Sc::F32, 4);
            let mut cur = t.spv.emit(61, ty, &[var]);
            for (c, v) in comps.iter().enumerate() {
                if let Some(v) = v {
                    cur = t.spv.emit(82, ty, &[v.id, cur, c as u32]);
                }
            }
            t.spv.emit_void(62, &[var, cur]);
            Ok(())
        })
    }

    // ------------------------------------------------------------ interpolation
    pub(crate) fn vintrp(&mut self, i: &Instruction) -> TResult<()> {
        if self.cfg.stage != Stage::Fragment {
            return unsupported(i);
        }
        let name = i.base_name();
        if name == "v_interp_p2_f32" {
            // p1 a déjà produit la valeur interpolée complète (Vulkan interpole pour nous).
            return Ok(());
        }
        if name != "v_interp_p1_f32" && name != "v_interp_mov_f32" {
            return unsupported(i);
        }
        let attr = u32::from(i.mods.attr);
        let var = match self.attrs.get(&attr) {
            Some(&v) => v,
            None => {
                let t = self.spv.ty(Sc::F32, 4);
                let pt = self.spv.ptr(1, t);
                let v = self.spv.global_var(pt, 1, None);
                self.spv.decorate(v, 30, &[attr]);
                self.interface.push(v);
                self.attrs.insert(attr, v);
                v
            }
        };
        let ty = self.spv.ty(Sc::F32, 4);
        let id = self.spv.emit(61, ty, &[var]);
        let c = self.spv.extract(
            Val {
                id,
                sc: Sc::F32,
                n: 4,
            },
            u32::from(i.mods.attr_chan),
        );
        let d = vgpr_of(&i.operands[0])?;
        self.write_v(d, c);
        Ok(())
    }

    // ------------------------------------------------------------ MIMG
    pub(crate) fn mimg(&mut self, i: &Instruction) -> TResult<()> {
        let name = i.name;
        let m = &i.mods;
        if m.a16 || m.d16 || m.tfe || m.lwe {
            return unsupported(i);
        }
        // dimension -> (Dim SPIR-V, arrayed, composantes)
        let (dim, arrayed, ncoord) = match m.dim {
            0 => (0u32, 0u32, 1usize),
            1 => (1, 0, 2),
            2 => (2, 0, 3),
            3 => (3, 0, 3),
            4 => (0, 1, 2),
            5 => (1, 1, 3),
            _ => return unsupported(i),
        };
        let (is_sample, is_load) = (
            name.starts_with("image_sample"),
            name == "image_load" || name == "image_load_mip",
        );
        if !is_sample && !is_load {
            return unsupported(i);
        }
        let ops = &i.operands;
        let n = ops.len();
        let n_trailer = if is_sample { 2 } else { 1 };
        let (vdata, addr_ops) = (ops[0], &ops[1..n - n_trailer]);
        let rsrc = ops[n - n_trailer];
        // registres d'adresse
        let mut regs = Vec::new();
        for o in addr_ops {
            let r = vgpr_of(o)?;
            for k in 0..usize::from(o.width.max(1)) {
                regs.push(r + k);
            }
        }
        let mut next = regs.into_iter();
        // image
        let rn = Self::sgpr_index(&rsrc)?;
        let Some(img_res) = self
            .res_map
            .get(&rn)
            .copied()
            .filter(|r| r.kind == ResourceKind::Image)
        else {
            return err(format!("aucun T# connu dans s[{rn}:{}]", rn + 7));
        };
        let (img_var, img_ty) = self.image_var(img_res.set, img_res.binding, dim, arrayed)?;
        let nvals = m.dmask.count_ones() as usize;
        if nvals == 0 {
            return Ok(());
        }
        let d0 = vgpr_of(&vdata)?;
        let f32t = self.spv.ty(Sc::F32, 1);
        let v4 = self.spv.ty(Sc::F32, 4);

        if is_load {
            let mut coords = Vec::new();
            for _ in 0..ncoord {
                let r = next.next().ok_or_else(|| {
                    TranslateError::Unsupported(format!("{name} : adresse trop courte"))
                })?;
                let var = self.vvar(r);
                let v = self.load_u32(var);
                coords.push(self.spv.bitcast(v, Sc::I32).id);
            }
            let lod = if name == "image_load_mip" {
                let r = next
                    .next()
                    .ok_or_else(|| TranslateError::Unsupported(format!("{name} : lod manquant")))?;
                let var = self.vvar(r);
                let v = self.load_u32(var);
                self.spv.bitcast(v, Sc::I32)
            } else {
                Val {
                    id: self.spv.c_i32(0),
                    sc: Sc::I32,
                    n: 1,
                }
            };
            let coord = self.spv.construct(Sc::I32, &coords);
            let imgt = self.spv.ty(Sc::F32, 1);
            let _ = imgt;
            let img = self.spv.emit(61, img_ty, &[img_var]);
            let px = self.spv.emit(95, v4, &[img, coord.id, 2, lod.id]);
            return self.write_dmask(
                m.dmask,
                d0,
                Val {
                    id: px,
                    sc: Sc::F32,
                    n: 4,
                },
            );
        }

        // --- échantillonnage : décoder les suffixes
        let rest = name.strip_prefix("image_sample").unwrap_or("");
        let toks: Vec<&str> = rest.split('_').filter(|t| !t.is_empty()).collect();
        let (mut has_c, mut has_b, mut has_l, mut has_lz) = (false, false, false, false);
        for t in &toks {
            match *t {
                "c" => has_c = true,
                "b" => has_b = true,
                "l" => has_l = true,
                "lz" => has_lz = true,
                _ => return unsupported(i),
            }
        }
        let take = |t: &mut Self, nx: &mut std::vec::IntoIter<usize>| -> TResult<Val> {
            let r = nx.next().ok_or_else(|| {
                TranslateError::Unsupported(format!("{name} : adresse trop courte"))
            })?;
            let var = t.vvar(r);
            let v = t.load_u32(var);
            Ok(t.spv.bitcast(v, Sc::F32))
        };
        let bias = if has_b {
            Some(take(self, &mut next)?)
        } else {
            None
        };
        let dref = if has_c {
            Some(take(self, &mut next)?)
        } else {
            None
        };
        let mut coords = Vec::new();
        for _ in 0..ncoord {
            coords.push(take(self, &mut next)?.id);
        }
        let lod = if has_l {
            Some(take(self, &mut next)?)
        } else {
            None
        };
        let coord = self.spv.construct(Sc::F32, &coords);
        // échantillonneur
        let sn = Self::sgpr_index(&ops[n - 1])?;
        let Some(smp_res) = self
            .res_map
            .get(&sn)
            .copied()
            .filter(|r| r.kind == ResourceKind::Sampler)
        else {
            return err(format!("aucun S# connu dans s[{sn}:{}]", sn + 3));
        };
        let smp_var = self.sampler_var(smp_res.set, smp_res.binding);
        let samp_ty = self.spv.raw_type(26, &[]);
        let si_ty = self.spv.raw_type(27, &[img_ty]);
        let img = self.spv.emit(61, img_ty, &[img_var]);
        let smp = self.spv.emit(61, samp_ty, &[smp_var]);
        let si = self.spv.emit(86, si_ty, &[img, smp]);

        let implicit = self.cfg.stage == Stage::Fragment && !has_l && !has_lz;
        let res_ty = if has_c { f32t } else { v4 };
        let zero = self.spv.c_f32(0.0);
        let px = match (dref, implicit) {
            (None, true) => match bias {
                Some(b) => self.spv.emit(87, res_ty, &[si, coord.id, 1, b.id]),
                None => self.spv.emit(87, res_ty, &[si, coord.id]),
            },
            (None, false) => {
                let l = lod.map(|v| v.id).unwrap_or(zero);
                self.spv.emit(88, res_ty, &[si, coord.id, 2, l])
            }
            (Some(d), true) => match bias {
                Some(b) => self.spv.emit(89, res_ty, &[si, coord.id, d.id, 1, b.id]),
                None => self.spv.emit(89, res_ty, &[si, coord.id, d.id]),
            },
            (Some(d), false) => {
                let l = lod.map(|v| v.id).unwrap_or(zero);
                self.spv.emit(90, res_ty, &[si, coord.id, d.id, 2, l])
            }
        };
        if has_c {
            // un seul canal : réplique la valeur de comparaison dans le premier registre
            return self.write_dmask(
                m.dmask,
                d0,
                Val {
                    id: px,
                    sc: Sc::F32,
                    n: 1,
                },
            );
        }
        self.write_dmask(
            m.dmask,
            d0,
            Val {
                id: px,
                sc: Sc::F32,
                n: 4,
            },
        )
    }

    /// Écrit les composantes de `px` sélectionnées par `dmask` dans des VGPR consécutifs.
    fn write_dmask(&mut self, dmask: u8, d0: usize, px: Val) -> TResult<()> {
        let mut k = 0usize;
        for c in 0..4u32 {
            if dmask >> c & 1 == 0 {
                continue;
            }
            let v = if px.n == 1 {
                px
            } else {
                self.spv.extract(px, c)
            };
            self.write_v(d0 + k, v);
            k += 1;
        }
        Ok(())
    }

    fn image_var(&mut self, set: u32, binding: u32, dim: u32, arrayed: u32) -> TResult<(u32, u32)> {
        let f32t = self.spv.ty(Sc::F32, 1);
        if dim == 0 {
            self.spv.caps.insert(43); // Sampled1D
        }
        let img_ty = self.spv.raw_type(25, &[f32t, dim, 0, arrayed, 0, 1, 0]);
        if let Some(&(v, t, d)) = self.images.get(&(set, binding)) {
            if t != img_ty || u32::from(d) != dim {
                return err(format!(
                    "la liaison ({set},{binding}) est utilisée avec plusieurs dimensions d'image"
                ));
            }
            return Ok((v, t));
        }
        let pt = self.spv.ptr(0, img_ty);
        let var = self.spv.global_var(pt, 0, None);
        self.spv.decorate(var, 34, &[set]);
        self.spv.decorate(var, 33, &[binding]);
        self.images.insert((set, binding), (var, img_ty, dim as u8));
        self.info.bindings.push(BindingUse {
            set,
            binding,
            kind: "sampled_image",
        });
        Ok((var, img_ty))
    }

    fn sampler_var(&mut self, set: u32, binding: u32) -> u32 {
        if let Some(&v) = self.samplers.get(&(set, binding)) {
            return v;
        }
        let st = self.spv.raw_type(26, &[]);
        let pt = self.spv.ptr(0, st);
        let var = self.spv.global_var(pt, 0, None);
        self.spv.decorate(var, 34, &[set]);
        self.spv.decorate(var, 33, &[binding]);
        self.samplers.insert((set, binding), var);
        self.info.bindings.push(BindingUse {
            set,
            binding,
            kind: "sampler",
        });
        var
    }
}

/// `s_load_dword{,x2,x4,x8,x16}` / `s_buffer_load_*` : nombre de dwords.
fn dword_count_smem(name: &str) -> Option<usize> {
    let tail = name
        .strip_prefix("s_buffer_load_")
        .or_else(|| name.strip_prefix("s_load_"))?;
    match tail {
        "dword" => Some(1),
        "dwordx2" => Some(2),
        "dwordx4" => Some(4),
        "dwordx8" => Some(8),
        "dwordx16" => Some(16),
        _ => None,
    }
}
