//! Harnais PE32+ x86-64 de **conformité d'API** : appelle, à travers une vraie table d'imports PE
//! (résolution par nom et par ordinal, flottants dans XMM, fonctions variadiques), les API que le
//! bridge implémente en Rust : UCRT (chaînes, conversions, `printf`, maths, temps, `bsearch`),
//! Win32 (fichiers, mémoire, locale, temps, topologie), Winsock (loopback + offline strict),
//! `bcrypt` et `rpcrt4`. Les tests unitaires du bridge appellent ces fonctions directement en Rust ;
//! ce harnais vérifie en plus le chemin complet loader → IAT → `extern "win64"`.

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "[api_conformance] Compiler pour PE32+ x86-64 :\n\
         cargo build --target x86_64-pc-windows-gnu -p sb-harness-api-conformance"
    );
}

#[cfg(windows)]
fn main() {
    std::process::exit(win_impl::run());
}

#[cfg(windows)]
mod win_impl {
    use core::ffi::{c_char, c_int, c_void};
    use std::ffi::CStr;
    use std::ptr;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Tm {
        sec: c_int,
        min: c_int,
        hour: c_int,
        mday: c_int,
        mon: c_int,
        year: c_int,
        wday: c_int,
        yday: c_int,
        isdst: c_int,
    }

    #[link(name = "api-ms-win-crt-convert-l1-1-0", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn strtol(s: *const c_char, end: *mut *mut c_char, base: c_int) -> i32;
        fn strtoul(s: *const c_char, end: *mut *mut c_char, base: c_int) -> u32;
        fn _strtoi64(s: *const c_char, end: *mut *mut c_char, base: c_int) -> i64;
        fn strtod(s: *const c_char, end: *mut *mut c_char) -> f64;
        fn atof(s: *const c_char) -> f64;
        fn _itoa(v: c_int, buf: *mut c_char, radix: c_int) -> *mut c_char;
    }

    #[link(name = "api-ms-win-crt-runtime-l1-1-0", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn _errno() -> *mut c_int;
    }

    #[link(name = "api-ms-win-crt-string-l1-1-0", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn strnlen(s: *const c_char, max: usize) -> usize;
        fn strcmp(a: *const c_char, b: *const c_char) -> c_int;
        fn _stricmp(a: *const c_char, b: *const c_char) -> c_int;
        fn strcpy_s(dst: *mut c_char, n: usize, src: *const c_char) -> c_int;
        fn strcat_s(dst: *mut c_char, n: usize, src: *const c_char) -> c_int;
        fn strtok_s(s: *mut c_char, delim: *const c_char, ctx: *mut *mut c_char) -> *mut c_char;
        fn toupper(c: c_int) -> c_int;
    }

    #[link(name = "api-ms-win-crt-stdio-l1-1-0", kind = "raw-dylib")]
    unsafe extern "win64" {
        /// `va_list` Win64 = pointeur vers le tableau d'arguments (8 octets chacun).
        fn __stdio_common_vsprintf(
            options: u64,
            buf: *mut c_char,
            size: usize,
            fmt: *const c_char,
            locale: *const c_void,
            args: *const u64,
        ) -> c_int;
    }

    #[link(name = "api-ms-win-crt-math-l1-1-0", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn atan2(y: f64, x: f64) -> f64;
        fn fmod(x: f64, y: f64) -> f64;
        fn exp2(x: f64) -> f64;
        fn log2(x: f64) -> f64;
        fn cbrtf(x: f32) -> f32;
    }

    #[link(name = "api-ms-win-crt-time-l1-1-0", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn _time64(t: *mut i64) -> i64;
        fn _gmtime64(t: *const i64) -> *mut Tm;
        fn _mkgmtime64(tm: *mut Tm) -> i64;
        fn strftime(buf: *mut c_char, n: usize, fmt: *const c_char, tm: *const Tm) -> usize;
    }

    #[link(name = "api-ms-win-crt-utility-l1-1-0", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn bsearch(
            key: *const c_void,
            base: *const c_void,
            n: usize,
            size: usize,
            cmp: unsafe extern "win64" fn(*const c_void, *const c_void) -> c_int,
        ) -> *mut c_void;
    }

    #[link(name = "kernel32", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn VirtualQuery(addr: *const c_void, info: *mut [u64; 6], len: usize) -> usize;
        fn FormatMessageW(
            flags: u32,
            source: *const c_void,
            id: u32,
            lang: u32,
            buf: *mut u16,
            size: u32,
            args: *const c_void,
        ) -> u32;
        fn GetSystemTime(st: *mut [u16; 8]);
        fn SystemTimeToFileTime(st: *const [u16; 8], ft: *mut [u32; 2]) -> i32;
        fn FileTimeToSystemTime(ft: *const [u32; 2], st: *mut [u16; 8]) -> i32;
        fn GetLogicalProcessorInformation(buf: *mut c_void, len: *mut u32) -> i32;
        fn GetLastError() -> u32;
        fn IsProcessorFeaturePresent(feature: u32) -> i32;
        fn GlobalMemoryStatusEx(st: *mut [u64; 8]) -> i32;
        fn lstrcmpW(a: *const u16, b: *const u16) -> c_int;
        fn MultiByteToWideChar(
            cp: u32,
            flags: u32,
            src: *const c_char,
            n: c_int,
            dst: *mut u16,
            dn: c_int,
        ) -> c_int;
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct SockAddrIn {
        family: u16,
        port: u16,
        addr: u32,
        zero: [u8; 8],
    }

    #[link(name = "ws2_32", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn WSAStartup(version: u16, data: *mut [u8; 408]) -> c_int;
        fn WSAGetLastError() -> c_int;
        fn socket(af: c_int, ty: c_int, proto: c_int) -> usize;
        fn bind(s: usize, name: *const SockAddrIn, len: c_int) -> c_int;
        fn listen(s: usize, backlog: c_int) -> c_int;
        fn accept(s: usize, addr: *mut SockAddrIn, len: *mut c_int) -> usize;
        fn connect(s: usize, name: *const SockAddrIn, len: c_int) -> c_int;
        fn getsockname(s: usize, name: *mut SockAddrIn, len: *mut c_int) -> c_int;
        fn send(s: usize, buf: *const c_char, len: c_int, flags: c_int) -> c_int;
        fn recv(s: usize, buf: *mut c_char, len: c_int, flags: c_int) -> c_int;
        fn closesocket(s: usize) -> c_int;
        fn gethostname(name: *mut c_char, len: c_int) -> c_int;
        // Ordinaux officiels de ws2_32.dll : 9 = htons, 115 = WSAStartup.
        #[link_ordinal(9)]
        fn htons_by_ordinal(v: u16) -> u16;
        #[link_ordinal(115)]
        fn wsa_startup_by_ordinal(version: u16, data: *mut [u8; 408]) -> c_int;
    }

    #[link(name = "bcrypt", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn BCryptGenRandom(alg: *mut c_void, buf: *mut u8, len: u32, flags: u32) -> i32;
    }

    #[link(name = "rpcrt4", kind = "raw-dylib")]
    unsafe extern "win64" {
        fn UuidCreate(uuid: *mut [u8; 16]) -> i32;
        fn UuidToStringA(uuid: *const [u8; 16], out: *mut *mut u8) -> i32;
        fn RpcStringFreeA(s: *mut *mut u8) -> i32;
    }

    static mut FAILURES: u32 = 0;
    static mut CHECKS: u32 = 0;

    fn check(ok: bool, what: &str) {
        // SAFETY: harnais mono-thread.
        unsafe {
            CHECKS += 1;
            if !ok {
                FAILURES += 1;
                println!("[api_conformance] ÉCHEC : {what}");
            }
        }
    }

    unsafe extern "win64" fn cmp_i32(a: *const c_void, b: *const c_void) -> c_int {
        // SAFETY: pointeurs vers des `i32` du tableau trié ci-dessous.
        let (x, y) = unsafe { (*(a as *const i32), *(b as *const i32)) };
        x.cmp(&y) as c_int
    }

    fn cstr(buf: &[c_char]) -> String {
        // SAFETY: tampons nul-terminés remplis par les API testées.
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    pub fn run() -> i32 {
        println!("[api_conformance] === Conformité d'API via table d'imports PE ===");
        // SAFETY: appels FFI avec des tampons locaux valides.
        unsafe {
            // --- UCRT : conversions et errno
            let mut end: *mut c_char = ptr::null_mut();
            check(
                strtol(c"  -0x1F".as_ptr(), &mut end, 0) == -31,
                "strtol base 0 hex négatif",
            );
            check(
                strtol(c"777".as_ptr(), ptr::null_mut(), 8) == 511,
                "strtol base 8",
            );
            check(
                strtoul(c"4294967295".as_ptr(), ptr::null_mut(), 10) == u32::MAX,
                "strtoul max",
            );
            *_errno() = 0;
            check(
                strtol(c"99999999999".as_ptr(), ptr::null_mut(), 10) == i32::MAX,
                "strtol sature",
            );
            check(*_errno() == 34, "strtol ERANGE");
            check(
                _strtoi64(c"-9223372036854775808".as_ptr(), ptr::null_mut(), 10) == i64::MIN,
                "_strtoi64 min",
            );
            let d = strtod(c"1.5e3xyz".as_ptr(), &mut end);
            check(d == 1500.0 && *end == b'x' as c_char, "strtod + endptr");
            check((atof(c"0.1".as_ptr()) - 0.1).abs() < 1e-15, "atof");
            let mut buf = [0 as c_char; 64];
            _itoa(-255, buf.as_mut_ptr(), 16);
            // UCRT : le signe `-` n'existe que pour la base 10 ; sinon la valeur est non signée.
            check(
                cstr(&buf) == "ffffff01",
                &format!("_itoa(-255, 16) = {:?}", cstr(&buf)),
            );
            _itoa(-255, buf.as_mut_ptr(), 10);
            check(cstr(&buf) == "-255", "_itoa base 10 négatif");

            // --- UCRT : chaînes
            check(
                strnlen(c"scarlet".as_ptr(), 64) == 7 && strnlen(c"scarlet".as_ptr(), 3) == 3,
                "strnlen",
            );
            check(strcmp(c"a".as_ptr(), c"b".as_ptr()) < 0, "strcmp");
            check(
                _stricmp(c"ScArLeT".as_ptr(), c"sCaRlEt".as_ptr()) == 0,
                "_stricmp",
            );
            check(toupper(b'q' as c_int) == b'Q' as c_int, "toupper");
            strcpy_s(buf.as_mut_ptr(), 64, c"bridge".as_ptr());
            strcat_s(buf.as_mut_ptr(), 64, c"-rs".as_ptr());
            check(cstr(&buf) == "bridge-rs", "strcpy_s + strcat_s");
            let mut small = [0 as c_char; 4];
            check(
                strcpy_s(small.as_mut_ptr(), 4, c"trop long".as_ptr()) != 0,
                "strcpy_s refuse un tampon trop petit",
            );
            let mut src: Vec<c_char> = b"a,b;c\0".iter().map(|b| *b as c_char).collect();
            let mut ctx: *mut c_char = ptr::null_mut();
            let mut got = Vec::new();
            let mut t = strtok_s(src.as_mut_ptr(), c",;".as_ptr(), &mut ctx);
            while !t.is_null() {
                got.push(CStr::from_ptr(t).to_string_lossy().into_owned());
                t = strtok_s(ptr::null_mut(), c",;".as_ptr(), &mut ctx);
            }
            check(got == ["a", "b", "c"], "strtok_s");

            // --- UCRT : printf (variadique : va_list = tableau d'arguments)
            let args: [u64; 4] = [42, c"hi".as_ptr() as u64, 3.14159_f64.to_bits(), 255];
            let n = __stdio_common_vsprintf(
                0,
                buf.as_mut_ptr(),
                64,
                c"%d %s %6.2f %x".as_ptr(),
                ptr::null(),
                args.as_ptr(),
            );
            check(
                cstr(&buf) == "42 hi   3.14 ff" && n == 15,
                "vsprintf %d %s %f %x",
            );

            // --- UCRT : maths (arguments et résultats flottants dans XMM)
            check(
                (atan2(1.0, 1.0) - core::f64::consts::FRAC_PI_4).abs() < 1e-12,
                "atan2",
            );
            check((fmod(10.5, 3.0) - 1.5).abs() < 1e-12, "fmod");
            check(exp2(10.0) == 1024.0 && log2(8.0) == 3.0, "exp2 / log2");
            check((cbrtf(27.0) - 3.0).abs() < 1e-6, "cbrtf (f32)");

            // --- UCRT : temps
            let epoch: i64 = 1_700_000_000; // 2023-11-14 22:13:20 UTC
            let tm = _gmtime64(&epoch);
            check(
                !tm.is_null() && (*tm).year == 123 && (*tm).mon == 10 && (*tm).mday == 14,
                "_gmtime64",
            );
            let mut copy = *tm;
            check(_mkgmtime64(&mut copy) == epoch, "_mkgmtime64 aller-retour");
            let n = strftime(buf.as_mut_ptr(), 64, c"%Y-%m-%d %H:%M:%S".as_ptr(), tm);
            check(n == 19 && cstr(&buf) == "2023-11-14 22:13:20", "strftime");
            check(_time64(ptr::null_mut()) > epoch, "_time64");

            // --- UCRT : bsearch avec rappel win64
            let sorted = [1, 3, 5, 7, 9, 11];
            let key = 7;
            let hit = bsearch(
                (&raw const key).cast(),
                sorted.as_ptr().cast(),
                sorted.len(),
                4,
                cmp_i32,
            );
            check(!hit.is_null() && *(hit as *const i32) == 7, "bsearch");
            let miss = 8;
            check(
                bsearch(
                    (&raw const miss).cast(),
                    sorted.as_ptr().cast(),
                    6,
                    4,
                    cmp_i32,
                )
                .is_null(),
                "bsearch absent",
            );

            // --- Win32
            let mut mbi = [0u64; 6];
            check(
                VirtualQuery(sorted.as_ptr().cast(), &mut mbi, 48) == 48,
                "VirtualQuery taille",
            );
            check(
                mbi[3] >> 32 == 0x1000 || (mbi[3] & 0xFFFF_FFFF) != 0,
                "VirtualQuery état renseigné",
            );
            let mut wide = [0u16; 256];
            let n = FormatMessageW(
                0x1200,
                ptr::null(),
                2,
                0,
                wide.as_mut_ptr(),
                256,
                ptr::null(),
            );
            check(
                n > 0
                    && String::from_utf16_lossy(&wide[..n as usize])
                        .to_lowercase()
                        .contains("file"),
                "FormatMessageW(2)",
            );
            let mut st = [0u16; 8];
            GetSystemTime(&mut st);
            let mut ft = [0u32; 2];
            check(
                SystemTimeToFileTime(&st, &mut ft) != 0,
                "SystemTimeToFileTime",
            );
            let mut st2 = [0u16; 8];
            check(
                FileTimeToSystemTime(&ft, &mut st2) != 0 && st2 == st,
                "FILETIME <-> SYSTEMTIME",
            );
            let mut len = 0u32;
            check(
                GetLogicalProcessorInformation(ptr::null_mut(), &mut len) == 0
                    && len > 0
                    && GetLastError() == 122,
                "GetLogicalProcessorInformation taille requise",
            );
            check(
                IsProcessorFeaturePresent(10) != 0,
                "IsProcessorFeaturePresent(SSE2)",
            );
            let mut ms = [0u64; 8];
            ms[0] = 64;
            check(
                GlobalMemoryStatusEx(&mut ms) != 0 && ms[1] >> 32 > 0 || ms[2] > 0,
                "GlobalMemoryStatusEx",
            );
            let a: Vec<u16> = "abc\0".encode_utf16().collect();
            let b: Vec<u16> = "abd\0".encode_utf16().collect();
            check(lstrcmpW(a.as_ptr(), b.as_ptr()) < 0, "lstrcmpW");
            let mut w = [0u16; 16];
            let n = MultiByteToWideChar(65001, 0, c"héllo".as_ptr(), -1, w.as_mut_ptr(), 16);
            check(n == 6 && w[1] == 0xE9, "MultiByteToWideChar UTF-8");

            // --- Winsock : par nom et par ordinal, loopback puis offline strict
            let mut wsa = [0u8; 408];
            check(WSAStartup(0x0202, &mut wsa) == 0, "WSAStartup");
            check(
                wsa_startup_by_ordinal(0x0202, &mut wsa) == 0,
                "WSAStartup par ordinal 115",
            );
            check(htons_by_ordinal(0x1234) == 0x3412, "htons par ordinal 9");
            let srv = socket(2, 1, 0);
            let loopback = SockAddrIn {
                family: 2,
                port: 0,
                addr: u32::from_le_bytes([127, 0, 0, 1]),
                zero: [0; 8],
            };
            check(
                bind(srv, &loopback, 16) == 0 && listen(srv, 1) == 0,
                "bind + listen loopback",
            );
            let mut bound = SockAddrIn::default();
            let mut blen = 16;
            check(
                getsockname(srv, &mut bound, &mut blen) == 0 && bound.port != 0,
                "getsockname",
            );
            let cli = socket(2, 1, 0);
            check(connect(cli, &bound, 16) == 0, "connect loopback");
            let conn = accept(srv, ptr::null_mut(), ptr::null_mut());
            check(send(cli, c"ping".as_ptr(), 4, 0) == 4, "send");
            let mut rb = [0 as c_char; 8];
            check(
                recv(conn, rb.as_mut_ptr(), 8, 0) == 4 && cstr(&rb[..4]).starts_with("ping"),
                "recv",
            );
            closesocket(conn);
            closesocket(cli);
            closesocket(srv);
            // Hors ligne strict : aucune destination non locale (WSAENETUNREACH 10051).
            let external = SockAddrIn {
                family: 2,
                port: 0x3500,
                addr: u32::from_le_bytes([8, 8, 8, 8]),
                zero: [0; 8],
            };
            let s = socket(2, 1, 0);
            check(
                connect(s, &external, 16) != 0 && WSAGetLastError() == 10051,
                "connect externe refusé (offline strict)",
            );
            closesocket(s);
            let mut host = [0 as c_char; 64];
            check(
                gethostname(host.as_mut_ptr(), 64) == 0 && !cstr(&host).is_empty(),
                "gethostname (nom virtuel)",
            );

            // --- bcrypt / rpcrt4
            let mut rnd = [0u8; 32];
            check(
                BCryptGenRandom(ptr::null_mut(), rnd.as_mut_ptr(), 32, 2) == 0
                    && rnd.iter().any(|b| *b != 0),
                "BCryptGenRandom",
            );
            let mut uuid = [0u8; 16];
            check(
                UuidCreate(&mut uuid) == 0 && (uuid[7] >> 4) == 4,
                "UuidCreate v4",
            );
            let mut text: *mut u8 = ptr::null_mut();
            check(
                UuidToStringA(&uuid, &mut text) == 0 && !text.is_null(),
                "UuidToStringA",
            );
            check(
                CStr::from_ptr(text.cast()).to_bytes().len() == 36,
                "UUID texte 36 caractères",
            );
            RpcStringFreeA(&mut text);
        }

        // SAFETY: harnais mono-thread.
        let (checks, failures) = unsafe { (CHECKS, FAILURES) };
        println!("[api_conformance] {checks} vérifications, {failures} échec(s)");
        if failures == 0 {
            println!("[api_conformance] === CONFORMITÉ D'API VÉRIFIÉE ===");
            0
        } else {
            1
        }
    }
}
