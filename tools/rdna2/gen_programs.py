#!/usr/bin/env python3
"""Génère `crates/sb-gpu/src/rdna2/test_programs.rs` : programmes RDNA 2 écrits à la main.

Les sources asm (ci-dessous) sont assemblées avec `llvm-mc -filetype=obj` (les étiquettes sont
résolues par LLVM) puis la section .text est extraite avec `llvm-objcopy`. Les tests Rust
n'utilisent que les dwords figés.

Usage : python3 -I tools/rdna2/gen_programs.py > crates/sb-gpu/src/rdna2/test_programs.rs
"""
import os
import struct
import subprocess
import sys
import tempfile

PROGRAMS = [
    ("vs_passthrough", 32, """
        s_load_dwordx4 s[4:7], s[0:1], 0x0
        s_waitcnt lgkmcnt(0)
        v_lshlrev_b32 v1, 4, v0
        buffer_load_dwordx4 v[4:7], v1, s[4:7], 0 offen
        s_waitcnt vmcnt(0)
        exp param0 v4, v5, v6, v7
        exp pos0 v4, v5, v6, v7 done
        s_endpgm
    """),
    ("vs_uv_passthrough", 32, """
        s_load_dwordx4 s[4:7], s[0:1], 0x0
        s_waitcnt lgkmcnt(0)
        v_lshlrev_b32 v1, 5, v0
        buffer_load_dwordx4 v[4:7], v1, s[4:7], 0 offen
        buffer_load_dwordx4 v[8:11], v1, s[4:7], 0 offen offset:16
        s_waitcnt vmcnt(0)
        exp param0 v8, v9, v10, v11
        exp pos0 v4, v5, v6, v7 done
        s_endpgm
    """),
    ("vs_constants_sbuffer", 32, """
        s_buffer_load_dwordx4 s[8:11], s[4:7], 0x10
        s_waitcnt lgkmcnt(0)
        v_mov_b32 v1, s8
        v_mov_b32 v2, s9
        v_mov_b32 v3, s10
        v_mov_b32 v4, s11
        exp pos0 v1, v2, v3, v4 done
        s_endpgm
    """),
    ("ps_const_color", 32, """
        v_mov_b32 v0, 1.0
        v_mov_b32 v1, 0.5
        v_mov_b32 v2, 0
        v_mov_b32 v3, 1.0
        exp mrt0 v0, v1, v2, v3 done vm
        s_endpgm
    """),
    ("ps_texture", 32, """
        s_load_dwordx8 s[8:15], s[0:1], 0x0
        s_load_dwordx4 s[16:19], s[0:1], 0x20
        s_waitcnt lgkmcnt(0)
        v_interp_p1_f32 v2, v0, attr0.x
        v_interp_p1_f32 v3, v0, attr0.y
        v_interp_p2_f32 v2, v1, attr0.x
        v_interp_p2_f32 v3, v1, attr0.y
        image_sample v[4:7], v[2:3], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D
        s_waitcnt vmcnt(0)
        v_cvt_pkrtz_f16_f32 v4, v4, v5
        v_cvt_pkrtz_f16_f32 v5, v6, v7
        exp mrt0 v4, v4, v5, v5 done compr vm
        s_endpgm
    """),
    ("ps_kill", 32, """
        v_interp_p1_f32 v2, v0, attr0.x
        v_interp_p2_f32 v2, v1, attr0.x
        v_cmpx_gt_f32 0.5, v2
        v_mov_b32 v3, 1.0
        exp mrt0 v3, v3, v3, v3 done vm
        s_endpgm
    """),
    ("cs_write_buffer", 32, """
        s_load_dwordx4 s[4:7], s[0:1], 0x0
        s_lshl_b32 s3, s2, 6
        s_waitcnt lgkmcnt(0)
        v_add_nc_u32 v0, s3, v0
        v_lshlrev_b32 v1, 2, v0
        v_cvt_f32_u32 v2, v0
        v_mul_f32 v2, 2.0, v2
        buffer_store_dword v2, v1, s[4:7], 0 offen
        s_endpgm
    """),
    ("cs_divergent_w32", 32, """
        s_load_dwordx4 s[4:7], s[0:1], 0x0
        s_waitcnt lgkmcnt(0)
        v_lshlrev_b32 v1, 2, v0
        v_cmp_gt_u32 vcc_lo, 16, v0
        s_and_saveexec_b32 s8, vcc_lo
        s_cbranch_execz skip
        v_mov_b32 v2, 7
        buffer_store_dword v2, v1, s[4:7], 0 offen
    skip:
        s_or_b32 exec_lo, exec_lo, s8
        v_mov_b32 v3, 9
        buffer_store_dword v3, v1, s[4:7], 0 offen offset:256
        s_endpgm
    """),
    ("cs_divergent_w64", 64, """
        s_load_dwordx4 s[4:7], s[0:1], 0x0
        s_waitcnt lgkmcnt(0)
        v_lshlrev_b32 v1, 2, v0
        v_cmp_gt_u32 vcc, 16, v0
        s_and_saveexec_b64 s[8:9], vcc
        s_cbranch_execz skip
        v_mov_b32 v2, 7
        buffer_store_dword v2, v1, s[4:7], 0 offen
    skip:
        s_or_b64 exec, exec, s[8:9]
        v_mov_b32 v3, 9
        buffer_store_dword v3, v1, s[4:7], 0 offen offset:256
        s_endpgm
    """),
    ("cs_scalar_loop", 32, """
        s_load_dwordx4 s[4:7], s[0:1], 0x0
        s_mov_b32 s8, 0
        s_mov_b32 s9, 0
    loop:
        s_add_u32 s9, s9, s8
        s_add_u32 s8, s8, 1
        s_cmp_lt_u32 s8, 10
        s_cbranch_scc1 loop
        s_waitcnt lgkmcnt(0)
        v_mov_b32 v1, s9
        v_lshlrev_b32 v2, 2, v0
        buffer_store_dword v1, v2, s[4:7], 0 offen
        s_endpgm
    """),
    ("cs_divergent_loop", 32, """
        s_load_dwordx4 s[4:7], s[0:1], 0x0
        v_and_b32 v1, 3, v0
        v_mov_b32 v2, 0
        v_mov_b32 v3, 0
        s_mov_b32 s10, exec_lo
    loop:
        v_add_nc_u32 v2, 10, v2
        v_add_nc_u32 v3, 1, v3
        v_cmp_lt_u32 vcc_lo, v3, v1
        s_and_b32 exec_lo, exec_lo, vcc_lo
        s_cbranch_execnz loop
        s_mov_b32 exec_lo, s10
        s_waitcnt lgkmcnt(0)
        v_lshlrev_b32 v4, 2, v0
        buffer_store_dword v2, v4, s[4:7], 0 offen
        s_endpgm
    """),
    ("cs_lds_barrier", 32, """
        s_load_dwordx4 s[4:7], s[0:1], 0x0
        v_lshlrev_b32 v1, 2, v0
        v_add_nc_u32 v2, 100, v0
        ds_write_b32 v1, v2
        s_waitcnt lgkmcnt(0)
        s_barrier
        v_sub_nc_u32 v5, 63, v0
        v_lshlrev_b32 v5, 2, v5
        ds_read_b32 v3, v5
        s_waitcnt lgkmcnt(0)
        buffer_store_dword v3, v1, s[4:7], 0 offen
        s_endpgm
    """),
    ("cs_valu_mods", 32, """
        s_load_dwordx4 s[4:7], s[0:1], 0x0
        s_waitcnt lgkmcnt(0)
        v_cvt_f32_u32 v1, v0
        v_fma_f32 v2, -v1, |v1|, 1.0 clamp mul:2
        v_cmp_lt_f32 vcc_lo, 0.5, v2
        v_cndmask_b32_e64 v3, 0, 1.0, vcc_lo
        v_add_co_u32 v4, s9, v0, v0
        v_add_co_ci_u32_e64 v5, s10, 0, 0, s9
        v_mad_u32_u24 v6, v0, 3, 5
        v_bfe_u32 v7, v0, 1, 3
        v_min_f32 v8, v2, v3
        v_sqrt_f32 v9, v1
        v_lshlrev_b32 v10, 4, v0
        buffer_store_dwordx4 v[2:5], v10, s[4:7], 0 offen
        buffer_store_dwordx4 v[6:9], v10, s[4:7], 0 offen offset:1024
        s_endpgm
    """),
    ("vs_tbuffer_fetch", 32, """
        s_load_dwordx4 s[4:7], s[0:1], 0x0
        s_waitcnt lgkmcnt(0)
        tbuffer_load_format_xyzw v[4:7], v0, s[4:7], 0 format:[BUF_FMT_32_32_32_32_FLOAT] idxen
        s_waitcnt vmcnt(0)
        exp pos0 v4, v5, v6, v7 done
        s_endpgm
    """),
    ("cs_atomic_add", 32, """
        s_load_dwordx4 s[4:7], s[0:1], 0x0
        s_waitcnt lgkmcnt(0)
        v_mov_b32 v1, 1
        buffer_atomic_add v1, off, s[4:7], 0 glc
        v_lshlrev_b32 v2, 2, v0
        buffer_store_dword v1, v2, s[4:7], 0 offen offset:4
        s_endpgm
    """),
]


# ---------------------------------------------------------------------------------------------
# Batterie ALU : chaque segment calcule un résultat dans v20 (ou v20 via SGPR) puis le range à
# l'offset 256*k de la sortie. Entrées par lane : v0 = tid, v2 = hash, v3 = hash ^ cst, v4 = tid-20,
# v5 = (tid-20)*0.37, v6 = 1 + tid*0.11. Les oracles CPU vivent dans tests_gpu.rs (même nom).
# Les marqueurs {VCC}, {M8}, {M10} sont remplacés selon la taille de vague.
BATTERY_PROLOGUE = [
    "s_load_dwordx4 s[4:7], s[0:1], 0x0",
    "s_mov_b32 s20, 0",
    "s_mov_b32 s30, 0xfffffff0",
    "s_mov_b32 s31, 0x123",
    "s_waitcnt lgkmcnt(0)",
    "v_lshlrev_b32 v1, 2, v0",
    "v_mul_lo_u32 v2, 0x9e3779b1, v0",
    "v_xor_b32 v3, 0x5bd1e995, v2",
    "v_add_nc_u32 v4, 0xffffffec, v0",
    "v_cvt_f32_i32 v5, v4",
    "v_mul_f32 v5, 0x3ebd70a4, v5",
    "v_cvt_f32_u32 v6, v0",
    "v_mul_f32 v6, 0x3de147ae, v6",
    "v_add_f32 v6, 1.0, v6",
]
BATTERY = [
    ("v_add_nc_u32", ["v_add_nc_u32 v20, v2, v3"]),
    ("v_sub_nc_u32", ["v_sub_nc_u32 v20, v2, v3"]),
    ("v_subrev_nc_u32", ["v_subrev_nc_u32 v20, v2, v3"]),
    ("v_and_b32", ["v_and_b32 v20, v2, v3"]),
    ("v_or_b32", ["v_or_b32 v20, v2, v3"]),
    ("v_xor_b32", ["v_xor_b32 v20, v2, v3"]),
    ("v_xnor_b32", ["v_xnor_b32 v20, v2, v3"]),
    ("v_lshlrev_b32", ["v_lshlrev_b32 v20, v3, v2"]),
    ("v_lshrrev_b32", ["v_lshrrev_b32 v20, v3, v2"]),
    ("v_ashrrev_i32", ["v_ashrrev_i32 v20, v3, v2"]),
    ("v_mul_u32_u24", ["v_mul_u32_u24 v20, v2, v3"]),
    ("v_mul_i32_i24", ["v_mul_i32_i24 v20, v2, v3"]),
    ("v_min_i32", ["v_min_i32 v20, v2, v3"]),
    ("v_max_i32", ["v_max_i32 v20, v2, v3"]),
    ("v_min_u32", ["v_min_u32 v20, v2, v3"]),
    ("v_max_u32", ["v_max_u32 v20, v2, v3"]),
    ("v_mul_hi_u32", ["v_mul_hi_u32 v20, v2, v3"]),
    ("v_mul_hi_i32", ["v_mul_hi_i32 v20, v2, v3"]),
    ("v_mul_lo_u32", ["v_mul_lo_u32 v20, v2, v3"]),
    ("v_alignbit_b32", ["v_alignbit_b32 v20, v2, v3, v4"]),
    ("v_bfi_b32", ["v_bfi_b32 v20, v2, v3, v4"]),
    ("v_med3_i32", ["v_med3_i32 v20, v2, v3, v4"]),
    ("v_lshl_add_u32", ["v_lshl_add_u32 v20, v2, v4, v3"]),
    ("v_not_b32", ["v_not_b32 v20, v2"]),
    ("v_bfrev_b32", ["v_bfrev_b32 v20, v2"]),
    ("v_ffbh_u32", ["v_ffbh_u32 v20, v2"]),
    ("v_bfe_u32", ["v_bfe_u32 v20, v2, 7, 9"]),
    ("v_bfe_i32", ["v_bfe_i32 v20, v2, 3, 13"]),
    ("v_add3_u32", ["v_add3_u32 v20, v2, v3, v4"]),
    ("v_and_or_b32", ["v_and_or_b32 v20, v2, v3, v4"]),
    ("v_mad_u32_u24", ["v_mad_u32_u24 v20, v2, v3, v4"]),
    ("v_mad_i32_i24", ["v_mad_i32_i24 v20, v2, v3, v4"]),
    ("v_add_f32", ["v_add_f32 v20, 0.5, v5"]),
    ("v_sub_f32", ["v_sub_f32 v20, v5, v6"]),
    ("v_mul_f32", ["v_mul_f32 v20, v5, v6"]),
    ("v_min_f32", ["v_min_f32 v20, v5, v6"]),
    ("v_max_f32", ["v_max_f32 v20, v5, v6"]),
    ("v_fma_f32", ["v_fma_f32 v20, v5, v6, v5"]),
    ("v_floor_f32", ["v_floor_f32 v20, v5"]),
    ("v_ceil_f32", ["v_ceil_f32 v20, v5"]),
    ("v_trunc_f32", ["v_trunc_f32 v20, v5"]),
    ("v_fract_f32", ["v_fract_f32 v20, v5"]),
    ("v_rndne_f32", ["v_rndne_f32 v20, v5"]),
    ("v_cvt_i32_f32", ["v_cvt_i32_f32 v20, v5"]),
    ("v_cvt_u32_f32", ["v_cvt_u32_f32 v20, v6"]),
    ("v_rcp_f32", ["v_rcp_f32 v20, v6"]),
    ("v_rsq_f32", ["v_rsq_f32 v20, v6"]),
    ("v_sqrt_f32", ["v_sqrt_f32 v20, v6"]),
    ("v_exp_f32", ["v_exp_f32 v20, v5"]),
    ("v_log_f32", ["v_log_f32 v20, v6"]),
    ("v_sin_f32", ["v_sin_f32 v20, v6"]),
    ("v_cvt_f16_f32_roundtrip", ["v_cvt_f16_f32 v20, v5", "v_cvt_f32_f16 v20, v20"]),
    ("v_cvt_pkrtz_f16_f32", ["v_cvt_pkrtz_f16_f32 v20, v5, v6"]),
    ("v_cndmask_lt_i32", ["v_cmp_lt_i32 {VCC}, v4, 0", "v_cndmask_b32 v20, v2, v3, {VCC}"]),
    ("v_cndmask_gt_u32", ["v_cmp_gt_u32 {VCC}, v2, v3", "v_cndmask_b32 v20, v2, v3, {VCC}"]),
    ("v_cndmask_ge_f32", ["v_cmp_ge_f32 {VCC}, v5, v6", "v_cndmask_b32 v20, v2, v3, {VCC}"]),
    ("v_add_co_u32_sum", ["v_add_co_u32 v20, {M8}, v2, v3"]),
    ("v_add_co_u32_carry", ["v_add_co_u32 v21, {M8}, v2, v3", "v_cndmask_b32_e64 v20, 0, 1, {M8}"]),
    ("v_sub_co_ci_u32", ["v_add_co_u32 v21, {M8}, v2, v3", "v_sub_co_ci_u32_e64 v20, {M10}, v3, v2, {M8}"]),
    ("s_add_u32", ["s_add_u32 s40, s30, s31", "v_mov_b32 v20, s40"]),
    ("s_add_u32_scc", ["s_add_u32 s40, s30, s31", "s_cselect_b32 s41, 1, 0", "v_mov_b32 v20, s41"]),
    ("s_sub_u32_scc", ["s_sub_u32 s40, s31, s30", "s_cselect_b32 s41, 0x100, 7", "v_mov_b32 v20, s41"]),
    ("s_mul_i32", ["s_mul_i32 s40, s30, s31", "v_mov_b32 v20, s40"]),
    ("s_bfe_u32", ["s_bfe_u32 s40, s30, 0x80004", "v_mov_b32 v20, s40"]),
    ("s_lshl_b32", ["s_lshl_b32 s40, s31, 5", "v_mov_b32 v20, s40"]),
    ("s_ashr_i32", ["s_ashr_i32 s40, s30, 2", "v_mov_b32 v20, s40"]),
    ("s_min_i32", ["s_min_i32 s40, s30, s31", "v_mov_b32 v20, s40"]),
    ("s_max_u32", ["s_max_u32 s40, s30, s31", "v_mov_b32 v20, s40"]),
    ("s_cmp_lt_i32_cselect", ["s_cmp_lt_i32 s30, s31", "s_cselect_b32 s40, 11, 22", "v_mov_b32 v20, s40"]),
    ("s_addc_u32", ["s_add_u32 s40, s30, s30", "s_addc_u32 s41, s31, 5", "v_mov_b32 v20, s41"]),
    ("s_bcnt1_i32_b32", ["s_bcnt1_i32_b32 s40, s30", "v_mov_b32 v20, s40"]),
    ("s_ff1_i32_b32", ["s_ff1_i32_b32 s40, s30", "v_mov_b32 v20, s40"]),
    ("s_flbit_i32_b32", ["s_flbit_i32_b32 s40, s31", "v_mov_b32 v20, s40"]),
    ("s_brev_b32", ["s_brev_b32 s40, s31", "v_mov_b32 v20, s40"]),
    ("s_abs_i32", ["s_abs_i32 s40, s30", "v_mov_b32 v20, s40"]),
    ("s_sext_i32_i16", ["s_mov_b32 s42, 0x1fff8", "s_sext_i32_i16 s40, s42", "v_mov_b32 v20, s40"]),
    ("s_pack_ll_b32_b16", ["s_pack_ll_b32_b16 s40, s30, s31", "v_mov_b32 v20, s40"]),
    ("s_absdiff_i32", ["s_absdiff_i32 s40, s30, s31", "v_mov_b32 v20, s40"]),
    ("s_bfm_b32", ["s_bfm_b32 s40, 7, 9", "v_mov_b32 v20, s40"]),
    ("s_mul_hi_u32", ["s_mul_hi_u32 s40, s30, s30", "v_mov_b32 v20, s40"]),
    ("s_andn2_b32", ["s_andn2_b32 s40, s30, s31", "v_mov_b32 v20, s40"]),
    ("s_xnor_b32", ["s_xnor_b32 s40, s30, s31", "v_mov_b32 v20, s40"]),
    ("s_movk_addk_mulk", ["s_movk_i32 s40, 0xfff0", "s_addk_i32 s40, 0x20", "s_mulk_i32 s40, 7", "v_mov_b32 v20, s40"]),
    ("v_readfirstlane_b32", ["v_readfirstlane_b32 s40, v3", "v_mov_b32 v20, s40"]),
]


def battery_asm(wave):
    sub = {"{VCC}": "vcc_lo" if wave == 32 else "vcc",
           "{M8}": "s8" if wave == 32 else "s[8:9]",
           "{M10}": "s10" if wave == 32 else "s[10:11]"}
    lines = list(BATTERY_PROLOGUE)
    for name, body in BATTERY:
        lines.append("; seg " + name)
        for l in body:
            for k, v in sub.items():
                l = l.replace(k, v)
            lines.append(l)
        lines.append("buffer_store_dword v20, v1, s[4:7], s20 offen")
        lines.append("s_add_u32 s20, s20, 0x100")
    lines.append("s_endpgm")
    return "\n".join(lines)


def assemble(src, wave):
    attr = "-mattr=+wavefrontsize%d,-wavefrontsize%d" % (wave, 96 - wave)
    with tempfile.TemporaryDirectory() as d:
        s, o, b = (os.path.join(d, n) for n in ("p.s", "p.o", "p.bin"))
        open(s, "w").write("\n".join(l.strip() for l in src.strip().splitlines()) + "\n")
        subprocess.run(["llvm-mc", "-triple=amdgcn", "-mcpu=gfx1030", attr, "-filetype=obj", s, "-o", o], check=True)
        subprocess.run(["llvm-objcopy", "-O", "binary", "--only-section=.text", o, b], check=True)
        data = open(b, "rb").read()
    return list(struct.unpack("<%dI" % (len(data) // 4), data))


def main():
    out = ["//! Programmes RDNA 2 de test — GÉNÉRÉ par tools/rdna2/gen_programs.py (llvm-mc). Ne pas éditer.\n",
           "/// Programme : nom, taille de vague, source asm (documentation) et dwords.",
           "pub struct Program {", "    pub name: &'static str,", "    pub wave: u8,", "    pub asm: &'static str,", "    pub words: &'static [u32],", "}\n",
           "pub static PROGRAMS: &[Program] = &["]
    progs = PROGRAMS + [("cs_alu_battery_w32", 32, battery_asm(32)), ("cs_alu_battery_w64", 64, battery_asm(64))]
    for name, wave, src in progs:
        words = assemble(src, wave)
        asm = "\\n".join(l.strip() for l in src.strip().splitlines())
        out.append('    Program { name: "%s", wave: %d, asm: "%s", words: &[%s] },' % (name, wave, asm, ", ".join("0x%08x" % w for w in words)))
    out.append("];")
    print("\n".join(out))
    sys.stderr.write("%d programmes\n" % len(progs))


main()
