#!/usr/bin/env bash
set -euo pipefail

# Seeds initial contributor labels & actionable "good first issue" / "help wanted"
# issues on your GitHub repository using the GitHub CLI (`gh`).
#
# Usage:
#   gh auth login
#   ./scripts/seed-good-first-issues.sh

if ! command -v gh >/dev/null 2>&1; then
    echo "error: GitHub CLI (gh) is required. Install it from https://cli.github.com/" >&2
    exit 1
fi

echo "Creating contributor labels..."
gh label create "gpu" --color "d93f0b" --description "Vulkan renderer, memory, tiling" --force
gh label create "shaders" --color "fbca04" --description "DXBC / DXIL to SPIR-V compiler" --force
gh label create "d3d11" --color "1d76db" --description "Direct3D 11 HLE layer" --force
gh label create "d3d12" --color "0052cc" --description "Direct3D 12 / d3d12_xs HLE layer" --force
gh label create "audio" --color "5319e7" --description "XAudio2 / PipeWire / XMA2" --force
gh label create "input" --color "0e8a16" --description "SDL3 GameInput / WinRT input" --force
gh label create "api" --color "bfdadc" --description "Win32 / GDK / WinRT / MSVC API shims" --force

echo "Creating starter issues..."

gh issue create \
  --title "[Shaders] Add structured control flow (IF / ELSE / ENDIF / DISCARD) to DXBC -> SPIR-V translator" \
  --label "good first issue,help wanted,shaders" \
  --body "## Goal
Extend \`crates/sb-gpu/src/dxbc_to_spirv.rs\` to translate SM4/SM5 control flow opcodes:
- \`OP_IF\` / \`OP_ELSE\` / \`OP_ENDIF\` (\`OpSelectionMerge\` + \`OpBranchConditional\`)
- \`OP_DISCARD\` in Pixel Shaders (\`OpKill\` / \`OpDemoteToHelperInvocation\`)

## Acceptance Criteria
- Generated SPIR-V 1.3 passes \`spirv-val --target-env vulkan1.3\`
- Add unit tests in \`crates/sb-gpu/src/dxbc_to_spirv.rs\` covering nested branches and discard
- Zero Clippy warnings (\`cargo clippy --workspace --all-targets -- -D warnings\`)"

gh issue create \
  --title "[D3D11] Decode D3D11_INPUT_ELEMENT_DESC & CreateTexture2D(pInitialData) for full 3D UWP rendering" \
  --label "good first issue,help wanted,d3d11,gpu" \
  --body "## Goal
Upgrade \`crates/sb-hle/src/d3d11.rs\` to reach feature parity with \`d3d12_xs.rs\`:
- Parse \`D3D11_INPUT_ELEMENT_DESC\` in \`d3d11_create_input_layout\` into \`sb_gpu::GraphicsPipelineConfig\`
- Copy initial subresource data in \`d3d11_create_tex2d\` when \`pInitialData != null\`
- Bind blend & depth-stencil states (\`OMSetBlendState\`, \`OMSetDepthStencilState\`)

## Acceptance Criteria
- Unit test verifying custom compact vertex layout + textured draw through \`ID3D11Device\` / \`ID3D11DeviceContext\`
- Verified cleanly with \`cargo test --workspace\`"

gh issue create \
  --title "[GPU] Multiple Render Targets (MRT) & Offscreen Render-to-Texture (OMSetRenderTargets)" \
  --label "help wanted,gpu,d3d12" \
  --body "## Goal
Enable deferred rendering and post-processing passes in \`crates/sb-gpu/src/renderer.rs\` and \`crates/sb-hle/src/d3d12_xs.rs\`:
- Support up to 4 simultaneous color attachments (RT0..RT3) and HDR formats (\`R16G16B16A16_SFLOAT\`)
- Allow rendering into an offscreen \`ID3D12Resource\` texture and sampling it in a subsequent pass

## Acceptance Criteria
- Live Vulkan unit test rendering to an offscreen RT and sampling it on a fullscreen quad
- Passes with \`SB_VK_VALIDATION=1\`"

gh issue create \
  --title "[Shaders] DXIL SM 6.x -> SPIR-V 1.3 Function & dx.op.* Intrinsic Codegen" \
  --label "help wanted,shaders" \
  --body "## Goal
Build on the LLVM 3.7 bitstream parser in \`crates/sb-gpu/src/dxil.rs\` to translate basic Shader Model 6.0 Vertex, Pixel, and Compute shaders into valid SPIR-V 1.3.

## Starting Milestones
1. Decode \`TYPE_BLOCK\`, \`CONSTANTS_BLOCK\`, and \`METADATA_BLOCK\` (\`dx.entryPoints\`, \`dx.resources\`)
2. Decode \`FUNCTION_BLOCK\` SSA instructions and core \`dx.op.*\` calls (\`loadInput\`, \`storeOutput\`, \`cbufferLoadLegacy\`, \`threadId\`)
3. Emit SPIR-V 1.3 verified by \`spirv-val --target-env vulkan1.3\`"

gh issue create \
  --title "[Input] Wire SDL3 Keyboard & Mouse events into WinRT CoreWindow & GameInput" \
  --label "good first issue,help wanted,input" \
  --body "## Goal
Currently \`crates/sb-gpu/src/sdl3.rs\` and \`crates/sb-hle/src/winrt_objects.rs\` forward SDL3 Gamepad input. Add keyboard and mouse event dispatch so UWP/GDK test apps and open-source engines are playable without a controller.

## Acceptance Criteria
- Poll SDL3 key/mouse events in the window loop
- Map Win32 virtual-key codes cleanly
- Add unit tests for keycode mapping"

gh issue create \
  --title "[Audio] XAudio2 Multi-Voice Software Mixer & Volume/Pitch Controls over PipeWire/SDL3" \
  --label "good first issue,help wanted,audio" \
  --body "## Goal
Expand \`crates/sb-hle/src/xaudio2.rs\` to support:
- Real-time mixing of multiple concurrent \`IXAudio2SourceVoice\` streams
- \`SetVolume\`, \`SetFrequencyRatio\`, and \`XAUDIO2_BUFFER\` looping (\`LoopCount\`)

## Acceptance Criteria
- Unit tests verifying multi-voice mixing & volume scaling without clipping
- Tested cleanly under \`cargo test --workspace\`"

echo "Done! All starter contributor issues have been created."
