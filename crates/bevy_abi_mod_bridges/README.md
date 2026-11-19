# `bevy_abi_mod_bridges`

Safe (`#![forbid(unsafe_code)]`) optional Bevy `0.20` subsystem bridges for [`bevy_abi_mod`](../bevy_abi_mod).

## Features

- `bridge-std-types`: Zero-copy / VTable bridges for `Transform`, `GlobalTransform`, `Visibility`, `Name`, `Time<Virtual|Fixed|Real>`, `ButtonInput<KeyCode>`, and `ButtonInput<MouseButton>` (`StdTypesBridgePlugin`).
- `bridge-asset`: Isolated `mod://<namespace>/...` Bevy `AssetSource` with traversal/symlink sandboxing and refcounted `FfiAssetHandle` lifecycle (`ModAssetPlugin`).
- `bridge-state`: Bevy `States` exposure (`ModStateExt`), transition schedules, `in_state` sets, and `DespawnOnExit`/`DespawnOnEnter` markers.
- `bridge-bsn`: Bevy Scene Notation (`bevy_scene`) spawning (`cmd_spawn_scene`) and `bevy_scene::Ready` observers (`ModBsnPlugin`).
- Additional Phase 3 bridge feature flags (`bridge-picking`, `bridge-audio`, `bridge-light`, `bridge-ui`, etc.) keep individual Bevy subsystems strictly opt-in.
