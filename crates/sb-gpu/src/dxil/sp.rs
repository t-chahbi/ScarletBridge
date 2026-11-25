//! Constantes SPIR-V (opcodes, capacités, décorations) et types scalaires DXIL.

#![allow(dead_code)]

// ---- Opcodes
pub const OP_UNDEF: u16 = 1;
pub const OP_EXT_INST: u16 = 12;
pub const OP_FUNCTION_CALL: u16 = 57;
pub const OP_VARIABLE: u16 = 59;
pub const OP_IMAGE_TEXEL_POINTER: u16 = 60;
pub const OP_LOAD: u16 = 61;
pub const OP_STORE: u16 = 62;
pub const OP_ACCESS_CHAIN: u16 = 65;
pub const OP_ARRAY_LENGTH: u16 = 68;
pub const OP_VECTOR_EXTRACT_DYN: u16 = 77;
pub const OP_VECTOR_SHUFFLE: u16 = 79;
pub const OP_COMPOSITE_CONSTRUCT: u16 = 80;
pub const OP_COMPOSITE_EXTRACT: u16 = 81;
pub const OP_SAMPLED_IMAGE: u16 = 86;
pub const OP_IMAGE_SAMPLE_IMPLICIT_LOD: u16 = 87;
pub const OP_IMAGE_SAMPLE_EXPLICIT_LOD: u16 = 88;
pub const OP_IMAGE_SAMPLE_DREF_IMPLICIT_LOD: u16 = 89;
pub const OP_IMAGE_SAMPLE_DREF_EXPLICIT_LOD: u16 = 90;
pub const OP_IMAGE_FETCH: u16 = 95;
pub const OP_IMAGE_GATHER: u16 = 96;
pub const OP_IMAGE_DREF_GATHER: u16 = 97;
pub const OP_IMAGE_READ: u16 = 98;
pub const OP_IMAGE_WRITE: u16 = 99;
pub const OP_IMAGE_QUERY_SIZE_LOD: u16 = 103;
pub const OP_IMAGE_QUERY_SIZE: u16 = 104;
pub const OP_IMAGE_QUERY_LOD: u16 = 105;
pub const OP_IMAGE_QUERY_LEVELS: u16 = 106;
pub const OP_IMAGE_QUERY_SAMPLES: u16 = 107;
pub const OP_CONVERT_F_TO_U: u16 = 109;
pub const OP_CONVERT_F_TO_S: u16 = 110;
pub const OP_CONVERT_S_TO_F: u16 = 111;
pub const OP_CONVERT_U_TO_F: u16 = 112;
pub const OP_U_CONVERT: u16 = 113;
pub const OP_S_CONVERT: u16 = 114;
pub const OP_F_CONVERT: u16 = 115;
pub const OP_BITCAST: u16 = 124;
pub const OP_S_NEGATE: u16 = 126;
pub const OP_F_NEGATE: u16 = 127;
pub const OP_I_ADD: u16 = 128;
pub const OP_F_ADD: u16 = 129;
pub const OP_I_SUB: u16 = 130;
pub const OP_F_SUB: u16 = 131;
pub const OP_I_MUL: u16 = 132;
pub const OP_F_MUL: u16 = 133;
pub const OP_U_DIV: u16 = 134;
pub const OP_S_DIV: u16 = 135;
pub const OP_F_DIV: u16 = 136;
pub const OP_U_MOD: u16 = 137;
pub const OP_S_REM: u16 = 138;
pub const OP_F_REM: u16 = 140;
pub const OP_DOT: u16 = 148;
pub const OP_I_ADD_CARRY: u16 = 149;
pub const OP_I_SUB_BORROW: u16 = 150;
pub const OP_U_MUL_EXTENDED: u16 = 151;
pub const OP_S_MUL_EXTENDED: u16 = 152;
pub const OP_ANY: u16 = 154;
pub const OP_ALL: u16 = 155;
pub const OP_IS_NAN: u16 = 156;
pub const OP_IS_INF: u16 = 157;
pub const OP_LOGICAL_EQUAL: u16 = 164;
pub const OP_LOGICAL_NOT_EQUAL: u16 = 165;
pub const OP_LOGICAL_OR: u16 = 166;
pub const OP_LOGICAL_AND: u16 = 167;
pub const OP_LOGICAL_NOT: u16 = 168;
pub const OP_SELECT: u16 = 169;
pub const OP_I_EQUAL: u16 = 170;
pub const OP_I_NOT_EQUAL: u16 = 171;
pub const OP_U_GREATER_THAN: u16 = 172;
pub const OP_S_GREATER_THAN: u16 = 173;
pub const OP_U_GREATER_THAN_EQUAL: u16 = 174;
pub const OP_S_GREATER_THAN_EQUAL: u16 = 175;
pub const OP_U_LESS_THAN: u16 = 176;
pub const OP_S_LESS_THAN: u16 = 177;
pub const OP_U_LESS_THAN_EQUAL: u16 = 178;
pub const OP_S_LESS_THAN_EQUAL: u16 = 179;
pub const OP_F_ORD_EQUAL: u16 = 180;
pub const OP_F_UNORD_EQUAL: u16 = 181;
pub const OP_F_ORD_NOT_EQUAL: u16 = 182;
pub const OP_F_UNORD_NOT_EQUAL: u16 = 183;
pub const OP_F_ORD_LESS_THAN: u16 = 184;
pub const OP_F_UNORD_LESS_THAN: u16 = 185;
pub const OP_F_ORD_GREATER_THAN: u16 = 186;
pub const OP_F_UNORD_GREATER_THAN: u16 = 187;
pub const OP_F_ORD_LESS_THAN_EQUAL: u16 = 188;
pub const OP_F_UNORD_LESS_THAN_EQUAL: u16 = 189;
pub const OP_F_ORD_GREATER_THAN_EQUAL: u16 = 190;
pub const OP_F_UNORD_GREATER_THAN_EQUAL: u16 = 191;
pub const OP_SHIFT_RIGHT_LOGICAL: u16 = 194;
pub const OP_SHIFT_RIGHT_ARITHMETIC: u16 = 195;
pub const OP_SHIFT_LEFT_LOGICAL: u16 = 196;
pub const OP_BITWISE_OR: u16 = 197;
pub const OP_BITWISE_XOR: u16 = 198;
pub const OP_BITWISE_AND: u16 = 199;
pub const OP_NOT: u16 = 200;
pub const OP_BIT_FIELD_INSERT: u16 = 201;
pub const OP_BIT_FIELD_S_EXTRACT: u16 = 202;
pub const OP_BIT_FIELD_U_EXTRACT: u16 = 203;
pub const OP_BIT_REVERSE: u16 = 204;
pub const OP_BIT_COUNT: u16 = 205;
pub const OP_DPDX_FINE: u16 = 210;
pub const OP_DPDY_FINE: u16 = 211;
pub const OP_DPDX_COARSE: u16 = 213;
pub const OP_DPDY_COARSE: u16 = 214;
pub const OP_CONTROL_BARRIER: u16 = 224;
pub const OP_MEMORY_BARRIER: u16 = 225;
pub const OP_ATOMIC_EXCHANGE: u16 = 229;
pub const OP_ATOMIC_COMPARE_EXCHANGE: u16 = 230;
pub const OP_ATOMIC_I_ADD: u16 = 234;
pub const OP_ATOMIC_I_SUB: u16 = 235;
pub const OP_ATOMIC_S_MIN: u16 = 236;
pub const OP_ATOMIC_U_MIN: u16 = 237;
pub const OP_ATOMIC_S_MAX: u16 = 238;
pub const OP_ATOMIC_U_MAX: u16 = 239;
pub const OP_ATOMIC_AND: u16 = 240;
pub const OP_ATOMIC_OR: u16 = 241;
pub const OP_ATOMIC_XOR: u16 = 242;
pub const OP_LOOP_MERGE: u16 = 246;
pub const OP_SELECTION_MERGE: u16 = 247;
pub const OP_LABEL: u16 = 248;
pub const OP_BRANCH: u16 = 249;
pub const OP_BRANCH_CONDITIONAL: u16 = 250;
pub const OP_SWITCH: u16 = 251;
pub const OP_KILL: u16 = 252;
pub const OP_RETURN: u16 = 253;
pub const OP_UNREACHABLE: u16 = 255;
pub const OP_GROUP_NU_ELECT: u16 = 333;
pub const OP_GROUP_NU_ALL: u16 = 334;
pub const OP_GROUP_NU_ANY: u16 = 335;
pub const OP_GROUP_NU_ALL_EQUAL: u16 = 336;
pub const OP_GROUP_NU_BROADCAST_FIRST: u16 = 338;
pub const OP_GROUP_NU_BALLOT: u16 = 339;
pub const OP_GROUP_NU_BALLOT_BIT_COUNT: u16 = 342;
pub const OP_GROUP_NU_SHUFFLE: u16 = 345;
pub const OP_GROUP_NU_IADD: u16 = 349;
pub const OP_GROUP_NU_FADD: u16 = 350;
pub const OP_GROUP_NU_IMUL: u16 = 351;
pub const OP_GROUP_NU_FMUL: u16 = 352;
pub const OP_GROUP_NU_SMIN: u16 = 353;
pub const OP_GROUP_NU_UMIN: u16 = 354;
pub const OP_GROUP_NU_FMIN: u16 = 355;
pub const OP_GROUP_NU_SMAX: u16 = 356;
pub const OP_GROUP_NU_UMAX: u16 = 357;
pub const OP_GROUP_NU_FMAX: u16 = 358;
pub const OP_GROUP_NU_BITWISE_AND: u16 = 359;
pub const OP_GROUP_NU_BITWISE_OR: u16 = 360;
pub const OP_GROUP_NU_BITWISE_XOR: u16 = 361;
pub const OP_GROUP_NU_QUAD_SWAP: u16 = 366;

// ---- Classes de stockage
pub const SC_UNIFORM_CONSTANT: u32 = 0;
pub const SC_INPUT: u32 = 1;
pub const SC_UNIFORM: u32 = 2;
pub const SC_OUTPUT: u32 = 3;
pub const SC_WORKGROUP: u32 = 4;
pub const SC_PRIVATE: u32 = 6;
pub const SC_FUNCTION: u32 = 7;
pub const SC_STORAGE_BUFFER: u32 = 12;

// ---- Décorations
pub const DEC_BLOCK: u32 = 2;
pub const DEC_ARRAY_STRIDE: u32 = 6;
pub const DEC_BUILTIN: u32 = 11;
pub const DEC_NO_PERSPECTIVE: u32 = 13;
pub const DEC_FLAT: u32 = 14;
pub const DEC_CENTROID: u32 = 16;
pub const DEC_SAMPLE: u32 = 17;
pub const DEC_NON_WRITABLE: u32 = 24;
pub const DEC_LOCATION: u32 = 30;
pub const DEC_COMPONENT: u32 = 31;
pub const DEC_BINDING: u32 = 33;
pub const DEC_DESCRIPTOR_SET: u32 = 34;
pub const DEC_OFFSET: u32 = 35;
pub const DEC_NON_UNIFORM: u32 = 5300;
pub const DEC_PER_VERTEX_KHR: u32 = 5285;

// ---- Capacités
pub const CAP_GEOMETRY: u32 = 2;
pub const CAP_TESSELLATION: u32 = 3;
pub const CAP_FLOAT16: u32 = 9;
pub const CAP_FLOAT64: u32 = 10;
pub const CAP_INT64: u32 = 11;
pub const CAP_INT16: u32 = 22;
pub const CAP_IMAGE_GATHER_EXTENDED: u32 = 25;
pub const CAP_STORAGE_IMAGE_MULTISAMPLE: u32 = 27;
pub const CAP_IMAGE_CUBE_ARRAY: u32 = 34;
pub const CAP_SAMPLE_RATE_SHADING: u32 = 35;
pub const CAP_SAMPLED1D: u32 = 43;
pub const CAP_IMAGE1D: u32 = 44;
pub const CAP_SAMPLED_BUFFER: u32 = 46;
pub const CAP_IMAGE_BUFFER: u32 = 47;
pub const CAP_IMAGE_MS_ARRAY: u32 = 48;
pub const CAP_IMAGE_QUERY: u32 = 50;
pub const CAP_DERIVATIVE_CONTROL: u32 = 51;
pub const CAP_INTERPOLATION_FUNCTION: u32 = 52;
pub const CAP_STORAGE_IMAGE_READ_WITHOUT_FORMAT: u32 = 55;
pub const CAP_STORAGE_IMAGE_WRITE_WITHOUT_FORMAT: u32 = 56;
pub const CAP_GROUP_NON_UNIFORM: u32 = 61;
pub const CAP_GROUP_NU_VOTE: u32 = 62;
pub const CAP_GROUP_NU_ARITHMETIC: u32 = 63;
pub const CAP_GROUP_NU_BALLOT: u32 = 64;
pub const CAP_GROUP_NU_SHUFFLE: u32 = 65;
pub const CAP_GROUP_NU_QUAD: u32 = 68;
pub const CAP_RUNTIME_DESCRIPTOR_ARRAY: u32 = 5302;
pub const CAP_SAMPLED_IMAGE_ARRAY_NON_UNIFORM: u32 = 5307;
pub const CAP_STORAGE_BUFFER_ARRAY_NON_UNIFORM: u32 = 5308;
pub const CAP_STORAGE_IMAGE_ARRAY_NON_UNIFORM: u32 = 5309;
pub const CAP_UNIFORM_TEXEL_BUFFER_ARRAY_NON_UNIFORM: u32 = 5311;
pub const CAP_STORAGE_TEXEL_BUFFER_ARRAY_NON_UNIFORM: u32 = 5312;
pub const CAP_FRAGMENT_BARYCENTRIC: u32 = 5284;

// ---- Built-ins
pub const BI_FRAG_COORD: u32 = 15;
pub const BI_FRONT_FACING: u32 = 17;
pub const BI_SAMPLE_ID: u32 = 18;
pub const BI_FRAG_DEPTH: u32 = 22;
pub const BI_SAMPLE_MASK: u32 = 20;
pub const BI_PRIMITIVE_ID: u32 = 7;
pub const BI_LAYER: u32 = 9;
pub const BI_VIEWPORT_INDEX: u32 = 10;
pub const BI_VERTEX_INDEX: u32 = 42;
pub const BI_INSTANCE_INDEX: u32 = 43;
pub const BI_NUM_WORKGROUPS: u32 = 24;
pub const BI_WORKGROUP_ID: u32 = 26;
pub const BI_LOCAL_INVOCATION_ID: u32 = 27;
pub const BI_GLOBAL_INVOCATION_ID: u32 = 28;
pub const BI_LOCAL_INVOCATION_INDEX: u32 = 29;
pub const BI_SUBGROUP_SIZE: u32 = 36;
pub const BI_SUBGROUP_LOCAL_INVOCATION_ID: u32 = 41;

// ---- GLSL.std.450
pub const G_ROUND_EVEN: u32 = 2;
pub const G_TRUNC: u32 = 3;
pub const G_FABS: u32 = 4;
pub const G_FLOOR: u32 = 8;
pub const G_CEIL: u32 = 9;
pub const G_FRACT: u32 = 10;
pub const G_SIN: u32 = 13;
pub const G_COS: u32 = 14;
pub const G_TAN: u32 = 15;
pub const G_ASIN: u32 = 16;
pub const G_ACOS: u32 = 17;
pub const G_ATAN: u32 = 18;
pub const G_SINH: u32 = 19;
pub const G_COSH: u32 = 20;
pub const G_TANH: u32 = 21;
pub const G_EXP2: u32 = 29;
pub const G_LOG2: u32 = 30;
pub const G_SQRT: u32 = 31;
pub const G_INVERSE_SQRT: u32 = 32;
pub const G_UMIN: u32 = 38;
pub const G_SMIN: u32 = 39;
pub const G_UMAX: u32 = 41;
pub const G_SMAX: u32 = 42;
pub const G_FCLAMP: u32 = 43;
pub const G_FMA: u32 = 50;
pub const G_PACK_HALF_2X16: u32 = 58;
pub const G_UNPACK_HALF_2X16: u32 = 62;
pub const G_FIND_I_LSB: u32 = 73;
pub const G_FIND_S_MSB: u32 = 74;
pub const G_FIND_U_MSB: u32 = 75;
pub const G_INTERPOLATE_AT_CENTROID: u32 = 76;
pub const G_INTERPOLATE_AT_SAMPLE: u32 = 77;
pub const G_INTERPOLATE_AT_OFFSET: u32 = 78;
pub const G_NMIN: u32 = 79;
pub const G_NMAX: u32 = 80;

/// Type scalaire DXIL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum St {
    I1,
    I8,
    I16,
    I32,
    I64,
    F16,
    F32,
    F64,
}

impl St {
    pub fn is_float(self) -> bool {
        matches!(self, St::F16 | St::F32 | St::F64)
    }
    pub fn bits(self) -> u32 {
        match self {
            St::I1 => 1,
            St::I8 => 8,
            St::I16 | St::F16 => 16,
            St::I32 | St::F32 => 32,
            St::I64 | St::F64 => 64,
        }
    }
    /// Entier de même largeur.
    pub fn as_int(self) -> St {
        match self {
            St::F16 => St::I16,
            St::F32 => St::I32,
            St::F64 => St::I64,
            o => o,
        }
    }
}
