# `bevy_abi_mod` Architecture & System Design

This document details the architectural principles, component models, memory contracts, and execution pipeline of the **`bevy_abi_mod`** framework.

---

## 1. Architectural Mission & Core Constraints

Bevy Engine's high performance stems from its archetypal Entity Component System (ECS). However, standard Rust dynamic libraries (`.dylib` / `.so` / `.dll`) share standard Rust ABI assumptions:
- Both host and plugin must be compiled with the **exact same `rustc` compiler version**.
- Both must share identical compiler flags (`codegen-units`, optimization levels, panic runtimes).
- Both cross-allocate over the same global heap allocator.
- Upgrading Bevy or the game requires recompiling every mod.

**`bevy_abi_mod` solves this by introducing an Asymmetric, Zero-Serialization C-ABI Boundary.**

```
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

### The Three Golden Invariants
1. **Zero Shared Rust ABI**: Mods can be written in any language targeting the C-ABI (Rust, C99, C++, Zig).
2. **Zero Memory Allocator Crossover**: Memory allocated by the host is deallocated by the host; memory allocated by the guest is deallocated by the guest.
3. **Hardware-Speed Contiguous Traversal**: No serialization (JSON/Protobuf/Bincode) in the hot loop. Systems receive direct pointers to contiguous archetype table columns.

---

## 2. Crate Responsibilities & Layered Boundaries

| Layer / Crate | Type | Responsibilities & Constraints |
|---|---|---|
| **`abi_contract`** | Core Contract | `#![no_std]`, `#[repr(C)]`, zero external dependencies. Defines `HostApi`, `SystemDescriptor`, `RawChunkIter`, `FfiEntity`, `FfiResult`, and status codes. |
| **`mod_derive`** | Procedural Macro | Generates `#[repr(C)]` VTables and C-ABI accessors for Bevy components (`#[derive(Component, ModExposable)]`). Prevents self-aliasing UB in setters. |
| **`host_runtime`** | Host Engine Bridge | Bevy plugin (`AbiModPlugin`), component registry (`ModRegistry`), dynamic system adapter (`DynamicCSystem`), table chunk iterator (`HostChunkIter`), and lifecycle manager (`ModHost`). |
| **`guest_sdk`** | Mod Author SDK | Lightweight, Bevy-independent Rust wrapper. Encapsulates `catch_guest_panic`, `export_mod!`, `wrap_system_callback!`, and bounds-checked `ChunkSlice`. |
| **`include/bevy_abi_mod.h`** | Canonical C Header | Complete C99/C++ header defining all types and function pointers matching `abi_contract`. |

---

## 3. The Memory & Allocator Isolation Model

### 3.1 Primitive Fields
Primitives (`f32`, `i32`, `u64`, `bool`) are copied directly across the boundary by value using generated accessors:
```c
float Position_get_x(const void* ptr);
void  Position_set_x(void* ptr, float val);
```

### 3.2 Compound Types & Nested Structs
Compound structures (e.g. `Vec3`) return typed interior pointers:
```c
const void* Transform_ptr_translation(const void* ptr);
void*       Transform_ptr_mut_translation(void* ptr);
```

### 3.3 Heap-Allocated Types (`String`, `Vec<T>`)
Rust's `String` and `Vec` allocate from the host's global allocator. Passing a pointer to be freed or resized by a `.cdylib` with its own allocator results in immediate allocator corruption (`SIGSEGV` or `malloc: free(): pointer being freed was not allocated`).

`mod_derive` mitigates this via **Host-Allocated Copy Mutators**:
- **Getter**: Exposes a borrowed slice view `StrView { ptr: *const u8, len: usize }`.
- **Setter**: The guest supplies a byte pointer and length. The host reallocates its own internal buffer and copies the bytes.
- **Self-Aliasing Protection**: If a guest passes a slice pointing to the component's own internal buffer (e.g. `tag.set_label(tag.get_label())`), `mod_derive` detects pointer overlap:
  $$\text{overlap} = (ptr_{\text{src}} < ptr_{\text{dest\_end}}) \land (ptr_{\text{dest}} < ptr_{\text{src\_end}})$$
  When overlap is detected, the host creates an intermediate heap buffer to prevent `ptr::copy_nonoverlapping` violation.

---

## 4. Contiguous Archetype Table Chunk Traversal

Bevy ECS organizes entities with identical component sets into contiguous **Archetypes** and **Tables** (Structure-of-Arrays).

```
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

### 4.1 Zero-Allocation Traversal
During system execution:
1. `HostChunkIter` queries Bevy's `world.storages().tables`.
2. It fetches the column pointer for each requested component using `table.get_column(component_id).data()`.
3. It hands the guest:
   - `count`: Number of entities in the current table chunk.
   - Column base pointers via `iter_column_readonly(iter, col_idx)` and `iter_column_mutable(iter, col_idx)`.
   - Byte strides via `iter_column_stride(iter, col_idx)`.
   - Entity IDs via `iter_entities(iter)`.
4. The guest system iterates through the chunk with simple pointer arithmetic:
   $$\text{element\_ptr}(i) = \text{base\_ptr} + (i \times \text{stride})$$

This achieves native hardware speed with full CPU cache prefetching, matching native Rust iteration performance (~20M entities/sec).

---

## 5. Parallel Scheduling & Borrow Checking

A critical requirement of Bevy is running non-conflicting systems concurrently across worker threads.

### 5.1 Dynamic `FilteredAccessSet`
When a mod registers a system, it supplies an array of `ComponentAccessDesc`:
```rust
ComponentAccessDesc {
    name: b"Position\0".as_ptr() as *const c_char,
    mode: ComponentAccessMode::WRITE,
}
```
During `System::initialize`:
1. `DynamicCSystem` resolves each component name to its runtime `ComponentId` via `ModRegistry`.
2. It populates Bevy's internal `FilteredAccessSet<ComponentId>`:
   - `add_component_read(component_id)` for `READ`.
   - `add_component_write(component_id)` for `WRITE`.
3. Bevy's multi-threaded scheduler inspects this access set. Systems accessing disjoint components or performing concurrent reads run in parallel across CPU cores. Conflicting systems are safely serialized.

---

## 6. Change Detection Integration (`AtomicTick`)

Bevy features a sophisticated change-detection mechanism (`Changed<T>` query filter) driven by world ticks.

In `bevy_abi_mod`:
- When a guest system requests mutable column access via `iter_column_mutable`, or explicitly invokes `iter_mark_changed(iter, col, row)`:
- `HostChunkIter` locates the component's `AtomicTick` storage in the table column.
- It writes the current `world.change_tick()` to the tick cell.
- Subsequent native or mod systems filtering by `Changed<Position>` instantly detect that the entity was mutated by the mod.

---

## 7. Fault Isolation & Error Containment

Cross-library panics between distinct Rust runtimes or C shared libraries pose a major safety risk:
- Catching an unwinding panic from a separate `.cdylib` in the host's `catch_unwind` is classified as catching a foreign exception, which can abort the entire process.
- To prevent this, `guest_sdk` enforces **Guest-Side Panic Containment** (`catch_guest_panic`):
  1. The guest catches any panic within its own `std` runtime.
  2. It intercepts the panic and converts it to `FfiResult::PANIC_CAUGHT`.
  3. The host inspects the returned status.
  4. Upon receiving `PANIC_CAUGHT` or an unhandled error:
     - The host logs the faulted system name.
     - The host marks the system as disabled.
     - The host continues running Bevy's game loop without interrupting other systems or crashing the game.

---

## 8. Deterministic Lifecycle & Hot-Reloading

Hot-unloading native code requires deterministic teardown:
1. **System Disabling**: All systems registered by the mod are disabled or removed from Bevy's schedule.
2. **Shutdown Hook**: The host calls the mod's optional `bevy_abi_mod_shutdown()` entry point to allow the mod to release guest-allocated resources.
3. **Handle Drop**: The `libloading::Library` handle is dropped, executing OS-level library unmapping (`dlclose` on Unix, `FreeLibrary` on Windows).
4. **Post-Unload Immunity**: The host ECS engine continues operating without dangling pointers.
