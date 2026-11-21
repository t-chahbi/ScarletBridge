# Contributing to ScarletBridge

Thank you for considering contributing to **ScarletBridge**! Every bug report, shader opcode, Win32/GDK API shim, Vulkan feature, documentation improvement, or PE32+ test harness helps push pure-Rust native Linux compatibility forward.

---

## Clean-Room & Legal Policy (Mandatory)

Before contributing, please note our strict project boundaries:

1. **Unencrypted binaries only:** ScarletBridge targets unencrypted PE32+ x86-64 homebrew, open-source games/engines, SDK samples, and custom test harnesses.
2. **Zero DRM circumvention:** Do **not** submit decryption code, cryptographic keys, DRM bypasses, or workarounds for protected commercial software.
3. **Zero copyrighted proprietary assets or code:** Never commit proprietary Microsoft/Xbox headers, SDK libraries, or commercial game files. All ABI layouts and implementations must be clean-room Rust derived from public documentation and black-box testing.

---

## High-Impact Areas Looking for Contributors

Not sure where to start? Pick a track that matches your interests:

### 1. Good First Issues (Great Entry Points)
- **Win32 / UCRT / GDK HLE shims** (`crates/sb-hle/`): Implement missing Win32 APIs reported as `[STUB][jit]` when running open-source PE32+ binaries, accompanied by a unit test or harness assertion.
- **Additional SM4/SM5 DXBC opcodes** (`crates/sb-gpu/src/dxbc_to_spirv.rs`): Add integer/bitwise/flow-control opcodes (`IF`/`ELSE`/`ENDIF`, `LOOP`/`BREAKC`, `SAMPLE_C`, `LD`, `RESINFO`, `BFREV`, etc.) validated against `spirv-val --target-env vulkan1.3`.
- **New PE32+ test harnesses** (`tests/harness/`): Small cross-compiled `.exe` tests exercising edge cases in COM, threading, VFS I/O, D3D11, or D3D12_XS.

### 2. Graphics, Shaders & Vulkan (`crates/sb-gpu/`, `crates/sb-hle/src/d3d12_xs.rs`, `d3d11.rs`)
- **DXIL SM 6.x → SPIR-V 1.3 translator** (`crates/sb-gpu/src/dxil.rs`): Expand the existing LLVM 3.7 bitstream parser to decode `FUNCTION` blocks & `dx.op.*` intrinsics into SPIR-V 1.3.
- **D3D11 InputLayout & Texture upload parity** (`crates/sb-hle/src/d3d11.rs`): Parse `D3D11_INPUT_ELEMENT_DESC`, `CreateTexture2D(pInitialData)`, and blend/depth states to drive full 3D rendering in open-source UWP/Xbox engines (e.g. OpenLara).
- **Multiple Render Targets (MRT) & Deferred Rendering**: Extend `VulkanRenderer` beyond RT0 to support 4–8 color attachments, HDR formats (`R16G16B16A16_SFLOAT`), and `OMSetRenderTargets` offscreen passes.
- **Dynamic Descriptor Tables & Samplers**: Support per-draw `VkSampler` states (point/anisotropic/clamp/comparison) and dynamic descriptor table offsets.
- **Vulkan Ray Tracing (`VK_KHR_acceleration_structure` / `VK_KHR_ray_tracing_pipeline`)**: Wire `XboxBvhBuilder` & DXR calls to hardware Vulkan RT when available.

### 3. Systems, Loader, Audio & Input (`crates/sb-loader/`, `crates/sb-hle/`)
- **SEH / Vectored Exception Handling (`RtlAddFunctionTable`, `.pdata`)**: Improve Windows x86-64 structured exception handling and C++ exception unwinding.
- **XAudio2 DSP & XMA2 Audio Decoding** (`crates/sb-hle/src/xaudio2.rs`, `crates/dlls/xaudio2_9/`): Mixing submix voices, pitch/volume filters, and pure-Rust XMA2 decoding over PipeWire/SDL3.
- **Keyboard/Mouse → WinRT `CoreWindow` Input**: Wire SDL3 keyboard and mouse events alongside existing SDL3 Gamepad support.

---

## Development Setup

### Option A: `mise` (Recommended — zero manual toolchain setup)

```bash
git clone <REPO_URL>
cd ScarletBridge
mise install
mise exec -- cargo test --workspace
```

`mise.toml` pins stable Rust and Zig (used transparently by `scripts/win64-linker.sh` as a zero-config cross-linker for `x86_64-pc-windows-gnu`).

### Option B: Native System Toolchains

Install:
- Rust stable (`rustup target add x86_64-pc-windows-gnu`)
- `mingw-w64` (`x86_64-w64-mingw32-gcc`) + `llvm` (`llvm-dlltool`)
- `spirv-tools` (`spirv-val`) for SPIR-V validation tests
- Vulkan loader + driver (or Mesa `lavapipe` for headless CI/software Vulkan)

---

## Mandatory Engineering Standards

To keep ScarletBridge maintainable, safe, and bug-free, every PR must follow these rules (see also [AGENTS.md](AGENTS.md)):

1. **100% Pure Rust (Edition 2024):** Do not add `.c`/`.cpp` files for shims or tooling.
2. **Strict Windows x86-64 ABI:**
   - All guest-callable exports and COM vtable methods use `extern "win64"`.
   - Wrap FFI entry points in `sb_common::ffi::ffi_guard!` — **never** allow a Rust panic to unwind across FFI into guest x86-64 code.
   - Isolate `unsafe` blocks and document every one with a `// SAFETY:` comment.
   - Verify all `#[repr(C)]` Windows/D3D12 ABI structs with compile-time `const _: () = { assert!(size_of::<...>() == ...); assert!(offset_of!(...) == ...); };`.
3. **No `.unwrap()` in library code:** `clippy::unwrap_used = "deny"` is enforced workspace-wide.
4. **Security & Isolation:** Never weaken `sb-vfs` sandbox checks or disable `SB_OFFLINE_MODE=1` by default.
5. **Test-Driven Changes:** Every new API, shader feature, or bug fix must include a `cargo test` unit test and/or a PE32+ harness verification under `tests/harness/`.

---

## Local Verification Checklist (Before Opening a PR)

Run all checks locally — GitHub Actions CI runs the exact same suite:

```bash
# 1. Formatting
mise exec -- cargo fmt --all -- --check

# 2. Native Linux Clippy (zero warnings)
mise exec -- cargo clippy --workspace --all-targets -- -D warnings

# 3. Cross-Target Win64 Clippy (all PE32+ DLLs & test harnesses)
mise exec -- cargo clippy --target x86_64-pc-windows-gnu \
  -p sb-dll-xmem -p sb-dll-xaudio2-9 -p sb-dll-vccorlib140-app \
  -p sb-dll-vcruntime140-app -p sb-dll-msvcp140-app \
  -p sb-harness-dll-probe -p sb-harness-seriesx-smoke \
  -p sb-harness-mt-io-stress -p sb-harness-d3d12-triangle \
  -p sb-harness-d3d12-cube -p sb-harness-aaa-pipeline-bench \
  -- -D warnings

# 4. Full Workspace Unit & Live GPU Tests
mise exec -- cargo test --workspace

# 5. Cross-Build & Run PE32+ Harnesses Natively (Headless)
mise exec -- cargo build -p sb-launcher
mise exec -- cargo build --target x86_64-pc-windows-gnu \
  -p sb-dll-xmem -p sb-harness-dll-probe -p sb-harness-seriesx-smoke \
  -p sb-harness-mt-io-stress -p sb-harness-d3d12-triangle \
  -p sb-harness-d3d12-cube -p sb-harness-aaa-pipeline-bench

SB_HEADLESS=1 ./target/debug/scarletbridge run target/x86_64-pc-windows-gnu/debug/dll_probe.exe -- xmem.dll --self-test-xmem
SB_HEADLESS=1 ./target/debug/scarletbridge run target/x86_64-pc-windows-gnu/debug/seriesx_smoke.exe
SB_HEADLESS=1 ./target/debug/scarletbridge run target/x86_64-pc-windows-gnu/debug/mt_io_stress.exe
SB_HEADLESS=1 ./target/debug/scarletbridge run target/x86_64-pc-windows-gnu/debug/d3d12_triangle.exe
SB_HEADLESS=1 ./target/debug/scarletbridge run target/x86_64-pc-windows-gnu/debug/d3d12_cube.exe
SB_HEADLESS=1 ./target/debug/scarletbridge run target/x86_64-pc-windows-gnu/debug/aaa_pipeline_bench.exe
```

---

## Submitting a Pull Request

1. Fork the repository and create a focused feature branch (`git checkout -b feat/my-feature`).
2. Keep commits clean and descriptive (Conventional Commits style preferred, e.g. `feat(sb-gpu): add DXBC IF/ELSE structured control flow`).
3. Fill out the Pull Request template completely and explain how you tested your change.
4. Maintainers will review and help iterate if needed. Welcome aboard!
