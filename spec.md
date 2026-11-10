# Specification: `bevy_abi_mod`
**Asymmetric, Zero-Serialization C-ABI Modding Framework for Bevy ECS**

---

## 1. Vision & Problem Statement

### 1.1 The Core Challenge
In Rust game development with Bevy Engine, dynamically loading external mods/plugins at runtime as native shared libraries (`.so`, `.dylib`, `.dll`) faces the **Rust ABI instability barrier**:
- Rust has no stable ABI. Different compiler versions, optimization flags (`-C opt-level`), target features, standard library revisions, or even compiler build hashes alter type layouts, calling conventions, and symbol name mangling.
- Heap allocators cannot be mixed across dynamic boundaries without strict encapsulation: memory allocated by host allocator cannot be freed by guest allocator.
- Serializing ECS data to JSON, Bincode, or MessagePack introduces unacceptable latency (destroying 60/120 FPS frame budgets) and loses cache-locality.

### 1.2 The Solution: `bevy_abi_mod`
`bevy_abi_mod` provides an **asymmetric, zero-serialization C-ABI boundary** between a Bevy host and guest mods (written in Rust, C99, C++, Zig, etc.):
1. **Zero Shared Rust ABI Assumptions:** Communication exclusively via `extern "C"` functions, primitive types, opaque pointers (`*mut c_void`), and `#[repr(C)]` VTables/iterators.
2. **Single-Handshake Symbol Resolution:** Inspired by Godot GDExtension, the host exports a single entry symbol or passes a `get_proc_address` function pointer at load time to populate function pointer tables without dynamic linking hell.
3. **Hardware-Speed ECS Access:** Exposes Bevy Archetype `Table` columns directly via contiguous chunk iterators (inspired by Flecs `ecs_iter_t`) using raw pointers and byte strides.
4. **First-Class Bevy Scheduler Integration:** Guest systems are wrapped in a native Bevy dynamic `System` implementation that registers exact `FilteredAccessSet` permissions (read/write component locks), enabling Bevy's multi-threaded executor to run mod systems in parallel without data races.
5. **Deterministic Lifecycle & Safe Unloading:** Mod modules declare setup and teardown hooks (`mod_init`, `mod_shutdown`), allowing clean state eviction and hot-reloading without dangling VTable pointers.

---

## 2. Architecture & Design

### 2.1 Component Interaction Overview

```
+---------------------------------------------------------------------------------+
|                                HOST (Bevy Engine)                               |
|                                                                                 |
|  [AbiModPlugin] ---> [ModManager (libloading)]                                  |
|                            | loads .dll/.dylib/.so                              |
|                            v                                                    |
|                   [HostApiVTable / get_proc_address]                            |
|                            ^                                                    |
|                            | (Single C-ABI Handshake)                           |
+----------------------------|----------------------------------------------------+
                             |
                     C-ABI Contract Boundary (abi_contract)
                             |
+----------------------------|----------------------------------------------------+
|                            v                                                    |
|                     [mod_entry(Api)]                                            |
|                            |                                                    |
|  +-------------------------+-------------------------------------------------+  |
|  | GUEST (Rust / C99 / Zig Mod)                                              |  |
|  | - Registers systems: `register_system("update_vel", access, callback)`   |  |
|  | - System callback receives `EcsChunkIter`:                                |  |
|  |     `entities: *const EntityId`                                           |  |
|  |     `column_ptrs: [*mut c_void; N]`                                       |  |
|  |     `strides: [usize; N]`                                                 |  |
|  |     `count: usize`                                                        |  |
|  | - Iterates table columns directly in SoA layout with zero copies          |  |
|  +---------------------------------------------------------------------------+  |
+---------------------------------------------------------------------------------+
```

### 2.2 Memory Safety & Allocation Rules
1. **Owner-Frees Rule:** Any memory allocated by the host must be freed by the host. Any memory allocated by the guest must be freed by the guest.
2. **Opaque Handles:** Complex host types (World, Entity, Query, Commands) are exchanged across the C ABI as opaque pointers (`*mut c_void` / type-safe newtypes like `EcsWorldHandle`, `EcsSystemContextHandle`).
3. **No Direct Fat Pointers:** Trait objects (`dyn Trait`), slices (`&[T]`), and Rust `String` never cross the ABI boundary directly. Instead, explicit `#[repr(C)]` structs are used:
   ```rust
   #[repr(C)]
   pub struct StrView {
       pub ptr: *const u8,
       pub len: usize,
   }
   ```
4. **Component Access Bounds:** Access sets are validated by the host prior to scheduling. A guest mod attempting to write to a component marked read-only or unregistered triggers an immediate error code / panic guard.

---

## 3. Workspace Layout

```text
bevy_abi_mod/
├── Cargo.toml                  # Workspace definition
├── spec.md                     # Deep technical specification
├── gemini.md                   # Agent workflow guidelines & instructions
├── crates/
│   ├── abi_contract/           # Zero-dependency #![no_std] C-ABI types & function pointer signatures
│   │   ├── Cargo.toml
│   │   └── src/lib.rs
│   ├── mod_derive/             # Proc-macro crate (#[derive(ModExposable)])
│   │   ├── Cargo.toml
│   │   └── src/lib.rs
│   ├── host_runtime/           # Bevy host plugin, dynamic system adapter, ChunkIterator, loader
│   │   ├── Cargo.toml
│   │   └── src/lib.rs
│   └── guest_sdk/              # Safe idiomatic Rust wrappers for mod developers
│       ├── Cargo.toml
│       └── src/lib.rs
└── tests/
    ├── asymmetric_rust_mod/    # Standalone Cargo project (independent lockfile) compiled to .cdylib
    │   ├── Cargo.toml
    │   └── src/lib.rs
    └── asymmetric_c_mod/       # Pure C99 mod source to verify multi-language ABI compliance
        ├── mod.c
        └── Makefile
```

---

## 4. Subsystem Specifications

### 4.1 `abi_contract`
- `#![no_std]` crate with zero external dependencies.
- Contains only:
  - `#[repr(C)]` structs (e.g., `EcsChunkIter`, `SystemAccessDescriptor`, `ComponentRegistration`, `ModHeader`).
  - `extern "C" fn` typedefs for host API functions and guest callbacks.
  - Constant return codes / error enums (`ModResult`: `MOD_SUCCESS`, `MOD_ERR_INVALID_VERSION`, `MOD_ERR_COMPONENT_NOT_FOUND`, etc.).
  - Version handshake constants (`BEVY_ABI_MOD_API_VERSION_MAJOR`, `_MINOR`, `_PATCH`).

### 4.2 `mod_derive`
- Procedural macro crate exporting `#[derive(ModExposable)]`.
- Generates:
  - Static component UUID / identifier hash based on type name or explicit attribute `#[mod_component(uuid = "...")]`.
  - VTable descriptor for host component inspection, size, alignment, drop glue function pointer (if managed by host).
  - Registration helper into host's `ComponentRegistry`.

### 4.3 `host_runtime`
- Depends on `bevy` (or `bevy_ecs`) and `libloading`.
- Implements:
  - `AbiModPlugin`: Bevy `Plugin` managing the lifecycle of mods.
  - `ModHost`: Loads libraries, validates header/handshake, invokes `mod_init`, and captures shutdown hooks.
  - `DynamicCSystem`: Implements `bevy_ecs::system::System`.
    - Declares `archetype_component_access()` and `component_access()` via `FilteredAccessSet`.
    - In `run_unsafe()`, creates an `UnsafeWorldCell`, iterates matching archetypes/tables, constructs `EcsChunkIter`, and invokes the mod's `extern "C" fn(ctx: *mut c_void, iter: *mut EcsChunkIter)`.
  - `CommandQueueBridge`: Receives commands recorded by guest mods (spawn, despawn, insert, remove) and flushes them into Bevy's deferred `CommandQueue`.

### 4.4 `guest_sdk`
- Ergonomic Rust wrapper over raw `abi_contract`.
- Does not depend on Bevy.
- Features:
  - Macro `export_mod!(init_fn, shutdown_fn)`.
  - Type-safe system builder: `SystemBuilder::new("my_system").with_read::<Transform>().with_write::<Velocity>().build(...)`.
  - Safe iterator over chunk columns: yields `(&Transform, &mut Velocity)` without unsafe pointer arithmetic on the consumer end.

---

## 5. Milestone & Implementation Roadmap

- **Phase 1: ABI Contract & Scaffold**
  - Define all C-ABI types, function signatures, and error codes in `crates/abi_contract`.
  - Verify C header generation capability (`cbindgen` compatibility).
- **Phase 2: Host Dynamic System Bridge**
  - Implement `DynamicCSystem` using Bevy `UnsafeWorldCell` and `FilteredAccessSet`.
  - Benchmark chunk iteration over Bevy table columns against native Bevy query iteration.
- **Phase 3: Module Loading & Handshake**
  - Implement `ModHost` with `libloading`.
  - Implement single-symbol `get_proc_address` handshake.
- **Phase 4: Guest SDK & Derive Macro**
  - Build `guest_sdk` ergonomic abstractions.
  - Implement `#[derive(ModExposable)]`.
- **Phase 5: Cross-Language Verification Tests**
  - Write `tests/asymmetric_rust_mod` compiled with different settings.
  - Write `tests/asymmetric_c_mod` compiled with `cc`.
  - Assert identical behavior, performance, and memory cleanliness.
