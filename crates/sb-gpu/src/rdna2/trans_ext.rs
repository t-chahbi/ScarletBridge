//! Extensions VOP SDWA et DPP (DPP16, DPP8) : opérandes sélectionnés/permutés avant l'opération,
//! écriture de la destination filtrée après.
//!
//! La sémantique DPP/SDWA suit le guide ISA public ; elle n'est pas vérifiable sur GPU (le
//! matériel n'exécute pas le code RDNA d'origine) : les tests GPU comparent à un oracle CPU qui
//! applique la même lecture du guide.
//! DPP utilise `OpGroupNonUniformShuffle` (capacités `GroupNonUniformShuffle`).

use super::inst::{Ext, Instruction, Src};
use super::translate::{DstXform, TResult, Tr, TranslateError, unsupported};
use crate::dxbc_to_spirv::spv::{Sc, Val};

const OP_ISUB: u16 = 130;
const OP_IADD: u16 = 128;
const OP_UGE: u16 = 174;
const OP_ULT: u16 = 176;
const OP_OR: u16 = 197;
const OP_AND: u16 = 199;
const OP_NOT: u16 = 200;
const OP_SHR: u16 = 194;
const OP_SHL: u16 = 196;
const OP_BFSEXT: u16 = 202;
const OP_BFUEXT: u16 = 203;

impl Tr<'_> {
    /// Applique le sélecteur SDWA (`sel` : 0..=3 octet, 4..=5 mot, 6 dword) à `v`.
    fn sdwa_select(&mut self, v: Val, sel: u8, sext: bool) -> Val {
        if sel >= 6 {
            return v;
        }
        let (bits, shift) = if sel < 4 {
            (8u32, u32::from(sel) * 8)
        } else {
            (16, u32::from(sel - 4) * 16)
        };
        let (off, w) = (self.cu(shift), self.cu(bits));
        if sext {
            let vi = self.spv.bitcast(v, Sc::I32);
            let r = self.spv.val(OP_BFSEXT, Sc::I32, 1, &[vi.id, off.id, w.id]);
            self.spv.bitcast(r, Sc::U32)
        } else {
            self.spv.val(OP_BFUEXT, Sc::U32, 1, &[v.id, off.id, w.id])
        }
    }

    pub(crate) fn valu_with_ext(&mut self, i: &Instruction) -> TResult<()> {
        let nd = usize::from(i.n_dst);
        if i.operands.len() <= nd {
            return unsupported(i);
        }
        let mut inst = i.clone();
        inst.mods.ext = Ext::None;
        let mut xform = DstXform::default();
        match i.mods.ext {
            Ext::Sdwa => {
                let sd = i.mods.sdwa().unwrap_or_default();
                inst.mods.clamp = sd.clamp;
                inst.mods.omod = sd.omod;
                let flags = [(sd.src0_sel, sd.src0_sext), (sd.src1_sel, sd.src1_sext)];
                for (k, (sel, sext)) in flags.into_iter().enumerate() {
                    let Some(o) = inst.operands.get(nd + k).copied() else {
                        break;
                    };
                    // seules src0/src1 explicites sont concernées (pas les VCC implicites)
                    if matches!(o.src, Src::VccLo) && o.width > 0 && k > 0 {
                        break;
                    }
                    let raw = self.src_u32(&o)?;
                    let v = self.sdwa_select(raw, sel, sext);
                    self.ext_vals[k] = Some(v);
                    inst.operands[nd + k].src = Src::Special(0x1F0 + k as u16);
                }
                if i.encoding != super::inst::Encoding::Vopc {
                    xform.sdwa = Some((sd.dst_sel, sd.dst_unused));
                }
            }
            Ext::Dpp16 | Ext::Dpp8 => {
                let o = inst.operands[nd];
                let Src::Vgpr(r) = o.src else {
                    return unsupported(i);
                };
                let x = {
                    let var = self.vvar(usize::from(r));
                    self.load_u32(var)
                };
                let (shuffled, valid, enable) = self.dpp_source(i, x)?;
                // lane invalide : 0 si bound_ctrl, sinon l'écriture est sautée
                let bound = i.mods.dpp16().is_some_and(|d| d.bound_ctrl);
                let z = self.cu(0);
                let val = if bound {
                    self.select(valid, shuffled, z)
                } else {
                    shuffled
                };
                self.ext_vals[0] = Some(val);
                inst.operands[nd].src = Src::Special(0x1F0);
                let mut write_ok = enable;
                if !bound {
                    write_ok = self.bool_and(write_ok, valid);
                }
                xform.valid = Some(write_ok);
            }
            Ext::None => {}
        }
        self.dst_xform = Some(xform);
        let r = self.translate_inst(&inst);
        self.dst_xform = None;
        self.ext_vals = [None; 2];
        r
    }

    /// Valeur de `src0` permutée par DPP, validité de la lane source et activation de la lane.
    fn dpp_source(&mut self, i: &Instruction, x: Val) -> TResult<(Val, Val, Val)> {
        self.spv.caps.insert(61);
        self.spv.caps.insert(64);
        self.spv.caps.insert(65);
        let lane = self.lane_id();
        let (c15, cn16, c3, c7) = (self.cu(15), self.cu(!15u32), self.cu(3), self.cu(7));
        let p = self.op2(OP_AND, Sc::U32, lane, c15);
        let row_base = self.op2(OP_AND, Sc::U32, lane, cn16);
        let truev = Val {
            id: self.spv.c_bool(true),
            sc: Sc::Bool,
            n: 1,
        };
        let (src_lane, valid) = if let Some((sel, _)) = i.mods.dpp8() {
            let mut packed = 0u32;
            for (k, s) in sel.iter().enumerate() {
                packed |= u32::from(*s & 7) << (3 * k);
            }
            let (l8, cn8) = (self.op2(OP_AND, Sc::U32, lane, c7), self.cu(!7u32));
            let c3m = self.cu(3);
            let sh = self.op2(132, Sc::U32, l8, c3m);
            let pk = self.cu(packed);
            let t = self.op2(OP_SHR, Sc::U32, pk, sh);
            let s3 = self.op2(OP_AND, Sc::U32, t, c7);
            let base = self.op2(OP_AND, Sc::U32, lane, cn8);
            (self.op2(OP_OR, Sc::U32, base, s3), truev)
        } else {
            let d = i.mods.dpp16().unwrap_or_default();
            let c = u32::from(d.ctrl);
            let n = self.cu(c & 15);
            let (sp, valid) = match c {
                0..=0xFF => {
                    let q = self.op2(OP_AND, Sc::U32, p, c3);
                    let c2 = self.cu(2);
                    let sh = self.op2(132, Sc::U32, q, c2);
                    let perm = self.cu(c);
                    let t = self.op2(OP_SHR, Sc::U32, perm, sh);
                    let sel = self.op2(OP_AND, Sc::U32, t, c3);
                    let nc3 = self.cu(!3u32);
                    let base = self.op2(OP_AND, Sc::U32, p, nc3);
                    (self.op2(OP_OR, Sc::U32, base, sel), truev)
                }
                0x101..=0x10F => {
                    let s = self.op2(OP_IADD, Sc::U32, p, n);
                    let c16 = self.cu(16);
                    (s, self.op2(OP_ULT, Sc::Bool, s, c16))
                }
                0x111..=0x11F => {
                    let s = self.op2(OP_ISUB, Sc::U32, p, n);
                    (s, self.op2(OP_UGE, Sc::Bool, p, n))
                }
                0x121..=0x12F => {
                    let c16 = self.cu(16);
                    let t = self.op2(OP_IADD, Sc::U32, p, c16);
                    let s = self.op2(OP_ISUB, Sc::U32, t, n);
                    (self.op2(OP_AND, Sc::U32, s, c15), truev)
                }
                0x140 => (self.op2(OP_ISUB, Sc::U32, c15, p), truev),
                0x141 => {
                    let (c8, nc8) = (self.cu(8), self.cu(!7u32));
                    let hi = self.op2(OP_AND, Sc::U32, p, c8);
                    let _ = nc8;
                    let lo = self.op2(OP_AND, Sc::U32, p, c7);
                    let m = self.op2(OP_ISUB, Sc::U32, c7, lo);
                    (self.op2(OP_OR, Sc::U32, hi, m), truev)
                }
                0x150..=0x15F => (n, truev),
                0x160..=0x16F => (self.op2(198, Sc::U32, p, n), truev),
                _ => {
                    return Err(TranslateError::Unsupported(format!(
                        "{} dpp_ctrl {c:#x}",
                        i.name
                    )));
                }
            };
            (self.op2(OP_OR, Sc::U32, row_base, sp), valid)
        };
        let scope = self.cu(3);
        let r = self
            .spv
            .val(345, Sc::U32, 1, &[scope.id, x.id, src_lane.id]);
        // activation : row_mask / bank_mask (DPP8 : toujours)
        let enable = if let Some(d) = i.mods.dpp16() {
            let c4 = self.cu(4);
            let row = self.op2(OP_SHR, Sc::U32, lane, c4);
            let bank = {
                let c2b = self.cu(2);
                let t = self.op2(OP_SHR, Sc::U32, p, c2b);
                self.op2(OP_AND, Sc::U32, t, c3)
            };
            let (rm, bm, one) = (
                self.cu(u32::from(d.row_mask)),
                self.cu(u32::from(d.bank_mask)),
                self.cu(1),
            );
            let rb = {
                let t = self.op2(OP_SHR, Sc::U32, rm, row);
                self.op2(OP_AND, Sc::U32, t, one)
            };
            let bb = {
                let t = self.op2(OP_SHR, Sc::U32, bm, bank);
                self.op2(OP_AND, Sc::U32, t, one)
            };
            let both = self.op2(OP_AND, Sc::U32, rb, bb);
            let z = self.cu(0);
            self.op2(171, Sc::Bool, both, z)
        } else {
            truev
        };
        Ok((r, valid, enable))
    }

    /// Applique `dst_sel`/`dst_unused` SDWA et la validité DPP à un résultat 32 bits.
    pub(crate) fn apply_dst_xform(&mut self, n: usize, v: Val, x: DstXform) -> Val {
        let mut out = self.spv.bitcast(v, Sc::U32);
        let var = self.vvar(n);
        let old = self.load_u32(var);
        if let Some((sel, unused)) = x.sdwa
            && sel < 6
        {
            let (bits, shift) = if sel < 4 {
                (8u32, u32::from(sel) * 8)
            } else {
                (16, u32::from(sel - 4) * 16)
            };
            let mask_lo = self.cu(if bits == 8 { 0xFF } else { 0xFFFF });
            let sh = self.cu(shift);
            let field = self.op2(OP_AND, Sc::U32, out, mask_lo);
            let placed = self.op2(OP_SHL, Sc::U32, field, sh);
            let mask = self.op2(OP_SHL, Sc::U32, mask_lo, sh);
            out = match unused {
                2 | 3 => {
                    let nm = self.op1(OP_NOT, Sc::U32, mask);
                    let keep = self.op2(OP_AND, Sc::U32, old, nm);
                    self.op2(OP_OR, Sc::U32, keep, placed)
                }
                1 => {
                    // les bits au-dessus du champ reçoivent le signe du champ
                    let top = self.cu(bits - 1);
                    let sgn = {
                        let t = self.op2(OP_SHR, Sc::U32, field, top);
                        let one = self.cu(1);
                        self.op2(OP_AND, Sc::U32, t, one)
                    };
                    let z = self.cu(0);
                    let neg = self.op2(171, Sc::Bool, sgn, z);
                    let nm = self.op1(OP_NOT, Sc::U32, mask);
                    let ones = self.cu(u32::MAX);
                    let above = if shift + bits >= 32 {
                        z
                    } else {
                        let cs = self.cu(shift + bits);
                        self.op2(OP_SHL, Sc::U32, ones, cs)
                    };
                    let fill = self.select(neg, above, z);
                    let _ = nm;
                    self.op2(OP_OR, Sc::U32, placed, fill)
                }
                _ => placed,
            };
        }
        if let Some(ok) = x.valid {
            out = self.select(ok, out, old);
        }
        out
    }
}
