//! Graphe de flot de contrôle scalaire d'un programme RDNA 2.
//!
//! Les branches RDNA (`s_branch`, `s_cbranch_*`) sont *uniformes sur la vague* : toute la vague
//! suit le même chemin. La divergence entre lanes passe uniquement par le masque `EXEC`, modélisé
//! explicitement par le traducteur (voir `translate`). Le CFG ne contient donc que du flot
//! scalaire : blocs de base, branche inconditionnelle, branche conditionnelle sur SCC/VCC/EXEC.

use super::decode::Decoded;
use super::inst::{Encoding, Instruction, Src};

/// Condition d'un `s_cbranch_*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchCond {
    Scc0,
    Scc1,
    Vccz,
    Vccnz,
    Execz,
    Execnz,
}

/// Fin de bloc de base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terminator {
    /// Continue dans le bloc suivant (sans branche explicite).
    Fallthrough(usize),
    /// `s_branch`.
    Branch(usize),
    /// `s_cbranch_*` : saute à `taken` si la condition est vraie, sinon `fall`.
    Cond {
        cond: BranchCond,
        taken: usize,
        fall: usize,
    },
    /// `s_endpgm`.
    End,
}

/// Bloc de base : plage `[start, end)` dans `Cfg::insts` (le terminateur est inclus s'il existe).
#[derive(Clone, Debug)]
pub struct Block {
    pub start: usize,
    pub end: usize,
    pub term: Terminator,
}

/// Programme découpé en blocs de base.
#[derive(Clone, Debug)]
pub struct Cfg {
    pub insts: Vec<Decoded>,
    pub blocks: Vec<Block>,
}

/// Erreurs de construction du CFG.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CfgError {
    #[error("programme vide")]
    Empty,
    #[error("cible de branchement {target} (dword) hors programme ou au milieu d'une instruction")]
    BadTarget { target: i64 },
    #[error("branchement indirect/appel non supporté : {0}")]
    Indirect(&'static str),
    #[error("le programme se termine sans s_endpgm")]
    NoEnd,
}

fn cond_of(name: &str) -> Option<BranchCond> {
    Some(match name {
        "s_cbranch_scc0" => BranchCond::Scc0,
        "s_cbranch_scc1" => BranchCond::Scc1,
        "s_cbranch_vccz" => BranchCond::Vccz,
        "s_cbranch_vccnz" => BranchCond::Vccnz,
        "s_cbranch_execz" => BranchCond::Execz,
        "s_cbranch_execnz" => BranchCond::Execnz,
        _ => return None,
    })
}

/// Cible (en dwords) d'un branchement : `pc_suivant + simm16`.
fn target_of(d: &Decoded) -> i64 {
    d.offset as i64 + i64::from(d.inst.words) + i64::from(d.inst.mods.imm)
}

/// Construit le CFG. Les blocs sont ordonnés comme les instructions ; le bloc 0 est l'entrée.
pub fn build_cfg(insts: Vec<Decoded>) -> Result<Cfg, CfgError> {
    if insts.is_empty() {
        return Err(CfgError::Empty);
    }
    let index_of = |off: i64| -> Result<usize, CfgError> {
        insts
            .iter()
            .position(|d| d.offset as i64 == off)
            .ok_or(CfgError::BadTarget { target: off })
    };
    // 1. Chefs de blocs.
    let mut leader = vec![false; insts.len()];
    leader[0] = true;
    for (i, d) in insts.iter().enumerate() {
        if d.inst.encoding != Encoding::Sopp {
            if matches!(d.inst.name, "s_setpc_b64" | "s_swappc_b64" | "s_getpc_b64") {
                return Err(CfgError::Indirect("s_setpc/s_swappc"));
            }
            continue;
        }
        let name = d.inst.name;
        if name == "s_branch" || cond_of(name).is_some() {
            leader[index_of(target_of(d))?] = true;
            if i + 1 < insts.len() {
                leader[i + 1] = true;
            }
        } else if name == "s_endpgm" && i + 1 < insts.len() {
            leader[i + 1] = true;
        } else if matches!(
            name,
            "s_cbranch_cdbgsys"
                | "s_cbranch_cdbguser"
                | "s_cbranch_cdbgsys_or_user"
                | "s_cbranch_cdbgsys_and_user"
        ) {
            return Err(CfgError::Indirect("s_cbranch_cdbg*"));
        }
    }
    // 2. Blocs.
    let starts: Vec<usize> = (0..insts.len()).filter(|&i| leader[i]).collect();
    let block_of_inst = |idx: usize| starts.binary_search(&idx).expect("chef de bloc");
    let mut blocks = Vec::new();
    for (b, &s) in starts.iter().enumerate() {
        let e = starts.get(b + 1).copied().unwrap_or(insts.len());
        let last = &insts[e - 1];
        let term = if last.inst.encoding == Encoding::Sopp {
            let name = last.inst.name;
            if name == "s_endpgm" {
                Terminator::End
            } else if name == "s_branch" {
                Terminator::Branch(block_of_inst(index_of(target_of(last))?))
            } else if let Some(cond) = cond_of(name) {
                if e >= insts.len() {
                    return Err(CfgError::NoEnd);
                }
                Terminator::Cond {
                    cond,
                    taken: block_of_inst(index_of(target_of(last))?),
                    fall: b + 1,
                }
            } else if e < insts.len() {
                Terminator::Fallthrough(b + 1)
            } else {
                return Err(CfgError::NoEnd);
            }
        } else if e < insts.len() {
            Terminator::Fallthrough(b + 1)
        } else {
            return Err(CfgError::NoEnd);
        };
        blocks.push(Block {
            start: s,
            end: e,
            term,
        });
    }
    Ok(Cfg { insts, blocks })
}

impl Cfg {
    /// Successeurs d'un bloc.
    pub fn successors(&self, b: usize) -> Vec<usize> {
        match self.blocks[b].term {
            Terminator::Fallthrough(t) | Terminator::Branch(t) => vec![t],
            Terminator::Cond { taken, fall, .. } => vec![taken, fall],
            Terminator::End => vec![],
        }
    }

    /// Vrai s'il existe une arête arrière (boucle) : cible d'indice ≤ source.
    pub fn has_loop(&self) -> bool {
        (0..self.blocks.len()).any(|b| self.successors(b).iter().any(|&s| s <= b))
    }

    /// Vrai si le programme est un unique bloc terminé par `s_endpgm` (code linéaire).
    pub fn is_straight_line(&self) -> bool {
        self.blocks.len() == 1 && self.blocks[0].term == Terminator::End
    }

    /// Instructions d'un bloc, terminateur éventuel exclu.
    pub fn body(&self, b: usize) -> &[Decoded] {
        let blk = &self.blocks[b];
        let last = &self.insts[blk.end - 1].inst;
        let has_term = last.encoding == Encoding::Sopp
            && (last.name == "s_endpgm" || last.name == "s_branch" || cond_of(last.name).is_some());
        let end = if has_term { blk.end - 1 } else { blk.end };
        &self.insts[blk.start..end]
    }
}

/// Une instruction écrit-elle EXEC (directement, via saveexec ou `v_cmpx`) ?
pub fn writes_exec(i: &Instruction) -> bool {
    let b = i.base_name();
    if b.starts_with("v_cmpx") {
        return true;
    }
    if i.n_dst == 0 {
        return false;
    }
    let is_exec = |s: Src| matches!(s, Src::ExecLo | Src::ExecHi);
    let direct = i
        .operands
        .iter()
        .take(usize::from(i.n_dst))
        .any(|o| is_exec(o.src));
    direct || (b.contains("saveexec") || b.contains("wrexec"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdna2::{WaveSize, decode_stream};

    fn cfg(words: &[u32]) -> Result<Cfg, CfgError> {
        build_cfg(decode_stream(words, WaveSize::W32).unwrap())
    }

    const ENDPGM: u32 = 0xBF81_0000;
    const NOP: u32 = 0xBF80_0000;

    #[test]
    fn straight_line_is_one_block() {
        let c = cfg(&[NOP, NOP, ENDPGM]).unwrap();
        assert_eq!(c.blocks.len(), 1);
        assert!(c.is_straight_line());
        assert!(!c.has_loop());
        assert_eq!(c.body(0).len(), 2);
    }

    #[test]
    fn conditional_branch_splits_blocks() {
        // 0: s_cbranch_scc1 +1 ; 1: nop ; 2: endpgm
        let c = cfg(&[0xBF85_0001, NOP, ENDPGM]).unwrap();
        assert_eq!(c.blocks.len(), 3);
        assert_eq!(
            c.blocks[0].term,
            Terminator::Cond {
                cond: BranchCond::Scc1,
                taken: 2,
                fall: 1
            }
        );
        assert_eq!(c.blocks[1].term, Terminator::Fallthrough(2));
        assert_eq!(c.blocks[2].term, Terminator::End);
        assert_eq!(c.successors(0), vec![2, 1]);
    }

    #[test]
    fn backward_branch_is_a_loop() {
        // 0: nop ; 1: s_cbranch_execnz -2 (vers 0) ; 2: endpgm
        let c = cfg(&[NOP, 0xBF89_FFFE, ENDPGM]).unwrap();
        assert!(c.has_loop());
        assert_eq!(
            c.blocks[0].term,
            Terminator::Cond {
                cond: BranchCond::Execnz,
                taken: 0,
                fall: 1
            }
        );
    }

    #[test]
    fn bad_targets_and_missing_end_are_errors() {
        assert!(matches!(
            cfg(&[0xBF82_0010, ENDPGM]),
            Err(CfgError::BadTarget { .. })
        ));
        assert_eq!(cfg(&[NOP, NOP]).unwrap_err(), CfgError::NoEnd);
        assert_eq!(build_cfg(vec![]).unwrap_err(), CfgError::Empty);
        // s_setpc_b64 s[2:3] (encodage llvm-mc)
        assert!(matches!(
            cfg(&[0xBE80_2002, ENDPGM]),
            Err(CfgError::Indirect(_))
        ));
    }

    #[test]
    fn exec_writers_are_detected() {
        let d = |w: &[u32]| decode_stream(w, WaveSize::W64).unwrap()[0].inst.clone();
        // s_mov_b64 exec, -1
        assert!(writes_exec(&d(&[0xBEFE_04C1])));
        // s_and_saveexec_b64 s[0:1], vcc
        assert!(writes_exec(&d(&[0xBE80_246A])));
        // v_cmpx_lt_f32 v0, v1
        assert!(writes_exec(&d(&[0x7C22_0300])));
        // s_mov_b64 s[0:1], s[2:3]
        assert!(!writes_exec(&d(&[0xBE80_0402])));
    }
}
