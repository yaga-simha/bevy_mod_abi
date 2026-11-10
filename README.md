# `bevy_abi_mod`

**Asymmetric, Zero-Serialization C-ABI Native Modding Framework for Bevy ECS (v0.20+)**

[![C-ABI Boundary](https://img.shields.io/badge/C--ABI-Strict%20C99-blue.svg)](#)
[![Bevy Compatibility](https://img.shields.io/badge/Bevy-v0.20%2B-orange.svg)](#)
[![License](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-green.svg)](#)
[![Tests Passing](https://img.shields.io/badge/Tests-345%2B%20Passing-brightgreen.svg)](#)
[![Throughput](https://img.shields.io/badge/Throughput-~20M%20entities%2Fsec-purple.svg)](#)

`bevy_abi_mod` is an industrial-grade native plugin and modding architecture designed for Bevy Engine. It enables loading external native shared libraries (`.so`, `.dylib`, `.dll`) compiled with different Rust toolchains, C99, C++, or Zig without sharing Rust ABI assumptions, without memory allocator crossover, and with **zero serialization overhead**.

---

## Key Highlights

- **Zero Shared Rust ABI**: Mods can be compiled with different `rustc` versions, different optimization flags, or written in pure C99/C++/Zig.
- **Hardware-Speed Contiguous Traversal**: Systems iterate directly over Bevy's contiguous Archetype Table column buffers via opaque pointers and byte strides. No JSON, Protobuf, or Bincode serialization.
- **Bevy Parallel Executor Integration**: Mod systems register component queries (`READ`, `WRITE`, `FILTER_ONLY`) which dynamically populate Bevy's `FilteredAccessSet<ComponentId>`. Bevy's multi-threaded scheduler runs non-conflicting guest and host systems in parallel across CPU cores.
- **Zero Memory Allocator Crossover**: Host and guest maintain independent allocators. Host allocates host memory; guest allocates guest memory.
- **Built-in Panic & Fault Isolation**: Guest Rust panics are trapped inside the `.cdylib` runtime via `catch_guest_panic` and translated to `FfiResult::PANIC_CAUGHT`. Faulted systems are disabled safely without crashing Bevy's main loop.
- **Seamless Change Detection**: Mutating column bytes automatically advances Bevy's `AtomicTick`, allowing native Bevy queries using `Changed<T>` to react to mod mutations instantly.
- **Deterministic Hot-Reloading**: Full lifecycle management with clean teardown, system unregistration, and `dlclose` library unmapping.

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
├── Makefile                 # Developer workflow targets (build, test, bench, e2e)
├── include/
│   └── bevy_abi_mod.h       # Canonical C99/C++ header matching the ABI contract
├── crates/
│   ├── abi_contract/        # Zero-dependency #[no_std] C-ABI data structures & function pointers
│   ├── mod_derive/          # Proc-macro providing #[derive(Component, ModExposable)]
│   ├── host_runtime/        # Bevy plugin, DynamicCSystem, HostChunkIter, ModRegistry, ModHost
│   └── guest_sdk/           # Ergonomic, Bevy-independent SDK for Rust guest mod authors
├── tests/
│   ├── asymmetric_rust_mod/ # Standalone .cdylib Rust mod (isolated Cargo.lock)
│   └── asymmetric_c_mod/    # Pure C99 mod compiled with clang/gcc
├── docs/
│   ├── ARCHITECTURE.md      # Detailed system design, memory layout & concurrency contracts
│   ├── GUEST_MOD_GUIDE.md   # Step-by-step authoring guide for Rust, C99, C++, and Zig mods
│   └── TESTING_AND_VERIFICATION.md # 4-tier E2E testing framework & benchmarks
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

#[derive(Component, ModExposable, Default)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Component, ModExposable, Default)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}
```

### 3. Initialize the Mod Plugin & Load Mods
```rust
fn main() {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins)
       .add_plugins(AbiModPlugin::default());

    // Register exposed components with the mod registry
    app.register_mod_component::<Position>()
       .register_mod_component::<Velocity>();

    // Load an asymmetric native mod (.so / .dylib / .dll)
    let mut mod_host = app.world_mut().resource_mut::<ModHost>();
    let mod_id = mod_host.load_mod(
        app.world_mut(),
        "path/to/libmy_mod.dylib"
    ).expect("Failed to load mod");

    app.run();
}
```

---

## Quickstart: Writing a Mod

### Option A: Rust Mod (using `guest_sdk`)

```rust
use guest_sdk::prelude::*;
use core::ffi::c_void;

#[repr(C)]
pub struct PositionVTable {
    pub size: usize,
    pub align: usize,
    pub get_x: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_x: unsafe extern "C" fn(*mut c_void, f32),
    pub get_y: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_y: unsafe extern "C" fn(*mut c_void, f32),
    pub get_z: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_z: unsafe extern "C" fn(*mut c_void, f32),
}

unsafe fn update_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    // Iterate over matching archetype chunks at hardware speed
    FfiResult::OK
}

wrap_system_callback!(fn c_update_system calls update_system);

unsafe fn mod_init(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
) -> FfiResult {
    // Resolve VTables and register systems into Bevy's schedule
    FfiResult::OK
}

unsafe fn mod_shutdown(_host_context: *mut c_void) -> FfiResult {
    FfiResult::OK
}

export_mod!(mod_init, mod_shutdown);
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
    // Resolve symbols and register systems via host_api->register_system
    return FFI_RESULT_OK;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_shutdown(void* host_context) {
    return FFI_RESULT_OK;
}
```

---

## Developer Experience & Tooling

The root `Makefile` provides unified workflows for building, testing, and benchmarking:

| Command | Action |
|---|---|
| `make build` | Compiles workspace crates and all test mods (C99 & Rust cdylib). |
| `make test` | Runs all workspace unit and integration tests (`345+` tests). |
| `make test-e2e` | Runs the full 9-stage end-to-end verification pipeline. |
| `make bench` | Executes high-scale benchmarks and outputs `target/benchmark_results.json`. |
| `make check` | Runs static type checks across all targets with 0 warnings/errors. |
| `make fmt` | Formats all Rust code across the workspace. |
| `make clean` | Cleans all target directories and build artifacts. |
| `make help` | Displays list of all available commands. |

---

## Performance Benchmarks

Benchmarks run directly inside live Bevy ECS worlds across monolithic and fragmented archetype distributions:

| Scale | Throughput | Latency | Iteration Overhead vs Native |
|---|---|---|---|
| **1,000 Entities** | **~11.6M entities/sec** | 86.4 ns / entity | Zero serialization cost |
| **10,000 Entities** | **~19.1M entities/sec** | 52.4 ns / entity | Matches native Bevy query loop |
| **100,000 Entities** | **~20.3M entities/sec** | 49.3 ns / entity | Direct contiguous memory traversal |
| **16-Way Archetypes** | **~18.5M entities/sec** | 54.0 ns / entity | High cache locality across chunks |

---

## Detailed Documentation

- **[System Architecture & Design](docs/ARCHITECTURE.md)**: Deep dive into the memory isolation model, archetype table traversal, and borrow-checked dynamic scheduling.
- **[Guest Mod Author Guide](docs/GUEST_MOD_GUIDE.md)**: Complete guide to creating mods in Rust, C99, C++, and Zig.
- **[Testing & Verification Framework](docs/TESTING_AND_VERIFICATION.md)**: Overview of the 4-tier E2E test suite, safety stress tests, and hot-reload lifecycle suites.
- **[C-ABI Header](include/bevy_abi_mod.h)**: Canonical C99 header for native mod authors.

---

## License

Dual-licensed under either of:
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
