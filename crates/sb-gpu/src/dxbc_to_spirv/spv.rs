//! Constructeur de modules SPIR-V 1.3 (binaire brut) : types/constantes dédupliqués, sections
//! ordonnées, fonctions, blocs de base avec suivi de terminaison.
#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap};

/// Type scalaire de base manipulé par le traducteur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sc {
    F32,
    I32,
    U32,
    Bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TyKey {
    Void,
    Sc(Sc),
    Vec(Sc, u32),
    Ptr(u32, u32),
    Array(u32, u32),
    RuntimeArray(u32),
    Struct(Vec<u32>),
    Fn(u32),
}

/// Valeur SSA typée : `n` composantes de type `sc`.
#[derive(Debug, Clone, Copy)]
pub struct Val {
    pub id: u32,
    pub sc: Sc,
    pub n: u32,
}

pub struct Spv {
    next_id: u32,
    pub caps: BTreeSet<u32>,
    pub exts: BTreeSet<&'static str>,
    pub glsl: u32,
    pub annotations: Vec<u32>,
    pub types: Vec<u32>,
    pub funcs: Vec<u32>,
    cur: Vec<u32>,
    pub terminated: bool,
    ty_cache: HashMap<TyKey, u32>,
    const_cache: HashMap<(u32, Vec<u32>), u32>,
    pub interface: Vec<u32>,
}

pub fn push_str(words: &mut Vec<u32>, s: &str) {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
    for c in bytes.as_chunks::<4>().0 {
        words.push(u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
    }
}

fn put(buf: &mut Vec<u32>, op: u16, operands: &[u32]) {
    buf.push((((operands.len() + 1) as u32) << 16) | u32::from(op));
    buf.extend_from_slice(operands);
}

impl Spv {
    pub fn new() -> Self {
        let mut s = Spv {
            next_id: 1,
            caps: BTreeSet::new(),
            exts: BTreeSet::new(),
            glsl: 0,
            annotations: Vec::new(),
            types: Vec::new(),
            funcs: Vec::new(),
            cur: Vec::new(),
            terminated: true,
            ty_cache: HashMap::new(),
            const_cache: HashMap::new(),
            interface: Vec::new(),
        };
        s.glsl = s.id();
        s.caps.insert(1); // Shader
        s
    }

    pub fn id(&mut self) -> u32 {
        let i = self.next_id;
        self.next_id += 1;
        i
    }

    pub fn bound(&self) -> u32 {
        self.next_id
    }

    // ---------- Types ----------
    fn ty_key(&mut self, key: TyKey) -> u32 {
        if let Some(&i) = self.ty_cache.get(&key) {
            return i;
        }
        let id = match &key {
            TyKey::Void => {
                let id = self.id();
                put(&mut self.types, 19, &[id]);
                id
            }
            TyKey::Sc(sc) => {
                let id = self.id();
                match sc {
                    Sc::Bool => put(&mut self.types, 20, &[id]),
                    Sc::F32 => put(&mut self.types, 22, &[id, 32]),
                    Sc::I32 => put(&mut self.types, 21, &[id, 32, 1]),
                    Sc::U32 => put(&mut self.types, 21, &[id, 32, 0]),
                }
                id
            }
            TyKey::Vec(sc, n) => {
                let base = self.ty(*sc, 1);
                let id = self.id();
                put(&mut self.types, 23, &[id, base, *n]);
                id
            }
            TyKey::Ptr(class, base) => {
                let id = self.id();
                put(&mut self.types, 32, &[id, *class, *base]);
                id
            }
            TyKey::Array(base, len) => {
                let id = self.id();
                put(&mut self.types, 28, &[id, *base, *len]);
                id
            }
            TyKey::RuntimeArray(base) => {
                let id = self.id();
                put(&mut self.types, 29, &[id, *base]);
                id
            }
            TyKey::Struct(members) => {
                let id = self.id();
                let mut ops = vec![id];
                ops.extend_from_slice(members);
                put(&mut self.types, 30, &ops);
                id
            }
            TyKey::Fn(ret) => {
                let id = self.id();
                put(&mut self.types, 33, &[id, *ret]);
                id
            }
        };
        self.ty_cache.insert(key, id);
        id
    }

    pub fn t_void(&mut self) -> u32 {
        self.ty_key(TyKey::Void)
    }
    pub fn t_fn_void(&mut self) -> u32 {
        let v = self.t_void();
        self.ty_key(TyKey::Fn(v))
    }
    /// Type scalaire (`n == 1`) ou vecteur (`n` en `2..=4`).
    pub fn ty(&mut self, sc: Sc, n: u32) -> u32 {
        if n <= 1 {
            self.ty_key(TyKey::Sc(sc))
        } else {
            self.ty_key(TyKey::Vec(sc, n))
        }
    }
    pub fn ptr(&mut self, class: u32, base: u32) -> u32 {
        self.ty_key(TyKey::Ptr(class, base))
    }
    pub fn array(&mut self, base: u32, len_const: u32) -> u32 {
        self.ty_key(TyKey::Array(base, len_const))
    }
    pub fn runtime_array(&mut self, base: u32) -> u32 {
        self.ty_key(TyKey::RuntimeArray(base))
    }
    pub fn structure(&mut self, members: &[u32]) -> u32 {
        self.ty_key(TyKey::Struct(members.to_vec()))
    }
    /// Type brut non dédupliqué (images, samplers...) : `words` = opérandes hors `id`.
    pub fn raw_type(&mut self, op: u16, words: &[u32]) -> u32 {
        let key = (op as u32 | 0x8000_0000, words.to_vec());
        if let Some(&i) = self.const_cache.get(&key) {
            return i;
        }
        let id = self.id();
        let mut ops = vec![id];
        ops.extend_from_slice(words);
        put(&mut self.types, op, &ops);
        self.const_cache.insert(key, id);
        id
    }

    // ---------- Constantes ----------
    pub fn c_bits(&mut self, sc: Sc, bits: u32) -> u32 {
        let ty = self.ty(sc, 1);
        let key = (ty, vec![bits]);
        if let Some(&i) = self.const_cache.get(&key) {
            return i;
        }
        let id = self.id();
        put(&mut self.types, 43, &[ty, id, bits]);
        self.const_cache.insert(key, id);
        id
    }
    pub fn c_u32(&mut self, v: u32) -> u32 {
        self.c_bits(Sc::U32, v)
    }
    pub fn c_i32(&mut self, v: i32) -> u32 {
        self.c_bits(Sc::I32, v as u32)
    }
    pub fn c_f32(&mut self, v: f32) -> u32 {
        self.c_bits(Sc::F32, v.to_bits())
    }
    /// Constante scalaire (`bits.len() == 1`) ou vectorielle de composantes `bits`.
    pub fn c_vec(&mut self, sc: Sc, bits: &[u32]) -> Val {
        let n = bits.len() as u32;
        if n == 1 {
            return Val {
                id: self.c_bits(sc, bits[0]),
                sc,
                n: 1,
            };
        }
        let ty = self.ty(sc, n);
        let comps: Vec<u32> = bits.iter().map(|&b| self.c_bits(sc, b)).collect();
        let key = (ty, comps.clone());
        if let Some(&i) = self.const_cache.get(&key) {
            return Val { id: i, sc, n };
        }
        let id = self.id();
        let mut ops = vec![ty, id];
        ops.extend_from_slice(&comps);
        put(&mut self.types, 44, &ops);
        self.const_cache.insert(key, id);
        Val { id, sc, n }
    }
    pub fn c_composite(&mut self, ty: u32, comps: &[u32]) -> u32 {
        let key = (ty, comps.to_vec());
        if let Some(&i) = self.const_cache.get(&key) {
            return i;
        }
        let id = self.id();
        let mut ops = vec![ty, id];
        ops.extend_from_slice(comps);
        put(&mut self.types, 44, &ops);
        self.const_cache.insert(key, id);
        id
    }
    pub fn c_null(&mut self, ty: u32) -> u32 {
        let key = (ty | 0x4000_0000, vec![]);
        if let Some(&i) = self.const_cache.get(&key) {
            return i;
        }
        let id = self.id();
        put(&mut self.types, 46, &[ty, id]);
        self.const_cache.insert(key, id);
        id
    }

    // ---------- Globales / décorations ----------
    pub fn global_var(&mut self, ptr_ty: u32, class: u32, init: Option<u32>) -> u32 {
        let id = self.id();
        let mut ops = vec![ptr_ty, id, class];
        if let Some(i) = init {
            ops.push(i);
        }
        put(&mut self.types, 59, &ops);
        id
    }
    pub fn decorate(&mut self, target: u32, deco: u32, args: &[u32]) {
        let mut ops = vec![target, deco];
        ops.extend_from_slice(args);
        put(&mut self.annotations, 71, &ops);
    }
    pub fn member_decorate(&mut self, target: u32, member: u32, deco: u32, args: &[u32]) {
        let mut ops = vec![target, member, deco];
        ops.extend_from_slice(args);
        put(&mut self.annotations, 72, &ops);
    }

    // ---------- Fonctions / blocs ----------
    pub fn begin_function(&mut self, fn_id: u32) {
        let void = self.t_void();
        let fnty = self.t_fn_void();
        self.cur.clear();
        put(&mut self.cur, 54, &[void, fn_id, 0, fnty]);
        let l = self.id();
        put(&mut self.cur, 248, &[l]);
        self.terminated = false;
    }
    pub fn end_function(&mut self) {
        if !self.terminated {
            put(&mut self.cur, 253, &[]);
            self.terminated = true;
        }
        put(&mut self.cur, 56, &[]);
        let f = std::mem::take(&mut self.cur);
        self.funcs.extend_from_slice(&f);
    }
    /// Démarre un bloc orphelin si le bloc courant est déjà terminé.
    pub fn ensure_block(&mut self) {
        if self.terminated {
            let l = self.id();
            put(&mut self.cur, 248, &[l]);
            self.terminated = false;
        }
    }
    /// Ouvre le bloc `label` (branche vers lui si le bloc courant est encore ouvert).
    pub fn begin_block(&mut self, label: u32) {
        if !self.terminated {
            put(&mut self.cur, 249, &[label]);
        }
        put(&mut self.cur, 248, &[label]);
        self.terminated = false;
    }
    /// Termine le bloc courant par un saut vers `branch_to` (s'il est ouvert), puis ouvre `label`.
    pub fn begin_block_from(&mut self, branch_to: u32, label: u32) {
        if !self.terminated {
            put(&mut self.cur, 249, &[branch_to]);
        }
        put(&mut self.cur, 248, &[label]);
        self.terminated = false;
    }
    /// Struct non dédupliquée (décorations propres).
    pub fn struct_new(&mut self, members: &[u32]) -> u32 {
        let id = self.id();
        let mut ops = vec![id];
        ops.extend_from_slice(members);
        put(&mut self.types, 30, &ops);
        id
    }
    pub fn terminate(&mut self, op: u16, ops: &[u32]) {
        self.ensure_block();
        put(&mut self.cur, op, ops);
        self.terminated = true;
    }
    pub fn branch(&mut self, target: u32) {
        self.terminate(249, &[target]);
    }
    pub fn emit_void(&mut self, op: u16, ops: &[u32]) {
        self.ensure_block();
        put(&mut self.cur, op, ops);
    }
    /// Instruction produisant un résultat : `ty` puis `id` insérés en tête.
    pub fn emit(&mut self, op: u16, ty: u32, ops: &[u32]) -> u32 {
        self.ensure_block();
        let id = self.id();
        let mut all = vec![ty, id];
        all.extend_from_slice(ops);
        put(&mut self.cur, op, &all);
        id
    }
    /// Instruction brute sans bloc requis (merge : doit suivre un label ouvert).
    pub fn emit_raw(&mut self, op: u16, ops: &[u32]) {
        put(&mut self.cur, op, ops);
    }

    // ---------- Aides typées ----------
    pub fn val(&mut self, op: u16, sc: Sc, n: u32, ops: &[u32]) -> Val {
        let ty = self.ty(sc, n);
        let id = self.emit(op, ty, ops);
        Val { id, sc, n }
    }
    pub fn glsl_op(&mut self, sc: Sc, n: u32, inst: u32, args: &[u32]) -> Val {
        let mut ops = vec![self.glsl, inst];
        ops.extend_from_slice(args);
        self.val(12, sc, n, &ops)
    }
    pub fn bitcast(&mut self, v: Val, to: Sc) -> Val {
        if v.sc == to {
            return v;
        }
        self.val(124, to, v.n, &[v.id])
    }
    /// Sélectionne les composantes `comps` du vecteur `v`.
    pub fn pick(&mut self, v: Val, comps: &[u32]) -> Val {
        let n = comps.len() as u32;
        if n == 1 {
            if v.n == 1 {
                return v;
            }
            return self.val(81, v.sc, 1, &[v.id, comps[0]]);
        }
        if v.n == n && comps.iter().enumerate().all(|(i, &c)| c == i as u32) {
            return v;
        }
        let mut ops = vec![v.id, v.id];
        ops.extend_from_slice(comps);
        self.val(79, v.sc, n, &ops)
    }
    pub fn splat(&mut self, v: Val, n: u32) -> Val {
        if n == 1 || v.n == n {
            return v;
        }
        let ops = vec![v.id; n as usize];
        self.val(80, v.sc, n, &ops)
    }
    pub fn construct(&mut self, sc: Sc, comps: &[u32]) -> Val {
        if comps.len() == 1 {
            return Val {
                id: comps[0],
                sc,
                n: 1,
            };
        }
        self.val(80, sc, comps.len() as u32, comps)
    }
    pub fn extract(&mut self, v: Val, i: u32) -> Val {
        if v.n == 1 {
            return v;
        }
        self.val(81, v.sc, 1, &[v.id, i])
    }

    /// Assemble le module final.
    pub fn assemble(&mut self, exec_model: u32, main_fn: u32, exec_modes: &[Vec<u32>]) -> Vec<u32> {
        let mut out = vec![0x0723_0203, 0x0001_0300, 0, self.next_id, 0];
        for &c in &self.caps {
            put(&mut out, 17, &[c]);
        }
        for e in &self.exts {
            let mut w = Vec::new();
            push_str(&mut w, e);
            put(&mut out, 10, &w);
        }
        let mut w = vec![self.glsl];
        push_str(&mut w, "GLSL.std.450");
        put(&mut out, 11, &w);
        put(&mut out, 14, &[0, 1]);
        let mut ep = vec![exec_model, main_fn];
        push_str(&mut ep, "main");
        ep.extend_from_slice(&self.interface);
        put(&mut out, 15, &ep);
        for m in exec_modes {
            let mut ops = vec![main_fn];
            ops.extend_from_slice(m);
            put(&mut out, 16, &ops);
        }
        out.extend_from_slice(&self.annotations);
        out.extend_from_slice(&self.types);
        out.extend_from_slice(&self.funcs);
        out
    }
}
