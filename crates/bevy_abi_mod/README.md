# `bevy_abi_mod`

Top-level Bevy ECS (`v0.20+`) host facade for the zero-serialization, asymmetric C-ABI (`ABI 0.3.0`) native modding framework.

This crate re-exports [`bevy_abi_mod_host`](../bevy_abi_mod_host), procedural derive macros (`#[derive(ModComponent, AbiPod)]`), [`bevy_abi_mod_sys`](../bevy_abi_mod_sys), and [`prelude`], while enforcing `#![forbid(unsafe_code)]` across all public host APIs.

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

## Features

- **`strict-ffi-checks`**: Extra runtime FFI pointer, alignment, and handle-generation validation in `bevy_abi_mod_host`.
- **`diagnostics`**: Publishes per-mod and aggregate `bevy_diagnostic` counters (`cpu_time_us`, `fault_count`, `commands_queued`).
- **`signature-verification`**: Enables pure-Rust OpenSSH `SSHSIG` (`.sig` sidecars, embedded `.bam_sig`, and `.bampack` archives), Ed25519 CA certificate validation, and `PROTOCOL.krl` (`SSHKRL`) revocation lists via [`bevy_abi_mod_trust`](../bevy_abi_mod_trust) (`ModTrustPolicy`, `VerifiedModSignature`, `evict_revoked_mods`) prior to `dlopen` / `LoadLibraryW`.
- **`hot-reload-watcher`**: Debounced filesystem hot-reload watcher (`ModHotReloadPlugin`) paired with witness-schema migration.
- **`bridge-std-types`**: Bridges Bevy `Transform`, `GlobalTransform`, `Visibility`, `Name`, `Time<*>`, and `ButtonInput<KeyCode|MouseButton>`.
- **`bridge-asset`**: Sandboxed `mod://<namespace>/...` Bevy `AssetSource` (`ModAssetPlugin`) and refcounted `FfiAssetHandle`.
- **`bridge-state`**: Exposes Bevy `States` (`ModStateExt::expose_mod_state`), transition schedules, and `in_state` run conditions.
- **`bridge-reflect`**: Dynamic `bevy_reflect` (`DynamicStruct`), `bevy_remote` BRP JSON-RPC methods (`bevy_abi_mod/*`), and `bevy_settings` TOML sync (`ModReflectPlugin`).
- **`bridge-bsn`**: Bevy Scene Notation (`bevy_scene`) spawning (`cmd_spawn_scene`) and `Ready` observer integration (`ModBsnPlugin`).
- **`sub-app-extract`**: Cross-world `SubApp` (`RenderApp`) extraction (`ModSubAppPlugin<L>`) and custom `.wesl` shader/material support (`ModWeslMaterialPlugin`).

## Documentation

See the repository [`README.md`](../../README.md), [`spec.md`](../../spec.md), and [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) for quickstarts, the 11-crate boundary map, and polyglot C99/C++17/Zig guest integration guides.
