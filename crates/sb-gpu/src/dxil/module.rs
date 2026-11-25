//! Interprétation du bitcode LLVM 3.7 : types, valeurs globales, constantes, métadonnées et
//! corps de fonctions (instructions) sous forme d'IR minimal exploitable par le traducteur.
#![allow(dead_code)] // métadonnées/IR décodées en entier (GS/HS/DS à venir) : champs non encore consommés

use super::DxilParseError;
use super::bitcode::{Block, Item, Record};

pub type TyId = u32;
pub type ValId = u32;

const MAX_VALUES: usize = 1 << 22;
const MAX_TYPES: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Void,
    Int(u32),
    Half,
    Float,
    Double,
    Label,
    Metadata,
    Ptr { elem: TyId, addr: u32 },
    Array(u64, TyId),
    Vector(u32, TyId),
    Struct(Vec<TyId>),
    Func { ret: TyId, params: Vec<TyId> },
    Opaque,
}

#[derive(Debug, Clone)]
pub enum Const {
    Null,
    Undef,
    Int(u64),
    Float(u64),
    Aggregate(Vec<ValId>),
    Data(Vec<u64>),
    CString(Vec<u8>),
    Gep { base: ValId, indices: Vec<ValId> },
    Cast { op: u32, val: ValId },
    Unsupported(u32),
}

#[derive(Debug, Clone)]
pub enum ValueKind {
    GlobalVar(usize),
    Function(usize),
    Const(Const),
    Arg(usize),
    Inst,
    Forward,
}

#[derive(Debug, Clone)]
pub struct Value {
    pub ty: TyId,
    pub kind: ValueKind,
}

#[derive(Debug, Clone)]
pub struct GlobalVar {
    pub name: String,
    /// Type de la valeur pointée.
    pub elem: TyId,
    pub addr: u32,
    pub is_const: bool,
    pub init: Option<ValId>,
}

#[derive(Debug, Clone)]
pub enum Op {
    Binop {
        op: u32,
        a: ValId,
        b: ValId,
    },
    Cast {
        op: u32,
        a: ValId,
    },
    Gep {
        base: ValId,
        indices: Vec<ValId>,
    },
    Select {
        c: ValId,
        a: ValId,
        b: ValId,
    },
    Cmp {
        pred: u32,
        a: ValId,
        b: ValId,
    },
    Ret(Option<ValId>),
    Br(u32),
    CondBr {
        cond: ValId,
        t: u32,
        f: u32,
    },
    Switch {
        cond: ValId,
        default: u32,
        cases: Vec<(u64, u32)>,
    },
    Unreachable,
    Phi(Vec<(ValId, u32)>),
    Alloca {
        ty: TyId,
    },
    Load {
        ptr: ValId,
    },
    Store {
        ptr: ValId,
        val: ValId,
    },
    ExtractVal {
        agg: ValId,
        idx: Vec<u32>,
    },
    InsertVal {
        agg: ValId,
        val: ValId,
        idx: Vec<u32>,
    },
    Call {
        callee: ValId,
        args: Vec<ValId>,
    },
    AtomicRmw {
        op: u32,
        ptr: ValId,
        val: ValId,
    },
    CmpXchg {
        ptr: ValId,
        cmp: ValId,
        new: ValId,
    },
    Fence,
    Unsupported(u32),
}

#[derive(Debug, Clone)]
pub struct Inst {
    pub result: Option<ValId>,
    pub op: Op,
}

#[derive(Debug, Clone, Default)]
pub struct BasicBlock {
    pub insts: Vec<Inst>,
}

#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    pub ty: TyId,
    pub is_proto: bool,
    pub blocks: Vec<BasicBlock>,
    /// Valeurs visibles dans cette fonction (globales + constantes + arguments + instructions).
    pub values: Vec<Value>,
    pub args: Vec<ValId>,
}

#[derive(Debug, Clone)]
pub enum Md {
    Str(String),
    /// Valeur LLVM (type, identifiant de valeur de niveau module).
    Value(TyId, ValId),
    Node(Vec<Option<u32>>),
    Other,
}

#[derive(Debug, Default)]
pub struct Module {
    pub types: Vec<Ty>,
    pub globals: Vec<GlobalVar>,
    pub functions: Vec<Function>,
    pub values: Vec<Value>,
    pub mds: Vec<Md>,
    pub named_mds: Vec<(String, Vec<u32>)>,
}

fn bad<T>(msg: impl Into<String>) -> Result<T, DxilParseError> {
    Err(DxilParseError::Module(msg.into()))
}

fn chars(ops: &[u64]) -> String {
    ops.iter().map(|&c| (c as u8) as char).collect()
}

fn sign_decode(v: u64) -> i64 {
    if v & 1 == 0 {
        (v >> 1) as i64
    } else if v == 1 {
        i64::MIN
    } else {
        -((v >> 1) as i64)
    }
}

impl Module {
    pub fn ty(&self, id: TyId) -> &Ty {
        self.types.get(id as usize).unwrap_or(&Ty::Opaque)
    }

    pub fn intern(&mut self, t: Ty) -> TyId {
        if let Some(p) = self.types.iter().position(|x| *x == t) {
            return p as TyId;
        }
        self.types.push(t);
        (self.types.len() - 1) as TyId
    }

    pub fn find_int(&self, width: u32) -> Option<TyId> {
        self.types
            .iter()
            .position(|x| *x == Ty::Int(width))
            .map(|p| p as TyId)
    }

    pub fn function_by_name(&self, name: &str) -> Option<usize> {
        self.functions.iter().position(|f| f.name == name)
    }

    /// Valeur de métadonnée `id` (si elle existe).
    pub fn md(&self, id: u32) -> Option<&Md> {
        self.mds.get(id as usize)
    }

    pub fn named_md(&self, name: &str) -> Option<&[u32]> {
        self.named_mds
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_slice())
    }

    /// Entier constant d'une valeur de niveau module (pour les métadonnées).
    pub fn const_int(&self, v: ValId) -> Option<u64> {
        match &self.values.get(v as usize)?.kind {
            ValueKind::Const(Const::Int(i)) => Some(*i),
            ValueKind::Const(Const::Null) => Some(0),
            _ => None,
        }
    }
}

// ------------------------------------------------------------------------- types

fn parse_types(m: &mut Module, blk: &Block) -> Result<(), DxilParseError> {
    let mut pending_name: Option<String> = None;
    for it in &blk.items {
        let Item::Record(r) = it else { continue };
        if m.types.len() >= MAX_TYPES {
            return bad("trop de types");
        }
        let o = &r.ops;
        let tid = |i: usize| -> Result<TyId, DxilParseError> {
            match o.get(i) {
                Some(&v) if (v as usize) < MAX_TYPES => Ok(v as TyId),
                _ => bad("identifiant de type invalide"),
            }
        };
        // Les types agrégés ne peuvent contenir que des types déjà définis (pas de cycle).
        let cur_len = m.types.len();
        let inner = |i: usize| -> Result<TyId, DxilParseError> {
            let t = tid(i)?;
            if (t as usize) < cur_len {
                Ok(t)
            } else {
                bad("type agrégé récursif ou référence avant")
            }
        };
        let t = match r.code {
            1 => continue, // NUMENTRY
            2 => Ty::Void,
            3 => Ty::Float,
            4 => Ty::Double,
            5 => Ty::Label,
            6 => {
                pending_name = None;
                Ty::Opaque
            }
            7 => Ty::Int(o.first().copied().unwrap_or(32) as u32),
            8 => Ty::Ptr {
                elem: tid(0)?,
                addr: o.get(1).copied().unwrap_or(0) as u32,
            },
            10 => Ty::Half,
            11 => Ty::Array(o.first().copied().unwrap_or(0), inner(1)?),
            12 => Ty::Vector(o.first().copied().unwrap_or(0) as u32, inner(1)?),
            16 => Ty::Metadata,
            18 | 20 => {
                pending_name = None;
                let mut f = Vec::new();
                for i in 1..o.len() {
                    f.push(inner(i)?);
                }
                Ty::Struct(f)
            }
            19 => {
                pending_name = Some(chars(o));
                continue;
            }
            21 => {
                let ret = tid(1)?;
                let mut params = Vec::new();
                for i in 2..o.len() {
                    params.push(tid(i)?);
                }
                Ty::Func { ret, params }
            }
            _ => Ty::Opaque,
        };
        let _ = &pending_name;
        m.types.push(t);
    }
    Ok(())
}

// ------------------------------------------------------------------------- constantes

fn int_bits(v: u64) -> u64 {
    // Entier signé encodé en « sign rotated VBR ».
    sign_decode(v) as u64
}

fn parse_constants(
    m: &mut Module,
    blk: &Block,
    values: &mut Vec<Value>,
    local_base: usize,
) -> Result<(), DxilParseError> {
    let _ = local_base;
    let mut cur_ty: TyId = 0;
    for it in &blk.items {
        let Item::Record(r) = it else { continue };
        if values.len() >= MAX_VALUES {
            return bad("trop de valeurs");
        }
        let o = &r.ops;
        let c = match r.code {
            1 => {
                cur_ty = o.first().copied().unwrap_or(0) as TyId;
                continue;
            }
            2 => Const::Null,
            3 => Const::Undef,
            4 => {
                let v = int_bits(o.first().copied().unwrap_or(0));
                let w = match m.ty(cur_ty) {
                    Ty::Int(w) => *w,
                    _ => 64,
                };
                let mask = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
                Const::Int(v & mask)
            }
            5 => Const::Int(o.first().copied().unwrap_or(0)),
            6 => Const::Float(o.first().copied().unwrap_or(0)),
            7 => Const::Aggregate(o.iter().map(|&v| v as ValId).collect()),
            8 => Const::CString(o.iter().map(|&v| v as u8).collect()),
            9 => {
                let mut s: Vec<u8> = o.iter().map(|&v| v as u8).collect();
                s.push(0);
                Const::CString(s)
            }
            11 => Const::Cast {
                op: o.first().copied().unwrap_or(0) as u32,
                val: o.get(2).copied().unwrap_or(0) as ValId,
            },
            12 | 20 | 24 => {
                // [source type?] + (type, value)*
                let mut i = 0;
                if o.len() % 2 == 1 {
                    i = 1;
                }
                let mut ids = Vec::new();
                while i + 1 < o.len() {
                    ids.push(o[i + 1] as ValId);
                    i += 2;
                }
                if ids.is_empty() {
                    Const::Unsupported(r.code)
                } else {
                    let base = ids.remove(0);
                    Const::Gep { base, indices: ids }
                }
            }
            22 => Const::Data(o.clone()),
            other => Const::Unsupported(other),
        };
        values.push(Value {
            ty: cur_ty,
            kind: ValueKind::Const(c),
        });
    }
    Ok(())
}

// ------------------------------------------------------------------------- métadonnées

fn parse_metadata(m: &mut Module, blk: &Block) -> Result<(), DxilParseError> {
    let mut name: Option<String> = None;
    for it in &blk.items {
        let Item::Record(r) = it else { continue };
        let o = &r.ops;
        match r.code {
            1 => m.mds.push(Md::Str(chars(o))),
            2 => m.mds.push(Md::Value(
                o.first().copied().unwrap_or(0) as TyId,
                o.get(1).copied().unwrap_or(0) as ValId,
            )),
            3 | 5 => m.mds.push(Md::Node(
                o.iter()
                    .map(|&v| if v == 0 { None } else { Some((v - 1) as u32) })
                    .collect(),
            )),
            4 => name = Some(chars(o)),
            10 => {
                let n = name.take().unwrap_or_default();
                m.named_mds.push((n, o.iter().map(|&v| v as u32).collect()));
            }
            35 => {
                let count = o.first().copied().unwrap_or(0) as usize;
                let off = o.get(1).copied().unwrap_or(0) as usize;
                let Some(blob) = &r.blob else {
                    return bad("METADATA_STRINGS sans blob");
                };
                if off > blob.len() || count > blob.len() * 8 + 1 {
                    return bad("METADATA_STRINGS invalide");
                }
                let lens = &blob[..off];
                let chars_part = &blob[off..];
                let mut rd = super::bitcode::VbrReader::new(lens);
                let mut pos = 0usize;
                for _ in 0..count {
                    let Some(l) = rd.vbr6() else {
                        return bad("longueurs METADATA_STRINGS tronquées");
                    };
                    let l = l as usize;
                    let Some(end) = pos.checked_add(l).filter(|&e| e <= chars_part.len()) else {
                        return bad("chaîne METADATA_STRINGS hors bornes");
                    };
                    m.mds.push(Md::Str(
                        String::from_utf8_lossy(&chars_part[pos..end]).into(),
                    ));
                    pos = end;
                }
            }
            6 | 36 | 38 | 39 | 11 => {}
            _ => {
                // Nœuds de debug-info et autres : réservent un identifiant.
                if matches!(r.code, 7 | 12..=34 | 37 | 40..=47) {
                    m.mds.push(Md::Other);
                }
            }
        }
        if m.mds.len() > MAX_VALUES {
            return bad("trop de métadonnées");
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------- fonctions

struct FnParser<'a> {
    m: &'a mut Module,
    values: Vec<Value>,
    blocks: Vec<BasicBlock>,
    cur_block: usize,
    relative: bool,
}

impl FnParser<'_> {
    fn next_id(&self) -> u32 {
        self.values.len() as u32
    }

    fn abs(&self, rel: u64) -> ValId {
        if self.relative {
            self.next_id().wrapping_sub(rel as u32)
        } else {
            rel as ValId
        }
    }

    fn vty(&self, v: ValId) -> Option<TyId> {
        self.values
            .get(v as usize)
            .filter(|x| !matches!(x.kind, ValueKind::Forward))
            .map(|x| x.ty)
    }

    /// `getValueTypePair` : valeur relative + type si référence avant.
    fn val_ty(&self, o: &[u64], i: &mut usize) -> Result<(ValId, TyId), DxilParseError> {
        let Some(&rel) = o.get(*i) else {
            return bad("opérande manquant");
        };
        *i += 1;
        let id = self.abs(rel);
        if let Some(t) = self.vty(id) {
            return Ok((id, t));
        }
        let Some(&t) = o.get(*i) else {
            return bad("type de référence avant manquant");
        };
        *i += 1;
        Ok((id, t as TyId))
    }

    fn val(&self, o: &[u64], i: &mut usize) -> Result<ValId, DxilParseError> {
        let Some(&rel) = o.get(*i) else {
            return bad("opérande manquant");
        };
        *i += 1;
        Ok(self.abs(rel))
    }

    fn push_inst(&mut self, result_ty: Option<TyId>, op: Op) -> Result<(), DxilParseError> {
        let result = match result_ty {
            Some(t) => {
                if self.values.len() >= MAX_VALUES {
                    return bad("trop de valeurs");
                }
                let id = self.next_id();
                self.values.push(Value {
                    ty: t,
                    kind: ValueKind::Inst,
                });
                Some(id)
            }
            None => None,
        };
        if let Some(b) = self.blocks.get_mut(self.cur_block) {
            b.insts.push(Inst { result, op });
        } else {
            return bad("instruction hors bloc");
        }
        Ok(())
    }

    fn end_block_if_term(&mut self, term: bool) {
        if term {
            self.cur_block += 1;
        }
    }

    fn elem_of(&self, t: TyId) -> TyId {
        match self.m.ty(t) {
            Ty::Ptr { elem, .. } => *elem,
            _ => t,
        }
    }

    /// Type après indexation par `idx` (GEP / extractvalue).
    fn walk_ty(&self, mut t: TyId, idx: &[Option<u64>]) -> Result<TyId, DxilParseError> {
        for i in idx {
            t = match self.m.ty(t) {
                Ty::Array(_, e) | Ty::Vector(_, e) => *e,
                Ty::Struct(f) => {
                    let Some(k) = i else {
                        return bad("indice de struct non constant");
                    };
                    match f.get(*k as usize) {
                        Some(&x) => x,
                        None => return bad("indice de struct hors bornes"),
                    }
                }
                _ => return bad("indexation d'un type scalaire"),
            };
        }
        Ok(t)
    }

    fn const_u64(&self, v: ValId) -> Option<u64> {
        match &self.values.get(v as usize)?.kind {
            ValueKind::Const(Const::Int(i)) => Some(*i),
            ValueKind::Const(Const::Null) => Some(0),
            _ => None,
        }
    }

    fn instruction(&mut self, r: &Record) -> Result<(), DxilParseError> {
        let o = &r.ops[..];
        let mut i = 0usize;
        match r.code {
            1 => {
                let n = o.first().copied().unwrap_or(0) as usize;
                if n > MAX_VALUES {
                    return bad("trop de blocs");
                }
                self.blocks = vec![BasicBlock::default(); n];
            }
            2 => {
                let (a, ta) = self.val_ty(o, &mut i)?;
                let b = self.val(o, &mut i)?;
                let op = o.get(i).copied().unwrap_or(0) as u32;
                self.push_inst(Some(ta), Op::Binop { op, a, b })?;
            }
            3 => {
                let (a, _) = self.val_ty(o, &mut i)?;
                let dest = o.get(i).copied().unwrap_or(0) as TyId;
                let op = o.get(i + 1).copied().unwrap_or(0) as u32;
                self.push_inst(Some(dest), Op::Cast { op, a })?;
            }
            43 => {
                // GEP : [inbounds, ty, (valtype)*]
                i = 2;
                let (base, bt) = self.val_ty(o, &mut i)?;
                let mut indices = Vec::new();
                while i < o.len() {
                    let (v, _) = self.val_ty(o, &mut i)?;
                    indices.push(v);
                }
                let src = o.get(1).copied().unwrap_or(0) as TyId;
                let addr = match self.m.ty(bt) {
                    Ty::Ptr { addr, .. } => *addr,
                    _ => 0,
                };
                let consts: Vec<Option<u64>> =
                    indices.iter().skip(1).map(|&v| self.const_u64(v)).collect();
                let elem = self.walk_ty(src, &consts)?;
                let pt = self.m.intern(Ty::Ptr { elem, addr });
                self.push_inst(Some(pt), Op::Gep { base, indices })?;
            }
            29 => {
                let (a, ta) = self.val_ty(o, &mut i)?;
                let b = self.val(o, &mut i)?;
                let (c, _) = self.val_ty(o, &mut i)?;
                self.push_inst(Some(ta), Op::Select { c, a, b })?;
            }
            28 | 9 => {
                let (a, ta) = self.val_ty(o, &mut i)?;
                let b = self.val(o, &mut i)?;
                let pred = o.get(i).copied().unwrap_or(0) as u32;
                let i1 = self.m.intern(Ty::Int(1));
                let rt = match self.m.ty(ta) {
                    Ty::Vector(n, _) => {
                        let n = *n;
                        self.m.intern(Ty::Vector(n, i1))
                    }
                    _ => i1,
                };
                self.push_inst(Some(rt), Op::Cmp { pred, a, b })?;
            }
            10 => {
                let v = if o.is_empty() {
                    None
                } else {
                    Some(self.val_ty(o, &mut i)?.0)
                };
                self.push_inst(None, Op::Ret(v))?;
                self.end_block_if_term(true);
            }
            11 => {
                let t = o.first().copied().unwrap_or(0) as u32;
                if o.len() >= 3 {
                    let f = o[1] as u32;
                    i = 2;
                    let cond = self.val(o, &mut i)?;
                    self.push_inst(None, Op::CondBr { cond, t, f })?;
                } else {
                    self.push_inst(None, Op::Br(t))?;
                }
                self.end_block_if_term(true);
            }
            12 => {
                let _ty = o.first().copied().unwrap_or(0);
                i = 1;
                let cond = self.val(o, &mut i)?;
                let default = o.get(i).copied().unwrap_or(0) as u32;
                i += 1;
                let mut cases = Vec::new();
                while i + 1 < o.len() {
                    let cv = o[i] as ValId;
                    let Some(c) = self.const_u64(cv) else {
                        return bad("case de switch non constant");
                    };
                    cases.push((c, o[i + 1] as u32));
                    i += 2;
                }
                self.push_inst(
                    None,
                    Op::Switch {
                        cond,
                        default,
                        cases,
                    },
                )?;
                self.end_block_if_term(true);
            }
            15 => {
                self.push_inst(None, Op::Unreachable)?;
                self.end_block_if_term(true);
            }
            16 => {
                let ty = o.first().copied().unwrap_or(0) as TyId;
                let mut inc = Vec::new();
                let mut k = 1;
                while k + 1 < o.len() {
                    let d = sign_decode(o[k]);
                    let id = i64::from(self.next_id()).wrapping_sub(d) as ValId;
                    inc.push((id, o[k + 1] as u32));
                    k += 2;
                }
                self.push_inst(Some(ty), Op::Phi(inc))?;
            }
            19 => {
                let ty = o.first().copied().unwrap_or(0) as TyId;
                let pt = self.m.intern(Ty::Ptr { elem: ty, addr: 0 });
                self.push_inst(Some(pt), Op::Alloca { ty })?;
            }
            20 => {
                let (ptr, _pt) = self.val_ty(o, &mut i)?;
                let ty = o.get(i).copied().unwrap_or(0) as TyId;
                self.push_inst(Some(ty), Op::Load { ptr })?;
            }
            44 => {
                let (ptr, _) = self.val_ty(o, &mut i)?;
                let (val, _) = self.val_ty(o, &mut i)?;
                self.push_inst(None, Op::Store { ptr, val })?;
            }
            26 => {
                let (agg, at) = self.val_ty(o, &mut i)?;
                let idx: Vec<u32> = o[i..].iter().map(|&v| v as u32).collect();
                let path: Vec<Option<u64>> = idx.iter().map(|&v| Some(u64::from(v))).collect();
                let t = self.walk_ty(at, &path)?;
                self.push_inst(Some(t), Op::ExtractVal { agg, idx })?;
            }
            27 => {
                let (agg, at) = self.val_ty(o, &mut i)?;
                let (val, _) = self.val_ty(o, &mut i)?;
                let idx: Vec<u32> = o[i..].iter().map(|&v| v as u32).collect();
                self.push_inst(Some(at), Op::InsertVal { agg, val, idx })?;
            }
            34 => {
                let cc = o.get(1).copied().unwrap_or(0);
                i = 2;
                if cc & (1 << 17) != 0 {
                    i += 1;
                }
                let mut fty = None;
                if cc & (1 << 15) != 0 {
                    fty = Some(o.get(i).copied().unwrap_or(0) as TyId);
                    i += 1;
                }
                let (callee, ct) = self.val_ty(o, &mut i)?;
                let fty = fty.unwrap_or_else(|| self.elem_of(ct));
                let Ty::Func { ret, params } = self.m.ty(fty).clone() else {
                    return bad("appel avec un type non fonction");
                };
                let mut args = Vec::new();
                for &p in &params {
                    if matches!(self.m.ty(p), Ty::Label) {
                        args.push(o.get(i).copied().unwrap_or(0) as ValId);
                        i += 1;
                    } else {
                        args.push(self.val(o, &mut i)?);
                    }
                }
                let rt = if matches!(self.m.ty(ret), Ty::Void) {
                    None
                } else {
                    Some(ret)
                };
                self.push_inst(rt, Op::Call { callee, args })?;
            }
            38 => {
                let (ptr, pt) = self.val_ty(o, &mut i)?;
                let val = self.val(o, &mut i)?;
                let op = o.get(i).copied().unwrap_or(0) as u32;
                let t = self.elem_of(pt);
                self.push_inst(Some(t), Op::AtomicRmw { op, ptr, val })?;
            }
            46 => {
                let (ptr, pt) = self.val_ty(o, &mut i)?;
                let (cmp, _) = self.val_ty(o, &mut i)?;
                let new = self.val(o, &mut i)?;
                let t = self.elem_of(pt);
                let i1 = self.m.intern(Ty::Int(1));
                let st = self.m.intern(Ty::Struct(vec![t, i1]));
                self.push_inst(Some(st), Op::CmpXchg { ptr, cmp, new })?;
            }
            36 => self.push_inst(None, Op::Fence)?,
            33 | 35 => {}
            other => {
                // Instruction non gérée : on réserve un résultat si le décodage est ambigu.
                self.push_inst(None, Op::Unsupported(other))?;
            }
        }
        Ok(())
    }
}

fn parse_function_block(
    m: &mut Module,
    blk: &Block,
    fi: usize,
    relative: bool,
) -> Result<(), DxilParseError> {
    let mut values = m.values.clone();
    let fty = m.functions[fi].ty;
    let Ty::Func { params, .. } = m.ty(fty).clone() else {
        return bad("fonction sans type fonction");
    };
    let mut args = Vec::new();
    for (k, &p) in params.iter().enumerate() {
        args.push(values.len() as ValId);
        values.push(Value {
            ty: p,
            kind: ValueKind::Arg(k),
        });
    }
    let base = values.len();
    for it in &blk.items {
        if let Item::Block(b) = it
            && b.id == 11
        {
            parse_constants(m, b, &mut values, base)?;
        }
    }
    let mut p = FnParser {
        m,
        values,
        blocks: Vec::new(),
        cur_block: 0,
        relative,
    };
    for it in &blk.items {
        if let Item::Record(r) = it {
            p.instruction(r)?;
        }
    }
    let FnParser { values, blocks, .. } = p;
    let f = &mut m.functions[fi];
    f.values = values;
    f.blocks = blocks;
    f.args = args;
    Ok(())
}

// ------------------------------------------------------------------------- module

pub fn parse_module(top: &[Block]) -> Result<Module, DxilParseError> {
    let Some(mb) = top.iter().find(|b| b.id == 8) else {
        return bad("bloc MODULE absent");
    };
    let mut m = Module::default();
    let mut relative = false;
    let mut fn_blocks: Vec<&Block> = Vec::new();
    let mut symtab: Vec<(ValId, String)> = Vec::new();
    for it in &mb.items {
        match it {
            Item::Record(r) => match r.code {
                1 => relative = r.ops.first().copied().unwrap_or(0) >= 1,
                7 => {
                    let o = &r.ops;
                    let flags = o.get(1).copied().unwrap_or(0);
                    let explicit = flags & 2 != 0;
                    let addr = (flags >> 2) as u32;
                    let (elem, addr) = if explicit {
                        (o.first().copied().unwrap_or(0) as TyId, addr)
                    } else {
                        match m.ty(o.first().copied().unwrap_or(0) as TyId) {
                            Ty::Ptr { elem, addr } => (*elem, *addr),
                            _ => (0, 0),
                        }
                    };
                    let initid = o.get(2).copied().unwrap_or(0);
                    let pt = m.intern(Ty::Ptr { elem, addr });
                    let gi = m.globals.len();
                    m.globals.push(GlobalVar {
                        name: String::new(),
                        elem,
                        addr,
                        is_const: flags & 1 != 0,
                        init: if initid == 0 {
                            None
                        } else {
                            Some((initid - 1) as ValId)
                        },
                    });
                    m.values.push(Value {
                        ty: pt,
                        kind: ValueKind::GlobalVar(gi),
                    });
                }
                8 => {
                    let o = &r.ops;
                    let ty = o.first().copied().unwrap_or(0) as TyId;
                    let ty = match m.ty(ty) {
                        Ty::Ptr { elem, .. } => *elem,
                        _ => ty,
                    };
                    let fi = m.functions.len();
                    let pt = m.intern(Ty::Ptr { elem: ty, addr: 0 });
                    m.functions.push(Function {
                        name: String::new(),
                        ty,
                        is_proto: o.get(2).copied().unwrap_or(1) != 0,
                        blocks: Vec::new(),
                        values: Vec::new(),
                        args: Vec::new(),
                    });
                    m.values.push(Value {
                        ty: pt,
                        kind: ValueKind::Function(fi),
                    });
                }
                _ => {}
            },
            Item::Block(b) => match b.id {
                17 => parse_types(&mut m, b)?,
                11 => {
                    let mut vals = std::mem::take(&mut m.values);
                    parse_constants(&mut m, b, &mut vals, 0)?;
                    m.values = vals;
                }
                15 => parse_metadata(&mut m, b)?,
                12 => fn_blocks.push(b),
                14 => {
                    for x in &b.items {
                        if let Item::Record(r) = x {
                            match r.code {
                                1 if !r.ops.is_empty() => {
                                    symtab.push((r.ops[0] as ValId, chars(&r.ops[1..])))
                                }
                                3 if r.ops.len() >= 2 => {
                                    symtab.push((r.ops[0] as ValId, chars(&r.ops[2..])))
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            },
        }
    }
    for (id, name) in symtab {
        match m.values.get(id as usize).map(|v| v.kind.clone()) {
            Some(ValueKind::GlobalVar(g)) => m.globals[g].name = name,
            Some(ValueKind::Function(f)) => m.functions[f].name = name,
            _ => {}
        }
    }
    let defined: Vec<usize> = (0..m.functions.len())
        .filter(|&i| !m.functions[i].is_proto)
        .collect();
    if defined.len() != fn_blocks.len() {
        return bad("nombre de corps de fonction incohérent");
    }
    for (fi, blk) in defined.into_iter().zip(fn_blocks) {
        parse_function_block(&mut m, blk, fi, relative)?;
    }
    Ok(m)
}
