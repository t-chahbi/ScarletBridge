//! Helpers `std::` de `msvcp140.dll` : `_X*` (levée d'exceptions STL), `_Syserror_map`,
//! `_Winerror_map`, `_Random_device`, `_Throw_C_error`, `uncaught_exception`, `_Fiopen`.

use core::ffi::{CStr, c_void};
use core::ptr;
use std::collections::HashMap;
use std::sync::OnceLock;

use sb_common::{ffi_guard, sb_debug};

use super::{
    CHANNEL, MODE_APP, MODE_ATE, MODE_BINARY, MODE_IN, MODE_NOCREATE, MODE_NOREPLACE, MODE_OUT,
    MODE_TRUNC, fatal_throw,
};

// ---------- `_X*` ----------

/// `std::uncaught_exception()` : le runtime ne propage jamais d'exception C++ → toujours faux.
extern "win64" fn uncaught_exception() -> bool {
    false
}

// ---------- `_Syserror_map` / `_Winerror_map` ----------

/// Messages `generic_category` MSVC (`std::errc`, valeurs errno UCRT).
const SYSERR: &[(i32, &CStr)] = &[
    (1, c"operation not permitted"),
    (2, c"no such file or directory"),
    (3, c"no such process"),
    (4, c"interrupted"),
    (5, c"io error"),
    (6, c"no such device or address"),
    (7, c"argument list too long"),
    (8, c"executable format error"),
    (9, c"bad file descriptor"),
    (10, c"no child process"),
    (11, c"resource unavailable try again"),
    (12, c"not enough memory"),
    (13, c"permission denied"),
    (14, c"bad address"),
    (16, c"device or resource busy"),
    (17, c"file exists"),
    (18, c"cross device link"),
    (19, c"no such device"),
    (20, c"not a directory"),
    (21, c"is a directory"),
    (22, c"invalid argument"),
    (23, c"too many files open in system"),
    (24, c"too many files open"),
    (25, c"inappropriate io control operation"),
    (27, c"file too large"),
    (28, c"no space on device"),
    (29, c"invalid seek"),
    (30, c"read only file system"),
    (31, c"too many links"),
    (32, c"broken pipe"),
    (33, c"argument out of domain"),
    (34, c"result out of range"),
    (36, c"resource deadlock would occur"),
    (38, c"filename too long"),
    (39, c"no lock available"),
    (40, c"function not supported"),
    (41, c"directory not empty"),
    (42, c"illegal byte sequence"),
    (100, c"address in use"),
    (101, c"address not available"),
    (102, c"address family not supported"),
    (103, c"connection already in progress"),
    (104, c"bad message"),
    (105, c"operation canceled"),
    (106, c"connection aborted"),
    (107, c"connection refused"),
    (108, c"connection reset"),
    (109, c"destination address required"),
    (110, c"host unreachable"),
    (111, c"identifier removed"),
    (112, c"operation in progress"),
    (113, c"already connected"),
    (114, c"too many symbolic link levels"),
    (115, c"message size"),
    (116, c"network down"),
    (117, c"network reset"),
    (118, c"network unreachable"),
    (119, c"no buffer space"),
    (120, c"no message available"),
    (121, c"no link"),
    (122, c"no message"),
    (123, c"no protocol option"),
    (124, c"no stream resources"),
    (125, c"not a stream"),
    (126, c"not connected"),
    (127, c"state not recoverable"),
    (128, c"not a socket"),
    (129, c"not supported"),
    (130, c"operation not supported"),
    (132, c"value too large"),
    (133, c"owner dead"),
    (134, c"protocol error"),
    (135, c"protocol not supported"),
    (136, c"wrong protocol type"),
    (137, c"stream timeout"),
    (138, c"timed out"),
    (139, c"text file busy"),
    (140, c"operation would block"),
];

extern "win64" fn syserror_map(code: i32) -> *const i8 {
    SYSERR
        .iter()
        .find(|(c, _)| *c == code)
        .map_or(c"unknown error".as_ptr(), |(_, m)| m.as_ptr())
}

/// Correspondance `GetLastError()`/WSA → `errc` (table de `syserror.cpp`), 0 si inconnue.
const WINERR: &[(i32, i32)] = &[
    (1, 40),      // ERROR_INVALID_FUNCTION -> function_not_supported
    (2, 2),       // ERROR_FILE_NOT_FOUND
    (3, 2),       // ERROR_PATH_NOT_FOUND
    (4, 24),      // ERROR_TOO_MANY_OPEN_FILES
    (5, 13),      // ERROR_ACCESS_DENIED
    (6, 22),      // ERROR_INVALID_HANDLE
    (8, 12),      // ERROR_NOT_ENOUGH_MEMORY
    (12, 13),     // ERROR_INVALID_ACCESS
    (14, 12),     // ERROR_OUTOFMEMORY
    (15, 19),     // ERROR_INVALID_DRIVE
    (16, 13),     // ERROR_CURRENT_DIRECTORY
    (17, 18),     // ERROR_NOT_SAME_DEVICE
    (19, 13),     // ERROR_WRITE_PROTECT
    (20, 19),     // ERROR_BAD_UNIT
    (21, 11),     // ERROR_NOT_READY
    (25, 5),      // ERROR_SEEK
    (29, 5),      // ERROR_WRITE_FAULT
    (30, 5),      // ERROR_READ_FAULT
    (32, 13),     // ERROR_SHARING_VIOLATION
    (33, 39),     // ERROR_LOCK_VIOLATION
    (39, 28),     // ERROR_HANDLE_DISK_FULL
    (55, 19),     // ERROR_DEV_NOT_EXIST
    (80, 17),     // ERROR_FILE_EXISTS
    (82, 13),     // ERROR_CANNOT_MAKE
    (110, 5),     // ERROR_OPEN_FAILED
    (111, 38),    // ERROR_BUFFER_OVERFLOW
    (112, 28),    // ERROR_DISK_FULL
    (123, 2),     // ERROR_INVALID_NAME
    (131, 22),    // ERROR_NEGATIVE_SEEK
    (142, 16),    // ERROR_BUSY_DRIVE
    (145, 41),    // ERROR_DIR_NOT_EMPTY
    (170, 16),    // ERROR_BUSY
    (183, 17),    // ERROR_ALREADY_EXISTS
    (212, 39),    // ERROR_LOCKED
    (267, 22),    // ERROR_DIRECTORY
    (995, 105),   // ERROR_OPERATION_ABORTED
    (998, 13),    // ERROR_NOACCESS
    (1011, 5),    // ERROR_CANTOPEN
    (1012, 5),    // ERROR_CANTREAD
    (1013, 5),    // ERROR_CANTWRITE
    (1237, 11),   // ERROR_RETRY
    (2401, 16),   // ERROR_OPEN_FILES
    (2404, 16),   // ERROR_DEVICE_IN_USE
    (10004, 4),   // WSAEINTR
    (10009, 9),   // WSAEBADF
    (10013, 13),  // WSAEACCES
    (10014, 14),  // WSAEFAULT
    (10022, 22),  // WSAEINVAL
    (10024, 24),  // WSAEMFILE
    (10035, 140), // WSAEWOULDBLOCK
    (10036, 112), // WSAEINPROGRESS
    (10037, 103), // WSAEALREADY
    (10038, 128), // WSAENOTSOCK
    (10039, 109), // WSAEDESTADDRREQ
    (10040, 115), // WSAEMSGSIZE
    (10041, 136), // WSAEPROTOTYPE
    (10042, 123), // WSAENOPROTOOPT
    (10043, 135), // WSAEPROTONOSUPPORT
    (10045, 130), // WSAEOPNOTSUPP
    (10047, 102), // WSAEAFNOSUPPORT
    (10048, 100), // WSAEADDRINUSE
    (10049, 101), // WSAEADDRNOTAVAIL
    (10050, 116), // WSAENETDOWN
    (10051, 118), // WSAENETUNREACH
    (10052, 117), // WSAENETRESET
    (10053, 106), // WSAECONNABORTED
    (10054, 108), // WSAECONNRESET
    (10055, 119), // WSAENOBUFS
    (10056, 113), // WSAEISCONN
    (10057, 126), // WSAENOTCONN
    (10060, 138), // WSAETIMEDOUT
    (10061, 107), // WSAECONNREFUSED
    (10063, 38),  // WSAENAMETOOLONG
    (10065, 110), // WSAEHOSTUNREACH
];

extern "win64" fn winerror_map(code: i32) -> i32 {
    WINERR
        .iter()
        .find(|(w, _)| *w == code)
        .map_or(0, |(_, e)| *e)
}

// ---------- `_Random_device` ----------

/// `_Random_device()` : 32 bits d'entropie système (équivalent `rand_s`).
extern "win64" fn random_device() -> u32 {
    ffi_guard!(CHANNEL, "_Random_device", 0, {
        let mut v = [0u8; 4];
        let mut got = 0usize;
        while got < v.len() {
            // SAFETY: tampon local de 4 octets.
            let n = unsafe { libc::getrandom(v.as_mut_ptr().add(got).cast(), v.len() - got, 0) };
            if n <= 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                fatal_throw("std::runtime_error", "random_device failed");
            }
            got += n as usize;
        }
        u32::from_ne_bytes(v)
    })
}

// ---------- `_Fiopen` ----------

type FopenFn = unsafe extern "win64" fn(*const i8, *const i8) -> *mut c_void;
type FcloseFn = unsafe extern "win64" fn(*mut c_void) -> i32;
type FseekFn = unsafe extern "win64" fn(*mut c_void, i32, i32) -> i32;

/// Fonctions stdio du CRT HLE (le `FILE*` doit venir du même CRT que celui utilisé par
/// `basic_filebuf` côté invité ; l'ouverture passe par le VFS sandboxé de `fopen`).
fn crt_fns() -> Option<(FopenFn, FcloseFn, FseekFn)> {
    static FNS: OnceLock<Option<(usize, usize, usize)>> = OnceLock::new();
    let (o, c, s) = (*FNS.get_or_init(|| {
        let m = crate::ucrt::exports();
        Some((*m.get("fopen")?, *m.get("fclose")?, *m.get("fseek")?))
    }))?;
    // SAFETY: adresses de fonctions `extern "win64"` du CRT HLE aux signatures C standard.
    unsafe {
        Some((
            core::mem::transmute::<usize, FopenFn>(o),
            core::mem::transmute::<usize, FcloseFn>(c),
            core::mem::transmute::<usize, FseekFn>(s),
        ))
    }
}

/// Table `valid`/`mods` de `fiopen.cpp`.
fn fopen_mode(mode: i32) -> Option<&'static CStr> {
    const TABLE: &[(i32, &CStr)] = &[
        (MODE_OUT, c"w"),
        (MODE_OUT | MODE_TRUNC, c"w"),
        (MODE_OUT | MODE_APP, c"a"),
        (MODE_APP, c"a"),
        (MODE_IN, c"r"),
        (MODE_IN | MODE_OUT, c"r+"),
        (MODE_IN | MODE_OUT | MODE_TRUNC, c"w+"),
        (MODE_IN | MODE_OUT | MODE_APP, c"a+"),
        (MODE_IN | MODE_APP, c"a+"),
        (MODE_OUT | MODE_BINARY, c"wb"),
        (MODE_OUT | MODE_TRUNC | MODE_BINARY, c"wb"),
        (MODE_OUT | MODE_APP | MODE_BINARY, c"ab"),
        (MODE_APP | MODE_BINARY, c"ab"),
        (MODE_IN | MODE_BINARY, c"rb"),
        (MODE_IN | MODE_OUT | MODE_BINARY, c"r+b"),
        (MODE_IN | MODE_OUT | MODE_TRUNC | MODE_BINARY, c"w+b"),
        (MODE_IN | MODE_OUT | MODE_APP | MODE_BINARY, c"a+b"),
        (MODE_IN | MODE_APP | MODE_BINARY, c"a+b"),
    ];
    let key = mode & !(MODE_ATE | MODE_NOCREATE | MODE_NOREPLACE);
    TABLE.iter().find(|(m, _)| *m == key).map(|(_, s)| *s)
}

fn fiopen_impl(path: &CStr, mode: i32, prot: i32) -> *mut c_void {
    let Some((fopen, fclose, fseek)) = crt_fns() else {
        return ptr::null_mut();
    };
    let Some(m) = fopen_mode(mode) else {
        sb_debug!(CHANNEL, "_Fiopen : openmode invalide {mode:#x}");
        return ptr::null_mut();
    };
    // SAFETY: fonctions CRT HLE ; chemins/modes C valides.
    unsafe {
        if mode & MODE_NOCREATE != 0 {
            let fp = fopen(path.as_ptr(), c"r".as_ptr());
            if fp.is_null() {
                return ptr::null_mut();
            }
            fclose(fp);
        }
        if mode & MODE_NOREPLACE != 0 && mode & (MODE_OUT | MODE_APP) != 0 {
            let fp = fopen(path.as_ptr(), c"r".as_ptr());
            if !fp.is_null() {
                fclose(fp);
                return ptr::null_mut();
            }
        }
        let fp = fopen(path.as_ptr(), m.as_ptr());
        if fp.is_null() {
            return ptr::null_mut();
        }
        if mode & MODE_ATE != 0 && fseek(fp, 0, 2) != 0 {
            fclose(fp);
            return ptr::null_mut();
        }
        sb_debug!(
            CHANNEL,
            "_Fiopen('{}', {mode:#x}, prot={prot}) -> {fp:p}",
            path.to_string_lossy()
        );
        fp
    }
}

extern "win64" fn fiopen_a(path: *const i8, mode: i32, prot: i32) -> *mut c_void {
    ffi_guard!(CHANNEL, "_Fiopen(char)", ptr::null_mut(), {
        if path.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: chemin C fourni par `basic_filebuf::open`.
        fiopen_impl(unsafe { CStr::from_ptr(path) }, mode, prot)
    })
}

extern "win64" fn fiopen_w(path: *const u16, mode: i32, prot: i32) -> *mut c_void {
    ffi_guard!(CHANNEL, "_Fiopen(wchar_t)", ptr::null_mut(), {
        if path.is_null() {
            return ptr::null_mut();
        }
        let mut len = 0usize;
        // SAFETY: chaîne UTF-16 terminée par 0.
        while unsafe { *path.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` éléments lisibles.
        let s = String::from_utf16_lossy(unsafe { core::slice::from_raw_parts(path, len) });
        let Ok(c) = std::ffi::CString::new(s) else {
            return ptr::null_mut();
        };
        fiopen_impl(&c, mode, prot)
    })
}

pub fn register(m: &mut HashMap<&'static str, usize>) {
    macro_rules! reg {
        ($name:expr, $f:expr) => {
            m.insert($name, $f as *const () as usize);
        };
    }
    reg!("?uncaught_exception@std@@YA_NXZ", uncaught_exception);
    reg!("?_Syserror_map@std@@YAPEBDH@Z", syserror_map);
    reg!("?_Winerror_map@std@@YAHH@Z", winerror_map);
    reg!("?_Random_device@std@@YAIXZ", random_device);
    reg!("?_Fiopen@std@@YAPEAU_iobuf@@PEBDHH@Z", fiopen_a);
    reg!("?_Fiopen@std@@YAPEAU_iobuf@@PEB_WHH@Z", fiopen_w);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Installe un VFS de test (racines dans le répertoire temporaire) si aucun n'est actif :
    /// `fopen` du CRT HLE n'accepte que des chemins résolus par le VFS.
    fn ensure_vfs() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            let mut st = crate::process::process_state().write();
            if st.vfs.is_none() {
                let base =
                    std::env::temp_dir().join(format!("sb-msvcp-test-{}", std::process::id()));
                let game = base.join("game");
                std::fs::create_dir_all(&game).expect("racine game de test");
                st.vfs = Some(
                    sb_vfs::VirtualFs::new(&game, &base.join("save"), &base.join("temp"), None)
                        .expect("VFS de test"),
                );
            }
        });
    }

    #[test]
    fn error_maps_match_msvc_tables() {
        // SAFETY: chaînes statiques.
        unsafe {
            assert_eq!(
                CStr::from_ptr(syserror_map(2)),
                c"no such file or directory"
            );
            assert_eq!(CStr::from_ptr(syserror_map(13)), c"permission denied");
            assert_eq!(CStr::from_ptr(syserror_map(9999)), c"unknown error");
        }
        assert_eq!(winerror_map(5), 13);
        assert_eq!(winerror_map(2), 2);
        assert_eq!(winerror_map(10061), 107);
        assert_eq!(winerror_map(424242), 0);
        assert!(!uncaught_exception());
    }

    #[test]
    fn random_device_varies() {
        let a: Vec<u32> = (0..8).map(|_| random_device()).collect();
        assert!(a.iter().any(|&x| x != a[0]));
    }

    #[test]
    fn fiopen_modes_and_flags() {
        assert_eq!(fopen_mode(MODE_IN | MODE_BINARY), Some(c"rb"));
        assert_eq!(fopen_mode(MODE_OUT | MODE_ATE), Some(c"w"));
        assert_eq!(fopen_mode(MODE_IN | MODE_TRUNC), None);

        ensure_vfs();
        let guest = format!("temp:/sb_fiopen_{}.bin", std::process::id());
        let cpath = std::ffi::CString::new(guest.clone()).unwrap_or_default();
        let Some((_, fclose, _)) = crt_fns() else {
            panic!("CRT fopen indisponible");
        };
        // _Nocreate sur fichier absent -> échec
        assert!(fiopen_a(cpath.as_ptr(), MODE_OUT | MODE_NOCREATE, 0x40).is_null());
        // création
        let fp = fiopen_a(cpath.as_ptr(), MODE_OUT | MODE_BINARY, 0x40);
        assert!(!fp.is_null());
        // SAFETY: FILE* valide.
        unsafe { fclose(fp) };
        // _Noreplace sur fichier existant -> échec
        assert!(fiopen_a(cpath.as_ptr(), MODE_OUT | MODE_NOREPLACE, 0x40).is_null());
        // version large + ate
        let wide: Vec<u16> = guest.encode_utf16().chain(std::iter::once(0)).collect();
        let fp = fiopen_w(wide.as_ptr(), MODE_IN | MODE_OUT | MODE_ATE, 0x40);
        assert!(!fp.is_null());
        // SAFETY: FILE* valide.
        unsafe { fclose(fp) };
        // mode invalide
        assert!(fiopen_a(cpath.as_ptr(), MODE_TRUNC, 0x40).is_null());
    }
}
