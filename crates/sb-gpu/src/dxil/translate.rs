//! Pilote de traduction DXIL -> SPIR-V : conteneur, module, métadonnées, structuration, assemblage.

use super::bitcode::parse_bitcode;
use super::cfg::structurize;
use super::dxmeta::{EntryInfo, SigElem, parse_entry};
use super::module::parse_module;
use super::tr::{Tr, block_terms};
use crate::dxbc_to_spirv::{ShaderTranslateError, SignatureElement, TranslatedShader};
use crate::shader::ShaderStage;

fn stage_of_kind(kind: u32) -> ShaderStage {
    match kind {
        0 => ShaderStage::Pixel,
        1 => ShaderStage::Vertex,
        2 => ShaderStage::Geometry,
        3 => ShaderStage::Hull,
        4 => ShaderStage::Domain,
        5 => ShaderStage::Compute,
        6..=14 => ShaderStage::LibraryOrMesh,
        o => ShaderStage::Unknown(o as u16),
    }
}

fn d3d_name(kind: u32) -> u32 {
    match kind {
        1 => 6,
        2 => 8,
        3 => 1,
        4 => 4,
        5 => 5,
        6 => 2,
        7 => 3,
        10 => 7,
        12 => 10,
        13 => 9,
        14 => 66,
        16 => 64,
        17 => 65,
        18 => 68,
        19 => 67,
        20 => 69,
        _ => 0,
    }
}

fn to_sig(e: &SigElem) -> SignatureElement {
    let comp = match e.comp_type {
        3 | 5 | 7 => 1,
        2 | 4 | 6 => 2,
        _ => 3,
    };
    SignatureElement {
        semantic_name: e.name.clone(),
        semantic_index: e.sem_index.first().copied().unwrap_or(0),
        system_value_type: d3d_name(e.sys_kind),
        register: e.start_row.max(0) as u32,
        mask: (((1u32 << e.cols.clamp(1, 4)) - 1) << e.start_col.min(3)) as u8 & 0xF,
        component_type: comp,
    }
}

pub(super) fn run(
    container: &[u8],
    hint: ShaderStage,
) -> Result<TranslatedShader, ShaderTranslateError> {
    let loc = super::container::locate(container)?;
    for n in &loc.notes {
        sb_common::sb_warn!("dxil", "{n} ({})", loc.diag);
    }
    let bitcode = loc.bitcode;
    let blocks = parse_bitcode(bitcode)
        .map_err(|e| ShaderTranslateError::InvalidDxil(format!("{e} ({})", loc.diag)))?;
    let module = parse_module(&blocks)
        .map_err(|e| ShaderTranslateError::InvalidDxil(format!("{e} ({})", loc.diag)))?;
    let info: EntryInfo = parse_entry(&module)
        .map_err(|e| ShaderTranslateError::InvalidDxil(format!("{e} ({})", loc.diag)))?;
    let stage = stage_of_kind(info.kind);
    if matches!(
        hint,
        ShaderStage::Pixel
            | ShaderStage::Vertex
            | ShaderStage::Compute
            | ShaderStage::Geometry
            | ShaderStage::Hull
            | ShaderStage::Domain
    ) && hint != stage
    {
        return Err(ShaderTranslateError::InvalidDxil(format!(
            "stage demandé {hint:?} différent du stage du shader {stage:?}"
        )));
    }
    if !matches!(
        stage,
        ShaderStage::Vertex | ShaderStage::Pixel | ShaderStage::Compute
    ) {
        return Err(ShaderTranslateError::UnsupportedStage(stage));
    }
    let Some(f) = module
        .functions
        .get(info.func)
        .filter(|f| !f.blocks.is_empty())
    else {
        return Err(ShaderTranslateError::InvalidDxil(
            "point d'entrée sans corps".into(),
        ));
    };
    let mut tr = Tr::new(&module, f, &info, stage);
    tr.prepare();
    tr.prescan_resources();
    tr.setup_io()?;
    let main_fn = tr.s.id();
    tr.s.begin_function(main_fn);
    let var_pos = tr.s.cur_len();
    tr.init_outputs();
    let (terms, trivial) = block_terms(f);
    let st = structurize(&terms, trivial);
    if std::env::var_os("DXIL_DEBUG_AST").is_some() {
        eprintln!("dispatcher={} ast={:#?}", st.dispatcher, st.ast);
    }
    tr.use_vars = st.dispatcher;
    tr.lower(&st.ast)?;
    tr.flush_hoisted(var_pos);
    tr.s.end_function();

    let model = match stage {
        ShaderStage::Vertex => 0,
        ShaderStage::Pixel => 4,
        _ => 5,
    };
    let mut modes: Vec<Vec<u32>> = Vec::new();
    match stage {
        ShaderStage::Pixel => {
            modes.push(vec![7]);
            if !tr.io.depth_modes.is_empty() {
                modes.push(vec![12]);
                if tr.io.depth_modes.contains(&14) {
                    modes.push(vec![14]);
                } else if tr.io.depth_modes.contains(&15) {
                    modes.push(vec![15]);
                }
            }
            if info.flags & 8 != 0 {
                modes.push(vec![9]);
            }
        }
        ShaderStage::Compute => {
            let t = info.numthreads;
            modes.push(vec![17, t[0], t[1], t[2]]);
        }
        _ => {}
    }
    let iface = std::mem::take(&mut tr.io.interface);
    tr.s.set_interface(&iface);
    let spirv = tr.s.assemble(model, main_fn, &modes);
    let r = &tr.res;
    Ok(TranslatedShader {
        stage,
        spirv,
        inputs: info.inputs.iter().map(to_sig).collect(),
        outputs: info.outputs.iter().map(to_sig).collect(),
        used_cbuffers: r.used_cb.iter().copied().collect(),
        used_textures: r.used_tex.iter().copied().collect(),
        used_samplers: r.used_smp.iter().copied().collect(),
        used_storage_buffers: r.used_uav.iter().copied().collect(),
        used_srv_buffers: r.used_srv_buf.iter().copied().collect(),
        thread_group_size: if stage == ShaderStage::Compute {
            info.numthreads
        } else {
            [1, 1, 1]
        },
        descriptor_heap: r.uses_heap,
    })
}
