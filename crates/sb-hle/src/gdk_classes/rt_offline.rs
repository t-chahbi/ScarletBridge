//! Classes du runtime GDK fonctionnant sans réseau ni matériel :
//! `XGameEventImpl`, `XGameUiImpl`, `XGameStreamingImpl`, `XDisplayImpl`, `XUserDeviceImpl`,
//! `XAccessibilityImpl` (`IXAccessibilityImpl2`) et `XLauncherImpl`.
//!
//! Les signatures et l'ordre des slots suivent les IDL publics (`xgameevent`, `xgameui`,
//! `xgamestreaming`, `xdisplay`, `xuser`, `xaccessibility`). Un slot réservé (`__PADDING__`) ou
//! propre à l'hébergement PC reste `E_NOTIMPL` (journalisé).

#![allow(clippy::too_many_arguments)]

use core::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use sb_common::abi::{E_FAIL, E_INVALIDARG, HResult, S_OK};
use sb_common::{ffi_guard, sb_debug, sb_info};

use super::ClassDef;
use super::rt_user::{
    E_NOT_FOUND, USER_LOCAL_ID, complete_async, drop_token, guid, is_live_user, new_token,
    new_user_handle, qi_fn, read_cstr, singleton, take_done, write_out,
};
use crate::com::{ComHeader, Guid, VtblSlot, com_add_ref, com_release, vslot};
use crate::gdk_xs::XAsyncBlock;

const CHANNEL: &str = "gdk_offline";

/// `E_GAMESTREAMING_NOT_INITIALIZED`.
const E_GAMESTREAMING_NOT_INITIALIZED: HResult = 0x8924_5400_u32 as i32;
/// `E_GAMESTREAMING_NO_DATACENTER`.
const E_GAMESTREAMING_NO_DATACENTER: HResult = 0x8924_5403_u32 as i32;
/// `E_GAMESTREAMING_NOT_STREAMING_CONTROLLER`.
const E_GAMESTREAMING_NOT_STREAMING_CONTROLLER: HResult = 0x8924_5404_u32 as i32;

// ---------------------------------------------------------------------------------------------
// XGameEventImpl (1 méthode, slot 3)
// ---------------------------------------------------------------------------------------------

const CLSID_XGAMEEVENT: Guid = guid("bbfbdcc7-bfe7-409b-a5ca-edf054960b4d");
const EVENT_SLOT_COUNT: usize = 4;

qi_fn!(event_qi, "XGameEventImpl", &[CLSID_XGAMEEVENT]);

static FIRST_EVENT_LOGGED: AtomicBool = AtomicBool::new(false);

extern "win64" fn event_write(
    _this: *mut c_void,
    user: usize,
    service_config_id: *const u8,
    play_session_id: *const u8,
    event_name: *const u8,
    dimensions_json: *const u8,
    measurements_json: *const u8,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameEventWrite", E_FAIL, {
        if !is_live_user(user)
            || service_config_id.is_null()
            || play_session_id.is_null()
            || event_name.is_null()
        {
            return E_INVALIDARG;
        }
        let name = read_cstr(event_name).unwrap_or_default();
        if !FIRST_EVENT_LOGGED.swap(true, Ordering::AcqRel) {
            sb_info!(
                CHANNEL,
                "XGameEventWrite: premier événement « {name} » (les événements sont jetés)"
            );
        } else {
            sb_debug!(
                CHANNEL,
                "XGameEventWrite({name}, dimensions={}, mesures={})",
                read_cstr(dimensions_json).unwrap_or_default(),
                read_cstr(measurements_json).unwrap_or_default()
            );
        }
        S_OK
    })
}

static EVENT_VTBL: [VtblSlot; EVENT_SLOT_COUNT] = [
    vslot(event_qi as *const c_void),
    vslot(com_add_ref as *const c_void),
    vslot(com_release as *const c_void),
    vslot(event_write as *const c_void),
];
const _: () = assert!(EVENT_VTBL.len() == EVENT_SLOT_COUNT);

fn create_event() -> *mut ComHeader {
    static CELL: OnceLock<usize> = OnceLock::new();
    singleton(&CELL, EVENT_VTBL.as_ptr().cast())
}

// ---------------------------------------------------------------------------------------------
// XGameUiImpl (IXGameUiImpl4, slots 3..50)
// ---------------------------------------------------------------------------------------------

const CLSID_XGAMEUI: Guid = guid("dfcd4649-4ff8-4043-ba07-35d607df98b0");
const IID_XGAMEUI_1: Guid = guid("6eeaa73e-9669-43ad-a2c7-d0da4e1f50a1");
const IID_XGAMEUI_2: Guid = guid("36a03122-9ea3-4a3a-a8a4-899cfd85d7db");
const IID_XGAMEUI_3: Guid = guid("ade7eba1-2093-42ce-a544-a523a66790e0");
const IID_XGAMEUI_4: Guid = guid("eaf669df-5542-4590-99a3-8dc061f837cc");
const UI_EXTRA_IIDS: &[Guid] = &[IID_XGAMEUI_1, IID_XGAMEUI_2, IID_XGAMEUI_3, IID_XGAMEUI_4];
const UI_SLOT_COUNT: usize = 51;

qi_fn!(
    ui_qi,
    "XGameUiImpl",
    &[
        CLSID_XGAMEUI,
        IID_XGAMEUI_1,
        IID_XGAMEUI_2,
        IID_XGAMEUI_3,
        IID_XGAMEUI_4
    ]
);

fn button_label(text: *const u8) -> String {
    read_cstr(text).unwrap_or_else(|| "<aucun>".to_string())
}

extern "win64" fn ui_message_dialog_async(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    title: *const u8,
    content: *const u8,
    first: *const u8,
    second: *const u8,
    third: *const u8,
    default_button: u32,
    cancel_button: u32,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameUiShowMessageDialogAsync", E_FAIL, {
        if block.is_null()
            || title.is_null()
            || content.is_null()
            || default_button > 2
            || cancel_button > 2
        {
            return E_INVALIDARG;
        }
        sb_info!(
            CHANNEL,
            "boîte de dialogue : « {} » / « {} » [{} | {} | {}] (défaut {default_button}, annulation {cancel_button}) -> bouton par défaut",
            read_cstr(title).unwrap_or_default(),
            read_cstr(content).unwrap_or_default(),
            button_label(first),
            button_label(second),
            button_label(third)
        );
        complete_async(block, S_OK, u64::from(default_button));
        S_OK
    })
}

extern "win64" fn ui_message_dialog_result(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    out: *mut u32,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameUiShowMessageDialogResult", E_FAIL, {
        if block.is_null() || out.is_null() {
            return E_INVALIDARG;
        }
        match take_done(block) {
            Some(d) => {
                write_out(out, d.value as u32);
                d.hr
            }
            None => E_INVALIDARG,
        }
    })
}

extern "win64" fn ui_error_dialog_async(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    error_code: i32,
    context: *const u8,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameUiShowErrorDialogAsync", E_FAIL, {
        if block.is_null() {
            return E_INVALIDARG;
        }
        sb_info!(
            CHANNEL,
            "boîte d'erreur : HRESULT {:#010x}, contexte « {} » -> fermée",
            error_code as u32,
            read_cstr(context).unwrap_or_default()
        );
        complete_async(block, S_OK, 0);
        S_OK
    })
}

/// `XGameUiShowErrorDialogResult` : HRESULT enregistré.
extern "win64" fn ui_result_plain(_this: *mut c_void, block: *mut XAsyncBlock) -> HResult {
    ffi_guard!(CHANNEL, "XGameUi...Result", E_FAIL, {
        if block.is_null() {
            return E_INVALIDARG;
        }
        match take_done(block) {
            Some(d) => d.hr,
            None => E_INVALIDARG,
        }
    })
}

extern "win64" fn ui_set_notification_position(_this: *mut c_void, position: u32) -> HResult {
    if position > 5 { E_INVALIDARG } else { S_OK }
}

/// UI jamais disponible hors ligne : échoue avant tout démarrage (aucun `XAsyncBlock` en
/// attente). Le premier argument des `...Async` est le bloc.
extern "win64" fn ui_unavailable_async(_this: *mut c_void, block: *mut XAsyncBlock) -> HResult {
    ffi_guard!(CHANNEL, "XGameUi...Async", E_FAIL, {
        if block.is_null() {
            return E_INVALIDARG;
        }
        sb_info!(CHANNEL, "UI système non disponible -> E_FAIL");
        E_FAIL
    })
}

/// `...Result` / `...ResultSize` d'une UI qui n'a jamais démarré.
extern "win64" fn ui_no_operation(_this: *mut c_void, _block: *mut XAsyncBlock) -> HResult {
    E_INVALIDARG
}

extern "win64" fn ui_text_entry_open(
    _this: *mut c_void,
    options: *const c_void,
    _max_length: u32,
    _initial_text: *const u8,
    _initial_cursor: u32,
    handle: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameUiTextEntryOpen", E_FAIL, {
        if options.is_null() || handle.is_null() {
            return E_INVALIDARG;
        }
        write_out(handle, 0usize);
        E_FAIL
    })
}

/// Fonctions `XGameUiTextEntry*` d'un handle jamais délivré.
extern "win64" fn ui_text_entry_bad_handle(_this: *mut c_void, _handle: usize) -> HResult {
    E_INVALIDARG
}

static UI_VTBL: [VtblSlot; UI_SLOT_COUNT] = [
    vslot(ui_qi as *const c_void),
    vslot(com_add_ref as *const c_void),
    vslot(com_release as *const c_void),
    vslot(ui_message_dialog_async as *const c_void), // 3
    vslot(ui_message_dialog_result as *const c_void), // 4
    vslot(ui_unavailable_async as *const c_void),    // 5 SendGameInvite
    vslot(ui_no_operation as *const c_void),         // 6
    vslot(ui_unavailable_async as *const c_void),    // 7 PlayerProfileCard
    vslot(ui_no_operation as *const c_void),         // 8
    vslot(ui_unavailable_async as *const c_void),    // 9 Achievements
    vslot(ui_no_operation as *const c_void),         // 10
    vslot(ui_unavailable_async as *const c_void),    // 11 PlayerPicker
    vslot(ui_no_operation as *const c_void),         // 12 ResultCount
    vslot(ui_no_operation as *const c_void),         // 13 Result
    vslot(ui_error_dialog_async as *const c_void),   // 14
    vslot(ui_result_plain as *const c_void),         // 15
    vslot(ui_set_notification_position as *const c_void), // 16
    vslot(ui_unavailable_async as *const c_void),    // 17 TextEntry
    vslot(ui_no_operation as *const c_void),         // 18 ResultSize
    vslot(ui_no_operation as *const c_void),         // 19 Result
    unimpl!(20),
    unimpl!(21),
    unimpl!(22),
    unimpl!(23),
    vslot(ui_unavailable_async as *const c_void), // 24 WebAuthentication
    vslot(ui_no_operation as *const c_void),      // 25
    vslot(ui_no_operation as *const c_void),      // 26
    vslot(ui_unavailable_async as *const c_void), // 27 WebAuthenticationWithOptions
    unimpl!(28),
    unimpl!(29),
    vslot(ui_unavailable_async as *const c_void), // 30 MultiplayerActivityGameInvite (IXGameUiImpl2)
    vslot(ui_no_operation as *const c_void),      // 31
    unimpl!(32),
    unimpl!(33),
    vslot(ui_text_entry_open as *const c_void),       // 34
    vslot(ui_text_entry_bad_handle as *const c_void), // 35 Close
    vslot(ui_text_entry_bad_handle as *const c_void), // 36 GetState
    vslot(ui_text_entry_bad_handle as *const c_void), // 37 GetExtents
    vslot(ui_text_entry_bad_handle as *const c_void), // 38 UpdatePositionHint
    vslot(ui_text_entry_bad_handle as *const c_void), // 39 UpdateVisibility
    vslot(ui_unavailable_async as *const c_void),     // 40 StateShare (IXGameUiImpl3)
    vslot(ui_no_operation as *const c_void),          // 41
    unimpl!(42), // IXGameUiImpl4 : rappels d'UI hébergée par le PC, sans objet ici
    unimpl!(43),
    unimpl!(44),
    unimpl!(45),
    unimpl!(46),
    unimpl!(47),
    unimpl!(48),
    unimpl!(49),
    unimpl!(50),
];
const _: () = assert!(UI_VTBL.len() == UI_SLOT_COUNT);

fn create_ui() -> *mut ComHeader {
    static CELL: OnceLock<usize> = OnceLock::new();
    singleton(&CELL, UI_VTBL.as_ptr().cast())
}

// ---------------------------------------------------------------------------------------------
// XGameStreamingImpl (IXGameStreamingImpl3, slots 3..34)
// ---------------------------------------------------------------------------------------------

const CLSID_XGAMESTREAMING: Guid = guid("0a2192aa-b2d5-4d58-83be-383b6d80799e");
const IID_XGAMESTREAMING_1: Guid = guid("8aff07f5-a1bf-4db8-80a5-31cca0de51b7");
const IID_XGAMESTREAMING_2: Guid = guid("5f5e5169-746c-4001-ad1c-da728d01c9eb");
const IID_XGAMESTREAMING_3: Guid = guid("57786622-6605-46d0-b917-0f22bbcd9c52");
const GS_SLOT_COUNT: usize = 35;

/// `XGameStreamingDisplayDetails` : 2 × u32, RECT (4 × i32), 4 × u32 = 40 octets.
const GS_DISPLAY_DETAILS_SIZE: usize = 40;

qi_fn!(
    gs_qi,
    "XGameStreamingImpl",
    &[
        CLSID_XGAMESTREAMING,
        IID_XGAMESTREAMING_1,
        IID_XGAMESTREAMING_2,
        IID_XGAMESTREAMING_3
    ]
);

extern "win64" fn gs_initialize(_this: *mut c_void) -> HResult {
    S_OK
}

extern "win64" fn gs_uninitialize(_this: *mut c_void) {}

extern "win64" fn gs_is_streaming(_this: *mut c_void) -> u8 {
    0
}

extern "win64" fn gs_register_client_properties(
    _this: *mut c_void,
    _client: u64,
    _queue: usize,
    _context: *mut c_void,
    callback: usize,
    token: *mut u64,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "XGameStreamingRegisterClientPropertiesChanged",
        E_FAIL,
        {
            if callback == 0 || token.is_null() {
                return E_INVALIDARG;
            }
            write_out(token, 0u64);
            E_GAMESTREAMING_NOT_INITIALIZED
        }
    )
}

extern "win64" fn gs_unregister_client_properties(
    _this: *mut c_void,
    _client: u64,
    _token: u64,
    _wait: u8,
) -> u8 {
    1
}

extern "win64" fn gs_physical_dimensions(
    _this: *mut c_void,
    _client: u64,
    horizontal: *mut u32,
    vertical: *mut u32,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "XGameStreamingGetStreamPhysicalDimensions",
        E_FAIL,
        {
            if horizontal.is_null() || vertical.is_null() {
                return E_INVALIDARG;
            }
            write_out(horizontal, 0u32);
            write_out(vertical, 0u32);
            E_GAMESTREAMING_NOT_INITIALIZED
        }
    )
}

extern "win64" fn gs_client_count(_this: *mut c_void) -> u32 {
    0
}

extern "win64" fn gs_get_clients(
    _this: *mut c_void,
    count: u32,
    clients: *mut u64,
    used: *mut u32,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameStreamingGetClients", E_FAIL, {
        if count != 0 && clients.is_null() {
            return E_INVALIDARG;
        }
        write_out(used, 0u32);
        S_OK
    })
}

extern "win64" fn gs_connection_state(_this: *mut c_void, _client: u64) -> u32 {
    0 // XGameStreamingConnectionState::Disconnected
}

extern "win64" fn gs_register_connection_state(
    _this: *mut c_void,
    _queue: usize,
    _context: *mut c_void,
    callback: usize,
    token: *mut u64,
) -> HResult {
    ffi_guard!(
        CHANNEL,
        "XGameStreamingRegisterConnectionStateChanged",
        E_FAIL,
        {
            if callback == 0 || token.is_null() {
                return E_INVALIDARG;
            }
            write_out(token, new_token()); // jeton inerte : jamais en streaming
            S_OK
        }
    )
}

extern "win64" fn gs_unregister_connection_state(_this: *mut c_void, token: u64, _wait: u8) -> u8 {
    drop_token(token);
    1
}

extern "win64" fn gs_added_latency(
    _this: *mut c_void,
    _client: u64,
    input: *mut u32,
    output: *mut u32,
    deviation: *mut u32,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameStreamingGetStreamAddedLatency", E_FAIL, {
        if input.is_null() || output.is_null() || deviation.is_null() {
            return E_INVALIDARG;
        }
        write_out(input, 0u32);
        write_out(output, 0u32);
        write_out(deviation, 0u32);
        E_GAMESTREAMING_NOT_INITIALIZED
    })
}

extern "win64" fn gs_server_location_name_size(_this: *mut c_void) -> usize {
    0
}

extern "win64" fn gs_server_location_name(
    _this: *mut c_void,
    size: usize,
    name: *mut u8,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameStreamingGetServerLocationName", E_FAIL, {
        if name.is_null() && size != 0 {
            return E_INVALIDARG;
        }
        if size != 0 {
            write_out(name, 0u8);
        }
        E_GAMESTREAMING_NO_DATACENTER
    })
}

/// Fonctions `void` de contrôle tactile : sans effet (jamais de streaming).
extern "win64" fn gs_void_noop(_this: *mut c_void) {}

extern "win64" fn gs_touch_input_enabled(
    _this: *mut c_void,
    _client: u64,
    out: *mut u8,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameStreamingIsTouchInputEnabled", E_FAIL, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, 0u8);
        E_GAMESTREAMING_NOT_INITIALIZED
    })
}

extern "win64" fn gs_last_frame_displayed(
    _this: *mut c_void,
    _client: u64,
    token: *mut u64,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameStreamingGetLastFrameDisplayed", E_FAIL, {
        if token.is_null() {
            return E_INVALIDARG;
        }
        write_out(token, 0u64);
        E_GAMESTREAMING_NOT_INITIALIZED
    })
}

/// `XGameStreamingGetAssociatedFrame` / `XGameStreamingGetGamepadPhysicality` : le gamepad lu
/// n'est jamais celui d'un client de streaming.
extern "win64" fn gs_gamepad_reading_query(
    _this: *mut c_void,
    reading: *mut c_void,
    out: *mut u64,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameStreamingGetGamepad...", E_FAIL, {
        if reading.is_null() || out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, 0u64);
        E_GAMESTREAMING_NOT_STREAMING_CONTROLLER
    })
}

extern "win64" fn gs_update_touch_state(
    _this: *mut c_void,
    count: usize,
    operations: *const c_void,
) -> HResult {
    if count != 0 && operations.is_null() {
        E_INVALIDARG
    } else {
        E_GAMESTREAMING_NOT_INITIALIZED
    }
}

extern "win64" fn gs_update_touch_state_client(
    _this: *mut c_void,
    _client: u64,
    count: usize,
    operations: *const c_void,
) -> HResult {
    if count != 0 && operations.is_null() {
        E_INVALIDARG
    } else {
        E_GAMESTREAMING_NOT_INITIALIZED
    }
}

extern "win64" fn gs_show_touch_with_state(
    _this: *mut c_void,
    _layout: *const u8,
    count: usize,
    operations: *const c_void,
) -> HResult {
    if count != 0 && operations.is_null() {
        E_INVALIDARG
    } else {
        E_GAMESTREAMING_NOT_INITIALIZED
    }
}

extern "win64" fn gs_show_touch_with_state_client(
    _this: *mut c_void,
    _client: u64,
    _layout: *const u8,
    count: usize,
    operations: *const c_void,
) -> HResult {
    if count != 0 && operations.is_null() {
        E_INVALIDARG
    } else {
        E_GAMESTREAMING_NOT_INITIALIZED
    }
}

extern "win64" fn gs_bundle_version_name_size(_this: *mut c_void, _client: u64) -> usize {
    0
}

extern "win64" fn gs_bundle_version(
    _this: *mut c_void,
    _client: u64,
    version: *mut u8,
    name_size: usize,
    name: *mut u8,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameStreamingGetTouchBundleVersion", E_FAIL, {
        if name_size != 0 && name.is_null() {
            return E_INVALIDARG;
        }
        write_out(version.cast::<[u8; 8]>(), [0u8; 8]); // XVersion = 4 × u16
        if name_size != 0 {
            write_out(name, 0u8);
        }
        E_GAMESTREAMING_NOT_INITIALIZED
    })
}

extern "win64" fn gs_client_ip(
    _this: *mut c_void,
    _client: u64,
    size: usize,
    buffer: *mut u8,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameStreamingGetClientIPAddress", E_FAIL, {
        if buffer.is_null() && size != 0 {
            return E_INVALIDARG;
        }
        if size != 0 {
            write_out(buffer, 0u8);
        }
        E_GAMESTREAMING_NOT_INITIALIZED
    })
}

extern "win64" fn gs_session_id(
    _this: *mut c_void,
    _client: u64,
    size: usize,
    buffer: *mut u8,
    used: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameStreamingGetSessionId", E_FAIL, {
        if buffer.is_null() && size != 0 {
            return E_INVALIDARG;
        }
        if size != 0 {
            write_out(buffer, 0u8);
        }
        write_out(used, 0usize);
        E_GAMESTREAMING_NOT_INITIALIZED
    })
}

extern "win64" fn gs_display_details(
    _this: *mut c_void,
    _client: u64,
    _max_supported_pixels: u32,
    _widest_aspect_ratio: f32,
    _tallest_aspect_ratio: f32,
    details: *mut u8,
) -> HResult {
    ffi_guard!(CHANNEL, "XGameStreamingGetDisplayDetails", E_FAIL, {
        if details.is_null() {
            return E_INVALIDARG;
        }
        write_out(
            details.cast::<[u8; GS_DISPLAY_DETAILS_SIZE]>(),
            [0u8; GS_DISPLAY_DETAILS_SIZE],
        );
        E_GAMESTREAMING_NOT_INITIALIZED
    })
}

extern "win64" fn gs_set_resolution(_this: *mut c_void, _width: u32, _height: u32) -> HResult {
    E_GAMESTREAMING_NOT_INITIALIZED
}

static GS_VTBL: [VtblSlot; GS_SLOT_COUNT] = [
    vslot(gs_qi as *const c_void),
    vslot(com_add_ref as *const c_void),
    vslot(com_release as *const c_void),
    vslot(gs_initialize as *const c_void),                 // 3
    vslot(gs_uninitialize as *const c_void),               // 4
    vslot(gs_is_streaming as *const c_void),               // 5
    vslot(gs_register_client_properties as *const c_void), // 6
    vslot(gs_unregister_client_properties as *const c_void), // 7
    vslot(gs_physical_dimensions as *const c_void),        // 8
    vslot(gs_client_count as *const c_void),               // 9
    vslot(gs_get_clients as *const c_void),                // 10
    vslot(gs_connection_state as *const c_void),           // 11
    vslot(gs_register_connection_state as *const c_void),  // 12
    vslot(gs_unregister_connection_state as *const c_void), // 13
    vslot(gs_added_latency as *const c_void),              // 14
    vslot(gs_server_location_name_size as *const c_void),  // 15
    vslot(gs_server_location_name as *const c_void),       // 16
    vslot(gs_void_noop as *const c_void),                  // 17 HideTouchControls
    vslot(gs_void_noop as *const c_void),                  // 18 ShowTouchControlLayout
    vslot(gs_void_noop as *const c_void),                  // 19 HideTouchControlsOnClient
    vslot(gs_void_noop as *const c_void),                  // 20 ShowTouchControlLayoutOnClient
    vslot(gs_touch_input_enabled as *const c_void),        // 21
    vslot(gs_last_frame_displayed as *const c_void),       // 22
    vslot(gs_gamepad_reading_query as *const c_void),      // 23 GetAssociatedFrame
    vslot(gs_gamepad_reading_query as *const c_void),      // 24 GetGamepadPhysicality
    vslot(gs_update_touch_state as *const c_void),         // 25
    vslot(gs_update_touch_state_client as *const c_void),  // 26
    vslot(gs_show_touch_with_state as *const c_void),      // 27
    vslot(gs_show_touch_with_state_client as *const c_void), // 28
    vslot(gs_bundle_version_name_size as *const c_void),   // 29
    vslot(gs_bundle_version as *const c_void),             // 30
    vslot(gs_client_ip as *const c_void),                  // 31
    vslot(gs_session_id as *const c_void),                 // 32 (IXGameStreamingImpl2)
    vslot(gs_display_details as *const c_void),            // 33 (IXGameStreamingImpl3)
    vslot(gs_set_resolution as *const c_void),             // 34
];
const _: () = assert!(GS_VTBL.len() == GS_SLOT_COUNT);

fn create_gs() -> *mut ComHeader {
    static CELL: OnceLock<usize> = OnceLock::new();
    singleton(&CELL, GS_VTBL.as_ptr().cast())
}

// ---------------------------------------------------------------------------------------------
// XDisplayImpl (slot 3 réservé, slot 4 = XDisplayTryEnableHdrMode)
// ---------------------------------------------------------------------------------------------

const CLSID_XDISPLAY: Guid = guid("03f0fe74-fdd9-4e5c-b630-f9339c47acc5");
const IID_XDISPLAY_1: Guid = guid("35f07670-706e-4bfb-9476-090798c5ebf3");
const DISPLAY_SLOT_COUNT: usize = 5;

qi_fn!(
    display_qi,
    "XDisplayImpl",
    &[CLSID_XDISPLAY, IID_XDISPLAY_1]
);

/// `XDisplayHdrModeResult`.
const HDR_RESULT_UNKNOWN: u32 = 0;
const HDR_RESULT_DISABLED: u32 = 2;

extern "win64" fn display_try_enable_hdr(
    _this: *mut c_void,
    preference: u32,
    _info: *mut u8,
) -> u32 {
    ffi_guard!(CHANNEL, "XDisplayTryEnableHdrMode", HDR_RESULT_UNKNOWN, {
        if preference > 1 {
            // XDisplayHdrModePreference inconnue : le résultat ne peut être qu'« inconnu ».
            return HDR_RESULT_UNKNOWN;
        }
        // La sortie hôte est SDR : HDR désactivé, `XDisplayHdrModeInfo` n'est valide
        // que si le résultat est « Enabled » et reste donc inchangé.
        HDR_RESULT_DISABLED
    })
}

static DISPLAY_VTBL: [VtblSlot; DISPLAY_SLOT_COUNT] = [
    vslot(display_qi as *const c_void),
    vslot(com_add_ref as *const c_void),
    vslot(com_release as *const c_void),
    unimpl!(3), // `__PADDING__`
    vslot(display_try_enable_hdr as *const c_void),
];
const _: () = assert!(DISPLAY_VTBL.len() == DISPLAY_SLOT_COUNT);

fn create_display() -> *mut ComHeader {
    static CELL: OnceLock<usize> = OnceLock::new();
    singleton(&CELL, DISPLAY_VTBL.as_ptr().cast())
}

// ---------------------------------------------------------------------------------------------
// XUserDeviceImpl (IXUserDeviceImpl2, slots 3..10)
// ---------------------------------------------------------------------------------------------

const CLSID_XUSERDEVICE: Guid = guid("7d824997-10dc-45ab-86b7-2737767c0bf1");
const IID_XUSERDEVICE_2: Guid = guid("0cc6a956-e7e1-4fdf-9341-9d5da649ebc8");
const DEVICE_SLOT_COUNT: usize = 11;
/// `APP_LOCAL_DEVICE_ID` : 32 octets.
const DEVICE_ID_SIZE: usize = 32;

qi_fn!(
    device_qi,
    "XUserDeviceImpl",
    &[CLSID_XUSERDEVICE, IID_XUSERDEVICE_2]
);

extern "win64" fn device_find_user(
    _this: *mut c_void,
    device_id: *const u8,
    out: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserFindForDevice", E_FAIL, {
        if device_id.is_null() || out.is_null() {
            return E_INVALIDARG;
        }
        write_out(out, new_user_handle()); // tout appareil appartient au joueur fictif
        S_OK
    })
}

/// `XUserRegisterForDeviceAssociationChanged` / `...DefaultAudioEndpointUtf16Changed` :
/// jeton inerte, le callback ne sera jamais appelé.
extern "win64" fn device_register(
    _this: *mut c_void,
    _queue: usize,
    _context: *mut c_void,
    callback: usize,
    token: *mut u64,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserRegisterFor...Changed", E_FAIL, {
        if callback == 0 || token.is_null() {
            return E_INVALIDARG;
        }
        write_out(token, new_token());
        S_OK
    })
}

extern "win64" fn device_unregister(_this: *mut c_void, token: u64, _wait: u8) -> u8 {
    drop_token(token);
    1
}

extern "win64" fn device_default_audio_endpoint(
    _this: *mut c_void,
    user_local_id: u64,
    kind: u32,
    count: usize,
    buffer: *mut u16,
    used: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserGetDefaultAudioEndpointUtf16", E_FAIL, {
        if kind > 1 || (buffer.is_null() && count != 0) {
            return E_INVALIDARG;
        }
        write_out(used, 0usize);
        if count != 0 {
            write_out(buffer, 0u16);
        }
        if user_local_id != USER_LOCAL_ID {
            return super::rt_user::E_GAMEUSER_USER_NOT_FOUND;
        }
        E_NOT_FOUND // aucun point de terminaison audio propre à l'utilisateur
    })
}

extern "win64" fn device_find_controller_async(
    _this: *mut c_void,
    user: usize,
    block: *mut XAsyncBlock,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserFindControllerForUserWithUiAsync", E_FAIL, {
        if block.is_null() || !is_live_user(user) {
            return E_INVALIDARG;
        }
        // Sans UI : comme sur PC, l'identifiant d'appareil rendu est entièrement nul.
        complete_async(block, S_OK, 0);
        S_OK
    })
}

extern "win64" fn device_find_controller_result(
    _this: *mut c_void,
    block: *mut XAsyncBlock,
    device_id: *mut u8,
) -> HResult {
    ffi_guard!(CHANNEL, "XUserFindControllerForUserWithUiResult", E_FAIL, {
        if block.is_null() || device_id.is_null() {
            return E_INVALIDARG;
        }
        write_out(
            device_id.cast::<[u8; DEVICE_ID_SIZE]>(),
            [0u8; DEVICE_ID_SIZE],
        );
        match take_done(block) {
            Some(d) => d.hr,
            None => E_INVALIDARG,
        }
    })
}

static DEVICE_VTBL: [VtblSlot; DEVICE_SLOT_COUNT] = [
    vslot(device_qi as *const c_void),
    vslot(com_add_ref as *const c_void),
    vslot(com_release as *const c_void),
    vslot(device_find_user as *const c_void),  // 3
    vslot(device_register as *const c_void),   // 4
    vslot(device_unregister as *const c_void), // 5
    vslot(device_default_audio_endpoint as *const c_void), // 6
    vslot(device_register as *const c_void),   // 7
    vslot(device_unregister as *const c_void), // 8
    vslot(device_find_controller_async as *const c_void), // 9
    vslot(device_find_controller_result as *const c_void), // 10
];
const _: () = assert!(DEVICE_VTBL.len() == DEVICE_SLOT_COUNT);

fn create_device() -> *mut ComHeader {
    static CELL: OnceLock<usize> = OnceLock::new();
    singleton(&CELL, DEVICE_VTBL.as_ptr().cast())
}

// ---------------------------------------------------------------------------------------------
// XAccessibilityImpl (IXAccessibilityImpl2, slots 3..21)
// ---------------------------------------------------------------------------------------------

const CLSID_XACCESSIBILITY: Guid = guid("3e241977-6237-41e9-8559-102c2d9983f1");
const IID_XACCESSIBILITY_2: Guid = guid("d722b373-8c4d-4692-9e51-d6ad9b37aa7d");
const ACCESS_SLOT_COUNT: usize = 22;

qi_fn!(
    access_qi,
    "XAccessibilityImpl",
    &[CLSID_XACCESSIBILITY, IID_XACCESSIBILITY_2]
);

/// `XClosedCaptionGetProperties` : les propriétés (types `XColor`…) ne sont pas fournies.
extern "win64" fn access_cc_properties(_this: *mut c_void, props: *mut c_void) -> HResult {
    if props.is_null() {
        E_INVALIDARG
    } else {
        E_FAIL
    }
}

extern "win64" fn access_cc_set_enabled(_this: *mut c_void, _enabled: u8) -> HResult {
    S_OK
}

extern "win64" fn access_high_contrast(_this: *mut c_void, mode: *mut u32) -> HResult {
    ffi_guard!(CHANNEL, "XHighContrastGetMode", E_FAIL, {
        if mode.is_null() {
            return E_INVALIDARG;
        }
        write_out(mode, 0u32); // XHighContrastMode::Off
        S_OK
    })
}

/// Affichage de la reconnaissance vocale (`XSpeechToTextSetPositionHint`, `SendString`, et
/// les quatre fonctions d'hypothèse) : indisponible.
extern "win64" fn access_stt_unavailable(_this: *mut c_void) -> HResult {
    E_FAIL
}

extern "win64" fn access_stt_begin_hypothesis(
    _this: *mut c_void,
    _speaker: *const u8,
    _content: *const u8,
    _kind: u32,
    id: *mut u32,
) -> HResult {
    if id.is_null() {
        return E_INVALIDARG;
    }
    write_out(id, 0u32);
    E_FAIL
}

extern "win64" fn access_enumerate_voices(
    _this: *mut c_void,
    _context: *mut c_void,
    callback: usize,
) -> HResult {
    // Aucune voix installée : le callback n'est jamais appelé.
    if callback == 0 { E_INVALIDARG } else { S_OK }
}

extern "win64" fn access_synth_create(_this: *mut c_void, out: *mut usize) -> HResult {
    if out.is_null() {
        return E_INVALIDARG;
    }
    write_out(out, 0usize);
    E_FAIL
}

extern "win64" fn access_synth_close(_this: *mut c_void, handle: usize) -> HResult {
    // Fermer NULL est permis ; aucun synthétiseur n'a jamais été délivré.
    if handle == 0 { S_OK } else { E_INVALIDARG }
}

/// Fonctions de synthèse prenant un handle jamais délivré en premier argument.
extern "win64" fn access_bad_handle(_this: *mut c_void, _handle: usize) -> HResult {
    E_INVALIDARG
}

extern "win64" fn access_stream_from_text(
    _this: *mut c_void,
    _synth: usize,
    text: *const u8,
    out: *mut usize,
) -> HResult {
    if text.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    write_out(out, 0usize);
    E_INVALIDARG
}

extern "win64" fn access_stream_size(
    _this: *mut c_void,
    _stream: usize,
    size: *mut usize,
) -> HResult {
    if size.is_null() {
        return E_INVALIDARG;
    }
    write_out(size, 0usize);
    E_INVALIDARG
}

extern "win64" fn access_stream_data(
    _this: *mut c_void,
    _stream: usize,
    size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> HResult {
    if buffer.is_null() && size != 0 {
        return E_INVALIDARG;
    }
    write_out(used, 0usize);
    E_INVALIDARG
}

static ACCESS_VTBL: [VtblSlot; ACCESS_SLOT_COUNT] = [
    vslot(access_qi as *const c_void),
    vslot(com_add_ref as *const c_void),
    vslot(com_release as *const c_void),
    vslot(access_cc_properties as *const c_void),    // 3
    vslot(access_cc_set_enabled as *const c_void),   // 4
    vslot(access_high_contrast as *const c_void),    // 5
    vslot(access_stt_unavailable as *const c_void),  // 6 SpeechToTextSetPositionHint
    vslot(access_stt_unavailable as *const c_void),  // 7 SpeechToTextSendString
    vslot(access_enumerate_voices as *const c_void), // 8
    vslot(access_synth_create as *const c_void),     // 9
    vslot(access_synth_close as *const c_void),      // 10
    vslot(access_bad_handle as *const c_void),       // 11 SetDefaultVoice
    vslot(access_bad_handle as *const c_void),       // 12 SetCustomVoice
    vslot(access_stream_from_text as *const c_void), // 13
    vslot(access_bad_handle as *const c_void),       // 14 CloseStreamHandle
    vslot(access_stream_size as *const c_void),      // 15
    vslot(access_stream_data as *const c_void),      // 16
    vslot(access_stt_begin_hypothesis as *const c_void), // 17
    vslot(access_stt_unavailable as *const c_void),  // 18 Update
    vslot(access_stt_unavailable as *const c_void),  // 19 Finalize
    vslot(access_stt_unavailable as *const c_void),  // 20 Cancel
    vslot(access_stream_from_text as *const c_void), // 21 CreateStreamFromSsml
];
const _: () = assert!(ACCESS_VTBL.len() == ACCESS_SLOT_COUNT);

fn create_access() -> *mut ComHeader {
    static CELL: OnceLock<usize> = OnceLock::new();
    singleton(&CELL, ACCESS_VTBL.as_ptr().cast())
}

// ---------------------------------------------------------------------------------------------
// XLauncherImpl (slots 3..5)
// ---------------------------------------------------------------------------------------------

const CLSID_XLAUNCHER: Guid = guid("1b339674-328d-4283-a200-3171f18d3639");
const LAUNCHER_SLOT_COUNT: usize = 6;

qi_fn!(launcher_qi, "XLauncherImpl", &[CLSID_XLAUNCHER]);

extern "win64" fn launcher_launch_uri(_this: *mut c_void, _user: usize, uri: *const u8) -> HResult {
    ffi_guard!(CHANNEL, "XLaunchUri", E_FAIL, {
        let Some(uri) = read_cstr(uri) else {
            return E_INVALIDARG;
        };
        sb_info!(CHANNEL, "XLaunchUri({uri}) : rien n'est lancé -> E_FAIL");
        E_FAIL
    })
}

extern "win64" fn launcher_acquire_timeout_deferral(
    _this: *mut c_void,
    handle: *mut usize,
) -> HResult {
    ffi_guard!(CHANNEL, "XDisplayAcquireTimeoutDeferral", E_FAIL, {
        if handle.is_null() {
            return E_INVALIDARG;
        }
        write_out(handle, 0usize);
        E_FAIL
    })
}

extern "win64" fn launcher_close_timeout_deferral(_this: *mut c_void, _handle: usize) {}

static LAUNCHER_VTBL: [VtblSlot; LAUNCHER_SLOT_COUNT] = [
    vslot(launcher_qi as *const c_void),
    vslot(com_add_ref as *const c_void),
    vslot(com_release as *const c_void),
    vslot(launcher_launch_uri as *const c_void), // 3
    vslot(launcher_acquire_timeout_deferral as *const c_void), // 4
    vslot(launcher_close_timeout_deferral as *const c_void), // 5
];
const _: () = assert!(LAUNCHER_VTBL.len() == LAUNCHER_SLOT_COUNT);

fn create_launcher() -> *mut ComHeader {
    static CELL: OnceLock<usize> = OnceLock::new();
    singleton(&CELL, LAUNCHER_VTBL.as_ptr().cast())
}

/// Classes fournies par ce module (enregistrées dans `gdk_classes::registry`).
pub fn classes() -> Vec<ClassDef> {
    vec![
        ClassDef {
            name: "XGameEventImpl",
            clsid: CLSID_XGAMEEVENT,
            extra_iids: &[],
            create: create_event,
        },
        ClassDef {
            name: "XGameUiImpl",
            clsid: CLSID_XGAMEUI,
            extra_iids: UI_EXTRA_IIDS,
            create: create_ui,
        },
        ClassDef {
            name: "XGameStreamingImpl",
            clsid: CLSID_XGAMESTREAMING,
            extra_iids: &[
                IID_XGAMESTREAMING_1,
                IID_XGAMESTREAMING_2,
                IID_XGAMESTREAMING_3,
            ],
            create: create_gs,
        },
        ClassDef {
            name: "XDisplayImpl",
            clsid: CLSID_XDISPLAY,
            extra_iids: &[IID_XDISPLAY_1],
            create: create_display,
        },
        ClassDef {
            name: "XUserDeviceImpl",
            clsid: CLSID_XUSERDEVICE,
            extra_iids: &[IID_XUSERDEVICE_2],
            create: create_device,
        },
        ClassDef {
            name: "XAccessibilityImpl",
            clsid: CLSID_XACCESSIBILITY,
            extra_iids: &[IID_XACCESSIBILITY_2],
            create: create_access,
        },
        ClassDef {
            name: "XLauncherImpl",
            clsid: CLSID_XLAUNCHER,
            extra_iids: &[],
            create: create_launcher,
        },
    ]
}

#[cfg(test)]
#[allow(clippy::manual_c_str_literals, clippy::manual_dangling_ptr)]
mod tests {
    use super::super::rt_user::test_util::*;
    use super::*;
    use sb_common::abi::{E_NOINTERFACE, E_NOTIMPL};

    const NULLP: *const u8 = core::ptr::null();

    fn handle() -> usize {
        new_user_handle()
    }

    #[test]
    fn vtable_lengths() {
        const _: () = assert!(EVENT_VTBL.len() == 4);
        const _: () = assert!(UI_VTBL.len() == 51);
        const _: () = assert!(GS_VTBL.len() == 35);
        const _: () = assert!(DISPLAY_VTBL.len() == 5);
        const _: () = assert!(DEVICE_VTBL.len() == 11);
        const _: () = assert!(ACCESS_VTBL.len() == 22);
        const _: () = assert!(LAUNCHER_VTBL.len() == 6);
    }

    #[test]
    fn iunknown_for_every_class() {
        let all: [(&Guid, &[Guid]); 7] = [
            (&CLSID_XGAMEEVENT, &[]),
            (&CLSID_XGAMEUI, UI_EXTRA_IIDS),
            (
                &CLSID_XGAMESTREAMING,
                &[
                    IID_XGAMESTREAMING_1,
                    IID_XGAMESTREAMING_2,
                    IID_XGAMESTREAMING_3,
                ],
            ),
            (&CLSID_XDISPLAY, &[IID_XDISPLAY_1]),
            (&CLSID_XUSERDEVICE, &[IID_XUSERDEVICE_2]),
            (&CLSID_XACCESSIBILITY, &[IID_XACCESSIBILITY_2]),
            (&CLSID_XLAUNCHER, &[]),
        ];
        for (clsid, extras) in all {
            let o = obj(clsid);
            assert!(add_ref(o) >= 2);
            assert!(release(o) >= 1);
            for iid in extras {
                // Même objet via QueryApiImpl et via QueryInterface.
                assert_eq!(obj_iid(clsid, iid), o);
                let mut p = core::ptr::null_mut();
                assert_eq!(qi(o, iid, &mut p), S_OK);
                assert_eq!(p, o);
            }
            let mut p = core::ptr::null_mut();
            assert_eq!(qi(o, &BOGUS, &mut p), E_NOINTERFACE);
            assert!(p.is_null());
            let mut x = core::ptr::null_mut();
            assert_eq!(
                crate::gdk_classes::query(clsid, &BOGUS, &mut x),
                E_NOINTERFACE
            );
        }
    }

    #[test]
    fn game_event() {
        let o = obj(&CLSID_XGAMEEVENT);
        let w: extern "win64" fn(
            *mut c_void,
            usize,
            *const u8,
            *const u8,
            *const u8,
            *const u8,
            *const u8,
        ) -> HResult = slot(o, 3);
        let h = handle();
        let s = b"abc\0".as_ptr();
        assert_eq!(w(o, h, s, s, b"LevelStart\0".as_ptr(), NULLP, NULLP), S_OK);
        assert_eq!(w(o, h, s, s, b"Other\0".as_ptr(), s, s), S_OK);
        assert_eq!(w(o, 0, s, s, s, NULLP, NULLP), E_INVALIDARG);
        assert_eq!(w(o, h, NULLP, s, s, NULLP, NULLP), E_INVALIDARG);
        assert_eq!(w(o, h, s, NULLP, s, NULLP, NULLP), E_INVALIDARG);
        assert_eq!(w(o, h, s, s, NULLP, NULLP, NULLP), E_INVALIDARG);
        assert!(FIRST_EVENT_LOGGED.load(Ordering::SeqCst));
    }

    #[test]
    fn game_ui_dialogs() {
        let o = obj_iid(&CLSID_XGAMEUI, &IID_XGAMEUI_4);
        let md: extern "win64" fn(
            *mut c_void,
            *mut XAsyncBlock,
            *const u8,
            *const u8,
            *const u8,
            *const u8,
            *const u8,
            u32,
            u32,
        ) -> HResult = slot(o, 3);
        let mr: extern "win64" fn(*mut c_void, *mut XAsyncBlock, *mut u32) -> HResult = slot(o, 4);
        let mut b = new_block();
        let t = b"Titre\0".as_ptr();
        assert_eq!(md(o, &mut *b, t, t, t, NULLP, NULLP, 1, 2), S_OK);
        assert_eq!(b.internal[0], 1);
        let mut btn = 9u32;
        assert_eq!(mr(o, &mut *b, core::ptr::null_mut()), E_INVALIDARG);
        assert_eq!(mr(o, &mut *b, &mut btn), S_OK);
        assert_eq!(btn, 1, "bouton par défaut");
        assert_eq!(mr(o, &mut *b, &mut btn), E_INVALIDARG, "résultat consommé");
        assert_eq!(
            md(o, core::ptr::null_mut(), t, t, t, NULLP, NULLP, 0, 0),
            E_INVALIDARG
        );
        assert_eq!(
            md(o, &mut *b, NULLP, t, t, NULLP, NULLP, 0, 0),
            E_INVALIDARG
        );
        assert_eq!(
            md(o, &mut *b, t, NULLP, t, NULLP, NULLP, 0, 0),
            E_INVALIDARG
        );
        assert_eq!(md(o, &mut *b, t, t, t, NULLP, NULLP, 3, 0), E_INVALIDARG);
        assert_eq!(md(o, &mut *b, t, t, t, NULLP, NULLP, 0, 3), E_INVALIDARG);
        // Erreur
        let ea: extern "win64" fn(*mut c_void, *mut XAsyncBlock, i32, *const u8) -> HResult =
            slot(o, 14);
        let er: extern "win64" fn(*mut c_void, *mut XAsyncBlock) -> HResult = slot(o, 15);
        assert_eq!(ea(o, core::ptr::null_mut(), E_FAIL, NULLP), E_INVALIDARG);
        assert_eq!(er(o, &mut *b), E_INVALIDARG);
        assert_eq!(ea(o, &mut *b, E_FAIL, t), S_OK);
        assert_eq!(er(o, &mut *b), S_OK);
        // Position des notifications
        let np: extern "win64" fn(*mut c_void, u32) -> HResult = slot(o, 16);
        assert_eq!(np(o, 5), S_OK);
        assert_eq!(np(o, 6), E_INVALIDARG);
    }

    #[test]
    fn game_ui_other_ui_fails_before_start() {
        let o = obj(&CLSID_XGAMEUI);
        let mut b = new_block();
        for n in [5usize, 7, 9, 11, 17, 24, 27, 30, 40] {
            let f: extern "win64" fn(*mut c_void, *mut XAsyncBlock) -> HResult = slot(o, n);
            assert_eq!(f(o, &mut *b), E_FAIL, "slot {n}");
            assert_eq!(f(o, core::ptr::null_mut()), E_INVALIDARG, "slot {n}");
            assert_eq!(b.internal[0], 0, "slot {n} ne doit rien démarrer");
        }
        for n in [6usize, 8, 10, 12, 13, 18, 19, 25, 26, 31, 41] {
            let f: extern "win64" fn(*mut c_void, *mut XAsyncBlock) -> HResult = slot(o, n);
            assert_eq!(f(o, &mut *b), E_INVALIDARG, "slot {n}");
        }
        // Saisie de texte
        let open: extern "win64" fn(
            *mut c_void,
            *const c_void,
            u32,
            *const u8,
            u32,
            *mut usize,
        ) -> HResult = slot(o, 34);
        let mut th = 5usize;
        let opts = [0u32; 3];
        assert_eq!(open(o, opts.as_ptr().cast(), 10, NULLP, 0, &mut th), E_FAIL);
        assert_eq!(th, 0);
        assert_eq!(
            open(o, core::ptr::null(), 10, NULLP, 0, &mut th),
            E_INVALIDARG
        );
        assert_eq!(
            open(o, opts.as_ptr().cast(), 10, NULLP, 0, core::ptr::null_mut()),
            E_INVALIDARG
        );
        for n in 35usize..=39 {
            let f: extern "win64" fn(*mut c_void, usize) -> HResult = slot(o, n);
            assert_eq!(f(o, 0x1234), E_INVALIDARG);
        }
        // Slots réservés / hébergement PC.
        for n in [20usize, 21, 22, 23, 28, 29, 32, 33, 42, 46, 50] {
            let f: extern "win64" fn(*mut c_void, usize, usize, usize) -> HResult = slot(o, n);
            assert_eq!(f(o, 0, 0, 0), E_NOTIMPL, "slot {n}");
        }
    }

    #[test]
    fn game_streaming_is_never_streaming() {
        let o = obj_iid(&CLSID_XGAMESTREAMING, &IID_XGAMESTREAMING_3);
        let init: extern "win64" fn(*mut c_void) -> HResult = slot(o, 3);
        let uninit: extern "win64" fn(*mut c_void) = slot(o, 4);
        let is: extern "win64" fn(*mut c_void) -> u8 = slot(o, 5);
        let count: extern "win64" fn(*mut c_void) -> u32 = slot(o, 9);
        assert_eq!(init(o), S_OK);
        assert_eq!(is(o), 0);
        assert_eq!(count(o), 0);
        uninit(o);
        let clients: extern "win64" fn(*mut c_void, u32, *mut u64, *mut u32) -> HResult =
            slot(o, 10);
        let mut used = 7u32;
        let mut ids = [0u64; 2];
        assert_eq!(clients(o, 2, ids.as_mut_ptr(), &mut used), S_OK);
        assert_eq!(used, 0);
        assert_eq!(
            clients(o, 2, core::ptr::null_mut(), &mut used),
            E_INVALIDARG
        );
        let state: extern "win64" fn(*mut c_void, u64) -> u32 = slot(o, 11);
        assert_eq!(state(o, 1), 0);
        // Enregistrement d'état de connexion : jeton inerte.
        let reg: extern "win64" fn(*mut c_void, usize, *mut c_void, usize, *mut u64) -> HResult =
            slot(o, 12);
        let unreg: extern "win64" fn(*mut c_void, u64, u8) -> u8 = slot(o, 13);
        let mut t = 0u64;
        assert_eq!(reg(o, 0, core::ptr::null_mut(), 0, &mut t), E_INVALIDARG);
        assert_eq!(
            reg(o, 0, core::ptr::null_mut(), 0x10, core::ptr::null_mut()),
            E_INVALIDARG
        );
        assert_eq!(reg(o, 0, core::ptr::null_mut(), 0x10, &mut t), S_OK);
        assert_ne!(t, 0);
        assert_eq!(unreg(o, t, 1), 1);
        // Appels par client.
        let (mut h, mut v) = (5u32, 5u32);
        let dims: extern "win64" fn(*mut c_void, u64, *mut u32, *mut u32) -> HResult = slot(o, 8);
        assert_eq!(dims(o, 1, &mut h, &mut v), E_GAMESTREAMING_NOT_INITIALIZED);
        assert_eq!((h, v), (0, 0));
        assert_eq!(dims(o, 1, core::ptr::null_mut(), &mut v), E_INVALIDARG);
        let touch: extern "win64" fn(*mut c_void, u64, *mut u8) -> HResult = slot(o, 21);
        let mut en = 5u8;
        assert_eq!(touch(o, 1, &mut en), E_GAMESTREAMING_NOT_INITIALIZED);
        assert_eq!(en, 0);
        assert_eq!(E_GAMESTREAMING_NOT_INITIALIZED as u32, 0x8924_5400);
        let lat: extern "win64" fn(*mut c_void, u64, *mut u32, *mut u32, *mut u32) -> HResult =
            slot(o, 14);
        let (mut a, mut b2, mut c) = (1u32, 1u32, 1u32);
        assert_eq!(
            lat(o, 1, &mut a, &mut b2, &mut c),
            E_GAMESTREAMING_NOT_INITIALIZED
        );
        assert_eq!((a, b2, c), (0, 0, 0));
        let size: extern "win64" fn(*mut c_void) -> usize = slot(o, 15);
        assert_eq!(size(o), 0);
        let name: extern "win64" fn(*mut c_void, usize, *mut u8) -> HResult = slot(o, 16);
        let mut nb = [9u8; 4];
        assert_eq!(name(o, 4, nb.as_mut_ptr()), E_GAMESTREAMING_NO_DATACENTER);
        assert_eq!(nb[0], 0);
        assert_eq!(name(o, 4, core::ptr::null_mut()), E_INVALIDARG);
        let phys: extern "win64" fn(*mut c_void, *mut c_void, *mut u64) -> HResult = slot(o, 24);
        let mut pv = 5u64;
        let dummy = 1u8;
        let rd = &dummy as *const u8 as *mut c_void;
        assert_eq!(
            phys(o, rd, &mut pv),
            E_GAMESTREAMING_NOT_STREAMING_CONTROLLER
        );
        assert_eq!(pv, 0);
        assert_eq!(phys(o, core::ptr::null_mut(), &mut pv), E_INVALIDARG);
        let upd: extern "win64" fn(*mut c_void, usize, *const c_void) -> HResult = slot(o, 25);
        assert_eq!(
            upd(o, 0, core::ptr::null()),
            E_GAMESTREAMING_NOT_INITIALIZED
        );
        assert_eq!(upd(o, 1, core::ptr::null()), E_INVALIDARG);
        let sid: extern "win64" fn(*mut c_void, u64, usize, *mut u8, *mut usize) -> HResult =
            slot(o, 32);
        let mut u = 4usize;
        assert_eq!(
            sid(o, 1, 4, nb.as_mut_ptr(), &mut u),
            E_GAMESTREAMING_NOT_INITIALIZED
        );
        assert_eq!(u, 0);
        let dd: extern "win64" fn(*mut c_void, u64, u32, f32, f32, *mut u8) -> HResult =
            slot(o, 33);
        let mut det = [0xffu8; GS_DISPLAY_DETAILS_SIZE];
        assert_eq!(
            dd(o, 1, 1000, 1.0, 2.0, det.as_mut_ptr()),
            E_GAMESTREAMING_NOT_INITIALIZED
        );
        assert!(det.iter().all(|&x| x == 0));
        assert_eq!(
            dd(o, 1, 1000, 1.0, 2.0, core::ptr::null_mut()),
            E_INVALIDARG
        );
        let sr: extern "win64" fn(*mut c_void, u32, u32) -> HResult = slot(o, 34);
        assert_eq!(sr(o, 1920, 1080), E_GAMESTREAMING_NOT_INITIALIZED);
        let bv: extern "win64" fn(*mut c_void, u64, *mut u8, usize, *mut u8) -> HResult =
            slot(o, 30);
        let mut ver = [0xffu8; 8];
        assert_eq!(
            bv(o, 1, ver.as_mut_ptr(), 4, nb.as_mut_ptr()),
            E_GAMESTREAMING_NOT_INITIALIZED
        );
        assert_eq!(ver, [0u8; 8]);
        let ip: extern "win64" fn(*mut c_void, u64, usize, *mut u8) -> HResult = slot(o, 31);
        assert_eq!(
            ip(o, 1, 4, nb.as_mut_ptr()),
            E_GAMESTREAMING_NOT_INITIALIZED
        );
        assert_eq!(ip(o, 1, 4, core::ptr::null_mut()), E_INVALIDARG);
    }

    #[test]
    fn display_hdr_disabled() {
        let o = obj_iid(&CLSID_XDISPLAY, &IID_XDISPLAY_1);
        let f: extern "win64" fn(*mut c_void, u32, *mut u8) -> u32 = slot(o, 4);
        let mut info = [0u8; 12];
        assert_eq!(f(o, 0, info.as_mut_ptr()), HDR_RESULT_DISABLED);
        assert_eq!(f(o, 1, core::ptr::null_mut()), HDR_RESULT_DISABLED);
        assert_eq!(f(o, 9, core::ptr::null_mut()), HDR_RESULT_UNKNOWN);
        let pad: extern "win64" fn(*mut c_void, usize, usize, usize) -> HResult = slot(o, 3);
        assert_eq!(pad(o, 0, 0, 0), E_NOTIMPL);
    }

    #[test]
    fn user_device() {
        let o = obj_iid(&CLSID_XUSERDEVICE, &IID_XUSERDEVICE_2);
        let find: extern "win64" fn(*mut c_void, *const u8, *mut usize) -> HResult = slot(o, 3);
        let dev = [0u8; DEVICE_ID_SIZE];
        let mut h = 0usize;
        assert_eq!(find(o, dev.as_ptr(), &mut h), S_OK);
        assert!(is_live_user(h));
        assert_eq!(find(o, NULLP, &mut h), E_INVALIDARG);
        assert_eq!(find(o, dev.as_ptr(), core::ptr::null_mut()), E_INVALIDARG);
        for (reg_slot, unreg_slot) in [(4usize, 5usize), (7, 8)] {
            let reg: extern "win64" fn(
                *mut c_void,
                usize,
                *mut c_void,
                usize,
                *mut u64,
            ) -> HResult = slot(o, reg_slot);
            let unreg: extern "win64" fn(*mut c_void, u64, u8) -> u8 = slot(o, unreg_slot);
            let mut t = 0u64;
            assert_eq!(reg(o, 0, core::ptr::null_mut(), 0, &mut t), E_INVALIDARG);
            assert_eq!(reg(o, 0, core::ptr::null_mut(), 0x10, &mut t), S_OK);
            assert_ne!(t, 0);
            assert_eq!(unreg(o, t, 0), 1);
        }
        let ep: extern "win64" fn(*mut c_void, u64, u32, usize, *mut u16, *mut usize) -> HResult =
            slot(o, 6);
        let mut buf = [7u16; 4];
        let mut used = 9usize;
        assert_eq!(
            ep(o, USER_LOCAL_ID, 0, 4, buf.as_mut_ptr(), &mut used),
            E_NOT_FOUND
        );
        assert_eq!((buf[0], used), (0, 0));
        assert_eq!(
            ep(o, USER_LOCAL_ID, 2, 4, buf.as_mut_ptr(), &mut used),
            E_INVALIDARG
        );
        assert_eq!(
            ep(o, USER_LOCAL_ID, 0, 4, core::ptr::null_mut(), &mut used),
            E_INVALIDARG
        );
        assert_eq!(
            ep(o, 999, 0, 4, buf.as_mut_ptr(), &mut used),
            super::super::rt_user::E_GAMEUSER_USER_NOT_FOUND
        );
        let fa: extern "win64" fn(*mut c_void, usize, *mut XAsyncBlock) -> HResult = slot(o, 9);
        let fr: extern "win64" fn(*mut c_void, *mut XAsyncBlock, *mut u8) -> HResult = slot(o, 10);
        let mut b = new_block();
        assert_eq!(fa(o, 0, &mut *b), E_INVALIDARG);
        assert_eq!(fa(o, h, core::ptr::null_mut()), E_INVALIDARG);
        assert_eq!(fa(o, h, &mut *b), S_OK);
        let mut id = [0xffu8; DEVICE_ID_SIZE];
        assert_eq!(fr(o, &mut *b, core::ptr::null_mut()), E_INVALIDARG);
        assert_eq!(fr(o, &mut *b, id.as_mut_ptr()), S_OK);
        assert!(id.iter().all(|&x| x == 0));
    }

    #[test]
    fn accessibility() {
        let o = obj_iid(&CLSID_XACCESSIBILITY, &IID_XACCESSIBILITY_2);
        let hc: extern "win64" fn(*mut c_void, *mut u32) -> HResult = slot(o, 5);
        let mut m = 9u32;
        assert_eq!(hc(o, &mut m), S_OK);
        assert_eq!(m, 0);
        assert_eq!(hc(o, core::ptr::null_mut()), E_INVALIDARG);
        let en: extern "win64" fn(*mut c_void, u8) -> HResult = slot(o, 4);
        assert_eq!(en(o, 1), S_OK);
        let cc: extern "win64" fn(*mut c_void, *mut c_void) -> HResult = slot(o, 3);
        let mut props = [0u8; 40];
        assert_eq!(cc(o, props.as_mut_ptr().cast()), E_FAIL);
        assert!(props.iter().all(|&x| x == 0), "rien n'est écrit");
        assert_eq!(cc(o, core::ptr::null_mut()), E_INVALIDARG);
        let voices: extern "win64" fn(*mut c_void, *mut c_void, usize) -> HResult = slot(o, 8);
        assert_eq!(voices(o, core::ptr::null_mut(), 0x10), S_OK);
        assert_eq!(voices(o, core::ptr::null_mut(), 0), E_INVALIDARG);
        let create: extern "win64" fn(*mut c_void, *mut usize) -> HResult = slot(o, 9);
        let mut sh = 5usize;
        assert_eq!(create(o, &mut sh), E_FAIL);
        assert_eq!(sh, 0);
        assert_eq!(create(o, core::ptr::null_mut()), E_INVALIDARG);
        let close: extern "win64" fn(*mut c_void, usize) -> HResult = slot(o, 10);
        assert_eq!(close(o, 0), S_OK);
        assert_eq!(close(o, 0x77), E_INVALIDARG);
        for n in [11usize, 14] {
            let f: extern "win64" fn(*mut c_void, usize) -> HResult = slot(o, n);
            assert_eq!(f(o, 0x77), E_INVALIDARG);
        }
        for n in [13usize, 21] {
            let f: extern "win64" fn(*mut c_void, usize, *const u8, *mut usize) -> HResult =
                slot(o, n);
            let mut st = 5usize;
            assert_eq!(f(o, 0x77, b"t\0".as_ptr(), &mut st), E_INVALIDARG);
            assert_eq!(st, 0);
            assert_eq!(f(o, 0x77, NULLP, &mut st), E_INVALIDARG);
        }
        let size: extern "win64" fn(*mut c_void, usize, *mut usize) -> HResult = slot(o, 15);
        let mut sz = 5usize;
        assert_eq!(size(o, 0x77, &mut sz), E_INVALIDARG);
        assert_eq!(sz, 0);
        let data: extern "win64" fn(*mut c_void, usize, usize, *mut c_void, *mut usize) -> HResult =
            slot(o, 16);
        assert_eq!(
            data(o, 0x77, 4, core::ptr::null_mut(), &mut sz),
            E_INVALIDARG
        );
        let stt_begin: extern "win64" fn(
            *mut c_void,
            *const u8,
            *const u8,
            u32,
            *mut u32,
        ) -> HResult = slot(o, 17);
        let mut id = 5u32;
        assert_eq!(stt_begin(o, NULLP, NULLP, 0, &mut id), E_FAIL);
        assert_eq!(id, 0);
        assert_eq!(
            stt_begin(o, NULLP, NULLP, 0, core::ptr::null_mut()),
            E_INVALIDARG
        );
        for n in [6usize, 7, 18, 19, 20] {
            let f: extern "win64" fn(*mut c_void) -> HResult = slot(o, n);
            assert_eq!(f(o), E_FAIL);
        }
    }

    #[test]
    fn launcher() {
        let o = obj(&CLSID_XLAUNCHER);
        let uri: extern "win64" fn(*mut c_void, usize, *const u8) -> HResult = slot(o, 3);
        assert_eq!(uri(o, 0, b"https://example.invalid\0".as_ptr()), E_FAIL);
        assert_eq!(uri(o, 0, NULLP), E_INVALIDARG);
        let acq: extern "win64" fn(*mut c_void, *mut usize) -> HResult = slot(o, 4);
        let mut h = 5usize;
        assert_eq!(acq(o, &mut h), E_FAIL);
        assert_eq!(h, 0);
        assert_eq!(acq(o, core::ptr::null_mut()), E_INVALIDARG);
        let close: extern "win64" fn(*mut c_void, usize) = slot(o, 5);
        close(o, 0);
    }
}
