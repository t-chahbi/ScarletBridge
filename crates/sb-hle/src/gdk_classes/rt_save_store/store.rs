//! `XStoreImpl` (clsid `0dd112ac-7c24-448c-b92b-3960fb5bd30c`) : Microsoft Store hors ligne.
//!
//! Vtable : 3 (IUnknown) + 81 méthodes = 84 entrées (`IXStoreImpl` = slots 3-69, `IXStoreImpl2`
//! = 70, `IXStoreImpl3` = 71-72, `IXStoreImpl4` = 73-76, `IXStoreImpl5` = 77-81,
//! `IXStoreImpl6` = 82-83). Les trois slots 46-48 (inconnus dans la doc publique) et le slot 70
//! (`XStoreIsAvailabilityPurchasable`, sans signature publique) répondent `E_NOTIMPL`.
//!
//! # Réponses hors ligne
//!
//! * la licence du jeu est active et n'est pas un essai (`XStoreGameLicense`, 104 octets :
//!   SKU `char[18]` en +0x00, `isActive` en +0x12, `isTrialOwnedByThisUser`/`isDiscLicense`/`isTrial`
//!   en +0x13..+0x15, secondes d'essai en +0x18, id d'essai `char[64]` en +0x1C, expiration en +0x60) ;
//! * une licence de paquet est acquérable et valide ; `XStoreCanAcquireLicense*` répond
//!   « licenciable » (`XStoreCanAcquireLicenseResult`, 12 octets) ;
//! * aucun module complémentaire, aucun produit, aucune mise à jour de paquet ;
//! * les enregistrements de licence (`RegisterGameLicenseChanged`, `RegisterPackageLicenseLost`)
//!   rendent un jeton inerte : le rappel n'est jamais appelé ;
//! * achats, consommables, jetons de licence, identifiants de collection/d'achat, téléchargements
//!   et toute interface (UI) échouent de façon synchrone avec `E_FAIL`, sans démarrer d'opération
//!   asynchrone (journalisé une fois par slot).

use core::ffi::c_void;
use core::mem::size_of;
use std::sync::{LazyLock, OnceLock};

use sb_common::abi::{E_FAIL, E_INVALIDARG, HResult, S_OK};
use sb_common::ffi_guard;

use super::{
    AsyncPayload, CHANNEL, HandleTable, async_check, async_complete, async_peek_size, async_take,
    guid, read_cstr, singleton, warn_once, write_out,
};
use crate::com::{
    ComHeader, Guid, VtblSlot, com_add_ref, com_query_interface_list, com_release, vslot,
};
use crate::gdk_classes::ClassDef;
use crate::gdk_xs::XAsyncBlock;

const NAME: &str = "XStoreImpl";
pub(super) const CLSID: Guid = guid("0dd112ac-7c24-448c-b92b-3960fb5bd30c");
const IID_IMPL2: Guid = guid("60b09f4e-1b85-45b1-826c-169118e230e1");
const IID_IMPL3: Guid = guid("2d42fea5-e71d-4b76-97cd-c50afbb3ae5d");
const IID_IMPL4: Guid = guid("de3dbdd4-0b37-4bdb-a10e-acf3a354d06a");
const IID_IMPL5: Guid = guid("5c48dedf-0b67-4492-a4b5-6829b8e796e1");
const IID_IMPL6: Guid = guid("b09d803c-2414-4a05-82c6-66dfdc9e9a44");
const EXTRA: [Guid; 5] = [IID_IMPL2, IID_IMPL3, IID_IMPL4, IID_IMPL5, IID_IMPL6];

/// Nombre d'entrées de la vtable (IUnknown + 81 méthodes).
pub(super) const VTBL_SLOTS: usize = 84;

/// Taille de `XStoreGameLicense`.
pub(super) const GAME_LICENSE_SIZE: usize = 104;
/// Taille de `XStoreCanAcquireLicenseResult`.
const CAN_ACQUIRE_SIZE: usize = 12;
/// SKU synthétique du jeu (`char skuStoreId[18]` : 17 caractères + NUL).
const GAME_SKU_ID: &str = "9N0000000000/0010";
/// `XStoreCanLicenseStatus_Licensable`.
const STATUS_LICENSABLE: u32 = 1;

const _: () = assert!(GAME_SKU_ID.len() < 18);
const _: () = assert!(GAME_LICENSE_SIZE == 0x68);

type LicenseCb = unsafe extern "win64" fn(*mut c_void);
type ProductCb = unsafe extern "win64" fn(*const c_void, *mut c_void) -> u8;

static CONTEXTS: LazyLock<HandleTable<()>> = LazyLock::new(|| HandleTable::new(0x7A00_0010));
static QUERIES: LazyLock<HandleTable<()>> = LazyLock::new(|| HandleTable::new(0x7B00_0010));
static LICENSES: LazyLock<HandleTable<()>> = LazyLock::new(|| HandleTable::new(0x7C00_0010));
static TOKENS: LazyLock<HandleTable<()>> = LazyLock::new(|| HandleTable::new(0x7D00_0010));

fn ctx_ok(ctx: usize) -> Result<(), HResult> {
    if CONTEXTS.contains(ctx) {
        Ok(())
    } else {
        Err(E_INVALIDARG)
    }
}

fn str_ok(p: *const u8) -> Result<String, HResult> {
    read_cstr(p).ok_or(E_INVALIDARG)
}

fn finish(block: *mut XAsyncBlock, outcome: Result<(usize, AsyncPayload), HResult>) -> HResult {
    match outcome {
        Err(hr) => hr, // erreurs d'arguments : synchrones, aucun bloc en attente
        ok => async_complete(block, ok),
    }
}

/// Requête de produits vide (hors ligne : aucun produit) : un nouveau handle de requête.
fn empty_query(block: *mut XAsyncBlock, validate: impl FnOnce() -> Result<(), HResult>) -> HResult {
    if let Err(hr) = async_check(block).and_then(|()| validate()) {
        return hr;
    }
    finish(
        block,
        Ok((size_of::<usize>(), AsyncPayload::Handle(QUERIES.insert(())))),
    )
}

/// Opération asynchrone sans donnée de sortie dont le résultat est un compteur nul (`size = 0`).
fn empty_count(block: *mut XAsyncBlock, validate: impl FnOnce() -> Result<(), HResult>) -> HResult {
    if let Err(hr) = async_check(block).and_then(|()| validate()) {
        return hr;
    }
    finish(block, Ok((0, AsyncPayload::None)))
}

// ---------------------------------------------------------------------------------------------
// Slots
// ---------------------------------------------------------------------------------------------

extern "win64" fn qi(this: *mut ComHeader, riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    let mut all = vec![CLSID];
    all.extend_from_slice(&EXTRA);
    com_query_interface_list(NAME, this, riid, ppv, &all)
}

// 3
extern "win64" fn create_context(_this: *mut c_void, _user: usize, out: *mut usize) -> HResult {
    ffi_guard!(CHANNEL, "XStoreCreateContext", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        // L'utilisateur est optionnel et n'est pas déréférencé : tout handle désigne le joueur local.
        write_out(out, CONTEXTS.insert(()));
        S_OK
    })
}

// 4
extern "win64" fn close_context(_this: *mut c_void, ctx: usize) {
    ffi_guard!(CHANNEL, "XStoreCloseContextHandle", (), {
        CONTEXTS.remove(ctx);
    })
}

// 5 : (ctx, kinds, maxItems, async)
extern "win64" fn query_associated_products_async(
    _this: *mut c_void,
    ctx: usize,
    _kinds: u32,
    _max_items: u32,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreQueryAssociatedProductsAsync", E_FAIL, {
        empty_query(block, || ctx_ok(ctx))
    })
}

/// Résultat « handle de requête de produits » (slots 6, 8, 10, 12, 14, 18, 78).
extern "win64" fn products_result(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    out: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreQueryProductsResult", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, 0);
        match async_take(block, |p| matches!(p, AsyncPayload::Handle(_))) {
            Ok(rec) => {
                if let AsyncPayload::Handle(h) = rec.payload {
                    write_out(out, h);
                }
                S_OK
            }
            Err(hr) => hr,
        }
    })
}

fn read_string_list(list: *const *const u8, count: usize) -> Result<Vec<String>, HResult> {
    if list.is_null() || count == 0 {
        return Err(E_INVALIDARG);
    }
    let mut v = Vec::new();
    for i in 0..count.min(4096) {
        // SAFETY: `list` pointe vers `count` chaînes C (`_In_z_count_`).
        v.push(str_ok(unsafe { *list.add(i) })?);
    }
    Ok(v)
}

// 7 : (ctx, kinds, storeIds, count, actionFilters, filterCount, async)
extern "win64" fn query_products_async(
    _this: *mut c_void,
    ctx: usize,
    _kinds: u32,
    store_ids: *const *const u8,
    count: usize,
    filters: *const *const u8,
    filter_count: usize,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreQueryProductsAsync", E_FAIL, {
        empty_query(block, || {
            ctx_ok(ctx)?;
            read_string_list(store_ids, count)?;
            if !filters.is_null() && filter_count > 0 {
                read_string_list(filters, filter_count)?;
            }
            Ok(())
        })
    })
}

// 9 : (ctx, kinds, maxItems, async)
extern "win64" fn query_entitled_products_async(
    _this: *mut c_void,
    ctx: usize,
    _kinds: u32,
    _max_items: u32,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreQueryEntitledProductsAsync", E_FAIL, {
        empty_query(block, || ctx_ok(ctx))
    })
}

// 11
extern "win64" fn query_product_for_current_game_async(
    _this: *mut c_void,
    ctx: usize,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreQueryProductForCurrentGameAsync", E_FAIL, {
        empty_query(block, || ctx_ok(ctx))
    })
}

// 13 : (ctx, kinds, packageIdentifier, async)
extern "win64" fn query_product_for_package_async(
    _this: *mut c_void,
    ctx: usize,
    _kinds: u32,
    pkg: *const u8,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreQueryProductForPackageAsync", E_FAIL, {
        empty_query(block, || {
            ctx_ok(ctx)?;
            str_ok(pkg).map(|_| ())
        })
    })
}

// 15
extern "win64" fn enumerate_products_query(
    _this: *mut c_void,
    query: usize,
    _ctx: *mut c_void,
    cb: Option<ProductCb>,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreEnumerateProductsQuery", E_FAIL, {
        if cb.is_none() || !QUERIES.contains(query) {
            return E_INVALIDARG;
        }
        S_OK // requête vide : aucun rappel
    })
}

// 16
extern "win64" fn products_query_has_more_pages(_this: *mut c_void, _query: usize) -> u8 {
    ffi_guard!(CHANNEL, "XStoreProductsQueryHasMorePages", 0u8, { 0u8 })
}

// 17 : une requête vide n'a pas de page suivante.
extern "win64" fn products_query_next_page_async(
    _this: *mut c_void,
    _query: usize,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreProductsQueryNextPageAsync", E_FAIL, {
        let _ = async_check(block);
        E_INVALIDARG
    })
}

// 19
extern "win64" fn close_products_query(_this: *mut c_void, query: usize) {
    ffi_guard!(CHANNEL, "XStoreCloseProductsQueryHandle", (), {
        QUERIES.remove(query);
    })
}

// 20 : (productQuery, packageIdentifier, async)
extern "win64" fn acquire_license_for_package_async(
    _this: *mut c_void,
    query: usize,
    pkg: *const u8,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreAcquireLicenseForPackageAsync", E_FAIL, {
        if let Err(hr) = async_check(block) {
            return hr;
        }
        if !QUERIES.contains(query) {
            return E_INVALIDARG;
        }
        if let Err(hr) = str_ok(pkg) {
            return hr;
        }
        finish(
            block,
            Ok((
                size_of::<usize>(),
                AsyncPayload::Handle(LICENSES.insert(())),
            )),
        )
    })
}

// 21
extern "win64" fn acquire_license_for_package_result(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    out: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreAcquireLicenseForPackageResult", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, 0);
        match async_take(block, |p| matches!(p, AsyncPayload::Handle(_))) {
            Ok(rec) => {
                if let AsyncPayload::Handle(h) = rec.payload {
                    write_out(out, h);
                }
                S_OK
            }
            Err(hr) => hr,
        }
    })
}

// 22
extern "win64" fn is_license_valid(_this: *mut c_void, lic: usize) -> u8 {
    ffi_guard!(CHANNEL, "XStoreIsLicenseValid", 0u8, {
        u8::from(LICENSES.contains(lic))
    })
}

// 23
extern "win64" fn close_license(_this: *mut c_void, lic: usize) {
    ffi_guard!(CHANNEL, "XStoreCloseLicenseHandle", (), {
        LICENSES.remove(lic);
    })
}

// 24 / 26 : (ctx, storeId | packageIdentifier, async)
extern "win64" fn can_acquire_license_async(
    _this: *mut c_void,
    ctx: usize,
    id: *const u8,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreCanAcquireLicenseAsync", E_FAIL, {
        if let Err(hr) = async_check(block)
            .and_then(|()| ctx_ok(ctx))
            .and_then(|()| str_ok(id).map(|_| ()))
        {
            return hr;
        }
        finish(block, Ok((CAN_ACQUIRE_SIZE, AsyncPayload::None)))
    })
}

// 25 / 27 : écrit `XStoreCanAcquireLicenseResult` { char licensableSku[5]; uint32 status }.
extern "win64" fn can_acquire_license_result(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    out: *mut u8,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreCanAcquireLicenseResult", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        match async_take(block, |p| matches!(p, AsyncPayload::None)) {
            Ok(_) => {
                let mut raw = [0u8; CAN_ACQUIRE_SIZE];
                raw[..4].copy_from_slice(b"0010");
                raw[8..12].copy_from_slice(&STATUS_LICENSABLE.to_le_bytes());
                write_out(out.cast::<[u8; CAN_ACQUIRE_SIZE]>(), raw);
                S_OK
            }
            Err(hr) => hr,
        }
    })
}

// 28
extern "win64" fn query_game_license_async(
    _this: *mut c_void,
    ctx: usize,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreQueryGameLicenseAsync", E_FAIL, {
        if let Err(hr) = async_check(block).and_then(|()| ctx_ok(ctx)) {
            return hr;
        }
        finish(block, Ok((GAME_LICENSE_SIZE, AsyncPayload::None)))
    })
}

/// Contenu de `XStoreGameLicense` : licence active, ni essai ni disque.
pub(super) fn game_license_bytes() -> [u8; GAME_LICENSE_SIZE] {
    let mut raw = [0u8; GAME_LICENSE_SIZE];
    raw[..GAME_SKU_ID.len()].copy_from_slice(GAME_SKU_ID.as_bytes());
    raw[0x12] = 1; // isActive
    // 0x13 isTrialOwnedByThisUser, 0x14 isDiscLicense, 0x15 isTrial : faux ;
    // 0x18 trialTimeRemainingInSeconds = 0 ; 0x1C trialUniqueId = "" ; 0x60 expirationDate = 0.
    raw
}

// 29
extern "win64" fn query_game_license_result(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    out: *mut u8,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreQueryGameLicenseResult", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        match async_take(block, |p| matches!(p, AsyncPayload::None)) {
            Ok(_) => {
                write_out(out.cast::<[u8; GAME_LICENSE_SIZE]>(), game_license_bytes());
                S_OK
            }
            Err(hr) => hr,
        }
    })
}

// 30 / 55 : (ctx, async) ; résultat = compteur nul.
extern "win64" fn empty_list_async(
    _this: *mut c_void,
    ctx: usize,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreQueryAddOnLicensesAsync", E_FAIL, {
        empty_count(block, || ctx_ok(ctx))
    })
}

// 77 : (ctx, storeId, kinds, maxItems, async)
extern "win64" fn query_associated_products_for_store_id_async(
    _this: *mut c_void,
    ctx: usize,
    store_id: *const u8,
    _kinds: u32,
    _max_items: u32,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "XStoreQueryAssociatedProductsForStoreIdAsync",
        E_FAIL,
        {
            empty_query(block, || {
                ctx_ok(ctx)?;
                str_ok(store_id).map(|_| ())
            })
        }
    )
}

// 79 : (ctx, packageIdentifiers, count, async)
extern "win64" fn query_package_updates_async(
    _this: *mut c_void,
    ctx: usize,
    ids: *const *const u8,
    count: usize,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreQueryPackageUpdatesAsync", E_FAIL, {
        empty_count(block, || {
            ctx_ok(ctx)?;
            read_string_list(ids, count).map(|_| ())
        })
    })
}

// 31 / 56 / 80 : nombre de résultats (toujours 0).
extern "win64" fn empty_list_count(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    out: *mut u32,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreQueryListResultCount", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, 0);
        match async_peek_size(block) {
            Ok(_) => S_OK,
            Err(hr) => hr,
        }
    })
}

// 32 / 57 / 81 : copie de `count` éléments (aucun) ; consomme le résultat.
extern "win64" fn empty_list_result(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    count: u32,
    out: *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreQueryListResult", E_FAIL, {
        if count != 0 && out.is_null() {
            return E_INVALIDARG;
        }
        match async_take(block, |p| matches!(p, AsyncPayload::None)) {
            Ok(_) if count != 0 => E_INVALIDARG, // le compteur annoncé par `…ResultCount` est 0
            Ok(_) => S_OK,
            Err(hr) => hr,
        }
    })
}

/// Slot dont l'opération est indisponible hors ligne : échec synchrone `E_FAIL`, journalisé.
extern "win64" fn offline_fail<const N: usize>(
    _this: *mut c_void,
    _a2: usize,
    _a3: usize,
    _a4: usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XStore(offline)", E_FAIL, {
        warn_once(NAME, N, "indisponible hors ligne");
        E_FAIL
    })
}

fn register_token(live: bool, cb: Option<LicenseCb>, token: *mut u64) -> HResult {
    if !live || cb.is_none() || token.is_null() {
        return E_INVALIDARG;
    }
    write_out(token, TOKENS.insert(()) as u64);
    S_OK
}

// 66 : (ctx, queue, context, callback, &token)
extern "win64" fn register_game_license_changed(
    _this: *mut c_void,
    ctx: usize,
    _queue: usize,
    _context: *mut c_void,
    cb: Option<LicenseCb>,
    token: *mut u64,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreRegisterGameLicenseChanged", E_FAIL, {
        register_token(CONTEXTS.contains(ctx), cb, token)
    })
}

// 67 / 69 : (handle, token, wait) -> BOOLEAN ; jeton inerte : jamais en attente.
extern "win64" fn unregister_token(
    _this: *mut c_void,
    _handle: usize,
    token: u64,
    _wait: u8,
) -> u8 {
    ffi_guard!(CHANNEL, "XStoreUnregister", 0u8, {
        u8::from(TOKENS.remove(token as usize).is_some())
    })
}

// 68 : (license, queue, context, callback, &token)
extern "win64" fn register_package_license_lost(
    _this: *mut c_void,
    lic: usize,
    _queue: usize,
    _context: *mut c_void,
    cb: Option<LicenseCb>,
    token: *mut u64,
) -> HResult {
    ffi_guard!(CHANNEL, "XStoreRegisterPackageLicenseLost", E_FAIL, {
        register_token(LICENSES.contains(lic), cb, token)
    })
}

macro_rules! fail {
    ($n:literal) => {
        vslot(offline_fail::<$n> as *const c_void)
    };
}

static VTBL: [VtblSlot; VTBL_SLOTS] = [
    vslot(qi as *const c_void),
    vslot(com_add_ref as *const c_void),
    vslot(com_release as *const c_void),
    vslot(create_context as *const c_void), // 3
    vslot(close_context as *const c_void),  // 4
    vslot(query_associated_products_async as *const c_void), // 5
    vslot(products_result as *const c_void), // 6
    vslot(query_products_async as *const c_void), // 7
    vslot(products_result as *const c_void), // 8
    vslot(query_entitled_products_async as *const c_void), // 9
    vslot(products_result as *const c_void), // 10
    vslot(query_product_for_current_game_async as *const c_void), // 11
    vslot(products_result as *const c_void), // 12
    vslot(query_product_for_package_async as *const c_void), // 13
    vslot(products_result as *const c_void), // 14
    vslot(enumerate_products_query as *const c_void), // 15
    vslot(products_query_has_more_pages as *const c_void), // 16
    vslot(products_query_next_page_async as *const c_void), // 17
    vslot(products_result as *const c_void), // 18
    vslot(close_products_query as *const c_void), // 19
    vslot(acquire_license_for_package_async as *const c_void), // 20
    vslot(acquire_license_for_package_result as *const c_void), // 21
    vslot(is_license_valid as *const c_void), // 22
    vslot(close_license as *const c_void),  // 23
    vslot(can_acquire_license_async as *const c_void), // 24
    vslot(can_acquire_license_result as *const c_void), // 25
    vslot(can_acquire_license_async as *const c_void), // 26
    vslot(can_acquire_license_result as *const c_void), // 27
    vslot(query_game_license_async as *const c_void), // 28
    vslot(query_game_license_result as *const c_void), // 29
    vslot(empty_list_async as *const c_void), // 30
    vslot(empty_list_count as *const c_void), // 31
    vslot(empty_list_result as *const c_void), // 32
    fail!(33),
    fail!(34),
    fail!(35),
    fail!(36),
    fail!(37),
    fail!(38),
    fail!(39),
    fail!(40),
    fail!(41),
    fail!(42),
    fail!(43),
    fail!(44),
    fail!(45),
    unimpl!(46),
    unimpl!(47),
    unimpl!(48),
    fail!(49),
    fail!(50),
    fail!(51),
    fail!(52),
    fail!(53),
    fail!(54),
    vslot(empty_list_async as *const c_void),  // 55
    vslot(empty_list_count as *const c_void),  // 56
    vslot(empty_list_result as *const c_void), // 57
    fail!(58),
    fail!(59),
    fail!(60),
    fail!(61),
    fail!(62),
    fail!(63),
    fail!(64),
    fail!(65),
    vslot(register_game_license_changed as *const c_void), // 66
    vslot(unregister_token as *const c_void),              // 67
    vslot(register_package_license_lost as *const c_void), // 68
    vslot(unregister_token as *const c_void),              // 69
    unimpl!(70),
    fail!(71),
    fail!(72),
    fail!(73),
    fail!(74),
    fail!(75),
    fail!(76),
    vslot(query_associated_products_for_store_id_async as *const c_void), // 77
    vslot(products_result as *const c_void),                              // 78
    vslot(query_package_updates_async as *const c_void),                  // 79
    vslot(empty_list_count as *const c_void),                             // 80
    vslot(empty_list_result as *const c_void),                            // 81
    fail!(82),
    fail!(83),
];

const _: () = assert!(VTBL.len() == VTBL_SLOTS);

fn create() -> *mut ComHeader {
    static OBJ: OnceLock<usize> = OnceLock::new();
    singleton(&OBJ, VTBL.as_ptr().cast())
}

pub(super) fn class() -> ClassDef {
    ClassDef {
        name: NAME,
        clsid: CLSID,
        extra_iids: &EXTRA,
        create,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests_support::*;
    use super::*;
    use sb_common::abi::{E_NOINTERFACE, E_NOTIMPL};
    use std::ffi::CString;

    fn blk() -> XAsyncBlock {
        XAsyncBlock {
            queue: 0,
            context: core::ptr::null_mut(),
            callback: None,
            internal: [0; 32],
        }
    }

    fn context(obj: *mut c_void) -> usize {
        let mut c = 0usize;
        assert_eq!(
            vcall!(obj, 3, (usize, *mut usize) -> HResult, 1, &mut c),
            S_OK
        );
        assert_ne!(c, 0);
        c
    }

    #[test]
    fn vtable_size_and_iunknown() {
        const _: () = assert!(VTBL_SLOTS == 84);
        let obj = query_class(&CLSID);
        assert!(vcall!(obj, 1, () -> u32) >= 2);
        assert!(vcall!(obj, 2, () -> u32) >= 1);
        for iid in [CLSID, IID_IMPL2, IID_IMPL3, IID_IMPL4, IID_IMPL5, IID_IMPL6] {
            let mut out: *mut c_void = core::ptr::null_mut();
            assert_eq!(
                vcall!(obj, 0, (*const Guid, *mut *mut c_void) -> HResult, &iid, &mut out),
                S_OK
            );
            vcall!(out, 2, () -> u32);
        }
        let bad = guid("00000000-1111-2222-3333-444444444444");
        let mut out: *mut c_void = core::ptr::null_mut();
        assert_eq!(
            vcall!(obj, 0, (*const Guid, *mut *mut c_void) -> HResult, &bad, &mut out),
            E_NOINTERFACE
        );
        assert!(out.is_null());
        assert_eq!(
            crate::gdk_classes::query(&CLSID, &bad, &mut out),
            E_NOINTERFACE
        );
    }

    #[test]
    fn context_lifecycle_and_errors() {
        let obj = query_class(&CLSID);
        assert_eq!(
            vcall!(obj, 3, (usize, *mut usize) -> HResult, 0, core::ptr::null_mut()),
            E_INVALIDARG
        );
        let mut c = 0usize;
        // utilisateur optionnel
        assert_eq!(
            vcall!(obj, 3, (usize, *mut usize) -> HResult, 0, &mut c),
            S_OK
        );
        vcall!(obj, 4, (usize) -> (), c);
        // contexte fermé : refusé
        let mut b = blk();
        assert_eq!(
            vcall!(obj, 28, (usize, *mut XAsyncBlock) -> HResult, c, &mut b),
            E_INVALIDARG
        );
        assert_eq!(b.internal[0], 0);
    }

    #[test]
    fn game_license_layout_and_values() {
        let obj = query_class(&CLSID);
        let ctx = context(obj);
        let mut b = blk();
        assert_eq!(
            vcall!(obj, 28, (usize, *mut XAsyncBlock) -> HResult, ctx, core::ptr::null_mut()),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 28, (usize, *mut XAsyncBlock) -> HResult, ctx, &mut b),
            S_OK
        );
        assert_eq!(b.internal[0], 1);
        assert_eq!(
            vcall!(obj, 29, (*mut XAsyncBlock, *mut u8) -> HResult, &mut b, core::ptr::null_mut()),
            E_INVALIDARG
        );
        let mut lic = [0xAAu8; GAME_LICENSE_SIZE];
        assert_eq!(
            vcall!(obj, 29, (*mut XAsyncBlock, *mut u8) -> HResult, &mut b, lic.as_mut_ptr()),
            S_OK
        );
        let sku = std::ffi::CStr::from_bytes_until_nul(&lic[..18]).expect("sku");
        assert_eq!(sku.to_str(), Ok(GAME_SKU_ID));
        assert_eq!(lic[0x12], 1, "isActive");
        assert_eq!(&lic[0x13..0x16], &[0, 0, 0], "pas d'essai ni disque");
        assert_eq!(lic[0x1C], 0, "id d'essai vide");
        assert_eq!(&lic[0x60..0x68], &[0u8; 8], "pas d'expiration");
        // résultat consommé
        assert_eq!(
            vcall!(obj, 29, (*mut XAsyncBlock, *mut u8) -> HResult, &mut b, lic.as_mut_ptr()),
            E_INVALIDARG
        );
    }

    #[test]
    fn products_are_empty_and_package_license_is_valid() {
        let obj = query_class(&CLSID);
        let ctx = context(obj);
        let pkg = CString::new("pkg").expect("cstr");
        let mut b = blk();
        assert_eq!(
            vcall!(obj, 13, (usize, u32, *const u8, *mut XAsyncBlock) -> HResult, ctx, 4, core::ptr::null(), &mut b),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 13, (usize, u32, *const u8, *mut XAsyncBlock) -> HResult, ctx, 4, pkg.as_ptr().cast(), &mut b),
            S_OK
        );
        let mut q = 0usize;
        assert_eq!(
            vcall!(obj, 14, (*mut XAsyncBlock, *mut usize) -> HResult, &mut b, core::ptr::null_mut()),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 14, (*mut XAsyncBlock, *mut usize) -> HResult, &mut b, &mut q),
            S_OK
        );
        assert_ne!(q, 0);

        extern "win64" fn never(_: *const c_void, _: *mut c_void) -> u8 {
            panic!("aucun produit attendu");
        }
        assert_eq!(
            vcall!(obj, 15, (usize, *mut c_void, Option<ProductCb>) -> HResult, q, core::ptr::null_mut(), Some(never as ProductCb)),
            S_OK
        );
        assert_eq!(
            vcall!(obj, 15, (usize, *mut c_void, Option<ProductCb>) -> HResult, q, core::ptr::null_mut(), None),
            E_INVALIDARG
        );
        assert_eq!(vcall!(obj, 16, (usize) -> u8, q), 0);
        assert_eq!(
            vcall!(obj, 17, (usize, *mut XAsyncBlock) -> HResult, q, &mut b),
            E_INVALIDARG
        );

        // Licence de paquet : acquérable, valide, fermable.
        let mut lb = blk();
        assert_eq!(
            vcall!(obj, 20, (usize, *const u8, *mut XAsyncBlock) -> HResult, 0xBAD, pkg.as_ptr().cast(), &mut lb),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 20, (usize, *const u8, *mut XAsyncBlock) -> HResult, q, pkg.as_ptr().cast(), &mut lb),
            S_OK
        );
        let mut lic = 0usize;
        assert_eq!(
            vcall!(obj, 21, (*mut XAsyncBlock, *mut usize) -> HResult, &mut lb, &mut lic),
            S_OK
        );
        assert_eq!(vcall!(obj, 22, (usize) -> u8, lic), 1);
        assert_eq!(vcall!(obj, 22, (usize) -> u8, 0xBAD), 0);

        // Jeton « licence perdue » inerte.
        extern "win64" fn lost(_: *mut c_void) {
            panic!("jamais appelé");
        }
        let mut tok = 0u64;
        assert_eq!(
            vcall!(obj, 68, (usize, usize, *mut c_void, Option<LicenseCb>, *mut u64) -> HResult, lic, 0, core::ptr::null_mut(), Some(lost as LicenseCb), &mut tok),
            S_OK
        );
        assert_ne!(tok, 0);
        assert_eq!(vcall!(obj, 69, (usize, u64, u8) -> u8, lic, tok, 1), 1);
        assert_eq!(vcall!(obj, 69, (usize, u64, u8) -> u8, lic, tok, 1), 0);
        assert_eq!(
            vcall!(obj, 68, (usize, usize, *mut c_void, Option<LicenseCb>, *mut u64) -> HResult, lic, 0, core::ptr::null_mut(), None, &mut tok),
            E_INVALIDARG
        );
        vcall!(obj, 23, (usize) -> (), lic);
        assert_eq!(vcall!(obj, 22, (usize) -> u8, lic), 0);
        vcall!(obj, 19, (usize) -> (), q);
        assert_eq!(
            vcall!(obj, 15, (usize, *mut c_void, Option<ProductCb>) -> HResult, q, core::ptr::null_mut(), Some(never as ProductCb)),
            E_INVALIDARG
        );
    }

    #[test]
    fn query_products_validates_arguments() {
        let obj = query_class(&CLSID);
        let ctx = context(obj);
        let id = CString::new("9ABC").expect("cstr");
        let ids = [id.as_ptr().cast::<u8>()];
        let mut b = blk();
        assert_eq!(
            vcall!(obj, 7, (usize, u32, *const *const u8, usize, *const *const u8, usize, *mut XAsyncBlock) -> HResult, ctx, 2, core::ptr::null(), 1, core::ptr::null(), 0, &mut b),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 7, (usize, u32, *const *const u8, usize, *const *const u8, usize, *mut XAsyncBlock) -> HResult, ctx, 2, ids.as_ptr(), 0, core::ptr::null(), 0, &mut b),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 7, (usize, u32, *const *const u8, usize, *const *const u8, usize, *mut XAsyncBlock) -> HResult, ctx, 2, ids.as_ptr(), 1, core::ptr::null(), 0, &mut b),
            S_OK
        );
        let mut q = 0usize;
        assert_eq!(
            vcall!(obj, 8, (*mut XAsyncBlock, *mut usize) -> HResult, &mut b, &mut q),
            S_OK
        );
        // Associés / titulaires / jeu courant.
        let mut b2 = blk();
        assert_eq!(
            vcall!(obj, 5, (usize, u32, u32, *mut XAsyncBlock) -> HResult, ctx, 1, 10, &mut b2),
            S_OK
        );
        assert_eq!(
            vcall!(obj, 6, (*mut XAsyncBlock, *mut usize) -> HResult, &mut b2, &mut q),
            S_OK
        );
        assert_eq!(
            vcall!(obj, 9, (usize, u32, u32, *mut XAsyncBlock) -> HResult, 0xBAD, 1, 10, &mut b2),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 77, (usize, *const u8, u32, u32, *mut XAsyncBlock) -> HResult, ctx, id.as_ptr().cast(), 1, 10, &mut b2),
            S_OK
        );
        assert_eq!(
            vcall!(obj, 78, (*mut XAsyncBlock, *mut usize) -> HResult, &mut b2, &mut q),
            S_OK
        );
        assert_eq!(
            vcall!(obj, 77, (usize, *const u8, u32, u32, *mut XAsyncBlock) -> HResult, ctx, core::ptr::null(), 1, 10, &mut b2),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 11, (usize, *mut XAsyncBlock) -> HResult, ctx, &mut b2),
            S_OK
        );
        assert_eq!(
            vcall!(obj, 12, (*mut XAsyncBlock, *mut usize) -> HResult, &mut b2, &mut q),
            S_OK
        );
    }

    #[test]
    fn can_acquire_license_and_empty_lists() {
        let obj = query_class(&CLSID);
        let ctx = context(obj);
        let id = CString::new("9ABC").expect("cstr");
        let mut b = blk();
        assert_eq!(
            vcall!(obj, 24, (usize, *const u8, *mut XAsyncBlock) -> HResult, ctx, core::ptr::null(), &mut b),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 24, (usize, *const u8, *mut XAsyncBlock) -> HResult, ctx, id.as_ptr().cast(), &mut b),
            S_OK
        );
        let mut res = [0xFFu8; CAN_ACQUIRE_SIZE];
        assert_eq!(
            vcall!(obj, 25, (*mut XAsyncBlock, *mut u8) -> HResult, &mut b, res.as_mut_ptr()),
            S_OK
        );
        assert_eq!(&res[..5], b"0010\0");
        assert_eq!(u32::from_le_bytes([res[8], res[9], res[10], res[11]]), 1);
        assert_eq!(
            vcall!(obj, 26, (usize, *const u8, *mut XAsyncBlock) -> HResult, ctx, id.as_ptr().cast(), &mut b),
            S_OK
        );
        assert_eq!(
            vcall!(obj, 27, (*mut XAsyncBlock, *mut u8) -> HResult, &mut b, core::ptr::null_mut()),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 27, (*mut XAsyncBlock, *mut u8) -> HResult, &mut b, res.as_mut_ptr()),
            S_OK
        );

        // Modules complémentaires et mises à jour : listes vides.
        for (first, args_ids) in [(30usize, false), (55, false), (79, true)] {
            let mut b = blk();
            let hr = if args_ids {
                let ids = [id.as_ptr().cast::<u8>()];
                vcall!(obj, first, (usize, *const *const u8, usize, *mut XAsyncBlock) -> HResult, ctx, ids.as_ptr(), 1, &mut b)
            } else {
                vcall!(obj, first, (usize, *mut XAsyncBlock) -> HResult, ctx, &mut b)
            };
            assert_eq!(hr, S_OK, "slot {first}");
            let mut n = 9u32;
            assert_eq!(
                vcall!(obj, first + 1, (*mut XAsyncBlock, *mut u32) -> HResult, &mut b, &mut n),
                S_OK
            );
            assert_eq!(n, 0);
            assert_eq!(
                vcall!(obj, first + 1, (*mut XAsyncBlock, *mut u32) -> HResult, &mut b, core::ptr::null_mut()),
                E_INVALIDARG
            );
            assert_eq!(
                vcall!(obj, first + 2, (*mut XAsyncBlock, u32, *mut c_void) -> HResult, &mut b, 0, core::ptr::null_mut()),
                S_OK
            );
            assert_eq!(
                vcall!(obj, first + 2, (*mut XAsyncBlock, u32, *mut c_void) -> HResult, &mut b, 0, core::ptr::null_mut()),
                E_INVALIDARG
            );
        }
        let mut b = blk();
        assert_eq!(
            vcall!(obj, 30, (usize, *mut XAsyncBlock) -> HResult, 0xBAD, &mut b),
            E_INVALIDARG
        );
    }

    #[test]
    fn game_license_changed_registration_is_inert() {
        let obj = query_class(&CLSID);
        let ctx = context(obj);
        extern "win64" fn changed(_: *mut c_void) {
            panic!("jamais appelé");
        }
        let mut tok = 0u64;
        assert_eq!(
            vcall!(obj, 66, (usize, usize, *mut c_void, Option<LicenseCb>, *mut u64) -> HResult, ctx, 0, core::ptr::null_mut(), Some(changed as LicenseCb), &mut tok),
            S_OK
        );
        assert_eq!(vcall!(obj, 67, (usize, u64, u8) -> u8, ctx, tok, 0), 1);
        assert_eq!(
            vcall!(obj, 66, (usize, usize, *mut c_void, Option<LicenseCb>, *mut u64) -> HResult, 0xBAD, 0, core::ptr::null_mut(), Some(changed as LicenseCb), &mut tok),
            E_INVALIDARG
        );
        assert_eq!(
            vcall!(obj, 66, (usize, usize, *mut c_void, Option<LicenseCb>, *mut u64) -> HResult, ctx, 0, core::ptr::null_mut(), Some(changed as LicenseCb), core::ptr::null_mut()),
            E_INVALIDARG
        );
    }

    #[test]
    fn unavailable_operations_fail_synchronously() {
        let obj = query_class(&CLSID);
        // achats, consommables, collections, jetons de licence, UI, téléchargements, cadeaux…
        let failing: Vec<usize> = (33..=45)
            .chain(49..=54)
            .chain(58..=65)
            .chain([71, 72, 73, 74, 75, 76, 82, 83])
            .collect();
        for n in failing {
            let mut b = blk();
            let hr =
                vcall!(obj, n, (usize, usize, usize) -> HResult, 1, 2, &mut b as *mut _ as usize);
            assert_eq!(hr, E_FAIL, "slot {n}");
            assert_eq!(
                b.internal[0], 0,
                "slot {n} ne doit pas démarrer d'opération"
            );
        }
        // slots sans signature publique
        for n in [46usize, 47, 48, 70] {
            assert_eq!(vcall!(obj, n, (usize) -> HResult, 0), E_NOTIMPL, "slot {n}");
        }
    }
}
