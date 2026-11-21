## Summary

<!-- Describe what this PR changes and why. Link related issues: Closes #123 -->

## Area

- [ ] `sb-gpu` (Vulkan renderer / DXBC → SPIR-V / DXIL / unified memory)
- [ ] `sb-hle` (Win32 / D3D12_XS / D3D11 / GDK / WinRT / XAudio2 / MSVC)
- [ ] `sb-loader` (PE32+ loader / TEB / TLS / JIT trampolines)
- [ ] `sb-vfs` / `sb-runtime` / `sb-common`
- [ ] `sb-launcher` / `sb-stubgen` / guest DLLs
- [ ] `tests/harness` / CI / Documentation

## Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] Cross-target `cargo clippy --target x86_64-pc-windows-gnu ... -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] Added or updated unit tests and/or PE32+ harness verification

## Mandatory Policy & ABI Checklist

- [ ] 100% pure Rust (Edition 2024) — no new `.c`/`.cpp` files added
- [ ] All FFI exports / COM methods use `extern "win64"` and `ffi_guard!` (no unwinding across FFI)
- [ ] Every `unsafe` block has a clear `// SAFETY:` justification
- [ ] Any new `#[repr(C)]` ABI struct includes compile-time `size_of`/`offset_of` assertions
- [ ] Clean-room & legal compliance: no DRM circumvention, no encryption keys, no copyrighted commercial assets
