#!/usr/bin/env python3
"""Génère `crates/sb-gpu/src/rdna2/test_vectors.rs` : encodages de référence issus de llvm-mc.

Chaque ligne asm ci-dessous est assemblée par `llvm-mc -mcpu=gfx1030` (wave32 par défaut,
`w64:` pour wave64) ; le texte canonique et les dwords sont figés dans le fichier Rust.
Les tests Rust n'appellent jamais llvm-mc.

Usage : python3 -I tools/rdna2/gen_vectors.py > crates/sb-gpu/src/rdna2/test_vectors.rs
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from llvm_dis import asm, dis  # noqa: E402

SRC = """
# --- SOP2
s_add_u32 s0, s1, s2
s_sub_u32 s3, s4, 5
s_addc_u32 s0, s0, 0
s_min_i32 s1, s2, s3
s_max_u32 s1, s2, 0x12345678
s_cselect_b32 s0, s1, s2
s_cselect_b64 s[0:1], s[2:3], 0
s_and_b32 s0, s1, s2
s_and_b64 s[2:3], vcc, exec
s_or_b32 s0, vcc_lo, exec_lo
s_xor_b64 s[0:1], s[2:3], s[4:5]
s_andn2_b64 exec, exec, s[4:5]
s_lshl_b32 s0, s1, 4
s_lshr_b32 s0, s1, 4
s_ashr_i32 s0, s1, 31
s_mul_i32 s0, s1, s2
s_bfe_u32 s0, s1, 0x80008
s_lshl_b64 s[0:1], s[2:3], 3
s_absdiff_i32 s0, s1, s2
s_pack_ll_b32_b16 s0, s1, s2
# --- SOP1
s_mov_b32 s0, s1
s_mov_b64 s[0:1], s[2:3]
s_mov_b32 s0, 0x40490fdb
s_mov_b32 s0, -1
s_mov_b32 m0, s0
s_mov_b32 s0, m0
s_mov_b64 s[0:1], exec
s_mov_b32 exec_lo, -1
s_not_b32 s0, s1
s_brev_b32 s0, s1
s_bcnt1_i32_b32 s0, s1
s_ff1_i32_b32 s0, s1
s_flbit_i32_b32 s0, s1
s_sext_i32_i8 s0, s1
s_and_saveexec_b32 s0, vcc_lo
w64: s_and_saveexec_b64 s[0:1], vcc
w64: s_or_saveexec_b64 s[2:3], s[4:5]
s_getpc_b64 s[0:1]
s_setpc_b64 s[2:3]
s_swappc_b64 s[30:31], s[4:5]
s_abs_i32 s0, s1
s_wqm_b32 s0, s1
s_bitset0_b32 s0, s1
# --- SOPK
s_movk_i32 s0, 0x1234
s_addk_i32 s0, 0x10
s_mulk_i32 s0, 0x20
s_cmpk_eq_i32 s0, 0x10
s_cmpk_lt_u32 s1, 0x10
s_waitcnt_vscnt null, 0x0
# --- SOPC
s_cmp_eq_u32 s0, s1
s_cmp_lg_i32 s0, 0
s_cmp_ge_u32 s0, 0x10
s_bitcmp1_b32 s0, 5
s_cmp_eq_u64 s[0:1], s[2:3]
# --- SOPP
s_endpgm
s_nop 0
s_nop 3
s_waitcnt vmcnt(0)
s_waitcnt lgkmcnt(0)
s_waitcnt vmcnt(0) lgkmcnt(0)
s_branch 5
s_cbranch_scc0 3
s_cbranch_scc1 7
s_cbranch_vccz 1
s_cbranch_vccnz 2
s_cbranch_execz 4
s_cbranch_execnz 6
s_barrier
s_sleep 1
s_setprio 3
s_inst_prefetch 1
s_clause 3
# --- SMEM
s_load_dword s0, s[2:3], 0x10
s_load_dwordx2 s[4:5], s[2:3], 0x0
s_load_dwordx4 s[4:7], s[2:3], 0x0
s_load_dwordx8 s[8:15], s[0:1], s5
s_load_dwordx16 s[16:31], s[0:1], 0x40
s_buffer_load_dword s0, s[4:7], 0x20
s_buffer_load_dwordx4 s[8:11], s[4:7], 0x30
s_buffer_load_dwordx2 s[8:9], s[4:7], 0x8
s_memtime s[0:1]
s_dcache_inv
# --- VOP1
v_mov_b32 v0, v1
v_mov_b32 v0, 1.0
v_mov_b32 v0, 0x12345678
v_mov_b32 v0, s5
v_cvt_f32_i32 v0, v1
v_cvt_f32_u32 v0, v1
v_cvt_i32_f32 v0, v1
v_cvt_u32_f32 v0, v1
v_rcp_f32 v0, v1
v_rsq_f32 v0, v1
v_sqrt_f32 v0, v1
v_exp_f32 v0, v1
v_log_f32 v0, v1
v_sin_f32 v0, v1
v_cos_f32 v0, v1
v_floor_f32 v0, v1
v_ceil_f32 v0, v1
v_trunc_f32 v0, v1
v_fract_f32 v0, v1
v_rndne_f32 v0, v1
v_not_b32 v0, v1
v_bfrev_b32 v0, v1
v_readfirstlane_b32 s0, v1
v_cvt_f16_f32 v0, v1
v_cvt_f32_f16 v0, v1
v_cvt_f64_f32 v[0:1], v2
v_nop
v_ffbh_u32 v0, v1
v_cvt_f32_ubyte0 v0, v1
# --- VOP2
v_add_f32 v0, v1, v2
v_sub_f32 v0, v1, v2
v_mul_f32 v0, 2.0, v1
v_min_f32 v0, v1, v2
v_max_f32 v0, v1, v2
v_add_nc_u32 v0, v1, v2
v_sub_nc_u32 v0, 1, v2
v_and_b32 v0, 0xff, v2
v_or_b32 v0, v1, v2
v_xor_b32 v0, v1, v2
v_lshlrev_b32 v0, 2, v1
v_lshrrev_b32 v0, 2, v1
v_ashrrev_i32 v0, 31, v1
v_mul_u32_u24 v0, v1, v2
v_mul_i32_i24 v0, v1, v2
v_min_i32 v0, v1, v2
v_max_u32 v0, v1, v2
v_cndmask_b32 v0, v1, v2, vcc_lo
v_fmac_f32 v0, v1, v2
v_fmamk_f32 v0, v1, 0x40490fdb, v2
v_fmaak_f32 v0, v1, v2, 0x40490fdb
v_add_co_ci_u32 v0, vcc_lo, v1, v2, vcc_lo
v_cvt_pkrtz_f16_f32 v0, v1, v2
v_add_f32 v0, 0x40490fdb, v1
# --- VOPC
v_cmp_lt_f32 vcc_lo, v0, v1
v_cmp_eq_u32 vcc_lo, 0, v1
v_cmp_gt_i32 vcc_lo, v0, v1
v_cmp_ge_f32 vcc_lo, 0.5, v1
v_cmp_neq_f32 vcc_lo, v0, v1
v_cmpx_lt_f32 v0, v1
v_cmpx_eq_u32 v0, v1
v_cmp_class_f32 vcc_lo, v0, v1
v_cmp_ne_u32 vcc_lo, v0, v1
w64: v_cmp_lt_f32 vcc, v0, v1
w64: v_cndmask_b32 v0, v1, v2, vcc
w64: v_cmp_gt_u32 s[0:1], v0, v1
w64: v_cmpx_lt_f32 v0, v1
# --- VOP3
v_cubeid_f32 v0, v1, v2, v3
v_fma_f32 v0, v1, v2, v3
v_mad_u32_u24 v0, v1, v2, v3
v_mad_i32_i24 v0, v1, v2, v3
v_med3_f32 v0, v1, v2, v3
v_med3_i32 v0, v1, v2, v3
v_bfe_u32 v0, v1, 8, 8
v_bfi_b32 v0, v1, v2, v3
v_alignbit_b32 v0, v1, v2, 8
v_min3_f32 v0, v1, v2, v3
v_max3_f32 v0, v1, v2, v3
v_mul_lo_u32 v0, v1, v2
v_mul_hi_u32 v0, v1, v2
v_lshl_add_u32 v0, v1, 2, v3
v_add3_u32 v0, v1, v2, v3
v_and_or_b32 v0, v1, v2, v3
v_lshl_or_b32 v0, v1, 2, v3
v_or3_b32 v0, v1, v2, v3
v_cndmask_b32_e64 v0, v1, v2, s0
v_cmp_lt_f32_e64 s0, v0, v1
v_add_f32_e64 v0, -v1, |v2| clamp mul:2
v_add_co_u32 v0, s0, v1, v2
v_sub_co_u32 v0, s0, v1, v2
v_add_co_ci_u32_e64 v0, s0, v1, v2, s1
v_fma_f32 v0, -v1, v2, |v3|
v_fma_f32 v0, 0x40490fdb, v1, v2
v_div_scale_f32 v0, vcc_lo, v1, v2, v3
v_div_fmas_f32 v0, v1, v2, v3
v_div_fixup_f32 v0, v1, v2, v3
v_readlane_b32 s0, v1, 5
v_writelane_b32 v0, s1, 5
v_perm_b32 v0, v1, v2, v3
v_mov_b32_e64 v0, v1
v_ldexp_f32 v0, v1, v2
v_mad_u64_u32 v[0:1], s0, v2, v3, v[4:5]
v_add_f64 v[0:1], v[2:3], v[4:5]
v_fma_f64 v[0:1], v[2:3], v[4:5], v[6:7]
v_mul_f64 v[0:1], v[2:3], 2.0
v_fma_f16 v0, v1, v2, v3 op_sel:[1,0,0,0]
w64: v_cmp_lt_f32_e64 s[0:1], v0, v1
w64: v_cndmask_b32_e64 v0, v1, v2, s[2:3]
w64: v_add_co_u32 v0, s[2:3], v1, v2
# --- VOP3P
v_pk_add_f16 v0, v1, v2
v_pk_mul_f16 v0, v1, v2
v_pk_fma_f16 v0, v1, v2, v3 op_sel_hi:[1,1,0]
v_pk_add_u16 v0, v1, v2 neg_lo:[1,0]
v_fma_mix_f32 v0, v1, v2, v3
v_dot2_f32_f16 v0, v1, v2, v3
# --- DS
ds_read_b32 v0, v1 offset:16
ds_write_b32 v1, v2 offset:4
ds_read_b64 v[0:1], v2
ds_write_b128 v0, v[4:7]
ds_read2_b32 v[0:1], v2 offset0:1 offset1:2
ds_write2_b32 v1, v2, v3 offset0:4 offset1:8
ds_add_u32 v0, v1
ds_add_rtn_u32 v0, v1, v2
ds_max_i32 v0, v1
ds_permute_b32 v0, v1, v2
ds_bpermute_b32 v0, v1, v2
ds_read_u8 v0, v1
ds_write_b8 v0, v1
ds_cmpst_b32 v0, v1, v2
ds_read_b128 v[0:3], v4
# --- MUBUF
buffer_load_dword v0, v1, s[4:7], 0 offen
buffer_load_dword v0, off, s[4:7], 0 offset:16
buffer_load_dwordx4 v[0:3], v1, s[4:7], s8 idxen offset:4 glc
buffer_store_dword v0, v1, s[4:7], 0 offen
buffer_store_dwordx4 v[0:3], off, s[8:11], 0
buffer_load_format_xyzw v[0:3], v1, s[4:7], 0 idxen
buffer_load_ubyte v0, v1, s[4:7], 0 offen
buffer_load_sbyte v0, v1, s[4:7], 0 offen
buffer_load_ushort v0, v1, s[4:7], 0 offen
buffer_store_byte v0, v1, s[4:7], 0 offen
buffer_store_short v0, v1, s[4:7], 0 offen
buffer_atomic_add v0, v1, s[4:7], 0 offen glc
buffer_atomic_cmpswap v[0:1], v2, s[4:7], 0 offen glc
buffer_gl0_inv
buffer_load_dwordx2 v[0:1], v2, s[4:7], 0 offen slc dlc
buffer_load_dword v0, v[1:2], s[4:7], 0 idxen offen
# --- MTBUF
tbuffer_load_format_xyzw v[0:3], v1, s[4:7], 0 format:[BUF_FMT_32_32_32_32_FLOAT] idxen
tbuffer_load_format_x v0, v1, s[4:7], 0 format:[BUF_FMT_32_FLOAT] offen
tbuffer_store_format_xy v[0:1], v2, s[4:7], 0 format:[BUF_FMT_16_16_FLOAT] offen
# --- MIMG
image_sample v[0:3], v[4:5], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D
image_sample_l v[0:3], v[4:6], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D
image_sample_b v[0:3], v[4:6], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D
image_sample_c v[0:3], v[4:6], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D
image_sample_lz v[0:3], v[4:5], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D
image_sample v0, v[4:6], s[8:15], s[16:19] dmask:0x1 dim:SQ_RSRC_IMG_3D
image_sample v[0:3], v4, s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_1D
image_sample_d v[0:3], v[4:9], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D
image_sample v[0:2], v[4:6], s[8:15], s[16:19] dmask:0x7 dim:SQ_RSRC_IMG_CUBE
image_load v[0:3], v[4:5], s[8:15] dmask:0xf dim:SQ_RSRC_IMG_2D unorm
image_load_mip v[0:3], v[4:6], s[8:15] dmask:0xf dim:SQ_RSRC_IMG_2D unorm
image_store v[0:3], v[4:5], s[8:15] dmask:0xf dim:SQ_RSRC_IMG_2D unorm
image_get_resinfo v[0:3], v4, s[8:15] dmask:0xf dim:SQ_RSRC_IMG_2D unorm
image_gather4 v[0:3], v[4:5], s[8:15], s[16:19] dmask:0x1 dim:SQ_RSRC_IMG_2D
image_atomic_add v0, v[4:5], s[8:15] dmask:0x1 dim:SQ_RSRC_IMG_2D unorm glc
image_sample v[0:3], [v4, v6], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D
# --- EXP
exp pos0 v0, v1, v2, v3 done
exp param0 v0, v1, v2, v3
exp param5 v4, v5, v6, v7
exp mrt0 v0, v1, v2, v3 done vm
exp mrt0 v0, off, v1, off
exp mrtz v0, off, off, off
exp mrt0 v0, v0, v1, v1 done compr vm
exp null off, off, off, off done vm
# --- DPP / SDWA
v_mov_b32_dpp v0, v1 quad_perm:[1,0,3,2] row_mask:0xa bank_mask:0x5 bound_ctrl:1
v_add_f32_dpp v0, -v1, |v2| row_shl:3 row_mask:0xf bank_mask:0xf
v_mov_b32_dpp v0, v1 row_share:5 row_mask:0xf bank_mask:0xf fi:1
v_mov_b32_dpp v0, v1 row_xmask:3 row_mask:0xf bank_mask:0xf
v_mov_b32_dpp v0, v1 row_mirror row_mask:0xf bank_mask:0xf
v_mov_b32_dpp v0, v1 dpp8:[7,6,5,4,3,2,1,0]
v_add_f32_sdwa v0, -v1, |v2| clamp dst_sel:WORD_1 dst_unused:UNUSED_PRESERVE src0_sel:BYTE_2 src1_sel:DWORD
v_mov_b32_sdwa v0, v1 dst_sel:BYTE_0 dst_unused:UNUSED_PAD src0_sel:BYTE_1
v_cmp_eq_u32_sdwa vcc_lo, v1, v2 src0_sel:BYTE_0 src1_sel:DWORD
v_add_nc_u32_sdwa v0, sext(v1), v2 dst_sel:DWORD dst_unused:UNUSED_PAD src0_sel:WORD_1 src1_sel:BYTE_0
# --- VINTRP
v_interp_p1_f32 v0, v1, attr0.x
v_interp_p2_f32 v0, v1, attr0.x
v_interp_mov_f32 v0, p0, attr1.y
# --- FLAT / GLOBAL / SCRATCH
flat_load_dword v0, v[2:3]
flat_store_dword v[2:3], v1
flat_atomic_add v0, v[2:3], v1 glc
global_load_dword v0, v[2:3], off
global_load_dword v0, v2, s[4:5] offset:8
global_store_dword v[2:3], v1, off
global_store_dwordx4 v2, v[4:7], s[0:1]
global_atomic_add v0, v[2:3], v1, off glc
global_load_ubyte v0, v[2:3], off
global_load_dwordx4 v[0:3], v[4:5], off offset:-16
scratch_load_dword v0, v1, off
scratch_store_dword v1, v2, off
"""

def main():
    items = []
    for raw in SRC.splitlines():
        raw = raw.strip()
        if not raw or raw.startswith("#"):
            continue
        wave = 32
        if raw.startswith("w64:"):
            wave, raw = 64, raw[4:].strip()
        items.append((wave, raw))
    out = ["//! Vecteurs de test RDNA 2 — GÉNÉRÉ par tools/rdna2/gen_vectors.py (llvm-mc, gfx1030). Ne pas éditer.\n",
           "/// Un vecteur : taille de vague, texte canonique LLVM, dwords encodés.",
           "pub struct Vector {", "    pub wave: u8,", "    pub text: &'static str,", "    pub words: &'static [u32],", "}\n",
           "pub static VECTORS: &[Vector] = &["]
    bad = 0
    for wave, line in items:
        r = asm([line], wave)[0]
        if r is None:
            sys.stderr.write("non assemblé : %s\n" % line)
            bad += 1
            continue
        text, words = r
        back = dis([words], wave)[0]
        if back is None:
            sys.stderr.write("non redésassemblable : %s\n" % line)
            continue
        # la forme canonique de référence est celle du désassembleur LLVM
        text = " ".join(back.split())
        out.append('    Vector { wave: %d, text: "%s", words: &[%s] },' % (wave, text.replace('"', '\\"'), ", ".join("0x%08x" % w for w in words)))
    out.append("];")
    print("\n".join(out))
    sys.stderr.write("%d vecteurs, %d non assemblés\n" % (len(items) - bad, bad))

main()
