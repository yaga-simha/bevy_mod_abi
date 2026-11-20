# Agent Guide: `bevy_abi_mod_render`

Crate guide for **`bevy_abi_mod_render`** (`v0.3.0`, ABI `0x0003_0000`, Bevy `0.20.1+`) — multi-world `SubApp` extraction, WESL shaders, 2D/3D materials, mesh shaders, and bounded GPU device-loss recovery.

---

## 1. Crate Identity
- **Name:** `bevy_abi_mod_render` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.95`)
- **Role:** Bridges modded data into Bevy `0.20.1`'s system-driven `RenderGraph`, `Core2d`/`Core3d` schedules, `.wesl`/`.wgsl` shader assets, `ShaderBuffer` storage assets, opt-in mesh-shader pipelines, and `RenderErrorPolicy::Recover` GPU device-loss recovery.

---

## 2. Safety & Dependency Boundaries
- **Safety:** **`#![forbid(unsafe_code)]`** at crate root. Zero `unsafe` code.
- **Allowed Dependencies:** `bevy_abi_mod_{sys,host_sys,host}`, Bevy `0.20.1` render/material stack (`bevy_app`, `bevy_asset`, `bevy_color`, `bevy_ecs`, `bevy_extract`, `bevy_image`, `bevy_material`, `bevy_pbr`, `bevy_platform`, `bevy_reflect`, `bevy_render`, `bevy_shader`, `bevy_sprite`, `bevy_sprite_render`), `tracing`, plus `bevy_abi_mod_bridges` (`bridge-asset`) & `bevy_abi_mod_macros` in `[dev-dependencies]`.

---

## 3. Owned Surface
- **`SubApp` Extraction (`lib.rs`, `sub_app_materials.rs`):** `ModSubAppPlugin<L>`, `ModSubAppExt`, `ModSubWorldExt` (`bevy_extract`), `ModWeslMaterialPlugin`, `ModMaterialAppExt`, `ModMaterialWorldExt`, `ShaderBuffer` (`ModShaderBufferSpec::{with_device_loss_recovery, retains_main_world_for_recovery}`), `SpriteMeshMaterial`, `StandardMaterial`.
- **2D `.wesl` Sprites & Mesh Shaders (`sprite_mesh_shaders.rs`):** `ModSpriteMaterialPlugin`, `ModWeslSpriteExtension2d`, `ModExtendedSpriteMaterial2d`, `ModSpriteMaterial`, opt-in `ModMeshShaderPlugin` / `ModMeshShaderSubAppPlugin<L>` (`ExtractedModMeshPipelines` with `RenderStartup` stale-pipeline clearing via `reset_mod_mesh_pipelines_on_render_startup` & deterministic `unload_mod` eviction).
- **Bounded Device-Loss Recovery (`recovery.rs`):** `ModRenderRecoveryPlugin`, `ModRenderRecoveryPolicy` (default 3 recoveries / 60s sliding window), `ModRenderRecoveryTelemetry`, `ModRenderRecoveryOutcome`, `ModRenderRecoveryHooks`, `ModRenderRecoveryRenderState`, `mod_render_error_handler`, `handle_mod_render_error_at`, `ModRenderRecovery{App,World}Ext` — issuing `RenderErrorPolicy::Recover` strictly for `ErrorType::DeviceLost` within budget and invalidating `ViewKeyCache` / `SpecializedMaterial*` / `ExtractedModMeshPipelines` on `RenderStartup`.

---

## 4. Strict Engineering Rules
1. **Bevy `0.20+` Render Graph as Systems:** Use `Core2d` / `Core3d` / `Render` / `ExtractSchedule` / `RenderStartup` schedules — NEVER reintroduce legacy pre-0.19 `RenderGraph::Node` or `ViewNode` traits.
2. **Bounded GPU Recovery & Unload Cleanup:** Only `ErrorType::DeviceLost` within budget may trigger `RenderErrorPolicy::Recover` (halt cleanly via `StopRendering` on `OutOfMemory`, `Validation`, `Internal`, or budget exhaustion). All mod-owned shaders, buffers, and pipelines MUST register unload cleanup with `ModUnloadHooks`.
3. **Zero Non-Test Panics:** Keep `.unwrap()` / `.expect()` / `panic!` at zero in `src/`.

---

## 5. Standalone Verification Commands

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
typos
```

---

## 6. Standalone Repository Split Checklist
1. **Replace Workspace Dependencies:** Replace `{ workspace = true }` on `bevy_abi_mod_{sys,host_sys,host}`, all `bevy_*` (`0.20.1`) crates, `tracing`, and `[dev-dependencies]` (`bevy_abi_mod_bridges`, `bevy_abi_mod_macros`) in `Cargo.toml`.
2. **Preserve `ModRenderRecoveryPlugin` Export Sync:** When exposed through `bevy_mod_abi` (`sub-app-extract`), ensure any new recovery or material symbols are also forwarded in `bevy_mod_abi`.
3. **Root Tooling Configs:** Copy `.gitignore`, `.gitattributes`, `.typos.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, and `LICENSE-MIT` / `LICENSE-APACHE`.
