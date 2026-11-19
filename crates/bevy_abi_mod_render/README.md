# `bevy_abi_mod_render`

Safe (`#![forbid(unsafe_code)]`) multi-world `SubApp` extraction, data-driven `.wesl` shaders, `AlignedVec`-backed `ShaderBuffer` storage assets, and Bevy `0.20` 2D/3D material modding for [`bevy_abi_mod`](../bevy_abi_mod).

## Overview

- **`ModSubAppPlugin<L>` / `ModSubAppExt` / `ModSubWorldExt`:** Extracts host-registered and guest-dynamic `BamComponent` components and singleton resources from `MainWorld` into arbitrary Bevy `SubApp`s (`RenderApp`, custom sub-apps) via `bevy_extract`.
- **`ModWeslMaterialPlugin` / `ModMaterialAppExt` / `ModMaterialWorldExt`:** Registers `ModId`-scoped `.wesl`/`.wgsl` `Shader` assets, aligned `ShaderBuffer`s, `SpriteMeshMaterial`, `ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>`, and `StandardMaterial` assets with automatic unload cleanup hooked into `bevy_abi_mod_host`.
