//! Utilitaires de conversion UTF-16LE (chaînes Windows `PCWSTR` / `PWSTR`) ↔ UTF-8 Rust.

/// Convertit un pointeur `PCWSTR` (UTF-16LE terminé par `0`) en `String` UTF-8.
///
/// # Safety
/// Si `ptr` est non-nul, il doit pointer vers une chaîne UTF-16 valide et terminée par `0`,
/// lisible jusqu'au terminateur inclus.
pub unsafe fn from_pcwstr(ptr: *const u16) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    // SAFETY: l'appelant garantit la validité jusqu'au terminateur nul.
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` unités UTF-16 valides sont garanties lisibles.
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    Some(String::from_utf16_lossy(slice))
}

/// Encode une chaîne UTF-8 en vecteur UTF-16LE terminé par un nul.
pub fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copie une chaîne UTF-8 dans un tampon UTF-16LE terminé par un nul.
/// Retourne le nombre d'unités UTF-16 écrites (hors nul terminal).
///
/// # Safety
/// Si `dst` est non-nul et `cap > 0`, `dst` doit être valide en écriture pour `cap` éléments `u16`.
pub unsafe fn write_wide_buf(s: &str, dst: *mut u16, cap: usize) -> usize {
    if dst.is_null() || cap == 0 {
        return 0;
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    let count = units.len().min(cap - 1);
    // SAFETY: `count < cap` et `dst` dispose d'au moins `cap` emplacements.
    unsafe {
        std::ptr::copy_nonoverlapping(units.as_ptr(), dst, count);
        *dst.add(count) = 0;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_utf16_ascii_and_unicode() {
        let wide = to_wide_null("ScarletBridge — éàü 🎮");
        // SAFETY: `wide` provient de `to_wide_null` et se termine par 0.
        let decoded = unsafe { from_pcwstr(wide.as_ptr()) };
        assert_eq!(decoded.as_deref(), Some("ScarletBridge — éàü 🎮"));
    }

    #[test]
    fn null_pointer_returns_none() {
        // SAFETY: pointeur nul géré explicitement.
        let decoded = unsafe { from_pcwstr(std::ptr::null()) };
        assert!(decoded.is_none());
    }

    #[test]
    fn write_wide_truncates_safely() {
        let mut buf = [0xFFFFu16; 4];
        // SAFETY: `buf` a une capacité de 4 u16.
        let written = unsafe { write_wide_buf("Scarlet", buf.as_mut_ptr(), buf.len()) };
        assert_eq!(written, 3);
        assert_eq!(buf[3], 0);
        // SAFETY: `buf` est terminé par 0.
        assert_eq!(unsafe { from_pcwstr(buf.as_ptr()) }.as_deref(), Some("Sca"));
    }
}
