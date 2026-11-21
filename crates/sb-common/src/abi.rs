//! Types et constantes ABI Windows x86-64 / COM / GDK.

/// Code de retour COM / WinRT / GDK standard (`HRESULT`).
pub type HResult = i32;

/// Type booléen Win32 (`BOOL` = `i32`).
pub type WinBool = i32;

pub const WIN_FALSE: WinBool = 0;
pub const WIN_TRUE: WinBool = 1;

// Codes HRESULT standards.
pub const S_OK: HResult = 0x0000_0000;
pub const S_FALSE: HResult = 0x0000_0001;
pub const E_NOTIMPL: HResult = 0x8000_4001_u32 as i32;
pub const E_NOINTERFACE: HResult = 0x8000_4002_u32 as i32;
pub const E_POINTER: HResult = 0x8000_4003_u32 as i32;
pub const E_ABORT: HResult = 0x8000_4004_u32 as i32;
pub const E_FAIL: HResult = 0x8000_4005_u32 as i32;
pub const E_UNEXPECTED: HResult = 0x8000_FFFF_u32 as i32;
pub const E_ACCESSDENIED: HResult = 0x8007_0005_u32 as i32;
pub const E_HANDLE: HResult = 0x8007_0006_u32 as i32;
pub const E_OUTOFMEMORY: HResult = 0x8007_000E_u32 as i32;
pub const E_INVALIDARG: HResult = 0x8007_0057_u32 as i32;

// Raisons d'invocation de DllMain.
pub const DLL_PROCESS_DETACH: u32 = 0;
pub const DLL_PROCESS_ATTACH: u32 = 1;
pub const DLL_THREAD_ATTACH: u32 = 2;
pub const DLL_THREAD_DETACH: u32 = 3;

/// Retourne `true` si le `HRESULT` indique un succès (`hr >= 0`).
#[inline]
pub const fn succeeded(hr: HResult) -> bool {
    hr >= 0
}

/// Retourne `true` si le `HRESULT` indique un échec (`hr < 0`).
#[inline]
pub const fn failed(hr: HResult) -> bool {
    hr < 0
}

/// Nom symbolique lisible d'un `HRESULT` connu.
pub const fn hresult_name(hr: HResult) -> &'static str {
    match hr {
        S_OK => "S_OK",
        S_FALSE => "S_FALSE",
        E_NOTIMPL => "E_NOTIMPL",
        E_NOINTERFACE => "E_NOINTERFACE",
        E_POINTER => "E_POINTER",
        E_ABORT => "E_ABORT",
        E_FAIL => "E_FAIL",
        E_UNEXPECTED => "E_UNEXPECTED",
        E_ACCESSDENIED => "E_ACCESSDENIED",
        E_HANDLE => "E_HANDLE",
        E_OUTOFMEMORY => "E_OUTOFMEMORY",
        E_INVALIDARG => "E_INVALIDARG",
        _ => "HRESULT_UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hresult_checks() {
        assert!(succeeded(S_OK));
        assert!(succeeded(S_FALSE));
        assert!(!failed(S_OK));
        assert!(failed(E_NOTIMPL));
        assert!(failed(E_INVALIDARG));
        assert_eq!(hresult_name(E_NOTIMPL), "E_NOTIMPL");
        assert_eq!(hresult_name(S_OK), "S_OK");
    }
}
