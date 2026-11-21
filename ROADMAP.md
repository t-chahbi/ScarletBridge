# ScarletBridge Roadmap

This document tracks completed milestones and open engineering areas where contributors can make the biggest impact.

---

## Completed Milestones

### Phase 1 — Native PE32+ Loader, VFS & Win64 Tooling
- [x] Standalone PE32+ x86-64 loader (`sb-loader`): `mmap`, relocations, `TEB`/`PEB` via `GS`, per-thread `.tls`, dynamic IAT resolution, Win64 JIT trampolines
- [x] Sandboxed VFS (`sb-vfs`): `G:\`, `S:\`, `T:\`, `D:\`, traversal & symlink escape prevention, save quota enforcement, `AppxManifest.xml` & `MicrosoftGame.config` parsers
- [x] `sb-stubgen` PE import/export inspector & Rust `.def` / `cdylib` stub generator
- [x] Offline runtime (`sb-runtime`) & `scarletbridge` CLI (`sb-launcher`)

### Phase 2 — Win64 Multi-Threading, Sync, 64-bit File I/O & GDK GameOS
- [x] Real OS threads (`CreateThread`, `_beginthreadex`), per-thread `Tls*`/`Fls*` with exit destructors
- [x] `CRITICAL_SECTION`, `SRWLOCK`, generation-counted `CONDITION_VARIABLE`, 256-bucket `WaitOnAddress` futex, waitable timers, `WaitForMultipleObjects`
- [x] Sandboxed 64-bit `OVERLAPPED` `pread`/`pwrite`, `MapViewOfFile`, `FindFirstFile*`, `IOCP`
- [x] Multi-threaded `XTaskQueue`/`XAsync`, `XUser`, strict offline `XCurl` & `libHttpClient.GDK`, MSVC runtimes (`msvcp140*`, `vcruntime140*`, `concrt140`, `vcomp140`)

### Phase 3 — Unified Memory, Vulkan 1.4 3D Renderer & DXBC Compiler
- [x] `xmem.dll` + `VK_EXT_external_memory_host` 64 KiB aligned unified CPU/GPU memory
- [x] AMD RDNA 2 2D tile/detile (`xg_tiling`)
- [x] Pure-Rust DXBC SM4/SM5 → SPIR-V 1.3 translator (VS/PS, `cb0..cb3` dynamic relative indexing, `t0..t3`/`s0..s3`, validated against `spirv-val` on 60 real `fxc` shaders)
- [x] `d3d12_xs.dll` & `dxgi.dll` COM translation over Vulkan 1.4 (`RGBA8` + `D32_SFLOAT`, SDL3/Wayland swapchain, GPU readback)

### Phase 4 — AAA Pipeline Foundations (Compute, Aliasing, BC1–BC7, Caching)
- [x] Compute Shaders (`cs_5_0` → SPIR-V `GLCompute`) + Storage Buffers / UAVs (`u0..u3`) + `vkCmdDispatch`
- [x] `CreateHeap` + `CreatePlacedResource` real memory aliasing & `CheckFeatureSupport`
- [x] Hardware BC1–BC7 & HDR texture uploads (`VK_FORMAT_BC*_BLOCK`), `GraphicsPipelineConfig` (InputLayout, Alpha Blending, Culling, Wireframe, Scissor, `SV_InstanceID` Instancing)
- [x] Disk-backed persistent `VkPipelineCache` (`~/.cache/scarletbridge/pipeline_cache.bin`)
- [x] DXIL SM 6.x `DxilProgramHeader` + LLVM 3.7 Bitstream structural parser (`sb-gpu::dxil`)
- [x] End-to-end PE32+ `aaa_pipeline_bench.exe` harness verified on real AMD Vulkan 1.4 hardware

---

## Active & Upcoming Milestones (Contributors Welcome!)

### Phase 5 — Full Modern Shader & Graphics Parity
- [ ] **DXIL SM 6.x → SPIR-V 1.3 Codegen (`sb-gpu::dxil`)**: Decode LLVM 3.7 `FUNCTION` records, SSA values, and `dx.op.*` intrinsics into valid SPIR-V 1.3 (VS/PS/CS)
- [ ] **Structured Control Flow in DXBC Translator**: `IF`/`ELSE`/`ENDIF`, `LOOP`/`ENDLOOP`/`BREAKC`, `SWITCH`, `DISCARD`
- [ ] **Multiple Render Targets (MRT) & Render-to-Texture**: `OMSetRenderTargets` offscreen framebuffers (G-Buffer / Deferred Rendering / Shadow Maps / Post-FX passes)
- [ ] **Dynamic `VkSampler` Cache & Texture Arrays / Cubemaps / Mipmaps**
- [ ] **D3D11 Complete Pipeline Parity (`sb-hle::d3d11`)**: `CreateInputLayout` format decoding, `CreateTexture2D(pInitialData)`, blend/depth states for full 3D rendering in open-source UWP engines (e.g. OpenLara)
- [ ] **Indirect Drawing & ExecuteIndirect** (`DrawInstanced` / `DrawIndexedInstanced` / `Dispatch` indirect buffers)

### Phase 6 — Next-Gen Scarlett Features, Audio & Polish
- [ ] **Vulkan Hardware Ray Tracing**: Map `XboxBvhBuilder` / DXR `BuildRaytracingAccelerationStructure` & `DispatchRays` to `VK_KHR_acceleration_structure` + `VK_KHR_ray_tracing_pipeline`
- [ ] **Mesh & Amplification Shaders**: `VK_EXT_mesh_shader` support for DXIL SM 6.5+ mesh pipelines
- [ ] **XAudio2 Full Mixer & Pure-Rust XMA2 Decoder**: Voice pitch/volume/submix routing & XMA2 hardware audio decoding over PipeWire
- [ ] **Win64 SEH / C++ Exception Unwinding**: `.pdata` unwind tables & `RtlAddFunctionTable`
- [ ] **Keyboard/Mouse WinRT Input & Gamepad Rumble/Triggers**: Complete SDL3 input bridge
- [ ] **RenderDoc & Tracy Profiling Integration**: Debug markers (`PIXBeginEvent` → `VK_EXT_debug_utils` labels)
