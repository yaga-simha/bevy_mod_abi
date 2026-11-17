# `bevy_abi_mod` (`ABI 0.2.0`)

**Asymmetric, Zero-Serialization C-ABI Native Modding Framework for Bevy ECS (`v0.20+`)**

[![ABI Version](https://img.shields.io/badge/ABI-0.2.0%20(repr(C))-blue.svg)](./spec.md)
[![Bevy Compatibility](https://img.shields.io/badge/Bevy-v0.20%2B-orange.svg)](https://bevyengine.org)
[![Safe Public API](https://img.shields.io/badge/Safe%20Layer-%23!%5Bforbid(unsafe__code)%5D-success.svg)](./spec.md)
[![License](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-green.svg)](#license)

`bevy_abi_mod` loads, schedules, quarantines, and hot-unloads native shared libraries (`.so`, `.dylib`, `.dll`) inside Bevy ECS (`v0.20+`) across **mismatched `rustc` versions**, **mixed Debug/Release optimization profiles**, and **ISO C99, C++17, and Zig**—with **zero serialization** and **zero steady-state heap allocations**.

> **Threat Model:** Native dynamic libraries execute in-process with full OS privileges of the host application. `bevy_abi_mod` isolates compiler ABIs, heap allocators, ECS aliasing rules, and `dlclose` lifecycles for **trusted native mods**; use WebAssembly (Wasm) for untrusted user-submitted code ([`spec.md` §1.3](./spec.md)).

---

## Core Capabilities (`ABI 0.2.0`)

- **Zero-Copy SoA & Sparse-Set Views + Parallel Chunks:** Guests read/write contiguous Bevy `Table` columns as typed `&[T]` / `&mut [T]` slices (`AbiPod`), batch-iterate `SparseSet` rows (`rows` / `rows_mut`), inspect or defer `Changed` ticks (`mark_changed_range`), and fan out via `par_chunks` (`ComputeTaskPool`).
- **`EntityWorldMut` Command Fidelity:** Deferred spawns, inserts, removes, and despawns execute via Bevy `EntityWorldMut` (`insert_by_ids` / `remove_by_id`), guaranteeing native `ComponentHooks`, `#[require(...)]` Required Components, and `ChildOf` / `Children` hierarchies (`FfiChildOf`).
- **Resources, Buffered `Messages<M>`, & `EntityEvent` Observers:** Access host singleton resources (`GuestRes<R>` / `GuestResMut<R>`) with Bevy `BamChangeTicks` (`is_added()`, `is_changed()`), exchange batched `Messages<M>`, emit `world.trigger()` events, and register dynamic global/`EntityEvent` observers.
- **Bevy `0.20+` Deterministic Scheduling & Host-Visible Sets:** Target built-in or custom `ScheduleId`s and order systems via `.in_set()`, `.before()`, and `.after()` across intra-mod, cross-mod (`"other_mod::step"`), and host-exported `ModVisibleSets` (`register_mod_visible_set` -> `"host::<name>"`).
- **Runtime Dynamic Components & Destructor Safety:** Mods attach private `#[repr(C)]` state to host entities via `ComponentDescriptor::new_with_layout` (`register_component_with_layout`), optional `on_add`/`on_remove` hooks, and 1,024 wait-free `DROP_SLOTS` trampolines protected by `WorldDropGuard`.
- **Shadow-Copy Hot Reload, Quarantine & Telemetry:** Core loader shadow-copying prevents OS DLL locks and Apple Silicon `CODESIGNING` faults; `ModQuarantine` isolates panics (`PANIC_CAUGHT`); `save_state` / `restore_state` preserves guest state across `reload_mod`.

---

## Crate Architecture (`-sys` vs. `#![forbid(unsafe_code)]`)

| Crate | `#![forbid(unsafe_code)]` | `bevy*` Deps | Purpose |
|---|---|---|---|
| [`bevy_abi_mod`](./crates/bevy_abi_mod) | **Yes** | Yes | Top-level Bevy host facade (`AbiModPlugin`, `ModWorldExt`, `ModComponent`, `prelude`). |
| [`bevy_abi_mod_host`](./crates/bevy_abi_mod_host) | **Yes** | Yes | Safe Bevy plugin, `LoadOptions`, `ComponentOptions`, `ModQuarantine`, lifecycle `Message`s. |
| [`bevy_abi_mod_host_sys`](./crates/bevy_abi_mod_host_sys) | No | Yes | Encapsulated host FFI (`HOST_API`, `HostIter`, `CommandArena`, `GuestSystem`, `DROP_SLOTS`). |
| [`bevy_abi_mod_guest`](./crates/bevy_abi_mod_guest) | **Yes** | **None** | Safe Rust guest SDK (`GuestPlugin`, `GuestApp`, `app.system(...)`, `Chunk`, `export_mod!`). |
| [`bevy_abi_mod_guest_sys`](./crates/bevy_abi_mod_guest_sys) | No | **None** | Guest FFI trampolines, `GuestInstance`, panic guards, validated column/row slices. |
| [`bevy_abi_mod_macros`](./crates/bevy_abi_mod_macros) | **Yes** | **None** | `#[derive(ModComponent, AbiPod)]` hygienic VTable & `SipHash-2-4-128` UUID codegen. |
| [`bevy_abi_mod_sys`](./crates/bevy_abi_mod_sys) | No (`#![no_std]`) | **None** | Zero-dependency `ABI 0.2.0` wire types & source for [`include/bevy_abi_mod.h`](./include/bevy_abi_mod.h). |

---

## Quickstart: Bevy Host (`bevy_abi_mod`)

```toml
[dependencies]
bevy_app = "0.20"
bevy_ecs = "0.20"
bevy_abi_mod = { path = "crates/bevy_abi_mod" }
```

```rust,no_run
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

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, ModComponent)]
#[mod_component(name = "my_game::Velocity")]
pub struct Velocity {
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
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register Velocity");

    app.world_mut().spawn((
        Position { x: 0.0, y: 0.0, z: 0.0 },
        Velocity { x: 1.0, y: 2.0, z: 3.0 },
    ));

    let mod_id = app
        .load_mod_with(
            "path/to/libmy_guest_mod.dylib",
            LoadOptions {
                shadow_copy: true,
                strict: true,
                ..Default::default()
            },
        )
        .expect("load mod");

    app.update();
    app.unload_mod(mod_id).expect("unload mod");
}
```

---

## Quickstart: Standalone Rust Guest (`bevy_abi_mod_guest`)

Guest `.cdylib` crates have **zero `bevy` dependencies** and compile with an independent `Cargo.lock`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
bevy_abi_mod_guest = { path = "crates/bevy_abi_mod_guest", features = ["derive"] }
```

```rust,no_run
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

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, ModComponent)]
#[mod_component(name = "my_game::Velocity")]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl GuestComponent for Velocity {
    const NAME: &'static CStr = c"my_game::Velocity";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[derive(Default)]
pub struct KinematicsPlugin;

impl GuestPlugin for KinematicsPlugin {
    const NAME: &'static CStr = c"kinematics_mod";
    const VERSION: &'static CStr = c"0.2.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        app.system(c"integrate_kinematics")
            .query(|q| q.write::<Position>().read::<Velocity>())
            .schedule(ScheduleId::UPDATE)
            .build(|ctx| {
                while let Some(mut chunk) = ctx.iter().next_chunk_view() {
                    let Some((vel, pos)) = chunk.read_write::<Velocity, Position>(1, 0) else {
                        return FfiResult::TYPE_MISMATCH;
                    };
                    for (p, v) in pos.iter_mut().zip(vel.iter()) {
                        p.x += v.x;
                        p.y += v.y;
                        p.z += v.z;
                    }
                }
                FfiResult::OK
            })
    }
}

export_mod!(plugin: KinematicsPlugin);
```

---

## Polyglot Examples (`examples/`)

| Example | Language(s) | Concept Taught |
|---|---|---|
| [`01_minimal_handshake`](./examples/01_minimal_handshake) | Rust | Minimal `BamManifest` handshake, shadow-copy loading, lifecycle `Message`s. |
| [`02_multi_system_pipeline`](./examples/02_multi_system_pipeline) | Rust | Scoped `.after()` ordering, query filters, singleton resources, deferred commands. |
| [`03_cross_language_interop`](./examples/03_cross_language_interop) | ISO C99 + Rust | Simultaneous C99 + Rust mods sharing Bevy components through `VTableHeader` VTables. |
| [`04_visual_game`](./examples/04_visual_game) | Rust (`ratatui`) | Interactive TUI arena with live runtime mod toggling and hot-reload. |
| [`05_cute_3d_winit`](./examples/05_cute_3d_winit) | Rust + C99 (`wgpu`) | Real-time 3D window (`avian3d` + `wgpu`) hot-swapping Rust & C99 physics mods. |
| [`06_cross_mod_components`](./examples/06_cross_mod_components) | Rust (2 mods) | Mod A registers a runtime component; Mod B queries & mutates it cross-mod. |
| [`07_cpp17_guest`](./examples/07_cpp17_guest) | ISO C++17 | Strict C++17 guest, per-`World` RAII `BamInstance`, `try/catch -> PANIC_CAUGHT`. |
| [`08_zig_guest`](./examples/08_zig_guest) | Zig | `@cImport("bevy_abi_mod.h")` guest, `errdefer` cleanup, Zig error union mapping. |

---

## Developer Workflow (`cargo xtask`)

```bash
# Run formatting, clippy, zero-Bevy checks, fixture builds, C99 tests, and parallel test suite
cargo xtask ci

# Regenerate & verify single-source ISO C99 header (include/bevy_abi_mod.h)
cargo xtask header --check --test-c99

# Run Miri (-Zmiri-tree-borrows -Zmiri-strict-provenance) across ABI, macro, guest, and host_sys
cargo xtask miri --all

# Run canonical throughput benchmarks & deterministic allocation/step-count gates
cargo xtask bench
cargo bench --manifest-path benches/deterministic_gate/Cargo.toml
```

---

## Documentation

- **[Developer Guide (mdBook)](./docs/book/src/SUMMARY.md)** — Quickstart, Rust/C99/C++17/Zig guest authoring, host schedules, shadow-copy hot reload, and performance tuning.
- **[Wire & Runtime Specification (`spec.md`)](./spec.md)** — Authoritative `ABI 0.2.0` struct layouts, handle lifetimes, and safety proofs.
- **[Benchmarks (`docs/BENCHMARKS.md`)](./docs/BENCHMARKS.md)** — Apple M4 Pro baselines across all 5 canonical scenarios and the 34-archetype permutation matrix.
- **[Architecture & Feature Summary (`docs/ARCHITECTURE.md`)](./docs/ARCHITECTURE.md)** — How each subsystem and feature is built across the 7-crate workspace.

## License

Dual-licensed under either **MIT** or **Apache-2.0** at your option.
