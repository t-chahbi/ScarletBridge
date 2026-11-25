//! Tests des vues de descripteurs écrites en mémoire matérielle (V#/T#/S#), des copies de
//! descripteurs et du (dé)tuilage des copies de ressources (`d3d12_xs`).

#![allow(clippy::unwrap_used)]

use super::*;
use crate::d3d12_views::SLOT_BYTES;
use crate::gfx10_descriptors::{
    ImageType, decode_buffer, decode_image, decode_sampler, is_image_descriptor,
};
use crate::gfx10_tiling::TileMode;

fn make_device() -> Option<*mut D3D12DeviceObject> {
    let mut dev: *mut c_void = ptr::null_mut();
    if D3D12CreateDevice(ptr::null_mut(), 0xC100, &IID_ID3D12DEVICE, &mut dev) != S_OK {
        return None; // pas de GPU Vulkan dans cet environnement
    }
    Some(dev.cast())
}

/// Crée un tas de `n` descripteurs ; renvoie l'adresse de base (handle CPU de début).
fn make_heap(n: u32) -> usize {
    let desc = D3D12DescriptorHeapDesc {
        heap_type: 0,
        num_descriptors: n,
        flags: 0,
        node_mask: 1,
    };
    let mut heap: *mut c_void = ptr::null_mut();
    assert_eq!(
        device_create_descriptor_heap(ptr::null_mut(), &desc, &IID_ID3D12DESCRIPTORHEAP, &mut heap),
        S_OK
    );
    // SAFETY: `heap` est un `D3D12DescHeapObject` créé ci-dessus.
    unsafe { (*(heap as *const D3D12DescHeapObject)).base_handle }
}

fn slot(base: usize, i: usize) -> usize {
    base + i * SLOT_BYTES
}

fn read_slot(addr: usize) -> [u8; SLOT_BYTES] {
    let mut out = [0u8; SLOT_BYTES];
    // SAFETY: `addr` est une case d'un tas créé par `make_heap` (mapping RW du bridge).
    unsafe { ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), SLOT_BYTES) };
    out
}

fn words(b: &[u8; SLOT_BYTES]) -> [u32; 8] {
    let mut w = [0u32; 8];
    for (i, c) in b.as_chunks::<4>().0.iter().enumerate() {
        w[i] = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
    }
    w
}

fn vsharp(b: &[u8; SLOT_BYTES]) -> crate::gfx10_descriptors::BufferDesc {
    let w = words(b);
    decode_buffer(&[w[0], w[1], w[2], w[3]]).expect("V# valide")
}

fn u32s(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn buffer_resource(size: u64) -> *mut D3D12ResourceObject {
    alloc_d3d12_resource(
        D3D12ResourceDesc {
            dimension: 1,
            width: size,
            height: 1,
            depth_or_array_size: 1,
            mip_levels: 1,
            ..Default::default()
        },
        false,
    )
}

fn texture_resource(w: u64, h: u32, fmt: u32) -> *mut D3D12ResourceObject {
    alloc_d3d12_resource(
        D3D12ResourceDesc {
            dimension: 3,
            width: w,
            height: h,
            depth_or_array_size: 1,
            mip_levels: 1,
            format: fmt,
            sample_desc: DxgiSampleDesc {
                count: 1,
                quality: 0,
            },
            ..Default::default()
        },
        false,
    )
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i.wrapping_mul(37) ^ (i >> 3)) as u8)
        .collect()
}

/// Remplit les cases 0..6 d'un tas : CBV, SRV 2D, UAV tampon, RTV, DSV, sampler.
fn fill_views(
    dev: *mut D3D12DeviceObject,
    base: usize,
) -> (*mut D3D12ResourceObject, *mut D3D12ResourceObject) {
    let buf = buffer_resource(4096);
    let tex = texture_resource(64, 32, 28);
    // SAFETY: ressources créées ci-dessus.
    let (b, t) = unsafe { (&*buf, &*tex) };
    let cbv = D3D12ConstantBufferViewDesc {
        buffer_location: b.fixed_addr as u64,
        size_in_bytes: 256,
        _pad: 0,
    };
    device_create_cbv(dev, &cbv, slot(base, 0));
    let mut srv = u32s(&[28, 4, 0x1688, 0, 0, 1, 0]);
    srv.extend(0.0f32.to_le_bytes());
    srv.resize(48, 0);
    device_create_srv(dev, tex, srv.as_ptr().cast(), slot(base, 1));
    let mut uav = u32s(&[0, 1, 0, 0, 16, 16, 0, 0]);
    uav.resize(48, 0);
    device_create_uav(
        dev,
        buf,
        ptr::null_mut(),
        uav.as_ptr().cast(),
        slot(base, 2),
    );
    device_create_rtv(dev, tex, ptr::null(), slot(base, 3));
    let dsv = u32s(&[28, 3, 0, 0]);
    device_create_dsv(dev, tex, dsv.as_ptr().cast(), slot(base, 4));
    let mut smp = u32s(&[0x15, 1, 2, 3]);
    smp.extend(0.0f32.to_le_bytes());
    smp.extend(u32s(&[1, 8]));
    for c in [0.0f32, 0.0, 0.0, 0.0, 0.0, 12.0] {
        smp.extend(c.to_le_bytes());
    }
    device_create_sampler(dev, smp.as_ptr().cast(), slot(base, 5));
    (buf, tex)
}

#[test]
fn views_write_hardware_words_beside_the_binding_table() {
    let _serial = crate::d3d12_xs::test_serial();
    let Some(dev) = make_device() else { return };
    let base = make_heap(8);
    let (buf, tex) = fill_views(dev, base);
    // SAFETY: ressources et périphérique valides.
    let (b, t, d) = unsafe { (&*buf, &*tex, &*dev) };

    // CBV : V# brut cohérent avec la table.
    let v = vsharp(&read_slot(slot(base, 0)));
    assert_eq!((v.address, v.num_records), (b.fixed_addr as u64, 256));
    let table = d.shared.descriptors.lock();
    assert!(matches!(
        table.get(&slot(base, 0)),
        Some(DescriptorBinding::Cbv { addr, size }) if *addr == v.address && *size == v.num_records
    ));
    // SRV : T# 2D de la texture, table sur la même ressource.
    let img = decode_image(&words(&read_slot(slot(base, 1)))).expect("T#");
    // Le T# stocke `adresse >> 8` : la base est tronquée à 256 octets.
    assert_eq!(img.address, t.fixed_addr as u64 & !0xFF);
    assert_eq!(img.image_type, ImageType::Image2D);
    assert_eq!((img.width, img.height), (64, 32));
    assert!(matches!(
        table.get(&slot(base, 1)),
        Some(DescriptorBinding::SrvTexture { resource_ptr }) if *resource_ptr == tex as usize
    ));
    // UAV tampon structuré (stride 16) : V# cohérent avec la table.
    let v = vsharp(&read_slot(slot(base, 2)));
    assert_eq!(
        (v.address, v.stride, v.num_records),
        (b.fixed_addr as u64, 16, 16)
    );
    assert!(matches!(
        table.get(&slot(base, 2)),
        Some(DescriptorBinding::UavBuffer { addr, .. }) if *addr == v.address
    ));
    drop(table);
    // RTV / DSV : T# de la sous-ressource.
    for i in [3usize, 4] {
        let w = words(&read_slot(slot(base, i)));
        assert!(is_image_descriptor(&w), "case {i}");
        assert_eq!(
            decode_image(&w).unwrap().address,
            t.fixed_addr as u64 & !0xFF
        );
    }
    // Sampler : S# GFX10 (min lod 0, max lod 12) ; la moitié haute de la case reste à zéro.
    let sraw = read_slot(slot(base, 5));
    let sw = words(&sraw);
    let s = decode_sampler(&[sw[0], sw[1], sw[2], sw[3]]);
    assert_eq!(s.max_lod, 12.0);
    assert_eq!(&sraw[16..], &[0u8; 16]);
    // Les cases non utilisées restent vierges.
    assert_eq!(read_slot(slot(base, 6)), [0u8; SLOT_BYTES]);
    assert_eq!(read_slot(slot(base, 7)), [0u8; SLOT_BYTES]);
}

#[test]
fn views_outside_a_descriptor_heap_are_not_written() {
    let _serial = crate::d3d12_xs::test_serial();
    let Some(dev) = make_device() else { return };
    let guard = [0xA5u8; 128];
    let dest = guard.as_ptr() as usize;
    let cbv = D3D12ConstantBufferViewDesc {
        buffer_location: 0x1000,
        size_in_bytes: 64,
        _pad: 0,
    };
    device_create_cbv(dev, &cbv, dest);
    assert!(guard.iter().all(|&b| b == 0xA5), "mémoire hors tas intacte");
    // La table interne reste renseignée.
    // SAFETY: périphérique valide.
    assert!(
        unsafe { &*dev }
            .shared
            .descriptors
            .lock()
            .contains_key(&dest)
    );
    // NULL / ressource invalide : aucun effet, aucun crash.
    device_create_srv(dev, ptr::null_mut(), ptr::null(), dest);
    device_create_rtv(dev, 0xFFFF_FFFFusize as *mut _, ptr::null(), dest);
    assert!(guard.iter().all(|&b| b == 0xA5));
}

#[test]
fn copy_descriptors_simple_copies_words_and_table() {
    let _serial = crate::d3d12_xs::test_serial();
    let Some(dev) = make_device() else { return };
    let base = make_heap(16);
    fill_views(dev, base);
    device_copy_descriptors_simple(dev, 6, slot(base, 8), slot(base, 0));
    // SAFETY: périphérique valide.
    let table = unsafe { &*dev }.shared.descriptors.lock();
    for i in 0..6 {
        let (s, d) = (read_slot(slot(base, i)), read_slot(slot(base, 8 + i)));
        assert_eq!(s, d, "mots identiques après copie (case {i})");
    }
    // Cohérence avec la table : CBV / SRV / UAV copiés à l'identique.
    for i in 0..3 {
        let (a, b) = (table.get(&slot(base, i)), table.get(&slot(base, 8 + i)));
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        assert!(a.is_some());
    }
    // Les mots copiés décodent comme les originaux.
    let v = vsharp(&read_slot(slot(base, 8)));
    assert_eq!(v.num_records, 256);
    let img = decode_image(&words(&read_slot(slot(base, 9)))).unwrap();
    assert_eq!(img.width, 64);
}

#[test]
fn copy_descriptors_ranges_gather_and_scatter() {
    let _serial = crate::d3d12_xs::test_serial();
    let Some(dev) = make_device() else { return };
    let base = make_heap(16);
    fill_views(dev, base);
    // 2 plages destination (cases 8..10 puis 12..14) <- 1 plage source (cases 0..4).
    let dests = [slot(base, 8), slot(base, 12)];
    let dsizes = [2u32, 2];
    let srcs = [slot(base, 0)];
    let ssizes = [4u32];
    device_copy_descriptors(
        dev,
        2,
        dests.as_ptr(),
        dsizes.as_ptr(),
        1,
        srcs.as_ptr(),
        ssizes.as_ptr(),
    );
    let expected = [(8, 0), (9, 1), (12, 2), (13, 3)];
    for (d, s) in expected {
        assert_eq!(
            read_slot(slot(base, d)),
            read_slot(slot(base, s)),
            "{d} <- {s}"
        );
    }
    assert_eq!(read_slot(slot(base, 10)), [0u8; SLOT_BYTES]);
    assert_eq!(read_slot(slot(base, 11)), [0u8; SLOT_BYTES]);
}

/// Mesure : un tas, des vues et des copies, puis relecture de toute la mémoire du tas.
#[test]
fn heap_memory_integration_views_then_copies() {
    let _serial = crate::d3d12_xs::test_serial();
    let Some(dev) = make_device() else { return };
    let n = 12usize;
    let base = make_heap(n as u32);
    fill_views(dev, base);
    device_copy_descriptors_simple(dev, 3, slot(base, 6), slot(base, 0));
    device_copy_descriptors_simple(dev, 3, slot(base, 9), slot(base, 3));
    // SAFETY: le tas fait `n × 32` octets (mapping RW du bridge).
    let heap = unsafe { std::slice::from_raw_parts(base as *const u8, n * SLOT_BYTES) };
    let cell = |i: usize| &heap[i * SLOT_BYTES..(i + 1) * SLOT_BYTES];
    for i in 0..6 {
        assert_ne!(cell(i), &[0u8; SLOT_BYTES], "case {i} écrite");
        assert_eq!(cell(i), cell(6 + i), "copie de la case {i}");
    }
    // Le type de chaque descripteur se relit depuis la mémoire.
    let w = |i: usize| {
        let mut a = [0u8; SLOT_BYTES];
        a.copy_from_slice(cell(i));
        words(&a)
    };
    assert!(!is_image_descriptor(&w(0)), "CBV = V#");
    assert!(is_image_descriptor(&w(1)), "SRV = T#");
    assert!(is_image_descriptor(&w(3)), "RTV = T#");
    assert!(is_image_descriptor(&w(7)), "copie du SRV = T#");
}

// ==================== Tuilage ====================

fn tiled_texture(mode: TileMode, w: u64, h: u32, fmt: u32) -> *mut D3D12ResourceObject {
    let tex = texture_resource(w, h, fmt);
    // SAFETY: ressource fraîchement créée, sans accès concurrent.
    unsafe { mark_resource_tiled(tex, mode) };
    tex
}

#[test]
fn unmarked_resources_keep_the_linear_behaviour() {
    let src = buffer_resource(64 * 64 * 4);
    let dst = texture_resource(64, 64, 28);
    // SAFETY: ressources valides.
    let (s, d) = unsafe { (&*src, &*dst) };
    assert!(s.swizzle_mode.is_none() && d.swizzle_mode.is_none());
    assert!(!copy_buffer_region_tiled(d, 0, s, 0, 128));
    assert!(detile_from_resource(d).is_none());
    assert!(!tile_into_resource(d, &[0u8; 64 * 64 * 4]));
}

#[test]
fn copy_buffer_region_tiles_into_and_detiles_out_of_a_marked_texture() {
    let (w, h) = (64u64, 64u32); // alignée sur les tuiles 32x32 RGBA8 de Sw4KbS : tiled == linear
    let linear = pattern(64 * 64 * 4);
    let buf = buffer_resource(linear.len() as u64);
    let tex = tiled_texture(TileMode::Sw4KbS, w, h, 28);
    // SAFETY: ressources valides.
    let (b, t) = unsafe { (&*buf, &*tex) };
    b.backing.lock()[..linear.len()].copy_from_slice(&linear);

    // Tampon -> texture tuilée : la mémoire est tuilée, le détuilage restitue l'image.
    assert!(copy_buffer_region_tiled(t, 0, b, 0, linear.len()));
    assert_ne!(
        &t.backing.lock()[..linear.len()],
        &linear[..],
        "stockage tuilé"
    );
    assert_eq!(detile_from_resource(t).unwrap(), linear);

    // Texture tuilée -> tampon : retrouve l'image linéaire.
    let out = buffer_resource(linear.len() as u64);
    // SAFETY: ressource valide.
    let o = unsafe { &*out };
    assert!(copy_buffer_region_tiled(o, 0, t, 0, linear.len()));
    assert_eq!(&o.backing.lock()[..linear.len()], &linear[..]);

    // Région partielle vers la texture : fusionnée dans l'image linéaire.
    let patch = [0xEEu8; 40];
    b.backing.lock()[1000..1040].copy_from_slice(&patch);
    assert!(copy_buffer_region_tiled(t, 500, b, 1000, 40));
    let mut expect = linear.clone();
    expect[500..540].copy_from_slice(&patch);
    assert_eq!(detile_from_resource(t).unwrap(), expect);

    // Deux ressources tuilées : copie brute historique (pas de traitement ici).
    let tex2 = tiled_texture(TileMode::Sw4KbS, w, h, 28);
    // SAFETY: ressource valide.
    assert!(!copy_buffer_region_tiled(unsafe { &*tex2 }, 0, t, 0, 64));
}

#[test]
fn tiled_allocation_too_small_falls_back_to_linear() {
    // 37x29 RGBA8 : la surface tuilée (2 tuiles de 4 Kio) dépasse l'allocation linéaire.
    let tex = tiled_texture(TileMode::Sw4KbS, 37, 29, 28);
    // SAFETY: ressource valide.
    let t = unsafe { &*tex };
    assert!(t.byte_len < 8192);
    assert!(!tile_into_resource(t, &pattern(37 * 29 * 4)));
    assert!(detile_from_resource(t).is_none());
}

fn location(
    res: *mut D3D12ResourceObject,
    copy_type: u32,
    offset: u64,
    w: u32,
    h: u32,
    fmt: u32,
    pitch: u32,
) -> D3D12TextureCopyLocation {
    D3D12TextureCopyLocation {
        resource: res.cast(),
        copy_type,
        _pad: 0,
        placed_footprint: D3D12PlacedSubresourceFootprint {
            offset,
            footprint: D3D12SubresourceFootprint {
                format: fmt,
                width: w,
                height: h,
                depth: 1,
                row_pitch: pitch,
            },
        },
    }
}

fn new_cmdlist(dev: *mut D3D12DeviceObject) -> *mut D3D12CmdListObject {
    let mut cl: *mut c_void = ptr::null_mut();
    assert_eq!(
        device_create_command_list(
            dev,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &IID_ID3D12GRAPHICSCOMMANDLIST,
            &mut cl
        ),
        S_OK
    );
    cl.cast()
}

#[test]
fn copy_texture_region_tiles_uploads_and_detiles_readbacks() {
    let _serial = crate::d3d12_xs::test_serial();
    let Some(dev) = make_device() else { return };
    let cl = new_cmdlist(dev);
    let (w, h) = (64u32, 64u32);
    let pitch = 64 * 4 + 64; // pitch de l'empreinte > largeur
    let mut upload = vec![0u8; pitch as usize * h as usize];
    let mut linear = Vec::new();
    for y in 0..h as usize {
        let row = pattern(64 * 4 + y);
        upload[y * pitch as usize..y * pitch as usize + 256].copy_from_slice(&row[y..y + 256]);
        linear.extend_from_slice(&row[y..y + 256]);
    }
    let buf = buffer_resource(upload.len() as u64);
    let tex = tiled_texture(TileMode::Sw4KbS, u64::from(w), h, 28);
    // SAFETY: ressources valides.
    let (b, t) = unsafe { (&*buf, &*tex) };
    b.backing.lock()[..upload.len()].copy_from_slice(&upload);

    // Upload : buffer (empreinte placée) -> texture tuilée.
    let dst = location(tex, 0, 0, 0, 0, 0, 0);
    let src = location(buf, 1, 0, w, h, 28, pitch);
    cmdlist_copy_texture_region(cl, &dst, 0, 0, 0, &src, ptr::null());
    assert_ne!(
        &t.backing.lock()[..linear.len()],
        &linear[..],
        "mémoire tuilée"
    );
    assert_eq!(detile_from_resource(t).unwrap(), linear);
    // SAFETY: liste valide ; l'upload GPU reçoit l'image linéaire.
    let ops = unsafe { &*cl }.ops.lock();
    assert!(matches!(ops.last(), Some(RecordedOp::UploadTexture { data, .. }) if *data == linear));
    drop(ops);

    // Readback : texture tuilée -> tampon (empreinte placée, offset 256, pitch 320).
    let rb = buffer_resource(256 + u64::from(pitch) * u64::from(h));
    // SAFETY: ressource valide.
    let r = unsafe { &*rb };
    let dst = location(rb, 1, 256, w, h, 28, pitch);
    let src = location(tex, 0, 0, 0, 0, 0, 0);
    cmdlist_copy_texture_region(cl, &dst, 0, 0, 0, &src, ptr::null());
    let mem = r.backing.lock();
    for y in 0..h as usize {
        let o = 256 + y * pitch as usize;
        assert_eq!(
            &mem[o..o + 256],
            &linear[y * 256..(y + 1) * 256],
            "ligne {y}"
        );
    }
}

#[test]
fn copy_texture_region_on_unmarked_texture_stays_linear() {
    let _serial = crate::d3d12_xs::test_serial();
    let Some(dev) = make_device() else { return };
    let cl = new_cmdlist(dev);
    let data = pattern(16 * 16 * 4);
    let buf = buffer_resource(data.len() as u64);
    let tex = texture_resource(16, 16, 28);
    // SAFETY: ressources valides.
    let (b, t) = unsafe { (&*buf, &*tex) };
    b.backing.lock()[..data.len()].copy_from_slice(&data);
    let dst = location(tex, 0, 0, 0, 0, 0, 0);
    let src = location(buf, 1, 0, 16, 16, 28, 64);
    cmdlist_copy_texture_region(cl, &dst, 0, 0, 0, &src, ptr::null());
    assert_eq!(&t.backing.lock()[..data.len()], &data[..]);
}
