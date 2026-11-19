# `bevy_abi_mod`

[![ABI Version](https://img.shields.io/badge/ABI-0.3.0-blue.svg)](./spec.md)
[![Bevy Version](https://img.shields.io/badge/Bevy-v0.20+-purple.svg)](https://bevyengine.org/)
[![Safety](https://img.shields.io/badge/safe%20crates-%23![forbid(unsafe_code)]-brightgreen.svg)](./docs/ARCHITECTURE.md)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**An asymmetric, zero-serialization C-ABI native modding framework for [Bevy ECS (`v0.20+`)](https://bevyengine.org/).**

`bevy_abi_mod` lets a Bevy game load, execute, fault-contain, and hot-reload native plugins (`.so`, `.dylib`, `.dll`) written with **different Rust compiler versions**, **mixed optimization levels (`debug` vs. `--release`)**, or **non-Rust languages (ISO C99, C++17, Zig)**—without shared Rust ABI assumptions or serialization overhead.

---

## Why `bevy_abi_mod`?

- **Zero Shared Rust ABI Assumptions:** Host and guest communicate solely through a versioned `#[repr(C)]` `extern "C"` boundary (`ABI 0.3.0`, `0x0003_0000`) with independent heap allocators. Guest crates have **zero `bevy*` dependencies**.
- **Native ECS Speed:** Systems iterate contiguous Bevy table columns directly (`&[T]` / `&mut [T]` for zero-padding `AbiPod` structs), record deferred structural mutations into a reusable command arena, and run in parallel on Bevy's multi-threaded scheduler.
- **Hot-Reload & Fault Isolation:** Built-in shadow copying avoids OS file locks and Apple Silicon code-sign invalidations; panic guards catch unwinds on the guest side (`FfiResult::PANIC_CAUGHT`) so hosts can quarantine faulting systems without crashing the game.
- **Safe Public Crates:** All public host, guest, derive macro, trust, manifest, SDK-generation, and CLI crates enforce `#![forbid(unsafe_code)]`, isolating raw FFI pointers inside internal `-sys` crates.

> **Native Trust Model:** Native shared libraries execute in-process with host OS privileges. `bevy_abi_mod` protects cooperative/trusted native mods against compiler ABI skew, allocator crossover, and accidental panics; it is **not** a sandbox against adversarial machine code (use WebAssembly for untrusted user scripts). Enabling the optional `signature-verification` feature verifies Ed25519 `SSHSIG` signatures, CA certificates, `.bam_sig` / `.bampack` payloads, and `SSHKRL` revocation lists **before** mapping any library into memory.

---

## Quickstart

### Option A: Scaffold & Build with `cargo-bam` (Recommended)

Install the standalone CLI tool (`crates/cargo-bam`):

```bash
cargo install --path crates/cargo-bam
```

Create, build, and inspect a guest mod in Rust, C99, C++17, or Zig:

```bash
# Scaffold a guest project (also supports --lang c99, cpp17, or zig)
cargo bam new my_mod --lang rust

# Build the shared library (optionally sign with --sign-key <key>)
cargo bam build --dir my_mod --release

# Validate mod.toml manifests & SemVer dependency order in a mods folder
cargo bam check-mods ./mods

# Export single-source C99 / C++17 / Zig headers and diagnose local toolchains
cargo bam export-sdk --out ./include
cargo bam doctor
```

---

### Option B: Minimal Bevy Host & Rust Guest

#### 1. Host Application (`bevy_abi_mod`)

Add `bevy_abi_mod`, `bevy_app`, and `bevy_ecs` to your game's `Cargo.toml`:

```toml
[dependencies]
bevy_app = "0.20"
bevy_ecs = "0.20"
bevy_abi_mod = { version = "0.2.0", path = "crates/bevy_abi_mod" }
```

Register shared components and load guest libraries in Bevy:

```rust
use bevy_abi_mod::prelude::*;
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
version = "0.2.0"
edition = "2024"

[workspace]

[lib]
crate-type = ["cdylib"]

[dependencies]
bevy_abi_mod_guest = { version = "0.2.0", path = "../crates/bevy_abi_mod_guest", features = ["derive"] }
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
    const VERSION: &'static CStr = c"0.2.0";

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

Enable extra host capabilities via Cargo feature flags on `bevy_abi_mod` or `bevy_abi_mod_host`:

| Feature | Purpose |
|---|---|
| `strict-ffi-checks` | Debug/audit poisoning of scoped FFI context headers on return |
| `diagnostics` | Publishes per-mod and aggregate `bevy_diagnostic` metrics |
| `signature-verification` | Enforces pre-load OpenSSH Ed25519 `SSHSIG`, CA cert, `SSHKRL`, `.bam_sig`, and `.bampack` checks via `ModTrustPolicy` |
| `hot-reload-watcher` | Enables debounced filesystem watching (`ModHotReloadPlugin`) |
| `bridge-std-types` | Bridges `Transform`, `GlobalTransform`, `Visibility`, `Name`, `Time<*>`, and `ButtonInput<*>` |
| `bridge-asset` | Sandboxed `mod://<namespace>/...` Bevy `AssetSource` and refcounted `FfiAssetHandle` |
| `bridge-state` | Exposes Bevy `States`, transition schedules, and `in_state` run conditions |
| `bridge-reflect` | Witness `DynamicStruct` reflection, `bevy_remote` BRP endpoints, and `bevy_settings` TOML sync |
| `bridge-bsn` | Supports Bevy `0.20` `bsn!` scenes, `ScenePatch`, `cmd_spawn_scene`, and `Ready` observers |
| `sub-app-extract` | Cross-world `SubApp` extraction and data-driven `.wesl` / material support |

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

---

## Documentation & Technical Reports

- **[Developer Guide (`docs/book`)](./docs/book/src/SUMMARY.md)** — Walkthroughs for Rust, C99, C++17, and Zig guests, host scheduling, hot-reload, optional bridges, `cargo-bam`, and signature verification.
- **[Wire Specification (`spec.md`)](./spec.md)** — Normative ABI `0.3.0` (`0x0003_0000`) contract, 68-slot `BamHostApi` table, crate boundaries, and invariants.
- **[Architecture & Contributor Workflow (`docs/ARCHITECTURE.md`)](./docs/ARCHITECTURE.md)** — Eleven-crate workspace topology, `DROP_PAGES` reclamation state machine, cryptographic trust architecture, and local CI / Miri / Loom verification commands.
- **[Benchmarks & Regression Gates (`docs/BENCHMARKS.md`)](./docs/BENCHMARKS.md)** — Canonical throughput benchmarks, reference Apple M4 Pro baselines, deterministic `iai-callgrind` / zero-allocation gates, and the 34-archetype permutation suite.
- **[Changelog (`CHANGELOG.md`)](./CHANGELOG.md)** — Release history and unreleased changes.

---

## License

Dual-licensed under `MIT OR Apache-2.0`.
