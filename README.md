# `bevy_abi_mod` (`bevy_mod_abi`)

[![ABI Version](https://img.shields.io/badge/ABI-0.3.0-blue.svg)](./spec.md)
[![Bevy Version](https://img.shields.io/badge/Bevy-v0.20+-purple.svg)](https://bevyengine.org/)
[![Safety](https://img.shields.io/badge/safe%20crates-%23![forbid(unsafe_code)]-brightgreen.svg)](./docs/ARCHITECTURE.md)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**An asymmetric, zero-serialization C-ABI native modding framework for [Bevy ECS (`v0.20+`)](https://bevyengine.org/).**

`bevy_abi_mod` (also published under the crates.io umbrella alias **`bevy_mod_abi`**) lets a Bevy game load, execute, budget, fault-contain, persist, and hot-reload native plugins (`.so`, `.dylib`, `.dll`) written with **different Rust compiler versions**, **mixed optimization levels (`debug` vs. `--release`)**, or **non-Rust languages (ISO C99, C++17, Zig)**—without shared Rust ABI assumptions or serialization overhead.

---

## Why `bevy_abi_mod`?

- **Zero Shared Rust ABI Assumptions:** Host and guest communicate strictly through a versioned, 68-slot `#[repr(C)]` `extern "C"` boundary (`ABI 0.3.0`, `0x0003_0000`) with independent heap allocators. Guest crates have **zero `bevy*` dependencies**.
- **Native ECS Speed:** Systems iterate contiguous Bevy table columns directly (`&[T]` / `&mut [T]` for zero-padding `AbiPod` structs), record deferred structural mutations into a reusable command arena (with RAII checkpoints/rollback), and run in parallel on Bevy's multi-threaded scheduler.
- **Hot-Reload, Budgets & Fault Isolation:** Built-in shadow copying avoids OS file locks and Apple Silicon code-sign invalidations; unchanged `SCHEMA_FINGERPRINT` columns reload with **zero bytes copied** while changed schemas migrate via `migrate_component_column`; panic guards catch unwinds on the guest side (`FfiResult::PANIC_CAUGHT`); and `ModBudgetPolicy` enforces per-mod spawn, arena, and frame-time watchdogs.
- **Modular & Safe Public Crates:** All 13 safe public crates (host, bridges, render, reflect, watcher, facade, `bevy_mod_abi` umbrella, guest SDK, macros, trust, manifest, SDK generator, and `cargo-bam`) enforce `#![forbid(unsafe_code)]`, while `bevy_abi_mod_host` stays slim and headless with zero rendering, reflection, scene, asset, or watcher dependencies.

> **Native Trust Model:** Native shared libraries execute in-process with host OS privileges. `bevy_abi_mod` protects cooperative/trusted native mods against compiler ABI skew, allocator crossover, and accidental panics; it is **not** a sandbox against adversarial machine code (use WebAssembly for untrusted user scripts). Enabling `signature-verification` verifies Ed25519 `SSHSIG` signatures, CA certificates, `.bam_sig` / `.bampack` archives, and `SSHKRL` revocation lists **before** mapping any library into memory.

---

## Quickstart

### Option A: Add to a Bevy Host & Scaffold with `cargo-bam` (Recommended)

Add the umbrella crate matching this repository to your Bevy `0.20+` game:

```bash
cargo add bevy_mod_abi
# Or enable specific companion features (e.g. standard types, assets, picking, UI, render, reflect):
cargo add bevy_mod_abi --features bridge-std-types,bridge-asset,bridge-state,bridge-reflect,hot-reload-watcher
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

### Option B: Minimal Bevy Host & Rust Guest

#### 1. Host Application (`bevy_mod_abi` / `bevy_abi_mod`)

Add `bevy_mod_abi` (or `bevy_abi_mod`), `bevy_app`, and `bevy_ecs` to your game's `Cargo.toml`:

```toml
[dependencies]
bevy_app = "0.20"
bevy_ecs = "0.20"
bevy_mod_abi = "0.3.0"
```

Register shared components and load guest libraries in Bevy:

```rust
use bevy_mod_abi::prelude::*;
use bevy_app::App;
use bevy_ecs::component::Component;

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
#[derive(Clone, Copy, Debug, Default, ModComponent)]
#[mod_component(name = "my_game::Position")]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl GuestComponent for Position {
    const NAME: &'static CStr = c"my_game::Position";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[derive(Default)]
pub struct MyGuestMod;

impl GuestPlugin for MyGuestMod {
    const NAME: &'static CStr = c"my_guest_mod";
    const VERSION: &'static CStr = c"0.3.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        app.system(c"nudge_positions")
            .query(|q| q.write::<Position>())
            .schedule(ScheduleId::UPDATE)
            .build(|ctx| {
                while let Some(mut chunk) = ctx.iter().next_chunk_view() {
                    let Some(positions) = chunk.component_mut::<Position>(0) else {
                        return FfiResult::TYPE_MISMATCH;
                    };
                    for p in positions {
                        p.x += 1.0;
                    }
                }
                FfiResult::OK
            })
    }
}

export_mod!(plugin: MyGuestMod);
```

See the [5-Minute Quickstart](./docs/book/src/quickstart.md) and [`examples/01_minimal_handshake`](./examples/01_minimal_handshake) for a complete runnable project.

---

## Optional Host Features

The headless core (`bevy_abi_mod_host`) pulls in only `bevy_ecs` + `bevy_app` (plus `ModBudgetPolicy` quotas/watchdogs). Enable optional companion capabilities via Cargo feature flags on `bevy_mod_abi` or `bevy_abi_mod`:

| Feature | Companion Crate | Purpose |
|---|---|---|
| `strict-ffi-checks` | `bevy_abi_mod_host` | Debug/audit poisoning of scoped FFI context headers on return |
| `diagnostics` | `bevy_abi_mod_host` | Publishes per-mod and aggregate `bevy_diagnostic` metrics (including budget telemetry) |
| `signature-verification` | `bevy_abi_mod_trust` | Enforces pre-load OpenSSH Ed25519 `SSHSIG`, CA cert, `SSHKRL`, `.bam_sig`, and `.bampack` checks via `ModTrustPolicy` |
| `hot-reload-watcher` | `bevy_abi_mod_watcher` | Debounced filesystem hot-reload watcher (`ModHotReloadPlugin`) with cascading DAG reloads |
| `bridge-std-types` | `bevy_abi_mod_bridges` | `Transform`, `GlobalTransform`, `Visibility`, `Name`, `Time<*>`, `ButtonInput<*>`, `Disabled`, `FfiMouseMotion`/`FfiMouseScroll` |
| `bridge-asset` | `bevy_abi_mod_bridges` | Sandboxed `mod://<namespace>/...` `AssetSource`, refcounted `FfiAssetHandle`, and opt-in `AssetOverlayPolicy` (`mod://overlay/...`) |
| `bridge-state` | `bevy_abi_mod_bridges` | Unbounded dynamic `ModStateSlab` (`States`, `SubStates`, read-only `ComputedStates`), transition schedules, `in_state` sets |
| `bridge-bsn` | `bevy_abi_mod_bridges` | Bevy `0.20` `bsn!` scenes, `ScenePatch`, `cmd_spawn_scene`, and `Ready` observers (`ModBsnPlugin`) |
| `bridge-picking` | `bevy_abi_mod_bridges` | All 7 Bevy `0.20` flattened `Pointer*` observers with bubbling, `FfiEntityCursor`, and `Gamepad` + rumble bridges |
| `bridge-audio` | `bevy_abi_mod_bridges` | `AudioPlayer<AudioSource>`, `PlaybackSettings`, and `SpatialAudioSink` / `AudioSink` controls (`ModAudioBridgePlugin`) |
| `bridge-light` | `bevy_abi_mod_bridges` | `Camera2d`, `Camera3d` (`Tonemapping`, `DebandDither`, `Exposure`, `Hdr`, `ScreenSpaceTransmission`), and point/spot/directional lights |
| `bridge-camera-controller` | `bevy_abi_mod_bridges` | Bevy `0.20` `PanOrbitCamera` controller bridge (`FfiPanOrbitCamera`) |
| `bridge-solari` | `bevy_abi_mod_bridges` | Bevy `0.20` `SolariLighting` realtime global illumination bridge (`FfiSolariLighting`) |
| `bridge-ui` | `bevy_abi_mod_bridges` | Retained Bevy `0.20` UI (`Val::Em`/`Rem`, `FixedNode`, `EditableText`/`TextInput`, `InlineBox`/`InlineImage`, headless `TabList`/`ValueChange`) |
| `bridge-feathers` | `bevy_abi_mod_bridges` | `bevy_feathers` controls (`ColorInput`, `Select`/`Dropdown`, `ListView`, `NumberInput`, `ThemeContext`; implies `bridge-ui`) |
| `sub-app-extract` | `bevy_abi_mod_render` | Cross-world `SubApp` extraction, `.wesl` `ShaderBuffer` / 2D `SpriteMaterial` / `ExtendedMaterial2d` / 3D `StandardMaterial`, and opt-in mesh-shader pipelines |
| `bridge-reflect` | `bevy_abi_mod_reflect` | Witness & declarative `BamFieldDesc` `ReflectFromPtr`, `bevy_remote` BRP, `bevy-settings` TOML, `ModSaveGamePlugin`, and `ModTelemetryPlugin` |

---

## Examples

Run the bundled examples after staging guest libraries with `cargo xtask fixtures`:

| Example | Concept | Run Command |
|---|---|---|
| [01 — Minimal Handshake](./examples/01_minimal_handshake) | ABI `0.3.0` handshake, shadow-copy load, lifecycle `Messages`, typed mutation | `cargo run -p example_01_minimal_handshake` |
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
