# `bevy_mod_abi` (`bevy_abi_mod`)

[![ABI Version](https://img.shields.io/badge/ABI-0.3.0-blue.svg)](./spec.md)
[![Bevy Version](https://img.shields.io/badge/Bevy-v0.20+-purple.svg)](https://bevyengine.org/)
[![Safety](https://img.shields.io/badge/safe%20crates-%23![forbid(unsafe_code)]-brightgreen.svg)](./docs/ARCHITECTURE.md)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**An asymmetric, zero-serialization C-ABI native modding framework for [Bevy ECS (`v0.20+`)](https://bevyengine.org/).**

**`bevy_mod_abi`** is the canonical top-level Bevy host facade crate (the legacy `bevy_abi_mod` facade crate is now a minimal `#[deprecated]` compatibility shim forwarding to `bevy_mod_abi`). It lets a Bevy game load, execute, budget, fault-contain, persist, and hot-reload native plugins (`.so`, `.dylib`, `.dll`) written with **different Rust compiler versions**, **mixed optimization levels (`debug` vs. `--release`)**, or **non-Rust languages (ISO C99, C++17, Zig)**—without shared Rust ABI assumptions or serialization overhead.

---

## Why `bevy_mod_abi`?

- **Zero Shared Rust ABI Assumptions:** Host and guest communicate strictly through a versioned, 68-slot `#[repr(C)]` `extern "C"` boundary (`ABI 0.3.0`, `0x0003_0000`) with independent heap allocators. Guest crates have **zero `bevy*` dependencies**.
- **Native ECS Speed:** Systems iterate contiguous Bevy table columns directly (`&[T]` / `&mut [T]` for zero-padding `AbiPod` structs), record deferred structural mutations into a reusable command arena (with RAII checkpoints/rollback), and run in parallel on Bevy's multi-threaded scheduler.
- **Hot-Reload, Budgets & Fault Isolation:** Built-in shadow copying avoids OS file locks and Apple Silicon code-sign invalidations; unchanged `SCHEMA_FINGERPRINT` columns reload with **zero bytes copied** while changed schemas migrate via `migrate_component_column`; panic guards catch unwinds on the guest side (`FfiResult::PANIC_CAUGHT`); observer and system faults route cleanly into `ModErrorPolicy` / Bevy's `FallbackErrorHandler`; and `ModBudgetPolicy` enforces per-mod spawn, arena, and frame-time watchdogs.
- **Modular & Safe Public Crates:** All 13 safe public crates (`bevy_mod_abi`, deprecated `bevy_abi_mod` shim, host, bridges, render, reflect, watcher, guest SDK, macros, trust, manifest, SDK generator, and `cargo-bam`) enforce `#![forbid(unsafe_code)]`, while `bevy_abi_mod_host` stays slim and headless with zero rendering, reflection, scene, asset, or watcher dependencies.

> **Native Trust Model:** Native shared libraries execute in-process with host OS privileges. `bevy_mod_abi` protects cooperative/trusted native mods against compiler ABI skew, allocator crossover, and accidental panics; it is **not** a sandbox against adversarial machine code (use WebAssembly for untrusted user scripts). Enabling `signature-verification` verifies Ed25519 `SSHSIG` signatures, CA certificates, `.bam_sig` / `.bampack` archives (`VerifiedBamPack::extract_to_dir`), and authority-scoped `SSHKRL` revocation lists **before** mapping any library into memory.

---

## Quickstart

### Option A: Add to a Bevy Host & Scaffold with `cargo-bam` (Recommended)

Add the canonical host facade crate **`bevy_mod_abi`** to your Bevy `0.20+` game (either bare headless, with a curated preset, or with granular features):

```bash
# Headless ECS core only (bevy_ecs + bevy_app)
cargo add bevy_mod_abi

# Or start quickly with a curated preset (preset-2d, preset-3d, preset-ui, or preset-full):
cargo add bevy_mod_abi --features preset-2d,hot-reload-watcher
cargo add bevy_mod_abi --features preset-3d,sub-app-extract,bridge-reflect
```

Install the standalone modder CLI (`crates/cargo-bam`):

```bash
cargo install --path crates/cargo-bam
```

Create, build, watch, package, and inspect a guest mod in Rust, C99, C++17, or Zig:

```bash
# Scaffold a guest project (also supports --lang c99, cpp17, or zig)
cargo bam new my_mod --lang rust

# Build the shared library (optionally sign with --sign-key <key>)
cargo bam build --dir my_mod --release

# Live watch-rebuild-stage loop feeding ModHotReloadPlugin
cargo bam watch --dir my_mod --out-dir ./mods

# Package a deterministic signed .bampack archive (library + mod.toml + assets/)
cargo bam pack --dir my_mod --release --key ./keys/mod_dev --out ./dist/my_mod.bampack

# Validate mod.toml manifests & SemVer DAG load order, export headers, and diagnose toolchains
cargo bam check-mods ./mods
cargo bam export-sdk --out ./include
cargo bam doctor
```

---

### Option B: Minimal Bevy Host & Zero-Boilerplate Rust Guest

#### 1. Host Application (`bevy_mod_abi`)

Add `bevy_mod_abi`, `bevy_app`, and `bevy_ecs` to your game's `Cargo.toml`:

```toml
[dependencies]
bevy_app = "0.20"
bevy_ecs = "0.20"
bevy_mod_abi = "0.3.0"
```

Register shared components and load guest libraries in Bevy:

```rust
use bevy_app::App;
use bevy_ecs::component::Component;
use bevy_mod_abi::prelude::*;

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, ModComponent)]
#[mod_component(name = "my_game::Position")]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

fn main() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));

    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register Position");

    let entity = app
        .world_mut()
        .spawn(Position { x: 1.0, y: 2.0, z: 3.0 })
        .id();

    let mod_id = app
        .load_mod_with(
            "target/release/libmy_guest_mod.dylib",
            LoadOptions::new().with_shadow_copy(true).strict(),
        )
        .expect("load guest mod");

    app.update();
    println!("Updated position: {:?}", app.world().get::<Position>(entity));

    app.unload_mod(mod_id).expect("unload guest mod");
}
```

#### 2. Standalone Rust Guest Mod (`bevy_abi_mod_guest`)

In a standalone crate (`[workspace]` with its own `Cargo.lock` and **no `bevy` dependencies**):

```toml
[package]
name = "my_guest_mod"
version = "0.3.0"
edition = "2024"

[workspace]

[lib]
crate-type = ["cdylib"]

[dependencies]
bevy_abi_mod_guest = { version = "0.3.0", features = ["derive"] }
```

```rust
use bevy_abi_mod_guest::prelude::*;
use core::ffi::CStr;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, AbiPod, GuestComponent)]
#[guest_component(name = "my_game::Position")]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Default)]
pub struct MyGuestMod;

impl GuestPlugin for MyGuestMod {
    const NAME: &'static CStr = c"my_guest_mod";
    const VERSION: &'static CStr = c"0.3.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        app.system(c"nudge_positions")
            .query_typed::<&mut Position>()
            .schedule(ScheduleId::UPDATE)
            .build(|ctx| {
                ctx.single_query::<&mut Position>().for_each(|positions| {
                    for p in positions {
                        p.x += 1.0;
                    }
                })
            })
    }
}

export_mod!(plugin: MyGuestMod);
```

> **Tip:** `#[derive(AbiPod, GuestComponent)]` with `#[guest_component(name = "...")]` eliminates all manual `impl GuestComponent` boilerplate (supporting `summary_tick`, `sparse_set`, and `immutable`), and `#[mod_component(name = "...", guest)]` offers a single-attribute shorthand when sharing a `ModComponent` definition. Both `SystemBuilder` and `ObserverBuilder` support composable `.run_if(...)` / `.run_if_fn(...)` predicates.

See the [5-Minute Quickstart](./docs/book/src/quickstart.md) and [`examples/01_minimal_handshake`](./examples/01_minimal_handshake) for a complete runnable project.

---

## Curated Bridge Presets & Optional Host Features

The headless core (`bevy_abi_mod_host`) pulls in only `bevy_ecs` + `bevy_app` (plus `ModBudgetPolicy` quotas/watchdogs and Bevy `0.20.1` `despawn_all_where` cleanup including `Disabled` entities). Enable optional capabilities via Cargo feature flags on **`bevy_mod_abi`** (also forwarded by the deprecated `bevy_abi_mod` compatibility shim):

### Curated Bridge Presets (`bevy_abi_mod_bridges`)

| Preset Feature | Bundle Plugin | Included Bridge Features |
|---|---|---|
| `preset-2d` | `ModPreset2dPlugin` | `bridge-std-types`, `bridge-asset`, `bridge-state`, `bridge-picking`, `bridge-audio`, `bridge-camera-2d`, `bridge-ui` |
| `preset-3d` | `ModPreset3dPlugin` | `preset-2d`, `bridge-light`, `bridge-camera-controller`, `bridge-bsn` |
| `preset-ui` | `ModPresetUiPlugin` | `bridge-std-types`, `bridge-asset`, `bridge-picking`, `bridge-ui`, `bridge-feathers` |
| `preset-full` | `ModPresetFullPlugin` | `preset-3d`, `preset-ui`, `bridge-solari` |

### Granular Host Features (18 Flags)

| Feature | Companion Crate | Purpose |
|---|---|---|
| `strict-ffi-checks` | `bevy_abi_mod_host` | Debug/audit poisoning of scoped FFI context headers on return |
| `diagnostics` | `bevy_abi_mod_host` | Publishes per-mod and aggregate `bevy_diagnostic` metrics (including budget telemetry) |
| `signature-verification` | `bevy_abi_mod_trust` | Enforces pre-load OpenSSH Ed25519 `SSHSIG`, CA cert, authority-scoped `SSHKRL`, `.bam_sig`, and hardened `.bampack` (`VerifiedBamPack::extract_to_dir`) checks via `ModTrustPolicy` |
| `hot-reload-watcher` | `bevy_abi_mod_watcher` | Debounced filesystem hot-reload watcher (`ModHotReloadPlugin`) with cascading DAG reloads |
| `bridge-std-types` | `bevy_abi_mod_bridges` | `Transform`, `GlobalTransform`, `Visibility`, `Name`, `Time<*>`, `ButtonInput<*>`, `Disabled`, `FfiMouseMotion`/`FfiMouseScroll` |
| `bridge-asset` | `bevy_abi_mod_bridges` | Sandboxed `mod://<namespace>/...` `AssetSource`, refcounted `FfiAssetHandle`, and opt-in `AssetOverlayPolicy` (`mod://overlay/...`) |
| `bridge-state` | `bevy_abi_mod_bridges` | Unbounded dynamic `ModStateSlab` (`States`, `SubStates`, read-only `ComputedStates`), transition schedules, `in_state` sets |
| `bridge-bsn` | `bevy_abi_mod_bridges` | Bevy `0.20` `bsn!` scenes, `ScenePatch`, `cmd_spawn_scene`, and `Ready` observers (`ModBsnPlugin`) |
| `bridge-picking` | `bevy_abi_mod_bridges` | All 7 Bevy `0.20` flattened `Pointer*` observers with bubbling, `FfiEntityCursor`, and `Gamepad` + rumble bridges |
| `bridge-audio` | `bevy_abi_mod_bridges` | `AudioPlayer<AudioSource>`, `PlaybackSettings`, and `SpatialAudioSink` / `AudioSink` controls (`ModAudioBridgePlugin`) |
| `bridge-camera-2d` | `bevy_abi_mod_bridges` | Lightweight `Camera2d` bridge (`FfiCamera2d` in `ModCameraLightBridgePlugin`) without pulling `bevy_pbr` or `bevy_light` |
| `bridge-light` | `bevy_abi_mod_bridges` | `Camera2d`, `Camera3d` (`Tonemapping`, `DebandDither`, `Exposure`, `Hdr`, `ScreenSpaceTransmission`), and point/spot/directional lights (implies `bridge-camera-2d`) |
| `bridge-camera-controller` | `bevy_abi_mod_bridges` | Bevy `0.20` `PanOrbitCamera` controller bridge (`FfiPanOrbitCamera`) |
| `bridge-solari` | `bevy_abi_mod_bridges` | Bevy `0.20` `SolariLighting` realtime global illumination bridge (`FfiSolariLighting`) |
| `bridge-ui` | `bevy_abi_mod_bridges` | Retained Bevy `0.20` UI (`Val::Em`/`Rem`, `FixedNode`, `EditableText`/`TextInput`, `InlineBox`/`InlineImage`, headless `TabList`/`ValueChange`) |
| `bridge-feathers` | `bevy_abi_mod_bridges` | Real `bevy_feathers` BSN-hydrated controls (`ColorInput`, `Select`/`Dropdown`, `ListView`, `NumberInput`, `ThemeContext`; implies `bridge-ui`) |
| `sub-app-extract` | `bevy_abi_mod_render` | Cross-world `SubApp` extraction, `.wesl` `ShaderBuffer` / 2D `SpriteMaterial` / `ExtendedMaterial2d` / 3D `StandardMaterial`, opt-in mesh-shader pipelines, and bounded `DeviceLost` recovery (`ModRenderRecoveryPlugin`) |
| `bridge-reflect` | `bevy_abi_mod_reflect` | Witness & declarative `BamFieldDesc` `ReflectFromPtr`, `bevy_remote` BRP, `bevy-settings` TOML, `ModSaveGamePlugin` (with Bevy `HandleSerializeProcessor`/`HandleDeserializeProcessor` round-trip), and `ModTelemetryPlugin` |

---

## Examples

Run the bundled examples after staging guest libraries with `cargo xtask fixtures`:

| Example | Concept | Run Command |
|---|---|---|
| [01 — Minimal Handshake](./examples/01_minimal_handshake) | ABI `0.3.0` handshake, zero-boilerplate `GuestComponent` + `query_typed`, shadow-copy load, lifecycle `Messages` | `cargo run -p example_01_minimal_handshake` |
| [02 — Multi-System Pipeline](./examples/02_multi_system_pipeline) | `query_typed` tuples, `With`/`Without` filters, `.after(...)`, `.run_if_fn(...)`, batched `spawn` | `cargo run -p example_02_multi_system_pipeline` |
| [03 — Cross-Language Interop](./examples/03_cross_language_interop) | Co-scheduled ISO C99 + Rust `.cdylib` mods over shared VTable components | `cargo run -p example_03_cross_language_interop` |
| [04 — 2D Visual Kinetic Arena](./examples/04_visual_game) | Terminal UI with live hot-reloading and unloading (`--test-mode` supported) | `cargo run -p example_04_visual_game -- --test-mode --ticks 30` |
| [05 — Cute 3D Slime Meadow](./examples/05_cute_3d_winit) | `winit` + `wgpu` + `avian3d` 3D loop hot-swapping Rust & C99 mods | `cargo run -p example_05_cute_3d_winit -- --test-mode --ticks 45` |
| [06 — Cross-Mod Components](./examples/06_cross_mod_components) | Guest-defined dynamic components shared and ordered across independent mods | `cargo run -p example_06_cross_mod_components` |
| [07 — ISO C++17 Guest](./examples/07_cpp17_guest) | Strict `-std=c++17` RAII views, `ModComponent::C_DECL`, observers, fault containment | `cargo run -p example_07_cpp17_guest` |
| [08 — Zig Guest](./examples/08_zig_guest) | Zig `comptime` wrappers (`zig_mod`), `ModComponent::ZIG_DECL`, requires `zig` (`0.14`–`0.16`) | `cargo run -p example_08_zig_guest` |
| [09 — `bevy_bql` Wire Parity](./examples/09_bql_wire_parity) | Dual ISO C99 vs. Safe Rust 500-frame differential parity + live frame-250 v1→v2 column migration | `cargo test --test bql_wire_contract_test` |
| [10 — Bevy 0.20 Full Companion](./examples/10_bevy20_full_companion) | End-to-end showcase: `PanOrbitCamera`, `.wesl` `SpriteMaterial`, picking, audio, UI, budgets, telemetry, save/load | `cargo test -p example_10_bevy20_full_companion` |

---

## Documentation & Technical Reports

- **[Developer Guide (`docs/book`)](./docs/book/src/SUMMARY.md)** — Walkthroughs for Rust, C99, C++17, and Zig guests, host scheduling, hot-reload, companion bridges, `cargo-bam`, signature verification, and the `bevy_bql` lowering contract.
- **[Wire Specification (`spec.md`)](./spec.md)** — Normative ABI `0.3.0` (`0x0003_0000`) contract, complete 68/68 live `BamHostApi` table, `bevy_bql` lowering contract, 16-crate boundaries, and invariants.
- **[Architecture & Contributor Workflow (`docs/ARCHITECTURE.md`)](./docs/ARCHITECTURE.md)** — Sixteen-crate workspace topology, `DROP_PAGES` reclamation state machine, relation/cascade/schedule internals, cryptographic trust architecture, and local CI / Miri / Loom verification commands.
- **[Benchmarks & Regression Gates (`docs/BENCHMARKS.md`)](./docs/BENCHMARKS.md)** — Canonical throughput benchmarks, reference Apple M4 Pro baselines, ABI `0.3.0` migration/hot-rewire/soak gates, deterministic `iai-callgrind` / zero-allocation gates, and the 34-archetype permutation suite.
- **[Changelog (`CHANGELOG.md`)](./CHANGELOG.md)** — Release history (`v0.3.0`, `v0.2.0`, `v0.1.0`).

---

## License

Dual-licensed under `MIT OR Apache-2.0`.
