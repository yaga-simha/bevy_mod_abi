# Agent Guide: `bevy_mod_abi`

Crate guide for **`bevy_mod_abi`** (`v0.3.0`, ABI `0x0003_0000`, Bevy `0.20.1+`) — the **canonical** top-level Bevy host facade crate.

---

## 1. Crate Identity
- **Name:** `bevy_mod_abi` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.95`)
- **Role:** Canonical top-level Bevy host facade unifying `bevy_abi_mod_host`, `bevy_abi_mod_macros`, `bevy_abi_mod_sys`, and opt-in companion crates (`bridges`, `render`, `reflect`, `watcher`) behind 18 granular feature flags, 4 curated bridge presets, and `prelude`.
- **Legacy Note:** `bevy_abi_mod` is a minimal (`<80` LoC) `#[deprecated]` shim re-exporting this crate. Always keep canonical APIs and docs here.

---

## 2. Safety & Dependency Boundaries
- **Safety:** **`#![forbid(unsafe_code)]`** at crate root. Never introduce `unsafe` code here.
- **Default Headless Ergonomics:** Default features MUST stay lean — enabling NO `bevy_render`, `bevy_reflect`, `bevy_scene`, `bevy_asset`, or `notify` unless explicitly opted in via feature flags.

---

## 3. Owned Surface & 22 Feature Flags
- **Core Re-exports (`src/lib.rs`, `prelude`):** `AbiModPlugin`, `ModAppExt`, `ModWorldExt`, `ModCommandsExt`, `ModComponent`, `AbiPod`, `GuestComponent`, `ComponentOptions`, `ModBudgetPolicy`, `ModUnloadHooks`, and `bevy_abi_mod_sys` wire primitives (`sys`).
- **Core Features:** `strict-ffi-checks`, `diagnostics`, `signature-verification`, `hot-reload-watcher` (`bevy_abi_mod_watcher`).
- **Curated Bridge Presets (mirrored 1:1 from `bevy_abi_mod_bridges`):**
  - `preset-2d` (`ModPreset2dPlugin`): `bridge-std-types`, `bridge-asset`, `bridge-state`, `bridge-picking`, `bridge-audio`, `bridge-camera-2d`, `bridge-ui`
  - `preset-3d` (`ModPreset3dPlugin`): `preset-2d`, `bridge-light`, `bridge-camera-controller`, `bridge-bsn`
  - `preset-ui` (`ModPresetUiPlugin`): `bridge-std-types`, `bridge-asset`, `bridge-picking`, `bridge-ui`, `bridge-feathers`
  - `preset-full` (`ModPresetFullPlugin`): `preset-3d`, `preset-ui`, `bridge-solari`
- **Granular Companion Features:** `bridge-std-types`, `bridge-asset`, `bridge-state`, `bridge-bsn`, `bridge-picking`, `bridge-audio`, `bridge-camera-2d`, `bridge-light` (implies `bridge-camera-2d`), `bridge-ui`, `bridge-feathers` (implies `bridge-ui`), `bridge-camera-controller`, `bridge-solari`, `sub-app-extract` (`bevy_abi_mod_render` + `ModRenderRecoveryPlugin`), `bridge-reflect` (`bevy_abi_mod_reflect` + `EphemeralHandleBehavior`).

---

## 4. Strict Engineering Rules
1. **1:1 Preset & Export Parity:** Whenever a feature or public type is added or modified in `bevy_abi_mod_bridges`, `bevy_abi_mod_render`, `bevy_abi_mod_reflect`, or `bevy_abi_mod_watcher`, mirror its feature gate and re-export (`root`, module, and `prelude`) here.
2. **Facade Thinness:** Keep this crate purely a re-export and integration-test facade; place implementation logic in `bevy_abi_mod_host` or the respective companion crate.
3. **Zero Deprecation Leakage:** Tests and doctests here MUST import `bevy_mod_abi::*`, never the deprecated `bevy_abi_mod` shim.

---

## 5. Standalone Verification Commands

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo test --no-default-features
cargo check --no-default-features --features preset-2d
cargo check --no-default-features --features preset-3d
cargo check --no-default-features --features preset-ui
cargo check --no-default-features --features preset-full
cargo tree -e no-dev --no-default-features --features preset-2d | grep -E "bevy_pbr|bevy_light|bevy_solari|bevy_camera_controller"   # MUST print NOTHING
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
typos
```

---

## 6. Standalone Repository Split Checklist
1. **Replace Workspace Dependencies:** In `Cargo.toml`, replace `{ workspace = true }` on `bevy_abi_mod_{sys,host,macros,bridges,render,reflect,watcher}` (`"0.3.0"`) and Bevy `dev-dependencies` (`0.20.1`, `serde_json`).
2. **Preset Sync Check:** Ensure `Cargo.toml` feature lists for `preset-2d`, `preset-3d`, `preset-ui`, and `preset-full` match `bevy_abi_mod_bridges/Cargo.toml` 1:1 (including activating matching `bridge-*` flags for `#[cfg(feature = "...")]` exports).
3. **Root Tooling Configs:** Copy `.gitignore`, `.gitattributes`, `.typos.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, and `LICENSE-MIT` / `LICENSE-APACHE`.
