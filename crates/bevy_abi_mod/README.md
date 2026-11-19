# `bevy_abi_mod`

Top-level Bevy ECS (`v0.20+`) host facade for the zero-serialization, asymmetric C-ABI (`ABI 0.3.0`, `v0.3.0`) native modding framework (also published under the crates.io umbrella alias [`bevy_mod_abi`](../bevy_mod_abi)).

This crate re-exports the slim headless [`bevy_abi_mod_host`](../bevy_abi_mod_host), procedural derive macros (`#[derive(ModComponent, AbiPod)]`), [`bevy_abi_mod_sys`](../bevy_abi_mod_sys), optional companion crates ([`bevy_abi_mod_bridges`](../bevy_abi_mod_bridges), [`bevy_abi_mod_render`](../bevy_abi_mod_render), [`bevy_abi_mod_reflect`](../bevy_abi_mod_reflect), [`bevy_abi_mod_watcher`](../bevy_abi_mod_watcher)), and unified `prelude`, while enforcing `#![forbid(unsafe_code)]` across all public host APIs.

## Quickstart

```rust,no_run
use bevy_abi_mod::prelude::*;
use bevy_app::App;
use bevy_ecs::component::Component;

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

## Features (17 Opt-In Host Flags)

- **Core (`bevy_abi_mod_host`):**
  - **`strict-ffi-checks`**: Extra runtime FFI pointer, alignment, and handle-generation validation.
  - **`diagnostics`**: Publishes per-mod and aggregate `bevy_diagnostic` metrics (`cpu_time_us`, `p99_time_us`, `fault_count`, `commands_queued`, `budget_breach_count`, `spawns_queued`, `arena_bytes_peak`).
  - **`signature-verification`**: Pure-Rust OpenSSH `SSHSIG` (`.sig` sidecars, embedded `.bam_sig`, and `.bampack` archives), Ed25519 CA certificate validation, and `PROTOCOL.krl` (`SSHKRL`) revocation lists via [`bevy_abi_mod_trust`](../bevy_abi_mod_trust) (`ModTrustPolicy`, `VerifiedModSignature`, `evict_revoked_mods`) prior to `dlopen` / `LoadLibraryW`.
- **Watcher (`bevy_abi_mod_watcher`):**
  - **`hot-reload-watcher`**: Debounced filesystem hot-reload watcher (`ModHotReloadPlugin`) with cascading SemVer DAG reloads.
- **Subsystem Bridges (`bevy_abi_mod_bridges`):**
  - **`bridge-std-types`**: Bridges Bevy `Transform`, `GlobalTransform`, `Visibility`, `Name`, `Time<*>`, `ButtonInput<KeyCode|MouseButton>`, `Disabled`, and `FfiMouseMotion`/`FfiMouseScroll`.
  - **`bridge-asset`**: Sandboxed `mod://<namespace>/...` Bevy `AssetSource` (`ModAssetPlugin`), refcounted `FfiAssetHandle`, and opt-in `AssetOverlayPolicy` (`mod://overlay/<rel_path>`).
  - **`bridge-state`**: Generation-indexed dynamic `ModStateSlab` (`States`, `SubStates`, read-only `ComputedStates`), transition schedules, and `in_state` sets (`ModStateExt`).
  - **`bridge-bsn`**: Bevy Scene Notation (`bevy_scene`) spawning (`cmd_spawn_scene`) and `Ready` observer integration (`ModBsnPlugin`).
  - **`bridge-picking`**: All 7 Bevy `0.20` flattened `Pointer*` observers with bubbling, bidirectional `FfiEntityCursor`, and `Gamepad` + rumble bridges (`ModPickingBridgePlugin`, `ModInputExtBridgePlugin`).
  - **`bridge-audio`**: `AudioPlayer<AudioSource>`, `PlaybackSettings`, and `SpatialAudioSink`/`AudioSink` bridges (`ModAudioBridgePlugin`).
  - **`bridge-light`**: `Camera2d`, `Camera3d` (`Tonemapping`, `DebandDither`, `Exposure`, `Hdr`, `ScreenSpaceTransmission`), and `PointLight`/`SpotLight`/`DirectionalLight` bridges (`ModCameraLightBridgePlugin`).
  - **`bridge-camera-controller`**: Bevy `0.20` `PanOrbitCamera` controller bridge (`FfiPanOrbitCamera`).
  - **`bridge-solari`**: Bevy `0.20` `SolariLighting` realtime GI bridge (`FfiSolariLighting`).
  - **`bridge-ui`**: Retained Bevy `0.20` UI (`Val::Em`/`Rem`, `FixedNode`, elliptical `BorderRadius`, split `EditableText`/`TextInput`, `InlineBox`/`InlineImage`, headless `TabList`/`ValueChange` via `ModUiBridgePlugin`).
  - **`bridge-feathers`**: `bevy_feathers` controls and `ThemeContext` bridges (implies `bridge-ui`).
- **Render (`bevy_abi_mod_render`):**
  - **`sub-app-extract`**: Cross-world `SubApp` extraction (`ModSubAppPlugin<L>`), `.wesl` shaders & `AlignedVec` `ShaderBuffer` (`ModWeslMaterialPlugin`), 2D `.wesl` `SpriteMaterial`/`ExtendedMaterial2d` & unified `Mesh2d` (`ModSpriteMaterialPlugin`), and opt-in mesh-shader pipelines (`ModMeshShaderPlugin`).
- **Reflection, Save Games & Telemetry (`bevy_abi_mod_reflect`):**
  - **`bridge-reflect`**: Dynamic witness + declarative `BamFieldDesc` `ReflectFromPtr` reflection (`ModReflectPlugin`), `bevy_remote` BRP & `bevy-settings` TOML sync, persistent save archives (`ModSaveGamePlugin`), and live `bam/*` BRP + 64-byte Unix/loopback digest streaming (`ModTelemetryPlugin`).

## Documentation

See the repository [`README.md`](../../README.md), [`spec.md`](../../spec.md), [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md), and [Developer Guide](../../docs/book/src/SUMMARY.md) for quickstarts, the 16-crate boundary map, the `bevy_bql` lowering contract, and polyglot C99/C++17/Zig guest guides.
