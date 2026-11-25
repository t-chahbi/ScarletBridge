//! Types de l'ISA AMD RDNA 2 (gfx10.3) : instruction décodée, opérandes et modificateurs.
//!
//! Clean-room : structure déduite de la documentation publique AMD « RDNA 2 Instruction Set
//! Architecture Reference Guide » et de la doc LLVM AMDGPU.

/// Taille de vague (nombre de lanes). Détermine la largeur des masques (VCC, EXEC, SGPR de masque).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WaveSize {
    W32,
    W64,
}

impl WaveSize {
    /// Nombre de lanes (32 ou 64).
    pub fn lanes(self) -> u32 {
        match self {
            WaveSize::W32 => 32,
            WaveSize::W64 => 64,
        }
    }

    /// Largeur d'un masque de lanes en dwords (1 ou 2).
    pub fn mask_dwords(self) -> u8 {
        match self {
            WaveSize::W32 => 1,
            WaveSize::W64 => 2,
        }
    }
}

/// Famille d'encodage de l'instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Encoding {
    Sop1,
    Sop2,
    Sopk,
    Sopc,
    Sopp,
    Smem,
    Vop1,
    Vop2,
    Vopc,
    Vop3,
    Vop3p,
    Vintrp,
    Ds,
    Mubuf,
    Mtbuf,
    Mimg,
    Exp,
    Flat,
    Scratch,
    Global,
}

/// Source/destination décodée d'un champ d'opérande.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Src {
    /// SGPR `s<n>` (0..=105).
    Sgpr(u8),
    /// Registre temporaire de trap `ttmp<n>` (0..=15).
    Ttmp(u8),
    /// VGPR `v<n>` (0..=255).
    Vgpr(u8),
    VccLo,
    VccHi,
    M0,
    Null,
    ExecLo,
    ExecHi,
    /// Constante entière inline (−16..=64).
    InlineInt(i32),
    /// Constante flottante inline (codes 240..=248).
    InlineFloat(u8),
    /// Constante littérale 32 bits (dword suivant l'instruction).
    Literal(u32),
    Scc,
    Vccz,
    Execz,
    LdsDirect,
    /// Autre code source nommé (shared_base, private_base, ...) : code brut 0..=511.
    Special(u16),
    /// Opérande absent (`off`).
    Off,
}

impl Src {
    /// Valeur f32 d'une constante flottante inline.
    pub fn inline_float_value(code: u8) -> f32 {
        match code {
            240 => 0.5,
            241 => -0.5,
            242 => 1.0,
            243 => -1.0,
            244 => 2.0,
            245 => -2.0,
            246 => 4.0,
            247 => -4.0,
            _ => 0.159_154_94, // 248 : 1/(2π)
        }
    }

    /// Décode un code source 9 bits (SGPR, constantes, VGPR=256+n). `literal` : valeur du
    /// littéral suivant, s'il est présent.
    pub fn from_code(code: u16, literal: Option<u32>) -> Src {
        match code {
            0..=105 => Src::Sgpr(code as u8),
            106 => Src::VccLo,
            107 => Src::VccHi,
            108..=123 => Src::Ttmp((code - 108) as u8),
            124 => Src::M0,
            125 => Src::Null,
            126 => Src::ExecLo,
            127 => Src::ExecHi,
            128 => Src::InlineInt(0),
            129..=192 => Src::InlineInt(i32::from(code) - 128),
            193..=208 => Src::InlineInt(192 - i32::from(code)),
            240..=248 => Src::InlineFloat(code as u8),
            251 => Src::Vccz,
            252 => Src::Execz,
            253 => Src::Scc,
            254 => Src::LdsDirect,
            255 => Src::Literal(literal.unwrap_or(0)),
            256..=511 => Src::Vgpr((code - 256) as u8),
            other => Src::Special(other),
        }
    }

    /// Code 9 bits correspondant (inverse de [`Src::from_code`], littéral = 255).
    pub fn to_code(self) -> u16 {
        match self {
            Src::Sgpr(n) => u16::from(n),
            Src::VccLo => 106,
            Src::VccHi => 107,
            Src::Ttmp(n) => 108 + u16::from(n),
            Src::M0 => 124,
            Src::Null => 125,
            Src::ExecLo => 126,
            Src::ExecHi => 127,
            Src::InlineInt(v) if v >= 0 => 128 + v as u16,
            Src::InlineInt(v) => (192 - v) as u16,
            Src::InlineFloat(c) => u16::from(c),
            Src::Vccz => 251,
            Src::Execz => 252,
            Src::Scc => 253,
            Src::LdsDirect => 254,
            Src::Literal(_) => 255,
            Src::Vgpr(n) => 256 + u16::from(n),
            Src::Special(c) => c,
            Src::Off => 0x7D,
        }
    }
}

/// Opérande : source décodée + largeur en dwords + modificateurs de source.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Operand {
    pub src: Src,
    /// Largeur en dwords (1 pour un scalaire 32 bits, 2 pour une paire, ...).
    pub width: u8,
    pub neg: bool,
    pub abs: bool,
}

impl Operand {
    /// Crée un opérande ; les registres multi-dwords sont alignés comme le fait le matériel
    /// (paire alignée sur 2, quad et plus sur 4 ; `vcc`/`exec` en paire reviennent à leur base).
    pub fn new(src: Src, width: u8) -> Self {
        let align = match width {
            0 | 1 => 1u8,
            2 => 2,
            _ => 4,
        };
        let src = match src {
            Src::Sgpr(n) => Src::Sgpr(n & !(align - 1)),
            Src::Ttmp(n) => Src::Ttmp(n & !(align - 1)),
            Src::VccHi if width >= 2 => Src::VccLo,
            Src::ExecHi if width >= 2 => Src::ExecLo,
            other => other,
        };
        Operand {
            src,
            width,
            neg: false,
            abs: false,
        }
    }
}

/// Extension d'encodage non décodée en détail (DPP/SDWA).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Ext {
    #[default]
    None,
    Sdwa,
    Dpp16,
    Dpp8,
}

/// Modificateurs et champs immédiats (tous à 0 par défaut).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mods {
    // --- ALU ---
    pub clamp: bool,
    /// omod : 0 = aucun, 1 = ×2, 2 = ×4, 3 = ÷2.
    pub omod: u8,
    pub op_sel: u8,
    pub op_sel_hi: u8,
    pub neg_lo: u8,
    pub neg_hi: u8,
    /// simm16 (SOPK/SOPP) étendu en signe ou valeur brute selon l'encodage.
    pub imm: i32,
    pub ext: Ext,
    /// Second dword d'une extension DPP/SDWA.
    pub ext_raw: u32,
    // --- mémoire ---
    /// Offset immédiat (MUBUF/MTBUF/SMEM/FLAT/DS).
    pub offset: i32,
    pub offset0: u8,
    pub offset1: u8,
    pub glc: bool,
    pub slc: bool,
    pub dlc: bool,
    pub tfe: bool,
    pub lds: bool,
    pub gds: bool,
    pub offen: bool,
    pub idxen: bool,
    pub format: u8,
    /// SMEM : soffset utilisé à la place de l'immédiat.
    pub smem_soffset: bool,
    // --- EXP ---
    pub target: u8,
    pub en: u8,
    pub compr: bool,
    pub done: bool,
    pub vm: bool,
    // --- MIMG ---
    pub dmask: u8,
    pub dim: u8,
    pub unorm: bool,
    pub lwe: bool,
    pub d16: bool,
    pub a16: bool,
    pub r128: bool,
    pub nsa: u8,
    // --- VINTRP ---
    pub attr: u8,
    pub attr_chan: u8,
}

/// Instruction RDNA 2 décodée.
#[derive(Clone, Debug, PartialEq)]
pub struct Instruction {
    pub encoding: Encoding,
    /// Opcode dans l'espace de l'encodage (VOP3 : 10 bits, MIMG : 8 bits...).
    pub opcode: u16,
    /// Mnémonique tel qu'affiché par LLVM (`v_add_f32_e32`, `s_load_dword`...).
    pub name: &'static str,
    /// Longueur encodée en dwords (littéral, second dword, adresse NSA inclus).
    pub words: u8,
    /// Nombre d'opérandes de tête qui sont des destinations.
    pub n_dst: u8,
    /// Opérandes registre/constante dans l'ordre d'affichage (implicites VCC inclus).
    pub operands: Vec<Operand>,
    pub mods: Mods,
}

impl Instruction {
    /// Mnémonique sans suffixe de forme (`_e32`, `_e64`).
    pub fn base_name(&self) -> &'static str {
        let n = self.name;
        n.strip_suffix("_e32")
            .or_else(|| n.strip_suffix("_e64"))
            .unwrap_or(n)
    }

    /// Destination principale, s'il y en a une.
    pub fn dst(&self) -> Option<&Operand> {
        if self.n_dst > 0 {
            self.operands.first()
        } else {
            None
        }
    }

    /// Opérandes sources (hors destinations).
    pub fn srcs(&self) -> &[Operand] {
        &self.operands[usize::from(self.n_dst).min(self.operands.len())..]
    }

    /// Valeur du littéral porté par l'instruction, s'il y en a un.
    pub fn literal(&self) -> Option<u32> {
        self.operands.iter().find_map(|o| match o.src {
            Src::Literal(v) => Some(v),
            _ => None,
        })
    }

    /// Vrai pour les instructions de branchement scalaire (`s_branch`, `s_cbranch_*`).
    pub fn is_branch(&self) -> bool {
        self.encoding == Encoding::Sopp
            && (self.name == "s_branch" || self.name.starts_with("s_cbranch_"))
    }
}

/// Erreurs de décodage.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("flux d'instructions tronqué à l'offset {0}")]
    Truncated(usize),
    #[error("encodage inconnu {0:#010x}")]
    UnknownEncoding(u32),
    #[error("opcode {opcode:#x} non défini pour {encoding:?}")]
    UnknownOpcode { encoding: Encoding, opcode: u16 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn src_code_roundtrip() {
        for code in 0u16..512 {
            if (209..240).contains(&code)
                || (233..240).contains(&code)
                || code == 249
                || code == 250
            {
                continue;
            }
            let s = Src::from_code(code, Some(7));
            assert_eq!(s.to_code(), code, "code {code}");
        }
    }

    #[test]
    fn inline_constants() {
        assert_eq!(Src::from_code(128, None), Src::InlineInt(0));
        assert_eq!(Src::from_code(129, None), Src::InlineInt(1));
        assert_eq!(Src::from_code(192, None), Src::InlineInt(64));
        assert_eq!(Src::from_code(193, None), Src::InlineInt(-1));
        assert_eq!(Src::from_code(208, None), Src::InlineInt(-16));
        assert_eq!(Src::inline_float_value(242), 1.0);
        assert_eq!(Src::inline_float_value(247), -4.0);
    }

    #[test]
    fn wave_widths() {
        assert_eq!(WaveSize::W32.mask_dwords(), 1);
        assert_eq!(WaveSize::W64.lanes(), 64);
    }
}
