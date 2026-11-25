//! Traduction de l'ALU scalaire (SOP*) et vectorielle 32 bits (VOP*) vers SPIR-V.

use super::inst::{Encoding, Instruction, Operand, Src, WaveSize};
use super::translate::{Stage, TResult, Tr, TranslateError, unsupported};
use crate::dxbc_to_spirv::spv::{Sc, Val};

// Opcodes SPIR-V utilisés (spec §3.32).
const OP_IADD: u16 = 128;
const OP_FADD: u16 = 129;
const OP_ISUB: u16 = 130;
const OP_FSUB: u16 = 131;
const OP_IMUL: u16 = 132;
const OP_FMUL: u16 = 133;
const OP_FDIV: u16 = 136;
const OP_IEQ: u16 = 170;
const OP_INE: u16 = 171;
const OP_UGT: u16 = 172;
const OP_SGT: u16 = 173;
const OP_UGE: u16 = 174;
const OP_SGE: u16 = 175;
const OP_ULT: u16 = 176;
const OP_SLT: u16 = 177;
const OP_ULE: u16 = 178;
const OP_SLE: u16 = 179;
const OP_OR: u16 = 197;
const OP_XOR: u16 = 198;
const OP_AND: u16 = 199;
const OP_NOT: u16 = 200;
const OP_SHR: u16 = 194;
const OP_SAR: u16 = 195;
const OP_SHL: u16 = 196;
const OP_BFSEXT: u16 = 202;
const OP_BFUEXT: u16 = 203;
const OP_BITREV: u16 = 204;
const OP_BITCNT: u16 = 205;
const OP_CVT_F2U: u16 = 109;
const OP_CVT_F2S: u16 = 110;
const OP_CVT_S2F: u16 = 111;
const OP_CVT_U2F: u16 = 112;

// Instructions GLSL.std.450.
const G_RNDNE: u32 = 2;
const G_TRUNC: u32 = 3;
const G_SABS: u32 = 5;
const G_FLOOR: u32 = 8;
const G_CEIL: u32 = 9;
const G_FRACT: u32 = 10;
const G_SIN: u32 = 13;
const G_COS: u32 = 14;
const G_EXP2: u32 = 29;
const G_LOG2: u32 = 30;
const G_SQRT: u32 = 31;
const G_RSQRT: u32 = 32;
const G_FMIN: u32 = 37;
const G_UMIN: u32 = 38;
const G_SMIN: u32 = 39;
const G_FMAX: u32 = 40;
const G_UMAX: u32 = 41;
const G_SMAX: u32 = 42;
const G_FCLAMP: u32 = 43;
const G_FMA: u32 = 50;
const G_LDEXP: u32 = 53;
const G_PACK_HALF: u32 = 58;
const G_UNPACK_HALF: u32 = 62;
const G_FIND_ILSB: u32 = 73;
const G_FIND_UMSB: u32 = 75;

impl Tr<'_> {
    /// Point d'entrée : traduit une instruction décodée.
    pub(crate) fn translate_inst(&mut self, i: &Instruction) -> TResult<()> {
        match i.encoding {
            Encoding::Sop1 | Encoding::Sop2 | Encoding::Sopk | Encoding::Sopc => self.salu(i),
            Encoding::Sopp => self.sopp(i),
            Encoding::Vop1 | Encoding::Vop2 | Encoding::Vopc | Encoding::Vop3 => self.valu(i),
            Encoding::Smem => self.smem(i),
            Encoding::Mubuf | Encoding::Mtbuf => self.vbuf(i),
            Encoding::Mimg => self.mimg(i),
            Encoding::Exp => self.export(i),
            Encoding::Vintrp => self.vintrp(i),
            Encoding::Ds => self.ds(i),
            _ => unsupported(i),
        }
    }

    // ------------------------------------------------------------ SOPP
    fn sopp(&mut self, i: &Instruction) -> TResult<()> {
        match i.name {
            "s_nop" | "s_waitcnt" | "s_sleep" | "s_setprio" | "s_inst_prefetch" | "s_clause"
            | "s_icache_inv" | "s_denorm_mode" | "s_round_mode" | "s_ttracedata" | "s_code_end"
            | "s_wakeup" => Ok(()),
            "s_barrier" if self.cfg.stage == Stage::Compute => {
                // OpControlBarrier Workgroup Workgroup AcquireRelease|WorkgroupMemory
                let (scope, sem) = (self.spv.c_u32(2), self.spv.c_u32(0x108));
                self.spv.emit_void(224, &[scope, scope, sem]);
                Ok(())
            }
            _ => unsupported(i),
        }
    }

    // ------------------------------------------------------------ SALU
    fn s_logic(&mut self, name: &str, a: Val, b: Val) -> Option<Val> {
        let r = match name {
            "and" => self.op2(OP_AND, Sc::U32, a, b),
            "or" => self.op2(OP_OR, Sc::U32, a, b),
            "xor" => self.op2(OP_XOR, Sc::U32, a, b),
            "andn2" => {
                let nb = self.op1(OP_NOT, Sc::U32, b);
                self.op2(OP_AND, Sc::U32, a, nb)
            }
            "orn2" => {
                let nb = self.op1(OP_NOT, Sc::U32, b);
                self.op2(OP_OR, Sc::U32, a, nb)
            }
            "nand" => {
                let t = self.op2(OP_AND, Sc::U32, a, b);
                self.op1(OP_NOT, Sc::U32, t)
            }
            "nor" => {
                let t = self.op2(OP_OR, Sc::U32, a, b);
                self.op1(OP_NOT, Sc::U32, t)
            }
            "xnor" => {
                let t = self.op2(OP_XOR, Sc::U32, a, b);
                self.op1(OP_NOT, Sc::U32, t)
            }
            _ => return None,
        };
        Some(r)
    }

    fn cmp_op(signed: bool, rel: &str) -> Option<u16> {
        Some(match (rel, signed) {
            ("eq", _) => OP_IEQ,
            ("lg", _) | ("ne", _) => OP_INE,
            ("gt", true) => OP_SGT,
            ("gt", false) => OP_UGT,
            ("ge", true) => OP_SGE,
            ("ge", false) => OP_UGE,
            ("lt", true) => OP_SLT,
            ("lt", false) => OP_ULT,
            ("le", true) => OP_SLE,
            ("le", false) => OP_ULE,
            _ => return None,
        })
    }

    fn scc_nonzero(&mut self, r: Val) {
        let b = self.ne0(r);
        self.scc_set(b);
    }

    fn salu(&mut self, i: &Instruction) -> TResult<()> {
        let name = i.base_name();
        let dst = i.dst().copied();
        let srcs: Vec<Operand> = i.srcs().to_vec();
        let nsrc = srcs.len();
        let w64 = name.ends_with("_b64") || name.ends_with("_u64") || name.ends_with("_i64");
        // --- SOPK : registre + simm16
        if i.encoding == Encoding::Sopk {
            return self.salu_sopk(i);
        }
        // --- SOPC
        if i.encoding == Encoding::Sopc {
            let (a, b) = (&srcs[0], &srcs[1]);
            if let Some(rest) = name.strip_prefix("s_cmp_") {
                let (rel, ty) = rest
                    .split_once('_')
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                if ty == "u64" {
                    let ((al, ah), (bl, bh)) = (self.src_pair(a)?, self.src_pair(b)?);
                    let (el, eh) = (
                        self.op2(OP_IEQ, Sc::Bool, al, bl),
                        self.op2(OP_IEQ, Sc::Bool, ah, bh),
                    );
                    let eq = self.bool_and(el, eh);
                    let r = if rel == "lg" { self.bool_not(eq) } else { eq };
                    self.scc_set(r);
                    return Ok(());
                }
                let op = Self::cmp_op(ty == "i32", rel)
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                let (av, bv) = (self.src_u32(a)?, self.src_u32(b)?);
                let (av, bv) = if ty == "i32" {
                    (self.spv.bitcast(av, Sc::I32), self.spv.bitcast(bv, Sc::I32))
                } else {
                    (av, bv)
                };
                let r = self.op2(op, Sc::Bool, av, bv);
                self.scc_set(r);
                return Ok(());
            }
            if name == "s_bitcmp0_b32" || name == "s_bitcmp1_b32" {
                let (av, bv) = (self.src_u32(a)?, self.src_u32(b)?);
                let m31 = self.cu(31);
                let sh = self.op2(OP_AND, Sc::U32, bv, m31);
                let t = self.op2(OP_SHR, Sc::U32, av, sh);
                let one = self.cu(1);
                let bit = self.op2(OP_AND, Sc::U32, t, one);
                let nz = self.ne0(bit);
                let r = if name == "s_bitcmp0_b32" {
                    self.bool_not(nz)
                } else {
                    nz
                };
                self.scc_set(r);
                return Ok(());
            }
            return unsupported(i);
        }
        let Some(dst) = dst else {
            return unsupported(i);
        };

        // --- saveexec : sdst = exec ; exec = f(src, exec)
        if let Some(base) = name.strip_prefix("s_").and_then(|n| {
            n.strip_suffix("_saveexec_b64")
                .or_else(|| n.strip_suffix("_saveexec_b32"))
        }) {
            let is64 = name.ends_with("_b64");
            if is64 != (self.wave == WaveSize::W64) {
                return Err(TranslateError::Unsupported(format!(
                    "{name} dans une vague {:?}",
                    self.wave
                )));
            }
            let (el, eh) = (
                self.sload(126),
                if is64 { self.sload(127) } else { self.cu(0) },
            );
            let (sl, sh) = if is64 {
                self.src_pair(&srcs[0])?
            } else {
                (self.src_u32(&srcs[0])?, self.cu(0))
            };
            let (nl, nh) = (self.s_logic(base, sl, el), self.s_logic(base, sh, eh));
            let (Some(nl), Some(nh)) = (nl, nh) else {
                return unsupported(i);
            };
            if is64 {
                self.write_s_pair(&dst, el, eh)?;
            } else {
                self.write_s(&dst, el)?;
            }
            let (a, b) = (self.svar(126), self.svar(127));
            self.store_u32(a, nl);
            if is64 {
                self.store_u32(b, nh);
                let any = self.op2(OP_OR, Sc::U32, nl, nh);
                self.scc_nonzero(any);
            } else {
                self.scc_nonzero(nl);
            }
            return Ok(());
        }

        // --- 64 bits (composante par composante)
        if w64 {
            return self.salu64(i, name, &dst, &srcs);
        }

        // --- 32 bits
        let a = if nsrc >= 1 {
            Some(self.src_u32(&srcs[0])?)
        } else {
            None
        };
        let b = if nsrc >= 2 {
            Some(self.src_u32(&srcs[1])?)
        } else {
            None
        };
        let base = name.strip_suffix("_b32").unwrap_or(name);
        let logic = base.strip_prefix("s_").and_then(|n| match (a, b) {
            (Some(a), Some(b)) => self.s_logic(n, a, b),
            _ => None,
        });
        if let Some(r) = logic {
            self.scc_nonzero(r);
            return self.write_s(&dst, r);
        }
        let r = match (name, a, b) {
            ("s_mov_b32", Some(a), _) => a,
            ("s_cmov_b32", Some(a), _) => {
                let old = self.src_u32(&dst)?;
                let c = self.scc_get();
                self.select(c, a, old)
            }
            ("s_cselect_b32", Some(a), Some(b)) => {
                let c = self.scc_get();
                self.select(c, a, b)
            }
            ("s_not_b32", Some(a), _) => {
                let r = self.op1(OP_NOT, Sc::U32, a);
                self.scc_nonzero(r);
                r
            }
            ("s_brev_b32", Some(a), _) => self.op1(OP_BITREV, Sc::U32, a),
            ("s_bcnt1_i32_b32", Some(a), _) => {
                let r = self.op1(OP_BITCNT, Sc::U32, a);
                self.scc_nonzero(r);
                r
            }
            ("s_ff1_i32_b32", Some(a), _) => {
                let r = self.glsl(Sc::I32, G_FIND_ILSB, &[a]);
                self.spv.bitcast(r, Sc::U32)
            }
            ("s_flbit_i32_b32", Some(a), _) => {
                let msb = self.glsl(Sc::I32, G_FIND_UMSB, &[a]);
                let msb = self.spv.bitcast(msb, Sc::U32);
                let c31 = self.cu(31);
                let lz = self.op2(OP_ISUB, Sc::U32, c31, msb);
                let zero = self.cu(0);
                let is0 = self.op2(OP_IEQ, Sc::Bool, a, zero);
                let m1 = self.cu(u32::MAX);
                self.select(is0, m1, lz)
            }
            ("s_abs_i32", Some(a), _) => {
                let ai = self.spv.bitcast(a, Sc::I32);
                let r = self.glsl(Sc::I32, G_SABS, &[ai]);
                let r = self.spv.bitcast(r, Sc::U32);
                self.scc_nonzero(r);
                r
            }
            ("s_sext_i32_i8", Some(a), _) | ("s_sext_i32_i16", Some(a), _) => {
                let w = if name.ends_with("i8") { 8 } else { 16 };
                let (z, c) = (self.cu(0), self.cu(w));
                let ai = self.spv.bitcast(a, Sc::I32);
                let r = self.spv.val(OP_BFSEXT, Sc::I32, 1, &[ai.id, z.id, c.id]);
                self.spv.bitcast(r, Sc::U32)
            }
            ("s_add_u32", Some(a), Some(b)) => {
                let r = self.op2(OP_IADD, Sc::U32, a, b);
                let c = self.op2(OP_ULT, Sc::Bool, r, a);
                self.scc_set(c);
                r
            }
            ("s_sub_u32", Some(a), Some(b)) => {
                let r = self.op2(OP_ISUB, Sc::U32, a, b);
                let c = self.op2(OP_ULT, Sc::Bool, a, b);
                self.scc_set(c);
                r
            }
            ("s_add_i32", Some(a), Some(b)) | ("s_sub_i32", Some(a), Some(b)) => {
                let add = name == "s_add_i32";
                let r = self.op2(if add { OP_IADD } else { OP_ISUB }, Sc::U32, a, b);
                let (x, y) = (
                    self.op2(OP_XOR, Sc::U32, a, b),
                    self.op2(OP_XOR, Sc::U32, a, r),
                );
                let ov = if add {
                    let nx = self.op1(OP_NOT, Sc::U32, x);
                    self.op2(OP_AND, Sc::U32, nx, y)
                } else {
                    self.op2(OP_AND, Sc::U32, x, y)
                };
                let c31 = self.cu(31);
                let top = self.op2(OP_SHR, Sc::U32, ov, c31);
                self.scc_nonzero(top);
                r
            }
            ("s_addc_u32", Some(a), Some(b)) | ("s_subb_u32", Some(a), Some(b)) => {
                let add = name == "s_addc_u32";
                let cin = {
                    let c = self.scc_get();
                    let (one, zero) = (self.cu(1), self.cu(0));
                    self.select(c, one, zero)
                };
                let t = self.op2(if add { OP_IADD } else { OP_ISUB }, Sc::U32, a, b);
                let r = self.op2(if add { OP_IADD } else { OP_ISUB }, Sc::U32, t, cin);
                let (c1, c2) = if add {
                    (
                        self.op2(OP_ULT, Sc::Bool, t, a),
                        self.op2(OP_ULT, Sc::Bool, r, t),
                    )
                } else {
                    (
                        self.op2(OP_ULT, Sc::Bool, a, b),
                        self.op2(OP_ULT, Sc::Bool, t, cin),
                    )
                };
                let c = self.bool_or(c1, c2);
                self.scc_set(c);
                r
            }
            ("s_min_u32" | "s_max_u32" | "s_min_i32" | "s_max_i32", Some(a), Some(b)) => {
                let signed = name.ends_with("i32");
                let is_min = name.contains("min");
                let (x, y) = if signed {
                    (self.spv.bitcast(a, Sc::I32), self.spv.bitcast(b, Sc::I32))
                } else {
                    (a, b)
                };
                let cmp = Self::cmp_op(signed, if is_min { "lt" } else { "gt" }).unwrap_or(OP_ULT);
                let pick_a = self.op2(cmp, Sc::Bool, x, y);
                self.scc_set(pick_a);
                self.select(pick_a, a, b)
            }
            ("s_lshl_b32" | "s_lshr_b32" | "s_ashr_i32", Some(a), Some(b)) => {
                let m31 = self.cu(31);
                let sh = self.op2(OP_AND, Sc::U32, b, m31);
                let op = match name {
                    "s_lshl_b32" => OP_SHL,
                    "s_lshr_b32" => OP_SHR,
                    _ => OP_SAR,
                };
                let r = if op == OP_SAR {
                    let ai = self.spv.bitcast(a, Sc::I32);
                    let r = self.spv.val(OP_SAR, Sc::I32, 1, &[ai.id, sh.id]);
                    self.spv.bitcast(r, Sc::U32)
                } else {
                    self.op2(op, Sc::U32, a, sh)
                };
                self.scc_nonzero(r);
                r
            }
            ("s_mul_i32", Some(a), Some(b)) => self.op2(OP_IMUL, Sc::U32, a, b),
            ("s_mul_hi_u32", Some(a), Some(b)) => self.mul_hi(a, b, false),
            ("s_mul_hi_i32", Some(a), Some(b)) => self.mul_hi(a, b, true),
            ("s_bfe_u32" | "s_bfe_i32", Some(a), Some(b)) => {
                let signed = name == "s_bfe_i32";
                let r = self.bfe(a, b, signed);
                self.scc_nonzero(r);
                r
            }
            ("s_bfm_b32", Some(a), Some(b)) => {
                let m31 = self.cu(31);
                let (w, o) = (
                    self.op2(OP_AND, Sc::U32, a, m31),
                    self.op2(OP_AND, Sc::U32, b, m31),
                );
                let one = self.cu(1);
                let sh = self.op2(OP_SHL, Sc::U32, one, w);
                let mask = self.op2(OP_ISUB, Sc::U32, sh, one);
                self.op2(OP_SHL, Sc::U32, mask, o)
            }
            ("s_absdiff_i32", Some(a), Some(b)) => {
                let (x, y) = (self.spv.bitcast(a, Sc::I32), self.spv.bitcast(b, Sc::I32));
                let d = self.spv.val(OP_ISUB, Sc::I32, 1, &[x.id, y.id]);
                let r = self.glsl(Sc::I32, G_SABS, &[d]);
                let r = self.spv.bitcast(r, Sc::U32);
                self.scc_nonzero(r);
                r
            }
            ("s_pack_ll_b32_b16" | "s_pack_lh_b32_b16" | "s_pack_hh_b32_b16", Some(a), Some(b)) => {
                let (lo_mask, hi_mask, c16) = (self.cu(0xFFFF), self.cu(0xFFFF_0000), self.cu(16));
                let lo = if name == "s_pack_hh_b32_b16" {
                    self.op2(OP_SHR, Sc::U32, a, c16)
                } else {
                    self.op2(OP_AND, Sc::U32, a, lo_mask)
                };
                let hi = if name == "s_pack_ll_b32_b16" {
                    self.op2(OP_SHL, Sc::U32, b, c16)
                } else {
                    self.op2(OP_AND, Sc::U32, b, hi_mask)
                };
                self.op2(OP_OR, Sc::U32, lo, hi)
            }
            ("s_getpc_b64", _, _) => return unsupported(i),
            _ => return unsupported(i),
        };
        self.write_s(&dst, r)
    }

    /// `s_bfe_*` : `src1[4:0]` = offset, `src1[22:16]` = largeur.
    fn bfe(&mut self, a: Val, b: Val, signed: bool) -> Val {
        let (m31, c16, m7f, c32) = (self.cu(31), self.cu(16), self.cu(0x7F), self.cu(32));
        let off = self.op2(OP_AND, Sc::U32, b, m31);
        let w = {
            let t = self.op2(OP_SHR, Sc::U32, b, c16);
            self.op2(OP_AND, Sc::U32, t, m7f)
        };
        let room = self.op2(OP_ISUB, Sc::U32, c32, off);
        let w = self.glsl(Sc::U32, G_UMIN, &[w, room]);
        if signed {
            let ai = self.spv.bitcast(a, Sc::I32);
            let r = self.spv.val(OP_BFSEXT, Sc::I32, 1, &[ai.id, off.id, w.id]);
            self.spv.bitcast(r, Sc::U32)
        } else {
            self.spv.val(OP_BFUEXT, Sc::U32, 1, &[a.id, off.id, w.id])
        }
    }

    fn mul_hi(&mut self, a: Val, b: Val, signed: bool) -> Val {
        let u = self.spv.ty(Sc::U32, 1);
        let st = self.spv.structure(&[u, u]);
        let (x, y) = if signed {
            let (x, y) = (self.spv.bitcast(a, Sc::I32), self.spv.bitcast(b, Sc::I32));
            let i = self.spv.ty(Sc::I32, 1);
            let st_i = self.spv.structure(&[i, i]);
            let r = self.spv.emit(152, st_i, &[x.id, y.id]);
            let hi = self.spv.val(81, Sc::I32, 1, &[r, 1]);
            return self.spv.bitcast(hi, Sc::U32);
        } else {
            (a, b)
        };
        let r = self.spv.emit(151, st, &[x.id, y.id]);
        self.spv.val(81, Sc::U32, 1, &[r, 1])
    }

    fn salu_sopk(&mut self, i: &Instruction) -> TResult<()> {
        let name = i.name;
        let imm = i.mods.imm as u32 & 0xFFFF;
        let sext = ((imm << 16) as i32 >> 16) as u32;
        if matches!(
            name,
            "s_waitcnt_vscnt"
                | "s_waitcnt_vmcnt"
                | "s_waitcnt_expcnt"
                | "s_waitcnt_lgkmcnt"
                | "s_version"
        ) {
            return Ok(());
        }
        if let Some(rest) = name.strip_prefix("s_cmpk_") {
            let (rel, ty) = rest
                .split_once('_')
                .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
            let signed = ty == "i32";
            let op = Self::cmp_op(signed, rel)
                .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
            let a = self.src_u32(&i.operands[0])?;
            let k = self.cu(if signed { sext } else { imm });
            let (a, k) = if signed {
                (self.spv.bitcast(a, Sc::I32), self.spv.bitcast(k, Sc::I32))
            } else {
                (a, k)
            };
            let r = self.op2(op, Sc::Bool, a, k);
            self.scc_set(r);
            return Ok(());
        }
        let dst = i.operands[0];
        let r = match name {
            "s_movk_i32" => self.cu(sext),
            "s_cmovk_i32" => {
                let old = self.src_u32(&dst)?;
                let k = self.cu(sext);
                let c = self.scc_get();
                self.select(c, k, old)
            }
            "s_addk_i32" => {
                let a = self.src_u32(&dst)?;
                let k = self.cu(sext);
                let r = self.op2(OP_IADD, Sc::U32, a, k);
                let (x, y) = (
                    self.op2(OP_XOR, Sc::U32, a, k),
                    self.op2(OP_XOR, Sc::U32, a, r),
                );
                let nx = self.op1(OP_NOT, Sc::U32, x);
                let ov = self.op2(OP_AND, Sc::U32, nx, y);
                let c31 = self.cu(31);
                let top = self.op2(OP_SHR, Sc::U32, ov, c31);
                self.scc_nonzero(top);
                r
            }
            "s_mulk_i32" => {
                let a = self.src_u32(&dst)?;
                let k = self.cu(sext);
                self.op2(OP_IMUL, Sc::U32, a, k)
            }
            _ => return unsupported(i),
        };
        self.write_s(&dst, r)
    }

    /// Opérations 64 bits scalaires de type masque/logique (composante par composante).
    fn salu64(
        &mut self,
        i: &Instruction,
        name: &str,
        dst: &Operand,
        srcs: &[Operand],
    ) -> TResult<()> {
        let base = name.strip_suffix("_b64").unwrap_or(name);
        let (lo, hi) = match (base, srcs.len()) {
            ("s_mov", 1) => self.src_pair(&srcs[0])?,
            ("s_not", 1) => {
                let (a, b) = self.src_pair(&srcs[0])?;
                let (l, h) = (self.op1(OP_NOT, Sc::U32, a), self.op1(OP_NOT, Sc::U32, b));
                let any = self.op2(OP_OR, Sc::U32, l, h);
                self.scc_nonzero(any);
                (l, h)
            }
            ("s_cselect" | "s_cmov", n) if n >= 1 => {
                let (a, b) = if n == 2 {
                    (srcs[0], srcs[1])
                } else {
                    (srcs[0], *dst)
                };
                let ((al, ah), (bl, bh)) = (self.src_pair(&a)?, self.src_pair(&b)?);
                let c = self.scc_get();
                (self.select(c, al, bl), self.select(c, ah, bh))
            }
            ("s_bcnt1_i32", 1) => {
                let (a, b) = self.src_pair(&srcs[0])?;
                let (ca, cb) = (
                    self.op1(OP_BITCNT, Sc::U32, a),
                    self.op1(OP_BITCNT, Sc::U32, b),
                );
                let r = self.op2(OP_IADD, Sc::U32, ca, cb);
                self.scc_nonzero(r);
                return self.write_s(dst, r);
            }
            (n, 2) if n.starts_with("s_") && !n.starts_with("s_cmp") => {
                let ((al, ah), (bl, bh)) = (self.src_pair(&srcs[0])?, self.src_pair(&srcs[1])?);
                let key = &n[2..];
                let (Some(l), Some(h)) = (self.s_logic(key, al, bl), self.s_logic(key, ah, bh))
                else {
                    return unsupported(i);
                };
                let any = self.op2(OP_OR, Sc::U32, l, h);
                self.scc_nonzero(any);
                (l, h)
            }
            _ => return unsupported(i),
        };
        self.write_s_pair(dst, lo, hi)
    }

    // ------------------------------------------------------------ VALU
    /// Applique `omod`/`clamp` à un résultat flottant puis l'écrit.
    fn finish_f32(&mut self, i: &Instruction, r: Val) -> TResult<()> {
        let mut r = r;
        match i.mods.omod {
            1 => {
                let t = self.cf(2.0);
                r = self.op2(OP_FMUL, Sc::F32, r, t);
            }
            2 => {
                let t = self.cf(4.0);
                r = self.op2(OP_FMUL, Sc::F32, r, t);
            }
            3 => {
                let t = self.cf(0.5);
                r = self.op2(OP_FMUL, Sc::F32, r, t);
            }
            _ => {}
        }
        if i.mods.clamp {
            let (z, o) = (self.cf(0.0), self.cf(1.0));
            r = self.glsl(Sc::F32, G_FCLAMP, &[r, z, o]);
        }
        let dst = *i
            .dst()
            .ok_or_else(|| TranslateError::Unsupported(i.name.into()))?;
        self.write_dst(&dst, r)
    }

    fn fargs(&mut self, i: &Instruction, n: usize) -> TResult<Vec<Val>> {
        let s = i.srcs();
        if s.len() < n {
            return unsupported(i);
        }
        (0..n).map(|k| self.src_f32(&s[k])).collect()
    }

    fn uargs(&mut self, i: &Instruction, n: usize) -> TResult<Vec<Val>> {
        let s = i.srcs();
        if s.len() < n {
            return unsupported(i);
        }
        (0..n).map(|k| self.src_u32(&s[k])).collect()
    }

    fn ibin(&mut self, i: &Instruction, op: u16) -> TResult<()> {
        let a = self.uargs(i, 2)?;
        let r = self.op2(op, Sc::U32, a[0], a[1]);
        let d = *i
            .dst()
            .ok_or_else(|| TranslateError::Unsupported(i.name.into()))?;
        self.write_dst(&d, r)
    }

    fn fbin(&mut self, i: &Instruction, op: u16) -> TResult<()> {
        let a = self.fargs(i, 2)?;
        let r = self.op2(op, Sc::F32, a[0], a[1]);
        self.finish_f32(i, r)
    }

    fn fun1(&mut self, i: &Instruction, g: u32) -> TResult<()> {
        let a = self.fargs(i, 1)?;
        let r = self.glsl(Sc::F32, g, &[a[0]]);
        self.finish_f32(i, r)
    }

    /// `min`/`max`/`med`-style sur entiers signés/non signés via GLSL.
    fn imin_max(&mut self, i: &Instruction, g: u32, signed: bool) -> TResult<()> {
        let a = self.uargs(i, 2)?;
        let sc = if signed { Sc::I32 } else { Sc::U32 };
        let (x, y) = (self.spv.bitcast(a[0], sc), self.spv.bitcast(a[1], sc));
        let r = self.glsl(sc, g, &[x, y]);
        let d = *i
            .dst()
            .ok_or_else(|| TranslateError::Unsupported(i.name.into()))?;
        self.write_dst(&d, r)
    }

    fn valu(&mut self, i: &Instruction) -> TResult<()> {
        let name = i.base_name();
        if i.mods.ext != super::inst::Ext::None {
            return Err(TranslateError::Unsupported(format!(
                "{name} avec extension DPP/SDWA"
            )));
        }
        if name.starts_with("v_cmp") {
            return self.vcmp(i, name);
        }
        match name {
            "v_nop" => Ok(()),
            "v_mov_b32" => {
                let a = self.uargs(i, 1)?;
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, a[0])
            }
            "v_readfirstlane_b32" => {
                let a = self.uargs(i, 1)?;
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.spv.caps.insert(64);
                self.guarded(|t| {
                    let scope = t.cu(3);
                    let r = t.spv.val(338, Sc::U32, 1, &[scope.id, a[0].id]);
                    t.write_s(&d, r)
                })
            }
            "v_readlane_b32" => {
                let a = self.uargs(i, 2)?;
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.spv.caps.insert(65);
                self.guarded(|t| {
                    let scope = t.cu(3);
                    let r = t.spv.val(345, Sc::U32, 1, &[scope.id, a[0].id, a[1].id]);
                    t.write_s(&d, r)
                })
            }
            // --- flottants 32 bits
            "v_add_f32" => self.fbin(i, OP_FADD),
            "v_sub_f32" => self.fbin(i, OP_FSUB),
            "v_subrev_f32" => {
                let a = self.fargs(i, 2)?;
                let r = self.op2(OP_FSUB, Sc::F32, a[1], a[0]);
                self.finish_f32(i, r)
            }
            "v_mul_f32" | "v_mul_legacy_f32" => self.fbin(i, OP_FMUL),
            "v_min_f32" | "v_max_f32" => {
                let a = self.fargs(i, 2)?;
                let g = if name == "v_min_f32" { G_FMIN } else { G_FMAX };
                let r = self.glsl(Sc::F32, g, &[a[0], a[1]]);
                self.finish_f32(i, r)
            }
            "v_fmac_f32" => {
                let a = self.fargs(i, 2)?;
                let dst = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                let acc = self.src_f32(&Operand::new(dst.src, 1))?;
                let r = self.glsl(Sc::F32, G_FMA, &[a[0], a[1], acc]);
                self.finish_f32(i, r)
            }
            "v_fmamk_f32" => {
                // dst = src0 * K + vsrc1
                let s = i.srcs();
                let (a, k, b) = (
                    self.src_f32(&s[0])?,
                    self.src_f32(&s[1])?,
                    self.src_f32(&s[2])?,
                );
                let r = self.glsl(Sc::F32, G_FMA, &[a, k, b]);
                self.finish_f32(i, r)
            }
            "v_fmaak_f32" => {
                // dst = src0 * vsrc1 + K
                let s = i.srcs();
                let (a, b, k) = (
                    self.src_f32(&s[0])?,
                    self.src_f32(&s[1])?,
                    self.src_f32(&s[2])?,
                );
                let r = self.glsl(Sc::F32, G_FMA, &[a, b, k]);
                self.finish_f32(i, r)
            }
            "v_fma_f32" => {
                let a = self.fargs(i, 3)?;
                let r = self.glsl(Sc::F32, G_FMA, &[a[0], a[1], a[2]]);
                self.finish_f32(i, r)
            }
            "v_med3_f32" => {
                let a = self.fargs(i, 3)?;
                let (lo, hi) = (
                    self.glsl(Sc::F32, G_FMIN, &[a[0], a[1]]),
                    self.glsl(Sc::F32, G_FMAX, &[a[0], a[1]]),
                );
                let m = self.glsl(Sc::F32, G_FMIN, &[hi, a[2]]);
                let r = self.glsl(Sc::F32, G_FMAX, &[lo, m]);
                self.finish_f32(i, r)
            }
            "v_min3_f32" | "v_max3_f32" => {
                let a = self.fargs(i, 3)?;
                let g = if name == "v_min3_f32" { G_FMIN } else { G_FMAX };
                let t = self.glsl(Sc::F32, g, &[a[0], a[1]]);
                let r = self.glsl(Sc::F32, g, &[t, a[2]]);
                self.finish_f32(i, r)
            }
            "v_ldexp_f32" => {
                let (x, e) = (self.src_f32(&i.srcs()[0])?, self.src_u32(&i.srcs()[1])?);
                let ei = self.spv.bitcast(e, Sc::I32);
                let ids = [x.id, ei.id];
                let r = self.spv.glsl_op(Sc::F32, 1, G_LDEXP, &ids);
                self.finish_f32(i, r)
            }
            "v_rcp_f32" | "v_rcp_iflag_f32" => {
                let a = self.fargs(i, 1)?;
                let one = self.cf(1.0);
                let r = self.op2(OP_FDIV, Sc::F32, one, a[0]);
                self.finish_f32(i, r)
            }
            "v_rsq_f32" => self.fun1(i, G_RSQRT),
            "v_sqrt_f32" => self.fun1(i, G_SQRT),
            "v_exp_f32" => self.fun1(i, G_EXP2),
            "v_log_f32" => self.fun1(i, G_LOG2),
            "v_floor_f32" => self.fun1(i, G_FLOOR),
            "v_ceil_f32" => self.fun1(i, G_CEIL),
            "v_trunc_f32" => self.fun1(i, G_TRUNC),
            "v_fract_f32" => self.fun1(i, G_FRACT),
            "v_rndne_f32" => self.fun1(i, G_RNDNE),
            "v_sin_f32" | "v_cos_f32" => {
                // Les instructions matérielles travaillent en tours : sin(2π·x).
                let a = self.fargs(i, 1)?;
                let tau = self.cf(std::f32::consts::TAU);
                let x = self.op2(OP_FMUL, Sc::F32, a[0], tau);
                let r = self.glsl(
                    Sc::F32,
                    if name == "v_sin_f32" { G_SIN } else { G_COS },
                    &[x],
                );
                self.finish_f32(i, r)
            }
            // --- conversions
            "v_cvt_f32_i32" | "v_cvt_f32_u32" => {
                let a = self.uargs(i, 1)?;
                let src = if name.ends_with("i32") {
                    self.spv.bitcast(a[0], Sc::I32)
                } else {
                    a[0]
                };
                let r = self.spv.val(
                    if name.ends_with("i32") {
                        OP_CVT_S2F
                    } else {
                        OP_CVT_U2F
                    },
                    Sc::F32,
                    1,
                    &[src.id],
                );
                self.finish_f32(i, r)
            }
            "v_cvt_i32_f32" => {
                let a = self.fargs(i, 1)?;
                let r = self.spv.val(OP_CVT_F2S, Sc::I32, 1, &[a[0].id]);
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            "v_cvt_u32_f32" => {
                let a = self.fargs(i, 1)?;
                let r = self.spv.val(OP_CVT_F2U, Sc::U32, 1, &[a[0].id]);
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            "v_cvt_f32_ubyte0" | "v_cvt_f32_ubyte1" | "v_cvt_f32_ubyte2" | "v_cvt_f32_ubyte3" => {
                let a = self.uargs(i, 1)?;
                let k = u32::from(name.as_bytes()[name.len() - 1] - b'0') * 8;
                let (off, w) = (self.cu(k), self.cu(8));
                let b = self
                    .spv
                    .val(OP_BFUEXT, Sc::U32, 1, &[a[0].id, off.id, w.id]);
                let r = self.spv.val(OP_CVT_U2F, Sc::F32, 1, &[b.id]);
                self.finish_f32(i, r)
            }
            "v_cvt_f16_f32" => {
                let a = self.fargs(i, 1)?;
                let z = self.cf(0.0);
                let v2 = self.spv.construct(Sc::F32, &[a[0].id, z.id]);
                let r = self.spv.glsl_op(Sc::U32, 1, G_PACK_HALF, &[v2.id]);
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            "v_cvt_f32_f16" => {
                let a = self.uargs(i, 1)?;
                let c = self.cu(0xFFFF);
                let h = self.op2(OP_AND, Sc::U32, a[0], c);
                let v2 = self.spv.glsl_op(Sc::F32, 2, G_UNPACK_HALF, &[h.id]);
                let r = self.spv.extract(v2, 0);
                self.finish_f32(i, r)
            }
            "v_cvt_pkrtz_f16_f32" => {
                let a = self.fargs(i, 2)?;
                let v2 = self.spv.construct(Sc::F32, &[a[0].id, a[1].id]);
                let r = self.spv.glsl_op(Sc::U32, 1, G_PACK_HALF, &[v2.id]);
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            // --- entiers
            "v_not_b32" => {
                let a = self.uargs(i, 1)?;
                let r = self.op1(OP_NOT, Sc::U32, a[0]);
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            "v_bfrev_b32" => {
                let a = self.uargs(i, 1)?;
                let r = self.op1(OP_BITREV, Sc::U32, a[0]);
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            "v_ffbh_u32" => {
                let a = self.uargs(i, 1)?;
                let msb = self.glsl(Sc::I32, G_FIND_UMSB, &[a[0]]);
                let msb = self.spv.bitcast(msb, Sc::U32);
                let c31 = self.cu(31);
                let lz = self.op2(OP_ISUB, Sc::U32, c31, msb);
                let z = self.cu(0);
                let is0 = self.op2(OP_IEQ, Sc::Bool, a[0], z);
                let m1 = self.cu(u32::MAX);
                let r = self.select(is0, m1, lz);
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            "v_add_nc_u32" => self.ibin(i, OP_IADD),
            "v_sub_nc_u32" => self.ibin(i, OP_ISUB),
            "v_subrev_nc_u32" => {
                let a = self.uargs(i, 2)?;
                let r = self.op2(OP_ISUB, Sc::U32, a[1], a[0]);
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            "v_and_b32" => self.ibin(i, OP_AND),
            "v_or_b32" => self.ibin(i, OP_OR),
            "v_xor_b32" => self.ibin(i, OP_XOR),
            "v_xnor_b32" => {
                let a = self.uargs(i, 2)?;
                let t = self.op2(OP_XOR, Sc::U32, a[0], a[1]);
                let r = self.op1(OP_NOT, Sc::U32, t);
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            "v_lshlrev_b32" | "v_lshrrev_b32" | "v_ashrrev_i32" => {
                // opérandes inversés : dst = src1 <op> (src0 & 31)
                let a = self.uargs(i, 2)?;
                let m31 = self.cu(31);
                let sh = self.op2(OP_AND, Sc::U32, a[0], m31);
                let r = match name {
                    "v_lshlrev_b32" => self.op2(OP_SHL, Sc::U32, a[1], sh),
                    "v_lshrrev_b32" => self.op2(OP_SHR, Sc::U32, a[1], sh),
                    _ => {
                        let x = self.spv.bitcast(a[1], Sc::I32);
                        self.spv.val(OP_SAR, Sc::I32, 1, &[x.id, sh.id])
                    }
                };
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            "v_mul_lo_u32" => self.ibin(i, OP_IMUL),
            "v_mul_hi_u32" | "v_mul_hi_i32" => {
                let a = self.uargs(i, 2)?;
                let r = self.mul_hi(a[0], a[1], name.ends_with("i32"));
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            "v_mul_u32_u24" | "v_mul_i32_i24" | "v_mad_u32_u24" | "v_mad_i32_i24" => {
                let signed = name.contains("i24");
                let mad = name.starts_with("v_mad");
                let a = self.uargs(i, if mad { 3 } else { 2 })?;
                let (z, w) = (self.cu(0), self.cu(24));
                let ext = |t: &mut Self, v: Val| {
                    if signed {
                        let vi = t.spv.bitcast(v, Sc::I32);
                        let r = t.spv.val(OP_BFSEXT, Sc::I32, 1, &[vi.id, z.id, w.id]);
                        t.spv.bitcast(r, Sc::U32)
                    } else {
                        t.spv.val(OP_BFUEXT, Sc::U32, 1, &[v.id, z.id, w.id])
                    }
                };
                let (x, y) = (ext(self, a[0]), ext(self, a[1]));
                let mut r = self.op2(OP_IMUL, Sc::U32, x, y);
                if mad {
                    r = self.op2(OP_IADD, Sc::U32, r, a[2]);
                }
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            "v_min_i32" => self.imin_max(i, G_SMIN, true),
            "v_max_i32" => self.imin_max(i, G_SMAX, true),
            "v_min_u32" => self.imin_max(i, G_UMIN, false),
            "v_max_u32" => self.imin_max(i, G_UMAX, false),
            "v_med3_i32" | "v_med3_u32" | "v_min3_i32" | "v_max3_i32" | "v_min3_u32"
            | "v_max3_u32" => {
                let signed = name.ends_with("i32");
                let sc = if signed { Sc::I32 } else { Sc::U32 };
                let a = self.uargs(i, 3)?;
                let v: Vec<Val> = a.iter().map(|&x| self.spv.bitcast(x, sc)).collect();
                let (mn, mx) = if signed {
                    (G_SMIN, G_SMAX)
                } else {
                    (G_UMIN, G_UMAX)
                };
                let r = if name.starts_with("v_med3") {
                    let (lo, hi) = (
                        self.glsl(sc, mn, &[v[0], v[1]]),
                        self.glsl(sc, mx, &[v[0], v[1]]),
                    );
                    let m = self.glsl(sc, mn, &[hi, v[2]]);
                    self.glsl(sc, mx, &[lo, m])
                } else {
                    let g = if name.starts_with("v_min3") { mn } else { mx };
                    let t = self.glsl(sc, g, &[v[0], v[1]]);
                    self.glsl(sc, g, &[t, v[2]])
                };
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            "v_bfe_u32" | "v_bfe_i32" => {
                let a = self.uargs(i, 3)?;
                let (m31, m1f) = (self.cu(31), self.cu(31));
                let off = self.op2(OP_AND, Sc::U32, a[1], m31);
                let w = self.op2(OP_AND, Sc::U32, a[2], m1f);
                let r = if name == "v_bfe_u32" {
                    self.spv
                        .val(OP_BFUEXT, Sc::U32, 1, &[a[0].id, off.id, w.id])
                } else {
                    let x = self.spv.bitcast(a[0], Sc::I32);
                    let r = self.spv.val(OP_BFSEXT, Sc::I32, 1, &[x.id, off.id, w.id]);
                    self.spv.bitcast(r, Sc::U32)
                };
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            "v_bfi_b32" => {
                // (a & b) | (~a & c)
                let a = self.uargs(i, 3)?;
                let t1 = self.op2(OP_AND, Sc::U32, a[0], a[1]);
                let na = self.op1(OP_NOT, Sc::U32, a[0]);
                let t2 = self.op2(OP_AND, Sc::U32, na, a[2]);
                let r = self.op2(OP_OR, Sc::U32, t1, t2);
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            "v_alignbit_b32" | "v_alignbyte_b32" => {
                // ({a,b} >> shift)[31:0]
                let a = self.uargs(i, 3)?;
                let (m31, c32) = (self.cu(31), self.cu(32));
                let mut sh = self.op2(OP_AND, Sc::U32, a[2], m31);
                if name == "v_alignbyte_b32" {
                    let (c3, c8) = (self.cu(3), self.cu(8));
                    let m = self.op2(OP_AND, Sc::U32, a[2], c3);
                    sh = self.op2(OP_IMUL, Sc::U32, m, c8);
                }
                let lo = self.op2(OP_SHR, Sc::U32, a[1], sh);
                let inv = self.op2(OP_ISUB, Sc::U32, c32, sh);
                let hi_raw = self.op2(OP_SHL, Sc::U32, a[0], inv);
                let zero = self.cu(0);
                let is0 = self.op2(OP_IEQ, Sc::Bool, sh, zero);
                let hi = self.select(is0, zero, hi_raw);
                let r = self.op2(OP_OR, Sc::U32, lo, hi);
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            "v_lshl_add_u32" | "v_lshl_or_b32" | "v_add3_u32" | "v_and_or_b32" | "v_or3_b32"
            | "v_xad_u32" => {
                let a = self.uargs(i, 3)?;
                let m31 = self.cu(31);
                let r = match name {
                    "v_lshl_add_u32" | "v_lshl_or_b32" => {
                        let sh = self.op2(OP_AND, Sc::U32, a[1], m31);
                        let t = self.op2(OP_SHL, Sc::U32, a[0], sh);
                        self.op2(
                            if name == "v_lshl_add_u32" {
                                OP_IADD
                            } else {
                                OP_OR
                            },
                            Sc::U32,
                            t,
                            a[2],
                        )
                    }
                    "v_add3_u32" => {
                        let t = self.op2(OP_IADD, Sc::U32, a[0], a[1]);
                        self.op2(OP_IADD, Sc::U32, t, a[2])
                    }
                    "v_and_or_b32" => {
                        let t = self.op2(OP_AND, Sc::U32, a[0], a[1]);
                        self.op2(OP_OR, Sc::U32, t, a[2])
                    }
                    "v_or3_b32" => {
                        let t = self.op2(OP_OR, Sc::U32, a[0], a[1]);
                        self.op2(OP_OR, Sc::U32, t, a[2])
                    }
                    _ => {
                        let t = self.op2(OP_XOR, Sc::U32, a[0], a[1]);
                        self.op2(OP_IADD, Sc::U32, t, a[2])
                    }
                };
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            "v_cndmask_b32" => {
                let s = i.srcs();
                if s.len() < 3 {
                    return unsupported(i);
                }
                let has_mods = s.iter().any(|o| o.neg || o.abs);
                let (a, b) = if has_mods {
                    let (x, y) = (self.src_f32(&s[0])?, self.src_f32(&s[1])?);
                    (x, y)
                } else {
                    (self.src_u32(&s[0])?, self.src_u32(&s[1])?)
                };
                let m = self.mask_lane(&s[2])?;
                let r = self.select(m, b, a);
                let d = *i
                    .dst()
                    .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                self.write_dst(&d, r)
            }
            "v_add_co_u32" | "v_sub_co_u32" | "v_subrev_co_u32" | "v_add_co_ci_u32"
            | "v_sub_co_ci_u32" | "v_subrev_co_ci_u32" => self.carry_op(i, name),
            _ => unsupported(i),
        }
    }

    fn carry_op(&mut self, i: &Instruction, name: &str) -> TResult<()> {
        let ops = &i.operands;
        if ops.len() < 4 {
            return unsupported(i);
        }
        let (d, d2) = (ops[0], ops[1]);
        let s = &ops[2..];
        let (mut a, mut b) = (self.src_u32(&s[0])?, self.src_u32(&s[1])?);
        let ci = name.contains("_ci_");
        let sub = name.contains("sub");
        if name.contains("subrev") {
            std::mem::swap(&mut a, &mut b);
        }
        let cin = if ci {
            let m = self.mask_lane(&s[2])?;
            let (one, zero) = (self.cu(1), self.cu(0));
            Some(self.select(m, one, zero))
        } else {
            None
        };
        let op = if sub { OP_ISUB } else { OP_IADD };
        let t = self.op2(op, Sc::U32, a, b);
        let (r, carry) = match cin {
            None => {
                let c = if sub {
                    self.op2(OP_ULT, Sc::Bool, a, b)
                } else {
                    self.op2(OP_ULT, Sc::Bool, t, a)
                };
                (t, c)
            }
            Some(cin) => {
                let r = self.op2(op, Sc::U32, t, cin);
                let (c1, c2) = if sub {
                    (
                        self.op2(OP_ULT, Sc::Bool, a, b),
                        self.op2(OP_ULT, Sc::Bool, t, cin),
                    )
                } else {
                    (
                        self.op2(OP_ULT, Sc::Bool, t, a),
                        self.op2(OP_ULT, Sc::Bool, r, t),
                    )
                };
                (r, self.bool_or(c1, c2))
            }
        };
        self.write_dst(&d, r)?;
        self.write_mask(&d2, carry)
    }

    /// `v_cmp_*` / `v_cmpx_*` (f32, i32, u32).
    fn vcmp(&mut self, i: &Instruction, name: &str) -> TResult<()> {
        let is_x = name.starts_with("v_cmpx_");
        let rest = name
            .trim_start_matches("v_cmpx_")
            .trim_start_matches("v_cmp_");
        let (rel, ty) = rest
            .rsplit_once('_')
            .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
        let s = i.srcs();
        if s.len() < 2 || rel == "class" {
            return unsupported(i);
        }
        let bit: Val = match ty {
            "f32" => {
                let (a, b) = (self.src_f32(&s[0])?, self.src_f32(&s[1])?);
                let op = match rel {
                    "lt" => 184,
                    "eq" => 180,
                    "le" => 188,
                    "gt" => 186,
                    "lg" => 182,
                    "ge" => 190,
                    "nge" => 185,
                    "nlg" => 181,
                    "ngt" => 189,
                    "nle" => 187,
                    "neq" => 183,
                    "nlt" => 191,
                    "o" | "u" => {
                        let (na, nb) = (self.op1(156, Sc::Bool, a), self.op1(156, Sc::Bool, b));
                        let any = self.bool_or(na, nb);
                        let r = if rel == "u" { any } else { self.bool_not(any) };
                        return self.vcmp_write(i, is_x, r);
                    }
                    "f" | "tru" => {
                        let v = Val {
                            id: self.spv.c_bool(rel == "tru"),
                            sc: Sc::Bool,
                            n: 1,
                        };
                        return self.vcmp_write(i, is_x, v);
                    }
                    _ => return unsupported(i),
                };
                self.op2(op, Sc::Bool, a, b)
            }
            "i32" | "u32" => {
                let signed = ty == "i32";
                let (a, b) = (self.src_u32(&s[0])?, self.src_u32(&s[1])?);
                if rel == "f" || rel == "t" {
                    Val {
                        id: self.spv.c_bool(rel == "t"),
                        sc: Sc::Bool,
                        n: 1,
                    }
                } else {
                    let op = Self::cmp_op(signed, rel)
                        .ok_or_else(|| TranslateError::Unsupported(name.into()))?;
                    let (a, b) = if signed {
                        (self.spv.bitcast(a, Sc::I32), self.spv.bitcast(b, Sc::I32))
                    } else {
                        (a, b)
                    };
                    self.op2(op, Sc::Bool, a, b)
                }
            }
            _ => return unsupported(i),
        };
        self.vcmp_write(i, is_x, bit)
    }

    fn vcmp_write(&mut self, i: &Instruction, is_x: bool, bit: Val) -> TResult<()> {
        if is_x {
            // v_cmpx : écrit EXEC.
            let exec = Operand::new(Src::ExecLo, self.wave.mask_dwords());
            return self.write_mask(&exec, bit);
        }
        let d = *i
            .dst()
            .ok_or_else(|| TranslateError::Unsupported(i.name.into()))?;
        self.write_mask(&d, bit)
    }
}
