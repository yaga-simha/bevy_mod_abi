# Bevy Host Integration Guide

**Target Audience:** Bevy game engine developers integrating `bevy_abi_mod` into their host games to enable native, zero-serialization modding.  
**Framework Version:** `bevy_abi_mod` v0.1.0 (Compatible with Bevy ECS v0.20+)  
**Canonical Repository:** `bevy_abi_mod`

---

## Table of Contents
1. [Architectural Overview for Hosts](#1-architectural-overview-for-hosts)
   - [1.1 The Rust ABI Instability Barrier in Native Modding](#11-the-rust-abi-instability-barrier-in-native-modding)
   - [1.2 The `bevy_abi_mod` Asymmetric Architecture](#12-the-bevy_abi_mod-asymmetric-architecture)
   - [1.3 Hardware-Speed Zero-Serialization vs. IPC/Wasm/JSON](#13-hardware-speed-zero-serialization-vs-ipcwasmjson)
   - [1.4 Host-Side Architectural Components](#14-host-side-architectural-components)
2. [Quickstart & Host Setup](#2-quickstart--host-setup)
   - [2.1 Cargo Dependencies](#21-cargo-dependencies)
   - [2.2 Adding `AbiModPlugin`](#22-adding-abimodplugin)
   - [2.3 Registering Exposable Host Components](#23-registering-exposable-host-components)
   - [2.4 Structuring Host Components with `#[derive(ModExposable)]`](#24-structuring-host-components-with-derivemodexposable)
     - [2.4.1 Primitive Fields](#241-primitive-fields)
     - [2.4.2 Nested Compound Structs & Interior Pointers](#242-nested-compound-structs--interior-pointers)
     - [2.4.3 Heap-Allocated Fields (`String` and `Vec<T>`)](#243-heap-allocated-fields-string-and-vect)
     - [2.4.4 The Self-Aliasing Hazard & Defense](#244-the-self-aliasing-hazard--defense)
     - [2.4.5 Skipping Internal Fields](#245-skipping-internal-fields)
3. [Dynamic Schedule & Execution Pipeline](#3-dynamic-schedule--execution-pipeline)
   - [3.1 Bridging Foreign Callbacks into Bevy ECS (`DynamicCSystem`)](#31-bridging-foreign-callbacks-into-bevy-ecs-dynamiccsystem)
   - [3.2 Dynamic `FilteredAccessSet` Registration](#32-dynamic-filteredaccessset-registration)
   - [3.3 Multi-Threaded Concurrency Safety & Parallel Execution](#33-multi-threaded-concurrency-safety--parallel-execution)
   - [3.4 Intra-System Borrow Conflict Detection](#34-intra-system-borrow-conflict-detection)
   - [3.5 Steady-State Zero Allocations via Archetype Generation Tracking](#35-steady-state-zero-allocations-via-archetype-generation-tracking)
   - [3.6 Archetype Table Chunk Traversal (`HostChunkIter`)](#36-archetype-table-chunk-traversal-hostchunkiter)
   - [3.7 Automatic & Fine-Grained Change Detection (`Changed<T>`)](#37-automatic--fine-grained-change-detection-changedt)
   - [3.8 Deferred Command Execution & Allocator Isolation](#38-deferred-command-execution--allocator-isolation)
4. [Mod Lifecycle Management at Runtime](#4-mod-lifecycle-management-at-runtime)
   - [4.1 Dynamic Library Loading (`load_mod_tracked`)](#41-dynamic-library-loading-load_mod_tracked)
   - [4.2 Mod Identification & Tracking (`ModId` and `LoadedMod`)](#42-mod-identification--tracking-modid-and-loadedmod)
   - [4.3 Safe Unloading & The Atomic Tombstone Pattern](#43-safe-unloading--the-atomic-tombstone-pattern)
   - [4.4 The 4-Phase Shutdown Sequence](#44-the-4-phase-shutdown-sequence)
   - [4.5 Hot-Reloading Patterns Without OS File Locking](#45-hot-reloading-patterns-without-os-file-locking)
5. [Fault Tolerance & Panic Containment](#5-fault-tolerance--panic-containment)
   - [5.1 Cross-Runtime Unwinding Panic Hazards](#51-cross-runtime-unwinding-panic-hazards)
   - [5.2 Two-Tier Panic Containment & `FfiResult::PANIC_CAUGHT`](#52-two-tier-panic-containment--ffiresultpanic_caught)
   - [5.3 Automatic System Disabling & World Protection](#53-automatic-system-disabling--world-protection)
   - [5.4 Memory & Allocator Boundary Isolation (The Owner-Frees Rule)](#54-memory--allocator-boundary-isolation-the-owner-frees-rule)
6. [Complete, Working Code Examples](#6-complete-working-code-examples)
   - [6.1 Example 1: Minimal Host Application Setup](#61-example-1-minimal-host-application-setup)
   - [6.2 Example 2: Advanced Host Component Design & Change Detection](#62-example-2-advanced-host-component-design--change-detection)
   - [6.3 Example 3: Runtime Mod Manager Resource & Keyboard Hot-Reloading](#63-example-3-runtime-mod-manager-resource--keyboard-hot-reloading)
   - [6.4 Example 4: Multi-Query Systems & Deferred Command Verification](#64-example-4-multi-query-systems--deferred-command-verification)
7. [API Reference Summary](#7-api-reference-summary)

---

## 1. Architectural Overview for Hosts

### 1.1 The Rust ABI Instability Barrier in Native Modding
Traditional game engine plugin architectures written in Rust suffer from a fundamental limitation: **Rust has no stable Application Binary Interface (ABI)**.

When attempting dynamic linking using standard Rust dynamic libraries (`dylib`), the host executable and the guest dynamic library must share:
- Exactly identical `rustc` compiler toolchains (even minor patch releases or nightly builds may reorder struct fields, shift enum discriminate values, or alter vtable layouts).
- Exactly identical Cargo feature flags, codegen units (`codegen-units`), optimization flags (`opt-level=2` vs `opt-level=3`), and panic strategies (`unwind` vs `abort`).
- A single unified global memory allocator. If a host standard library frees memory allocated by a guest standard library, the process immediately aborts (`malloc: pointer being freed was not allocated`).

In practical terms: if community modders compile a mod using their own local Rust installation, loading that mod into a player's game via `dlopen` triggers silent memory corruption or instantaneous segmentation faults (`SIGSEGV`). Furthermore, whenever the game developer updates Bevy by a single patch release, every published mod in existence is rendered permanently broken.

### 1.2 The `bevy_abi_mod` Asymmetric Architecture
`bevy_abi_mod` eliminates the Rust ABI instability barrier by establishing an **asymmetric, zero-serialization C-ABI boundary**:
- **Host Engine (Bevy ECS v0.20+):** The host game runs the full Bevy engine, managing entity archetypes, multi-threaded schedules, rendering, assets, and physics.
- **Guest Mods (`.cdylib` / `.so` / `.dylib` / `.dll`):** Mods can be compiled with **any Rust compiler version**, ISO C99, C++17, or Zig with **zero Bevy dependencies**.
- **C-ABI Contract (`abi_contract`, `#![no_std]`):** The sole shared interface consists of primitive integers, opaque handles, explicit `#[repr(C)]` layouts, and `extern "C"` function pointers.

```text
┌────────────────────────────────────────────────────────────────────────┐
│                        HOST GAME (Bevy Engine)                         │
│                                                                        │
│   ┌────────────────────┐               ┌───────────────────────────┐   │
│   │    ModRegistry     │               │       AbiModPlugin        │   │
│   │  (Symbol Catalog & │               │  (ModHost / libloading /  │   │
│   │      VTables)      │               │   Schedule Registration)  │   │
│   └─────────┬──────────┘               └─────────────┬─────────────┘   │
│             │                                        │                 │
│             ▼                                        ▼                 │
│   ┌─────────────────────────────────────────────────────────────┐      │
│   │                 DynamicCSystem (impl System)                │      │
│   │  • FilteredAccessSet<ComponentId> (Parallel Borrow Checker) │      │
│   │  • HostChunkIter (Archetype Table Column Pointers)          │      │
│   │  • CommandQueueBridge (Deferred Allocator Isolation)        │      │
│   └──────────────────────────────┬──────────────────────────────┘      │
└──────────────────────────────────┼─────────────────────────────────────┘
                                   │
              ═════════════════════╪═════════════════════
              STABLE C-ABI BOUNDARY│ (abi_contract)
              ═════════════════════╪═════════════════════
                                   │
┌──────────────────────────────────┴─────────────────────────────────────┐
│                      GUEST MODS (Shared Libraries)                     │
│                                                                        │
│  ┌───────────────────────────────┐   ┌───────────────────────────────┐ │
│  │    Rust Mod (guest_sdk)       │   │     C99 Mod (bevy_abi_mod.h)  │ │
│  │  • Independent rustc compiler │   │   • Standard ISO C99          │ │
│  │  • catch_guest_panic wrapper  │   │   • Clang / GCC / Zig cc      │ │
│  │  • Zero Bevy dependencies     │   │   • Zero runtime dependencies │ │
│  └───────────────────────────────┘   └───────────────────────────────┘ │
└────────────────────────────────────────────────────────────────────────┘
```

### 1.3 Hardware-Speed Zero-Serialization vs. IPC/Wasm/JSON
Traditional decoupling strategies impose severe performance penalties:
- **Inter-Process Communication (IPC / JSON / Bincode):** Serializing 10,000 entities across sockets or pipes requires 10–25 milliseconds per frame. At 60 FPS (a 16.6ms total frame budget), serialization alone causes dropped frames.
- **WebAssembly (Wasm):** While memory-safe, Wasm linear memory requires copying ECS buffers across the VM boundary every tick, adding memory overhead and JIT compilation penalties.
- **`bevy_abi_mod` Hardware-Speed Columnar Access:**
  - **Zero Serialization:** The host hands foreign callbacks raw pointers directly into Bevy's dense, contiguous archetype `Table` columns (Structure-of-Arrays layout).
  - **Native Cache Prefetching:** Guest systems iterate through components using simple CPU pointer arithmetic (`ptr.add(row * stride)`), running at native L1/L2 cache line speeds (~20M entity evaluations per second).

### 1.4 Host-Side Architectural Components
1. **`ModRegistry`**: Central host catalog that maps Bevy dynamic `ComponentId`s to deterministic 128-bit `ComponentUuid`s, names, static `#[repr(C)]` VTables, and unaligned byte deserializers.
2. **`ModHost`**: ECS resource managing loaded dynamic library handles (`libloading::Library`), unique generational `ModId`s, and atomic tombstone flags.
3. **`DynamicCSystem`**: Implements Bevy's native `bevy_ecs::system::System` trait. Registers foreign query access sets with Bevy's borrow checker, drives chunk traversal, and contains foreign panics.
4. **`HostChunkIter`**: Contiguous columnar chunk iterator that wraps Bevy's archetype tables into C-ABI function pointers (`iter_next_chunk`, `iter_column_mutable`, `iter_column_readonly`).
5. **`CommandQueueBridge`**: Bridges deferred structural mutations (`spawn_empty`, `despawn`, `insert_component`), maintaining strict allocator isolation.

---

## 2. Quickstart & Host Setup

### 2.1 Cargo Dependencies
In your Bevy game project's `Cargo.toml`:

```toml
[dependencies]
bevy = "0.20" # or bevy_app, bevy_ecs (v0.20+)
host_runtime = { path = "path/to/bevy_abi_mod/crates/host_runtime" }
mod_derive = { path = "path/to/bevy_abi_mod/crates/mod_derive" }
```

### 2.2 Adding `AbiModPlugin` & Configuring `ModErrorPolicy`
Initialize the modding subsystem in your Bevy `App` by adding `AbiModPlugin`:

```rust
use bevy::prelude::*;
use host_runtime::prelude::*;

fn main() {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins);

    // Add the ABI modding plugin (default policy: ModErrorPolicy::Quarantine;
    // also supports ModErrorPolicy::Log and ModErrorPolicy::ReportToBevy)
    app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));
}
```

When added, `AbiModPlugin`:
1. Ensures `ModRegistry` and `ModHost` are inserted into the ECS world resources and registers `ModLoaded`, `ModUnloaded`, and `ModSystemFaulted` messages/events.
2. Registers built-in engine bridge function symbols:
   - `cmd_spawn_empty`: Immediate entity ID allocation (`FfiEntity::NULL = 0` on error) with deferred spawn.
   - `cmd_despawn`: Deferred recursive entity despawning.
   - `cmd_insert_component`: Deferred raw-byte component insertion (`IS_RAW_INSERTABLE` enforced).
   - `iter_reset`: $O(1)$ query cursor rewinding for nested iteration.
   - `register_scheduled_system`: Single-query registration targeting `Startup`/`PreUpdate`/`Update`/`PostUpdate`/`FixedUpdate`/`Last` with `.after()`/`.before()` ordering.
   - `register_multi_query_system`: Multi-query dynamic system registration (up to 8 queries).
   - `register_component`: Runtime dynamic component registration (`ModComponentDescriptor`).

### 2.3 Registering Exposable Host Components (`Table` & `SparseSet`)
Host components intended for mod reading or writing (using either default `StorageType::Table` or `#[component(storage = "SparseSet")]`) must be registered with the Bevy `App`:

```rust
// Registers ECS component (Table or SparseSet), assigns ComponentId, and records VTable
app.register_mod_component::<Position>();
app.register_mod_component::<Velocity>();

// Force VTable-only opaque exposure (forbids raw byte cmd_insert_component even on Copy structs):
// app.register_mod_component_opaque::<AuthState>();

// If you have a nested struct or shared data type (not an ECS component):
app.register_mod_type::<Vec3f>();
```

Under the hood, `register_mod_component::<T>()`:
1. Calls `app.world_mut().register_component::<T>()` to obtain Bevy's runtime `ComponentId` and `StorageType`.
2. Registers `T` in `ModRegistry`, recording its 128-bit `ComponentUuid`, byte size, byte alignment, `T::IS_RAW_INSERTABLE`, and static VTable pointer.
3. Exposes the VTable symbol `"<ComponentName>_vtable"` to the handshake symbol resolver `host_get_proc_address`.

### 2.4 Structuring Host Components with `#[derive(ModExposable)]` & `#[derive(AbiPod)]`
Components can either use `#[repr(C)]` + `Copy` (implementing both `AbiPod` for zero-copy typed guest slices and `ModExposable` for VTables) or stay standard `#[repr(Rust)]` structs with `String`/`Vec`/`bool` fields (implementing `ExposableComponent` VTables without `AbiPod`):

```rust
use bevy_ecs::component::Component;
use host_runtime::ModExposable;

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[mod_component(name = "game::Position", prefix = "Pos")]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}
```

Supported container attributes on `#[mod_component(...)]`:
- `name = "..."`, `prefix = "..."`, `export_symbols`
- `no_pod`: Suppresses automatic `AbiPod` implementation even on a `#[repr(C)] + Copy` struct.
- `opaque`: Marks `IS_POD = false` and `IS_RAW_INSERTABLE = false`, allowing nested `ExposableComponent` fields via pointer accessors while forbidding raw-byte overwrites.

#### 2.4.1 Primitive & Boolean (`FfiBool`) Fields
Numeric primitives (`f32`, `f64`, `i8`..`i128`, `u8`..`u128`, `usize`, `isize`, `FfiEntity`) generate direct getter/setter pairs:
- `<Struct>_get_<field>(ptr: *const c_void) -> <Type>`
- `<Struct>_set_<field>(ptr: *mut c_void, val: <Type>)`

Boolean (`bool`) fields automatically bridge over the C-ABI as `FfiBool` (`u8`: `0` = false, non-zero = true) so foreign byte values `2..=255` can never construct an invalid Rust `bool`:
- `<Struct>_get_<field>(ptr: *const c_void) -> u8`
- `<Struct>_set_<field>(ptr: *mut c_void, val: u8)`

#### 2.4.2 Nested Compound Structs & Interior Pointers (`#[derive(AbiPod)]`)
For compound sub-structs, derive `AbiPod` (which enforces `#[repr(C)]` / `#[repr(transparent)]`, non-packed layout, `Copy`, and `!needs_drop` at compile time) and tag the field with `#[mod_field(nested)]`:

```rust
use mod_derive::AbiPod;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, AbiPod)]
pub struct Vec3f {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Component, ModExposable)]
pub struct Transform {
    #[mod_field(nested)]
    pub translation: Vec3f,
}
```
This generates interior pointer accessors:
- `<Struct>_ptr_<field>(ptr: *const c_void) -> *const c_void`
- `<Struct>_ptr_mut_<field>(ptr: *mut c_void) -> *mut c_void`

#### 2.4.3 Heap-Allocated Fields (`String` and `Vec<T: AbiPod>`)
Heap collections owned by the host allocator must **never** be passed as raw owned types to guest allocators (and automatically set `IS_POD = false` and `IS_RAW_INSERTABLE = false`):
- **For `String` (`#[mod_field(string)]`):**
  - Getter returns borrowed `FfiSlice<u8>` (`StrView { ptr: *const u8, len: usize }`).
  - Setter takes `(*const u8, usize)`, validates UTF-8, and mutates the host `String` using the host allocator.
- **For `Vec<T: AbiPod>` (`#[mod_field(vec)]`):**
  - Getter returns borrowed `FfiSlice<T> { ptr: *const T, len: usize }`.
  - Setter takes `(*const T, usize)` and copies elements using the host allocator.

#### 2.4.4 The Self-Aliasing Hazard & Defense
Consider what happens if guest code passes a slice borrowed from the component's own buffer back into its setter:
```rust
// Guest mod logic:
tag.set_label(tag.get_label());
```
If the host setter naively called:
```rust
field.clear(); // Truncates or frees internal buffer!
field.push_str(incoming); // Reads from incoming (now a dangling pointer!)
```
This causes undefined behavior (use-after-free).

`mod_derive` automatically generates interval overlap detection:
$$\text{overlaps} = (src\_start < buf\_end) \land (buf\_start < src\_end)$$

- **If disjoint ($overlaps = \text{false}$):** Mutates in-place with zero extra allocations.
- **If overlapping ($overlaps = \text{true}$):** Automatically clones the incoming data into a temporary host buffer first before updating the field, guaranteeing 100% memory safety.

#### 2.4.5 Skipping Internal Fields
Fields that are private to the host, contain closures, raw pointers, or unexposed complex types (`Option<T>`, `HashMap<K, V>`, `Box<T>`) must be tagged with `#[mod_field(skip)]`:

```rust
#[derive(Component, ModExposable)]
pub struct PlayerState {
    pub score: u32,

    #[mod_field(skip)]
    pub session_token: Option<String>, // Hidden from mod C-ABI VTable
}
```

---

## 3. Dynamic Schedule & Execution Pipeline

### 3.1 Bridging Foreign Callbacks into Bevy ECS (`DynamicCSystem`)
Bevy's ECS scheduler executes objects implementing `bevy_ecs::system::System`. When a guest mod calls `register_system`, the host instantiates a `DynamicCSystem`. This wraps the foreign function pointer (`DynamicSystemCallback` or `MultiQuerySystemCallback`) and seamlessly adapts it into the Bevy schedule graph.

### 3.2 Dynamic `FilteredAccessSet` Registration
During system registration, the mod specifies its component access requirements via `ComponentAccessDesc`:
- `COMPONENT_ACCESS_READ (0)`: Read-only access (`&T`).
- `COMPONENT_ACCESS_WRITE (1)`: Mutable exclusive access (`&mut T`).
- `COMPONENT_ACCESS_FILTER_ONLY (2)`: Archetype presence filter (`With<T>`).
- `COMPONENT_ACCESS_WITHOUT (3)`: Archetype absence filter (`Without<T>`).

When Bevy initializes the system (`DynamicCSystem::initialize`), the host translates these declarations into Bevy's native `FilteredAccess<ComponentId>`:
- `READ` adds a component read to `FilteredAccess`.
- `WRITE` adds a component write to `FilteredAccess`.
- `FILTER_ONLY` calls `filtered_access.and_with(comp_id)`.
- `WITHOUT` calls `filtered_access.and_without(comp_id)`.

The resulting `SystemAccess::Shared(access_set)` is returned to Bevy's scheduler.

### 3.3 Multi-Threaded Concurrency Safety & Parallel Execution
Because accesses are declared through Bevy's native access set:
1. **Parallel Execution:** If Mod A writes `Position` and Mod B writes `Velocity`, Bevy detects their access sets are disjoint and executes both systems **simultaneously in parallel across CPU worker threads**.
2. **Concurrent Reads:** Multiple mod systems reading `Position` run concurrently without lock contention.
3. **Data Race Prevention:** If Mod A writes `Position` and Mod B reads `Position`, Bevy automatically sequences them sequentially, eliminating data races without locks or mutexes.

### 3.4 Intra-System Borrow Conflict Detection
A single system cannot hold conflicting borrows (for example, Query 0 requesting `WRITE Position` while Query 1 in the same system requests `READ Position`).

During `initialize`, `DynamicCSystem` verifies that all query groups within the system are mutually compatible using `access_set.is_compatible(&candidate_set)`. If an intra-system conflict is detected:
```rust
self.self_conflict = true;
self.enabled = false;
```
The conflicting system is quarantined safely at startup before it can corrupt memory or trigger data races.

### 3.5 Steady-State Zero Allocations via Archetype Generation Tracking
Scanning Bevy's archetype tables on every frame introduces CPU cache misses and heap allocations. `DynamicCSystem` solves this by tracking `world.archetypes().generation()`:
- When entities are spawned or despawned with new component combinations, the archetype generation increments.
- If the generation has not changed since the last tick, `DynamicCSystem` reuses its precomputed `matched_tables: Vec<TableId>` list directly.
- During steady-state frame execution, dynamic systems execute with **zero heap allocations on the hot path**.

### 3.6 Archetype Table Chunk Traversal (`HostChunkIter`)
Bevy stores entities in dense contiguous columnar arrays within `Table`s. During execution:
1. `HostChunkIter` iterates through matched tables, skipping empty tables (`entity_count == 0`).
2. Foreign code drives traversal via `HostApi` function pointers:
   - `iter_next_chunk(iter)`: Advances to the next matching table chunk. Returns entity count (or `0` when finished).
   - `iter_column_readonly(iter, index) -> *const u8`: Returns pointer to column start. Returns `NULL` if accessed with `FILTER_ONLY` or `WITHOUT`.
   - `iter_column_mutable(iter, index) -> *mut u8`: Returns mutable pointer to column start. Strictly enforces `mode == WRITE`; returns `NULL` for read-only columns.
   - `iter_column_stride(iter, index) -> usize`: Returns component byte stride (`sizeof(T)`).
   - `iter_entities(iter) -> *const FfiEntity`: Returns pointer to contiguous entity IDs.
   - `iter_reset(iter)`: Rewinds chunk cursor back to table index 0 for nested $O(N \times M)$ query systems without allocations.

### 3.7 Automatic & Fine-Grained Change Detection (`Changed<T>`)
Bevy's change detection allows systems to query only mutated components (`Query<&Position, Changed<Position>>`).

`bevy_abi_mod` seamlessly integrates with Bevy's change detection:
1. **Automatic Chunk-Level Notification:** When a mod calls `iter_column_mutable(iter, index)`, the host runtime automatically updates the column's `changed_ticks` and `summary_tick` to `world.change_tick()`.
2. **Fine-Grained Row-Level Notification:** Mods can also call `iter_mark_changed(iter, index, row)` to flag only specific mutated rows.
3. Native host Bevy systems using `Changed<T>` detect these mutations immediately on the subsequent system run!

### 3.8 Deferred Command Execution & Allocator Isolation
Dynamic mod systems cannot mutate archetype topology immediately during query iteration (that would invalidate column pointers).

The host provides a `RawCommandQueue*` handle backed by `CommandQueueBridge`:
- `cmd_spawn_empty(queue)`: Invokes Bevy's atomic `EntityAllocator` to **immediately** generate a valid 64-bit `Entity` ID, while queueing a deferred `world.spawn_empty_at(entity)` command. The mod can immediately attach components to this entity in the same frame!
- `cmd_despawn(queue, entity)`: Queues deferred entity despawning.
- `cmd_insert_component(queue, entity, name, data, len)`: Copies `len` bytes from guest pointer `data` into a host-allocated `Vec<u8>`.

When Bevy runs `apply_deferred`, all queued operations are flushed safely into the `World`.

---

## 4. Mod Lifecycle Management at Runtime

### 4.1 Dynamic Library Loading (`try_load_mod` & `ModLoadError`)
Host games load dynamic mods using the `ModAppExt` trait on `App`:

```rust
// Returns Result<ModId, ModLoadError> with structured diagnostics
let mod_id: ModId = app.try_load_mod("mods/kinematics.dylib")
    .expect("Failed to load mod");

// Or validate component names + borrow conflicts strictly before schedule insertion:
// let mod_id = app.try_load_mod_strict("mods/kinematics.dylib")?;
// Or route default systems into a specific Bevy schedule (Startup / FixedUpdate / PostUpdate):
// let mod_id = app.try_load_mod_into_schedule("mods/init_mod.dylib", ModSchedule::Startup)?;
```

The Loading Sequence:
1. **Dynamic Loading & Per-Inode Locking:** `libloading::Library::new(path)` loads the shared library under a per-inode `SharedLibState` reference-counted lock.
2. **SemVer ABI Version Check:** Calls `bevy_abi_mod_abi_version() -> u32` if exported, verifying `is_abi_version_compatible(ABI_VERSION_PACKED, guest_version)`.
3. **Symbol Handshake:** Resolves mandatory `bevy_abi_mod_init` and optional `bevy_abi_mod_shutdown`.
4. **Context & Guard Creation:** Instantiates `is_active = Arc::new(AtomicBool::new(true))` and `ModInitContext`.
5. **Initialization Hook:** Invokes `bevy_abi_mod_init(host_get_proc_address, &HOST_API, &mut init_ctx)`.
6. **Schedule Injection, `ModSystemSet` & ECS Events:** Injects registered `DynamicCSystem` instances into their target schedules tagged with `ModSystemSet::All`, `ModSystemSet::Mod(mod_id)`, and `ModNamedSystemSet(name)`, spawns a queryable `LoadedModInfo` entity, and emits `ModLoaded`.

### 4.2 Mod Identification, ECS Entities & Lifecycle Events
- `ModId(pub u32)`: Unique identifier assigned to each loaded mod instance.
- `LoadedModInfo`: ECS `Component` spawned for each active mod so Bevy UI/systems can query `Query<&LoadedModInfo>` directly.
- `ModLoaded`, `ModUnloaded`, and `ModSystemFaulted`: Bevy `Event` + `Message` types emitted for observers (`app.add_observer(...)`) and `MessageReader<...>`.

### 4.3 Safe Unloading: Atomic Tombstones + `ModSystemSet` Schedule Excision
Dropping `libloading::Library` issues an OS-level `dlclose()`, unmapping the shared library's `.text` segment. `bevy_abi_mod` combines two complementary mechanisms on `app.try_unload_mod(mod_id)`:
1. **Schedule Graph Excision (`remove_systems_in_set`):** Removes all systems belonging to `ModSystemSet::Mod(mod_id)` from tracked Bevy schedules (`ScheduleCleanupPolicy::RemoveSystemsOnly`), preventing schedule node accumulation across thousands of hot-reload cycles.
2. **Atomic Tombstone & Drop-Trampoline Deactivation:** Stores `is_active.store(false, Ordering::Release)` and updates the per-`(WorldId, ComponentUuid)` atomic drop-trampoline pool (`MAX_DROP_TRAMPOLINES = 1024`), guaranteeing that any surviving system reference or unmapped foreign `ComponentDropFn` becomes an immediate safe no-op.

### 4.4 The Shutdown Sequence
`ModHost::try_unload_mod` (and `unload_all` in LIFO reverse load order) executes in strict order:
- **Phase 1: Tombstone & Drop-Slot Deactivation:** `is_active.store(false, Ordering::Release)` and per-`(WorldId, ComponentUuid)` drop-trampoline snapshot update.
- **Phase 2: Schedule Excision & ECS Cleanup:** Excises `ModSystemSet::Mod(mod_id)` from schedules, despawns `LoadedModInfo`, and emits `ModUnloaded`.
- **Phase 3: Guest Teardown Hook:** When the per-inode `SharedLibState` active count transitions `1 -> 0`, calls `bevy_abi_mod_shutdown` **while `.text` is still mapped** so guest closures and heap allocations are freed cleanly.
- **Phase 4: Dynamic Unmapping:** Drops `libloading::Library` (`dlclose` / `FreeLibrary`).

### 4.5 Hot-Reloading Patterns Without OS File Locking
To support seamless runtime hot-reloading without Windows `.dll` locks or Unix `ETXTBSY`:
1. Copy the newly compiled `.dylib` / `.so` / `.dll` to a versioned shadow path (e.g. `.cache/mods/kinematics_v2.dylib`).
2. Unload the previous instance: `app.try_unload_mod(current_mod_id)?`.
3. Load the shadow-copied instance: `app.try_load_mod(shadow_path)?`.

---

## 5. Fault Tolerance, `ModErrorPolicy` & Panic Containment

### 5.1 Cross-Runtime Unwinding Panic Hazards
Standalone `.cdylib` mods link an isolated Rust `std` runtime; unwinding across `extern "C"` into the host is undefined behavior.

### 5.2 Two-Tier Panic Containment & `FfiResult::PANIC_CAUGHT`
- **Tier 1 (Guest-Side Containment):** `guest_sdk` (`export_mod!`, `GuestApp::add_system`, `wrap_system_callback!`) wraps guest callbacks in `std::panic::catch_unwind` inside the guest runtime and returns `FfiResult::PANIC_CAUGHT`.
- **Tier 2 (Host-Side Defense):** `DynamicCSystem::run_unsafe` also guards callbacks with `catch_unwind`.

### 5.3 Configurable `ModErrorPolicy` & `ModSystemFaulted`
When a system returns an error `FfiResult` or `PANIC_CAUGHT`, `DynamicCSystem` emits a `ModSystemFaulted` event/message and applies the configured `ModErrorPolicy`:
- **`ModErrorPolicy::Quarantine` (default):** Logs the fault and sets `self.enabled = false` so healthy host and mod systems continue uninterrupted.
- **`ModErrorPolicy::Log`:** Logs the fault and emits `ModSystemFaulted` without disabling future runs.
- **`ModErrorPolicy::ReportToBevy`:** Propagates `Err(BevyError)` into Bevy's native schedule error handler.

### 5.4 Memory & Allocator Boundary Isolation (The Owner-Frees Rule)
- **The Golden Rule:** Memory allocated by the host is freed by the host; memory allocated by the guest is freed by the guest.
- `cmd_insert_component` copies bytes into a host-owned aligned buffer and rejects non-raw-insertable (`String`, `Vec`, `bool`, `opaque`) host components with `FfiResult::PERMISSION_DENIED`.

---

## 6. Complete, Working Code Examples

### 6.1 Example 1: Minimal Host Application Setup
A self-contained Bevy host app demonstrating component registration, mod loading, and change detection monitoring:

```rust
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::query::Changed;
use bevy_ecs::system::Query;
use host_runtime::prelude::*;

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

fn monitor_motion(query: Query<&Position, Changed<Position>>) {
    for pos in query.iter() {
        println!("Entity moved by mod to: ({:.1}, {:.1}, {:.1})", pos.x, pos.y, pos.z);
    }
}

fn main() {
    let mut app = App::new();

    // 1. Initialize modding framework
    app.add_plugins(AbiModPlugin::default());

    // 2. Register exposable components
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    // 3. Add host monitoring system
    app.add_systems(Update, monitor_motion);

    // 4. Spawn sample entities
    for i in 0..5 {
        app.world_mut().spawn((
            Position { x: i as f32 * 10.0, y: 0.0, z: 0.0 },
            Velocity { x: 0.0, y: 1.0, z: 0.0 },
        ));
    }

    // 5. Load dynamic mod shared library
    let mod_path = if cfg!(target_os = "windows") {
        "mods/kinematics.dll"
    } else if cfg!(target_os = "macos") {
        "mods/libkinematics.dylib"
    } else {
        "mods/libkinematics.so"
    };

    let mod_id = app.load_mod_tracked(mod_path).expect("Failed to load mod");
    println!("Successfully loaded mod with ID: {:?}", mod_id);

    // 6. Run 10 ticks
    for tick in 1..=10 {
        println!("--- Tick {} ---", tick);
        app.update();
    }
}
```

### 6.2 Example 2: Advanced Host Component Design & Change Detection
Illustrating primitives, host strings with self-aliasing safety, nested `AbiPod` sub-structs, and skipped fields:

```rust
use bevy_app::App;
use bevy_ecs::component::Component;
use host_runtime::prelude::*;
use mod_derive::AbiPod;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, AbiPod)]
pub struct Vec3f {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Component, Debug, Clone, PartialEq, ModExposable)]
#[mod_component(name = "game::Character")]
pub struct Character {
    pub health: i32,
    pub max_health: i32,

    #[mod_field(string)]
    pub name: String,

    #[mod_field(nested)]
    pub position: Vec3f,

    #[mod_field(skip)]
    pub auth_token: u64, // Host-private field hidden from mods
}

pub fn setup_character_host(app: &mut App) {
    app.add_plugins(AbiModPlugin::default());
    app.register_mod_component::<Character>();
    app.register_mod_type::<Vec3f>();
}
```

### 6.3 Example 3: Runtime Mod Manager Resource & Keyboard Hot-Reloading
Illustrates an interactive host resource managing multiple mods, handling shadow-copy reloading without file locking:

```rust
use bevy_app::App;
use bevy_ecs::prelude::Resource;
use host_runtime::prelude::*;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Resource, Default)]
pub struct ModManager {
    loaded_mods: HashMap<String, ModId>,
    generation: u32,
}

impl ModManager {
    pub fn load_mod(&mut self, app: &mut App, name: &str, original_path: impl AsRef<Path>) {
        let original_path = original_path.as_ref();
        self.generation += 1;

        // Shadow copy to prevent OS dynamic library file locking
        let tmp_dir = std::env::temp_dir().join("bevy_abi_mod_cache");
        fs::create_dir_all(&tmp_dir).unwrap();
        let ext = original_path.extension().unwrap_or_default().to_string_lossy();
        let shadow_path = tmp_dir.join(format!("{}_{}.{}", name, self.generation, ext));
        fs::copy(original_path, &shadow_path).expect("Failed to shadow-copy mod library");

        let mod_id = app.load_mod_tracked(&shadow_path).expect("Failed to load mod");
        self.loaded_mods.insert(name.to_string(), mod_id);
        println!("Loaded mod '{}' (ModId: {:?})", name, mod_id);
    }

    pub fn unload_mod(&mut self, app: &mut App, name: &str) {
        if let Some(mod_id) = self.loaded_mods.remove(name) {
            app.unload_mod(mod_id).expect("Failed to unload mod");
            println!("Unloaded mod '{}' (ModId: {:?})", name, mod_id);
        }
    }

    pub fn reload_mod(&mut self, app: &mut App, name: &str, original_path: impl AsRef<Path>) {
        self.unload_mod(app, name);
        self.load_mod(app, name, original_path);
    }
}
```

### 6.4 Example 4: Multi-Query Systems & Deferred Command Verification
Demonstrates how the host inspects deferred entities spawned by mods via `cmd_spawn_empty` and `cmd_insert_component`:

```rust
use bevy_app::App;
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::system::Query;
use host_runtime::prelude::*;

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct MinionTag {
    pub wave_id: u32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

pub fn inspect_minions(query: Query<(Entity, &MinionTag, &Position)>) {
    for (entity, tag, pos) in query.iter() {
        println!(
            "Host sees minion {:?} (wave {}) at ({:.1}, {:.1})",
            entity, tag.wave_id, pos.x, pos.y
        );
    }
}
```

---

## 7. API Reference Summary

| Trait / Type | Location | Method / Item | Description |
|---|---|---|---|
| **`AbiModPlugin`** | `host_runtime` | `AbiModPlugin::with_error_policy(...)` | Initializes ECS mod subsystem with `ModErrorPolicy`. |
| **`ModErrorPolicy`** | `host_runtime` | `Quarantine` / `Log` / `ReportToBevy` | Controls runtime fault handling for dynamic mod systems. |
| **`ModLoadError`** | `host_runtime` | Structured load/unload error enum | Typed diagnostics (`DlOpen`, `IncompatibleAbiVersion`, `InitFailed`, etc.). |
| **`ModAppExt`** | `host_runtime` | `app.register_mod_component::<T>()` | Registers `Table` or `SparseSet` component, assigns `ComponentId`, exposes VTable. |
| **`ModAppExt`** | `host_runtime` | `app.register_mod_component_opaque::<T>()` | Registers component for VTable access while forbidding raw-byte insertion. |
| **`ModAppExt`** | `host_runtime` | `app.register_mod_type::<T>()` | Registers helper type VTable without assigning ECS `ComponentId`. |
| **`ModAppExt`** | `host_runtime` | `app.try_load_mod(path)` | Loads mod returning `Result<ModId, ModLoadError>`. |
| **`ModAppExt`** | `host_runtime` | `app.try_load_mod_strict(path)` | Pre-validates component names and borrow conflicts before inserting into schedule. |
| **`ModAppExt`** | `host_runtime` | `app.try_load_mod_into_schedule(path, sched)` | Loads mod routing default systems into `ModSchedule` (`Startup`, `FixedUpdate`, etc.). |
| **`ModAppExt`** | `host_runtime` | `app.try_unload_mod(mod_id)` | Excises `ModSystemSet::Mod(mod_id)`, flips tombstones, runs shutdown, and `dlclose`s. |
| **`ModSystemSet`** | `host_runtime` | `ModSystemSet::{All, Mod(ModId)}` | Bevy `SystemSet`s for ordering against or excising mod systems. |
| **`ModLoaded` / `ModUnloaded` / `ModSystemFaulted`** | `host_runtime` | ECS `Event` + `Message` | Lifecycle and fault telemetry observable via observers or `MessageReader`. |
| **`ModExposable` / `AbiPod`** | `mod_derive` | `#[derive(ModExposable, AbiPod)]` | Generates `#[repr(C)]` VTables and compile-time verified POD layout markers. |
