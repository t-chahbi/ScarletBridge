//! Garde-fous FFI empêchant tout unwind de panic Rust à travers une frontière `extern "win64"`.

use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::sb_err;

/// Exécute `f` en interceptant tout panic pour éviter un UB à travers la frontière ABI.
pub fn guard<R, F>(channel: &str, fn_name: &str, on_panic: R, f: F) -> R
where
    F: FnOnce() -> R,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => value,
        Err(payload) => {
            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "<payload non affichable>".to_string()
            };
            sb_err!(
                channel,
                "PANIC intercepté à la frontière FFI dans {fn_name}: {msg}"
            );
            on_panic
        }
    }
}

/// Macro ergonomique entourant le corps d'une fonction `extern "win64"`.
#[macro_export]
macro_rules! ffi_guard {
    ($ch:expr, $fn_name:expr, $on_panic:expr, $body:block) => {
        $crate::ffi::guard($ch, $fn_name, $on_panic, || $body)
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::E_UNEXPECTED;

    #[test]
    fn guard_catches_panic_and_returns_fallback() {
        let result = guard("test", "panicking_fn", E_UNEXPECTED, || {
            panic!("boom");
        });
        assert_eq!(result, E_UNEXPECTED);
    }

    #[test]
    fn guard_passes_through_ok_value() {
        let result = guard("test", "ok_fn", -1, || 42);
        assert_eq!(result, 42);
    }
}
