# `bevy_abi_mod`

**Asymmetric, Zero-Serialization C-ABI Native Modding Framework for Bevy ECS (v0.20+)**

[![C-ABI Boundary](https://img.shields.io/badge/C--ABI-Strict%20C99-blue.svg)](#)
[![Bevy Compatibility](https://img.shields.io/badge/Bevy-v0.20%2B-orange.svg)](#)
[![License](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-green.svg)](#)
[![Tests Passing](https://img.shields.io/badge/Tests-530%2B%20Passing-brightgreen.svg)](#)
[![Throughput](https://img.shields.io/badge/Throughput-~20M%20entities%2Fsec-purple.svg)](#)

`bevy_abi_mod` is an industrial-grade native plugin and modding architecture designed for Bevy Engine. It enables loading external native shared libraries (`.so`, `.dylib`, `.dll`) compiled with different Rust toolchains, C99, C++, or Zig without sharing Rust ABI assumptions, without memory allocator crossover, and with **zero serialization overhead**.

---

## Key Highlights

- **Zero Shared Rust ABI**: Mods can be compiled with different `rustc` versions, different optimization flags, or written in pure C99/C++/Zig.
- **Hardware-Speed `Table` & `SparseSet` Traversal**: Systems iterate directly over Bevy's contiguous Archetype `Table` column buffers (with automatic archetype-aware support for `StorageType::SparseSet` data and `With<T>`/`Without<T>` filters) via opaque pointers, byte strides, and safe `Chunk` split-borrow views.
- **Compile-Time POD vs. Opaque Safety (`AbiPod` vs. `ModExposable`)**: Strictly separates `#[repr(C)]` Plain Old Data (`AbiPod`, eligible for zero-copy typed guest slices) from `#[repr(Rust)]`, `String`, `Vec<T>`, and `bool` components (accessed via `#[repr(C)]` VTables using `FfiBool = u8` and `FfiEntity::NULL = 0` so foreign bytes never trigger Rust UB).
- **Bevy Parallel Executor & Schedule Integration**: Mod systems build native `QueryState` / `FilteredAccessSet<ComponentId>` permissions (`READ`, `WRITE`, `FILTER_ONLY`, `WITHOUT`), target any Bevy schedule (`Startup`, `PreUpdate`, `Update`, `PostUpdate`, `FixedUpdate`, `Last`), and declare `.after()` / `.before()` ordering via `ScheduledSystemDescriptor`.
- **Configurable Fault Policies & Typed Errors**: Guest Rust panics are trapped inside the `.cdylib` runtime via `catch_guest_panic` (`FfiResult::PANIC_CAUGHT`). Hosts configure `ModErrorPolicy` (`Quarantine`, `Log`, or `ReportToBevy`), inspect `ModLoadError`, and observe `ModLoaded`, `ModUnloaded`, and `ModSystemFaulted` ECS events.
- **Seamless Change Detection**: `DynamicCSystem` advances `World` change ticks accurately (`Changed<T>` parity across multi-frame schedules and `u32::MAX` tick wraparound).
- **Bounded-Memory Hot-Reloading**: Unloading deactivates atomic tombstones, excises system nodes via `Schedule::remove_systems_in_set(ModSystemSet::Mod(id))`, runs `bevy_abi_mod_shutdown` (freeing `GuestApp` closures in LIFO order), and safely `dlclose`s the shared library.

---

## Architecture Overview

```
                      ┌──────────────────────────────────────────┐
                      │            Bevy Engine Host              │
                      │  - AbiModPlugin & ModHost                │
                      │  - ModRegistry (Symbols & VTables)       │
                      │  - DynamicCSystem (impl System)          │
                      │  - Bevy Parallel Schedule Executor       │
                      └────────────────────┬─────────────────────┘
                                           │
                     ══════════════════════╪══════════════════════
                     STRICT C-ABI BOUNDARY │ (crates/abi_contract)
                     ══════════════════════╪══════════════════════
                                           │
         ┌─────────────────────────────────┴─────────────────────────────────┐
         │                                                                   │
         ▼                                                                   ▼
┌─────────────────────────────────┐                         ┌─────────────────────────────────┐
│     Asymmetric Rust Mod         │                         │        Pure C99 Mod             │
│ (tests/asymmetric_rust_mod)     │                         │  (tests/asymmetric_c_mod)       │
│ - Independent Cargo.lock        │                         │ - bevy_abi_mod.h                │
│ - crates/guest_sdk              │                         │ - Standard C99 / clang / gcc    │
│ - catch_guest_panic isolation   │                         │ - Zero runtime dependencies     │
└─────────────────────────────────┘                         └─────────────────────────────────┘
```

---

## Workspace Layout

```text
bevy_abi_mod/
├── Cargo.toml               # Workspace manifest
├── Makefile                 # Developer workflow targets (build, test, test-c99, clippy, miri, bench)
├── .github/workflows/ci.yml # Multi-OS matrix, strict C99, Miri, ASan/UBSan & -Zrandomize-layout CI
├── include/
│   └── bevy_abi_mod.h       # Canonical C99/C++ header matching the ABI contract
├── crates/
│   ├── abi_contract/        # Zero-dependency #[no_std] C-ABI data structures & function pointers
│   ├── mod_derive/          # Proc-macros providing #[derive(ModExposable, AbiPod)]
│   ├── host_runtime/        # Bevy plugin, DynamicCSystem, HostChunkIter, ModRegistry, ModHost
│   └── guest_sdk/           # Ergonomic, Bevy-independent SDK (`GuestApp`, `Chunk` split borrows)
├── tests/
│   ├── standalone_c_test.c  # Standalone ISO C99 ABI layout & SemVer harness
│   ├── asymmetric_rust_mod/ # Standalone .cdylib Rust mod (isolated Cargo.lock)
│   └── asymmetric_c_mod/    # Pure C99 mod compiled with clang/gcc
├── benches/
│   ├── shared_scenario/     # 34-archetype nested Query combat scenario & OracleDigest
│   ├── coupled_rust_mod/    # Coupled Rust dylib plugin (unstable Rust ABI comparison)
│   ├── c_abi_rust_mod/      # Standalone C-ABI Rust cdylib (ChunkOptimized / EntityNested / NaiveChatty)
│   ├── c_abi_c99_mod/       # Pure ISO C99 mod (-O0 debug & -O3 release across all 3 FFI tiers)
│   └── permutation_bench/   # Multi-profile (Debug + Release) orchestrator & report generator
├── docs/
│   ├── ARCHITECTURE.md            # System design, formal memory safety proofs & concurrency
│   ├── HOST_INTEGRATION_GUIDE.md  # Comprehensive guide for Bevy game developers hosting mods
│   ├── C_ABI_SPECIFICATION.md     # Formal C-ABI specification for C99, C++, and Zig mod authors
│   ├── GUEST_MOD_GUIDE.md         # Authoring guide for Rust, C99, C++, and Zig guest mods
│   ├── BENCHMARKS.md              # Full Debug/Release x Native/dylib/C-ABI degradation ladder
│   └── TESTING_AND_VERIFICATION.md # Differential parity, fuzzing, compile-fail & E2E suites
└── scripts/
    └── run_e2e_tests.sh     # 9-stage end-to-end verification pipeline
```

---

## Quickstart: Host Game (Bevy Engine)

### 1. Add Dependencies
```toml
[dependencies]
bevy = "0.20"
host_runtime = { path = "crates/host_runtime" }
```

### 2. Expose Components with `#[derive(ModExposable)]`
```rust
use bevy::prelude::*;
use host_runtime::prelude::*;

// #[repr(C)] + Copy components automatically implement AbiPod + FfiLayout for zero-copy slices;
// standard #[repr(Rust)] or String/Vec/bool structs also work via generated #[repr(C)] VTables!
#[repr(C)]
#[derive(Component, Clone, Copy, Default, ModExposable)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, ModExposable)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}
```

### 3. Initialize the Mod Plugin & Load Mods
```rust
use bevy::prelude::*;
use host_runtime::prelude::*;

fn main() {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins)
       .add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));

    // Register exposed components (dense Table or SparseSet) with the mod registry
    app.register_mod_component::<Position>()
       .register_mod_component::<Velocity>();

    // Load an asymmetric native mod (.so / .dylib / .dll) with typed ModLoadError diagnostics
    let mod_id = app.try_load_mod("path/to/libmy_mod.dylib")
        .expect("Failed to load mod");

    // Hot-unload cleanly at runtime (excises systems via ModSystemSet::Mod(mod_id)):
    // app.try_unload_mod(mod_id).expect("Failed to unload mod");
    let _ = mod_id;

    app.run();
}
```

---

## Quickstart: Writing a Mod

### Option A: Idiomatic Rust Mod (`GuestApp` + `Chunk` Split Borrows)

```rust
use guest_sdk::prelude::*;

#[repr(C)]
#[derive(Clone, Copy, AbiPod)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Clone, Copy, AbiPod)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

fn setup(app: &mut GuestApp<'_>) -> FfiResult {
    let queries = [
        ComponentAccessDesc::write(c"Position".as_ptr()),
        ComponentAccessDesc::read(c"Velocity".as_ptr()),
    ];

    app.add_system(c"kinematics_system", &queries, |ctx| {
        while let Some(mut chunk) = ctx.iter().next_chunk_view() {
            // Safe disjoint split-borrow: column 1 (&[Velocity]) + column 0 (&mut [Position])
            let Some((vel, pos)) = chunk.read_write::<Velocity, Position>(1, 0) else {
                return FfiResult::INTERNAL_ERROR;
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

// Exports bevy_abi_mod_abi_version, bevy_abi_mod_init, and bevy_abi_mod_shutdown
// with automatic guest-side panic containment and LIFO closure cleanup on unload.
export_mod!(setup);
```

### Option B: Pure C99 Mod (using `bevy_abi_mod.h`)

```c
#include "bevy_abi_mod.h"

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* host_api,
    void* host_context
) {
    if (!host_api || !bevy_abi_version_is_compatible(host_api->abi_version, BEVY_ABI_VERSION_PACKED)) {
        return FFI_RESULT_INVALID_VERSION;
    }
    (void)get_proc_address;
    (void)host_context;
    // Resolve VTables / scheduled registration and register systems via host_api->register_system
    return FFI_RESULT_OK;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_shutdown(void* host_context) {
    (void)host_context;
    return FFI_RESULT_OK;
}
```

---

## Developer Experience & Tooling

The root `Makefile` provides unified workflows for building, testing, linting, Miri verification, and benchmarking:

| Command | Action |
|---|---|
| `make build` | Compiles workspace crates and all test mods (C99 & Rust cdylib). |
| `make test` | Runs all workspace unit, differential parity, fuzz, compile-fail, and integration tests. |
| `make test-c99` | Compiles C99 mod & runs `standalone_c_test.c` with `-std=c99 -Wall -Wextra -Werror`. |
| `make clippy` | Runs `cargo clippy --workspace --all-targets -- -D warnings`. |
| `make miri` | Runs Miri (`-Zmiri-tree-borrows`) on in-process ABI, macro, SDK, fuzz, and parity suites. |
| `make test-e2e` | Runs the full 9-stage end-to-end verification pipeline. |
| `make bench` | Executes full multi-architecture & cross-profile permutation benchmark matrix. |
| `make bench-quick` | Runs Milestone 4 host_runtime single-query benchmark (1k, 10k, 100k entities). |
| `make check` | Runs static type checks across all targets with 0 warnings/errors. |
| `make fmt` | Formats all Rust code across the workspace. |
| `make clean` | Cleans all target directories and build artifacts. |
| `make help` | Displays list of all available commands. |

---

## Performance Benchmarks & Complete Degradation Ladder

`make bench` (`cargo run --release -p permutation_bench`) benchmarks **every permutation** of **Pure Native Bevy (no-mod)**, **Coupled Rust `dylib` Mod**, **C-ABI Rust `cdylib` Mod**, and **C-ABI C99 Mod** across both **`dev`/`debug`** and **`release`/`-O3`** targets inside a complex **34-archetype nested `Query` simulation** (`1,024` combat units across 16 archetypes × `48` aura emitters across 6 archetypes + 12 excluded decoy/frozen archetypes = **`49,152` pair evaluations/frame**, plus a **`294,912` pairs/frame** stress tier), verified bit-accurately against a Pure Native Bevy mathematical oracle:

| Architecture | Access Pattern | Host | Mod Target | Frame (`ms`) | `ns/pair` | vs True Native | Lifecycle / ABI Compatibility |
|---|---|---|---|---:|---:|---:|---|
| **Pure Native Bevy (No-Mod)** | Pre-Cached SoA + Inlined `Query` | `release` | `static` | **`0.186 ms`** | `3.79 ns` | **`1.00x`** | Static baseline (`7.8 µs` schedule init) |
| **Coupled Rust `dylib` Mod** | Pre-Cached SoA + Shared `Query` | `release` | `release` | **`0.182 ms`** | `3.71 ns` | **`0.98x`** | **ABI-Locked** across `debug`/`release` & unsafe `dlclose` |
| **C-ABI C99 Mod** | **`ChunkOptimized` ($O(U+E)$ VTable)** | `release` | `release (-O3)` | **`0.186 ms`** | `3.78 ns` | **`1.00x`** | **`33 KB` binary, `201 µs` handshake, clean `dlclose`** |
| **C-ABI Rust `cdylib` Mod** | **`ChunkOptimized` ($O(U+E)$ VTable)** | `release` | `release` | **`0.215 ms`** | `4.38 ns` | **`1.15x`** | **Decoupled toolchain, `152 µs` handshake, clean `dlclose`** |
| **Pure Native Bevy (No-Mod)** | Direct Idiomatic Nested `Query` | `release` | `static` | **`0.218 ms`** | `4.43 ns` | **`1.17x`** | Re-walks 6 emitter tables per unit |
| **C-ABI C99 / Rust Mod** | `ChunkOptimized` ($O(U+E)$ VTable) | **`debug`** | `release` | `0.39–0.44 ms` | `7.96–8.88 ns` | **`2.10x–2.34x`** | **`20x` faster than `Debug` Native Bevy (`8.26 ms`)!** |
| **C-ABI C99 / Rust Mod** | `EntityNestedVTable` ($O(U\times E)$ Getters) | `release` | `release` | `0.70–0.72 ms` | `14.3–14.6 ns` | **`3.78x–3.85x`** | `~600k` indirect VTable calls/frame |
| **C-ABI C99 / Rust Mod** | `ChunkOptimized` ($O(U+E)$ VTable) | `release` | **`debug`** | `0.94–1.14 ms` | `19.2–23.2 ns` | **`5.07x–6.12x`** | Source-level mod debugging at `< 7%` of 60 FPS budget |
| **C-ABI Rust / C99 Mod** | `NaiveChattyFfi` ($O(U\times E)$ R/W + Tick) | `release` | `release` | `1.44–1.47 ms` | `29.3–29.9 ns` | **`7.72x–7.88x`** | Per-pair VTable R/W + `iter_mark_changed` |
| **Pure Native / Coupled `dylib`** | Direct Idiomatic Nested `Query` | **`debug`** | **`debug`** | `8.26–8.32 ms` | `168–169 ns` | **`44.3x–44.6x`** | Unoptimized `bevy_ecs` iterator overhead (`50%` of 60 FPS) |
| **C-ABI C99 / Rust Mod** | `NaiveChattyFfi` ($O(U\times E)$ R/W + Tick) | **`debug`** | **`debug`** | `12.15–12.39 ms` | `247–252 ns` | **`65.2x–66.5x`** | Worst-case unoptimized Host + unoptimized chatty Mod |

See **[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md)** for the full 34-row degradation table, cold vs. warm `dlopen` loading breakdown, and the `294,912` pairs/frame stress tier.

---

## Detailed Documentation

- **[System Architecture & Design](docs/ARCHITECTURE.md)**: Deep dive into the zero shared Rust ABI model, formal memory safety proofs (allocator isolation, self-aliasing prevention, atomic tombstoning), multi-query mechanics, and ASCII lifecycle flowcharts.
- **[Host Integration Guide](docs/HOST_INTEGRATION_GUIDE.md)**: Comprehensive guide for Bevy game developers hosting native mods, configuring plugins, exposing components via `#[derive(ModExposable)]`, and managing runtime lifecycles.
- **[C-ABI Specification](docs/C_ABI_SPECIFICATION.md)**: Formal C-ABI contract, calling conventions, struct layout specifications, and symbol resolution rules for C99, C++, and Zig mod authors.
- **[Guest Mod Author Guide](docs/GUEST_MOD_GUIDE.md)**: Practical authoring guide for creating native mods in Rust (`guest_sdk`), ISO C99, C++, and Zig.
- **[Comprehensive Benchmark Matrix (`Debug` & `Release`)](docs/BENCHMARKS.md)**: Full loading & nested multi-archetype execution comparison across Pure Native Bevy, Coupled Rust `dylib`, C-ABI Rust `cdylib`, and C-ABI C99.
- **[Testing & Verification Framework](docs/TESTING_AND_VERIFICATION.md)**: Overview of the 4-tier E2E test suite, safety stress tests, and hot-reload lifecycle suites.
- **[C-ABI Header](include/bevy_abi_mod.h)**: Canonical C99/C++ header for native mod authors.

---

## License

Dual-licensed under either of:
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
