# Agent Guide: `bevy_abi_mod_bridges`

Crate guide for **`bevy_abi_mod_bridges`** (`v0.3.0`, ABI `0x0003_0000`, Bevy `0.20.1+`) — opt-in Bevy subsystem bridges and curated feature presets.

---

## 1. Crate Identity
- **Name:** `bevy_abi_mod_bridges` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.95`)
- **Role:** Provides strictly feature-gated (`default = []`) Bevy `0.20.1` subsystem bridges (std types, isolated `mod://` assets, states, BSN scenes, picking/input, audio, 2D/3D cameras & lights, and UI/Feathers) plus curated 2D/3D/UI/Full preset bundles.

---

## 2. Safety & Dependency Boundaries
- **Safety:** **`#![forbid(unsafe_code)]`** at crate root. Never write `unsafe` blocks.
- **Strict Feature Gating (`default = []`):** Base build depends only on `bevy_abi_mod_{sys,host_sys,host,macros}`, `bevy_ecs`, `bevy_app`, `bevy_platform`. Heavy Bevy subsystem crates (`bevy_render`, `bevy_pbr`, `bevy_light`, `bevy_feathers`, `bevy_solari`, etc.) MUST remain strictly optional under their dedicated bridge features.

---

## 3. Owned Surface & Feature Presets
- **Curated Presets (`ModPreset2dPlugin`, `ModPreset3dPlugin`, `ModPresetUiPlugin`, `ModPresetFullPlugin`):**
  - `preset-2d`: `bridge-std-types`, `bridge-asset`, `bridge-state`, `bridge-picking`, `bridge-audio`, `bridge-camera-2d`, `bridge-ui`
  - `preset-3d`: `preset-2d`, `bridge-light`, `bridge-camera-controller`, `bridge-bsn`
  - `preset-ui`: `bridge-std-types`, `bridge-asset`, `bridge-picking`, `bridge-ui`, `bridge-feathers`
  - `preset-full`: `preset-3d`, `preset-ui`, `bridge-solari`
- **Granular Bridges:**
  - `bridge-std-types` (`std_types.rs`, `input_ext.rs`): `Transform`, `GlobalTransform`, `Visibility`, `Name`, `Time<Virtual|Fixed|Real>`, `ButtonInput<*>`, `Disabled`, `FfiMouseMotion`/`FfiMouseScroll`.
  - `bridge-asset` (`asset.rs`): Sandboxed `mod://<namespace>/...` `AssetSource`, refcounted `FfiAssetHandle`, multi-layer `AssetOverlayPolicy` (`ModAssetReader::read_directory` merges overlay layers cleanly).
  - `bridge-state` (`state.rs`): Generation-indexed `ModStateSlab`, `States`, `SubStates`, read-only `ComputedStates`, transition schedules.
  - `bridge-bsn` (`bsn.rs`): Bevy `0.20` `ScenePatch`, `cmd_spawn_scene`, `bevy_scene::Ready` observers (preserving `pending_scene` until dependencies validate).
  - `bridge-picking` (`picking.rs`, `input_ext.rs`): Flattened `Pointer*` observers, `FfiEntityCursor`, `Gamepad`/rumble.
  - `bridge-camera-2d` & `bridge-light` (`camera_light.rs`): Lightweight `FfiCamera2d` (`bridge-camera-2d` pulls only `bevy_camera`/`bevy_color` — zero `bevy_pbr`/`bevy_light`); `bridge-light` adds `Camera3d`, lights, and implies `bridge-camera-2d`.
  - `bridge-camera-controller` (`PanOrbitCamera`) & `bridge-solari` (`SolariLighting`).
  - `bridge-audio` (`audio.rs`): `AudioPlayer<AudioSource>`, `PlaybackSettings`, `SpatialAudioSink`/`AudioSink`.
  - `bridge-ui` & `bridge-feathers` (`ui.rs`): Retained Bevy `0.20` UI (`Val::Em`/`Rem`, `FixedNode`, `EditableText`/`TextInput`, `TabList`/`ValueChange`), real `bevy_feathers` BSN scene hydration (`FeathersColorInputProps`/`Select`/`ListView`/`NumberInput`), and `bypass_change_detection()` on `mirror_*_to_ffi` systems.

---

## 4. Strict Engineering Rules
1. **Real Bevy Bindings Only:** Every bridge feature MUST hydrate or mirror real Bevy `0.20+` components/resources/scenes (no no-op ZST stubs or silently dropped FFI fields); use `.bypass_change_detection()` on FFI mirror-back paths to avoid feedback loops.
2. **Zero Non-Test Panics:** Maintain zero `.unwrap()` / `.expect()` calls in `src/` — propagate errors via `ModStateError` or host diagnostics.
3. **2D / Headless Isolation:** Never leak `bevy_pbr`, `bevy_light`, `bevy_solari`, or `bevy_camera_controller` into `preset-2d` or `bridge-camera-2d`.

---

## 5. Standalone Verification Commands

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
cargo check --no-default-features --features bridge-camera-2d
cargo check --no-default-features --features preset-2d
cargo check --no-default-features --features preset-3d
cargo check --no-default-features --features preset-ui
cargo check --no-default-features --features preset-full
cargo tree -e no-dev --no-default-features --features preset-2d | grep -E "bevy_pbr|bevy_light|bevy_solari|bevy_camera_controller"   # MUST print NOTHING
cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
typos
```

---

## 6. Standalone Repository Split Checklist
1. **Replace Workspace Dependencies:** Replace `{ workspace = true }` on `bevy_abi_mod_{sys,host_sys,host,macros}` (`"0.3.0"`), all `bevy_*` (`0.20.1`) crates, and `dev-dependencies` (`bevy_reflect`, `rodio`) in `Cargo.toml`.
2. **Preset Coordination:** Keep `preset-2d`, `preset-3d`, `preset-ui`, and `preset-full` definitions in sync with `bevy_mod_abi/Cargo.toml`.
3. **Root Tooling Configs:** Copy `.gitignore`, `.gitattributes`, `.typos.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, and `LICENSE-MIT` / `LICENSE-APACHE`.
