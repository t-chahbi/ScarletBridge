//! Ressources DXIL : handles, cbuffers, textures, samplers, buffers (SRV/UAV), atomiques.
//!
//! Bindings (set 0, identiques au traducteur DXBC) : `cb#` -> `#`, `t#` (textures et buffers
//! typés) -> `8 + #`, `s#` -> `16 + #`, `u#` (images et buffers) -> `24 + #`, SRV bruts/structurés
//! -> `32 + #` (slots `0..8`, espace de registres 0).
//!
//! Tas de descripteurs SM6.6 (`ResourceDescriptorHeap[]` / `SamplerDescriptorHeap[]`) : tableaux
//! non bornés dans le **set 1** (`RuntimeDescriptorArray`, `NonUniform`) : binding 0 = samplers,
//! 1 = SRV buffers bruts/structurés, 2 = UAV buffers bruts/structurés, `16 + 3*dim + type` =
//! images échantillonnées, `48 + 3*dim + type` = images de stockage (`dim` = ordre de `Dim`,
//! `type` = float/sint/uint). Le renderer actuel ne branche pas ce set : voir
//! `TranslatedShader::descriptor_heap`.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::dxmeta::{ResClass, ResRecord, rk};
use super::module::{Const, Op, Ty, ValId, ValueKind};
use super::sp::*;
use super::tr::{Handle, R, RVal, Slot, Tr, invalid, st_of_ty, unsup};
use crate::dxbc_to_spirv::spv::Sc;
use crate::dxbc_to_spirv::{
    MAX_BINDING_SLOTS, SAMPLER_BINDING_BASE, SRV_BUFFER_BINDING_BASE, STORAGE_BINDING_BASE,
    TEXTURE_BINDING_BASE,
};

pub(super) const HEAP_SET: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Dim {
    pub spv: u32,
    pub arrayed: bool,
    pub ms: bool,
    pub coords: u32,
    pub size: u32,
    pub offs: u32,
    pub idx: u32,
    pub buffer: bool,
    pub cube: bool,
}

pub(super) fn dim_of_kind(k: u32) -> Option<Dim> {
    let d = |spv, arrayed, ms, coords, size, offs, idx| Dim {
        spv,
        arrayed,
        ms,
        coords,
        size,
        offs,
        idx,
        buffer: spv == 5,
        cube: spv == 3,
    };
    Some(match k {
        rk::TEXTURE1D => d(0, false, false, 1, 1, 1, 0),
        rk::TEXTURE2D => d(1, false, false, 2, 2, 2, 1),
        rk::TEXTURE2DMS => d(1, false, true, 2, 2, 2, 2),
        rk::TEXTURE3D => d(2, false, false, 3, 3, 3, 3),
        rk::TEXTURECUBE => d(3, false, false, 3, 2, 0, 4),
        rk::TEXTURE1DARRAY => d(0, true, false, 2, 2, 1, 5),
        rk::TEXTURE2DARRAY => d(1, true, false, 3, 3, 2, 6),
        rk::TEXTURE2DMSARRAY => d(1, true, true, 3, 3, 2, 7),
        rk::TEXTURECUBEARRAY => d(3, true, false, 4, 3, 0, 8),
        rk::TYPEDBUFFER => d(5, false, false, 1, 1, 0, 9),
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ImgInfo {
    pub ty: u32,
    pub dim: Dim,
    pub sc: Sc,
}

#[derive(Default)]
pub(super) struct ResState {
    pub cbufs: HashMap<u32, (u32, u32)>,
    pub srv_img: HashMap<u32, (u32, ImgInfo)>,
    pub uav_img: HashMap<u32, (u32, ImgInfo)>,
    pub samplers: HashMap<u32, u32>,
    pub uav_buf: HashMap<u32, u32>,
    pub srv_buf: HashMap<u32, u32>,
    pub depth_slots: HashSet<u32>,
    pub atomic_slots: HashSet<u32>,
    pub heap_imgs: HashMap<(bool, u32, u32), (u32, ImgInfo)>,
    pub heap_sampler: Option<u32>,
    pub heap_srv_buf: Option<u32>,
    pub heap_uav_buf: Option<u32>,
    pub rta: Option<u32>,
    pub striped: HashSet<u32>,
    pub uses_heap: bool,
    pub used_cb: BTreeSet<u32>,
    pub used_tex: BTreeSet<u32>,
    pub used_smp: BTreeSet<u32>,
    pub used_uav: BTreeSet<u32>,
    pub used_srv_buf: BTreeSet<u32>,
}

fn sc_of_comp(comp: u32) -> Sc {
    match comp {
        4 | 2 => Sc::I32,
        5 | 3 => Sc::U32,
        _ => Sc::F32,
    }
}

impl Tr<'_> {
    // ------------------------------------------------------------------ pré-analyse

    /// Détermine (classe, slot) d'un handle créé par `createHandle*` (à travers `annotateHandle`).
    fn origin_of(&self, id: ValId, depth: u32) -> Option<(u32, u32)> {
        if depth > 4 {
            return None;
        }
        let f = self.f;
        for b in &f.blocks {
            for i in &b.insts {
                if i.result != Some(id) {
                    continue;
                }
                let Op::Call { callee, args } = &i.op else {
                    return None;
                };
                let ValueKind::Function(fi) = f.values.get(*callee as usize)?.kind else {
                    return None;
                };
                let name = &self.m.functions[fi].name;
                if name.starts_with("dx.op.createHandle.") || name == "dx.op.createHandle" {
                    let class = self.const_u(*args.get(1)?)? as u32;
                    let idx = self.const_u(*args.get(3)?)? as u32;
                    return Some((class, idx));
                }
                if name == "dx.op.annotateHandle" {
                    return self.origin_of(*args.get(1)?, depth + 1);
                }
                if name == "dx.op.createHandleFromBinding" {
                    let ValueKind::Const(Const::Aggregate(c)) =
                        &f.values.get(*args.get(1)? as usize)?.kind
                    else {
                        return None;
                    };
                    let lower = self.const_u(*c.first()?)? as u32;
                    let class = self.const_u(*c.get(3)?)? as u32;
                    let idx = self.const_u(*args.get(2)?)? as u32;
                    return Some((class, lower + idx));
                }
                return None;
            }
        }
        None
    }

    pub fn prescan_resources(&mut self) {
        let f = self.f;
        let mut marks: Vec<(u32, ValId)> = Vec::new();
        for b in &f.blocks {
            for i in &b.insts {
                let Op::Call { callee, args } = &i.op else {
                    continue;
                };
                let Some(ValueKind::Function(fi)) =
                    f.values.get(*callee as usize).map(|v| v.kind.clone())
                else {
                    continue;
                };
                if !self.m.functions[fi].name.starts_with("dx.op.") {
                    continue;
                }
                let op = args.first().and_then(|&a| self.const_u(a)).unwrap_or(0) as u32;
                if matches!(op, 64 | 65 | 74 | 78 | 79)
                    && let Some(&h) = args.get(1)
                {
                    marks.push((op, h));
                }
            }
        }
        for (op, h) in marks {
            if let Some((class, slot)) = self.origin_of(h, 0) {
                match (op, class) {
                    (64 | 65 | 74, 0) => {
                        self.res.depth_slots.insert(slot);
                    }
                    (78 | 79, 1) => {
                        self.res.atomic_slots.insert(slot);
                    }
                    _ => {}
                }
            }
        }
    }

    // ------------------------------------------------------------------ handles

    fn record(&self, class: ResClass, slot: u32, space: u32) -> Option<&ResRecord> {
        let list = match class {
            ResClass::Srv => &self.info.srvs,
            ResClass::Uav => &self.info.uavs,
            ResClass::Cbv => &self.info.cbvs,
            ResClass::Sampler => &self.info.samplers,
        };
        list.iter()
            .find(|r| r.space == space && slot >= r.lower && (slot - r.lower) < r.size)
    }

    fn class_of(c: u64) -> Option<ResClass> {
        Some(match c {
            0 => ResClass::Srv,
            1 => ResClass::Uav,
            2 => ResClass::Cbv,
            3 => ResClass::Sampler,
            _ => return None,
        })
    }

    pub fn op_create_handle(&mut self, args: &[ValId]) -> R<RVal> {
        let (Some(c), Some(range), Some(idx)) = (
            args.get(1).and_then(|&a| self.const_u(a)),
            args.get(2).and_then(|&a| self.const_u(a)),
            args.get(3).and_then(|&a| self.const_u(a)),
        ) else {
            return unsup(
                "createHandle avec index de ressource dynamique (tableau de descripteurs)",
            );
        };
        let Some(class) = Self::class_of(c) else {
            return invalid("createHandle : classe invalide");
        };
        let list = match class {
            ResClass::Srv => &self.info.srvs,
            ResClass::Uav => &self.info.uavs,
            ResClass::Cbv => &self.info.cbvs,
            ResClass::Sampler => &self.info.samplers,
        };
        let Some(r) = list.iter().find(|r| u64::from(r.id) == range) else {
            return invalid("createHandle : range inconnu");
        };
        if r.space != 0 {
            return unsup(format!(
                "espace de registres {} (seul l'espace 0 est lié)",
                r.space
            ));
        }
        Ok(RVal::Handle(Box::new(Handle {
            class: Some(class),
            kind: r.kind,
            slot: Slot::Bound(idx as u32),
            comp: r.comp_type,
            stride: r.stride,
            space: r.space,
        })))
    }

    pub fn op_create_handle_binding(&mut self, args: &[ValId]) -> R<RVal> {
        let Some(&b) = args.get(1) else {
            return invalid("createHandleFromBinding : arguments manquants");
        };
        let RVal::Agg(_) = self.val(b)? else {
            return invalid("createHandleFromBinding : ResBind attendu");
        };
        // Les champs sont des constantes : on les relit via la valeur LLVM.
        let Some(ValueKind::Const(Const::Aggregate(c))) =
            self.f.values.get(b as usize).map(|v| v.kind.clone())
        else {
            return invalid("ResBind non constant");
        };
        let cu = |i: usize| c.get(i).and_then(|&x| self.const_u(x)).unwrap_or(0);
        let lower = cu(0) as u32;
        let space = cu(2) as u32;
        let class = Self::class_of(cu(3));
        self.binding_handle(lower, space, 0, class, args)
    }

    fn binding_handle(
        &mut self,
        lower: u32,
        space: u32,
        _unused: u32,
        class: Option<ResClass>,
        args: &[ValId],
    ) -> R<RVal> {
        let Some(class) = class else {
            return invalid("createHandleFromBinding : classe invalide");
        };
        let Some(idx) = args.get(2).and_then(|&a| self.const_u(a)) else {
            return unsup("createHandleFromBinding avec index dynamique (tableau de descripteurs)");
        };
        if space != 0 {
            return unsup(format!(
                "espace de registres {space} (seul l'espace 0 est lié)"
            ));
        }
        let slot = lower + idx as u32;
        let (kind, comp, stride) = match self.record(class, slot, space) {
            Some(r) => (r.kind, r.comp_type, r.stride),
            None => (
                match class {
                    ResClass::Cbv => rk::CBUFFER,
                    ResClass::Sampler => rk::SAMPLER,
                    _ => 0,
                },
                0,
                0,
            ),
        };
        Ok(RVal::Handle(Box::new(Handle {
            class: Some(class),
            kind,
            slot: Slot::Bound(slot),
            comp,
            stride,
            space,
        })))
    }

    pub fn op_create_handle_heap(&mut self, args: &[ValId]) -> R<RVal> {
        let (Some(&idx), Some(sampler)) = (args.get(1), args.get(2).and_then(|&a| self.const_u(a)))
        else {
            return invalid("createHandleFromHeap : arguments manquants");
        };
        let nonuniform = args.get(3).and_then(|&a| self.const_u(a)).unwrap_or(0) != 0;
        self.res.uses_heap = true;
        Ok(RVal::Handle(Box::new(Handle {
            class: if sampler != 0 {
                Some(ResClass::Sampler)
            } else {
                None
            },
            kind: if sampler != 0 { rk::SAMPLER } else { 0 },
            slot: Slot::Heap { idx, nonuniform },
            comp: 0,
            stride: 0,
            space: 0,
        })))
    }

    pub fn op_annotate_handle(&mut self, args: &[ValId]) -> R<RVal> {
        let (Some(&h), Some(&p)) = (args.get(1), args.get(2)) else {
            return invalid("annotateHandle : arguments manquants");
        };
        let RVal::Handle(mut h) = self.val(h)? else {
            return invalid("annotateHandle : handle attendu");
        };
        let Some(ValueKind::Const(Const::Aggregate(c))) =
            self.f.values.get(p as usize).map(|v| v.kind.clone())
        else {
            return Ok(RVal::Handle(h));
        };
        let p0 = c.first().and_then(|&x| self.const_u(x)).unwrap_or(0) as u32;
        let p1 = c.get(1).and_then(|&x| self.const_u(x)).unwrap_or(0) as u32;
        let kind = p0 & 0xFF;
        h.kind = kind;
        h.class = Some(match kind {
            rk::CBUFFER => ResClass::Cbv,
            rk::SAMPLER | rk::SAMPLERCOMPARISON => ResClass::Sampler,
            _ if p0 & 0x1000 != 0 => ResClass::Uav,
            _ => ResClass::Srv,
        });
        match kind {
            rk::STRUCTUREDBUFFER | rk::STRUCTUREDBUFFERCOUNTER => h.stride = p1,
            rk::RAWBUFFER | rk::CBUFFER | rk::SAMPLER => {}
            _ => h.comp = p1 & 0xFF,
        }
        Ok(RVal::Handle(h))
    }

    pub fn handle(&mut self, id: ValId) -> R<Handle> {
        match self.val(id)? {
            RVal::Handle(h) => Ok(*h),
            _ => invalid("handle de ressource attendu"),
        }
    }

    fn bound_slot(&self, h: &Handle, what: &str) -> R<u32> {
        match h.slot {
            Slot::Bound(s) if s < MAX_BINDING_SLOTS => Ok(s),
            Slot::Bound(s) => unsup(format!("{what} slot {s} >= {MAX_BINDING_SLOTS}")),
            Slot::Heap { .. } => invalid("handle de tas inattendu"),
        }
    }

    // ------------------------------------------------------------------ cbuffer

    pub fn cbuffer_row(&mut self, h: &Handle, row: u32) -> R<u32> {
        let Slot::Bound(_) = h.slot else {
            return unsup("cbuffer dans le tas de descripteurs (UBO non bornés)");
        };
        let slot = self.bound_slot(h, "cbuffer")?;
        let (var, _) = match self.res.cbufs.get(&slot) {
            Some(&c) => c,
            None => {
                let rows = self
                    .record(ResClass::Cbv, slot, 0)
                    .map(|r| r.byte_size.div_ceil(16))
                    .filter(|&n| n > 0)
                    .or(if h.stride > 0 {
                        Some(h.stride.div_ceil(16))
                    } else {
                        None
                    })
                    .unwrap_or(256)
                    .clamp(1, 4096);
                let u4 = self.vty(St::I32, 4);
                let len = self.c32(rows);
                let arr = self.s.array(u4, len);
                if self.res.striped.insert(arr) {
                    self.s.decorate(arr, DEC_ARRAY_STRIDE, &[16]);
                }
                let st = self.s.struct_new(&[arr]);
                self.s.decorate(st, DEC_BLOCK, &[]);
                self.s.member_decorate(st, 0, DEC_OFFSET, &[0]);
                let ptr = self.s.ptr(SC_UNIFORM, st);
                let var = self.s.global_var(ptr, SC_UNIFORM, None);
                self.s.decorate(var, DEC_DESCRIPTOR_SET, &[0]);
                self.s.decorate(var, DEC_BINDING, &[slot]);
                self.res.cbufs.insert(slot, (var, rows));
                self.res.used_cb.insert(slot);
                (var, rows)
            }
        };
        let u4 = self.vty(St::I32, 4);
        let pt = self.s.ptr(SC_UNIFORM, u4);
        let z = self.c32(0);
        let p = self.s.emit(OP_ACCESS_CHAIN, pt, &[var, z, row]);
        Ok(self.s.emit(OP_LOAD, u4, &[p]))
    }

    // ------------------------------------------------------------------ images

    pub fn image_type(&mut self, sc: Sc, dim: Dim, depth: u32, sampled: u32, format: u32) -> u32 {
        match (dim.spv, sampled) {
            (0, 1) => {
                self.s.caps.insert(CAP_SAMPLED1D);
            }
            (0, _) => {
                self.s.caps.insert(CAP_IMAGE1D);
            }
            (5, 1) => {
                self.s.caps.insert(CAP_SAMPLED_BUFFER);
            }
            (5, _) => {
                self.s.caps.insert(CAP_IMAGE_BUFFER);
            }
            _ => {}
        }
        if dim.cube && dim.arrayed {
            self.s.caps.insert(CAP_IMAGE_CUBE_ARRAY);
        }
        if dim.ms && dim.arrayed {
            self.s.caps.insert(CAP_IMAGE_MS_ARRAY);
        }
        if dim.ms && sampled == 2 {
            self.s.caps.insert(CAP_STORAGE_IMAGE_MULTISAMPLE);
        }
        let st = self.s.ty(sc, 1);
        self.s.raw_type(
            25,
            &[
                st,
                dim.spv,
                depth,
                u32::from(dim.arrayed),
                u32::from(dim.ms),
                sampled,
                format,
            ],
        )
    }

    fn sampled_sc(h: &Handle) -> Sc {
        sc_of_comp(h.comp)
    }

    /// Image (valeur chargée) pour un SRV ou UAV typé.
    pub fn image_value(&mut self, h: &Handle, uav: bool) -> R<(u32, ImgInfo)> {
        let Some(dim) = dim_of_kind(h.kind) else {
            return unsup(format!("ressource de type {} utilisée comme image", h.kind));
        };
        let sc = Self::sampled_sc(h);
        match h.slot {
            Slot::Bound(_) => {
                let slot = self.bound_slot(h, if uav { "uav" } else { "texture" })?;
                let cached = if uav {
                    self.res.uav_img.get(&slot)
                } else {
                    self.res.srv_img.get(&slot)
                };
                let (var, info) = match cached {
                    Some(&c) => c,
                    None => {
                        let (depth, sampled, format, binding) = if uav {
                            let fmt = if self.res.atomic_slots.contains(&slot) {
                                match sc {
                                    Sc::I32 => 24,
                                    Sc::U32 => 33,
                                    _ => 3,
                                }
                            } else {
                                self.s.caps.insert(CAP_STORAGE_IMAGE_READ_WITHOUT_FORMAT);
                                self.s.caps.insert(CAP_STORAGE_IMAGE_WRITE_WITHOUT_FORMAT);
                                0
                            };
                            self.res.used_uav.insert(slot);
                            (0, 2, fmt, STORAGE_BINDING_BASE + slot)
                        } else {
                            self.res.used_tex.insert(slot);
                            (
                                u32::from(self.res.depth_slots.contains(&slot)),
                                1,
                                0,
                                TEXTURE_BINDING_BASE + slot,
                            )
                        };
                        let ty = self.image_type(sc, dim, depth, sampled, format);
                        let ptr = self.s.ptr(SC_UNIFORM_CONSTANT, ty);
                        let var = self.s.global_var(ptr, SC_UNIFORM_CONSTANT, None);
                        self.s.decorate(var, DEC_DESCRIPTOR_SET, &[0]);
                        self.s.decorate(var, DEC_BINDING, &[binding]);
                        let info = ImgInfo { ty, dim, sc };
                        if uav {
                            self.res.uav_img.insert(slot, (var, info));
                        } else {
                            self.res.srv_img.insert(slot, (var, info));
                        }
                        (var, info)
                    }
                };
                let id = self.s.emit(OP_LOAD, info.ty, &[var]);
                Ok((id, info))
            }
            Slot::Heap { idx, nonuniform } => {
                let (var, info) = self.heap_image_var(dim, sc, uav, nonuniform);
                let i = self.scalar_u32(idx)?;
                let pt = self.s.ptr(SC_UNIFORM_CONSTANT, info.ty);
                let p = self.s.emit(OP_ACCESS_CHAIN, pt, &[var, i]);
                let id = self.s.emit(OP_LOAD, info.ty, &[p]);
                if nonuniform {
                    self.s.decorate(i, DEC_NON_UNIFORM, &[]);
                    self.s.decorate(p, DEC_NON_UNIFORM, &[]);
                    self.s.decorate(id, DEC_NON_UNIFORM, &[]);
                }
                Ok((id, info))
            }
        }
    }

    /// Pointeur vers la variable d'image (pour `OpImageTexelPointer`).
    pub fn image_ptr(&mut self, h: &Handle) -> R<(u32, ImgInfo)> {
        let slot = match h.slot {
            Slot::Bound(_) => self.bound_slot(h, "uav")?,
            Slot::Heap { .. } => return unsup("atomiques sur image du tas de descripteurs"),
        };
        // Force la création de la variable.
        let _ = self.image_value(h, true)?;
        let Some(&(var, info)) = self.res.uav_img.get(&slot) else {
            return invalid("image UAV introuvable");
        };
        Ok((var, info))
    }

    fn heap_image_var(&mut self, dim: Dim, sc: Sc, uav: bool, nonuniform: bool) -> (u32, ImgInfo) {
        let scidx = match sc {
            Sc::F32 | Sc::Bool => 0,
            Sc::I32 => 1,
            Sc::U32 => 2,
        };
        let key = (uav, dim.idx, scidx);
        self.heap_caps(
            if uav {
                if dim.buffer {
                    CAP_STORAGE_TEXEL_BUFFER_ARRAY_NON_UNIFORM
                } else {
                    CAP_STORAGE_IMAGE_ARRAY_NON_UNIFORM
                }
            } else if dim.buffer {
                CAP_UNIFORM_TEXEL_BUFFER_ARRAY_NON_UNIFORM
            } else {
                CAP_SAMPLED_IMAGE_ARRAY_NON_UNIFORM
            },
            nonuniform,
        );
        if let Some(&c) = self.res.heap_imgs.get(&key) {
            return c;
        }
        if uav {
            self.s.caps.insert(CAP_STORAGE_IMAGE_READ_WITHOUT_FORMAT);
            self.s.caps.insert(CAP_STORAGE_IMAGE_WRITE_WITHOUT_FORMAT);
        }
        let ty = self.image_type(sc, dim, 0, if uav { 2 } else { 1 }, 0);
        let arr = self.s.runtime_array(ty);
        let ptr = self.s.ptr(SC_UNIFORM_CONSTANT, arr);
        let var = self.s.global_var(ptr, SC_UNIFORM_CONSTANT, None);
        let binding = if uav { 48 } else { 16 } + 3 * dim.idx + scidx;
        self.s.decorate(var, DEC_DESCRIPTOR_SET, &[HEAP_SET]);
        self.s.decorate(var, DEC_BINDING, &[binding]);
        let info = ImgInfo { ty, dim, sc };
        self.res.heap_imgs.insert(key, (var, info));
        (var, info)
    }

    fn heap_caps(&mut self, nonuniform_cap: u32, nonuniform: bool) {
        self.s.caps.insert(CAP_RUNTIME_DESCRIPTOR_ARRAY);
        self.s.exts.insert("SPV_EXT_descriptor_indexing");
        if nonuniform {
            self.s.caps.insert(nonuniform_cap);
        }
    }

    pub fn sampler_value(&mut self, h: &Handle) -> R<u32> {
        let st = self.s.raw_type(26, &[]);
        match h.slot {
            Slot::Bound(_) => {
                let slot = self.bound_slot(h, "sampler")?;
                let var = match self.res.samplers.get(&slot) {
                    Some(&v) => v,
                    None => {
                        let ptr = self.s.ptr(SC_UNIFORM_CONSTANT, st);
                        let var = self.s.global_var(ptr, SC_UNIFORM_CONSTANT, None);
                        self.s.decorate(var, DEC_DESCRIPTOR_SET, &[0]);
                        self.s
                            .decorate(var, DEC_BINDING, &[SAMPLER_BINDING_BASE + slot]);
                        self.res.samplers.insert(slot, var);
                        self.res.used_smp.insert(slot);
                        var
                    }
                };
                Ok(self.s.emit(OP_LOAD, st, &[var]))
            }
            Slot::Heap { idx, nonuniform } => {
                self.heap_caps(CAP_SAMPLED_IMAGE_ARRAY_NON_UNIFORM, nonuniform);
                let var = match self.res.heap_sampler {
                    Some(v) => v,
                    None => {
                        let arr = self.s.runtime_array(st);
                        let ptr = self.s.ptr(SC_UNIFORM_CONSTANT, arr);
                        let var = self.s.global_var(ptr, SC_UNIFORM_CONSTANT, None);
                        self.s.decorate(var, DEC_DESCRIPTOR_SET, &[HEAP_SET]);
                        self.s.decorate(var, DEC_BINDING, &[0]);
                        self.res.heap_sampler = Some(var);
                        var
                    }
                };
                let i = self.scalar_u32(idx)?;
                let pt = self.s.ptr(SC_UNIFORM_CONSTANT, st);
                let p = self.s.emit(OP_ACCESS_CHAIN, pt, &[var, i]);
                let id = self.s.emit(OP_LOAD, st, &[p]);
                if nonuniform {
                    self.s.decorate(i, DEC_NON_UNIFORM, &[]);
                    self.s.decorate(p, DEC_NON_UNIFORM, &[]);
                    self.s.decorate(id, DEC_NON_UNIFORM, &[]);
                }
                Ok(id)
            }
        }
    }

    // ------------------------------------------------------------------ buffers bruts / structurés

    fn rta_u32(&mut self) -> u32 {
        let u = self.sty(St::I32);
        let rta = self.s.runtime_array(u);
        if self.res.rta.is_none() {
            self.s.decorate(rta, DEC_ARRAY_STRIDE, &[4]);
            self.res.rta = Some(rta);
        }
        rta
    }

    fn ssbo_struct(&mut self, writable: bool) -> u32 {
        let rta = self.rta_u32();
        let st = self.s.struct_new(&[rta]);
        self.s.decorate(st, DEC_BLOCK, &[]);
        self.s.member_decorate(st, 0, DEC_OFFSET, &[0]);
        if !writable {
            self.s.member_decorate(st, 0, DEC_NON_WRITABLE, &[]);
        }
        st
    }

    /// Pointeur vers le mot `word` (u32) d'un buffer brut/structuré.
    pub fn buffer_word_ptr(&mut self, h: &Handle, uav: bool, word: u32) -> R<u32> {
        let u = self.sty(St::I32);
        let pt = self.s.ptr(SC_STORAGE_BUFFER, u);
        let z = self.c32(0);
        match h.slot {
            Slot::Bound(_) => {
                let slot = self.bound_slot(h, if uav { "uav" } else { "srv" })?;
                let cache = if uav {
                    &self.res.uav_buf
                } else {
                    &self.res.srv_buf
                };
                let var = match cache.get(&slot) {
                    Some(&v) => v,
                    None => {
                        let st = self.ssbo_struct(uav);
                        let ptr = self.s.ptr(SC_STORAGE_BUFFER, st);
                        let var = self.s.global_var(ptr, SC_STORAGE_BUFFER, None);
                        let binding = if uav {
                            STORAGE_BINDING_BASE + slot
                        } else {
                            SRV_BUFFER_BINDING_BASE + slot
                        };
                        self.s.decorate(var, DEC_DESCRIPTOR_SET, &[0]);
                        self.s.decorate(var, DEC_BINDING, &[binding]);
                        if uav {
                            self.res.uav_buf.insert(slot, var);
                            self.res.used_uav.insert(slot);
                        } else {
                            self.res.srv_buf.insert(slot, var);
                            self.res.used_srv_buf.insert(slot);
                        }
                        var
                    }
                };
                Ok(self.s.emit(OP_ACCESS_CHAIN, pt, &[var, z, word]))
            }
            Slot::Heap { idx, nonuniform } => {
                self.heap_caps(CAP_STORAGE_BUFFER_ARRAY_NON_UNIFORM, nonuniform);
                let cur = if uav {
                    self.res.heap_uav_buf
                } else {
                    self.res.heap_srv_buf
                };
                let var = match cur {
                    Some(v) => v,
                    None => {
                        let st = self.ssbo_struct(uav);
                        let arr = self.s.runtime_array(st);
                        let ptr = self.s.ptr(SC_STORAGE_BUFFER, arr);
                        let var = self.s.global_var(ptr, SC_STORAGE_BUFFER, None);
                        self.s.decorate(var, DEC_DESCRIPTOR_SET, &[HEAP_SET]);
                        self.s
                            .decorate(var, DEC_BINDING, &[if uav { 2 } else { 1 }]);
                        if uav {
                            self.res.heap_uav_buf = Some(var);
                        } else {
                            self.res.heap_srv_buf = Some(var);
                        }
                        var
                    }
                };
                let i = self.scalar_u32(idx)?;
                let p = self.s.emit(OP_ACCESS_CHAIN, pt, &[var, i, z, word]);
                if nonuniform {
                    self.s.decorate(i, DEC_NON_UNIFORM, &[]);
                    self.s.decorate(p, DEC_NON_UNIFORM, &[]);
                }
                Ok(p)
            }
        }
    }

    /// `OpArrayLength` (en mots) du buffer.
    pub fn buffer_words(&mut self, h: &Handle, uav: bool) -> R<u32> {
        let u = self.sty(St::I32);
        // Force la création de la variable.
        let z = self.c32(0);
        let _ = self.buffer_word_ptr(h, uav, z)?;
        match h.slot {
            Slot::Bound(_) => {
                let slot = self.bound_slot(h, "buffer")?;
                let var = if uav {
                    self.res.uav_buf[&slot]
                } else {
                    self.res.srv_buf[&slot]
                };
                Ok(self.s.emit(OP_ARRAY_LENGTH, u, &[var, 0]))
            }
            Slot::Heap { .. } => unsup("GetDimensions sur un buffer du tas de descripteurs"),
        }
    }

    /// Adresse en octets d'un accès buffer (`idx` = indice ou offset, `off` = offset d'élément).
    pub fn byte_addr(&mut self, h: &Handle, a0: ValId, a1: Option<ValId>) -> R<u32> {
        let i = self.scalar_u32(a0)?;
        let off = match a1 {
            Some(a) if !self.is_undef(a) => Some(self.scalar_u32(a)?),
            _ => None,
        };
        let u = self.sty(St::I32);
        let structured = matches!(h.kind, rk::STRUCTUREDBUFFER | rk::STRUCTUREDBUFFERCOUNTER);
        let base = if structured {
            if h.stride == 0 {
                return invalid("buffer structuré sans stride");
            }
            let s = self.c32(h.stride);
            self.s.emit(OP_I_MUL, u, &[i, s])
        } else {
            i
        };
        Ok(match off {
            Some(o) => self.s.emit(OP_I_ADD, u, &[base, o]),
            None => base,
        })
    }

    pub fn word_index(&mut self, byte: u32, extra_words: u32) -> u32 {
        let u = self.sty(St::I32);
        let two = self.c32(2);
        let w = self.s.emit(OP_SHIFT_RIGHT_LOGICAL, u, &[byte, two]);
        if extra_words == 0 {
            w
        } else {
            let k = self.c32(extra_words);
            self.s.emit(OP_I_ADD, u, &[w, k])
        }
    }

    // ------------------------------------------------------------------ requêtes de taille

    pub fn st_from_resret(&self, rt: Option<u32>) -> R<St> {
        let Some(t) = rt else {
            return invalid("ResRet : type de retour manquant");
        };
        match self.m.ty(t) {
            Ty::Struct(fs) => match fs.first() {
                Some(&f) => st_of_ty(self.m, f),
                None => invalid("ResRet vide"),
            },
            _ => st_of_ty(self.m, t),
        }
    }
}
