//! Noyau du traducteur DXIL -> SPIR-V : valeurs, mémoire, arithmétique, flux de contrôle.

use std::collections::HashMap;

use super::cfg::{Ast, Term};
use super::dxmeta::{EntryInfo, ResClass};
use super::module::{Const, Function, Module, Op, Ty, TyId, ValId, ValueKind};
use super::sp::*;
use crate::dxbc_to_spirv::ShaderTranslateError;
use crate::dxbc_to_spirv::spv::{Sc, Spv};
use crate::shader::ShaderStage;

pub(super) type R<T> = Result<T, ShaderTranslateError>;

pub(super) fn unsup<T>(m: impl Into<String>) -> R<T> {
    Err(ShaderTranslateError::Unsupported(m.into()))
}
pub(super) fn invalid<T>(m: impl Into<String>) -> R<T> {
    Err(ShaderTranslateError::InvalidDxil(m.into()))
}

#[derive(Clone, Debug)]
pub(super) enum Step {
    C(u32),
    V(ValId),
}

/// Pointeur « recette » : variable de base + chemin d'accès, rematérialisé à chaque usage.
#[derive(Clone, Debug)]
pub(super) struct Ptr {
    pub base: u32,
    pub class: u32,
    pub steps: Vec<Step>,
    /// Type LLVM de la valeur pointée.
    pub ty: TyId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Slot {
    /// Ressource liée : slot absolu dans l'espace 0.
    Bound(u32),
    /// Tas de descripteurs (SM6.6) : index dynamique.
    Heap { idx: ValId, nonuniform: bool },
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(super) struct Handle {
    pub class: Option<ResClass>,
    pub kind: u32,
    pub slot: Slot,
    pub comp: u32,
    pub stride: u32,
    pub space: u32,
}

#[derive(Clone, Debug)]
pub(super) enum RVal {
    S(u32, St),
    Agg(Vec<RVal>),
    Ptr(Ptr),
    Handle(Box<Handle>),
    Func(usize),
}

pub(super) enum Cx {
    Loop {
        labels: Vec<usize>,
        merge: u32,
        cont: u32,
        real: bool,
        merge_used: bool,
        cont_used: bool,
        pending: Vec<(bool, usize)>,
    },
    Sel,
}

pub(super) struct Tr<'a> {
    pub m: &'a Module,
    pub f: &'a Function,
    pub info: &'a EntryInfo,
    pub stage: ShaderStage,
    pub s: Spv,
    pub vals: Vec<Option<RVal>>,
    pub cur_block: usize,
    pub def_block: Vec<usize>,
    pub cross: Vec<bool>,
    pub use_vars: bool,
    pub cross_vars: HashMap<ValId, Vec<(u32, St)>>,
    pub phi_vars: HashMap<ValId, Vec<(u32, St)>>,
    pub hoisted: Vec<(u32, u32, Option<u32>)>,
    pub globals: HashMap<usize, Ptr>,
    pub used_mask: Vec<u8>,
    pub cx: Vec<Cx>,
    pub state_var: Option<u32>,
    pub esc_var: Option<u32>,
    pub depth: u32,
    pub mem_cache: HashMap<TyId, u32>,
    pub res: super::res::ResState,
    pub io: super::iface::IoState,
}

pub(super) const NO_BLOCK: usize = usize::MAX;

pub(super) fn st_of_ty(m: &Module, t: TyId) -> R<St> {
    Ok(match m.ty(t) {
        Ty::Int(1) => St::I1,
        Ty::Int(w) if *w <= 8 => St::I8,
        Ty::Int(16) => St::I16,
        Ty::Int(32) => St::I32,
        Ty::Int(64) => St::I64,
        Ty::Half => St::F16,
        Ty::Float => St::F32,
        Ty::Double => St::F64,
        other => return unsup(format!("type scalaire LLVM non supporté : {other:?}")),
    })
}

fn operands(op: &Op) -> Vec<(ValId, bool)> {
    // (valeur, est_un_phi_entrant) ; pour les phi, le bloc d'usage est le prédécesseur.
    match op {
        Op::Binop { a, b, .. } | Op::Cmp { a, b, .. } => vec![(*a, false), (*b, false)],
        Op::Cast { a, .. } => vec![(*a, false)],
        Op::Gep { base, indices } => {
            let mut v = vec![(*base, false)];
            v.extend(indices.iter().map(|&i| (i, false)));
            v
        }
        Op::Select { c, a, b } => vec![(*c, false), (*a, false), (*b, false)],
        Op::Ret(Some(v)) => vec![(*v, false)],
        Op::CondBr { cond, .. } => vec![(*cond, false)],
        Op::Switch { cond, .. } => vec![(*cond, false)],
        Op::Phi(inc) => inc.iter().map(|&(v, _)| (v, true)).collect(),
        Op::Load { ptr } => vec![(*ptr, false)],
        Op::Store { ptr, val } => vec![(*ptr, false), (*val, false)],
        Op::ExtractVal { agg, .. } => vec![(*agg, false)],
        Op::InsertVal { agg, val, .. } => vec![(*agg, false), (*val, false)],
        Op::Call { args, .. } => args.iter().map(|&a| (a, false)).collect(),
        Op::AtomicRmw { ptr, val, .. } => vec![(*ptr, false), (*val, false)],
        Op::CmpXchg { ptr, cmp, new } => vec![(*ptr, false), (*cmp, false), (*new, false)],
        _ => vec![],
    }
}

pub(super) fn block_terms(f: &Function) -> (Vec<Term>, Vec<bool>) {
    let mut terms = Vec::new();
    let mut trivial = Vec::new();
    let n = f.blocks.len();
    let ok = |b: u32| (b as usize) < n;
    for b in &f.blocks {
        let last = b.insts.last().map(|i| &i.op);
        let t = match last {
            Some(Op::Br(t)) if ok(*t) => Term::Br(*t as usize),
            Some(Op::CondBr { cond, t, f }) if ok(*t) && ok(*f) => Term::Cond {
                cond: *cond,
                t: *t as usize,
                f: *f as usize,
            },
            Some(Op::Switch {
                cond,
                default,
                cases,
            }) if ok(*default) && cases.iter().all(|c| ok(c.1)) => Term::Switch {
                sel: *cond,
                default: *default as usize,
                cases: cases.iter().map(|c| (c.0, c.1 as usize)).collect(),
            },
            Some(Op::Ret(_)) => Term::Ret,
            _ => Term::Unreachable,
        };
        let tiv = b.insts.len() == 1 && matches!(t, Term::Ret | Term::Unreachable);
        terms.push(t);
        trivial.push(tiv);
    }
    // Le bloc d'entrée n'est jamais « trivial » (il doit exister).
    if let Some(t) = trivial.first_mut() {
        *t = false;
    }
    (terms, trivial)
}

fn ast_uses(a: &Ast, pred: &dyn Fn(&Ast) -> bool) -> bool {
    if pred(a) {
        return true;
    }
    match a {
        Ast::Seq(v) => v.iter().any(|x| ast_uses(x, pred)),
        Ast::If { then, els, .. } => ast_uses(then, pred) || ast_uses(els, pred),
        Ast::Switch { arms, default, .. } => {
            arms.iter().any(|(_, x)| ast_uses(x, pred)) || ast_uses(default, pred)
        }
        Ast::Block { body, .. } | Ast::Loop { body, .. } => ast_uses(body, pred),
        Ast::SwitchState(arms) => arms.iter().any(|(_, x)| ast_uses(x, pred)),
        _ => false,
    }
}

impl<'a> Tr<'a> {
    pub fn new(m: &'a Module, f: &'a Function, info: &'a EntryInfo, stage: ShaderStage) -> Self {
        let n = f.values.len();
        Tr {
            m,
            f,
            info,
            stage,
            s: Spv::new(),
            vals: vec![None; n],
            cur_block: 0,
            def_block: vec![NO_BLOCK; n],
            cross: vec![false; n],
            use_vars: false,
            cross_vars: HashMap::new(),
            phi_vars: HashMap::new(),
            hoisted: Vec::new(),
            globals: HashMap::new(),
            used_mask: vec![0; n],
            cx: Vec::new(),
            state_var: None,
            esc_var: None,
            depth: 0,
            mem_cache: HashMap::new(),
            res: Default::default(),
            io: Default::default(),
        }
    }

    // ------------------------------------------------------------------ analyse préalable

    pub fn prepare(&mut self) {
        let f = self.f;
        for (bi, b) in f.blocks.iter().enumerate() {
            for i in &b.insts {
                if let Some(r) = i.result {
                    self.def_block[r as usize] = bi;
                }
            }
        }
        for (bi, b) in f.blocks.iter().enumerate() {
            for i in &b.insts {
                let is_extract = matches!(i.op, Op::ExtractVal { .. });
                if let Op::ExtractVal { agg, idx } = &i.op
                    && let (Some(&k), true) = (idx.first(), (*agg as usize) < self.used_mask.len())
                {
                    self.used_mask[*agg as usize] |= 1u8 << k.min(7);
                }
                let phi_preds: Vec<u32> = match &i.op {
                    Op::Phi(inc) => inc.iter().map(|x| x.1).collect(),
                    _ => vec![],
                };
                for (k, (v, is_phi)) in operands(&i.op).into_iter().enumerate() {
                    let vi = v as usize;
                    if vi >= self.def_block.len() {
                        continue;
                    }
                    if !is_extract {
                        self.used_mask[vi] = 0xFF;
                    }
                    let use_block = if is_phi {
                        phi_preds.get(k).map(|&p| p as usize).unwrap_or(bi)
                    } else {
                        bi
                    };
                    let d = self.def_block[vi];
                    if d != NO_BLOCK && d != use_block {
                        self.cross[vi] = true;
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------ types

    pub fn sty(&mut self, st: St) -> u32 {
        match st {
            St::I1 => self.s.ty(Sc::Bool, 1),
            St::I8 | St::I32 => self.s.ty(Sc::U32, 1),
            St::F32 => self.s.ty(Sc::F32, 1),
            St::I16 => {
                self.s.caps.insert(CAP_INT16);
                self.s.raw_type(21, &[16, 0])
            }
            St::I64 => {
                self.s.caps.insert(CAP_INT64);
                self.s.raw_type(21, &[64, 0])
            }
            St::F16 => {
                self.s.caps.insert(CAP_FLOAT16);
                self.s.raw_type(22, &[16])
            }
            St::F64 => {
                self.s.caps.insert(CAP_FLOAT64);
                self.s.raw_type(22, &[64])
            }
        }
    }

    pub fn vty(&mut self, st: St, n: u32) -> u32 {
        if n <= 1 {
            return self.sty(st);
        }
        match st {
            St::I1 => self.s.ty(Sc::Bool, n),
            St::I8 | St::I32 => self.s.ty(Sc::U32, n),
            St::F32 => self.s.ty(Sc::F32, n),
            _ => {
                let e = self.sty(st);
                self.s.raw_type(23, &[e, n])
            }
        }
    }

    /// Constante scalaire à partir de bits bruts.
    pub fn cst(&mut self, st: St, bits: u64) -> u32 {
        match st {
            St::I1 => self.s.c_bool(bits & 1 != 0),
            St::I8 => self.s.c_u32(bits as u32 & 0xFF),
            St::I32 => self.s.c_u32(bits as u32),
            St::F32 => self.s.c_bits(Sc::F32, bits as u32),
            St::I16 | St::F16 => {
                let t = self.sty(st);
                self.s.c_raw(t, &[(bits & 0xFFFF) as u32])
            }
            St::I64 | St::F64 => {
                let t = self.sty(st);
                self.s.c_raw(t, &[bits as u32, (bits >> 32) as u32])
            }
        }
    }

    pub fn c32(&mut self, v: u32) -> u32 {
        self.s.c_u32(v)
    }

    pub fn mem_ty(&mut self, t: TyId) -> R<u32> {
        if let Some(&c) = self.mem_cache.get(&t) {
            return Ok(c);
        }
        let m = self.m;
        let r = match m.ty(t).clone() {
            Ty::Int(1) | Ty::Int(8) => self.sty(St::I32),
            Ty::Int(_) | Ty::Half | Ty::Float | Ty::Double => {
                let st = st_of_ty(m, t)?;
                self.sty(st)
            }
            Ty::Vector(n, e) => {
                let st = st_of_ty(m, e)?;
                let st = if st == St::I1 { St::I32 } else { st };
                self.vty(st, n)
            }
            Ty::Array(n, e) => {
                if n == 0 || n > (1 << 24) {
                    return unsup("tableau LLVM de taille invalide");
                }
                let et = self.mem_ty(e)?;
                let len = self.c32(n as u32);
                self.s.array(et, len)
            }
            Ty::Struct(fs) => {
                let mut ms = Vec::new();
                for x in fs {
                    ms.push(self.mem_ty(x)?);
                }
                if ms.is_empty() {
                    return unsup("struct vide en mémoire");
                }
                self.s.structure(&ms)
            }
            other => return unsup(format!("type mémoire LLVM non supporté : {other:?}")),
        };
        self.mem_cache.insert(t, r);
        Ok(r)
    }

    /// Aplatit un type LLVM en feuilles scalaires.
    pub fn leaves(&self, t: TyId, out: &mut Vec<St>) -> R<()> {
        if out.len() > 4096 {
            return unsup("valeur agrégée trop grande");
        }
        match self.m.ty(t) {
            Ty::Vector(n, e) => {
                let st = st_of_ty(self.m, *e)?;
                for _ in 0..*n {
                    out.push(st);
                }
            }
            Ty::Struct(fs) => {
                for &x in fs {
                    self.leaves(x, out)?;
                }
            }
            Ty::Array(n, e) if *n <= 64 => {
                for _ in 0..*n {
                    self.leaves(*e, out)?;
                }
            }
            Ty::Void | Ty::Label | Ty::Metadata => {}
            _ => out.push(st_of_ty(self.m, t)?),
        }
        Ok(())
    }

    pub fn zero_val(&mut self, t: TyId) -> R<RVal> {
        match self.m.ty(t).clone() {
            Ty::Vector(n, e) => {
                let mut v = Vec::new();
                for _ in 0..n {
                    v.push(self.zero_val(e)?);
                }
                Ok(RVal::Agg(v))
            }
            Ty::Struct(fs) => {
                let mut v = Vec::new();
                for x in fs {
                    v.push(self.zero_val(x)?);
                }
                Ok(RVal::Agg(v))
            }
            Ty::Array(n, e) if n <= 64 => {
                let mut v = Vec::new();
                for _ in 0..n {
                    v.push(self.zero_val(e)?);
                }
                Ok(RVal::Agg(v))
            }
            Ty::Ptr { .. } => unsup("pointeur indéfini"),
            Ty::Metadata | Ty::Label | Ty::Void => unsup("valeur non scalaire"),
            _ => {
                let st = st_of_ty(self.m, t)?;
                let id = self.cst(st, 0);
                Ok(RVal::S(id, st))
            }
        }
    }

    // ------------------------------------------------------------------ valeurs

    fn const_rval(&mut self, c: &Const, t: TyId) -> R<RVal> {
        let m = self.m;
        match c {
            Const::Null | Const::Undef => self.zero_val(t),
            Const::Int(v) | Const::Float(v) => {
                let st = st_of_ty(m, t)?;
                // Les entiers 8 bits restent dans le domaine 32 bits.
                let id = self.cst(st, *v);
                Ok(RVal::S(id, st))
            }
            Const::Aggregate(ids) => {
                let mut out = Vec::new();
                let fts: Vec<TyId> = match m.ty(t) {
                    Ty::Struct(fs) => fs.clone(),
                    Ty::Array(n, e) => vec![*e; *n as usize],
                    Ty::Vector(n, e) => vec![*e; *n as usize],
                    _ => return unsup("agrégat constant de type inattendu"),
                };
                for (i, &id) in ids.iter().enumerate() {
                    let _ = fts.get(i);
                    out.push(self.val(id)?);
                }
                Ok(RVal::Agg(out))
            }
            Const::Data(words) => {
                let e = match m.ty(t) {
                    Ty::Array(_, e) | Ty::Vector(_, e) => *e,
                    _ => return unsup("constante DATA de type inattendu"),
                };
                let st = st_of_ty(m, e)?;
                let mut out = Vec::new();
                for &w in words {
                    out.push(RVal::S(self.cst(st, w), st));
                }
                Ok(RVal::Agg(out))
            }
            Const::Cast { val, .. } => self.val(*val),
            Const::Gep { base, indices } => {
                let b = self.val(*base)?;
                let RVal::Ptr(mut p) = b else {
                    return unsup("GEP constant sur une non-adresse");
                };
                let mut first = true;
                let mut ty = p.ty;
                for &ix in indices {
                    let Some(v) = self.const_u(ix) else {
                        return unsup("GEP constant avec indice non constant");
                    };
                    if first {
                        first = false;
                        if v != 0 {
                            return unsup("arithmétique de pointeur (indice de tête non nul)");
                        }
                        continue;
                    }
                    p.steps.push(Step::C(v as u32));
                    ty = self.child_ty(ty, Some(v))?;
                }
                p.ty = ty;
                Ok(RVal::Ptr(p))
            }
            Const::CString(_) | Const::Unsupported(_) => unsup("constante LLVM non supportée"),
        }
    }

    pub fn child_ty(&self, t: TyId, idx: Option<u64>) -> R<TyId> {
        match self.m.ty(t) {
            Ty::Array(_, e) | Ty::Vector(_, e) => Ok(*e),
            Ty::Struct(fs) => match idx.and_then(|i| fs.get(i as usize)) {
                Some(&x) => Ok(x),
                None => invalid("indice de struct invalide"),
            },
            _ => invalid("indexation d'un type scalaire"),
        }
    }

    pub fn const_u(&self, id: ValId) -> Option<u64> {
        match &self.f.values.get(id as usize)?.kind {
            ValueKind::Const(Const::Int(i)) => Some(*i),
            ValueKind::Const(Const::Null) => Some(0),
            _ => None,
        }
    }

    pub fn is_undef(&self, id: ValId) -> bool {
        matches!(
            self.f.values.get(id as usize).map(|v| &v.kind),
            Some(ValueKind::Const(Const::Undef))
        )
    }

    pub fn val(&mut self, id: ValId) -> R<RVal> {
        if self.depth > 48 {
            return invalid("constantes imbriquées trop profondément (cycle ?)");
        }
        self.depth += 1;
        let r = self.val_inner(id);
        self.depth -= 1;
        r
    }

    fn val_inner(&mut self, id: ValId) -> R<RVal> {
        let f = self.f;
        let Some(v) = f.values.get(id as usize) else {
            return invalid("identifiant de valeur hors bornes");
        };
        if let Some(r) = &self.vals[id as usize] {
            if self.use_vars
                && self.cross[id as usize]
                && self.def_block[id as usize] != self.cur_block
                && self.cross_vars.contains_key(&id)
            {
                return self.reload(id, r.clone());
            }
            return Ok(r.clone());
        }
        let r = match &v.kind {
            ValueKind::Const(c) => self.const_rval(c, v.ty)?,
            ValueKind::GlobalVar(g) => RVal::Ptr(self.global_ptr(*g)?),
            ValueKind::Function(fi) => RVal::Func(*fi),
            ValueKind::Arg(_) => return unsup("argument de fonction non inliné"),
            ValueKind::Inst | ValueKind::Forward => {
                return invalid(format!(
                    "valeur %{id} (définie bloc {}) utilisée avant sa définition (bloc courant {})",
                    self.def_block[id as usize] as i64, self.cur_block
                ));
            }
        };
        // Les constantes/globales sont indépendantes du bloc : on les met en cache (ids de
        // constantes globales valides partout ; pointeurs = recettes).
        self.vals[id as usize] = Some(r.clone());
        Ok(r)
    }

    pub fn scalar(&mut self, id: ValId) -> R<(u32, St)> {
        match self.val(id)? {
            RVal::S(i, s) => Ok((i, s)),
            _ => invalid("scalaire attendu"),
        }
    }

    /// Scalaire converti en `u32` (indices, coordonnées).
    pub fn scalar_u32(&mut self, id: ValId) -> R<u32> {
        let (i, st) = self.scalar(id)?;
        self.to_u32(i, st)
    }

    #[allow(clippy::wrong_self_convention)]
    pub fn to_u32(&mut self, i: u32, st: St) -> R<u32> {
        Ok(match st {
            St::I32 | St::I8 => i,
            St::I1 => {
                let one = self.c32(1);
                let z = self.c32(0);
                let t = self.sty(St::I32);
                self.s.emit(OP_SELECT, t, &[i, one, z])
            }
            St::I16 | St::I64 => {
                let t = self.sty(St::I32);
                self.s.emit(OP_U_CONVERT, t, &[i])
            }
            _ => return invalid("conversion flottant -> indice"),
        })
    }

    pub fn define(&mut self, id: ValId, v: RVal) -> R<()> {
        let idx = id as usize;
        if idx >= self.vals.len() {
            return invalid("résultat hors bornes");
        }
        if self.use_vars && self.cross[idx] {
            let mut leaves = Vec::new();
            collect_leaves(&v, &mut leaves);
            if !leaves.is_empty() {
                let mut vars = Vec::new();
                for (sid, st) in leaves {
                    let t = self.sty(st);
                    let var = self.hoist(t, SC_FUNCTION, None);
                    self.s.emit_void(OP_STORE, &[var, sid]);
                    vars.push((var, st));
                }
                self.cross_vars.insert(id, vars);
            }
        }
        self.vals[idx] = Some(v);
        Ok(())
    }

    fn reload(&mut self, id: ValId, template: RVal) -> R<RVal> {
        let vars = self.cross_vars.get(&id).cloned().unwrap_or_default();
        let mut it = vars.into_iter();
        fn rebuild(t: &mut Tr, v: &RVal, it: &mut std::vec::IntoIter<(u32, St)>) -> R<RVal> {
            Ok(match v {
                RVal::S(..) => {
                    let Some((var, st)) = it.next() else {
                        return invalid("variable de valeur croisée manquante");
                    };
                    let ty = t.sty(st);
                    let id = t.s.emit(OP_LOAD, ty, &[var]);
                    RVal::S(id, st)
                }
                RVal::Agg(xs) => {
                    let mut out = Vec::new();
                    for x in xs {
                        out.push(rebuild(t, x, it)?);
                    }
                    RVal::Agg(out)
                }
                other => other.clone(),
            })
        }
        rebuild(self, &template, &mut it)
    }

    /// Crée une variable hoistée en tête de fonction (classe `Function`) ou globale.
    pub fn hoist(&mut self, ty: u32, class: u32, init: Option<u32>) -> u32 {
        let pty = self.s.ptr(class, ty);
        let id = self.s.id();
        if class == SC_FUNCTION {
            self.hoisted.push((id, pty, init));
        } else {
            let mut ops = vec![pty, id, class];
            if let Some(i) = init {
                ops.push(i);
            }
            self.s.global_raw(59, &ops);
        }
        id
    }

    // ------------------------------------------------------------------ globales / mémoire

    fn global_ptr(&mut self, g: usize) -> R<Ptr> {
        if let Some(p) = self.globals.get(&g) {
            return Ok(p.clone());
        }
        let m = self.m;
        let gv = &m.globals[g];
        let class = match gv.addr {
            3 => SC_WORKGROUP,
            0 | 2 => SC_PRIVATE,
            a => return unsup(format!("espace d'adresses {a} non supporté")),
        };
        let mt = self.mem_ty(gv.elem)?;
        let init = match (gv.init, class) {
            (Some(i), SC_PRIVATE) => self.const_spv(i, gv.elem).ok(),
            _ => None,
        };
        let var = self.hoist(mt, class, init);
        let p = Ptr {
            base: var,
            class,
            steps: vec![],
            ty: gv.elem,
        };
        self.globals.insert(g, p.clone());
        Ok(p)
    }

    /// Constante SPIR-V (éventuellement composite) pour un initialiseur global.
    fn const_spv(&mut self, id: ValId, t: TyId) -> R<u32> {
        if self.depth > 48 {
            return invalid("initialiseur imbriqué trop profondément (cycle ?)");
        }
        self.depth += 1;
        let r = self.const_spv_inner(id, t);
        self.depth -= 1;
        r
    }

    fn const_spv_inner(&mut self, id: ValId, t: TyId) -> R<u32> {
        let m = self.m;
        let Some(v) = self.f.values.get(id as usize) else {
            return invalid("initialiseur hors bornes");
        };
        let kind = v.kind.clone();
        let ValueKind::Const(c) = kind else {
            return unsup("initialiseur non constant");
        };
        let mt = self.mem_ty(t)?;
        match (c, m.ty(t).clone()) {
            (Const::Null | Const::Undef, _) => Ok(self.s.c_null(mt)),
            (Const::Int(v) | Const::Float(v), Ty::Int(1)) => Ok(self.cst(St::I32, v & 1)),
            (Const::Int(v) | Const::Float(v), _) => {
                let st = st_of_ty(m, t)?;
                Ok(self.cst(st, v))
            }
            (Const::Aggregate(ids), Ty::Struct(fs)) => {
                let mut cs = Vec::new();
                for (i, x) in ids.iter().enumerate() {
                    let Some(&ft) = fs.get(i) else {
                        return invalid("agrégat trop long");
                    };
                    cs.push(self.const_spv(*x, ft)?);
                }
                Ok(self.s.c_composite(mt, &cs))
            }
            (Const::Aggregate(ids), Ty::Array(_, e) | Ty::Vector(_, e)) => {
                let mut cs = Vec::new();
                for x in ids {
                    cs.push(self.const_spv(x, e)?);
                }
                Ok(self.s.c_composite(mt, &cs))
            }
            (Const::Data(words), Ty::Array(_, e) | Ty::Vector(_, e)) => {
                let st = st_of_ty(m, e)?;
                let st = if st == St::I1 { St::I32 } else { st };
                let cs: Vec<u32> = words.iter().map(|&w| self.cst(st, w)).collect();
                Ok(self.s.c_composite(mt, &cs))
            }
            _ => unsup("initialiseur global non supporté"),
        }
    }

    pub fn emit_ptr(&mut self, p: &Ptr) -> R<u32> {
        if p.steps.is_empty() {
            return Ok(p.base);
        }
        let mt = self.mem_ty(p.ty)?;
        let pt = self.s.ptr(p.class, mt);
        let mut ops = vec![p.base];
        for st in &p.steps {
            ops.push(match st {
                Step::C(c) => self.c32(*c),
                Step::V(v) => self.scalar_u32(*v)?,
            });
        }
        Ok(self.s.emit(OP_ACCESS_CHAIN, pt, &ops))
    }

    /// Charge un scalaire depuis la mémoire (conversion bool <-> u32).
    pub fn load_scalar(&mut self, ptr: u32, t: TyId) -> R<RVal> {
        let st = st_of_ty(self.m, t)?;
        if matches!(self.m.ty(t), Ty::Int(1)) {
            let u = self.sty(St::I32);
            let w = self.s.emit(OP_LOAD, u, &[ptr]);
            let z = self.c32(0);
            let bt = self.sty(St::I1);
            let b = self.s.emit(OP_I_NOT_EQUAL, bt, &[w, z]);
            return Ok(RVal::S(b, St::I1));
        }
        let ty = self.sty(st);
        let id = self.s.emit(OP_LOAD, ty, &[ptr]);
        Ok(RVal::S(id, st))
    }

    pub fn store_scalar(&mut self, ptr: u32, t: TyId, v: &RVal) -> R<()> {
        let RVal::S(id, st) = v else {
            return unsup("stockage d'un agrégat");
        };
        let val = if matches!(self.m.ty(t), Ty::Int(1)) || *st == St::I1 {
            let one = self.c32(1);
            let z = self.c32(0);
            let u = self.sty(St::I32);
            if *st == St::I1 {
                self.s.emit(OP_SELECT, u, &[*id, one, z])
            } else {
                *id
            }
        } else {
            *id
        };
        self.s.emit_void(OP_STORE, &[ptr, val]);
        Ok(())
    }

    // ------------------------------------------------------------------ instructions

    pub fn emit_block(&mut self, b: usize) -> R<()> {
        let f = self.f;
        self.cur_block = b;
        let Some(bb) = f.blocks.get(b) else {
            return invalid("bloc hors bornes");
        };
        // Les phi d'abord : lecture de leurs variables.
        for inst in &bb.insts {
            if let (Op::Phi(_), Some(r)) = (&inst.op, inst.result) {
                self.load_phi(r)?;
            }
        }
        let n = bb.insts.len();
        for (k, inst) in bb.insts.iter().enumerate() {
            if matches!(inst.op, Op::Phi(_)) {
                continue;
            }
            if k + 1 == n && is_terminator(&inst.op) {
                break;
            }
            let r = self.inst(inst.result, &inst.op)?;
            if let (Some(id), Some(v)) = (inst.result, r) {
                self.define(id, v)?;
            }
        }
        Ok(())
    }

    fn phi_vars_for(&mut self, id: ValId) -> R<Vec<(u32, St)>> {
        if let Some(v) = self.phi_vars.get(&id) {
            return Ok(v.clone());
        }
        let t = self.f.values[id as usize].ty;
        let mut leaves = Vec::new();
        self.leaves(t, &mut leaves)?;
        let mut vars = Vec::new();
        for st in leaves {
            let ty = self.sty(st);
            let v = self.hoist(ty, SC_FUNCTION, None);
            vars.push((v, st));
        }
        self.phi_vars.insert(id, vars.clone());
        Ok(vars)
    }

    fn load_phi(&mut self, id: ValId) -> R<()> {
        let vars = self.phi_vars_for(id)?;
        let t = self.f.values[id as usize].ty;
        let template = self.zero_val(t)?;
        let mut it = vars.into_iter();
        fn rebuild(t: &mut Tr, v: &RVal, it: &mut std::vec::IntoIter<(u32, St)>) -> R<RVal> {
            Ok(match v {
                RVal::S(..) => {
                    let Some((var, st)) = it.next() else {
                        return invalid("variable de phi manquante");
                    };
                    let ty = t.sty(st);
                    let id = t.s.emit(OP_LOAD, ty, &[var]);
                    RVal::S(id, st)
                }
                RVal::Agg(xs) => {
                    let mut out = Vec::new();
                    for x in xs {
                        out.push(rebuild(t, x, it)?);
                    }
                    RVal::Agg(out)
                }
                other => other.clone(),
            })
        }
        let v = rebuild(self, &template, &mut it)?;
        self.define(id, v)
    }

    /// Affectations des phi du bloc `to` pour l'arête `from -> to`.
    pub fn edge(&mut self, from: usize, to: usize) -> R<()> {
        let f = self.f;
        let Some(tb) = f.blocks.get(to) else {
            return invalid("arête vers un bloc hors bornes");
        };
        let saved = self.cur_block;
        self.cur_block = from;
        let mut work: Vec<(Vec<(u32, St)>, RVal)> = Vec::new();
        for inst in &tb.insts {
            let (Op::Phi(inc), Some(r)) = (&inst.op, inst.result) else {
                continue;
            };
            let Some(&(v, _)) = inc.iter().find(|x| x.1 as usize == from) else {
                continue;
            };
            let vars = self.phi_vars_for(r)?;
            let val = self.val(v)?;
            work.push((vars, val));
        }
        for (vars, val) in work {
            let mut leaves = Vec::new();
            collect_leaves(&val, &mut leaves);
            for ((var, st), (sid, vst)) in vars.iter().zip(leaves) {
                let sid = self.convert_scalar(sid, vst, *st)?;
                self.s.emit_void(OP_STORE, &[*var, sid]);
            }
        }
        self.cur_block = saved;
        Ok(())
    }

    fn convert_scalar(&mut self, id: u32, from: St, to: St) -> R<u32> {
        if from == to
            || (from.bits() == 32 && to.bits() == 32 && !from.is_float() && !to.is_float())
        {
            return Ok(id);
        }
        invalid("types de phi incohérents")
    }

    fn inst(&mut self, result: Option<ValId>, op: &Op) -> R<Option<RVal>> {
        match op {
            Op::Binop { op, a, b } => {
                let (x, xs) = self.scalar(*a)?;
                let (y, _) = self.scalar(*b)?;
                Ok(Some(self.binop(*op, x, y, xs)?))
            }
            Op::Cast { op, a } => {
                let rt = self.f.values[result.unwrap_or(0) as usize].ty;
                let v = self.val(*a)?;
                Ok(Some(self.cast(*op, v, rt)?))
            }
            Op::Cmp { pred, a, b } => {
                let (x, xs) = self.scalar(*a)?;
                let (y, _) = self.scalar(*b)?;
                Ok(Some(self.cmp(*pred, x, y, xs)?))
            }
            Op::Select { c, a, b } => {
                let (cid, _) = self.scalar(*c)?;
                let va = self.val(*a)?;
                let vb = self.val(*b)?;
                Ok(Some(self.select(cid, &va, &vb)?))
            }
            Op::Gep { base, indices } => {
                let RVal::Ptr(mut p) = self.val(*base)? else {
                    return unsup("GEP sur une valeur non adresse");
                };
                let rt = self.f.values[result.unwrap_or(0) as usize].ty;
                for (k, &ix) in indices.iter().enumerate() {
                    if k == 0 {
                        if self.const_u(ix) != Some(0) {
                            return unsup("arithmétique de pointeur (indice de tête non nul)");
                        }
                        continue;
                    }
                    match self.const_u(ix) {
                        Some(c) => p.steps.push(Step::C(c as u32)),
                        None => p.steps.push(Step::V(ix)),
                    }
                }
                p.ty = match self.m.ty(rt) {
                    Ty::Ptr { elem, .. } => *elem,
                    _ => return invalid("GEP de type non pointeur"),
                };
                Ok(Some(RVal::Ptr(p)))
            }
            Op::Alloca { ty } => {
                let mt = self.mem_ty(*ty)?;
                let var = self.hoist(mt, SC_FUNCTION, None);
                Ok(Some(RVal::Ptr(Ptr {
                    base: var,
                    class: SC_FUNCTION,
                    steps: vec![],
                    ty: *ty,
                })))
            }
            Op::Load { ptr } => {
                let RVal::Ptr(p) = self.val(*ptr)? else {
                    return unsup("load d'une valeur non adresse");
                };
                let id = self.emit_ptr(&p)?;
                Ok(Some(self.load_scalar(id, p.ty)?))
            }
            Op::Store { ptr, val } => {
                let RVal::Ptr(p) = self.val(*ptr)? else {
                    return unsup("store vers une valeur non adresse");
                };
                let v = self.val(*val)?;
                let id = self.emit_ptr(&p)?;
                self.store_scalar(id, p.ty, &v)?;
                Ok(None)
            }
            Op::ExtractVal { agg, idx } => {
                let mut v = self.val(*agg)?;
                for &i in idx {
                    v = match v {
                        RVal::Agg(xs) => match xs.into_iter().nth(i as usize) {
                            Some(x) => x,
                            None => return invalid("extractvalue hors bornes"),
                        },
                        _ => return invalid("extractvalue sur un non-agrégat"),
                    };
                }
                Ok(Some(v))
            }
            Op::InsertVal { agg, val, idx } => {
                let a = self.val(*agg)?;
                let v = self.val(*val)?;
                fn ins(a: RVal, v: RVal, idx: &[u32]) -> R<RVal> {
                    let Some((&i, rest)) = idx.split_first() else {
                        return Ok(v);
                    };
                    let RVal::Agg(mut xs) = a else {
                        return invalid("insertvalue sur un non-agrégat");
                    };
                    let Some(slot) = xs.get_mut(i as usize) else {
                        return invalid("insertvalue hors bornes");
                    };
                    let old = std::mem::replace(slot, RVal::Func(0));
                    *slot = ins(old, v, rest)?;
                    Ok(RVal::Agg(xs))
                }
                Ok(Some(ins(a, v, idx)?))
            }
            Op::Call { callee, args } => self.call(result, *callee, args),
            Op::AtomicRmw { op, ptr, val } => {
                let RVal::Ptr(p) = self.val(*ptr)? else {
                    return unsup("atomicrmw sur une non-adresse");
                };
                let (v, st) = self.scalar(*val)?;
                let id = self.emit_ptr(&p)?;
                let scope = self.c32(if p.class == SC_WORKGROUP { 2 } else { 1 });
                let sem = self.c32(0);
                let t = self.sty(st);
                let (spop, v) = match op {
                    0 => (OP_ATOMIC_EXCHANGE, v),
                    1 => (OP_ATOMIC_I_ADD, v),
                    2 => (OP_ATOMIC_I_SUB, v),
                    3 => (OP_ATOMIC_AND, v),
                    5 => (OP_ATOMIC_OR, v),
                    6 => (OP_ATOMIC_XOR, v),
                    7 => (OP_ATOMIC_S_MAX, v),
                    8 => (OP_ATOMIC_S_MIN, v),
                    9 => (OP_ATOMIC_U_MAX, v),
                    10 => (OP_ATOMIC_U_MIN, v),
                    o => return unsup(format!("atomicrmw opération {o}")),
                };
                let r = self.s.emit(spop, t, &[id, scope, sem, v]);
                Ok(Some(RVal::S(r, st)))
            }
            Op::CmpXchg { ptr, cmp, new } => {
                let RVal::Ptr(p) = self.val(*ptr)? else {
                    return unsup("cmpxchg sur une non-adresse");
                };
                let (c, st) = self.scalar(*cmp)?;
                let (n, _) = self.scalar(*new)?;
                let id = self.emit_ptr(&p)?;
                let scope = self.c32(if p.class == SC_WORKGROUP { 2 } else { 1 });
                let sem = self.c32(0);
                let t = self.sty(st);
                let old = self
                    .s
                    .emit(OP_ATOMIC_COMPARE_EXCHANGE, t, &[id, scope, sem, sem, n, c]);
                let bt = self.sty(St::I1);
                let ok = self.s.emit(OP_I_EQUAL, bt, &[old, c]);
                Ok(Some(RVal::Agg(vec![RVal::S(old, st), RVal::S(ok, St::I1)])))
            }
            Op::Fence => Ok(None),
            Op::Phi(_)
            | Op::Ret(_)
            | Op::Br(_)
            | Op::CondBr { .. }
            | Op::Switch { .. }
            | Op::Unreachable => Ok(None),
            Op::Unsupported(c) => unsup(format!("instruction LLVM de code {c}")),
        }
    }

    fn binop(&mut self, op: u32, x: u32, y: u32, st: St) -> R<RVal> {
        let t = self.sty(st);
        let o = if st == St::I1 {
            match op {
                10 => OP_LOGICAL_AND,
                11 => OP_LOGICAL_OR,
                12 => OP_LOGICAL_NOT_EQUAL,
                _ => return unsup(format!("binop {op} sur i1")),
            }
        } else if st.is_float() {
            match op {
                0 => OP_F_ADD,
                1 => OP_F_SUB,
                2 => OP_F_MUL,
                4 => OP_F_DIV,
                6 => OP_F_REM,
                _ => return unsup(format!("binop flottant {op}")),
            }
        } else {
            match op {
                0 => OP_I_ADD,
                1 => OP_I_SUB,
                2 => OP_I_MUL,
                3 => OP_U_DIV,
                4 => OP_S_DIV,
                5 => OP_U_MOD,
                6 => OP_S_REM,
                7 => OP_SHIFT_LEFT_LOGICAL,
                8 => OP_SHIFT_RIGHT_LOGICAL,
                9 => OP_SHIFT_RIGHT_ARITHMETIC,
                10 => OP_BITWISE_AND,
                11 => OP_BITWISE_OR,
                12 => OP_BITWISE_XOR,
                _ => return unsup(format!("binop entier {op}")),
            }
        };
        let id = self.s.emit(o, t, &[x, y]);
        Ok(RVal::S(id, st))
    }

    fn cast(&mut self, op: u32, v: RVal, rt: TyId) -> R<RVal> {
        let to = match self.m.ty(rt) {
            Ty::Ptr { .. } => {
                if let RVal::Ptr(_) = v {
                    return Ok(v);
                }
                return unsup("cast vers pointeur");
            }
            _ => st_of_ty(self.m, rt)?,
        };
        let RVal::S(x, from) = v else {
            if let RVal::Ptr(_) = v {
                return Ok(v);
            }
            return unsup("cast d'un agrégat");
        };
        let tt = self.sty(to);
        let id = match op {
            0 => {
                // trunc
                if to == St::I1 {
                    let one = self.cst(from, 1);
                    let ft = self.sty(from);
                    let a = self.s.emit(OP_BITWISE_AND, ft, &[x, one]);
                    let z = self.cst(from, 0);
                    self.s.emit(OP_I_NOT_EQUAL, tt, &[a, z])
                } else if from.bits() == to.bits() || (from.bits() <= 32 && to.bits() == 32) {
                    x
                } else if to == St::I8 {
                    let ft = self.sty(from);
                    let m = self.cst(from, 0xFF);
                    if from == St::I32 {
                        self.s.emit(OP_BITWISE_AND, ft, &[x, m])
                    } else {
                        let u = self.s.emit(OP_U_CONVERT, tt, &[x]);
                        let m8 = self.c32(0xFF);
                        self.s.emit(OP_BITWISE_AND, tt, &[u, m8])
                    }
                } else {
                    self.s.emit(OP_U_CONVERT, tt, &[x])
                }
            }
            1 => {
                // zext
                if from == St::I1 {
                    let one = self.cst(to, 1);
                    let z = self.cst(to, 0);
                    self.s.emit(OP_SELECT, tt, &[x, one, z])
                } else if from.bits() == to.bits() || (from == St::I8 && to == St::I32) {
                    x
                } else {
                    self.s.emit(OP_U_CONVERT, tt, &[x])
                }
            }
            2 => {
                if from == St::I1 {
                    let ones = self.cst(to, u64::MAX);
                    let z = self.cst(to, 0);
                    self.s.emit(OP_SELECT, tt, &[x, ones, z])
                } else if from.bits() == to.bits() {
                    x
                } else {
                    self.s.emit(OP_S_CONVERT, tt, &[x])
                }
            }
            3 => self.s.emit(OP_CONVERT_F_TO_U, tt, &[x]),
            4 => self.s.emit(OP_CONVERT_F_TO_S, tt, &[x]),
            5 => {
                if from == St::I1 {
                    let one = self.cst(
                        to,
                        if to == St::F32 {
                            u64::from(1.0f32.to_bits())
                        } else {
                            0
                        },
                    );
                    let _ = one;
                    return unsup("uitofp depuis i1");
                }
                self.s.emit(OP_CONVERT_U_TO_F, tt, &[x])
            }
            6 => self.s.emit(OP_CONVERT_S_TO_F, tt, &[x]),
            7 | 8 => {
                if from == to {
                    x
                } else {
                    self.s.emit(OP_F_CONVERT, tt, &[x])
                }
            }
            11 => {
                if from == to {
                    x
                } else if from.bits() == to.bits() {
                    self.s.emit(OP_BITCAST, tt, &[x])
                } else {
                    return unsup("bitcast entre largeurs différentes");
                }
            }
            o => return unsup(format!("cast LLVM {o}")),
        };
        Ok(RVal::S(id, to))
    }

    fn cmp(&mut self, pred: u32, x: u32, y: u32, st: St) -> R<RVal> {
        let bt = self.sty(St::I1);
        let id = if st.is_float() {
            match pred {
                0 => self.s.c_bool(false),
                15 => self.s.c_bool(true),
                1 => self.s.emit(OP_F_ORD_EQUAL, bt, &[x, y]),
                2 => self.s.emit(OP_F_ORD_GREATER_THAN, bt, &[x, y]),
                3 => self.s.emit(OP_F_ORD_GREATER_THAN_EQUAL, bt, &[x, y]),
                4 => self.s.emit(OP_F_ORD_LESS_THAN, bt, &[x, y]),
                5 => self.s.emit(OP_F_ORD_LESS_THAN_EQUAL, bt, &[x, y]),
                6 => self.s.emit(OP_F_ORD_NOT_EQUAL, bt, &[x, y]),
                7 | 8 => {
                    // ord : x == x && y == y ; uno : sa négation (OpOrdered est Kernel-only).
                    let a = self.s.emit(OP_F_ORD_EQUAL, bt, &[x, x]);
                    let b = self.s.emit(OP_F_ORD_EQUAL, bt, &[y, y]);
                    let both = self.s.emit(OP_LOGICAL_AND, bt, &[a, b]);
                    if pred == 7 {
                        both
                    } else {
                        self.s.emit(OP_LOGICAL_NOT, bt, &[both])
                    }
                }
                9 => self.s.emit(OP_F_UNORD_EQUAL, bt, &[x, y]),
                10 => self.s.emit(OP_F_UNORD_GREATER_THAN, bt, &[x, y]),
                11 => self.s.emit(OP_F_UNORD_GREATER_THAN_EQUAL, bt, &[x, y]),
                12 => self.s.emit(OP_F_UNORD_LESS_THAN, bt, &[x, y]),
                13 => self.s.emit(OP_F_UNORD_LESS_THAN_EQUAL, bt, &[x, y]),
                14 => self.s.emit(OP_F_UNORD_NOT_EQUAL, bt, &[x, y]),
                o => return unsup(format!("prédicat fcmp {o}")),
            }
        } else if st == St::I1 {
            match pred {
                32 => self.s.emit(OP_LOGICAL_EQUAL, bt, &[x, y]),
                33 => self.s.emit(OP_LOGICAL_NOT_EQUAL, bt, &[x, y]),
                o => return unsup(format!("prédicat icmp {o} sur i1")),
            }
        } else {
            let o = match pred {
                32 => OP_I_EQUAL,
                33 => OP_I_NOT_EQUAL,
                34 => OP_U_GREATER_THAN,
                35 => OP_U_GREATER_THAN_EQUAL,
                36 => OP_U_LESS_THAN,
                37 => OP_U_LESS_THAN_EQUAL,
                38 => OP_S_GREATER_THAN,
                39 => OP_S_GREATER_THAN_EQUAL,
                40 => OP_S_LESS_THAN,
                41 => OP_S_LESS_THAN_EQUAL,
                o => return unsup(format!("prédicat icmp {o}")),
            };
            self.s.emit(o, bt, &[x, y])
        };
        Ok(RVal::S(id, St::I1))
    }

    fn select(&mut self, c: u32, a: &RVal, b: &RVal) -> R<RVal> {
        match (a, b) {
            (RVal::S(x, st), RVal::S(y, _)) => {
                let t = self.sty(*st);
                let id = self.s.emit(OP_SELECT, t, &[c, *x, *y]);
                Ok(RVal::S(id, *st))
            }
            (RVal::Agg(xs), RVal::Agg(ys)) if xs.len() == ys.len() => {
                let mut out = Vec::new();
                for (x, y) in xs.iter().zip(ys) {
                    out.push(self.select(c, x, y)?);
                }
                Ok(RVal::Agg(out))
            }
            _ => unsup("select sur pointeurs / handles"),
        }
    }

    fn call(&mut self, result: Option<ValId>, callee: ValId, args: &[ValId]) -> R<Option<RVal>> {
        let RVal::Func(fi) = self.val(callee)? else {
            return unsup("appel indirect");
        };
        let name = self.m.functions[fi].name.clone();
        if let Some(rest) = name.strip_prefix("dx.op.") {
            let rt = result.map(|r| self.f.values[r as usize].ty);
            return self.dx_op(rest, rt, args);
        }
        if name.starts_with("llvm.lifetime") || name.starts_with("llvm.dbg") {
            return Ok(None);
        }
        unsup(format!("appel de la fonction non inlinée « {name} »"))
    }

    // ------------------------------------------------------------------ flux de contrôle

    pub fn bool_val(&mut self, id: ValId) -> R<u32> {
        let (c, st) = self.scalar(id)?;
        if st != St::I1 {
            return invalid("condition non booléenne");
        }
        Ok(c)
    }

    fn innermost_loop(&mut self) -> Option<usize> {
        self.cx.iter().rposition(|c| matches!(c, Cx::Loop { .. }))
    }

    pub fn lower(&mut self, a: &Ast) -> R<()> {
        match a {
            Ast::Nop => Ok(()),
            Ast::Code(b) => self.emit_block(*b),
            Ast::Seq(v) => {
                for x in v {
                    if self.s.terminated {
                        break;
                    }
                    self.lower(x)?;
                }
                Ok(())
            }
            Ast::Edge(u, t) => self.edge(*u, *t),
            Ast::Ret => {
                self.s.terminate(OP_RETURN, &[]);
                Ok(())
            }
            Ast::Unreachable => {
                self.s.terminate(OP_UNREACHABLE, &[]);
                Ok(())
            }
            Ast::Br(l) => self.lower_exit(false, *l),
            Ast::Cont(l) => self.lower_exit(true, *l),
            Ast::If { cond, then, els } => self.lower_if(*cond, then, els),
            Ast::Switch { sel, arms, default } => self.lower_switch(*sel, arms, default),
            Ast::Block { label, body } => self.lower_loop(&[*label], body, None, false),
            Ast::Loop { labels, body, cont } => {
                self.lower_loop(labels, body, cont.as_deref(), true)
            }
            Ast::SetState(n) => {
                let var = self.state();
                let c = self.c32(*n as u32);
                self.s.emit_void(OP_STORE, &[var, c]);
                Ok(())
            }
            Ast::SwitchState(arms) => self.lower_switch_state(arms),
        }
    }

    fn state(&mut self) -> u32 {
        if let Some(v) = self.state_var {
            return v;
        }
        let t = self.sty(St::I32);
        let v = self.hoist(t, SC_FUNCTION, None);
        self.state_var = Some(v);
        v
    }

    /// `break` (`cont == false`) ou `continue` vers l'étiquette `l`, éventuellement à travers
    /// plusieurs constructions (variable d'échappement + test après chaque fusion).
    fn lower_exit(&mut self, cont: bool, l: usize) -> R<()> {
        let Some(i) = self.innermost_loop() else {
            return invalid("break/continue hors boucle");
        };
        let direct = match &self.cx[i] {
            Cx::Loop { labels, real, .. } => {
                if cont {
                    *real && labels.first() == Some(&l)
                } else {
                    labels.contains(&l)
                }
            }
            Cx::Sel => false,
        };
        if direct {
            let Cx::Loop {
                merge,
                cont: c,
                merge_used,
                cont_used,
                ..
            } = &mut self.cx[i]
            else {
                return invalid("construction incohérente");
            };
            let target = if cont {
                *cont_used = true;
                *c
            } else {
                *merge_used = true;
                *merge
            };
            self.s.terminate(OP_BRANCH, &[target]);
            return Ok(());
        }
        // Échappement multi-niveaux.
        let exists = self.cx.iter().any(|c| match c {
            Cx::Loop { labels, real, .. } => {
                if cont {
                    *real && labels.first() == Some(&l)
                } else {
                    labels.contains(&l)
                }
            }
            Cx::Sel => false,
        });
        if !exists {
            return invalid("cible de break/continue introuvable");
        }
        let code = (l as u32 + 1) * 2 + u32::from(cont);
        let esc = self.esc();
        let c = self.c32(code);
        self.s.emit_void(OP_STORE, &[esc, c]);
        let Cx::Loop {
            merge,
            merge_used,
            pending,
            ..
        } = &mut self.cx[i]
        else {
            return invalid("construction incohérente");
        };
        *merge_used = true;
        if !pending.contains(&(cont, l)) {
            pending.push((cont, l));
        }
        let m = *merge;
        self.s.terminate(OP_BRANCH, &[m]);
        Ok(())
    }

    fn esc(&mut self) -> u32 {
        if let Some(v) = self.esc_var {
            return v;
        }
        let t = self.sty(St::I32);
        let z = self.c32(0);
        let v = self.hoist(t, SC_FUNCTION, Some(z));
        self.esc_var = Some(v);
        v
    }

    /// Après la fusion d'une construction : propage les sorties multi-niveaux en attente.
    fn flush_pending(&mut self, pending: Vec<(bool, usize)>) -> R<()> {
        for (cont, l) in pending {
            if self.s.terminated {
                break;
            }
            let esc = self.esc();
            let u = self.sty(St::I32);
            let cur = self.s.emit(OP_LOAD, u, &[esc]);
            let code = self.c32((l as u32 + 1) * 2 + u32::from(cont));
            let bt = self.sty(St::I1);
            let eq = self.s.emit(OP_I_EQUAL, bt, &[cur, code]);
            let t = self.s.id();
            let m = self.s.id();
            self.s.emit_raw(OP_SELECTION_MERGE, &[m, 0]);
            self.s.terminate(OP_BRANCH_CONDITIONAL, &[eq, t, m]);
            self.s.begin_block(t);
            let z = self.c32(0);
            self.s.emit_void(OP_STORE, &[esc, z]);
            self.cx.push(Cx::Sel);
            let r = self.lower_exit(cont, l);
            self.cx.pop();
            r?;
            self.s.begin_block(m);
        }
        Ok(())
    }

    fn lower_if(&mut self, cond: ValId, then: &Ast, els: &Ast) -> R<()> {
        let c = self.bool_val(cond)?;
        let m = self.s.id();
        let t_empty = matches!(then, Ast::Nop);
        let e_empty = matches!(els, Ast::Nop);
        let tl = if t_empty { m } else { self.s.id() };
        let el = if e_empty { m } else { self.s.id() };
        self.s.ensure_block();
        self.s.emit_raw(OP_SELECTION_MERGE, &[m, 0]);
        self.s.terminate(OP_BRANCH_CONDITIONAL, &[c, tl, el]);
        let mut used = t_empty || e_empty;
        self.cx.push(Cx::Sel);
        if !t_empty {
            self.s.begin_block(tl);
            self.lower(then)?;
            if !self.s.terminated {
                self.s.terminate(OP_BRANCH, &[m]);
                used = true;
            }
        }
        if !e_empty {
            self.s.begin_block(el);
            self.lower(els)?;
            if !self.s.terminated {
                self.s.terminate(OP_BRANCH, &[m]);
                used = true;
            }
        }
        self.cx.pop();
        self.s.begin_block(m);
        if !used {
            self.s.terminate(OP_UNREACHABLE, &[]);
        }
        Ok(())
    }

    fn lower_switch(&mut self, sel: ValId, arms: &[(Vec<u64>, Ast)], default: &Ast) -> R<()> {
        let (s, st) = self.scalar(sel)?;
        let s = match st {
            St::I32 | St::I8 => s,
            St::I16 => self.to_u32(s, st)?,
            _ => return unsup("sélecteur de switch non 32 bits"),
        };
        let m = self.s.id();
        let d_empty = matches!(default, Ast::Nop);
        let dl = if d_empty { m } else { self.s.id() };
        let mut labels = Vec::new();
        let mut used = d_empty;
        for (_, a) in arms {
            if matches!(a, Ast::Nop) {
                labels.push(m);
                used = true;
            } else {
                labels.push(self.s.id());
            }
        }
        self.s.ensure_block();
        self.s.emit_raw(OP_SELECTION_MERGE, &[m, 0]);
        let mut ops = vec![s, dl];
        let mut seen = std::collections::BTreeSet::new();
        for ((vals, _), l) in arms.iter().zip(&labels) {
            for &v in vals {
                if seen.insert(v as u32) {
                    ops.push(v as u32);
                    ops.push(*l);
                }
            }
        }
        self.s.terminate(OP_SWITCH, &ops);
        self.cx.push(Cx::Sel);
        for ((_, a), l) in arms.iter().zip(&labels) {
            if matches!(a, Ast::Nop) {
                continue;
            }
            self.s.begin_block(*l);
            self.lower(a)?;
            if !self.s.terminated {
                self.s.terminate(OP_BRANCH, &[m]);
                used = true;
            }
        }
        if !d_empty {
            self.s.begin_block(dl);
            self.lower(default)?;
            if !self.s.terminated {
                self.s.terminate(OP_BRANCH, &[m]);
                used = true;
            }
        }
        self.cx.pop();
        self.s.begin_block(m);
        if !used {
            self.s.terminate(OP_UNREACHABLE, &[]);
        }
        Ok(())
    }

    fn lower_switch_state(&mut self, arms: &[(usize, Ast)]) -> R<()> {
        let var = self.state();
        let t = self.sty(St::I32);
        let sel = self.s.emit(OP_LOAD, t, &[var]);
        let m = self.s.id();
        let dl = self.s.id();
        let labels: Vec<u32> = arms.iter().map(|_| self.s.id()).collect();
        self.s.ensure_block();
        self.s.emit_raw(OP_SELECTION_MERGE, &[m, 0]);
        let mut ops = vec![sel, dl];
        for ((b, _), l) in arms.iter().zip(&labels) {
            ops.push(*b as u32);
            ops.push(*l);
        }
        self.s.terminate(OP_SWITCH, &ops);
        self.cx.push(Cx::Sel);
        for ((_, a), l) in arms.iter().zip(&labels) {
            self.s.begin_block(*l);
            self.lower(a)?;
            if !self.s.terminated {
                self.s.terminate(OP_BRANCH, &[m]);
            }
        }
        self.cx.pop();
        self.s.begin_block(dl);
        self.s.terminate(OP_UNREACHABLE, &[]);
        self.s.begin_block(m);
        self.s.terminate(OP_UNREACHABLE, &[]);
        Ok(())
    }

    fn lower_loop(
        &mut self,
        labels: &[usize],
        body: &Ast,
        cont: Option<&Ast>,
        real: bool,
    ) -> R<()> {
        let h = self.s.id();
        let b = self.s.id();
        let c = self.s.id();
        let m = self.s.id();
        let first = labels.first().copied().unwrap_or(0);
        let uses_cont = real && ast_uses(body, &|x| matches!(x, Ast::Cont(l) if *l == first));
        self.s.ensure_block();
        self.s.begin_block(h);
        self.s.emit_raw(OP_LOOP_MERGE, &[m, c, 0]);
        if uses_cont {
            self.s.terminate(OP_BRANCH, &[b]);
        } else {
            let t = self.s.c_bool(true);
            self.s.terminate(OP_BRANCH_CONDITIONAL, &[t, b, c]);
        }
        self.s.begin_block(b);
        self.cx.push(Cx::Loop {
            labels: labels.to_vec(),
            merge: m,
            cont: c,
            real,
            merge_used: false,
            cont_used: false,
            pending: Vec::new(),
        });
        self.lower(body)?;
        let mut merge_used = false;
        if !self.s.terminated {
            self.s.terminate(OP_BRANCH, &[m]);
            merge_used = true;
        }
        let mut pending = Vec::new();
        if let Some(Cx::Loop {
            merge_used: mu,
            pending: p,
            ..
        }) = self.cx.pop()
        {
            merge_used |= mu;
            pending = p;
        }
        self.s.begin_block(c);
        if let Some(cc) = cont {
            self.lower(cc)?;
        }
        if !self.s.terminated {
            self.s.terminate(OP_BRANCH, &[h]);
        }
        self.s.begin_block(m);
        if !merge_used {
            self.s.terminate(OP_UNREACHABLE, &[]);
        } else {
            self.flush_pending(pending)?;
        }
        Ok(())
    }

    /// Insère les variables `Function` hoistées en tête de la fonction courante.
    pub fn flush_hoisted(&mut self, at: usize) {
        let list = std::mem::take(&mut self.hoisted);
        for (var, pty, init) in list.into_iter().rev() {
            let mut ops = vec![pty, var, SC_FUNCTION];
            if let Some(i) = init {
                ops.push(i);
            }
            self.s.insert_raw(at, OP_VARIABLE, &ops);
        }
    }
}

pub(super) fn is_terminator(op: &Op) -> bool {
    matches!(
        op,
        Op::Ret(_) | Op::Br(_) | Op::CondBr { .. } | Op::Switch { .. } | Op::Unreachable
    )
}

pub(super) fn collect_leaves(v: &RVal, out: &mut Vec<(u32, St)>) {
    match v {
        RVal::S(i, s) => out.push((*i, *s)),
        RVal::Agg(xs) => xs.iter().for_each(|x| collect_leaves(x, out)),
        _ => {}
    }
}
