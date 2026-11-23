# Changelog

All notable changes to **ScarletBridge** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- **Broad Win32 / C runtime coverage (`sb-hle`)**: UCRT string/convert/time/math/stdio/filesystem with `errno`, MSVC STL iostream & locale layouts, `setjmp`/`longjmp`, RTTI, `.pdata`-based `RtlVirtualUnwind`, `bcrypt`, `rpcrt4`, `dbghelp`, ntuser window/message stubs, `GetFileAttributes*` reporting NTFS-style attributes.
- **Offline-first networking**: `ws2_32` (by name and ordinal), `winhttp` and a full `libHttpClient.GDK` surface, strictly offline by default.
- **Xbox GDK runtime classes** served through `QueryApiImpl` (threading/`XAsync`, `XUser`, `XNetworking`, `XGameSave`, `XStore`, `XPackage`, …), DirectStorage (public and console interfaces), `xmem` physical pages, `XboxBvhBuilder`, Media Foundation and Bink stubs.
- **Diagnostics**: `inspect --missing`, `SB_TRACE_API` call tracer with string previews, `SB_UNIMPLEMENTED=stop`, `SB_GUEST_DLLS`, and a crash report (guest/host symbolization, call stack from `.pdata`).

### Fixed
- VFS now resolves `GAME:\…`, `SAVE:\…` and `TEMP:\…` (backslashes, any case); escape attempts are still rejected.
- D3D12 / DXGI vtables are padded to 192 slots so calls to newer or console-specific methods fail cleanly instead of reading past the table.
- SDL3: the video subsystem and the Vulkan loader are initialised before querying instance extensions (previously a `SIGSEGV`).

- **Standalone Native PE32+ x86-64 Loader (`sb-loader`)**: `mmap` image mapping, base relocations, `TEB`/`PEB` on Linux `GS` via `arch_prctl(ARCH_SET_GS)`, per-thread `.tls` cloning + `DLL_THREAD_ATTACH`/`DETACH`, dynamic IAT resolution, and on-the-fly Win64 x86-64 JIT stub trampolines.
- **DXBC SM4/SM5 → SPIR-V 1.3 Compiler (`sb-gpu::dxbc_to_spirv`)**: Pure-Rust shader translator validated by `spirv-val --target-env vulkan1.3`, supporting Vertex, Pixel, and Compute (`GLCompute`) shaders, Constant Buffers (`cb0..cb3`, dynamic relative indexing), 2D Textures & Samplers (`t0..t3`, `s0..s3`), Storage Buffers / UAVs (`u0..u3`), and `SV_InstanceID`/`SV_VertexID`.
- **DXIL SM 6.x Bitstream Parser (`sb-gpu::dxil`)**: Native Rust parser for `DxilProgramHeader` and LLVM 3.7 Bitstream (`BC\xC0\xDE`).
- **Vulkan 1.4 3D & Compute Renderer (`sb-gpu::renderer`)**: RGBA8 + D32F targets, dynamic resize, disk-backed `VkPipelineCache`, Compute Pipelines + `vkCmdDispatch`, hardware BC1–BC7 & HDR textures, custom vertex input layouts, alpha blending, culling, wireframe, scissor rects, instanced drawing, GPU→CPU readback, and SDL3/Wayland swapchain presentation.
- **Unified Memory & AMD RDNA 2 Tiling (`sb-gpu`, `xmem.dll`)**: `VK_EXT_external_memory_host` 64 KiB aligned unified memory manager and RDNA 2 2D tile/detile engine.
- **Direct3D 12 Xbox Series X & D3D11 HLE (`sb-hle`)**:
  - `d3d12_xs.dll` & `dxgi.dll`: COM vtables with compile-time ABI checks, Graphics & Compute PSOs, `CreateHeap` + `CreatePlacedResource` memory aliasing, `CheckFeatureSupport`, `Dispatch`, `DrawIndexedInstanced`, `CopyTextureRegion` (BC/HDR), `CopyResource` GPU readback, `D3D12SerializeRootSignature`.
  - `d3d11.rs` & WinRT/UWP (`CoreApplication`, `Gamepad`, `ApplicationData`, `GlobalizationPreferences`).
- **Native Win64 Multi-Threading, Sync & 64-bit VFS File I/O (`sb-hle`)**: Real OS threads (`CreateThread`/`_beginthreadex`), per-thread `Tls*`/`Fls*` with destructors, `WaitForSingleObject`/`WaitForMultipleObjects`, `CRITICAL_SECTION`, `SRWLOCK`, generation-counted `CONDITION_VARIABLE`, 256-bucket `WaitOnAddress` futex, waitable timers, sandboxed 64-bit `OVERLAPPED` `pread`/`pwrite`, `MapViewOfFile`, `FindFirstFile*`, and I/O Completion Ports (`IOCP`).
- **Xbox GDK & MSVC Runtimes (`sb-hle`)**: Multi-threaded `XTaskQueue`/`XAsync`, `XUser`, `XPersistentLocalStorage`, strict offline `XCurl`/`libHttpClient.GDK`, SDL3 `GameInput`, `XAudio2_9` (PipeWire/SDL3), `msvcp140*`, `vcruntime140*`, `concrt140`, `vcomp140`, `amd_fidelityfx_*_xbox`, `XboxBvhBuilder`, `Bink2Scarlett`.
- **PE32+ Test Harnesses (`tests/harness/`)**: 6 cross-compiled PE32+ x86-64 test executables (`dll_probe`, `seriesx_smoke`, `mt_io_stress`, `d3d12_triangle`, `d3d12_cube`, `aaa_pipeline_bench`) + `sb-stubgen` CLI tool.
