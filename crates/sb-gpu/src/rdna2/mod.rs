//! Décodeur/désassembleur AMD RDNA 2 (gfx10.3, wave32 et wave64) et traducteur vers SPIR-V.
//!
//! Voir `docs/rdna2.md` pour la couverture et les limites.
#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod cfg;
pub mod decode;
pub mod disasm;
pub mod inst;
#[rustfmt::skip]
mod tables;
mod trans_alu;
mod trans_ds;
mod trans_ext;
mod trans_f16;
mod trans_f64;
mod trans_flat;
mod trans_mem;
mod trans_mimg;
mod trans_misc;
pub mod translate;

pub use decode::{Decoded, decode, decode_stream};
pub use disasm::{disassemble, disassemble_stream, format_operand};
pub use inst::{DecodeError, Encoding, Ext, Instruction, Mods, Operand, Src, WaveSize};
pub use translate::{
    BindingUse, ProgramInfo, Resource, ResourceKind, ResourceLoc, SgprInit, Stage, TranslateConfig,
    TranslateError, TranslatedProgram, VgprInit, translate_program,
};

#[cfg(test)]
#[rustfmt::skip]
mod test_vectors;
#[cfg(test)]
mod tests;
#[cfg(test)]
#[rustfmt::skip]
mod test_programs;
#[cfg(test)]
mod tests_gpu;
#[cfg(test)]
mod tests_gpu_ext;
#[cfg(test)]
mod tests_gpu_gfx;
#[cfg(test)]
mod tests_translate;
