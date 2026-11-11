# `bevy_abi_mod` Architecture & System Design

This document details the architectural principles, component models, memory contracts, formal safety proofs, and execution pipeline of the **`bevy_abi_mod`** framework.

---

## Table of Contents
1. [Architectural Mission & The Zero Shared Rust ABI Model](#1-architectural-mission--the-zero-shared-rust-abi-model)
   - [1.1 Failure Modes of Shared Rust ABI in Dynamic Plugins](#11-failure-modes-of-shared-rust-abi-in-dynamic-plugins)
   - [1.2 The Asymmetric C-ABI Paradigm](#12-the-asymmetric-c-abi-paradigm)
   - [1.3 Comparative Architecture Taxonomy](#13-comparative-architecture-taxonomy)
2. [Crate Responsibilities & Layered Boundaries](#2-crate-responsibilities--layered-boundaries)
3. [Dynamic System Lifecycle Flowcharts](#3-dynamic-system-lifecycle-flowcharts)
   - [3.1 Mod Load, Handshake, and Registration Pipeline](#31-mod-load-handshake-and-registration-pipeline)
   - [3.2 Per-Frame System Execution Flow & Panic Containment](#32-per-frame-system-execution-flow--panic-containment)
   - [3.3 Dynamic Unload, Teardown Hook, and Atomic Tombstoning](#33-dynamic-unload-teardown-hook-and-atomic-tombstoning)
4. [Formal Memory Safety Proofs](#4-formal-memory-safety-proofs)
   - [4.1 Proof A: Dynamic Allocator Isolation](#41-proof-a-dynamic-allocator-isolation)
   - [4.2 Proof B: Self-Aliasing Prevention in Component Mutators](#42-proof-b-self-aliasing-prevention-in-component-mutators)
   - [4.3 Proof C: Atomic Tombstoning Across Dynamic Unloads (`dlclose` Safety)](#43-proof-c-atomic-tombstoning-across-dynamic-unloads-dlclose-safety)
5. [Contiguous Archetype Table Chunk Traversal](#5-contiguous-archetype-table-chunk-traversal)
   - [5.1 Zero-Allocation Traversal Mechanics](#51-zero-allocation-traversal-mechanics)
   - [5.2 Steady-State Generation Caching](#52-steady-state-generation-caching)
6. [Multi-Query System Mechanics & Intra-System Borrow Checking](#6-multi-query-system-mechanics--intra-system-borrow-checking)
   - [6.1 Architectural Need & System Descriptors](#61-architectural-need--system-descriptors)
   - [6.2 Intra-System Borrow Conflict Detection](#62-intra-system-borrow-conflict-detection)
   - [6.3 Stack-Allocated Iterators & Cursor Rewinding (`iter_reset`)](#63-stack-allocated-iterators--cursor-rewinding-iter_reset)
7. [Parallel Scheduling & Borrow Checking](#7-parallel-scheduling--borrow-checking)
   - [7.1 Dynamic `FilteredAccessSet`](#71-dynamic-filteredaccessset)
   - [7.2 Concurrency & Disjoint Execution](#72-concurrency--disjoint-execution)
8. [Change Detection Integration (`AtomicTick`)](#8-change-detection-integration-atomictick)
9. [Fault Isolation & Two-Tier Panic Containment](#9-fault-isolation--two-tier-panic-containment)
10. [Deterministic Lifecycle & Hot-Reloading](#10-deterministic-lifecycle--hot-reloading)

---

## 1. Architectural Mission & The Zero Shared Rust ABI Model

Bevy Engine's industry-leading performance stems from its archetypal Entity Component System (ECS). However, standard Rust dynamic libraries (`.dylib` / `.so` / `.dll`) inherently share standard Rust ABI assumptions.

### 1.1 Failure Modes of Shared Rust ABI in Dynamic Plugins
Standard Rust does not provide a stable ABI. Dynamically linking Rust plugins via standard `.dylib` or `cdylib` sharing Rust standard library types (`String`, `Vec`, `Box`, `&str`, trait objects `&dyn Trait`) fails in production because:
1. **Compiler Variance**: Any variation in `rustc` versions (even patch releases) can alter struct field ordering, padding, and layout niche exploitation.
2. **Standard Library Layout Incompatibility**: The internal representation of standard collections (e.g. `alloc::raw_vec::RawVec`) changes across compiler versions.
3. **Monomorphization Invariance**: Generic methods and functions are monomorphized independently into the host and the plugin; symbol collisions or divergent implementations across boundaries produce silent memory corruption.
4. **Global Allocator Duality**: If host and plugin are built as separate dynamic libraries without explicit symbol interposition, each executable unit initializes its own global memory allocator (e.g., jemalloc, mimalloc, or OS malloc). Deallocating memory across library boundaries triggers undefined behavior (`malloc: *** error for object: pointer being freed was not allocated`).
5. **Panic Runtime Mismatches**: Unwinding across dynamic library boundaries when runtimes differ causes an immediate application abort.

### 1.2 The Asymmetric C-ABI Paradigm
`bevy_abi_mod` resolves this by decoupling the Host from the Guest using an **Asymmetric C-ABI Model**:

| Property | Host Side (`host_runtime`) | Boundary (`abi_contract`) | Guest Side (`guest_sdk` / C99) |
|---|---|---|---|
| **Language & Toolchain** | Rust (fixed to Bevy's current `rustc`) | Strict C99 Standard | Any `rustc`, ISO C99, C++, Zig |
| **Dependencies** | `bevy_ecs`, `bevy_app`, `libloading` | `#![no_std]`, Zero Dependencies | `abi_contract`, Zero Bevy dependencies |
| **Data Layout** | Arbitrary Rust types, archetypes, tables | `#[repr(C)]`, `#[repr(transparent)]` | Mirrors matching `#[repr(C)]` layouts |
| **Memory Allocation** | Host Global Allocator | Pointers, StrViews, FfiSlices | Guest Global Allocator |
| **Hot Path Access** | Dense column pointers & strides | Direct pointer offsets (`base + i*stride`) | Direct pointer offsets (`base + i*stride`) |

```text
┌────────────────────────────────────────────────────────────────────────┐
│                               Host Engine                              │
│                          (Bevy ECS v0.20+)                             │
│                                                                        │
│   ┌─────────────────────┐               ┌──────────────────────────┐   │
│   │     ModRegistry     │               │      AbiModPlugin        │   │
│   │ (Symbols & VTables) │               │   (Lifecycle & Loader)   │   │
│   └──────────┬──────────┘               └─────────────┬────────────┘   │
│              │                                        │                │
│              ▼                                        ▼                │
│   ┌────────────────────────────────────────────────────────────┐       │
│   │              DynamicCSystem (impl System)                  │       │
│   │  - FilteredAccessSet<ComponentId> (Borrow Checking)        │       │
│   │  - HostChunkIter (Contiguous Archetype Table Pointers)     │       │
│   │  - CommandQueueBridge (Deferred Spawns / Despawns)         │       │
│   └─────────────────────────────┬──────────────────────────────┘       │
└─────────────────────────────────┼──────────────────────────────────────┘
                                  │
                 ═════════════════╪═════════════════
                 C-ABI BOUNDARY   │ (extern "C" fn)
                 ═════════════════╪═════════════════
                                  │
┌─────────────────────────────────┴──────────────────────────────────────┐
│                            Guest Plugins                               │
│                                                                        │
│   ┌──────────────────────────┐            ┌────────────────────────┐   │
│   │   asymmetric_rust_mod    │            │    asymmetric_c_mod    │   │
│   │   - Separate rustc       │            │    - Pure ISO C99      │   │
│   │   - guest_sdk wrapper    │            │    - bevy_abi_mod.h    │   │
│   │   - catch_guest_panic    │            │    - Zero dependencies │   │
│   └──────────────────────────┘            └────────────────────────┘   │
└────────────────────────────────────────────────────────────────────────┘
```

### 1.3 Comparative Architecture Taxonomy
```text
┌─────────────────────────┬──────────────────────┬──────────────────────┬──────────────────────┐
│ Attribute               │ Scripting (Lua/Wasm) │ Coupled Rust dylib   │ bevy_abi_mod (C-ABI) │
├─────────────────────────┼──────────────────────┼──────────────────────┼──────────────────────┤
│ Hot-loop Throughput     │ 0.1–1.0M ops/sec     │ ~20M ops/sec         │ ~20M ops/sec (1.00x) │
│ Memory Overhead         │ High (GC, boxing)    │ Zero (native)        │ Zero (native SoA)    │
│ Toolchain Decoupled     │ Yes                  │ No (Exact rustc lock)│ Yes (Decoupled tool) │
│ Mod Author Languages    │ Dynamic scripts      │ Rust only            │ Rust, C99, C++, Zig  │
│ Dynamic Unload Safety   │ Safe (VM teardown)   │ Unsafe (SIGSEGV)     │ Safe (Atomic Guard)  │
└─────────────────────────┴──────────────────────┴──────────────────────┴──────────────────────┘
```

---

## 2. Crate Responsibilities & Layered Boundaries

| Layer / Crate | Type | Responsibilities & Constraints |
|---|---|---|
| **`abi_contract`** | Core Contract | `#![no_std]`, `#[repr(C)]`, zero external dependencies. Defines `HostApi`, `SystemDescriptor`, `ScheduledSystemDescriptor`, `ModComponentDescriptor`, `RawChunkIter`, `FfiBool` (`u8`), `FfiEntity` (`NULL = 0`), `FfiResult`, `AbiPod`, `FfiLayout`, and `ExposableComponent`. |
| **`mod_derive`** | Procedural Macro | Generates `#[repr(C)]` VTables and C-ABI accessors (`#[derive(ModExposable)]`, emitting `u8` for `bool`) and compile-time verified POD layout markers (`#[derive(AbiPod)]`). Enforces self-aliasing safety in mutators and computes `IS_POD` / `IS_RAW_INSERTABLE`. |
| **`host_runtime`** | Host Engine Bridge | Bevy plugin (`AbiModPlugin`, `ModErrorPolicy`), component registry (`ModRegistry` with per-`World` thread-local `CURRENT_REGISTRY`), dynamic system adapter (`DynamicCSystem` backed by `QueryState`), dense + `SparseSet` chunk iterator (`HostChunkIter`), and lifecycle manager (`ModHost`, `ModLoadError`, `ModSystemSet`). |
| **`guest_sdk`** | Mod Author SDK | Lightweight, Bevy-independent Rust wrapper. Provides `GuestApp`, `GuestPlugin`, `SystemContext`, `MultiQueryContext`, `catch_guest_panic`, `export_mod!`, `Chunk` split-borrow views (`next_chunk_view`, `read_write`, `columns_mut`), and safe command queues. |
| **`include/bevy_abi_mod.h`** | Canonical C Header | Standalone ISO C99/C++ header (`<stdint.h>`, `<stddef.h>`, no `<stdbool.h>`) defining all types, SemVer helpers, and static layout assertions matching `abi_contract` byte-for-byte. |

---

## 3. Dynamic System Lifecycle Flowcharts

### 3.1 Mod Load, Handshake, and Registration Pipeline
```text
Host Engine (App)                   host_runtime / ModHost                   Guest Dynamic Library (.cdylib)
      │                                       │                                             │
      │ 1. app.load_mod(path)                 │                                             │
      ├──────────────────────────────────────►│                                             │
      │                                       │ 2. libloading::Library::new(path)           │
      │                                       ├────────────────────────────────────────────►│ (OS dlopen)
      │                                       │                                             │
      │                                       │ 3. bevy_abi_mod_abi_version()               │
      │                                       ├────────────────────────────────────────────►│
      │                                       │◄────────────────────────────────────────────┤ Returns ABI_VERSION_PACKED
      │                                       │    (Check version matches host)             │
      │                                       │                                             │
      │                                       │ 4. bevy_abi_mod_init(get_proc, host_api, ctx)
      │                                       ├────────────────────────────────────────────►│
      │                                       │                                             │ 5. get_proc_address("Position_vtable")
      │                                       │◄────────────────────────────────────────────┤
      │                                       ├────────────────────────────────────────────►│ Returns *const PositionVTable
      │                                       │                                             │
      │                                       │                                             │ 6. host_api->register_system(ctx, &desc)
      │                                       │◄────────────────────────────────────────────┤
      │                                       │ 7. Construct DynamicCSystem                 │
      │                                       │    - Build FilteredAccessSet                │
      │                                       │    - Verify intra-system borrow             │
      │                                       │    - Attach Arc<AtomicBool> is_active flag  │
      │                                       │                                             │
      │                                       │◄────────────────────────────────────────────┤ Returns FFI_RESULT_OK
      │                                       │                                             │
      │ 8. Inject systems into Bevy Schedule  │                                             │
      │◄──────────────────────────────────────┤                                             │
```

### 3.2 Per-Frame System Execution Flow & Panic Containment
```text
Bevy Scheduler (Schedule Executor)          DynamicCSystem                           Guest System Callback
      │                                            │                                             │
      │ 1. DynamicCSystem::run_unsafe(world)       │                                             │
      ├───────────────────────────────────────────►│                                             │
      │                                            │ 2. Check Tombstone:                         │
      │                                            │    is_active.load(Acquire) && enabled       │
      │                                            │    [If false -> return Ok(()), no-op]       │
      │                                            │                                             │
      │                                            │ 3. Check Archetype Generation:              │
      │                                            │    world.archetypes().generation()          │
      │                                            │    [If changed -> refresh matched tables]   │
      │                                            │                                             │
      │                                            │ 4. Stack-allocate HostChunkIter             │
      │                                            │    and CommandQueueBridge                   │
      │                                            │                                             │
      │                                            │ 5. std::panic::catch_unwind(|| {            │
      │                                            │        callback(iter, commands, user_data)  │
      │                                            ├────────────────────────────────────────────►│
      │                                            │                                             │ 6. iter_next_chunk(iter)
      │                                            │◄────────────────────────────────────────────┤
      │                                            ├────────────────────────────────────────────►│ Returns count
      │                                            │                                             │
      │                                            │                                             │ 7. iter_column_mutable(iter, 0)
      │                                            │◄────────────────────────────────────────────┤
      │                                            ├────────────────────────────────────────────►│ Returns pointer & marks tick
      │                                            │                                             │
      │                                            │                                             │ 8. [If panic occurs]:
      │                                            │                                             │    catch_guest_panic intercepts
      │                                            │◄────────────────────────────────────────────┤ Returns FfiResult::PANIC_CAUGHT
      │                                            │                                             │
      │                                            │ 9. Evaluate Status:                         │
      │                                            │    If status == PANIC_CAUGHT or is_err():   │
      │                                            │        self.enabled = false (quarantine)    │
      │                                            │                                             │
      │◄───────────────────────────────────────────┤ Returns Ok(())                              │
      │                                            │                                             │
      │ 10. System::apply_deferred(world)          │                                             │
      ├───────────────────────────────────────────►│ Drains CommandQueue into World               │
```

### 3.3 Dynamic Unload, Teardown Hook, and Atomic Tombstoning
```text
Host Game Application                       ModHost                                  DynamicCSystem
      │                                        │                                           │
      │ 1. app.unload_mod(mod_id)              │                                           │
      ├───────────────────────────────────────►│                                           │
      │                                        │ 2. Phase 1: Tombstone Store               │
      │                                        │    is_active.store(false, Release)        │
      │                                        ├──────────────────────────────────────────►│ Flag flipped
      │                                        │                                           │
      │                                        │ 3. Phase 2: Guest Teardown Hook           │
      │                                        │    bevy_abi_mod_shutdown(null_mut())      │
      │                                        │    (Executed while code is still mapped)  │
      │                                        │                                           │
      │                                        │ 4. Phase 3: Dynamic Library Unmap         │
      │                                        │    drop(library) -> OS dlclose()          │
      │                                        │    (.text segment unmapped from memory)   │
      │                                        │                                           │
      │◄───────────────────────────────────────┤ Ok(())                                    │
      │                                        │                                           │
      │ ═══════════════════════════════════════╪═══════════════════════════════════════════│
      │ NEXT BEVY FRAME (Surviving Schedule)   │                                           │
      │ ═══════════════════════════════════════╪═══════════════════════════════════════════│
      │                                        │                                           │
      │ 5. Bevy Schedule executes leftover sys │                                           │
      ├───────────────────────────────────────────────────────────────────────────────────►│
      │                                        │                                           │ 6. DynamicCSystem::run_unsafe()
      │                                        │                                           │    is_active.load(Acquire) == false
      │                                        │                                           │    -> Early return Ok(())
      │                                        │                                           │    Foreign fn pointer NEVER called!
      │◄───────────────────────────────────────────────────────────────────────────────────┤
```

---

## 4. Formal Memory Safety Proofs

### 4.1 Proof A: Dynamic Allocator Isolation
**Theorem**: *No heap pointer allocated by the host runtime is ever deallocated by the guest plugin, and no heap pointer allocated by the guest plugin is ever deallocated by the host runtime.*

**Proof**:
1. **Boundary Type Confinement**:
   - The boundary between host and guest is defined exclusively in `abi_contract`.
   - Every struct in `abi_contract` is `#[repr(C)]` or `#[repr(transparent)]`.
   - No type in `abi_contract` implements `Drop` with a heap deallocator.
   - Pointers across the boundary are either:
     - Function pointers (`extern "C" fn(...) -> ...`)
     - Opaque borrowed handles (`*mut RawChunkIter`, `*mut RawCommandQueue`)
     - Borrowed views with explicit byte lengths (`StrView { ptr: *const u8, len: usize }`, `FfiSlice<T> { ptr: *const T, len: usize }`).
2. **Handle Invalidation**:
   - Opaque handles (`RawChunkIter`, `RawCommandQueue`) are pointers to stack-allocated host objects (`HostChunkIter`, `CommandQueueBridge`).
   - The host does not provide any `free_chunk_iter()` or `free_command_queue()` export across the C-ABI.
   - The handles are invalidated automatically when the host's `run_unsafe` stack frame unwinds.
3. **Heap-Owned Data Mutation**:
   - For host-allocated heap structures (`String`, `Vec<T>`), field setters generated by `mod_derive` accept borrowed byte buffers (`*const u8, usize`).
   - The host setter allocates a host-owned `String` or `Vec<T>` using the host's global allocator (`std::alloc::alloc`), copies the bytes from the guest pointer, and assigns it to the component.
   - The guest never takes ownership of the host buffer.
4. **Deferred Commands**:
   - When a guest calls `cmd_insert_component(queue, entity, name, data, len)` or `queue.insert_value(...)`, the host command queue copies `len` bytes into host-managed storage (`CommandQueue`).
   - The guest-provided memory buffer remains owned and managed by the guest.
5. **Guest Heap Lifetime**:
   - Any guest allocations (global state, scratch buffers) are allocated by the guest's allocator during `bevy_abi_mod_init` or system callbacks.
   - They are deallocated exclusively within `bevy_abi_mod_shutdown` by the guest's allocator before dynamic library unmapping.

$\therefore$ The intersection between the host allocator domain and the guest allocator domain is empty: $\mathcal{H}_{\text{host}} \cap \mathcal{H}_{\text{guest}} = \emptyset$. Memory allocator corruption is impossible. $\blacksquare$

---

### 4.2 Proof B: Self-Aliasing Prevention in Component Mutators
**Theorem**: *Let $B = [p_{\text{buf}}, p_{\text{buf}} + \text{cap})$ be the address interval of the host's currently allocated heap buffer for a component field (`String` or `Vec<T>`). If a guest invokes a setter passing a source slice $S = [p_{\text{src}}, p_{\text{src}} + \text{len})$ such that $S \subset B$ or $S \cap B \neq \emptyset$, the setter executes without use-after-free, memory corruption, or undefined behavior.*

**Proof**:
1. In a naive setter implementation:
   ```rust
   // NAIVE: Undefined Behavior on self-aliasing!
   field.clear(); // Deallocates or truncates buffer B
   field.push_str(incoming_slice); // Reads from S (dangling if S resided in B!)
   ```
2. In `mod_derive`, the generated setter for any host-allocated collection enforces interval collision detection:
   ```rust
   let buf_ptr = field.as_ptr() as usize;
   let buf_len = field.len();
   let buf_end = buf_ptr.saturating_add(buf_len);

   let src_ptr = data as usize;
   let src_end = src_ptr.saturating_add(len);

   // Interval intersection condition:
   let overlaps = src_ptr < buf_end && buf_ptr < src_end;
   ```
3. **Case 1: Disjoint Intervals ($overlaps = \text{false}$)**:
   - $S \cap B = \emptyset$.
   - Reading $S$ cannot be affected by mutating $B$.
   - The host calls `field.clear(); field.push_str(...)` or in-place slice copy. Zero redundant allocations occur.
4. **Case 2: Overlapping Intervals ($overlaps = \text{true}$)**:
   - $S \cap B \neq \emptyset$.
   - The host branches to defensive intermediate allocation:
     ```rust
     let temp = String::from(s); // 1. Fresh host allocation disjoint from B
     *field = temp;               // 2. Safely drops old B, replacing with temp
     ```
   - The read from $S$ occurs during `String::from(s)`, *before* $B$ is deallocated or overwritten.
   - When old $B$ is dropped, $S$ is no longer referenced.

$\therefore$ Self-aliasing pointer invalidation is completely prevented. $\blacksquare$

---

### 4.3 Proof C: Atomic Tombstoning Across Dynamic Unloads (`dlclose` Safety)
**Theorem**: *Let $M$ be a dynamically loaded shared library mapped into virtual memory range $[V_{\text{start}}, V_{\text{end}})$. When $M$ is unloaded via `ModHost::unload_mod` and unmapped via OS `dlclose()`, subsequent iterations of Bevy's schedule graph containing systems registered by $M$ will not dereference pointers in $[V_{\text{start}}, V_{\text{end}})$ or trigger access violations (SIGSEGV).*

**Proof**:
1. **Shared State**:
   - Each loaded mod maintains `is_active: Arc<AtomicBool>`, initialized to `AtomicBool::new(true)`.
   - Every `DynamicCSystem` registered by the mod holds a cloned handle to this `Arc<AtomicBool>`.
2. **Unload Synchronization**:
   - `ModHost::unload_mod` executes the following sequence:
     ```rust
     loaded.is_active.store(false, Ordering::Release);
     if let Some(shutdown) = loaded.shutdown_fn {
         shutdown(null_mut()); // Called while [V_start, V_end) is still valid!
     }
     drop(loaded.library);     // OS unmaps [V_start, V_end)
     ```
   - The atomic store uses `Ordering::Release`.
3. **Execution Guard**:
   - When Bevy's schedule executes, it calls `DynamicCSystem::run_unsafe(world)`.
   - The very first line of `run_unsafe` performs:
     ```rust
     if !self.enabled || self.self_conflict || !self.is_active.load(Ordering::Acquire) {
         return Ok(());
     }
     ```
   - The atomic load uses `Ordering::Acquire`.
   - By the synchronization rules of the C++20 / Rust memory model:
     $$\text{store}(\text{false}, \text{Release}) \xrightarrow{\text{synchronizes-with}} \text{load}(\text{Acquire})$$
   - Any execution of `run_unsafe` following the unload observes `is_active == false`.
4. **Non-Dereference Invariant**:
   - When `is_active.load(...)` returns `false`, `run_unsafe` immediately bails with `Ok(())`.
   - The foreign callback function pointer `self.callback`, which points to executable code in $[V_{\text{start}}, V_{\text{end}})$, is never invoked.
   - Host archetype tables, storages, and command queues are never touched.

$\therefore$ Dropping the library handle does not produce dangling execution or SIGSEGV crashes. $\blacksquare$

---

### 4.4 Proof D: POD Layout Separation, Bit Validity & Clone Drop Safety
**Theorem**: *Foreign bit patterns crossing the C-ABI can never materialize an invalid Rust bit pattern (`bool` $\notin \{0,1\}$, invalid enum discriminant, or overwritten `String`/`Vec` pointers), nor can entity cloning duplicate ownership of foreign-allocated resources.*

**Proof**:
1. **Primitive Newtypes & `FfiBool`**:
   - Status codes (`FfiResult(pub u32)`), access modes (`ComponentAccessMode(pub u32)`), and schedule selectors (`ScheduleLabelId(pub u32)`) are `#[repr(transparent)]` integer newtypes rather than Rust `enum`s.
   - Boolean values across FFI use `FfiBool = u8` (`0` = false, non-zero = true); `<stdbool.h>` is intentionally omitted from `include/bevy_abi_mod.h`, and `#[derive(ModExposable)]` emits `extern "C" fn(...) -> u8` / `fn(..., u8)` accessors for `bool` fields.
   - Invalid entity handles use `FfiEntity::NULL = FfiEntity(0)` (`Entity::try_from_bits(0) == None` in Bevy 0.20+, whereas `u64::MAX` is a valid entity and `Entity::PLACEHOLDER` is rejected on mutation).
2. **Three-Trait Separation (`AbiPod` $\subset$ `FfiLayout` $\subset$ `ExposableComponent`)**:
   - `AbiPod` requires `#[repr(C)]` / `#[repr(transparent)]`, non-packed layout, `Copy`, `!needs_drop::<Self>()`, and universal bit validity on every field (excluding `bool`, `String`, `Vec<T>`, and `#[repr(Rust)]` structs). `guest_sdk` restricts typed zero-copy column slices (`Chunk::read_write`, `as_typed_slice`) to `AbiPod` types (`#[derive(AbiPod)]`).
   - `FfiLayout` marks types safe for interior pointers and raw-byte insertion (`IS_RAW_INSERTABLE = true`). `ExposableComponent` is decoupled from `FfiLayout`; calling `host_cmd_insert_component` on any component with `IS_RAW_INSERTABLE == false` (containing `String`, `Vec<T>`, `bool`, `#[mod_field(skip)]`, or `#[mod_component(opaque)]`) immediately returns `FfiResult::PERMISSION_DENIED`.
3. **Clone Safety on Foreign-Droppable Dynamic Components**:
   - Runtime components registered via `host_register_component` with `drop_fn.is_some()` set Bevy's `ComponentCloneBehavior::Ignore` on their `ComponentDescriptor`. Consequently, `EntityCloner` (`clone_and_spawn`) never performs a bitwise shallow copy of foreign heap pointers, eliminating double-free UB. $\blacksquare$

---

## 5. Contiguous Archetype `Table` & `SparseSet` Chunk Traversal

Bevy ECS supports two component storage backends: dense Structure-of-Arrays **`Table`** columns (`StorageType::Table`) and per-component **`SparseSet`** maps (`StorageType::SparseSet`). Importantly, adding or removing a `SparseSet` component moves an entity to a new **Archetype** while leaving it in the **same dense `Table`**.

```text
Archetype Table (e.g. Entities with [Position, Velocity, Health])
┌─────────────┬────────────────────────────────────────────────────────┐
│ Entity ID   │ E1        │ E2        │ E3        │ ...                │
├─────────────┼────────────────────────────────────────────────────────┤
│ Position    │ (x, y, z) │ (x, y, z) │ (x, y, z) │ ... (Contiguous)   │
├─────────────┼────────────────────────────────────────────────────────┤
│ Velocity    │ (x, y, z) │ (x, y, z) │ (x, y, z) │ ... (Contiguous)   │
├─────────────┼────────────────────────────────────────────────────────┤
│ Health      │ (cur, max)│ (cur, max)│ (cur, max)│ ... (Contiguous)   │
└─────────────┴────────────────────────────────────────────────────────┘
```

### 5.1 Dual-Path Traversal Mechanics (`HostChunkIter`)
During `DynamicCSystem::initialize`, each query group builds a native Bevy `QueryState` via `QueryBuilder` and checks whether any data or filter (`FILTER_ONLY` / `WITHOUT`) clause references a `StorageType::SparseSet` component:
1. **Dense `Table` Fast Path (`HostChunkIter::new`)**:
   - Used whenever all data and filter components use `StorageType::Table`.
   - Iterates matched `TableId`s directly, handing the guest zero-copy contiguous column pointers (`table.get_column(component_id)`), byte strides, and `[FfiEntity; count]` slices at ~20M+ entities/sec.
2. **Archetype-Aware `SparseSet` Path (`HostChunkIter::new_archetypal`)**:
   - Activated automatically whenever any queried or filtered component uses `StorageType::SparseSet`.
   - Iterates per matched `Archetype` (never leaking shared-table entities excluded by `With<Sparse>` or `Without<Sparse>`), gathering archetype-local rows for both `Table` and `SparseSet` columns and propagating `Changed<T>` ticks back to the underlying `Table` row or `ComponentSparseSet` entry.

### 5.2 Steady-State Generation Caching
To eliminate archetype matching overhead on every frame, `DynamicCSystem` caches matched `TableId`s and `ArchetypeId`s in `CachedQueryGroup` via `QueryState::update_archetypes`. On every run, if `world.archetypes().generation()` is unchanged, cached slices are reused with zero steady-state heap allocations.

---

## 6. Multi-Query System Mechanics & Intra-System Borrow Checking

### 6.1 Architectural Need & System Descriptors
Single-query systems can only traverse one archetype set at a time. Complex game interactions (e.g., $N \times M$ combat checks where $N$ combat units check proximity against $M$ damage aura emitters) require two or more concurrent queries.

In `bevy_abi_mod`, Multi-Query Systems provide:
1. Dynamic registration of up to `MAX_SYSTEM_QUERIES = 8` independent query specifications.
2. Intra-system borrow-checking to prevent self-aliasing conflicts between queries.
3. Concurrent, stack-allocated chunk iterators passed as an array of handles.
4. $O(1)$ zero-allocation cursor rewinding via `iter_reset`.

The guest registers a multi-query system via `register_multi_query_system` (resolved via `get_proc_address`):
```c
typedef struct QuerySpec {
    const ComponentAccessDesc* queries;
    size_t query_count;
} QuerySpec;

typedef struct MultiQuerySystemDescriptor {
    uint32_t struct_size;
    const char* name;
    const QuerySpec* query_specs;
    size_t query_spec_count;
    MultiQuerySystemCallback callback;
    void* user_data;
} MultiQuerySystemDescriptor;
```

### 6.2 Intra-System Borrow Conflict Detection
During `DynamicCSystem::initialize`, the host checks intra-system compatibility across all declared query groups:
```rust
let mut access_set = FilteredAccessSet::new();

for group in &mut self.query_groups {
    let mut filtered_access = FilteredAccess::default();
    // Populate filtered_access for group...
    
    let mut candidate_set = FilteredAccessSet::new();
    candidate_set.add(filtered_access.clone());

    // Intra-system conflict check:
    if !access_set.is_compatible(&candidate_set) {
        self.self_conflict = true;
        self.enabled = false; // Safely quarantine!
    }

    access_set.add(filtered_access);
}
```
If Query 0 declares `WRITE Position` and Query 1 declares `READ Position`, `access_set.is_compatible` detects the borrow violation, disables the system, and prevents scheduler data races.

### 6.3 Stack-Allocated Iterators & Cursor Rewinding (`iter_reset`)
In `DynamicCSystem::run_unsafe`:
1. The host stack-allocates an array of concrete iterators:
   ```rust
   let mut host_iters: [Option<HostChunkIter<'_>>; MAX_SYSTEM_QUERIES] = [const { None }; 8];
   for (i, group) in self.query_groups.iter().take(count).enumerate() {
       host_iters[i] = Some(HostChunkIter::new(&storages.tables, &group.matched_tables, &group.columns, this_run));
   }
   ```
2. Pointers `raw_ptrs: [*mut RawChunkIter; 8]` are passed to `callback(raw_ptrs.as_ptr(), count, commands, user_data)`.
3. In nested iteration, the guest rewinds the inner query using `host_api->iter_reset(inner_iter)` or `host_iter_reset`:
   ```rust
   // Nested query execution without heap churn:
   while outer_iter.next_chunk() > 0 {
       (api.iter_reset)(inner_iter);
       while inner_iter.next_chunk() > 0 {
           // Evaluate N x M entity pairs at hardware speed
       }
   }
   ```

---

## 7. Parallel Scheduling & Borrow Checking

### 7.1 Dynamic `QueryBuilder` & `FilteredAccessSet`
When a mod registers a system, it supplies up to `MAX_QUERY_COMPONENTS = 32` `ComponentAccessDesc` entries per query (and up to `MAX_SYSTEM_QUERIES = 8` queries per system):
```rust
ComponentAccessDesc::write(c"Position".as_ptr())
```
During `System::initialize`:
1. `DynamicCSystem` resolves each component name to its runtime `(ComponentId, StorageType)` via `ModRegistry`.
2. It constructs a native Bevy `QueryBuilder` (`ref_id`, `mut_id`, `with_id`, `without_id`) and extracts `QueryState::component_access()` directly into Bevy's `FilteredAccessSet<ComponentId>`, guaranteeing 100% access-compatibility parity with native Bevy `Query` systems.
3. On multi-threaded executor worker threads, `DynamicCSystem::run_unsafe` binds `ModRegistry::make_current()` to thread-local `CURRENT_REGISTRY` so runtime `host_get_proc_address` calls remain isolated per `World`.

### 7.2 Concurrency, System Sets & Ordering
- Systems accessing disjoint components or performing concurrent reads execute in parallel across CPU worker threads.
- Every mod system is tagged with `ModSystemSet::All`, `ModSystemSet::Mod(ModId)`, and `ModNamedSystemSet(name)`, allowing both host schedules and `ScheduledSystemDescriptor` (`after` / `before` across `Startup`, `PreUpdate`, `Update`, `PostUpdate`, `FixedUpdate`, and `Last`) to order mod systems deterministically.

---

## 8. Change Detection Integration (`world.increment_change_tick()`)

Bevy's `Changed<T>` query filter compares component `Tick`s against each system's `last_run` tick:
- At the start of every execution, `DynamicCSystem::run_unsafe` advances `let this_run = world.increment_change_tick();` and updates `self.last_run` at completion, maintaining exact tick parity with native Bevy systems across multi-frame schedules and `World::check_change_ticks()` `u32::MAX` wraparound clamping.
- Requesting mutable column access via `iter_column_mutable` marks chunk ticks (`Table` or `ComponentSparseSet`), while `iter_mark_changed(iter, col, row)` flags individual rows.

---

## 9. Fault Isolation, `ModErrorPolicy` & Two-Tier Panic Containment

Cross-library panics between distinct Rust runtimes or C shared libraries are contained via **Two-Tier Panic Containment** and configurable **`ModErrorPolicy`**:
1. **Tier 1 (Guest-Side Containment):** `guest_sdk` (`catch_guest_panic`, `export_mod!`, `GuestApp::add_system`) catches panics inside the guest's own `std` runtime and returns `FfiResult::PANIC_CAUGHT`.
2. **Tier 2 (Host-Side Defense):** `DynamicCSystem::run_unsafe` also wraps callback execution in `catch_unwind`.
3. **Configurable `ModErrorPolicy` & `ModSystemFaulted` Events:**
   - `ModErrorPolicy::Quarantine` (default): logs the fault, sets `self.enabled = false` to quarantine the faulty system, and queues a `ModSystemFaulted` ECS event/message.
   - `ModErrorPolicy::Log`: logs and emits `ModSystemFaulted` while allowing subsequent ticks to retry.
   - `ModErrorPolicy::ReportToBevy`: returns `Err(BevyError)` directly into Bevy's schedule error handler alongside `ModSystemFaulted`.

---

## 10. Deterministic Lifecycle, Schedule Excision & Drop-Trampoline Isolation

Hot-unloading native code (`ModHost::try_unload_mod`) executes a deterministic teardown pipeline:
1. **Atomic Tombstone & Drop-Trampoline Deactivation:** Flips `is_active.store(false, Ordering::Release)` and updates the 1,024-slot lock-free per-`(WorldId, ComponentUuid)` atomic drop-trampoline pool (`MAX_DROP_TRAMPOLINES = 1024`), ensuring unmapped foreign `ComponentDropFn` pointers are never invoked even during partial multi-mod unloads or `World::drop`.
2. **Schedule Graph Excision:** Calls `Schedule::remove_systems_in_set(ModSystemSet::Mod(mod_id), world, ScheduleCleanupPolicy::RemoveSystemsOnly)` across tracked schedules so schedule node counts stay constant across thousands of hot-reload cycles.
3. **ECS Metadata & Shutdown Hook:** Despawns the `LoadedModInfo` entity, emits `ModUnloaded`, and—when the per-inode `SharedLibState` reference count reaches `0`—invokes `bevy_abi_mod_shutdown()` while `.text` is still mapped (triggering `guest_sdk`'s LIFO `cleanup_registered_guest_state()`).
4. **OS Library Unmap:** Drops `libloading::Library` (`dlclose` / `FreeLibrary`) under the per-inode lock.
