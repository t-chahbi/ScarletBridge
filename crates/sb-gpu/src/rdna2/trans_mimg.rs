//! MIMG : échantillonnage (`image_sample*` avec `_c _b _l _lz _cl _o _d _cd`), `image_gather4*`,
//! `image_get_lod`, `image_get_resinfo`, `image_load[_mip]`, `image_store`, `image_atomic_*`.
//!
//! Dimensions : 1D, 2D, 3D, cube, 1D/2D arrays ; MSAA (2D_MSAA[_ARRAY]) pour `image_load`.
//! `d16` (résultats empaquetés en demi-flottants), `a16` (coordonnées 16 bits), `tfe`/`lwe`
//! (le registre de statut reçoit 0 = « résident ») sont supportés. Hors périmètre : dérivées
//! `_g16`, `r128`, cube arrays, `image_load/store_pck`, `image_bvh_*`.
//!
//! Capacités SPIR-V selon l'usage : `Sampled1D`, `ImageQuery`, `ImageGatherExtended` (offsets
//! dynamiques), `MinLod`, `StorageImageWriteWithoutFormat` (`image_store`).

use super::inst::Instruction;
use super::trans_mem::{err, vgpr_of};
use super::translate::{BindingUse, ResourceKind, Stage, TResult, Tr, TranslateError, unsupported};
use crate::dxbc_to_spirv::spv::{Sc, Val};

const OP_BFSEXT: u16 = 202;

#[derive(Clone, Copy)]
struct DimInfo {
    spv_dim: u32,
    arrayed: u32,
    ms: u32,
    /// Nombre de composantes d'adresse (hors indice d'échantillon MSAA).
    ncoord: usize,
    /// Dimensions de dérivées / d'offset (sans couche).
    grad: usize,
}

fn dim_info(dim: u8) -> Option<DimInfo> {
    let d = |spv_dim, arrayed, ms, ncoord, grad| DimInfo {
        spv_dim,
        arrayed,
        ms,
        ncoord,
        grad,
    };
    Some(match dim {
        0 => d(0, 0, 0, 1, 1),
        1 => d(1, 0, 0, 2, 2),
        2 => d(2, 0, 0, 3, 3),
        3 => d(3, 0, 0, 3, 2),
        4 => d(0, 1, 0, 2, 1),
        5 => d(1, 1, 0, 3, 2),
        6 => d(1, 0, 1, 2, 2),
        7 => d(1, 1, 1, 3, 2),
        _ => return None,
    })
}

/// Flux de registres d'adresse (vaddr) avec lecture typée.
struct Addr {
    regs: std::vec::IntoIter<usize>,
    name: String,
}

impl Addr {
    fn next_reg(&mut self) -> TResult<usize> {
        self.regs.next().ok_or_else(|| {
            TranslateError::Unsupported(format!("{} : adresse trop courte", self.name))
        })
    }
}

impl Tr<'_> {
    fn addr_bits(&mut self, a: &mut Addr) -> TResult<Val> {
        let r = a.next_reg()?;
        let v = self.vvar(r);
        Ok(self.load_u32(v))
    }

    fn addr_f32(&mut self, a: &mut Addr) -> TResult<Val> {
        let b = self.addr_bits(a)?;
        Ok(self.spv.bitcast(b, Sc::F32))
    }

    /// `n` coordonnées flottantes (empaquetées par paires de f16 si `a16`).
    fn addr_coords_f(&mut self, a: &mut Addr, n: usize, a16: bool) -> TResult<Vec<u32>> {
        let mut out = Vec::new();
        if a16 {
            let mut k = 0;
            while k < n {
                let b = self.addr_bits(a)?;
                for half in 0..2 {
                    if k < n {
                        out.push(self.half_of(b, half == 1).id);
                        k += 1;
                    }
                }
            }
        } else {
            for _ in 0..n {
                out.push(self.addr_f32(a)?.id);
            }
        }
        Ok(out)
    }

    fn addr_coords_i(&mut self, a: &mut Addr, n: usize, a16: bool) -> TResult<Vec<u32>> {
        let mut out = Vec::new();
        if a16 {
            let mut k = 0;
            while k < n {
                let b = self.addr_bits(a)?;
                for half in 0..2 {
                    if k < n {
                        let e = self.ext16(b, half == 1, false);
                        out.push(self.spv.bitcast(e, Sc::I32).id);
                        k += 1;
                    }
                }
            }
        } else {
            for _ in 0..n {
                let b = self.addr_bits(a)?;
                out.push(self.spv.bitcast(b, Sc::I32).id);
            }
        }
        Ok(out)
    }

    pub(crate) fn image_var(
        &mut self,
        set: u32,
        binding: u32,
        d: DimInfoPub,
        storage: u32,
    ) -> TResult<(u32, u32)> {
        // storage : 1 = échantillonnée, 2 = stockage (format inconnu), 3 = stockage r32ui
        let (sampled_ty, fmt, sampled) = match storage {
            3 => (self.spv.ty(Sc::U32, 1), 24, 2),
            2 => (self.spv.ty(Sc::F32, 1), 0, 2),
            _ => (self.spv.ty(Sc::F32, 1), 0, 1),
        };
        if d.spv_dim == 0 {
            self.spv.caps.insert(if sampled == 2 { 45 } else { 43 }); // Image1D / Sampled1D
        }
        let img_ty = self.spv.raw_type(
            25,
            &[sampled_ty, d.spv_dim, 0, d.arrayed, d.ms, sampled, fmt],
        );
        if let Some(&(v, t, _)) = self.images.get(&(set, binding)) {
            if t != img_ty {
                return err(format!(
                    "la liaison ({set},{binding}) est utilisée avec plusieurs types d'image (dimension/usage)"
                ));
            }
            return Ok((v, t));
        }
        let pt = self.spv.ptr(0, img_ty);
        let var = self.spv.global_var(pt, 0, None);
        self.spv.decorate(var, 34, &[set]);
        self.spv.decorate(var, 33, &[binding]);
        self.images
            .insert((set, binding), (var, img_ty, d.spv_dim as u8));
        let kind = if sampled == 2 {
            "storage_image"
        } else {
            "sampled_image"
        };
        self.info.bindings.push(BindingUse { set, binding, kind });
        Ok((var, img_ty))
    }

    pub(crate) fn sampler_var(&mut self, set: u32, binding: u32) -> u32 {
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

    /// Écrit les composantes de `comps` (ordre canal) sélectionnées par `dmask`, avec `d16`/`tfe`.
    fn write_image_result(&mut self, i: &Instruction, d0: usize, comps: &[Val]) {
        let m = &i.mods;
        let mut sel: Vec<Val> = Vec::new();
        for (c, v) in comps.iter().enumerate() {
            if m.dmask >> c & 1 != 0 {
                sel.push(*v);
            }
        }
        let mut k = 0usize;
        if m.d16 {
            for pair in sel.chunks(2) {
                let lo = pair[0];
                let hi = pair.get(1).copied().unwrap_or_else(|| self.cf(0.0));
                let p = self.pack2h(lo, hi);
                self.write_v(d0 + k, p);
                k += 1;
            }
        } else {
            for v in sel {
                self.write_v(d0 + k, v);
                k += 1;
            }
        }
        if m.tfe || m.lwe {
            let z = self.cu(0);
            self.write_v(d0 + k, z);
        }
    }

    pub(crate) fn mimg(&mut self, i: &Instruction) -> TResult<()> {
        let name = i.name;
        let m = &i.mods;
        if m.r128 {
            return unsupported(i);
        }
        let Some(di) = dim_info(m.dim) else {
            return unsupported(i);
        };
        let d = DimInfoPub {
            spv_dim: di.spv_dim,
            arrayed: di.arrayed,
            ms: di.ms,
        };
        let is_sample = name.starts_with("image_sample");
        let is_gather = name.starts_with("image_gather4") && !name.starts_with("image_gather4h");
        let is_lod = name == "image_get_lod";
        let is_res = name == "image_get_resinfo";
        let is_load = name == "image_load" || name == "image_load_mip";
        let is_store = name == "image_store" || name == "image_store_mip";
        let is_atomic = name.starts_with("image_atomic_");
        if !(is_sample || is_gather || is_lod || is_res || is_load || is_store || is_atomic) {
            return unsupported(i);
        }
        let ops = &i.operands;
        let n = ops.len();
        let has_samp = is_sample || is_gather || is_lod;
        let tr = if has_samp { 2 } else { 1 };
        let (vdata, addr_ops) = (ops[0], &ops[1..n - tr]);
        let rsrc = ops[n - tr];
        let mut regs = Vec::new();
        for o in addr_ops {
            let r = vgpr_of(o)?;
            for k in 0..usize::from(o.width.max(1)) {
                regs.push(r + k);
            }
        }
        let mut a = Addr {
            regs: regs.into_iter(),
            name: name.to_string(),
        };
        let rn = Self::sgpr_index(&rsrc)?;
        let Some(res) = self
            .res_map
            .get(&rn)
            .copied()
            .filter(|r| r.kind == ResourceKind::Image)
        else {
            return err(format!("aucun T# connu dans s[{rn}:{}]", rn + 7));
        };
        let d0 = vgpr_of(&vdata)?;
        let f32t = self.spv.ty(Sc::F32, 1);
        let v4 = self.spv.ty(Sc::F32, 4);
        let storage = if is_store {
            2
        } else if is_atomic {
            3
        } else {
            1
        };
        if (is_sample || is_gather || is_lod) && di.ms != 0 {
            return unsupported(i);
        }
        let (img_var, img_ty) = self.image_var(res.set, res.binding, d, storage)?;

        // ------------------------------------------------------------ load / store / atomic
        if is_load || is_store || is_atomic {
            let mut coords = self.addr_coords_i(&mut a, di.ncoord, m.a16)?;
            let sample = if di.ms != 0 {
                let v = self.addr_bits(&mut a)?;
                Some(self.spv.bitcast(v, Sc::I32))
            } else {
                None
            };
            let coord = self.spv.construct(Sc::I32, &coords);
            coords.clear();
            let img = self.spv.emit(61, img_ty, &[img_var]);
            if is_load {
                let mut ops_ids: Vec<u32> = Vec::new();
                let mut mask = 0u32;
                if name == "image_load_mip" {
                    let l = self.addr_bits(&mut a)?;
                    let li = self.spv.bitcast(l, Sc::I32);
                    mask |= 2;
                    ops_ids.push(li.id);
                }
                if let Some(s) = sample {
                    mask |= 0x40;
                    ops_ids.push(s.id);
                }
                let mut args = vec![img, coord.id];
                if mask != 0 {
                    args.push(mask);
                    args.extend(ops_ids);
                }
                let px = self.spv.emit(95, v4, &args);
                let comps: Vec<Val> = (0..4)
                    .map(|c| {
                        self.spv.extract(
                            Val {
                                id: px,
                                sc: Sc::F32,
                                n: 4,
                            },
                            c,
                        )
                    })
                    .collect();
                self.write_image_result(i, d0, &comps);
                return Ok(());
            }
            if is_store {
                if name == "image_store_mip" {
                    return unsupported(i);
                }
                self.spv.caps.insert(56);
                let mut vals = Vec::new();
                let mut k = 0usize;
                let z = self.cf(0.0);
                for c in 0..4 {
                    if m.dmask >> c & 1 != 0 {
                        let var = self.vvar(d0 + k);
                        let b = self.load_u32(var);
                        vals.push(self.spv.bitcast(b, Sc::F32));
                        k += 1;
                    } else {
                        vals.push(z);
                    }
                }
                let ids: Vec<u32> = vals.iter().map(|v| v.id).collect();
                let texel = self.spv.construct(Sc::F32, &ids);
                return self.guarded(|t| {
                    t.spv.emit_void(99, &[img, coord.id, texel.id]);
                    Ok(())
                });
            }
            // atomique : image r32ui
            let op = &name["image_atomic_".len()..];
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
            let var = self.vvar(d0);
            let val = self.load_u32(var);
            let cmp = if code == 230 {
                let v = self.vvar(d0 + 1);
                Some(self.load_u32(v))
            } else {
                None
            };
            let returns = m.glc;
            return self.guarded(|t| {
                let u = t.spv.ty(Sc::U32, 1);
                let pt = t.spv.ptr(11, u);
                let zero = t.spv.c_u32(0);
                let texel_ptr = t.spv.emit(60, pt, &[img_var, coord.id, zero]);
                let (scope, sem) = (t.spv.c_u32(1), t.spv.c_u32(0));
                let old = match cmp {
                    Some(c) => t
                        .spv
                        .emit(230, u, &[texel_ptr, scope, sem, sem, val.id, c.id]),
                    None => t.spv.emit(code, u, &[texel_ptr, scope, sem, val.id]),
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
            });
        }

        // ------------------------------------------------------------ resinfo
        if is_res {
            self.spv.caps.insert(50);
            let lod = self.addr_bits(&mut a)?;
            let img = self.spv.emit(61, img_ty, &[img_var]);
            let nsize = di.ncoord;
            let sz_ty = self.spv.ty(Sc::I32, nsize as u32);
            let sz = self.spv.emit(103, sz_ty, &[img, lod.id]);
            let szv = Val {
                id: sz,
                sc: Sc::I32,
                n: nsize as u32,
            };
            let u = self.spv.ty(Sc::U32, 1);
            let levels = self.spv.emit(106, u, &[img]);
            let mut comps: Vec<Val> = Vec::new();
            for c in 0..nsize {
                let x = self.spv.extract(szv, c as u32);
                comps.push(self.spv.bitcast(x, Sc::U32));
            }
            let zero = self.cu(0);
            while comps.len() < 3 {
                comps.push(zero);
            }
            comps.push(Val {
                id: levels,
                sc: Sc::U32,
                n: 1,
            });
            // résultats entiers : on écrit les bits tels quels
            let mut k = 0;
            for (c, v) in comps.iter().enumerate() {
                if m.dmask >> c & 1 != 0 {
                    self.write_v(d0 + k, *v);
                    k += 1;
                }
            }
            return Ok(());
        }

        // ------------------------------------------------------------ échantillonnage / gather / lod
        let rest = if is_sample {
            &name["image_sample".len()..]
        } else if is_gather {
            &name["image_gather4".len()..]
        } else {
            ""
        };
        let (mut c, mut b, mut l, mut lz, mut cl, mut o, mut dd) =
            (false, false, false, false, false, false, false);
        for t in rest.split('_').filter(|t| !t.is_empty()) {
            match t {
                "c" => c = true,
                "b" => b = true,
                "l" => l = true,
                "lz" => lz = true,
                "cl" => cl = true,
                "o" => o = true,
                "d" | "cd" => dd = true,
                _ => return unsupported(i),
            }
        }
        if dd && (di.spv_dim == 3 || is_gather) {
            return unsupported(i);
        }
        // OpImageGather n'accepte ni Lod, ni Bias, ni MinLod
        if is_gather && (l || lz || b || cl) {
            return unsupported(i);
        }
        // ordre : offset, bias, compare, dérivées, coordonnées, lod, clamp
        let mut offset = if o {
            let v = self.addr_bits(&mut a)?;
            let mut comps = Vec::new();
            for k in 0..di.grad {
                let (sh, w) = (self.cu(8 * k as u32), self.cu(6));
                let x = self.spv.bitcast(v, Sc::I32);
                comps.push(self.spv.val(OP_BFSEXT, Sc::I32, 1, &[x.id, sh.id, w.id]).id);
            }
            let g = if comps.len() == 1 {
                comps[0]
            } else {
                self.spv.construct(Sc::I32, &comps).id
            };
            Some(Val {
                id: g,
                sc: Sc::I32,
                n: comps.len() as u32,
            })
        } else {
            None
        };
        let bias = if b {
            Some(self.addr_f32(&mut a)?)
        } else {
            None
        };
        let dref = if c {
            Some(self.addr_f32(&mut a)?)
        } else {
            None
        };
        let grads = if dd {
            let mut dx = Vec::new();
            let mut dy = Vec::new();
            for _ in 0..di.grad {
                dx.push(self.addr_f32(&mut a)?.id);
            }
            for _ in 0..di.grad {
                dy.push(self.addr_f32(&mut a)?.id);
            }
            Some((
                self.spv.construct(Sc::F32, &dx),
                self.spv.construct(Sc::F32, &dy),
            ))
        } else {
            None
        };
        let mut ids = self.addr_coords_f(&mut a, di.ncoord, m.a16)?;
        if let Some(of) = offset
            && !is_gather
        {
            // Offset dynamique interdit hors gather (VUID Offset-10213) : émulé en décalant les
            // coordonnées de `offset / taille(mip 0)` (exact pour le mip 0 ; documenté).
            self.spv.caps.insert(50); // ImageQuery
            let img0 = self.spv.emit(61, img_ty, &[img_var]);
            let nsz = di.grad as u32;
            let sz_ty = self.spv.ty(Sc::I32, nsz);
            let lod0 = self.spv.c_u32(0);
            let sz = self.spv.emit(103, sz_ty, &[img0, lod0]);
            let szv = Val {
                id: sz,
                sc: Sc::I32,
                n: nsz,
            };
            for (k, id) in ids.iter_mut().enumerate().take(di.grad) {
                let (o, z) = (
                    self.spv.extract(of, k as u32),
                    self.spv.extract(szv, k as u32),
                );
                let (of_f, sz_f) = (
                    self.spv.val(111, Sc::F32, 1, &[o.id]),
                    self.spv.val(111, Sc::F32, 1, &[z.id]),
                );
                let d = self.spv.val(136, Sc::F32, 1, &[of_f.id, sz_f.id]);
                *id = self.spv.val(129, Sc::F32, 1, &[*id, d.id]).id;
            }
            offset = None;
        }
        let coord = self.spv.construct(Sc::F32, &ids);
        let lod = if l {
            Some(self.addr_f32(&mut a)?)
        } else {
            None
        };
        let clamp = if cl {
            Some(self.addr_f32(&mut a)?)
        } else {
            None
        };

        let sn = Self::sgpr_index(&ops[n - 1])?;
        let Some(smp) = self
            .res_map
            .get(&sn)
            .copied()
            .filter(|r| r.kind == ResourceKind::Sampler)
        else {
            return err(format!("aucun S# connu dans s[{sn}:{}]", sn + 3));
        };
        let smp_var = self.sampler_var(smp.set, smp.binding);
        let samp_ty = self.spv.raw_type(26, &[]);
        let si_ty = self.spv.raw_type(27, &[img_ty]);
        let img = self.spv.emit(61, img_ty, &[img_var]);
        let sm = self.spv.emit(61, samp_ty, &[smp_var]);
        let si = self.spv.emit(86, si_ty, &[img, sm]);

        if is_lod {
            self.spv.caps.insert(50);
            let v2 = self.spv.ty(Sc::F32, 2);
            let r = self.spv.emit(105, v2, &[si, coord.id]);
            let rv = Val {
                id: r,
                sc: Sc::F32,
                n: 2,
            };
            let comps = [
                self.spv.extract(rv, 0),
                self.spv.extract(rv, 1),
                self.cf(0.0),
                self.cf(0.0),
            ];
            self.write_image_result(i, d0, &comps);
            return Ok(());
        }

        let explicit = !is_gather
            && (lod.is_some() || lz || grads.is_some() || self.cfg.stage != Stage::Fragment);
        let mut mask = 0u32;
        let mut extra: Vec<u32> = Vec::new();
        if let Some(bv) = bias {
            mask |= 1;
            extra.push(bv.id);
        }
        if explicit && grads.is_none() {
            mask |= 2;
            let lv = match lod {
                Some(v) => v.id,
                None => self.spv.c_f32(0.0),
            };
            extra.push(lv);
        }
        if let Some((dx, dy)) = grads {
            mask |= 4;
            extra.push(dx.id);
            extra.push(dy.id);
        }
        if let Some(of) = offset {
            self.spv.caps.insert(25); // ImageGatherExtended (offset dynamique, gather)
            mask |= 0x10;
            extra.push(of.id);
        }
        if let Some(cv) = clamp {
            self.spv.caps.insert(42); // MinLod
            mask |= 0x80;
            extra.push(cv.id);
        }
        let emit = |t: &mut Self, op: u16, ty: u32, mut head: Vec<u32>| -> u32 {
            if mask != 0 {
                head.push(mask);
                head.extend(extra.iter().copied());
            }
            t.spv.emit(op, ty, &head)
        };
        if is_gather {
            let chan = m.dmask.trailing_zeros().min(3);
            let r = if let Some(dr) = dref {
                emit(self, 97, v4, vec![si, coord.id, dr.id])
            } else {
                let ch = self.spv.c_u32(chan);
                emit(self, 96, v4, vec![si, coord.id, ch])
            };
            let rv = Val {
                id: r,
                sc: Sc::F32,
                n: 4,
            };
            let comps: Vec<Val> = (0..4).map(|k| self.spv.extract(rv, k)).collect();
            let mut mm = i.clone();
            mm.mods.dmask = 0xF;
            self.write_image_result(&mm, d0, &comps);
            return Ok(());
        }
        let px = match (dref, explicit) {
            (Some(dr), false) => emit(self, 89, f32t, vec![si, coord.id, dr.id]),
            (Some(dr), true) => emit(self, 90, f32t, vec![si, coord.id, dr.id]),
            (None, false) => emit(self, 87, v4, vec![si, coord.id]),
            (None, true) => emit(self, 88, v4, vec![si, coord.id]),
        };
        let comps: Vec<Val> = if dref.is_some() {
            let v = Val {
                id: px,
                sc: Sc::F32,
                n: 1,
            };
            vec![v, v, v, v]
        } else {
            let pv = Val {
                id: px,
                sc: Sc::F32,
                n: 4,
            };
            (0..4).map(|k| self.spv.extract(pv, k)).collect()
        };
        self.write_image_result(i, d0, &comps);
        Ok(())
    }
}

/// Description minimale d'une dimension d'image pour `image_var`.
#[derive(Clone, Copy)]
pub(crate) struct DimInfoPub {
    pub spv_dim: u32,
    pub arrayed: u32,
    pub ms: u32,
}
