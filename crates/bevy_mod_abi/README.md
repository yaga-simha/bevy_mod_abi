# `bevy_mod_abi`

Top-level `crates.io` umbrella crate for the zero-serialization, asymmetric C-ABI (`ABI 0.3.0`) native modding framework for Bevy ECS (`v0.20+`).

`bevy_mod_abi` matches the GitHub repository name (`https://github.com/yaga-simha/bevy_mod_abi`) and re-exports [`bevy_abi_mod`](https://crates.io/crates/bevy_abi_mod) (`bevy_mod_abi::*` and `bevy_mod_abi::prelude::*`) along with every optional feature flag while enforcing `#![forbid(unsafe_code)]`.

## Installation

```bash
cargo add bevy_mod_abi
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

## Feature Flags

All 17 feature flags from `bevy_abi_mod` are forwarded 1:1:

| Feature | Purpose |
|---|---|
| `strict-ffi-checks` | Extra runtime FFI pointer, alignment, and handle-generation validation |
| `diagnostics` | Per-mod and aggregate `bevy_diagnostic` metrics |
| `signature-verification` | Pure-Rust OpenSSH `SSHSIG`, Ed25519 CA, `SSHKRL`, `.bam_sig`, and `.bampack` pre-load verification |
| `hot-reload-watcher` | Debounced filesystem hot-reload watcher (`ModHotReloadPlugin`) |
| `bridge-std-types` | `Transform`, `GlobalTransform`, `Visibility`, `Name`, `Disabled`, `Time<*>`, and `ButtonInput<*>` bridges |
| `bridge-asset` | Sandboxed `mod://<namespace>/...` `AssetSource` & `AssetOverlayPolicy` (`ModAssetPlugin`) |
| `bridge-state` | Dynamic Bevy `States`, `SubStates`, and `ComputedStates` slabs (`ModStateExt`) |
| `bridge-reflect` | Dynamic `bevy_reflect`, `bevy_remote` BRP, `bevy_settings`, `ModSaveGamePlugin`, and `ModTelemetryPlugin` |
| `bridge-bsn` | Bevy Scene Notation (`bevy_scene`) spawning (`ModBsnPlugin`, implies `bridge-asset`) |
| `bridge-picking` | Bevy `0.20` flattened `Pointer*` observers, `EntityCursor`, and `Gamepad` + rumble bridges |
| `bridge-audio` | Spatial `AudioPlayer<AudioSource>` and `PlaybackSettings` bridges (`ModAudioBridgePlugin`, implies `bridge-asset`) |
| `bridge-light` | `Camera2d`/`Camera3d`, tonemapping, and `PointLight`/`SpotLight`/`DirectionalLight` bridges |
| `bridge-ui` | Retained Bevy `0.20` UI, `EditableText`/`TextInput`, `InlineBox`/`InlineImage`, and headless widgets |
| `bridge-feathers` | `bevy_feathers` controls and `ThemeContext` bridges (implies `bridge-ui`) |
| `bridge-camera-controller` | `PanOrbitCamera` controller bridge |
| `bridge-solari` | `SolariLighting` realtime GI bridge |
| `sub-app-extract` | `SubApp` extraction, `.wesl` 2D/3D materials, and mesh-shader pipelines (implies `bridge-asset`) |

## Guest Mods & Developer CLI

- **Rust Guest SDK (zero `bevy` dependencies):** [`bevy_abi_mod_guest`](https://crates.io/crates/bevy_abi_mod_guest)
- **Developer CLI (`new`, `build`, `pack`, `watch`, `export-sdk`, `check-mods`, `doctor`, `keygen`, `sign`, `verify`):** [`cargo-bam`](https://crates.io/crates/cargo-bam)

## License

Dual-licensed under `MIT OR Apache-2.0`.
