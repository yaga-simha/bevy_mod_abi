# Project: bevy_abi_mod Bevy Runtime Testbed, Benchmarks, Safety, and Lifecycle Verification

## Architecture
- **Boundary**: Strict asymmetric C-ABI (`crates/abi_contract`, `#![no_std]`, no external dependencies, no Bevy dependency).
- **Macro**: `crates/mod_derive` generates `#[repr(C)]` VTables, size/align strides, interior pointer and host-allocated mutators.
- **Guest SDK**: `crates/guest_sdk` provides safe Rust guest wrapper with `catch_guest_panic` and `export_mod!`.
- **Host Runtime**: `crates/host_runtime` integrates with Bevy ECS 0.20 (`bevy_ecs`, `bevy_app`, `libloading`). Exposes `ModRegistry`, `RawChunkIter` over Bevy `Table` columns, and `DynamicCSystem` with `FilteredAccessSet` scheduling.
- **Testbed Environment**: Representative Bevy application runtime hosting native C and Rust mods in shared schedules with multi-archetype topologies and real game coding scenarios.
- **Benchmarking Suite**: Dual-mode (programmatic CI CLI runner emitting JSON baselines + Criterion statistical harness) measuring chunk iteration throughput, FFI invocation overhead, and scaling across 1k, 10k, 100k entities.
- **Safety & Lifecycle Verification**: Automated suites verifying memory alignment (1, 4, 8, 16, 64 bytes), pointer safety, borrow conflict detection, allocator isolation, panic containment, 50-cycle mod hot-reloading, and post-unload execution safety.

## Code Layout
- `crates/abi_contract/`: C-ABI types, VTables, function pointers. (Owned by M1)
- `crates/mod_derive/`: Procedural macro for exposable components.
- `crates/guest_sdk/`: Safe guest mod builder and chunk iterator wrappers. (Owned by M2)
- `crates/host_runtime/src/`:
  - `registry.rs`: `ModRegistry` and symbol resolution. (Owned by M1)
  - `chunk_iter.rs`: `RawChunkIter` implementation on Bevy `Table`/`Column`. (Owned by M1)
  - `dynamic_system.rs`: `DynamicCSystem` implementing `bevy_ecs::system::System`. (Owned by M1)
  - `commands.rs`: `CommandQueueBridge` for deferred operations. (Owned by M1)
  - `plugin.rs` / `lib.rs`: `ModHost` and `AbiModPlugin`. (Owned by M1)
- `tests/asymmetric_rust_mod/`: Rust guest `.cdylib` implementing test scenarios. (Owned by M2)
- `tests/asymmetric_c_mod/`: C99 guest dynamic library implementing test scenarios. (Owned by M2)
- `crates/host_runtime/tests/`:
  - `common/`: Testbed builder, archetypes, entity generators, mod compilation. (Owned by M3)
  - `integration_tests.rs`: Bevy runtime testbed running C and Rust mods. (Owned by M3)
  - `benchmarks.rs`: Programmatic benchmark suite with JSON metrics output. (Owned by M4)
  - `safety_tests.rs`: Alignment, bounds, borrow rules, panic containment. (Owned by M5)
  - `lifecycle_tests.rs`: Mod reload stress, dangling pointer prevention, world consistency. (Owned by M5)
- `benches/chunk_throughput.rs`: Criterion micro-benchmark suite. (Owned by M4)

## Feature Inventory
| # | Feature | Description | Milestone | Source |
|---|---------|-------------|-----------|--------|
| 1 | Host Mod Registry (`ModRegistry`) | Maps ComponentUuid/name to Bevy ComponentId, stores static VTables, resolves get_proc_address | M1 | spec.md §4.3, miner |
| 2 | Dynamic C System (`DynamicCSystem`) | Bevy System implementation wrapping guest callbacks with FilteredAccessSet permissions | M1 | spec.md §4.3, miner |
| 3 | Raw Chunk Iterator (`RawChunkIter`) | Traverses Bevy Table columns matching query, exposing contiguous pointers & byte strides | M1 | spec.md §4.1,4.3, miner |
| 4 | Deferred Command Bridge (`CommandQueueBridge`) | Bridges guest deferred entity spawns/despawns/insertions to Bevy Commands | M1 | spec.md §4.1,4.3, miner |
| 5 | 3D Kinematics Scenario | Testbed scenario updating Position with Velocity across C and Rust native mods | M2, M3 | ORIGINAL_REQUEST §R1, miner |
| 6 | Combat & Health Damage Scenario | Testbed scenario with Health component verifying change detection (Changed<Health>) | M2, M3 | ORIGINAL_REQUEST §R1, miner |
| 7 | Dynamic Metadata Scenario | Testbed scenario exercising NameTag(String) with host-heap allocation across FFI | M2, M3 | ORIGINAL_REQUEST §R1, miner |
| 8 | Dynamic Buffer Scenario | Testbed scenario exercising Vec<u32> (Scores/Cooldowns) slice borrowing & resizing | M2, M3 | ORIGINAL_REQUEST §R1, miner |
| 9 | Chunk Iteration Throughput Benchmark | Measures ns/entity and throughput (entities/sec) for guest mods processing table columns | M4 | ORIGINAL_REQUEST §R2, miner |
| 10 | C-ABI Boundary Overhead Benchmark | Measures overhead of crossing C-ABI boundary vs identical native Bevy system | M4 | ORIGINAL_REQUEST §R2, miner |
| 11 | Multi-Entity Scale Scaling | Evaluates memory and throughput scaling across 1K, 10K, and 100K entity scales | M4 | ORIGINAL_REQUEST §R2, miner |
| 12 | Archetype Fragmentation Benchmark | Measures chunk iteration efficiency across monolithic, 4-way, and 16+ fragmented archetypes | M4 | ORIGINAL_REQUEST §R2, miner |
| 13 | Baseline Recording & Regression Tool | Programmatic harness outputting benchmark metrics to JSON for regression comparisons | M4 | ORIGINAL_REQUEST §R2, miner |
| 14 | Component Alignment Verification | Verifies pointer alignment across heterogeneous components (1, 4, 8, 16, 64 bytes) | M5 | ORIGINAL_REQUEST §R3, miner |
| 15 | Chunk Iterator Bounds & Empty Table Guard | Verifies safety on empty tables (0 entities), partial chunks, and iterator exhaustion | M5 | ORIGINAL_REQUEST §R3, miner |
| 16 | FFI Transparent Type Safety | Verifies foreign integer values for FfiResult/ComponentAccessMode cannot cause enum UB | M5 | ORIGINAL_REQUEST §R3, miner |
| 17 | Self-Aliasing String/Vec Overlap Guard | Protects host heap from use-after-free when passing existing slice back into setter | M5 | ORIGINAL_REQUEST §R3, miner |
| 18 | Bevy ECS Borrow Rule Enforcement | Validates conflicting read/write component access across guest systems is detected | M5 | ORIGINAL_REQUEST §R3, miner |
| 19 | Column Permission Enforcement | Defensively ensures READ or FILTER_ONLY columns cannot be mutably accessed | M5 | ORIGINAL_REQUEST §R3, miner |
| 20 | Cross-Runtime Panic Containment | Catches guest unwinds returning PANIC_CAUGHT; isolates and disables offending system | M5 | ORIGINAL_REQUEST §R3, miner |
| 21 | Memory Allocator Boundary Isolation | Ensures host memory is freed by host and guest by guest; no cross-boundary free operations | M5 | ORIGINAL_REQUEST §R3, miner |
| 22 | Mod Unloading & Teardown Hook | Executes bevy_abi_mod_shutdown and drops shared library handle cleanly | M5 | ORIGINAL_REQUEST §R4, miner |
| 23 | Mod Hot-Reloading | Unloads existing library and reloads new library into active Bevy runtime without restart | M5 | ORIGINAL_REQUEST §R4, miner |
| 24 | Repeated Registration Stress Cycle | Stress tests repeated registration, execution, and unloading cycles (50+ iterations) | M5 | ORIGINAL_REQUEST §R4, miner |
| 25 | Hot-Path Zero-Allocation Verification | Verifies DynamicCSystem execution and chunk iteration perform zero heap allocations | M5 | ORIGINAL_REQUEST §R4, miner |
| 26 | ECS World Consistency After Failure | Verifies Bevy World remains uncorrupted when a guest mod panics or unloads | M5 | ORIGINAL_REQUEST §R4, miner |

## Milestones
| # | Name | Scope | Dependencies | Status |
|---|------|-------|-------------|--------|
| M1 | Host Runtime Bevy ECS Engine Bridge | Add bevy_ecs/bevy_app 0.20 to host_runtime; implement ModRegistry, RawChunkIter over Table/Column, DynamicCSystem with FilteredAccessSet, CommandQueueBridge | none | DONE |
| M2 | Native Guest Mods (C & Rust) Scenarios | Implement kinematics, combat damage, string metadata, commands, and concurrency fixes in asymmetric_rust_mod & asymmetric_c_mod | M1 | DONE |
| M3 | Bevy Runtime Testbed Environment (R1) | Implement testbed builder (common/) and integration_tests.rs running native C & Rust mods across representative coding scenarios | M1, M2 | DONE |
| M4 | Performance Benchmarking Suite (R2) | Programmatic benchmark runner (benchmarks.rs) outputting JSON metrics + Criterion benchmark (benches/chunk_throughput.rs) | M1, M2, M3 | DONE |
| M5 | Safety & Lifecycle Test Suites (R3, R4) | Automated safety tests (alignment, bounds, borrow rules, panic, isolation) and lifecycle tests (50-cycle reload, dangling pointer guard, ECS consistency) | M1, M2, M3 | DONE |
| M6 | Final Verification & Hardening | Pass 100% E2E test suite (Tiers 1-4) and adversarial coverage hardening (Tier 5) | M1, M2, M3, M4, M5 | DONE |

## Interface Contracts

### Host Runtime ↔ Bevy ECS
- `DynamicCSystem::initialize(&mut self, world: &mut World)`:
  - Queries `world.components().get_id(uuid)` for each registered component.
  - Builds `FilteredAccessSet<ComponentId>`:
    - `READ` -> `add_unfiltered_component_read(id)`
    - `WRITE` -> `add_unfiltered_component_write(id)`
    - `FILTER_ONLY` -> `and_with(id)`
  - Verifies access compatibility; returns `SystemAccess::Shared(access)`.
- `DynamicCSystem::run_unsafe(&mut self, _input: (), world: UnsafeWorldCell)`:
  - Constructs `RawChunkIter` backed by `world.storages().tables`.
  - Iterates matching tables; passes column pointers and byte strides to guest callback.
  - Detects `FfiResult::PANIC_CAUGHT` or host caught unwind; logs and disables system.

### Host Runtime ↔ Guest Mods (C-ABI Handshake)
- Entry point: `bevy_abi_mod_init(get_proc_address: GetProcAddressFn, host_api: *const HostApi, host_context: *mut c_void) -> FfiResult`.
- Symbol lookup: `get_proc_address("<Component>_vtable")` returns static VTable address.
- Registration: `register_system(host_context, &SystemDescriptor)` registers callback, component access descriptors, and user data.
- Chunk iteration: `iter_next_chunk`, `iter_column_readonly`, `iter_column_mutable`, `iter_mark_changed`, `iter_entities`, `iter_column_stride`.
- Teardown: `bevy_abi_mod_shutdown(host_context: *mut c_void) -> FfiResult`.
