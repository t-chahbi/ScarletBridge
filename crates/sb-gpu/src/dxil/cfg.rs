//! Structuriseur de flux de contrôle : CFG LLVM (réductible, non structuré) -> arbre structuré
//! (`Ast`) lowerable en SPIR-V valide (`OpSelectionMerge` / `OpLoopMerge`).
//!
//! Algorithme (dérivé de « Beyond Relooper », N. Ramsey) : arbre de dominance, arêtes de retour,
//! noeuds de fusion (>= 2 arêtes avant) emballés dans des `Block` à sortie par `Br`, boucles
//! naturelles avec sorties placées après la boucle, bloc de continuation unique, retours triviaux
//! dupliqués. Si l'arbre obtenu n'est pas directement exprimable (sortie multi-niveaux, CFG
//! irréductible), `dispatcher` produit une machine à états (boucle + `switch`) toujours valide.

use super::module::ValId;

#[derive(Debug, Clone)]
pub enum Term {
    Br(usize),
    Cond {
        cond: ValId,
        t: usize,
        f: usize,
    },
    Switch {
        sel: ValId,
        default: usize,
        cases: Vec<(u64, usize)>,
    },
    Ret,
    Unreachable,
}

#[derive(Debug, Clone)]
pub enum Ast {
    Code(usize),
    Seq(Vec<Ast>),
    If {
        cond: ValId,
        then: Box<Ast>,
        els: Box<Ast>,
    },
    Switch {
        sel: ValId,
        arms: Vec<(Vec<u64>, Ast)>,
        default: Box<Ast>,
    },
    Block {
        label: usize,
        body: Box<Ast>,
    },
    Loop {
        labels: Vec<usize>,
        body: Box<Ast>,
        cont: Option<Box<Ast>>,
    },
    Br(usize),
    Cont(usize),
    Edge(usize, usize),
    Ret,
    Unreachable,
    Nop,
    SetState(usize),
    SwitchState(Vec<(usize, Ast)>),
}

const NONE: usize = usize::MAX;

#[allow(dead_code)]
struct Cfg<'a> {
    n: usize,
    terms: &'a [Term],
    succ: Vec<Vec<usize>>,
    trivial: Vec<bool>,
    reach: Vec<bool>,
    rpo: Vec<usize>, // numéro RPO par noeud (NONE si inaccessible)
    order: Vec<usize>,
    idom: Vec<usize>,
    back: Vec<Vec<usize>>, // pour chaque en-tête : sources d'arêtes de retour
    is_back: Vec<(usize, usize)>,
    fwd_in: Vec<usize>,
    body: Vec<Vec<bool>>, // par en-tête : appartenance à la boucle (vec vide sinon)
    body_size: Vec<usize>,
    cont_block: Vec<usize>, // par en-tête
    is_cont: Vec<usize>,    // par noeud : en-tête dont il est le bloc de continuation
    mparent: Vec<usize>,
    post: Vec<bool>,
    children: Vec<Vec<usize>>,
}

fn distinct_succ(t: &Term) -> Vec<usize> {
    let mut v: Vec<usize> = match t {
        Term::Br(a) => vec![*a],
        Term::Cond { t, f, .. } => vec![*t, *f],
        Term::Switch { default, cases, .. } => {
            let mut v: Vec<usize> = cases.iter().map(|c| c.1).collect();
            v.push(*default);
            v
        }
        _ => vec![],
    };
    let mut seen = Vec::new();
    v.retain(|x| {
        if seen.contains(x) {
            false
        } else {
            seen.push(*x);
            true
        }
    });
    v
}

impl<'a> Cfg<'a> {
    fn analyse(terms: &'a [Term], trivial: Vec<bool>) -> Option<Self> {
        let n = terms.len();
        if n == 0 || n > 4000 {
            return None;
        }
        let mut succ: Vec<Vec<usize>> = Vec::with_capacity(n);
        for t in terms {
            let s = distinct_succ(t);
            if s.iter().any(|&x| x >= n) {
                return None;
            }
            succ.push(s);
        }
        // DFS itératif : ordre post, arêtes de retour.
        let mut state = vec![0u8; n]; // 0 = non vu, 1 = sur la pile, 2 = terminé
        let mut post_order = Vec::new();
        let mut is_back = Vec::new();
        let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
        state[0] = 1;
        while let Some(&mut (u, ref mut i)) = stack.last_mut() {
            if *i < succ[u].len() {
                let v = succ[u][*i];
                *i += 1;
                if trivial[v] {
                    continue;
                }
                match state[v] {
                    0 => {
                        state[v] = 1;
                        stack.push((v, 0));
                    }
                    1 => is_back.push((u, v)),
                    _ => {}
                }
            } else {
                state[u] = 2;
                post_order.push(u);
                stack.pop();
            }
        }
        let order: Vec<usize> = post_order.iter().rev().copied().collect();
        let mut rpo = vec![NONE; n];
        for (i, &b) in order.iter().enumerate() {
            rpo[b] = i;
        }
        let reach: Vec<bool> = rpo.iter().map(|&r| r != NONE).collect();
        // Prédécesseurs.
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
        for &u in &order {
            for &v in &succ[u] {
                if !trivial[v] {
                    preds[v].push(u);
                }
            }
        }
        // Dominateurs (Cooper-Harvey-Kennedy).
        let mut idom = vec![NONE; n];
        idom[0] = 0;
        let mut changed = true;
        while changed {
            changed = false;
            for &b in order.iter().skip(1) {
                let mut new = NONE;
                for &p in &preds[b] {
                    if idom[p] == NONE {
                        continue;
                    }
                    new = if new == NONE {
                        p
                    } else {
                        let (mut a, mut c) = (p, new);
                        while a != c {
                            while rpo[a] > rpo[c] {
                                a = idom[a];
                            }
                            while rpo[c] > rpo[a] {
                                c = idom[c];
                            }
                        }
                        a
                    };
                }
                if new != idom[b] {
                    idom[b] = new;
                    changed = true;
                }
            }
        }
        let dominates = |a: usize, mut b: usize| -> bool {
            loop {
                if a == b {
                    return true;
                }
                if b == 0 || idom[b] == NONE || idom[b] == b {
                    return false;
                }
                b = idom[b];
            }
        };
        // Réductibilité.
        for &(u, v) in &is_back {
            if !dominates(v, u) {
                return None;
            }
        }
        let mut back = vec![Vec::new(); n];
        for &(u, v) in &is_back {
            back[v].push(u);
        }
        let mut fwd_in = vec![0usize; n];
        for &u in &order {
            for &v in &succ[u] {
                if !trivial[v] && !is_back.contains(&(u, v)) {
                    fwd_in[v] += 1;
                }
            }
        }
        // Boucles naturelles.
        let mut body: Vec<Vec<bool>> = vec![Vec::new(); n];
        let mut body_size = vec![0usize; n];
        for h in 0..n {
            if back[h].is_empty() {
                continue;
            }
            let mut inb = vec![false; n];
            inb[h] = true;
            let mut work: Vec<usize> = Vec::new();
            for &s in &back[h] {
                if !inb[s] {
                    inb[s] = true;
                    work.push(s);
                }
            }
            while let Some(x) = work.pop() {
                for &p in &preds[x] {
                    if !inb[p] {
                        inb[p] = true;
                        work.push(p);
                    }
                }
            }
            body_size[h] = inb.iter().filter(|&&b| b).count();
            body[h] = inb;
        }
        // Bloc de continuation.
        let mut cont_block = vec![NONE; n];
        let mut is_cont = vec![NONE; n];
        for h in 0..n {
            if back[h].len() == 1 {
                let b = back[h][0];
                if b != h && succ[b].len() == 1 && succ[b][0] == h && fwd_in[b] >= 2 {
                    cont_block[h] = b;
                    is_cont[b] = h;
                }
            }
        }
        let headers: Vec<usize> = (0..n).filter(|&l| !body[l].is_empty()).collect();
        // Parent modifié (sorties de boucle replacées après la boucle).
        let mut mparent = vec![NONE; n];
        let mut post = vec![false; n];
        for &y in order.iter().skip(1) {
            let mut p = idom[y];
            let mut loops: Vec<usize> = headers.iter().copied().filter(|&l| !body[l][y]).collect();
            // boucle la plus externe contenant p mais pas y : p devient son en-tête
            let cand = loops
                .iter()
                .copied()
                .filter(|&l| body[l][p])
                .max_by_key(|&l| body_size[l]);
            if let Some(l) = cand {
                p = l;
                post[y] = true;
                loops.retain(|&x| x != l);
            }
            mparent[y] = p;
        }
        let mut children = vec![Vec::new(); n];
        for &y in order.iter().skip(1) {
            if mparent[y] != NONE {
                children[mparent[y]].push(y);
            }
        }
        for c in &mut children {
            c.sort_by_key(|&y| rpo[y]);
        }
        Some(Self {
            n,
            terms,
            succ,
            trivial,
            reach,
            rpo,
            order,
            idom,
            back,
            is_back,
            fwd_in,
            body,
            body_size,
            cont_block,
            is_cont,
            mparent,
            post,
            children,
        })
    }

    fn is_wrap(&self, y: usize) -> bool {
        self.is_cont[y] == NONE && (self.post[y] || self.fwd_in[y] >= 2)
    }

    fn do_branch(&self, u: usize, t: usize, depth: usize) -> Option<Ast> {
        if self.trivial[t] {
            return Some(match &self.terms[t] {
                Term::Unreachable => Ast::Unreachable,
                _ => Ast::Ret,
            });
        }
        if self.is_back.contains(&(u, t)) {
            return Some(Ast::Seq(vec![Ast::Edge(u, t), Ast::Cont(t)]));
        }
        // Branche vers le bloc de continuation de la boucle englobante.
        if self.is_cont[t] != NONE {
            return Some(Ast::Seq(vec![Ast::Edge(u, t), Ast::Cont(self.is_cont[t])]));
        }
        if self.is_wrap(t) {
            return Some(Ast::Seq(vec![Ast::Edge(u, t), Ast::Br(t)]));
        }
        let rest = self.do_tree(t, depth + 1)?;
        Some(Ast::Seq(vec![Ast::Edge(u, t), rest]))
    }

    fn terminator(&self, x: usize, depth: usize) -> Option<Ast> {
        let mut seq = vec![Ast::Code(x)];
        match &self.terms[x] {
            Term::Ret => seq.push(Ast::Ret),
            Term::Unreachable => seq.push(Ast::Unreachable),
            Term::Br(t) => seq.push(self.do_branch(x, *t, depth)?),
            Term::Cond { cond, t, f } => {
                if t == f {
                    seq.push(self.do_branch(x, *t, depth)?);
                } else {
                    seq.push(Ast::If {
                        cond: *cond,
                        then: Box::new(self.do_branch(x, *t, depth)?),
                        els: Box::new(self.do_branch(x, *f, depth)?),
                    });
                }
            }
            Term::Switch {
                sel,
                default,
                cases,
            } => {
                let mut arms: Vec<(Vec<u64>, usize)> = Vec::new();
                for &(v, t) in cases {
                    if t == *default {
                        continue;
                    }
                    if let Some(a) = arms.iter_mut().find(|a| a.1 == t) {
                        a.0.push(v);
                    } else {
                        arms.push((vec![v], t));
                    }
                }
                let mut out = Vec::new();
                for (vals, t) in arms {
                    out.push((vals, self.do_branch(x, t, depth)?));
                }
                seq.push(Ast::Switch {
                    sel: *sel,
                    arms: out,
                    default: Box::new(self.do_branch(x, *default, depth)?),
                });
            }
        }
        Some(Ast::Seq(seq))
    }

    fn node_within(&self, x: usize, ws: &[usize], depth: usize) -> Option<Ast> {
        if depth > 500 {
            return None;
        }
        match ws.split_last() {
            None => self.terminator(x, depth),
            Some((&y, rest)) => {
                let inner = self.node_within(x, rest, depth + 1)?;
                let after = self.do_tree(y, depth + 1)?;
                Some(Ast::Seq(vec![
                    Ast::Block {
                        label: y,
                        body: Box::new(inner),
                    },
                    after,
                ]))
            }
        }
    }

    fn do_tree(&self, x: usize, depth: usize) -> Option<Ast> {
        if depth > 500 {
            return None;
        }
        let ws: Vec<usize> = self.children[x]
            .iter()
            .copied()
            .filter(|&y| self.is_wrap(y))
            .collect();
        if self.back[x].is_empty() {
            return self.node_within(x, &ws, depth);
        }
        // En-tête de boucle.
        let (inside, outside): (Vec<usize>, Vec<usize>) =
            ws.into_iter().partition(|&y| self.body[x][y]);
        // Les noeuds replacés après la boucle non-fusion (single-pred) sont aussi des « post ».
        let post_nodes: Vec<usize> = self.children[x]
            .iter()
            .copied()
            .filter(|&y| !self.body[x][y] && self.post[y])
            .collect();
        let mut outs = outside;
        for p in post_nodes {
            if !outs.contains(&p) && self.is_cont[p] == NONE {
                outs.push(p);
            }
        }
        outs.sort_by_key(|&y| self.rpo[y]);
        let cont = if self.cont_block[x] != NONE {
            let b = self.cont_block[x];
            Some(Box::new(Ast::Seq(vec![Ast::Code(b), Ast::Edge(b, x)])))
        } else {
            None
        };
        let body = self.node_within(x, &inside, depth + 1)?;
        let mut acc = Ast::Loop {
            labels: vec![x],
            body: Box::new(body),
            cont,
        };
        // Les sorties sont emballées de la plus basse (interne) à la plus haute (externe).
        let mut posts: Vec<Ast> = Vec::new();
        for &z in &outs {
            acc = Ast::Block {
                label: z,
                body: Box::new(acc),
            };
            posts.push(self.do_tree(z, depth + 1)?);
            // Le code de z suit immédiatement le bloc z ; les suivants emballent l'ensemble.
            acc = Ast::Seq(vec![acc, posts.pop()?]);
        }
        Some(acc)
    }
}

// ------------------------------------------------------------------ simplification / validité

fn flatten(a: Ast) -> Ast {
    match a {
        Ast::Seq(v) => {
            let mut out = Vec::new();
            for x in v {
                match flatten(x) {
                    Ast::Seq(inner) => out.extend(inner),
                    Ast::Nop => {}
                    o => out.push(o),
                }
            }
            if out.len() == 1 {
                out.remove(0)
            } else if out.is_empty() {
                Ast::Nop
            } else {
                Ast::Seq(out)
            }
        }
        Ast::If { cond, then, els } => Ast::If {
            cond,
            then: Box::new(flatten(*then)),
            els: Box::new(flatten(*els)),
        },
        Ast::Switch { sel, arms, default } => Ast::Switch {
            sel,
            arms: arms.into_iter().map(|(v, a)| (v, flatten(a))).collect(),
            default: Box::new(flatten(*default)),
        },
        Ast::Block { label, body } => Ast::Block {
            label,
            body: Box::new(flatten(*body)),
        },
        Ast::Loop { labels, body, cont } => Ast::Loop {
            labels,
            body: Box::new(flatten(*body)),
            cont: cont.map(|c| Box::new(flatten(*c))),
        },
        o => o,
    }
}

/// Retire les `Br(l)` en position de queue ; retourne `true` s'il reste un usage hors queue.
fn strip(a: Ast, l: usize, tail: bool, nontail: &mut bool) -> Ast {
    match a {
        Ast::Br(x) if x == l => {
            if tail {
                Ast::Nop
            } else {
                *nontail = true;
                Ast::Br(x)
            }
        }
        Ast::Seq(v) => {
            let n = v.len();
            Ast::Seq(
                v.into_iter()
                    .enumerate()
                    .map(|(i, x)| strip(x, l, tail && i + 1 == n, nontail))
                    .collect(),
            )
        }
        Ast::If { cond, then, els } => Ast::If {
            cond,
            then: Box::new(strip(*then, l, tail, nontail)),
            els: Box::new(strip(*els, l, tail, nontail)),
        },
        Ast::Switch { sel, arms, default } => Ast::Switch {
            sel,
            arms: arms
                .into_iter()
                .map(|(v, x)| (v, strip(x, l, tail, nontail)))
                .collect(),
            default: Box::new(strip(*default, l, tail, nontail)),
        },
        Ast::Block { label, body } => Ast::Block {
            label,
            body: Box::new(strip(*body, l, false, nontail)),
        },
        Ast::Loop { labels, body, cont } => Ast::Loop {
            labels,
            body: Box::new(strip(*body, l, false, nontail)),
            cont,
        },
        o => o,
    }
}

fn simplify(a: Ast) -> Ast {
    match a {
        Ast::Seq(v) => Ast::Seq(v.into_iter().map(simplify).collect()),
        Ast::If { cond, then, els } => Ast::If {
            cond,
            then: Box::new(simplify(*then)),
            els: Box::new(simplify(*els)),
        },
        Ast::Switch { sel, arms, default } => Ast::Switch {
            sel,
            arms: arms.into_iter().map(|(v, x)| (v, simplify(x))).collect(),
            default: Box::new(simplify(*default)),
        },
        Ast::Loop { labels, body, cont } => Ast::Loop {
            labels,
            body: Box::new(simplify(*body)),
            cont,
        },
        Ast::Block { label, body } => {
            let b = flatten(simplify(*body));
            if let Ast::Loop {
                mut labels,
                body,
                cont,
            } = b
            {
                labels.push(label);
                return Ast::Loop { labels, body, cont };
            }
            let mut nontail = false;
            let b = flatten(strip(b, label, true, &mut nontail));
            if nontail {
                Ast::Block {
                    label,
                    body: Box::new(b),
                }
            } else {
                b
            }
        }
        o => o,
    }
}

fn valid(a: &Ast, st: &mut Vec<(Vec<usize>, bool, bool)>) -> bool {
    // st : (étiquettes, réelle, est_boucle) ; les sélections sont (vec![], false, false)
    match a {
        Ast::Seq(v) => v.iter().all(|x| valid(x, st)),
        Ast::If { then, els, .. } => {
            st.push((vec![], false, false));
            let ok = valid(then, st) && valid(els, st);
            st.pop();
            ok
        }
        Ast::Switch { arms, default, .. } => {
            st.push((vec![], false, false));
            let ok = arms.iter().all(|(_, x)| valid(x, st)) && valid(default, st);
            st.pop();
            ok
        }
        Ast::Block { label, body } => {
            st.push((vec![*label], false, true));
            let ok = valid(body, st);
            st.pop();
            ok
        }
        Ast::Loop { labels, body, .. } => {
            st.push((labels.clone(), true, true));
            let ok = valid(body, st);
            st.pop();
            ok
        }
        Ast::Br(l) => st.iter().any(|e| e.2 && e.0.contains(l)),
        Ast::Cont(l) => st.iter().any(|e| e.2 && e.1 && e.0.first() == Some(l)),
        Ast::SwitchState(arms) => arms.iter().all(|(_, x)| valid(x, st)),
        _ => true,
    }
}

/// Machine à états : `loop { switch (state) { case b: code(b); state = next; continue } }`.
fn dispatcher(terms: &[Term], trivial: &[bool]) -> Ast {
    let n = terms.len();
    let mut arms = Vec::new();
    let go = |u: usize, t: usize| -> Ast {
        if trivial[t] {
            return match &terms[t] {
                Term::Unreachable => Ast::Unreachable,
                _ => Ast::Ret,
            };
        }
        Ast::Seq(vec![Ast::Edge(u, t), Ast::SetState(t), Ast::Cont(0)])
    };
    // Ordre des bras : RPO (les définitions précèdent les usages).
    let mut seen = vec![false; n];
    let mut post = Vec::new();
    let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
    seen[0] = true;
    while let Some(&mut (u, ref mut i)) = stack.last_mut() {
        let succ = distinct_succ(&terms[u]);
        if *i < succ.len() {
            let v = succ[*i];
            *i += 1;
            if v < n && !trivial[v] && !seen[v] {
                seen[v] = true;
                stack.push((v, 0));
            }
        } else {
            post.push(u);
            stack.pop();
        }
    }
    for &b in post.iter().rev() {
        if trivial[b] {
            continue;
        }
        let mut seq = vec![Ast::Code(b)];
        match &terms[b] {
            Term::Ret => seq.push(Ast::Ret),
            Term::Unreachable => seq.push(Ast::Unreachable),
            Term::Br(t) => seq.push(go(b, *t)),
            Term::Cond { cond, t, f } => seq.push(Ast::If {
                cond: *cond,
                then: Box::new(go(b, *t)),
                els: Box::new(go(b, *f)),
            }),
            Term::Switch {
                sel,
                default,
                cases,
            } => {
                let mut arms2: Vec<(Vec<u64>, usize)> = Vec::new();
                for &(v, t) in cases {
                    if t == *default {
                        continue;
                    }
                    if let Some(a) = arms2.iter_mut().find(|a| a.1 == t) {
                        a.0.push(v);
                    } else {
                        arms2.push((vec![v], t));
                    }
                }
                seq.push(Ast::Switch {
                    sel: *sel,
                    arms: arms2.into_iter().map(|(v, t)| (v, go(b, t))).collect(),
                    default: Box::new(go(b, *default)),
                });
            }
        }
        arms.push((b, Ast::Seq(seq)));
    }
    Ast::Seq(vec![
        Ast::SetState(0),
        Ast::Loop {
            labels: vec![0],
            body: Box::new(Ast::SwitchState(arms)),
            cont: None,
        },
    ])
}

pub struct Structured {
    pub ast: Ast,
    pub dispatcher: bool,
}

pub fn structurize(terms: &[Term], trivial: Vec<bool>) -> Structured {
    let try_structured = || -> Option<Ast> {
        let cfg = Cfg::analyse(terms, trivial.clone());
        if cfg.is_none() && std::env::var_os("DXIL_DEBUG_AST").is_some() {
            eprintln!("analyse CFG impossible (irréductible ?)");
        }
        let cfg = cfg?;
        let _ = (
            &cfg.reach,
            &cfg.order,
            &cfg.idom,
            &cfg.succ,
            cfg.n,
            &cfg.mparent,
        );
        let root = cfg.do_tree(0, 0)?;
        let ast = flatten(simplify(flatten(root)));
        let mut st = Vec::new();
        if valid(&ast, &mut st) {
            Some(ast)
        } else {
            if std::env::var_os("DXIL_DEBUG_AST").is_some() {
                eprintln!("structurisation invalide : {ast:#?}");
            }
            None
        }
    };
    match try_structured() {
        Some(ast) => Structured {
            ast,
            dispatcher: false,
        },
        None => Structured {
            ast: dispatcher(terms, &trivial),
            dispatcher: true,
        },
    }
}
