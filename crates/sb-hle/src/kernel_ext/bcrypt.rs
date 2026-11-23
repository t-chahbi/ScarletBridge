//! `bcrypt.dll` (CNG) : fournisseur RNG uniquement (`BCRYPT_RNG_ALGORITHM`), alimenté par
//! `getrandom(2)`. Les autres algorithmes retournent `STATUS_NOT_FOUND`.

use std::collections::HashSet;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use sb_common::ffi_guard;
use sb_common::wide::from_pcwstr;

use super::{CHANNEL, write_out};

pub(crate) type NtStatus = i32;
pub(crate) const STATUS_SUCCESS: NtStatus = 0;
pub(crate) const STATUS_INVALID_HANDLE: NtStatus = 0xC000_0008_u32 as i32;
pub(crate) const STATUS_INVALID_PARAMETER: NtStatus = 0xC000_000D_u32 as i32;
pub(crate) const STATUS_NOT_FOUND: NtStatus = 0xC000_0225_u32 as i32;
pub(crate) const STATUS_UNSUCCESSFUL: NtStatus = 0xC000_0001_u32 as i32;

const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
const BCRYPT_RNG_USE_ENTROPY_IN_BUFFER: u32 = 0x0000_0001;
const BCRYPT_PROV_DISPATCH: u32 = 0x0000_0001;
const BCRYPT_HASH_REUSABLE_FLAG: u32 = 0x0000_0020;
/// Pseudo-handle `BCRYPT_RNG_ALG_HANDLE` (Windows 10+).
const BCRYPT_RNG_ALG_HANDLE: usize = 0x81;
const MS_PRIMITIVE_PROVIDER: &str = "Microsoft Primitive Provider";

static NEXT_ALG: AtomicUsize = AtomicUsize::new(0x5B_C000);
static RNG_HANDLES: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Remplit `buf` avec l'aléa du noyau.
pub(crate) fn fill_random(buf: &mut [u8]) -> bool {
    let mut filled = 0;
    while filled < buf.len() {
        // SAFETY: écrit au plus `len - filled` octets dans `buf`.
        let n =
            unsafe { libc::getrandom(buf[filled..].as_mut_ptr().cast(), buf.len() - filled, 0) };
        if n < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return false;
        }
        filled += n as usize;
    }
    true
}

pub(super) extern "win64" fn BCryptOpenAlgorithmProvider(
    out: *mut usize,
    alg_id: *const u16,
    implementation: *const u16,
    flags: u32,
) -> NtStatus {
    ffi_guard!(
        CHANNEL,
        "BCryptOpenAlgorithmProvider",
        STATUS_UNSUCCESSFUL,
        {
            if out.is_null() {
                return STATUS_INVALID_PARAMETER;
            }
            write_out(out, 0);
            // SAFETY: `alg_id` / `implementation` sont des `PCWSTR` valides ou nuls.
            let (Some(alg), imp) = (unsafe { from_pcwstr(alg_id) }, unsafe {
                from_pcwstr(implementation)
            }) else {
                return STATUS_INVALID_PARAMETER;
            };
            if (flags & !(BCRYPT_PROV_DISPATCH | BCRYPT_HASH_REUSABLE_FLAG | 0x8)) != 0 {
                return STATUS_INVALID_PARAMETER;
            }
            if let Some(imp) = imp
                && !imp.eq_ignore_ascii_case(MS_PRIMITIVE_PROVIDER)
            {
                return STATUS_NOT_FOUND;
            }
            if !alg.eq_ignore_ascii_case("RNG") {
                // SHA*/MD5/AES... ne sont pas fournis par ScarletBridge.
                return STATUS_NOT_FOUND;
            }
            if (flags & 0x8) != 0 {
                // BCRYPT_ALG_HANDLE_HMAC_FLAG n'a pas de sens pour un RNG.
                return STATUS_INVALID_PARAMETER;
            }
            let h = NEXT_ALG.fetch_add(0x10, Ordering::AcqRel);
            RNG_HANDLES.lock().insert(h);
            write_out(out, h);
            STATUS_SUCCESS
        }
    )
}

pub(super) extern "win64" fn BCryptCloseAlgorithmProvider(alg: usize, flags: u32) -> NtStatus {
    ffi_guard!(
        CHANNEL,
        "BCryptCloseAlgorithmProvider",
        STATUS_UNSUCCESSFUL,
        {
            if RNG_HANDLES.lock().remove(&alg) {
                STATUS_SUCCESS
            } else {
                STATUS_INVALID_HANDLE
            }
        }
    )
}

pub(super) extern "win64" fn BCryptGenRandom(
    alg: usize,
    buf: *mut u8,
    len: u32,
    flags: u32,
) -> NtStatus {
    ffi_guard!(CHANNEL, "BCryptGenRandom", STATUS_UNSUCCESSFUL, {
        if (flags & !(BCRYPT_USE_SYSTEM_PREFERRED_RNG | BCRYPT_RNG_USE_ENTROPY_IN_BUFFER)) != 0 {
            return STATUS_INVALID_PARAMETER;
        }
        if (flags & BCRYPT_USE_SYSTEM_PREFERRED_RNG) != 0 {
            if alg != 0 {
                return STATUS_INVALID_HANDLE;
            }
        } else if alg != BCRYPT_RNG_ALG_HANDLE && !RNG_HANDLES.lock().contains(&alg) {
            return STATUS_INVALID_HANDLE;
        }
        if len == 0 {
            return STATUS_SUCCESS;
        }
        if buf.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        // SAFETY: `buf` dispose de `len` octets en écriture (contrat BCryptGenRandom).
        let slice = unsafe { std::slice::from_raw_parts_mut(buf, len as usize) };
        if fill_random(slice) {
            STATUS_SUCCESS
        } else {
            STATUS_UNSUCCESSFUL
        }
    })
}

/// `advapi32!SystemFunction036` (`RtlGenRandom`) : BOOLEAN.
pub(super) extern "win64" fn SystemFunction036(buf: *mut u8, len: u32) -> u8 {
    ffi_guard!(CHANNEL, "SystemFunction036", 0, {
        if len == 0 {
            return 1;
        }
        if buf.is_null() {
            return 0;
        }
        // SAFETY: `buf` dispose de `len` octets en écriture.
        let slice = unsafe { std::slice::from_raw_parts_mut(buf, len as usize) };
        u8::from(fill_random(slice))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_common::wide::to_wide_null;

    #[test]
    fn rng_provider_lifecycle() {
        let rng = to_wide_null("RNG");
        let sha = to_wide_null("SHA256");
        let mut h = 0usize;
        assert_eq!(
            BCryptOpenAlgorithmProvider(&mut h, rng.as_ptr(), core::ptr::null(), 0),
            STATUS_SUCCESS
        );
        assert_ne!(h, 0);
        let mut a = [0u8; 64];
        let mut b = [0u8; 64];
        assert_eq!(BCryptGenRandom(h, a.as_mut_ptr(), 64, 0), STATUS_SUCCESS);
        assert_eq!(
            BCryptGenRandom(0, b.as_mut_ptr(), 64, BCRYPT_USE_SYSTEM_PREFERRED_RNG),
            STATUS_SUCCESS
        );
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 64]);
        assert_eq!(
            BCryptGenRandom(h, b.as_mut_ptr(), 64, BCRYPT_USE_SYSTEM_PREFERRED_RNG),
            STATUS_INVALID_HANDLE
        );
        assert_eq!(
            BCryptGenRandom(0, b.as_mut_ptr(), 64, 0),
            STATUS_INVALID_HANDLE
        );
        assert_eq!(
            BCryptGenRandom(BCRYPT_RNG_ALG_HANDLE, b.as_mut_ptr(), 8, 0),
            STATUS_SUCCESS
        );
        assert_eq!(
            BCryptGenRandom(h, core::ptr::null_mut(), 8, 0),
            STATUS_INVALID_PARAMETER
        );
        assert_eq!(BCryptCloseAlgorithmProvider(h, 0), STATUS_SUCCESS);
        assert_eq!(BCryptCloseAlgorithmProvider(h, 0), STATUS_INVALID_HANDLE);

        let mut h2 = 1usize;
        assert_eq!(
            BCryptOpenAlgorithmProvider(&mut h2, sha.as_ptr(), core::ptr::null(), 0),
            STATUS_NOT_FOUND
        );
        assert_eq!(h2, 0);
        assert_eq!(
            BCryptOpenAlgorithmProvider(core::ptr::null_mut(), rng.as_ptr(), core::ptr::null(), 0),
            STATUS_INVALID_PARAMETER
        );
        assert_eq!(SystemFunction036(a.as_mut_ptr(), 16), 1);
    }
}
