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
    ("cs_global_mem", 32, """
        s_add_u32 s4, s2, 0x800
        s_addc_u32 s5, s3, 0
        v_lshlrev_b32 v1, 2, v0
        v_add_nc_u32 v3, 7, v0
        global_store_dword v1, v3, s[2:3]
        global_load_dword v4, v1, s[2:3]
        s_waitcnt vmcnt(0)
        v_add_nc_u32 v4, 1, v4
        global_store_dword v1, v4, s[2:3] offset:256
        v_add_nc_u32 v7, 0xab, v0
        global_store_byte v0, v7, s[2:3] offset:512
        s_waitcnt_vscnt null, 0x0
        global_load_ubyte v8, v0, s[2:3] offset:512
        global_load_sbyte v9, v0, s[2:3] offset:512
        s_waitcnt vmcnt(0)
        v_add_nc_u32 v8, v8, v9
        global_store_dword v1, v8, s[2:3] offset:1024
        v_mov_b32 v10, s2
        v_mov_b32 v11, s3
        v_add_co_u32 v10, vcc_lo, v10, v1
        v_add_co_ci_u32_e32 v11, vcc_lo, 0, v11, vcc_lo
        global_store_dword v[10:11], v3, off offset:1536
        v_mov_b32 v12, 0
        v_mov_b32 v13, 1
        global_atomic_add v9, v12, v13, s[2:3] offset:1800 glc
        s_waitcnt vmcnt(0)
        global_store_dword v1, v9, s[4:5]
        s_endpgm
    """),
    ("cs_bounds", 32, """
        s_load_dwordx4 s[4:7], s[0:1], 0x0
        s_load_dwordx4 s[8:11], s[0:1], 0x10
        s_waitcnt lgkmcnt(0)
        v_lshlrev_b32 v1, 2, v0
        v_add_nc_u32 v2, 1, v0
        buffer_store_dword v2, v1, s[4:7], 0 offen
        buffer_load_dword v3, v1, s[4:7], 0 offen
        s_buffer_load_dword s12, s[4:7], 0x7c
        s_buffer_load_dword s13, s[4:7], 0x80
        s_waitcnt vmcnt(0) lgkmcnt(0)
        buffer_store_dword v3, v1, s[8:11], 0 offen
        v_mov_b32 v4, s12
        v_mov_b32 v5, s13
        buffer_store_dword v4, v1, s[8:11], 0 offen offset:256
        buffer_store_dword v5, v1, s[8:11], 0 offen offset:512
        s_endpgm
    """),
    ("cs_buf_byte", 32, """
        s_load_dwordx4 s[4:7], s[0:1], 0x0
        s_waitcnt lgkmcnt(0)
        v_add_nc_u32 v2, 0x41, v0
        v_lshlrev_b32 v3, 1, v0
        buffer_store_byte v2, v0, s[4:7], 0 offen
        buffer_store_short v2, v3, s[4:7], 0 offen offset:256
        s_endpgm
    """),
    ("cs_table_x16", 32, """
        s_load_dwordx16 s[8:23], s[0:1], 0x0
        s_add_u32 s24, s0, 0x40
        s_addc_u32 s25, s1, 0
        s_load_dwordx16 s[32:47], s[24:25], 0x10
        s_load_dwordx4 s[48:51], s[24:25], 0x40
        s_waitcnt lgkmcnt(0)
        v_lshlrev_b32 v1, 2, v0
        buffer_load_dword v2, v1, s[12:15], 0 offen
        buffer_load_dword v3, v1, s[16:19], 0 offen
        buffer_load_dword v4, v1, s[20:23], 0 offen
        buffer_load_dword v5, v1, s[32:35], 0 offen
        buffer_load_dword v6, v1, s[36:39], 0 offen
        buffer_load_dword v7, v1, s[40:43], 0 offen
        buffer_load_dword v8, v1, s[44:47], 0 offen
        buffer_load_dword v9, v1, s[48:51], 0 offen
        s_waitcnt vmcnt(0)
        v_mad_u32_u24 v10, v3, 10, v2
        v_mad_u32_u24 v10, v4, 100, v10
        v_mad_u32_u24 v10, v5, 1000, v10
        v_mad_u32_u24 v10, v6, 10000, v10
        v_mad_u32_u24 v10, v7, 100000, v10
        v_mad_u32_u24 v10, v8, 1000000, v10
        v_add_nc_u32 v10, v9, v10
        buffer_store_dword v10, v1, s[8:11], 0 offen
        s_endpgm
    """),
    ("ps_table_views", 32, """
        s_load_dwordx4 s[16:19], s[0:1], 0x0
        s_load_dwordx16 s[20:35], s[0:1], 0x10
        s_add_u32 s40, s0, 0x50
        s_addc_u32 s41, s1, 0
        s_load_dwordx16 s[44:59], s[40:41], 0x0
        s_mov_b64 s[60:61], s[40:41]
        s_load_dwordx4 s[64:67], s[60:61], 0x40
        s_waitcnt lgkmcnt(0)
        v_interp_p1_f32 v2, v0, attr0.x
        v_interp_p1_f32 v3, v0, attr0.y
        v_interp_p1_f32 v4, v0, attr0.z
        image_sample v[8:11], v[2:3], s[20:27], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D
        image_sample v[12:15], v[2:4], s[28:35], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D_ARRAY
        image_sample v[24:27], v[2:4], s[44:51], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_3D
        image_sample v[28:31], v[2:4], s[52:59], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_CUBE
        buffer_load_format_xyzw v[32:35], v0, s[64:67], 0 idxen
        s_waitcnt vmcnt(0)
        v_add_f32 v8, v8, v12
        v_add_f32 v8, v8, v24
        v_add_f32 v8, v8, v28
        v_add_f32 v8, v8, v32
        exp mrt0 v8, v9, v10, v11 done vm
        s_endpgm
    """),
    ("err_dynamic_base", 32, """
        s_add_u32 s8, s0, s2
        s_addc_u32 s9, s1, 0
        s_load_dwordx4 s[16:19], s[8:9], 0x0
        s_waitcnt lgkmcnt(0)
        buffer_load_dword v1, v0, s[16:19], 0 offen
        s_endpgm
    """),
    ("err_dynamic_soffset", 32, """
        s_load_dwordx4 s[16:19], s[0:1], s2
        s_waitcnt lgkmcnt(0)
        buffer_load_dword v1, v0, s[16:19], 0 offen
        s_endpgm
    """),
    ("err_not_loaded", 32, """
        buffer_load_dword v1, v0, s[16:19], 0 offen
        s_endpgm
    """),
    ("err_split_descriptor", 32, """
        s_load_dwordx2 s[16:17], s[0:1], 0x0
        s_load_dwordx2 s[18:19], s[0:1], 0x20
        s_waitcnt lgkmcnt(0)
        buffer_load_dword v1, v0, s[16:19], 0 offen
        s_endpgm
    """),
    ("cs_infer_tables", 32, """
        s_mov_b64 s[10:11], s[4:5]
        s_load_dwordx16 s[16:31], s[10:11], 0x0
        s_load_dwordx4 s[32:35], s[6:7], 0x10
        s_waitcnt lgkmcnt(0)
        v_lshlrev_b32 v1, 2, v0
        buffer_load_dword v2, v1, s[20:23], 0 offen
        buffer_load_dword v3, v1, s[28:31], 0 offen
        s_waitcnt vmcnt(0)
        v_add_nc_u32 v4, v2, v3
        buffer_store_dword v4, v1, s[32:35], 0 offen
        s_endpgm
    """),
    ("err_ptr_used_as_table", 32, """
        s_load_dwordx16 s[8:23], s[2:3], 0x0
        s_waitcnt lgkmcnt(0)
        image_load v[4:7], v[0:1], s[8:15] dmask:0xf dim:SQ_RSRC_IMG_2D unorm
        s_waitcnt vmcnt(0)
        s_endpgm
    """),
    ("err_range_overlap", 32, """
        s_load_dwordx8 s[8:15], s[4:5], 0x0
        s_load_dwordx4 s[16:19], s[4:5], 0x10
        s_waitcnt lgkmcnt(0)
        image_load v[4:7], v[0:1], s[8:15] dmask:0xf dim:SQ_RSRC_IMG_2D unorm
        buffer_load_dword v2, v0, s[16:19], 0 offen
        s_endpgm
    """),
    ("cs_reuse_regs", 32, """
        s_load_dwordx8 s[16:23], s[4:5], 0x0
        s_load_dwordx4 s[12:15], s[4:5], 0x20
        s_waitcnt lgkmcnt(0)
        image_sample_lz v[4:7], v[0:1], s[16:23], s[12:15] dmask:0xf dim:SQ_RSRC_IMG_2D
        s_load_dwordx4 s[12:15], s[4:5], 0x30
        s_waitcnt lgkmcnt(0)
        s_buffer_load_dwordx2 s[24:25], s[12:15], 0x0
        s_waitcnt lgkmcnt(0) vmcnt(0)
        v_mov_b32 v8, s24
        buffer_store_dword v8, v0, s[12:15], 0 offen
        s_endpgm
    """),
    ("cs_store_2d", 32, """
        s_load_dwordx8 s[8:15], s[0:1], 0x0
        s_waitcnt lgkmcnt(0)
        v_and_b32 v1, 7, v0
        v_lshrrev_b32 v2, 3, v0
        v_mov_b32 v3, s2
        v_cvt_f32_u32 v4, v1
        v_cvt_f32_u32 v5, v2
        v_mov_b32 v6, 0.5
        v_mov_b32 v7, 1.0
        image_store v[4:7], v[1:2], s[8:15] dmask:0xf dim:SQ_RSRC_IMG_2D unorm
        s_endpgm
    """),
    ("cs_store_2darray", 32, """
        s_load_dwordx8 s[8:15], s[0:1], 0x0
        s_waitcnt lgkmcnt(0)
        v_and_b32 v1, 7, v0
        v_lshrrev_b32 v2, 3, v0
        v_mov_b32 v3, s2
        v_cvt_f32_u32 v4, v1
        v_cvt_f32_u32 v5, v2
        v_cvt_f32_u32 v6, v3
        v_mov_b32 v7, 1.0
        image_store v[4:7], v[1:3], s[8:15] dmask:0xf dim:SQ_RSRC_IMG_2D_ARRAY unorm
        s_endpgm
    """),
    ("cs_store_3d", 32, """
        s_load_dwordx8 s[8:15], s[0:1], 0x0
        s_waitcnt lgkmcnt(0)
        v_and_b32 v1, 7, v0
        v_lshrrev_b32 v2, 3, v0
        v_mov_b32 v3, s2
        v_cvt_f32_u32 v4, v1
        v_cvt_f32_u32 v5, v2
        v_cvt_f32_u32 v6, v3
        v_mov_b32 v7, 1.0
        image_store v[4:7], v[1:3], s[8:15] dmask:0xf dim:SQ_RSRC_IMG_3D unorm
        s_endpgm
    """),
    ("cs_store_u32_2d", 32, """
        s_load_dwordx8 s[8:15], s[0:1], 0x0
        s_waitcnt lgkmcnt(0)
        v_and_b32 v1, 7, v0
        v_lshrrev_b32 v2, 3, v0
        v_mov_b32 v3, s2
        v_lshl_add_u32 v4, v2, 3, v1
        v_mov_b32 v5, v4
        v_mov_b32 v6, v4
        v_mov_b32 v7, v4
        image_store v[4:7], v[1:2], s[8:15] dmask:0xf dim:SQ_RSRC_IMG_2D unorm
        s_endpgm
    """),
    ("cs_storage_rw_2d", 32, """
        s_load_dwordx8 s[8:15], s[0:1], 0x0
        s_waitcnt lgkmcnt(0)
        v_and_b32 v1, 7, v0
        v_lshrrev_b32 v2, 3, v0
        v_mov_b32 v3, s2
        image_load v[4:7], v[1:2], s[8:15] dmask:0xf dim:SQ_RSRC_IMG_2D unorm
        s_waitcnt vmcnt(0)
        v_add_f32 v4, 1.0, v4
        v_add_f32 v5, 2.0, v5
        image_store v[4:7], v[1:2], s[8:15] dmask:0xf dim:SQ_RSRC_IMG_2D unorm
        s_endpgm
    """),
    ("cs_storage_atomic_2d", 32, """
        s_load_dwordx8 s[8:15], s[0:1], 0x0
        s_waitcnt lgkmcnt(0)
        v_and_b32 v1, 7, v0
        v_lshrrev_b32 v2, 3, v0
        v_mov_b32 v3, s2
        v_mov_b32 v4, 1
        v_mov_b32 v1, 0
        v_mov_b32 v2, 0
        image_atomic_add v4, v[1:2], s[8:15] dmask:0x1 dim:SQ_RSRC_IMG_2D unorm
        s_endpgm
    """),
    ("cs_table_images", 32, """
        s_load_dwordx16 s[8:23], s[0:1], 0x0
        s_add_u32 s24, s0, 0x40
        s_addc_u32 s25, s1, 0
        s_load_dwordx8 s[32:39], s[24:25], 0x0
        s_load_dwordx16 s[40:55], s[24:25], 0x20
        s_waitcnt lgkmcnt(0)
        v_lshlrev_b32 v1, 2, v0
        v_and_b32 v2, 3, v0
        v_lshrrev_b32 v3, 2, v0
        v_and_b32 v3, 3, v3
        v_cvt_f32_u32 v10, v2
        v_add_f32 v10, 0.5, v10
        v_mul_f32 v10, 0.25, v10
        v_cvt_f32_u32 v11, v3
        v_add_f32 v11, 0.5, v11
        v_mul_f32 v11, 0.25, v11
        v_and_b32 v12, 1, v0
        v_lshrrev_b32 v13, 1, v0
        v_and_b32 v13, 1, v13
        image_sample_lz v[4:7], v[10:11], s[16:23], s[12:15] dmask:0xf dim:SQ_RSRC_IMG_2D
        image_sample_lz v[24:27], v[10:11], s[32:39], s[12:15] dmask:0xf dim:SQ_RSRC_IMG_2D
        v_cvt_f32_u32 v40, v12
        v_cvt_f32_u32 v41, v13
        v_mov_b32 v42, 0
        v_mov_b32 v43, 1.0
        image_store v[40:43], v[12:13], s[48:55] dmask:0xf dim:SQ_RSRC_IMG_2D unorm
        v_mov_b32 v44, 1
        v_mov_b32 v45, 0
        v_mov_b32 v46, 0
        image_atomic_add v44, v[45:46], s[40:47] dmask:0x1 dim:SQ_RSRC_IMG_2D unorm glc
        s_waitcnt vmcnt(0)
        buffer_store_dword v4, v1, s[8:11], 0 offen
        buffer_store_dword v25, v1, s[8:11], 0 offen offset:256
        buffer_store_dword v44, v1, s[8:11], 0 offen offset:512
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
    ("ps_mimg_ext", 32, """
        s_load_dwordx8 s[8:15], s[0:1], 0x0
        s_load_dwordx4 s[16:19], s[0:1], 0x20
        s_load_dwordx8 s[24:31], s[0:1], 0x30
        s_load_dwordx8 s[32:39], s[0:1], 0x50
        s_load_dwordx8 s[40:47], s[0:1], 0x70
        s_load_dwordx8 s[48:55], s[0:1], 0x90
        s_waitcnt lgkmcnt(0)
        v_interp_p1_f32 v2, v0, attr0.x
        v_interp_p1_f32 v3, v0, attr0.y
        v_interp_p1_f32 v4, v0, attr0.z
        v_interp_p1_f32 v5, v0, attr0.w
        v_interp_p1_f32 v6, v0, attr1.x
        v_interp_p1_f32 v7, v0, attr1.y
        v_interp_p1_f32 v8, v0, attr1.z
        image_sample_b v[20:23], v[2:4], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D
        image_sample_c v20, v[2:4], s[8:15], s[16:19] dmask:0x1 dim:SQ_RSRC_IMG_2D
        image_sample_cl v[20:23], v[2:4], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D
        image_sample_d_cl v[20:23], v[2:8], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D
        image_sample_o v[20:23], v[2:4], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D
        image_sample_c_l v20, v[2:5], s[8:15], s[16:19] dmask:0x1 dim:SQ_RSRC_IMG_2D
        image_gather4_c_o v[20:23], v[2:5], s[8:15], s[16:19] dmask:0x1 dim:SQ_RSRC_IMG_2D
        image_gather4 v[20:23], v[2:3], s[8:15], s[16:19] dmask:0x4 dim:SQ_RSRC_IMG_2D
        image_get_lod v[20:21], v[2:3], s[8:15], s[16:19] dmask:0x3 dim:SQ_RSRC_IMG_2D
        image_sample v[20:23], v2, s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D a16
        image_sample v[20:24], v[2:3], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D tfe
        image_sample v[20:22], v[2:3], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D tfe d16
        image_sample v[20:23], v[2:4], s[24:31], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_3D
        image_sample v[20:23], v[2:4], s[32:39], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_CUBE
        image_sample v[20:23], v[2:3], s[40:47], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_1D_ARRAY
        image_load v[20:23], v[2:4], s[48:55] dmask:0xf dim:SQ_RSRC_IMG_2D_MSAA unorm
        s_waitcnt vmcnt(0)
        exp mrt0 v20, v21, v22, v23 done vm
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



BATTERY2_PROLOGUE = BATTERY_PROLOGUE[:5] + [
    "v_lshlrev_b32 v1, 2, v0",
    "v_mul_lo_u32 v2, 0x9e3779b1, v0",
    "v_xor_b32 v3, 0x5bd1e995, v2",
    "v_add_nc_u32 v4, 0xffffffec, v0",
    "v_cvt_f32_i32 v5, v4",
    "v_mul_f32 v5, 0x3ebd70a4, v5",
    "v_cvt_f32_u32 v6, v0",
    "v_mul_f32 v6, 0x3de147ae, v6",
    "v_add_f32 v6, 1.0, v6",
    "v_and_b32 v10, 15, v0",
    "v_lshrrev_b32 v11, 2, v0",
    "v_and_b32 v11, 7, v11",
    "v_add_nc_u32 v11, 1, v11",
    "v_cvt_f16_u16 v12, v10",
    "v_cvt_f16_u16 v13, v11",
    "v_pack_b32_f16 v14, v12, v13",
    "v_pack_b32_f16 v15, v13, v12",
    "v_lshl_or_b32 v16, v11, 16, v10",
    "v_lshl_or_b32 v17, v10, 16, v11",
]
BATTERY2 = [
    ("v_add_f16", ["v_add_f16 v20, v12, v13"]),
    ("v_sub_f16", ["v_sub_f16 v20, v12, v13"]),
    ("v_mul_f16", ["v_mul_f16 v20, v12, v13"]),
    ("v_max_f16", ["v_max_f16 v20, v12, v13"]),
    ("v_min_f16", ["v_min_f16 v20, v12, v13"]),
    ("v_fma_f16", ["v_fma_f16 v20, v12, v13, v12"]),
    ("v_cvt_u16_f16", ["v_cvt_u16_f16 v20, v13"]),
    ("v_pk_add_f16", ["v_pk_add_f16 v20, v14, v15"]),
    ("v_pk_sub_f16_neg", ["v_pk_add_f16 v20, v14, v15 neg_lo:[0,1]"]),
    ("v_pk_mul_f16", ["v_pk_mul_f16 v20, v14, v15"]),
    ("v_pk_add_f16_opsel", ["v_pk_add_f16 v20, v14, v15 op_sel:[1,0] op_sel_hi:[1,1]"]),
    ("v_pk_add_u16", ["v_pk_add_u16 v20, v16, v17"]),
    ("v_pk_mul_lo_u16", ["v_pk_mul_lo_u16 v20, v16, v17"]),
    ("v_pk_sub_i16", ["v_pk_sub_i16 v20, v16, v17"]),
    ("v_pk_max_u16", ["v_pk_max_u16 v20, v16, v17"]),
    ("v_pk_lshlrev_b16", ["v_pk_lshlrev_b16 v20, v16, v17"]),
    ("v_pk_mad_u16", ["v_pk_mad_u16 v20, v16, v17, v16"]),
    ("v_dot2_f32_f16", ["v_dot2_f32_f16 v20, v14, v15, 1.0"]),
    ("v_dot2_u32_u16", ["v_dot2_u32_u16 v20, v16, v17, 5"]),
    ("v_dot4_u32_u8", ["v_dot4_u32_u8 v20, v2, v3, 7"]),
    ("v_dot4_i32_i8", ["v_dot4_i32_i8 v20, v2, v3, 7"]),
    ("v_fma_mix_f32", ["v_fma_mix_f32 v20, v14, v6, 1.0 op_sel_hi:[1,0,0]"]),
    ("v_add_nc_u16", ["v_add_nc_u16 v20, v10, v11"]),
    ("v_mul_lo_u16", ["v_mul_lo_u16 v20, v10, v11"]),
    ("v_max_i16", ["v_max_i16 v20, v10, v11"]),
    ("v_lshlrev_b16", ["v_lshlrev_b16 v20, v11, v10"]),
    ("v_mad_u16", ["v_mad_u16 v20, v10, v11, v10"]),
    ("v_cmp_class_neg_normal", ["v_cmp_class_f32 {VCC}, v5, 8", "v_cndmask_b32 v20, v2, v3, {VCC}"]),
    ("v_cmp_class_zero", ["v_cmp_class_f32 {VCC}, v5, 0x60", "v_cndmask_b32 v20, v2, v3, {VCC}"]),
    ("v_cmp_lt_f16", ["v_cmp_lt_f16 {VCC}, v12, v13", "v_cndmask_b32 v20, v2, v3, {VCC}"]),
    ("v_cmp_gt_u16", ["v_cmp_gt_u16 {VCC}, v10, v11", "v_cndmask_b32 v20, v2, v3, {VCC}"]),
    ("v_cmp_lt_i16", ["v_cmp_lt_i16 {VCC}, v11, v10", "v_cndmask_b32 v20, v2, v3, {VCC}"]),
    ("v_frexp_mant_f32", ["v_frexp_mant_f32 v20, v5"]),
    ("v_frexp_exp_i32_f32", ["v_frexp_exp_i32_f32 v20, v5"]),
    ("v_mad_u64_u32_lo", ["v_mov_b32 v22, v2", "v_mov_b32 v23, v3", "v_mad_u64_u32 v[20:21], {M8}, v2, v3, v[22:23]"]),
    ("v_mad_u64_u32_hi", ["v_mov_b32 v22, v2", "v_mov_b32 v23, v3", "v_mad_u64_u32 v[24:25], {M8}, v2, v3, v[22:23]", "v_mov_b32 v20, v25"]),
    ("v_mad_i64_i32_hi", ["v_mov_b32 v22, v2", "v_mov_b32 v23, v3", "v_mad_i64_i32 v[24:25], {M8}, v2, v3, v[22:23]", "v_mov_b32 v20, v25"]),
    ("v_perm_b32", ["v_perm_b32 v20, v2, v3, 0x0c070402"]),
    ("v_perm_b32_ff", ["v_perm_b32 v20, v2, v3, 0x0d050100"]),
    ("v_cvt_pk_u8_f32", ["v_mov_b32 v20, v2", "v_cvt_pk_u8_f32 v20, v6, 2, v20"]),
    ("v_mul_hi_u32_u24", ["v_mul_hi_u32_u24 v20, v2, v3"]),
    ("v_ffbl_b32", ["v_ffbl_b32 v20, v2"]),
    ("v_cvt_flr_i32_f32", ["v_cvt_flr_i32_f32 v20, v5"]),
    ("v_cvt_rpi_i32_f32", ["v_cvt_rpi_i32_f32 v20, v5"]),
    ("f64_fma_cvt", ["v_cvt_f64_i32 v[22:23], v4", "v_fma_f64 v[24:25], v[22:23], v[22:23], 1.0", "v_cvt_f32_f64 v20, v[24:25]"]),
    ("f64_sqrt", ["v_cvt_f64_u32 v[22:23], v0", "v_sqrt_f64 v[24:25], v[22:23]", "v_cvt_f32_f64 v20, v[24:25]"]),
    ("f64_floor_ldexp", ["v_cvt_f64_f32 v[22:23], v5", "v_floor_f64 v[24:25], v[22:23]", "v_ldexp_f64 v[24:25], v[24:25], 3", "v_cvt_f32_f64 v20, v[24:25]"]),
    ("v_cmp_lt_f64", ["v_cvt_f64_i32 v[22:23], v4", "v_cmp_lt_f64 {VCC}, v[22:23], 0", "v_cndmask_b32 v20, v2, v3, {VCC}"]),
    ("v_cmp_gt_u64", ["v_mov_b32 v22, v2", "v_mov_b32 v23, v3", "v_mov_b32 v24, v3", "v_mov_b32 v25, v2", "v_cmp_gt_u64 {VCC}, v[22:23], v[24:25]", "v_cndmask_b32 v20, v2, v3, {VCC}"]),
    ("v_cmp_lt_i64", ["v_mov_b32 v22, v2", "v_mov_b32 v23, v3", "v_mov_b32 v24, v3", "v_mov_b32 v25, v2", "v_cmp_lt_i64 {VCC}, v[22:23], v[24:25]", "v_cndmask_b32 v20, v2, v3, {VCC}"]),
]


BATTERY3_PROLOGUE = BATTERY_PROLOGUE[:5] + [
    "v_lshlrev_b32 v1, 2, v0",
    "v_add_nc_u32 v2, 100, v0",
    "v_mov_b32 v3, 0x11223344",
    "v_add_nc_u32 v4, 1024, v0",
    "v_add_nc_u32 v5, 0xa0, v0",
    "v_lshl_add_u32 v6, v0, 3, 2048",
    "v_mov_b32 v18, 3600",
    "v_mov_b32 v16, 1",
    "v_mov_b32 v7, 0",
    "ds_write_b32 v1, v2",
    "ds_write_b8 v4, v5",
    "ds_write2_b32 v6, v2, v3 offset0:0 offset1:1",
    "ds_write_b32 v18, v7",
    "ds_write_b32 v18, v7 offset:4",
    "s_waitcnt lgkmcnt(0)",
    "s_barrier",
]
BATTERY3 = [
    ("ds_read_u8", ["ds_read_u8 v20, v4", "s_waitcnt lgkmcnt(0)"]),
    ("ds_read_i8", ["ds_read_i8 v20, v4", "s_waitcnt lgkmcnt(0)"]),
    ("ds_read_u16", ["ds_read_u16 v20, v1", "s_waitcnt lgkmcnt(0)"]),
    ("ds_read2_b32_lo", ["ds_read2_b32 v[22:23], v6 offset0:0 offset1:1", "s_waitcnt lgkmcnt(0)", "v_mov_b32 v20, v22"]),
    ("ds_read2_b32_hi", ["ds_read2_b32 v[22:23], v6 offset0:0 offset1:1", "s_waitcnt lgkmcnt(0)", "v_mov_b32 v20, v23"]),
    ("ds_add_rtn_u32", ["ds_add_rtn_u32 v20, v18, v16", "s_waitcnt lgkmcnt(0)"]),
    ("ds_append", ["s_movk_i32 m0, 0xe14", "ds_append v20", "s_waitcnt lgkmcnt(0)"]),
    ("ds_bpermute_b32", ["v_and_b32 v19, {W1}, v0", "v_sub_nc_u32 v19, {W1}, v19", "v_lshlrev_b32 v19, 2, v19", "ds_bpermute_b32 v20, v19, v2", "s_waitcnt lgkmcnt(0)"]),
    ("ds_permute_b32", ["v_add_nc_u32 v19, 1, v0", "v_and_b32 v19, {W1}, v19", "v_lshlrev_b32 v19, 2, v19", "ds_permute_b32 v20, v19, v2", "s_waitcnt lgkmcnt(0)"]),
    ("ds_swizzle_xor1", ["ds_swizzle_b32 v20, v2 offset:0x41f", "s_waitcnt lgkmcnt(0)"]),
    ("ds_swizzle_quad", ["ds_swizzle_b32 v20, v2 offset:0x801b", "s_waitcnt lgkmcnt(0)"]),
    ("v_mbcnt_lane", ["v_mbcnt_lo_u32_b32 v20, -1, 0", "v_mbcnt_hi_u32_b32 v20, -1, v20"]),
    ("v_writelane_b32", ["v_mov_b32 v20, 0", "v_writelane_b32 v20, 77, 5"]),
    ("s_wqm_b32", ["s_mov_b32 s9, 0x00010204", "s_wqm_b32 s9, s9", "v_mov_b32 v20, s9"]),
    ("s_lshl_b64_lo", ["s_mov_b32 s10, 0x89abcdef", "s_mov_b32 s11, 0x01234567", "s_lshl_b64 s[12:13], s[10:11], 36", "v_mov_b32 v20, s12"]),
    ("s_lshl_b64_hi", ["s_mov_b32 s10, 0x89abcdef", "s_mov_b32 s11, 0x01234567", "s_lshl_b64 s[12:13], s[10:11], 36", "v_mov_b32 v20, s13"]),
    ("s_lshr_b64_lo", ["s_mov_b32 s10, 0x89abcdef", "s_mov_b32 s11, 0x01234567", "s_lshr_b64 s[12:13], s[10:11], 4", "v_mov_b32 v20, s12"]),
    ("s_ashr_i64_hi", ["s_mov_b32 s10, 0x89abcdef", "s_mov_b32 s11, 0x81234567", "s_ashr_i64 s[12:13], s[10:11], 40", "v_mov_b32 v20, s13"]),
    ("s_ashr_i64_lo", ["s_mov_b32 s10, 0x89abcdef", "s_mov_b32 s11, 0x81234567", "s_ashr_i64 s[12:13], s[10:11], 40", "v_mov_b32 v20, s12"]),
    ("s_bitset1_b32", ["s_mov_b32 s9, 0x10", "s_bitset1_b32 s9, 7", "v_mov_b32 v20, s9"]),
    ("s_ff1_i32_b64", ["s_mov_b32 s10, 0", "s_mov_b32 s11, 0x00400000", "s_ff1_i32_b64 s9, s[10:11]", "v_mov_b32 v20, s9"]),
    ("v_lshlrev_b64_hi", ["v_lshlrev_b64 v[22:23], 36, v[2:3]", "v_mov_b32 v20, v23"]),
    ("v_lshrrev_b64_lo", ["v_lshrrev_b64 v[22:23], 36, v[2:3]", "v_mov_b32 v20, v22"]),
    ("v_ashrrev_i64_hi", ["v_ashrrev_i64 v[22:23], 4, v[2:3]", "v_mov_b32 v20, v23"]),
    ("v_bfm_b32", ["v_bfm_b32 v20, 7, 9"]),
    ("v_xor3_b32", ["v_xor3_b32 v20, v2, v3, v0"]),
]


BATTERY4_PROLOGUE = BATTERY_PROLOGUE[:1] + [
    "s_load_dwordx8 s[8:15], s[0:1], 0x20",
    "s_load_dwordx4 s[16:19], s[0:1], 0x40",
    "s_load_dwordx8 s[24:31], s[0:1], 0x60",
    "s_load_dwordx8 s[32:39], s[0:1], 0x80",
    "s_mov_b32 s20, 0",
    "s_waitcnt lgkmcnt(0)",
    "v_lshlrev_b32 v1, 2, v0",
    "v_and_b32 v2, 3, v0",
    "v_lshrrev_b32 v3, 2, v0",
    "v_and_b32 v3, 3, v3",
    "v_cvt_f32_u32 v10, v2",
    "v_add_f32 v10, 0.5, v10",
    "v_mul_f32 v10, 0.25, v10",
    "v_cvt_f32_u32 v11, v3",
    "v_add_f32 v11, 0.5, v11",
    "v_mul_f32 v11, 0.25, v11",
    "v_mov_b32 v9, 1",
    "v_and_b32 v12, 1, v0",
    "v_lshrrev_b32 v13, 1, v0",
    "v_and_b32 v13, 1, v13",
    "v_mov_b32 v14, 0",
]
BATTERY4 = [
    ("sample_lz_r", ["image_sample_lz v[4:7], v[10:11], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D", "s_waitcnt vmcnt(0)", "v_mov_b32 v20, v4"]),
    ("sample_lz_g", ["image_sample_lz v[4:7], v[10:11], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D", "s_waitcnt vmcnt(0)", "v_mov_b32 v20, v5"]),
    ("sample_l", ["v_mov_b32 v15, 0", "image_sample_l v[4:7], v[10:12], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D", "s_waitcnt vmcnt(0)", "v_mov_b32 v20, v6"]),
    ("sample_lz_o_r", ["image_sample_lz_o v[4:7], v[9:11], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D", "s_waitcnt vmcnt(0)", "v_mov_b32 v20, v4"]),
    ("sample_d_r", ["v_mov_b32 v21, 0", "v_mov_b32 v22, 0", "v_mov_b32 v23, 0", "v_mov_b32 v24, 0", "v_mov_b32 v25, v10", "v_mov_b32 v26, v11", "image_sample_d v[4:7], v[21:26], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D", "s_waitcnt vmcnt(0)", "v_mov_b32 v20, v5"]),
    ("sample_d16_r", ["image_sample_lz v4, v[10:11], s[8:15], s[16:19] dmask:0x1 dim:SQ_RSRC_IMG_2D d16", "s_waitcnt vmcnt(0)", "v_cvt_f32_f16 v20, v4"]),
    ("gather4_r_w", ["image_gather4 v[4:7], v[10:11], s[8:15], s[16:19] dmask:0x1 dim:SQ_RSRC_IMG_2D", "s_waitcnt vmcnt(0)", "v_mov_b32 v20, v4"]),
    ("gather4_r_x", ["image_gather4 v[4:7], v[10:11], s[8:15], s[16:19] dmask:0x1 dim:SQ_RSRC_IMG_2D", "s_waitcnt vmcnt(0)", "v_mov_b32 v20, v7"]),
    ("gather4_g_z", ["image_gather4 v[4:7], v[10:11], s[8:15], s[16:19] dmask:0x2 dim:SQ_RSRC_IMG_2D", "s_waitcnt vmcnt(0)", "v_mov_b32 v20, v5"]),
    ("load_r", ["image_load v[4:7], v[12:13], s[8:15] dmask:0xf dim:SQ_RSRC_IMG_2D unorm", "s_waitcnt vmcnt(0)", "v_mov_b32 v20, v4"]),
    ("load_tfe_status", ["v_mov_b32 v8, 0x55", "image_load v[4:8], v[12:13], s[8:15] dmask:0xf dim:SQ_RSRC_IMG_2D unorm tfe", "s_waitcnt vmcnt(0)", "v_mov_b32 v20, v8"]),
    ("resinfo_wh", ["image_get_resinfo v[4:5], v14, s[8:15] dmask:0x3 dim:SQ_RSRC_IMG_2D unorm", "s_waitcnt vmcnt(0)", "v_lshl_add_u32 v20, v5, 8, v4"]),
    ("resinfo_levels", ["image_get_resinfo v4, v14, s[8:15] dmask:0x8 dim:SQ_RSRC_IMG_2D unorm", "s_waitcnt vmcnt(0)", "v_mov_b32 v20, v4"]),
    ("store_rgba8", ["v_cvt_f32_u32 v40, v12", "v_cvt_f32_u32 v41, v13", "v_mov_b32 v42, 0", "v_mov_b32 v43, 1.0", "image_store v[40:43], v[12:13], s[32:39] dmask:0xf dim:SQ_RSRC_IMG_2D unorm", "s_waitcnt_vscnt null, 0x0", "v_mov_b32 v20, 0"]),
    ("atomic_add_ret", ["v_mov_b32 v44, 1", "v_mov_b32 v45, 0", "v_mov_b32 v46, 0", "image_atomic_add v44, v[45:46], s[24:31] dmask:0x1 dim:SQ_RSRC_IMG_2D unorm glc", "s_waitcnt vmcnt(0)", "v_mov_b32 v20, v44"]),
    ("atomic_umax", ["v_mov_b32 v47, v0", "v_mov_b32 v48, 1", "v_mov_b32 v49, 0", "image_atomic_umax v47, v[48:49], s[24:31] dmask:0x1 dim:SQ_RSRC_IMG_2D unorm", "s_waitcnt_vscnt null, 0x0", "v_mov_b32 v20, 0"]),
]


BATTERY5_PROLOGUE = BATTERY_PROLOGUE[:5] + [
    "v_lshlrev_b32 v1, 2, v0",
    "v_add_nc_u32 v2, 100, v0",
    "v_mul_lo_u32 v3, 0x01010101, v0",
    "v_add_nc_u32 v3, 0x12345678, v3",
]
def _dpp(name, line):
    return (name, ["v_mov_b32 v20, 0xdead", line])
BATTERY5 = [
    _dpp("dpp_quad_perm", "v_mov_b32_dpp v20, v2 quad_perm:[1,0,3,2] row_mask:0xf bank_mask:0xf"),
    _dpp("dpp_row_shl3_bc", "v_mov_b32_dpp v20, v2 row_shl:3 row_mask:0xf bank_mask:0xf bound_ctrl:1"),
    _dpp("dpp_row_shl3", "v_mov_b32_dpp v20, v2 row_shl:3 row_mask:0xf bank_mask:0xf"),
    _dpp("dpp_row_shr1", "v_mov_b32_dpp v20, v2 row_shr:1 row_mask:0xf bank_mask:0xf"),
    _dpp("dpp_row_ror4", "v_mov_b32_dpp v20, v2 row_ror:4 row_mask:0xf bank_mask:0xf"),
    _dpp("dpp_row_mirror", "v_mov_b32_dpp v20, v2 row_mirror row_mask:0xf bank_mask:0xf"),
    _dpp("dpp_row_half_mirror", "v_mov_b32_dpp v20, v2 row_half_mirror row_mask:0xf bank_mask:0xf"),
    _dpp("dpp_row_share5", "v_mov_b32_dpp v20, v2 row_share:5 row_mask:0xf bank_mask:0xf"),
    _dpp("dpp_row_xmask3", "v_mov_b32_dpp v20, v2 row_xmask:3 row_mask:0xf bank_mask:0xf"),
    _dpp("dpp_masked", "v_mov_b32_dpp v20, v2 quad_perm:[3,2,1,0] row_mask:0x5 bank_mask:0xa"),
    _dpp("dpp8_reverse", "v_mov_b32_dpp v20, v2 dpp8:[7,6,5,4,3,2,1,0]"),
    _dpp("dpp_add", "v_add_nc_u32_dpp v20, v2, v3 quad_perm:[0,0,0,0] row_mask:0xf bank_mask:0xf"),
    _dpp("sdwa_add_sel", "v_add_nc_u32_sdwa v20, v2, v3 dst_sel:DWORD dst_unused:UNUSED_PAD src0_sel:BYTE_1 src1_sel:WORD_0"),
    _dpp("sdwa_dst_byte1", "v_mov_b32_sdwa v20, v3 dst_sel:BYTE_1 dst_unused:UNUSED_PAD src0_sel:BYTE_2"),
    _dpp("sdwa_preserve", "v_mov_b32_sdwa v20, v3 dst_sel:WORD_1 dst_unused:UNUSED_PRESERVE src0_sel:WORD_0"),
    _dpp("sdwa_sext", "v_add_nc_u32_sdwa v20, sext(v3), v2 dst_sel:DWORD dst_unused:UNUSED_PAD src0_sel:BYTE_3 src1_sel:DWORD"),
]

def battery_asm(wave, prologue=None, battery=None):
    sub = {"{VCC}": "vcc_lo" if wave == 32 else "vcc",
           "{M8}": "s8" if wave == 32 else "s[8:9]",
           "{M10}": "s10" if wave == 32 else "s[10:11]",
           "{W1}": "31" if wave == 32 else "63"}
    lines = list(prologue or BATTERY_PROLOGUE)
    for name, body in (battery or BATTERY):
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
    progs = PROGRAMS + [("cs_alu_battery_w32", 32, battery_asm(32)), ("cs_alu_battery_w64", 64, battery_asm(64)),
             ("cs_ext_battery_w32", 32, battery_asm(32, BATTERY2_PROLOGUE, BATTERY2)),
             ("cs_ext_battery_w64", 64, battery_asm(64, BATTERY2_PROLOGUE, BATTERY2)),
             ("cs_ds_battery_w32", 32, battery_asm(32, BATTERY3_PROLOGUE, BATTERY3)),
             ("cs_ds_battery_w64", 64, battery_asm(64, BATTERY3_PROLOGUE, BATTERY3)),
             ("cs_mimg_battery", 32, battery_asm(32, BATTERY4_PROLOGUE, BATTERY4)),
             ("cs_dpp_battery_w32", 32, battery_asm(32, BATTERY5_PROLOGUE, BATTERY5)),
             ("cs_dpp_battery_w64", 64, battery_asm(64, BATTERY5_PROLOGUE, BATTERY5))]
    for name, wave, src in progs:
        words = assemble(src, wave)
        asm = "\\n".join(l.strip() for l in src.strip().splitlines())
        out.append('    Program { name: "%s", wave: %d, asm: "%s", words: &[%s] },' % (name, wave, asm, ", ".join("0x%08x" % w for w in words)))
    out.append("];")
    print("\n".join(out))
    sys.stderr.write("%d programmes\n" % len(progs))


main()
