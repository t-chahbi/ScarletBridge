//! Traduction des accès mémoire, exports et interpolation : SMEM, MUBUF/MTBUF, MIMG (échantillonnage
//! de base), DS (LDS), `exp` et `v_interp_*`.

use super::inst::{Instruction, Operand, Src};
use super::tables::TBUF_FORMATS;
use super::translate::{
    DescKind, ResRef, ResourceKind, ResourceLoc, Stage, TResult, TableBindingUse, Tr,
    TranslateError, res_ref, unsupported,
};
use crate::dxbc_to_spirv::spv::{Sc, Val};

const OP_IADD: u16 = 128;
const OP_IMUL: u16 = 132;
const OP_AND: u16 = 199;
const OP_SHR: u16 = 194;
const OP_BFSEXT: u16 = 202;
const OP_BFUEXT: u16 = 203;

pub(crate) fn err<T>(msg: impl Into<String>) -> TResult<T> {
    Err(TranslateError::Unresolved(msg.into()))
}

/// Opérande registre vectoriel : index du VGPR de base.
pub(crate) fn vgpr_of(o: &Operand) -> TResult<usize> {
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
    pub(crate) fn sgpr_index(o: &Operand) -> TResult<u8> {
        match o.src {
            Src::Sgpr(n) => Ok(n),
            Src::Ttmp(n) => Ok(108 + n),
            other => err(format!(
                "descripteur attendu dans des SGPR, trouvé {other:?}"
            )),
        }
    }

    fn buffer_res(&mut self, o: &Operand) -> TResult<(ResRef, u32)> {
        let r = self.resolve_desc(o, DescKind::Buffer)?;
        match r.kind {
            ResourceKind::Buffer { stride } => Ok((r, stride)),
            _ => err(format!("{}: descripteur de buffer attendu", self.cur_name)),
        }
    }

    /// Explique pourquoi les SGPR `[n, n+words)` ne contiennent pas de descripteur résolu.
    fn explain_sgpr(&self, n: u8, words: u8) -> String {
        let mut why = Vec::new();
        for k in n..n.saturating_add(words) {
            let alias = self.sgpr_alias.get(&k);
            let origin = self.desc_words.get(&k);
            why.push(match (origin, alias) {
                (Some((t, off)), _) => format!("s{k}=mot de table s[{t}:{}]+{off:#x}", t + 1),
                (None, Some((o, a))) => format!("s{k}=alias de s{o}+{a:#x} (pas un descripteur)"),
                (None, None) => match self.last_writer.get(&k) {
                    Some(w) => {
                        format!("s{k} écrit en dernier par `{w}` (valeur non constante/inconnue)")
                    }
                    None => format!("s{k} jamais chargé par un s_load depuis une table"),
                },
            });
        }
        let tables: Vec<String> = self
            .cfg
            .tables
            .iter()
            .map(|t| format!("s[{}:{}]", t.sgpr, t.sgpr + 1))
            .collect();
        format!(
            "{} ; tables déclarées : {}",
            why.join(", "),
            if tables.is_empty() {
                "aucune".into()
            } else {
                tables.join(" ")
            }
        )
    }

    /// Résout le descripteur de nature `kind` dont le premier SGPR est celui de `o` : ressource
    /// déclarée (`Resource`), sinon entrée d'une table déclarée (`TranslateConfig::tables`) avec une
    /// liaison attribuée automatiquement `(set, binding_base + offset/16)`.
    pub(crate) fn resolve_desc(&mut self, o: &Operand, kind: DescKind) -> TResult<ResRef> {
        let n = Self::sgpr_index(o)?;
        let words = kind.dwords();
        let ctx = self.cur_name;
        if let Some(r) = self.res_map.get(&n).copied() {
            let ok = matches!(
                (r.kind, kind),
                (ResourceKind::Buffer { .. }, DescKind::Buffer)
                    | (ResourceKind::Image, DescKind::Image)
                    | (ResourceKind::Sampler, DescKind::Sampler)
            );
            return if ok {
                Ok(r)
            } else {
                err(format!(
                    "{ctx} : s[{n}:{}] est un descripteur {:?} déclaré, {} attendu",
                    n + words - 1,
                    r.kind,
                    kind.label()
                ))
            };
        }
        let Some(&(t, off)) = self.desc_words.get(&n) else {
            return err(format!(
                "{ctx} : {} dans s[{n}:{}] non résolu : {}",
                kind.label(),
                n + words - 1,
                self.explain_sgpr(n, words)
            ));
        };
        for j in 1..words {
            if self.desc_words.get(&(n + j)) != Some(&(t, off + 4 * i64::from(j))) {
                return err(format!(
                    "{ctx} : {} dans s[{n}:{}] mal formé : les mots ne viennent pas d'offsets contigus de la table s[{t}:{}] ({})",
                    kind.label(),
                    n + words - 1,
                    t + 1,
                    self.explain_sgpr(n, words)
                ));
            }
        }
        let Some(tbl) = self.cfg.tables.iter().find(|x| x.sgpr == t).copied() else {
            return err(format!("{ctx} : table s[{t}:{}] non déclarée", t + 1));
        };
        if off < 0 || off % 16 != 0 {
            return err(format!(
                "{ctx} : descripteur à l'offset {off:#x} de la table s[{t}:{}] : l'offset doit être un multiple de 16 positif",
                t + 1
            ));
        }
        let binding = tbl.binding_base + (off / 16) as u32;
        let tag = match kind {
            DescKind::Buffer => "storage_buffer",
            DescKind::Image => "sampled_image",
            DescKind::Sampler => "sampler",
        };
        if let Some(prev) = self
            .info
            .table_bindings
            .iter()
            .find(|b| b.table_sgpr == t && i64::from(b.byte_offset) == off)
        {
            let same = (prev.kind == "storage_buffer") == (kind == DescKind::Buffer)
                && (prev.kind == "sampler") == (kind == DescKind::Sampler);
            if !same {
                return err(format!(
                    "{ctx} : l'entrée {off:#x} de la table s[{t}:{}] est utilisée avec deux natures ({} puis {})",
                    t + 1,
                    prev.kind,
                    tag
                ));
            }
        } else {
            self.info.table_bindings.push(TableBindingUse {
                table_sgpr: t,
                byte_offset: off as u32,
                kind: tag,
                set: tbl.set,
                binding,
            });
        }
        Ok(ResRef {
            kind: match kind {
                DescKind::Buffer => ResourceKind::Buffer {
                    stride: tbl.buffer_stride,
                },
                DescKind::Image => ResourceKind::Image,
                DescKind::Sampler => ResourceKind::Sampler,
            },
            set: tbl.set,
            binding,
            size: words,
        })
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
        // s_load : base = paire de SGPR (suivi symbolique `origine + offset constant` via
        // `s_mov_b32/b64`, `s_add_u32`/`s_sub_u32` + `s_addc_u32`, `s_addk_i32`).
        let n = Self::sgpr_index(&base)?;
        let (orig, add_off) = self.sgpr_alias.get(&n).copied().unwrap_or((n, 0));
        let eff_imm = i64::from(imm).saturating_add(add_off);
        // soffset constant (inline/littéral/null) ou inconnu
        let const_soff: Option<i64> = if i.mods.smem_soffset {
            match i.operands.get(2).map(|o| o.src) {
                Some(Src::InlineInt(v)) => Some(i64::from(v)),
                Some(Src::Literal(v)) => Some(i64::from(v)),
                Some(Src::Null) => Some(0),
                _ => None,
            }
        } else {
            Some(0)
        };
        let cfg = self.cfg;
        // 1. pointeur vers un buffer de données
        for r in &cfg.resources {
            if let (ResourceLoc::Pointer(p), ResourceKind::Buffer { .. }) = (r.loc, r.kind)
                && (p == n || p == orig)
            {
                return self.smem_read_words(r.set, r.binding, eff_imm as i32, soff, dcode, count);
            }
        }
        // 2. table de descripteurs : on associe les SGPR de destination aux ressources chargées
        let lo = eff_imm.saturating_add(const_soff.unwrap_or(0));
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
                if (table_sgpr == n || table_sgpr == orig)
                    && off >= lo
                    && off + sz <= hi
                    && const_soff.is_some()
                {
                    let sgpr = dcode + ((off - lo) / 4) as usize;
                    self.res_map.insert(sgpr as u8, res_ref(r));
                    found = true;
                }
            }
        }
        // 3. table déclarée (`TranslateConfig::tables`) : les mots chargés sont des mots de
        //    descripteurs ; leur nature (V#/T#/S#) est décidée à l'usage.
        if let Some(tbl) = cfg.tables.iter().find(|t| t.sgpr == orig || t.sgpr == n) {
            let Some(cs) = const_soff else {
                return err(format!(
                    "{name} depuis s[{n}:{}] (base = table s[{}:{}] + {add_off:#x}, imm={imm:#x}) : offset SGPR/non constant (soffset={:?}), impossible de choisir l'entrée de table",
                    n + 1,
                    tbl.sgpr,
                    tbl.sgpr + 1,
                    i.operands.get(2).map(|o| o.src)
                ));
            };
            let start = eff_imm.saturating_add(cs);
            if start < 0 || start % 4 != 0 {
                return err(format!(
                    "{name} depuis la table s[{}:{}] : offset {start} invalide (doit être positif et multiple de 4)",
                    tbl.sgpr,
                    tbl.sgpr + 1
                ));
            }
            for k in 0..count {
                let code = dcode + k;
                if let Ok(c8) = u8::try_from(code) {
                    self.desc_words.insert(c8, (tbl.sgpr, start + 4 * k as i64));
                }
                let v = self.cu(0);
                let var = self.svar(code);
                self.store_u32(var, v);
            }
            return Ok(());
        }
        if found {
            return Ok(());
        }
        let writer = |t: &Self, r: u8| {
            t.last_writer
                .get(&r)
                .map_or("jamais écrit".to_string(), |w| {
                    format!("écrit en dernier par `{w}`")
                })
        };
        let ptrs: Vec<String> = cfg
            .resources
            .iter()
            .filter_map(|r| match r.loc {
                ResourceLoc::Pointer(p) => Some(format!("s[{p}:{}]", p + 1)),
                _ => None,
            })
            .collect();
        let tbls: Vec<String> = cfg
            .tables
            .iter()
            .map(|t| format!("s[{}:{}]", t.sgpr, t.sgpr + 1))
            .collect();
        err(format!(
            "{name} depuis s[{n}:{}] (origine symbolique s{orig}{}, imm={imm:#x}, {count} dwords) : base ni pointeur de buffer ni table de descripteurs déclaré (s{n} {}, s{} {}; pointeurs : {}; tables : {})",
            n + 1,
            if orig == n && add_off == 0 {
                " [aucun alias connu]".to_string()
            } else {
                format!(" + {add_off:#x}")
            },
            writer(self, n),
            n + 1,
            writer(self, n + 1),
            if ptrs.is_empty() {
                "aucun".into()
            } else {
                ptrs.join(" ")
            },
            if tbls.is_empty() {
                "aucune".into()
            } else {
                tbls.join(" ")
            },
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
            let (idx, okc) = match self.buffer_limit(set, binding) {
                Some(l) => {
                    let ok = self.dword_in_bounds(byte, k as u32, l);
                    let z = self.cu(0);
                    (self.select(ok, idx, z), Some(ok))
                }
                None => (idx, None),
            };
            let ptr = self.buffer_word_ptr(set, binding, idx);
            let mut v = self.load_ptr_u32(ptr);
            if let Some(ok) = okc {
                let z = self.cu(0);
                v = self.select(ok, v, z);
            }
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
                    return err(format!(
                        "{name} : accès idxen sur un buffer de stride 0 (déclarez `Resource::Buffer {{ stride }}` ou `DescriptorTable::buffer_stride`)"
                    ));
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
        let limit = self.buffer_limit(set, binding);
        let base = name
            .strip_prefix("tbuffer_")
            .or_else(|| name.strip_prefix("buffer_"))
            .unwrap_or(name);

        // --- atomiques
        if let Some(op) = base.strip_prefix("atomic_") {
            let ok = limit.map(|l| self.dword_in_bounds(addr, 0, l));
            return self.buffer_atomic(i, op, data, set, binding, widx, ok);
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
                "byte" | "short" | "byte_d16_hi" | "short_d16_hi" => (1, 1, None),
                _ => return unsupported(i),
            }
        };
        let d0 = match data {
            Some(d) => vgpr_of(&d)?,
            None => return unsupported(i),
        };
        if is_store {
            if kind == "byte" || kind == "short" || kind == "byte_d16_hi" || kind == "short_d16_hi"
            {
                // lecture-modification-écriture atomique du dword contenant
                let bits = if kind.starts_with("byte") { 8u32 } else { 16 };
                let var = self.vvar(d0);
                let mut v = self.load_u32(var);
                if kind.ends_with("_hi") {
                    let c16 = self.cu(16);
                    v = self.op2(194, Sc::U32, v, c16);
                }
                let ok = limit.map(|l| self.dword_in_bounds(addr, 0, l));
                return self.guarded_with(ok, |t| {
                    let ptr = t.buffer_word_ptr(set, binding, widx);
                    let (c3, c8, mask) = (
                        t.cu(if bits == 8 { 3 } else { 2 }),
                        t.cu(8),
                        t.cu(if bits == 8 { 0xFF } else { 0xFFFF }),
                    );
                    let lowb = t.op2(OP_AND, Sc::U32, addr, c3);
                    let sh = t.op2(OP_IMUL, Sc::U32, lowb, c8);
                    let mv = t.op2(OP_AND, Sc::U32, v, mask);
                    let vs = t.op2(196, Sc::U32, mv, sh);
                    let ms = t.op2(196, Sc::U32, mask, sh);
                    let inv = t.op1(200, Sc::U32, ms);
                    let (scope, sem) = (t.spv.c_u32(1), t.spv.c_u32(0));
                    let u = t.spv.ty(Sc::U32, 1);
                    t.spv.emit(240, u, &[ptr, scope, sem, inv.id]);
                    t.spv.emit(241, u, &[ptr, scope, sem, vs.id]);
                    Ok(())
                });
            }
            if small.is_some() {
                return unsupported(i);
            }
            let mut vals = Vec::new();
            for c in 0..ncomp.min(mem_comps) {
                let var = self.vvar(d0 + c);
                vals.push(self.load_u32(var));
            }
            let mut ok = None;
            if let Some(l) = limit {
                let mut all = self.dword_in_bounds(addr, 0, l);
                for c in 1..vals.len() {
                    let o = self.dword_in_bounds(addr, c as u32, l);
                    all = self.bool_and(all, o);
                }
                ok = Some(all);
            }
            return self.guarded_with(ok, |t| {
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
                    let (idx, okc) = match limit {
                        Some(l) => {
                            let ok = t.dword_in_bounds(addr, c as u32, l);
                            let z = t.cu(0);
                            (t.select(ok, idx, z), Some(ok))
                        }
                        None => (idx, None),
                    };
                    let ptr = t.buffer_word_ptr(set, binding, idx);
                    let mut w = t.load_ptr_u32(ptr);
                    if let Some(ok) = okc {
                        let z = t.cu(0);
                        w = t.select(ok, w, z);
                    }
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

    #[allow(clippy::too_many_arguments)]
    fn buffer_atomic(
        &mut self,
        i: &Instruction,
        op: &str,
        data: Option<Operand>,
        set: u32,
        binding: u32,
        widx: Val,
        ok: Option<Val>,
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
        self.guarded_with(ok, |t| {
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

    pub(crate) fn lds_var(&mut self) -> u32 {
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
