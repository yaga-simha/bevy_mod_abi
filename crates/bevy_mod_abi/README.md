# `bevy_mod_abi`

Canonical top-level Bevy ECS (`v0.20+`) host facade crate for the zero-serialization, asymmetric C-ABI (`ABI 0.3.0`, `v0.3.0`) native modding framework.

`bevy_mod_abi` re-exports the slim headless [`bevy_abi_mod_host`](../bevy_abi_mod_host), procedural derive macros (`#[derive(ModComponent, AbiPod, GuestComponent)]`), [`bevy_abi_mod_sys`](../bevy_abi_mod_sys), optional companion crates ([`bevy_abi_mod_bridges`](../bevy_abi_mod_bridges), [`bevy_abi_mod_render`](../bevy_abi_mod_render), [`bevy_abi_mod_reflect`](../bevy_abi_mod_reflect), [`bevy_abi_mod_watcher`](../bevy_abi_mod_watcher)), curated bridge presets, and unified `prelude`, while enforcing `#![forbid(unsafe_code)]` across all public host APIs.

> **Note:** The legacy `bevy_abi_mod` facade crate is now a minimal `#[deprecated]` compatibility shim forwarding to `bevy_mod_abi`. New and migrating projects should depend directly on `bevy_mod_abi`.

## Installation

```bash
# Headless ECS core only (bevy_ecs + bevy_app)
cargo add bevy_mod_abi

# Or enable a curated bridge preset (preset-2d, preset-3d, preset-ui, or preset-full):
cargo add bevy_mod_abi --features preset-2d,hot-reload-watcher
cargo add bevy_mod_abi --features preset-3d,sub-app-extract,bridge-reflect
```

## Quickstart

```rust,no_run
use bevy_app::App;
use bevy_ecs::component::Component;
use bevy_mod_abi::prelude::*;

#[repr(C)]
#[derive(Component, ModComponent, Clone, Copy, Default)]
#[mod_component(name = "my_game::Health")]
struct Health {
    current: f32,
    max: f32,
}

fn main() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));
    app.register_mod_component::<Health>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register Health");
    app.load_mod_with("mods/libcombat_mod.dylib", LoadOptions::default())
        .expect("load mod");
    app.run();
}
```

## Curated Bridge Presets

| Preset Feature | Bundle Plugin | Included Bridge Features |
|---|---|---|
| `preset-2d` | `ModPreset2dPlugin` | `bridge-std-types`, `bridge-asset`, `bridge-state`, `bridge-picking`, `bridge-audio`, `bridge-camera-2d`, `bridge-ui` |
| `preset-3d` | `ModPreset3dPlugin` | `preset-2d`, `bridge-light`, `bridge-camera-controller`, `bridge-bsn` |
| `preset-ui` | `ModPresetUiPlugin` | `bridge-std-types`, `bridge-asset`, `bridge-picking`, `bridge-ui`, `bridge-feathers` |
| `preset-full` | `ModPresetFullPlugin` | `preset-3d`, `preset-ui`, `bridge-solari` |

## Granular Feature Flags (18 Opt-In Flags)

| Feature | Purpose |
|---|---|
| `strict-ffi-checks` | Extra runtime FFI pointer, alignment, and handle-generation validation |
| `diagnostics` | Per-mod and aggregate `bevy_diagnostic` metrics (including budget telemetry) |
| `signature-verification` | Pure-Rust OpenSSH `SSHSIG`, Ed25519 CA, authority-scoped `SSHKRL`, `.bam_sig`, and hardened `.bampack` (`VerifiedBamPack::extract_to_dir`) pre-load verification |
| `hot-reload-watcher` | Debounced filesystem hot-reload watcher (`ModHotReloadPlugin`) |
| `bridge-std-types` | `Transform`, `GlobalTransform`, `Visibility`, `Name`, `Disabled`, `Time<*>`, and `ButtonInput<*>` bridges |
| `bridge-asset` | Sandboxed `mod://<namespace>/...` `AssetSource` & `AssetOverlayPolicy` (`ModAssetPlugin`) |
| `bridge-state` | Dynamic Bevy `States`, `SubStates`, and `ComputedStates` slabs (`ModStateExt`) |
| `bridge-bsn` | Bevy Scene Notation (`bevy_scene`) spawning (`ModBsnPlugin`, implies `bridge-asset`) |
| `bridge-picking` | Bevy `0.20` flattened `Pointer*` observers, `EntityCursor`, and `Gamepad` + rumble bridges |
| `bridge-audio` | Spatial `AudioPlayer<AudioSource>` and `PlaybackSettings` bridges (`ModAudioBridgePlugin`, implies `bridge-asset`) |
| `bridge-camera-2d` | Lightweight `Camera2d` bridge (`FfiCamera2d`) without pulling `bevy_pbr` or `bevy_light` |
| `bridge-light` | `Camera2d`/`Camera3d`, tonemapping, and `PointLight`/`SpotLight`/`DirectionalLight` bridges (implies `bridge-camera-2d`) |
| `bridge-camera-controller` | `PanOrbitCamera` controller bridge (`FfiPanOrbitCamera`) |
| `bridge-solari` | `SolariLighting` realtime GI bridge (`FfiSolariLighting`) |
| `bridge-ui` | Retained Bevy `0.20` UI, `EditableText`/`TextInput`, `InlineBox`/`InlineImage`, and headless widgets |
| `bridge-feathers` | Real `bevy_feathers` BSN-hydrated controls and `ThemeContext` bridges (implies `bridge-ui`) |
| `sub-app-extract` | `SubApp` extraction, `.wesl` 2D/3D materials, mesh-shader pipelines, and bounded `DeviceLost` recovery (`ModRenderRecoveryPlugin`; implies `bridge-asset`) |
| `bridge-reflect` | Dynamic `bevy_reflect`, `bevy_remote` BRP, `bevy-settings`, `ModSaveGamePlugin` (`HandleSerializeProcessor`/`HandleDeserializeProcessor` round-trip), and `ModTelemetryPlugin` |

## Guest Mods, Developer CLI & Documentation

- **Rust Guest SDK (zero `bevy` dependencies):** [`bevy_abi_mod_guest`](../bevy_abi_mod_guest)
- **Developer CLI (`new`, `build`, `pack`, `watch`, `export-sdk`, `check-mods`, `doctor`, `keygen`, `sign`, `verify`):** [`cargo-bam`](../cargo-bam)
- **Repository Docs:** [`README.md`](../../README.md), [`spec.md`](../../spec.md), [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md), and [Developer Guide](../../docs/book/src/SUMMARY.md).

## License

Dual-licensed under `MIT OR Apache-2.0`.
