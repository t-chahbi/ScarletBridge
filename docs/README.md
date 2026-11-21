# ScarletBridge Documentation Index

All technical architecture notes, ABI references, and contributor guides live here and at the repository root.

| Document | Contents |
| :--- | :--- |
| [README.md](../README.md) | Project overview, live status table, GPU captures, quickstart, and architecture summary |
| [CONTRIBUTING.md](../CONTRIBUTING.md) | Developer setup (`mise` / native), ABI & safety rules, good first areas, and PR checklist |
| [ROADMAP.md](../ROADMAP.md) | Completed milestones (Phases 1–4) and open engineering tracks (Phases 5–6) |
| [LLM.md](../LLM.md) | Full technical architecture blueprint & phase design notes (French) |
| [AGENTS.md](../AGENTS.md) | Strict engineering & ABI invariants for human and AI contributors |
| [abi/xmem.md](abi/xmem.md) | `xmem.dll` Xbox unified memory ABI notes (`XMemAlloc`, attributes, 64 KiB alignment) |
| [SECURITY.md](../SECURITY.md) | VFS sandbox invariants, offline-by-default guarantees, and vulnerability reporting |
| [CHANGELOG.md](../CHANGELOG.md) | Release and feature history |

## GPU Captures (`docs/images/`)

- [aaa-pipeline-bench.png](images/aaa-pipeline-bench.png) — 256-thread GPU Compute Shader → 4 instanced BC1-textured 3D cubes + Alpha-Blended HUD & Scissor clipping
- [d3d12-cube.png](images/d3d12-cube.png) — 3D perspective rotating textured cube (`cb0` MVP, `u16` IB, `D32_SFLOAT` depth buffer)
- [d3d12-triangle.png](images/d3d12-triangle.png) — End-to-end `d3d12_xs.dll` → DXBC → SPIR-V → Vulkan 1.4 RGB triangle
