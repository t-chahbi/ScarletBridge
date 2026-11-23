//! COM de base (`ole32` / `combase` / `api-ms-win-core-com-*`) : appartements par thread,
//! `CoCreateGuid` (UUID v4), `CoCreateInstance` (aucune classe enregistrée), `PropVariantClear`,
//! conversions GUID ↔ chaîne et `CoTaskMemRealloc` (allocateur libc, comme `CoTaskMemAlloc`).

use core::ffi::c_void;
use core::mem::size_of;
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use sb_common::abi::{E_INVALIDARG, E_OUTOFMEMORY, E_POINTER, HResult, S_FALSE, S_OK};
use sb_common::ffi_guard;
use sb_common::wide::from_pcwstr;

use super::{CHANNEL, bcrypt::fill_random, write_out};
use crate::com::{Guid, format_guid_opt};

pub(crate) const RPC_E_CHANGED_MODE: HResult = 0x8001_0106_u32 as i32;
pub(crate) const REGDB_E_CLASSNOTREG: HResult = 0x8004_0154_u32 as i32;
pub(crate) const CO_E_NOTINITIALIZED: HResult = 0x8004_01F0_u32 as i32;
pub(crate) const CO_E_CLASSSTRING: HResult = 0x8004_01F3_u32 as i32;
const DISP_E_BADVARTYPE: HResult = 0x8002_0008_u32 as i32;

const COINIT_APARTMENTTHREADED: u32 = 0x2;
const COINIT_VALID: u32 = 0x2 | 0x4 | 0x8;

thread_local! {
    /// (nombre d'initialisations, STA ?)
    static APARTMENT: Cell<(u32, bool)> = const { Cell::new((0, false)) };
}
/// Nombre de threads MTA initialisés (MTA implicite pour les autres threads).
static MTA_THREADS: AtomicUsize = AtomicUsize::new(0);

pub(super) extern "win64" fn CoInitializeEx(reserved: *mut c_void, coinit: u32) -> HResult {
    ffi_guard!(CHANNEL, "CoInitializeEx", E_INVALIDARG, {
        if !reserved.is_null() || (coinit & !COINIT_VALID) != 0 {
            return E_INVALIDARG;
        }
        let sta = (coinit & COINIT_APARTMENTTHREADED) != 0;
        APARTMENT.with(|a| {
            let (count, cur_sta) = a.get();
            if count == 0 {
                a.set((1, sta));
                if !sta {
                    MTA_THREADS.fetch_add(1, Ordering::AcqRel);
                }
                S_OK
            } else if cur_sta != sta {
                RPC_E_CHANGED_MODE
            } else {
                a.set((count + 1, cur_sta));
                S_FALSE
            }
        })
    })
}

pub(super) extern "win64" fn CoInitialize(reserved: *mut c_void) -> HResult {
    CoInitializeEx(reserved, COINIT_APARTMENTTHREADED)
}

pub(super) extern "win64" fn CoUninitialize() {
    ffi_guard!(CHANNEL, "CoUninitialize", (), {
        APARTMENT.with(|a| {
            let (count, sta) = a.get();
            if count == 0 {
                return;
            }
            a.set((count - 1, sta));
            if count == 1 && !sta {
                MTA_THREADS.fetch_sub(1, Ordering::AcqRel);
            }
        });
    })
}

fn com_available() -> bool {
    APARTMENT.with(|a| a.get().0 > 0) || MTA_THREADS.load(Ordering::Acquire) > 0
}

pub(super) extern "win64" fn CoCreateGuid(out: *mut Guid) -> HResult {
    ffi_guard!(CHANNEL, "CoCreateGuid", E_INVALIDARG, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        let mut b = [0u8; 16];
        if !fill_random(&mut b) {
            return E_OUTOFMEMORY;
        }
        let mut g = Guid::new(
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            u16::from_le_bytes([b[4], b[5]]),
            u16::from_le_bytes([b[6], b[7]]),
            [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
        );
        g.data3 = (g.data3 & 0x0FFF) | 0x4000; // version 4
        g.data4[0] = (g.data4[0] & 0x3F) | 0x80; // variante RFC 4122
        write_out(out, g);
        S_OK
    })
}

pub(super) extern "win64" fn CoCreateInstance(
    clsid: *const Guid,
    outer: *mut c_void,
    clsctx: u32,
    iid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CoCreateInstance", E_POINTER, {
        if ppv.is_null() {
            return E_POINTER;
        }
        write_out(ppv, core::ptr::null_mut());
        if clsid.is_null() || iid.is_null() {
            return E_INVALIDARG;
        }
        if !com_available() {
            return CO_E_NOTINITIALIZED;
        }
        // Aucune classe COM n'est enregistrée (pas de registre) ; les objets WinRT passent par
        // `RoGetActivationFactory`.
        sb_common::sb_debug!(
            CHANNEL,
            "CoCreateInstance(clsid={}, iid={}) -> REGDB_E_CLASSNOTREG",
            format_guid_opt(clsid),
            format_guid_opt(iid)
        );
        REGDB_E_CLASSNOTREG
    })
}

pub(super) extern "win64" fn CoGetClassObject(
    clsid: *const Guid,
    clsctx: u32,
    server_info: *mut c_void,
    iid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    ffi_guard!(CHANNEL, "CoGetClassObject", E_POINTER, {
        if ppv.is_null() {
            return E_POINTER;
        }
        write_out(ppv, core::ptr::null_mut());
        if !com_available() {
            return CO_E_NOTINITIALIZED;
        }
        REGDB_E_CLASSNOTREG
    })
}

pub(super) extern "win64" fn CoTaskMemRealloc(p: *mut c_void, size: usize) -> *mut c_void {
    ffi_guard!(CHANNEL, "CoTaskMemRealloc", core::ptr::null_mut(), {
        if size == 0 && !p.is_null() {
            // SAFETY: `p` provient de `CoTaskMemAlloc` (malloc).
            unsafe { libc::free(p.cast()) };
            return core::ptr::null_mut();
        }
        // SAFETY: `p` est nul ou provient de `CoTaskMemAlloc` (malloc).
        unsafe { libc::realloc(p.cast(), size.max(1)) }.cast()
    })
}

/// `PROPVARIANT` (24 octets).
#[repr(C)]
pub(crate) struct PropVariant {
    pub vt: u16,
    pub reserved: [u16; 3],
    pub data: [u64; 2],
}

const _: () = assert!(size_of::<PropVariant>() == 24);

const VT_DISPATCH: u16 = 9;
const VT_BSTR: u16 = 8;
const VT_UNKNOWN: u16 = 13;
const VT_LPSTR: u16 = 30;
const VT_LPWSTR: u16 = 31;
const VT_BLOB: u16 = 65;
const VT_CLSID: u16 = 72;

pub(super) extern "win64" fn PropVariantClear(pv: *mut PropVariant) -> HResult {
    ffi_guard!(CHANNEL, "PropVariantClear", E_INVALIDARG, {
        if pv.is_null() {
            return S_OK;
        }
        // SAFETY: `pv` non-nul pointe vers un PROPVARIANT de l'invité.
        let v = unsafe { &mut *pv };
        let p0 = v.data[0] as usize;
        match v.vt {
            // Types scalaires (VT_EMPTY, VT_NULL, entiers, réels, CY, DATE, BOOL, ERROR, FILETIME...).
            0..=7 | 10 | 11 | 14 | 16..=23 | 25 | 64 => {}
            VT_BSTR => {
                if p0 != 0 {
                    // SAFETY: BSTR alloué par malloc avec préfixe de longueur 32 bits.
                    unsafe { libc::free((p0 - 4) as *mut libc::c_void) };
                }
            }
            VT_LPSTR | VT_LPWSTR | VT_CLSID => {
                if p0 != 0 {
                    // SAFETY: mémoire `CoTaskMemAlloc` (malloc).
                    unsafe { libc::free(p0 as *mut libc::c_void) };
                }
            }
            VT_BLOB => {
                let p = v.data[1] as usize;
                if p != 0 {
                    // SAFETY: `pBlobData` alloué par `CoTaskMemAlloc`.
                    unsafe { libc::free(p as *mut libc::c_void) };
                }
            }
            VT_UNKNOWN | VT_DISPATCH => {
                if p0 != 0 {
                    // SAFETY: `p0` est un pointeur d'interface COM ; slot 2 = Release.
                    unsafe {
                        let vtbl = *(p0 as *const *const usize);
                        let release: unsafe extern "win64" fn(usize) -> u32 =
                            core::mem::transmute(*vtbl.add(2));
                        release(p0);
                    }
                }
            }
            _ => return DISP_E_BADVARTYPE,
        }
        v.vt = 0;
        v.reserved = [0; 3];
        v.data = [0; 2];
        S_OK
    })
}

/// Format `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}` (majuscules).
pub(crate) fn guid_to_braced(g: &Guid) -> String {
    format!("{{{}}}", g).to_ascii_uppercase()
}

pub(crate) fn parse_braced_guid(s: &str) -> Option<Guid> {
    let inner = s.strip_prefix('{')?.strip_suffix('}')?;
    let parts: Vec<&str> = inner.split('-').collect();
    if parts.len() != 5
        || [8, 4, 4, 4, 12]
            != [
                parts[0].len(),
                parts[1].len(),
                parts[2].len(),
                parts[3].len(),
                parts[4].len(),
            ]
        || !inner.chars().all(|c| c == '-' || c.is_ascii_hexdigit())
    {
        return None;
    }
    let d4 = format!("{}{}", parts[3], parts[4]);
    let mut data4 = [0u8; 8];
    for (i, b) in data4.iter_mut().enumerate() {
        *b = u8::from_str_radix(&d4[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(Guid::new(
        u32::from_str_radix(parts[0], 16).ok()?,
        u16::from_str_radix(parts[1], 16).ok()?,
        u16::from_str_radix(parts[2], 16).ok()?,
        data4,
    ))
}

pub(super) extern "win64" fn StringFromGUID2(guid: *const Guid, out: *mut u16, cch: i32) -> i32 {
    ffi_guard!(CHANNEL, "StringFromGUID2", 0, {
        if guid.is_null() || out.is_null() || cch < 39 {
            return 0;
        }
        // SAFETY: `guid` non-nul.
        let s = guid_to_braced(unsafe { &*guid });
        let units: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `out` dispose d'au moins 39 unités.
        unsafe { core::ptr::copy_nonoverlapping(units.as_ptr(), out, units.len()) };
        units.len() as i32
    })
}

pub(super) extern "win64" fn StringFromCLSID(guid: *const Guid, out: *mut *mut u16) -> HResult {
    ffi_guard!(CHANNEL, "StringFromCLSID", E_INVALIDARG, {
        if guid.is_null() || out.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: allocation compatible `CoTaskMemFree` (malloc).
        let p = unsafe { libc::malloc(39 * 2) }.cast::<u16>();
        if p.is_null() {
            return E_OUTOFMEMORY;
        }
        StringFromGUID2(guid, p, 39);
        write_out(out, p);
        S_OK
    })
}

pub(super) extern "win64" fn CLSIDFromString(s: *const u16, out: *mut Guid) -> HResult {
    ffi_guard!(CHANNEL, "CLSIDFromString", E_INVALIDARG, {
        if out.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: `s` est un `PCWSTR` valide ou nul.
        let Some(text) = (unsafe { from_pcwstr(s) }) else {
            write_out(out, Guid::new(0, 0, 0, [0; 8]));
            return S_OK; // NULL -> CLSID_NULL
        };
        match parse_braced_guid(&text) {
            Some(g) => {
                write_out(out, g);
                S_OK
            }
            None => CO_E_CLASSSTRING,
        }
    })
}

pub(super) extern "win64" fn IIDFromString(s: *const u16, out: *mut Guid) -> HResult {
    ffi_guard!(CHANNEL, "IIDFromString", E_INVALIDARG, {
        // SAFETY: `s` est un `PCWSTR` valide ou nul.
        let text = unsafe { from_pcwstr(s) };
        if out.is_null() {
            return E_INVALIDARG;
        }
        match text.as_deref().map(parse_braced_guid) {
            None => {
                write_out(out, Guid::new(0, 0, 0, [0; 8]));
                S_OK
            }
            Some(Some(g)) => {
                write_out(out, g);
                S_OK
            }
            Some(None) => E_INVALIDARG,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_common::wide::to_wide_null;

    #[test]
    fn apartments_per_thread() {
        std::thread::spawn(|| {
            assert_eq!(CoInitializeEx(core::ptr::null_mut(), 0x2), S_OK);
            assert_eq!(CoInitializeEx(core::ptr::null_mut(), 0x2), S_FALSE);
            assert_eq!(
                CoInitializeEx(core::ptr::null_mut(), 0x0),
                RPC_E_CHANGED_MODE
            );
            assert_eq!(CoInitializeEx(core::ptr::dangling_mut(), 0x2), E_INVALIDARG);
            let clsid = Guid::new(1, 2, 3, [4; 8]);
            let mut ppv: *mut c_void = core::ptr::dangling_mut();
            assert_eq!(
                CoCreateInstance(
                    &clsid,
                    core::ptr::null_mut(),
                    1,
                    &crate::com::IID_IUNKNOWN,
                    &mut ppv
                ),
                REGDB_E_CLASSNOTREG
            );
            assert!(ppv.is_null());
            CoUninitialize();
            CoUninitialize();
            assert_eq!(APARTMENT.with(Cell::get).0, 0);
        })
        .join()
        .expect("thread");
    }

    #[test]
    fn guid_generation_and_strings() {
        let mut g = Guid::new(0, 0, 0, [0; 8]);
        assert_eq!(CoCreateGuid(&mut g), S_OK);
        assert_eq!(g.data3 >> 12, 4);
        assert_eq!(g.data4[0] & 0xC0, 0x80);
        let mut g2 = g;
        CoCreateGuid(&mut g2);
        assert_ne!(g, g2);

        let known = Guid::new(0x0000_0000, 0x0000, 0x0000, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
        let mut buf = [0u16; 39];
        assert_eq!(StringFromGUID2(&known, buf.as_mut_ptr(), 39), 39);
        assert_eq!(
            String::from_utf16_lossy(&buf[..38]),
            "{00000000-0000-0000-C000-000000000046}"
        );
        assert_eq!(StringFromGUID2(&known, buf.as_mut_ptr(), 38), 0);

        let mut parsed = Guid::new(9, 9, 9, [9; 8]);
        assert_eq!(CLSIDFromString(buf.as_ptr(), &mut parsed), S_OK);
        assert_eq!(parsed, known);
        let bad = to_wide_null("{nope}");
        assert_eq!(CLSIDFromString(bad.as_ptr(), &mut parsed), CO_E_CLASSSTRING);

        let mut sp: *mut u16 = core::ptr::null_mut();
        assert_eq!(StringFromCLSID(&g, &mut sp), S_OK);
        // SAFETY: `sp` alloué et terminé par nul.
        let s = unsafe { from_pcwstr(sp) }.expect("str");
        assert_eq!(parse_braced_guid(&s), Some(g));
        // SAFETY: mémoire malloc.
        unsafe { libc::free(sp.cast()) };
    }

    #[test]
    fn propvariant_clear_and_realloc() {
        let s = to_wide_null("text");
        // SAFETY: allocation malloc équivalente à CoTaskMemAlloc.
        let p = unsafe { libc::malloc(s.len() * 2) }.cast::<u16>();
        // SAFETY: `p` a la place pour `s`.
        unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), p, s.len()) };
        let mut pv = PropVariant {
            vt: VT_LPWSTR,
            reserved: [0; 3],
            data: [p as u64, 0],
        };
        assert_eq!(PropVariantClear(&mut pv), S_OK);
        assert_eq!((pv.vt, pv.data), (0, [0, 0]));
        let mut i4 = PropVariant {
            vt: 3,
            reserved: [0; 3],
            data: [42, 0],
        };
        assert_eq!(PropVariantClear(&mut i4), S_OK);
        assert_eq!(i4.data[0], 0);
        let mut bad = PropVariant {
            vt: 0x1003, // VT_VECTOR | VT_I4
            reserved: [0; 3],
            data: [0, 0],
        };
        assert_eq!(PropVariantClear(&mut bad), DISP_E_BADVARTYPE);
        assert_eq!(PropVariantClear(core::ptr::null_mut()), S_OK);

        let q = CoTaskMemRealloc(core::ptr::null_mut(), 32);
        assert!(!q.is_null());
        let q = CoTaskMemRealloc(q, 4096);
        assert!(!q.is_null());
        assert!(CoTaskMemRealloc(q, 0).is_null());
    }
}
