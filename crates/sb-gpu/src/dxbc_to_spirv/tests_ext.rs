//! Tests unitaires par famille d'opcodes (programmes DXBC synthétiques assemblés à la main),
//! chacun validé par `spirv-val --target-env vulkan1.3` lorsqu'il est installé.

use super::opcodes::*;
use super::*;
use std::io::Write;
use std::process::{Command, Stdio};

fn validate(bytes: &[u8]) {
    let Ok(mut child) = Command::new("spirv-val")
        .args(["--target-env", "vulkan1.3", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(bytes);
    }
    let out = child.wait_with_output().expect("spirv-val");
    assert!(
        out.status.success(),
        "spirv-val a rejeté le module :\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn swz(a: u32, b: u32, c: u32, d: u32) -> u32 {
    a | (b << 2) | (c << 4) | (d << 6)
}
const XYZW: u32 = 0xE4;

fn src(ty: u32, idx: &[u32], sw: u32) -> Vec<u32> {
    let mut v = vec![2 | (1 << 2) | (sw << 4) | (ty << 12) | ((idx.len() as u32) << 20)];
    v.extend_from_slice(idx);
    v
}
fn sel(ty: u32, idx: &[u32], comp: u32) -> Vec<u32> {
    let mut v = vec![2 | (2 << 2) | (comp << 4) | (ty << 12) | ((idx.len() as u32) << 20)];
    v.extend_from_slice(idx);
    v
}
fn dst(ty: u32, idx: &[u32], mask: u32) -> Vec<u32> {
    let mut v = vec![2 | (mask << 4) | (ty << 12) | ((idx.len() as u32) << 20)];
    v.extend_from_slice(idx);
    v
}
fn r(i: u32) -> Vec<u32> {
    src(OPERAND_TEMP, &[i], XYZW)
}
fn rs(i: u32, c: u32) -> Vec<u32> {
    sel(OPERAND_TEMP, &[i], c)
}
fn rd(i: u32, mask: u32) -> Vec<u32> {
    dst(OPERAND_TEMP, &[i], mask)
}
fn od(i: u32, mask: u32) -> Vec<u32> {
    dst(OPERAND_OUTPUT, &[i], mask)
}
fn v(i: u32) -> Vec<u32> {
    src(OPERAND_INPUT, &[i], XYZW)
}
fn imm(a: [u32; 4]) -> Vec<u32> {
    let mut v = vec![2 | (OPERAND_IMMEDIATE32 << 12)];
    v.extend_from_slice(&a);
    v
}
fn imm1(a: u32) -> Vec<u32> {
    vec![1 | (OPERAND_IMMEDIATE32 << 12), a]
}
fn f(x: f32) -> u32 {
    x.to_bits()
}
fn tres(slot: u32, sw: u32) -> Vec<u32> {
    src(OPERAND_RESOURCE, &[slot], sw)
}
fn samp(slot: u32) -> Vec<u32> {
    vec![(OPERAND_SAMPLER << 12) | (1 << 20), slot]
}
fn samp_c(slot: u32, comp: u32) -> Vec<u32> {
    sel(OPERAND_SAMPLER, &[slot], comp)
}
fn uav_s(slot: u32, sw: u32) -> Vec<u32> {
    src(OPERAND_UNORDERED_ACCESS_VIEW, &[slot], sw)
}
fn uav_d(slot: u32, mask: u32) -> Vec<u32> {
    dst(OPERAND_UNORDERED_ACCESS_VIEW, &[slot], mask)
}
fn g_d(slot: u32, mask: u32) -> Vec<u32> {
    dst(OPERAND_THREAD_GROUP_SHARED_MEMORY, &[slot], mask)
}
fn g_s(slot: u32) -> Vec<u32> {
    src(OPERAND_THREAD_GROUP_SHARED_MEMORY, &[slot], XYZW)
}

fn inst(opcode: u32, extra_bits: u32, ops: &[Vec<u32>]) -> Vec<u32> {
    let body: Vec<u32> = ops.iter().flatten().copied().collect();
    let mut w = vec![(((1 + body.len()) as u32) << 24) | extra_bits | opcode];
    w.extend(body);
    w
}
fn i0(opcode: u32) -> Vec<u32> {
    inst(opcode, 0, &[])
}
const NZ: u32 = 1 << 18;
const SAT: u32 = 1 << 13;

fn decl_temps(n: u32) -> Vec<u32> {
    vec![(2 << 24) | OP_DCL_TEMPS, n]
}
fn decl_res(slot: u32, dim: u32) -> Vec<u32> {
    vec![
        (4 << 24) | (dim << 11) | OP_DCL_RESOURCE,
        (OPERAND_RESOURCE << 12) | (1 << 20),
        slot,
        0x5555,
    ]
}
fn decl_samp(slot: u32, mode: u32) -> Vec<u32> {
    vec![
        (3 << 24) | (mode << 11) | OP_DCL_SAMPLER,
        (OPERAND_SAMPLER << 12) | (1 << 20),
        slot,
    ]
}
fn decl_uav_typed(slot: u32, dim: u32) -> Vec<u32> {
    vec![
        (4 << 24) | (dim << 11) | OP_DCL_UAV_TYPED,
        (OPERAND_UNORDERED_ACCESS_VIEW << 12) | (1 << 20),
        slot,
        0x4444,
    ]
}
fn decl_uav_raw(slot: u32) -> Vec<u32> {
    vec![
        (3 << 24) | OP_DCL_UAV_RAW,
        (OPERAND_UNORDERED_ACCESS_VIEW << 12) | (1 << 20),
        slot,
    ]
}
fn decl_uav_struct(slot: u32, stride: u32) -> Vec<u32> {
    vec![
        (4 << 24) | OP_DCL_UAV_STRUCTURED,
        (OPERAND_UNORDERED_ACCESS_VIEW << 12) | (1 << 20),
        slot,
        stride,
    ]
}
fn decl_srv_raw(slot: u32) -> Vec<u32> {
    vec![
        (3 << 24) | OP_DCL_RESOURCE_RAW,
        (OPERAND_RESOURCE << 12) | (1 << 20),
        slot,
    ]
}
fn decl_tgsm_raw(slot: u32, bytes: u32) -> Vec<u32> {
    vec![
        (4 << 24) | OP_DCL_TGSM_RAW,
        (OPERAND_THREAD_GROUP_SHARED_MEMORY << 12) | (1 << 20),
        slot,
        bytes,
    ]
}
fn decl_group(x: u32, y: u32, z: u32) -> Vec<u32> {
    vec![(4 << 24) | OP_DCL_THREAD_GROUP, x, y, z]
}

fn cat(parts: &[Vec<u32>]) -> Vec<u32> {
    parts.iter().flatten().copied().collect()
}

fn ps(code: &[u32]) -> TranslatedShader {
    let dxbc = build_dxbc_container(
        ShaderStage::Pixel,
        &[("SV_POSITION", 0, 1, 0), ("TEXCOORD", 0, 0, 1)],
        &[("SV_Target", 0, 0, 0)],
        code,
    );
    let t = translate_dxbc_to_spirv(&dxbc).expect("traduction PS");
    validate(&t.as_bytes());
    t
}
fn cs(code: &[u32]) -> TranslatedShader {
    let dxbc = build_dxbc_container(ShaderStage::Compute, &[], &[], code);
    let t = translate_dxbc_to_spirv(&dxbc).expect("traduction CS");
    validate(&t.as_bytes());
    t
}
fn mov_out(i: u32) -> Vec<u32> {
    inst(OP_MOV, 0, &[od(0, 0xF), r(i)])
}

#[test]
fn control_flow_if_else_loop_switch_discard() {
    let code = cat(&[
        decl_temps(2),
        inst(OP_MOV, 0, &[rd(0, 0xF), v(1)]),
        // if_nz r0.x / else / endif
        inst(OP_IF, NZ, &[rs(0, 0)]),
        inst(OP_ADD, 0, &[rd(1, 0xF), r(0), imm([f(1.0); 4])]),
        i0(OP_ELSE),
        inst(OP_MUL, 0, &[rd(1, 0xF), r(0), imm([f(2.0); 4])]),
        i0(OP_ENDIF),
        // loop { breakc_nz r1.x; continuec_z r1.y; r1.z += 1 }
        i0(OP_LOOP),
        inst(OP_BREAKC, NZ, &[rs(1, 0)]),
        inst(OP_CONTINUEC, 0, &[rs(1, 1)]),
        inst(OP_ADD, 0, &[rd(1, 4), rs(1, 2), imm1(f(1.0))]),
        i0(OP_ENDLOOP),
        // switch avec fallthrough et default
        inst(OP_SWITCH, 0, &[rs(0, 1)]),
        inst(OP_CASE, 0, &[imm1(1)]),
        inst(OP_CASE, 0, &[imm1(2)]),
        inst(OP_MOV, 0, &[rd(1, 8), imm1(f(3.0))]),
        i0(OP_BREAK),
        i0(OP_DEFAULT),
        inst(OP_MOV, 0, &[rd(1, 8), imm1(f(4.0))]),
        i0(OP_BREAK),
        i0(OP_ENDSWITCH),
        inst(OP_DISCARD, NZ, &[rs(1, 3)]),
        inst(OP_RETC, 0, &[rs(1, 0)]),
        mov_out(1),
        i0(OP_RET),
    ]);
    ps(&code);
}

#[test]
fn control_flow_dead_code_after_terminators_and_empty_loop() {
    let code = cat(&[
        decl_temps(1),
        i0(OP_LOOP),
        i0(OP_BREAK),
        inst(OP_MOV, 0, &[rd(0, 0xF), imm([f(1.0); 4])]),
        i0(OP_ENDLOOP),
        inst(OP_IF, NZ, &[rs(0, 0)]),
        i0(OP_RET),
        inst(OP_MOV, 0, &[rd(0, 0xF), imm([0; 4])]),
        i0(OP_ENDIF),
        mov_out(0),
        i0(OP_RET),
        inst(OP_MOV, 0, &[rd(0, 0xF), imm([0; 4])]),
    ]);
    ps(&code);
}

#[test]
fn control_flow_nested_loops_switch_in_loop_and_subroutines() {
    let label = |n: u32| vec![(OPERAND_LABEL << 12) | (1 << 20), n];
    let code = cat(&[
        decl_temps(2),
        inst(OP_CALL, 0, &[label(0)]),
        inst(OP_CALLC, NZ, &[rs(0, 0), label(0)]),
        i0(OP_LOOP),
        i0(OP_LOOP),
        inst(OP_BREAKC, NZ, &[rs(0, 1)]),
        i0(OP_ENDLOOP),
        inst(OP_SWITCH, 0, &[rs(0, 2)]),
        inst(OP_CASE, 0, &[imm1(0)]),
        i0(OP_CONTINUE),
        i0(OP_DEFAULT),
        i0(OP_BREAK),
        i0(OP_ENDSWITCH),
        i0(OP_BREAK),
        i0(OP_ENDLOOP),
        mov_out(0),
        i0(OP_RET),
        inst(OP_LABEL, 0, &[label(0)]),
        inst(OP_ADD, 0, &[rd(0, 0xF), r(0), imm([f(1.0); 4])]),
        i0(OP_RET),
    ]);
    ps(&code);
}

#[test]
fn integer_and_bitwise_ops() {
    let two = |op: u32| inst(op, 0, &[rd(1, 0xF), r(0), r(0)]);
    let one = |op: u32| inst(op, 0, &[rd(1, 0xF), r(0)]);
    let mut parts = vec![decl_temps(3), inst(OP_MOV, 0, &[rd(0, 0xF), v(1)])];
    for op in [
        OP_IADD, OP_AND, OP_OR, OP_XOR, OP_IMIN, OP_IMAX, OP_UMIN, OP_UMAX, OP_ISHL, OP_ISHR,
        OP_USHR, OP_IEQ, OP_INE, OP_ILT, OP_IGE, OP_ULT, OP_UGE,
    ] {
        parts.push(two(op));
    }
    for op in [
        OP_INEG,
        OP_NOT,
        OP_BFREV,
        OP_COUNTBITS,
        OP_FIRSTBIT_HI,
        OP_FIRSTBIT_LO,
        OP_FIRSTBIT_SHI,
        OP_ITOF,
        OP_UTOF,
        OP_FTOI,
        OP_FTOU,
        OP_F32TOF16,
        OP_F16TOF32,
    ] {
        parts.push(one(op));
    }
    parts.push(inst(OP_IMAD, 0, &[rd(1, 0xF), r(0), r(0), r(0)]));
    parts.push(inst(OP_UMAD, 0, &[rd(1, 0x3), r(0), r(0), r(0)]));
    parts.push(inst(OP_IMUL, 0, &[rd(1, 0xF), rd(2, 0xF), r(0), r(0)]));
    parts.push(inst(
        OP_UMUL,
        0,
        &[vec![OPERAND_NULL << 12], rd(2, 0x1), r(0), r(0)],
    ));
    parts.push(inst(OP_UDIV, 0, &[rd(1, 0xF), rd(2, 0xF), r(0), r(0)]));
    parts.push(inst(
        OP_UDIV,
        0,
        &[rd(1, 0x1), vec![OPERAND_NULL << 12], r(0), r(0)],
    ));
    parts.push(inst(OP_BFI, 0, &[rd(1, 0xF), r(0), r(0), r(0), r(0)]));
    parts.push(inst(OP_UBFE, 0, &[rd(1, 0xF), r(0), r(0), r(0)]));
    parts.push(inst(OP_IBFE, 0, &[rd(1, 0xF), r(0), r(0), r(0)]));
    parts.push(inst(OP_UADDC, 0, &[rd(1, 0xF), rd(2, 0xF), r(0), r(0)]));
    parts.push(inst(OP_USUBB, 0, &[rd(1, 0xF), rd(2, 0xF), r(0), r(0)]));
    parts.push(inst(OP_MOVC, 0, &[rd(1, 0xF), r(0), r(1), r(2)]));
    parts.push(inst(
        OP_MOVC,
        SAT,
        &[rd(1, 0x7), r(0), r(1), imm([f(1.0); 4])],
    ));
    parts.push(mov_out(1));
    parts.push(i0(OP_RET));
    ps(&cat(&parts));
}

#[test]
fn float_ops_and_modifiers() {
    let mut neg_abs = r(0);
    // Opérande étendu : modificateur abs+neg (bit 31 + jeton `type=1, modifier=3`).
    neg_abs[0] |= 1 << 31;
    neg_abs.insert(1, 1 | (3 << 6));
    let mut parts = vec![decl_temps(2), inst(OP_MOV, 0, &[rd(0, 0xF), v(1)])];
    for op in [
        OP_RCP,
        OP_RSQ,
        OP_SQRT,
        OP_EXP,
        OP_LOG,
        OP_FRC,
        OP_ROUND_NE,
        OP_ROUND_NI,
        OP_ROUND_PI,
        OP_ROUND_Z,
        OP_DERIV_RTX,
        OP_DERIV_RTY,
        OP_DERIV_RTX_COARSE,
        OP_DERIV_RTY_FINE,
    ] {
        parts.push(inst(op, SAT, &[rd(1, 0xB), r(0)]));
    }
    for op in [
        OP_ADD, OP_MUL, OP_DIV, OP_MIN, OP_MAX, OP_EQ, OP_NE, OP_LT, OP_GE,
    ] {
        parts.push(inst(op, 0, &[rd(1, 0xF), neg_abs.clone(), r(0)]));
    }
    parts.push(inst(OP_MAD, SAT, &[rd(1, 0x5), r(0), r(0), r(0)]));
    parts.push(inst(OP_DP2, 0, &[rd(1, 0xF), r(0), r(0)]));
    parts.push(inst(OP_DP3, 0, &[rd(1, 0x1), r(0), r(0)]));
    parts.push(inst(OP_DP4, 0, &[rd(1, 0x2), r(0), r(0)]));
    parts.push(inst(OP_SINCOS, 0, &[rd(1, 0x1), rd(0, 0x2), r(0)]));
    parts.push(inst(
        OP_SINCOS,
        0,
        &[vec![OPERAND_NULL << 12], rd(0, 0x2), r(0)],
    ));
    parts.push(mov_out(1));
    parts.push(i0(OP_RET));
    ps(&cat(&parts));
}

fn tex_ps(dim: u32, extra_decl: Vec<u32>, body: Vec<Vec<u32>>) -> TranslatedShader {
    let mut parts = vec![decl_temps(2), decl_res(0, dim), decl_samp(0, 0), extra_decl];
    parts.push(inst(OP_MOV, 0, &[rd(0, 0xF), v(1)]));
    parts.extend(body);
    parts.push(mov_out(1));
    parts.push(i0(OP_RET));
    ps(&cat(&parts))
}

#[test]
fn texture_sampling_all_dimensions_and_variants() {
    // dims : 2 = 1D, 3 = 2D, 5 = 3D, 6 = Cube, 7 = 1DArray, 8 = 2DArray, 10 = CubeArray.
    for dim in [2u32, 3, 5, 6, 7, 8, 10] {
        let s = |op: u32, extra: Vec<Vec<u32>>| {
            let mut o = vec![rd(1, 0xF), r(0), tres(0, XYZW), samp(0)];
            o.extend(extra);
            inst(op, 0, &o)
        };
        tex_ps(
            dim,
            vec![],
            vec![
                s(OP_SAMPLE, vec![]),
                s(OP_SAMPLE_B, vec![rs(0, 3)]),
                s(OP_SAMPLE_L, vec![rs(0, 3)]),
                s(OP_SAMPLE_D, vec![r(0), r(0)]),
                inst(OP_LOD, 0, &[rd(1, 0x3), r(0), tres(0, XYZW), samp(0)]),
                inst(OP_RESINFO, 0, &[rd(1, 0xF), imm1(0), tres(0, XYZW)]),
                inst(OP_RESINFO, 1 << 11, &[rd(1, 0xF), imm1(0), tres(0, XYZW)]),
                inst(OP_RESINFO, 2 << 11, &[rd(1, 0xF), imm1(0), tres(0, XYZW)]),
            ],
        );
    }
}

#[test]
fn texture_compare_gather_fetch_and_offsets() {
    // Offset de texel immédiat `aoffimmi(1, -1, 0)` (opcode étendu type 1).
    let off = (1u32 & 0xF) << 9 | (((-1i32) as u32) & 0xF) << 13;
    let with_off = |op: u32, ops: Vec<Vec<u32>>| {
        let mut w = inst(op, 1 << 31, &ops);
        w.insert(1, 1 | off);
        w[0] += 1 << 24;
        w
    };
    let t = tex_ps(
        3,
        vec![],
        vec![
            inst(
                OP_SAMPLE_C,
                0,
                &[rd(1, 0x1), r(0), tres(0, XYZW), samp(0), rs(0, 2)],
            ),
            inst(
                OP_SAMPLE_C_LZ,
                0,
                &[rd(1, 0x1), r(0), tres(0, XYZW), samp(0), rs(0, 2)],
            ),
            with_off(OP_SAMPLE, vec![rd(1, 0xF), r(0), tres(0, XYZW), samp(0)]),
            inst(
                OP_GATHER4,
                0,
                &[rd(1, 0xF), r(0), tres(0, XYZW), samp_c(0, 2)],
            ),
            inst(
                OP_GATHER4_C,
                0,
                &[rd(1, 0xF), r(0), tres(0, XYZW), samp(0), rs(0, 2)],
            ),
            inst(
                OP_GATHER4_PO,
                0,
                &[rd(1, 0xF), r(0), r(0), tres(0, XYZW), samp_c(0, 0)],
            ),
            inst(
                OP_GATHER4_PO_C,
                0,
                &[rd(1, 0xF), r(0), r(0), tres(0, XYZW), samp(0), rs(0, 1)],
            ),
            with_off(
                OP_GATHER4,
                vec![rd(1, 0xF), r(0), tres(0, XYZW), samp_c(0, 1)],
            ),
            inst(OP_LD, 0, &[rd(1, 0xF), r(0), tres(0, swz(2, 1, 0, 3))]),
            with_off(OP_LD, vec![rd(1, 0xF), r(0), tres(0, XYZW)]),
        ],
    );
    assert_eq!(t.used_textures, vec![0]);
    assert_eq!(t.used_samplers, vec![0]);
}

#[test]
fn texture_buffers_multisample_and_info() {
    // Buffer typé (dim 1) + `ld` + `bufinfo`.
    tex_ps(
        1,
        vec![],
        vec![
            inst(OP_LD, 0, &[rd(1, 0xF), r(0), tres(0, XYZW)]),
            inst(OP_BUFINFO, 0, &[rd(1, 0x1), tres(0, XYZW)]),
        ],
    );
    // Texture2DMS (dim 4) : `ld_ms`, `sampleinfo`, `resinfo`.
    tex_ps(
        4,
        vec![],
        vec![
            inst(OP_LD_MS, 0, &[rd(1, 0xF), r(0), tres(0, XYZW), rs(0, 3)]),
            inst(OP_SAMPLEINFO, 0, &[rd(1, 0x1), tres(0, XYZW)]),
            inst(
                OP_SAMPLEINFO,
                1 << 11,
                &[rd(1, 0x1), vec![OPERAND_RASTERIZER << 12]],
            ),
            inst(OP_RESINFO, 0, &[rd(1, 0xF), imm1(0), tres(0, XYZW)]),
        ],
    );
    // Texture2DArray MS (dim 9) + Texture2D UAV typée via resinfo.
    tex_ps(
        9,
        vec![],
        vec![inst(
            OP_LD_MS,
            0,
            &[rd(1, 0xF), r(0), tres(0, XYZW), rs(0, 3)],
        )],
    );
}

#[test]
fn pixel_system_values_depth_coverage_and_eval() {
    let dxbc = build_dxbc_container(
        ShaderStage::Pixel,
        &[
            ("SV_POSITION", 0, 1, 0),
            ("TEXCOORD", 0, 0, 1),
            ("SV_IsFrontFace", 0, 9, 2),
            ("SV_PrimitiveID", 0, 7, 3),
            ("SV_SampleIndex", 0, 10, 4),
        ],
        &[("SV_Target", 0, 0, 0)],
        &cat(&[
            decl_temps(2),
            inst(OP_MOV, 0, &[rd(0, 0xF), v(2)]),
            inst(OP_ADD, 0, &[rd(0, 0x1), rs(0, 0), v(3)]),
            inst(OP_ADD, 0, &[rd(0, 0x2), rs(0, 1), v(4)]),
            inst(OP_EVAL_CENTROID, 0, &[rd(1, 0xF), v(1)]),
            inst(OP_EVAL_SAMPLE_INDEX, 0, &[rd(1, 0xF), v(1), rs(0, 0)]),
            inst(OP_EVAL_SNAPPED, 0, &[rd(1, 0xF), v(1), r(0)]),
            inst(OP_MOV, 0, &[dst(OPERAND_OUTPUT_DEPTH, &[], 0x1), rs(1, 0)]),
            inst(
                OP_MOV,
                0,
                &[dst(OPERAND_OUTPUT_COVERAGE_MASK, &[], 0x1), rs(1, 1)],
            ),
            mov_out(0),
            i0(OP_RET),
        ]),
    );
    // `dst(...)` avec `idx` vide : `OPERAND_OUTPUT_DEPTH` n'a pas d'index (dims = 0).
    let t = translate_dxbc_to_spirv(&dxbc).expect("PS valeurs système");
    validate(&t.as_bytes());
}

#[test]
fn vertex_system_values_clip_distance_and_layer() {
    let dxbc = build_dxbc_container(
        ShaderStage::Vertex,
        &[
            ("POSITION", 0, 0, 0),
            ("SV_VertexID", 0, 6, 1),
            ("SV_InstanceID", 0, 8, 2),
        ],
        &[
            ("SV_POSITION", 0, 1, 0),
            ("SV_ClipDistance", 0, 2, 1),
            ("SV_RenderTargetArrayIndex", 0, 4, 2),
            ("TEXCOORD", 0, 0, 3),
        ],
        &cat(&[
            decl_temps(1),
            inst(OP_MOV, 0, &[rd(0, 0xF), v(0)]),
            inst(OP_ADD, 0, &[rd(0, 0x1), rs(0, 0), v(1)]),
            inst(OP_MOV, 0, &[od(0, 0xF), r(0)]),
            inst(OP_MOV, 0, &[od(1, 0x1), r(0)]),
            inst(OP_MOV, 0, &[od(2, 0x1), v(2)]),
            inst(OP_MOV, 0, &[od(3, 0xF), r(0)]),
            i0(OP_RET),
        ]),
    );
    let t = translate_dxbc_to_spirv(&dxbc).expect("VS valeurs système");
    validate(&t.as_bytes());
}

#[test]
fn compute_buffers_tgsm_atomics_and_barriers() {
    let sync = |flags: u32| inst(OP_SYNC, flags << 11, &[]);
    let tid = sel(OPERAND_INPUT_THREAD_ID, &[], 0);
    let t = cs(&cat(&[
        decl_temps(2),
        decl_group(64, 1, 1),
        decl_uav_raw(0),
        decl_uav_struct(1, 20),
        decl_uav_typed(2, 3),
        decl_uav_typed(3, 5),
        decl_srv_raw(0),
        decl_tgsm_raw(0, 256),
        inst(OP_LD_RAW, 0, &[rd(0, 0xF), imm1(16), uav_s(0, XYZW)]),
        inst(
            OP_LD_RAW,
            0,
            &[rd(0, 0x3), imm1(16), tres(0, swz(1, 0, 0, 0))],
        ),
        inst(OP_STORE_RAW, 0, &[uav_d(0, 0x7), tid.clone(), r(0)]),
        inst(
            OP_LD_STRUCTURED,
            0,
            &[rd(0, 0xF), tid.clone(), imm1(4), uav_s(1, XYZW)],
        ),
        inst(
            OP_STORE_STRUCTURED,
            0,
            &[uav_d(1, 0x3), tid.clone(), imm1(8), r(0)],
        ),
        inst(OP_STORE_RAW, 0, &[g_d(0, 0xF), imm1(0), r(0)]),
        sync(0b0011),
        inst(OP_LD_RAW, 0, &[rd(1, 0x1), imm1(4), g_s(0)]),
        sync(0b1010),
        sync(0b0100),
        inst(
            OP_IMM_ATOMIC_IADD,
            0,
            &[rd(1, 0x1), uav_d(0, 0), imm1(8), imm1(1)],
        ),
        inst(OP_ATOMIC_UMAX, 0, &[uav_d(0, 0), imm1(8), imm1(1)]),
        inst(
            OP_IMM_ATOMIC_CMP_EXCH,
            0,
            &[rd(1, 0x1), uav_d(0, 0), imm1(8), imm1(1), imm1(2)],
        ),
        inst(
            OP_ATOMIC_CMP_STORE,
            0,
            &[uav_d(0, 0), imm1(8), imm1(1), imm1(2)],
        ),
        inst(
            OP_IMM_ATOMIC_EXCH,
            0,
            &[rd(1, 0x1), g_d(0, 0), imm1(8), imm1(1)],
        ),
        inst(OP_ATOMIC_OR, 0, &[g_d(0, 0), imm1(8), imm1(1)]),
        inst(
            OP_IMM_ATOMIC_IMAX,
            0,
            &[rd(1, 0x1), uav_d(1, 0), r(0), imm1(1)],
        ),
        inst(OP_LD_UAV_TYPED, 0, &[rd(0, 0xF), r(0), uav_s(2, XYZW)]),
        inst(OP_STORE_UAV_TYPED, 0, &[uav_d(2, 0xF), r(0), r(0)]),
        inst(OP_ATOMIC_IADD, 0, &[uav_d(3, 0), r(0), imm1(1)]),
        inst(OP_BUFINFO, 0, &[rd(1, 0x1), uav_s(0, XYZW)]),
        inst(OP_BUFINFO, 0, &[rd(1, 0x1), uav_s(1, XYZW)]),
        inst(OP_BUFINFO, 0, &[rd(1, 0x1), tres(0, XYZW)]),
        inst(OP_RESINFO, 0, &[rd(1, 0xF), imm1(0), uav_s(2, XYZW)]),
        i0(OP_RET),
    ]));
    assert_eq!(t.thread_group_size, [64, 1, 1]);
    assert_eq!(t.used_storage_buffers, vec![0, 1, 2, 3]);
    assert_eq!(t.used_srv_buffers, vec![0]);
}

#[test]
fn compute_thread_ids_and_flattened_index() {
    cs(&cat(&[
        decl_temps(1),
        decl_group(8, 8, 1),
        inst(
            OP_MOV,
            0,
            &[rd(0, 0x7), src(OPERAND_INPUT_THREAD_GROUP_ID, &[], XYZW)],
        ),
        inst(
            OP_IADD,
            0,
            &[
                rd(0, 0x7),
                r(0),
                src(OPERAND_INPUT_THREAD_ID_IN_GROUP, &[], XYZW),
            ],
        ),
        inst(
            OP_IADD,
            0,
            &[
                rd(0, 0x1),
                rs(0, 0),
                sel(OPERAND_INPUT_THREAD_ID_IN_GROUP_FLATTENED, &[], 0),
            ],
        ),
        i0(OP_RET),
    ]));
}

#[test]
fn indexable_temps_and_immediate_constant_buffer() {
    let dcl_x = vec![(4 << 24) | OP_DCL_INDEXABLE_TEMP, 0, 4, 4];
    let mut x_idx =
        vec![2 | (1 << 2) | (XYZW << 4) | (OPERAND_INDEXABLE_TEMP << 12) | (2 << 20) | (3 << 25)];
    x_idx.extend([0, 1]); // x0[r0.x + 1]
    x_idx.extend(rs(0, 0));
    let mut icb = vec![
        2 | (1 << 2)
            | (XYZW << 4)
            | (OPERAND_IMMEDIATE_CONSTANT_BUFFER << 12)
            | (1 << 20)
            | (2 << 22),
    ];
    icb.extend(rs(0, 1));
    let data = {
        let mut d = vec![(3 << 11) | OP_CUSTOMDATA, 2 + 1 + 8];
        d.push(0);
        d.extend((0..8).map(|i| f(i as f32)));
        d
    };
    ps(&cat(&[
        decl_temps(1),
        dcl_x,
        data,
        inst(OP_MOV, 0, &[rd(0, 0xF), v(1)]),
        inst(
            OP_MOV,
            0,
            &[dst(OPERAND_INDEXABLE_TEMP, &[0, 2], 0xF), r(0)],
        ),
        inst(OP_ADD, 0, &[rd(0, 0xF), x_idx, icb]),
        mov_out(0),
        i0(OP_RET),
    ]));
}

#[test]
fn malformed_inputs_return_errors_without_panicking() {
    // Longueur d'instruction nulle / débordante.
    for code in [
        vec![OP_MOV],
        vec![(200u32 << 24) | OP_MOV, 0],
        vec![(3u32 << 24) | OP_ADD, 0],
        vec![
            (2u32 << 24) | OP_MOV,
            2 | (OPERAND_TEMP << 12) | (1 << 20) | (3 << 22),
        ],
    ] {
        let dxbc = build_dxbc_container(ShaderStage::Pixel, &[], &[], &code);
        assert!(translate_dxbc_to_spirv(&dxbc).is_err());
    }
    // Opcode inconnu / non supporté : erreur typée portant le nom.
    let dxbc = build_dxbc_container(ShaderStage::Pixel, &[], &[], &[(1u32 << 24) | OP_SWAPC]);
    match translate_dxbc_to_spirv(&dxbc) {
        Err(ShaderTranslateError::UnsupportedOpcode { name, .. }) => assert_eq!(name, "swapc"),
        other => panic!("attendu UnsupportedOpcode, obtenu {other:?}"),
    }
    let dxbc = build_dxbc_container(ShaderStage::Pixel, &[], &[], &[(1u32 << 24) | 0x7F0]);
    assert!(matches!(
        translate_dxbc_to_spirv(&dxbc),
        Err(ShaderTranslateError::UnsupportedOpcode {
            name: "unknown",
            ..
        })
    ));
    // Flux de contrôle déséquilibré.
    for code in [vec![(1u32 << 24) | OP_ENDIF], vec![(1u32 << 24) | OP_LOOP]] {
        let dxbc = build_dxbc_container(ShaderStage::Pixel, &[], &[], &code);
        assert!(translate_dxbc_to_spirv(&dxbc).is_err());
    }
    // Conteneurs tronqués / bruit pseudo-aléatoire : jamais de panic.
    let good = sample_textured_cube_ps_dxbc();
    for cut in 0..good.len() {
        let _ = translate_dxbc_to_spirv(&good[..cut]);
    }
    let mut x = 0x1234_5678u32;
    for _ in 0..400 {
        let mut bytes = good.clone();
        for _ in 0..6 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let p = (x >> 8) as usize % bytes.len();
            bytes[p] = (x >> 24) as u8;
        }
        let _ = translate_dxbc_to_spirv(&bytes);
    }
}

#[test]
fn unsupported_stages_fail_cleanly() {
    // Types de programme 2 (GS), 3 (HS), 4 (DS) : reconnus, rejetés proprement.
    for kind in [2u32, 3, 4] {
        let mut dxbc = build_dxbc_container(ShaderStage::Vertex, &[], &[], &[(1 << 24) | OP_RET]);
        // Le premier mot du chunk SHEX porte le type de programme (bits 16..31).
        let pos = dxbc.windows(4).position(|w| w == b"SHEX").expect("SHEX") + 8;
        dxbc[pos + 2] = kind as u8;
        assert!(matches!(
            translate_dxbc_to_spirv(&dxbc),
            Err(ShaderTranslateError::UnsupportedStage(_))
        ));
    }
}
