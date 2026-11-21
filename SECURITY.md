# Security Policy

## Scope & Security Guarantees

ScarletBridge executes untrusted or third-party unencrypted Windows/Xbox PE32+ x86-64 binaries directly in a native Linux process. Security and isolation are first-class design requirements:

- **Strict VFS Sandboxing (`sb-vfs`)**: Virtual drive letters (`G:\`, `S:\`, `T:\`, `D:\`) are strictly confined to title-scoped directories. Path traversal (`..`), NUL bytes, and escaping symlinks are rejected. Host directories such as `/home` or `/etc` are never exposed.
- **Offline-by-Default (`SB_OFFLINE_MODE=1`)**: Network shims (`XCurl`, `libHttpClient.GDK`, WinSock/WinHTTP) deny outbound traffic by default.
- **No Unwinding Across FFI**: Every `extern "win64"` entry point is wrapped in `sb_common::ffi::ffi_guard!` to prevent undefined behavior from Rust panics unwinding into guest machine code.
- **Memory & ABI Safety**: `unsafe` code is isolated with explicit `// SAFETY:` invariants and compile-time `size_of`/`offset_of` checks for all `#[repr(C)]` Windows ABI structures.

## Reporting a Vulnerability

If you discover a security vulnerability (such as a VFS sandbox escape, memory safety bug in the PE loader or JIT trampolines, or unintended network access in offline mode):

1. **Do not open a public GitHub issue.**
2. Use GitHub's **Private Vulnerability Reporting** ("Security" tab -> "Report a vulnerability") on this repository.
3. Include a minimal reproduction (command line, affected crate/file, and expected vs. actual behavior).

Maintainers will acknowledge reports promptly and coordinate a fix before public disclosure.
