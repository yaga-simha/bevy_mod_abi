# `bevy_abi_mod_render`

Safe (`#![forbid(unsafe_code)]`) multi-world `SubApp` extraction, data-driven `.wesl` shaders, `AlignedVec`-backed `ShaderBuffer` storage assets, 2D/3D material modding, and opt-in mesh-shader pipelines for [`bevy_abi_mod`](../bevy_abi_mod) (`ABI 0.3.0`, `v0.3.0`).

## Overview

- **`ModSubAppPlugin<L>` / `ModSubAppExt` / `ModSubWorldExt`:** Extracts host-registered and guest-dynamic `BamComponent` components and singleton resources from `MainWorld` into arbitrary Bevy `SubApp`s (`RenderApp`, custom `AppLabel`s) via `bevy_extract`, fully aligned with Bevy `0.20`'s system-driven `RenderGraph` / `Core2d` / `Core3d` schedules.
- **`ModWeslMaterialPlugin` / `ModMaterialAppExt` / `ModMaterialWorldExt`:** Registers `ModId`-scoped `.wesl`/`.wgsl` `Shader` assets, aligned `ShaderBuffer`s, `SpriteMeshMaterial`, `ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>`, and 3D `StandardMaterial` assets with automatic unload cleanup hooked into `bevy_abi_mod_host::ModUnloadHooks`.
- **`ModSpriteMaterialPlugin` / `ModSpriteMeshAppExt` / `ModSpriteMeshWorldExt`:** Runtime `.wesl` 2D sprite material specialization (`ModWeslSpriteExtension2d`, `ModExtendedSpriteMaterial2d`, `ModSpriteMaterial`) and unified Bevy `0.20` `Mesh2d + SpriteMaterial` spawning, attachment, live shader/param hot-swap, `RenderStartup` cache invalidation, and unload cleanup.
- **`ModMeshShaderPlugin` / `ModMeshShaderSubAppPlugin<L>`:** Explicit opt-in mesh-shader pipeline bridge (`ModMeshShaderConfig`, per-mod pipeline quotas, `WgpuFeatures::EXPERIMENTAL_MESH_SHADER`, `ExtractedModMeshPipelines` extraction into `RenderApp` / custom `SubApp`s) with `RenderStartup` stale-pipeline clearing and deterministic eviction on `unload_mod`.
- **`ModRenderRecoveryPlugin` / `ModRenderRecoveryPolicy` / `ModRenderRecoveryTelemetry`:** Bounded Bevy `0.20` `RenderErrorHandler` + `RenderErrorPolicy::Recover` recovery for `ErrorType::DeviceLost` (with retry limits, sliding recovery windows, circuit-breaker halt on non-`DeviceLost` errors or exhausted budgets, `ShaderBuffer` `MAIN_WORLD` CPU-copy preservation, and extensible `ModRenderRecoveryHooks`).
