# `bevy_abi_mod_bridges`

Safe (`#![forbid(unsafe_code)]`) opt-in Bevy `0.20` subsystem bridges (`default = []`) and curated feature presets for [`bevy_mod_abi`](../bevy_mod_abi) (`ABI 0.3.0`, `v0.3.0`).

## Curated Presets

Enable a curated preset feature (here or via `bevy_mod_abi`) and add its matching bundle plugin:

| Preset Feature | Bundle Plugin | Included Bridge Features |
|---|---|---|
| `preset-2d` | `ModPreset2dPlugin` | `bridge-std-types`, `bridge-asset`, `bridge-state`, `bridge-picking`, `bridge-audio`, `bridge-camera-2d`, `bridge-ui` |
| `preset-3d` | `ModPreset3dPlugin` | `preset-2d`, `bridge-light`, `bridge-camera-controller`, `bridge-bsn` |
| `preset-ui` | `ModPresetUiPlugin` | `bridge-std-types`, `bridge-asset`, `bridge-picking`, `bridge-ui`, `bridge-feathers` |
| `preset-full` | `ModPresetFullPlugin` | `preset-3d`, `preset-ui`, `bridge-solari` |

## Granular Bridge Features

- **`bridge-std-types`:** Zero-copy / VTable bridges for `Transform`, `GlobalTransform`, `Visibility`, `Name`, `Time<Virtual|Fixed|Real>`, `ButtonInput<KeyCode>`, `ButtonInput<MouseButton>`, and `bevy_ecs::entity_disabling::Disabled` (`StdTypesBridgePlugin`), plus `FfiMouseMotion` / `FfiMouseScroll` (`ModInputExtBridgePlugin`).
- **`bridge-state`:** Unbounded generation-indexed dynamic `ModStateSlab` (`ModStateHandle`, `ModStateEntry`, `ModStateTransitionRequest`, `ModDespawnOnExit`/`ModDespawnOnEnter`), `ModStateExt::expose_mod_state` (`States`), `register_mod_sub_state` (`SubStates`), and read-only `register_mod_computed_state` (`ComputedStates`).
- **`bridge-asset`:** Isolated `mod://<namespace>/...` Bevy `AssetSource` with traversal/symlink sandboxing, refcounted `FfiAssetHandle` lifecycle (`ModAssetPlugin`), and opt-in `AssetOverlayPolicy` (`Disabled`, `LoadOrderPriority`, `TopologicalOverlay`, `ExplicitPriority`, `mod://overlay/<rel_path>`) with automatic reload on override/unload.
- **`bridge-bsn`:** Bevy Scene Notation (`bevy_scene`) spawning (`cmd_spawn_scene`, `ScenePatch`) and `bevy_scene::Ready` observers (`ModBsnPlugin`).
- **`bridge-picking`:** All 7 Bevy `0.20` flattened `Pointer*` observers (`FfiPointerPress`, `Release`, `Move`, `Click`, `Over`, `Out`, `Drag`) with `PointerTraversal` bubbling, bidirectional `FfiEntityCursor` sync (`ModPickingBridgePlugin`), and `FfiGamepadState` / `FfiGamepadRumble` (`ModInputExtBridgePlugin`).
- **`bridge-camera-2d`:** Lightweight `Camera2d` bridge (`FfiCamera2d` in `ModCameraLightBridgePlugin`) without pulling `bevy_pbr` or `bevy_light`.
- **`bridge-light`:** `Camera2d`, `Camera3d` (`Tonemapping`, `DebandDither`, `Exposure`, `Hdr`, `ScreenSpaceTransmission`), and `PointLight`/`SpotLight`/`DirectionalLight` mirrors (`ModCameraLightBridgePlugin`; implies `bridge-camera-2d`).
- **`bridge-camera-controller`:** Bevy `0.20` `PanOrbitCamera` controller bridge (`FfiPanOrbitCamera` in `ModCameraLightBridgePlugin`).
- **`bridge-solari`:** Bevy `0.20` `SolariLighting` realtime global illumination bridge (`FfiSolariLighting` in `ModCameraLightBridgePlugin`).
- **`bridge-audio`:** `FfiAudioPlaybackSettings`, `FfiAudioPlayerSpec` (`AudioPlayer<AudioSource>`), and `FfiSpatialAudioSinkControl` (`SpatialAudioSink`/`AudioSink`) bridges (`ModAudioBridgePlugin`).
- **`bridge-ui`:** Retained Bevy `0.20` UI (`FfiVal` incl. `Em`/`Rem`, `FfiUiNode` + `FixedNode`, elliptical `FfiBorderRadius`/`FfiCornerRadius`, `FfiFontSize::Rem`, split `EditableText`/`TextInput`, `FfiInlineBox`/`FfiInlineImage`, headless `TabList`/`Tab`/`SelectedTab`/`Activate`/`FfiValueChange`, and `ModUnloadHooks` UI teardown via `ModUiBridgePlugin`).
- **`bridge-feathers`:** Real `bevy_feathers` BSN scene hydration (`bsn!` / `ScenePatch` controls: `FfiThemeContext`, `FfiFeathersColorInput`, `FfiFeathersDropdown`/`FfiFeathersSelect`, `FfiFeathersListView`/`FfiFeathersListRow`, `FfiFeathersNumberInput`; implies `bridge-ui`).
