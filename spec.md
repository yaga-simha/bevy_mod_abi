# Specification: `bevy_abi_mod`
**Asymmetric, Zero-Serialization C-ABI Modding Framework for Bevy ECS**

---

## 1. Vision & Problem Statement

### 1.1 The Core Challenge
In Rust game development with Bevy Engine (v0.20+), dynamically loading external mods/plugins at runtime as native shared libraries (`.so`, `.dylib`, `.dll`) faces the **Rust ABI instability barrier**:
- Rust has no stable ABI. Different compiler versions, optimization flags (`-C opt-level`), target features, standard library revisions, or even compiler build hashes alter type layouts, calling conventions, and symbol name mangling.
- Heap allocators cannot be mixed across dynamic boundaries without strict encapsulation: memory allocated by the host allocator cannot be freed by the guest allocator.
- Serializing ECS data to JSON, Bincode, or MessagePack introduces unacceptable latency (destroying 60/120 FPS frame budgets) and loses cache-locality.

### 1.2 The Solution: `bevy_abi_mod`
`bevy_abi_mod` provides an **asymmetric, zero-serialization C-ABI boundary** between a Bevy host and guest mods (written in Rust, C99, C++, Zig, etc.):
1. **Zero Shared Rust ABI Assumptions:** Communication exclusively via `extern "C"` functions, primitive types, opaque pointers (`*mut c_void`), and `#[repr(C)]` VTables/iterators.
2. **Single-Handshake Symbol Resolution:** Inspired by Godot GDExtension (`gdextension_interface.h`), the host passes a `get_proc_address` function pointer at load time to populate static function-pointer structs (`VTables`) inside the plugin, avoiding OS-specific `dlsym`/`GetProcAddress` symbol-export headaches.
3. **Hardware-Speed ECS Access:** Exposes Bevy Archetype `Table` columns directly via contiguous chunk iterators (inspired by Flecs `ecs_iter_t`) using opaque handles, raw column pointers, and byte strides.
4. **First-Class Bevy Scheduler Integration:** Guest systems are wrapped in a native Bevy dynamic `System` implementation (`DynamicCSystem`) that registers exact `FilteredAccessSet` permissions (read/write component locks), enabling Bevy's multi-threaded executor to run mod systems in parallel without data races.
5. **Deterministic Lifecycle & Safe Unloading:** Mod modules declare setup and teardown hooks (`bevy_abi_mod_init`, `bevy_abi_mod_shutdown`, modeled after Unreal Engine Gameplay Modules), allowing clean state eviction and hot-reloading without dangling VTable pointers.

---

## 2. Architecture & Design

### 2.1 Component Interaction Overview

```text
+---------------------------------------------------------------------------------+
|                                HOST (Bevy Engine)                               |
|                                                                                 |
|  [AbiModPlugin] ---> [ModHost / ModRegistry (libloading)]                       |
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
|                     [bevy_abi_mod_init(get_proc_address, host_api, ctx)]        |
|                            |                                                    |
|  +-------------------------+-------------------------------------------------+  |
|  | GUEST (Rust / C99 / Zig Mod)                                              |  |
|  | - Resolves component VTables via `get_proc_address("<Component>_vtable")` |  |
|  | - Registers systems: `register_system(ctx, &SystemDescriptor)`            |  |
|  | - System callback receives `(iter: *mut RawChunkIter,                     |  |
|  |                              commands: *mut RawCommandQueue,              |  |
|  |                              user_data: *mut c_void) -> FfiResult`        |  |
|  | - Iterates table columns directly in SoA layout with zero copies          |  |
|  +---------------------------------------------------------------------------+  |
+---------------------------------------------------------------------------------+
```

### 2.2 Memory Safety, Borrow Lifetimes & Allocation Rules
1. **Owner-Frees Rule:** Any memory allocated by the host must be freed by the host. Any memory allocated by the guest must be freed by the guest.
2. **Opaque Handles:** Complex host types (`World`, `RawChunkIter`, `RawCommandQueue`) are exchanged across the C ABI as opaque pointers (`*mut c_void` or zero-sized `#[repr(C)]` opaque structs).
3. **No Direct Fat Pointers:** Trait objects (`dyn Trait`), slices (`&[T]`), and Rust `String` never cross the ABI boundary directly. Instead, explicit `#[repr(C)]` structs are used:
   ```rust
   #[repr(C)]
   pub struct FfiSlice<T> {
       pub ptr: *const T,
       pub len: usize,
   }

   #[repr(C)]
   pub struct StrView {
       pub ptr: *const u8,
       pub len: usize,
   }
   ```
4. **Borrowed Slice Lifetimes & Aliasing Safety:** Any `FfiSlice<T>` or `StrView` returned by a component getter borrows directly from the host's component storage. It is valid only until the next mutation of that field, structural ECS change, or the end of the current chunk iteration step. Host string/vec setters detect overlapping source buffers (`get` slice passed directly back into `set`) and copy via a host-side temporary before clearing to prevent aliasing UB.
5. **No Rust `enum` or `bool` Return/Input Hazards Across FFI:** Any status code or access-mode field written by C/foreign code and read by Rust uses a `#[repr(transparent)]` primitive integer newtype (`FfiResult(pub u32)`, `ComponentAccessMode(pub u32)`, `u8` boolean flags) so arbitrary bit patterns from foreign code can never construct an invalid Rust enum discriminant or invalid `bool` (which would be immediate UB).
6. **Cross-Runtime Panic Isolation:** Because an asymmetrically compiled Rust `.cdylib` links its own isolated `std` runtime, an unwinding panic from the guest is a foreign exception to the host (`std::panic::catch_unwind` on a foreign exception has unspecified behavior and may abort). Therefore:
   - Guest-side callbacks return `FfiResult` (`FfiResult::OK` or `FfiResult::PANIC_CAUGHT`).
   - `guest_sdk` wraps guest Rust callbacks in `std::panic::catch_unwind` **inside the guest runtime** before crossing the `extern "C"` boundary and translates panics into `FfiResult::PANIC_CAUGHT`.
   - Host-side `DynamicCSystem::run_unsafe` checks for `FfiResult::PANIC_CAUGHT` (and also guards in-process test callbacks with `catch_unwind`), logs the fault, and marks the offending system/mod disabled.
7. **Component Access Bounds:** Access sets are validated by the host prior to scheduling. A guest mod attempting to write to a component column marked read-only or filter-only receives a null pointer (`ptr::null_mut()`).

---

## 3. Workspace Layout

```text
bevy_abi_mod/
├── Cargo.toml                  # Workspace definition
├── spec.md                     # Deep technical specification
├── gemini.md                   # Agent workflow guidelines & instructions
├── include/
│   └── bevy_abi_mod.h          # C99 header matching `abi_contract` (cbindgen-compatible)
├── crates/
│   ├── abi_contract/           # Zero-dependency `#![no_std]` C-ABI types & function pointer signatures
│   │   ├── Cargo.toml
│   │   └── src/lib.rs
│   ├── mod_derive/             # Proc-macro crate (`#[derive(ModExposable)]`)
│   │   ├── Cargo.toml
│   │   ├── src/lib.rs
│   │   └── tests/validation_gate_1.rs
│   ├── host_runtime/           # Bevy host plugin, dynamic system adapter, ChunkIterator, CommandQueueBridge, loader
│   │   ├── Cargo.toml
│   │   └── src/lib.rs
│   └── guest_sdk/              # Safe idiomatic Rust wrappers for mod developers
│       ├── Cargo.toml
│       └── src/lib.rs
└── tests/
    ├── asymmetric_rust_mod/    # Standalone Cargo project (committed independent Cargo.lock) compiled to .cdylib
    │   ├── Cargo.toml
    │   ├── Cargo.lock
    │   └── src/lib.rs
    └── asymmetric_c_mod/       # Pure C99 mod source to verify multi-language ABI compliance
        ├── mod.c
        └── Makefile
```

*(Progression examples live under `examples/01_minimal_handshake` through `examples/05_cute_3d_winit`, including 2D TUI and 3D `winit` + `wgpu` runtime dynamic mod hot-reloading across Rust and C99.)*

---

## 4. Detailed Subsystem Specifications

### 4.1. `crates/abi_contract` (The Immutable C-ABI Boundary)
- Compiles with `#![no_std]`, `#![deny(unsafe_op_in_unsafe_fn)]`, and `#![deny(improper_ctypes_definitions)]`.
- Version handshake constants (`ABI_VERSION_MAJOR`, `ABI_VERSION_MINOR`, `ABI_VERSION_PATCH`, `ABI_VERSION_PACKED`) and `struct_size: u32` headers on extensible descriptor structs.
- Defines `#[repr(C)]` / `#[repr(transparent)]` types:
  - `FfiEntity(pub u64)` (opaque 64-bit entity bits matching `bevy_ecs::entity::Entity::to_bits()`).
  - `FfiSlice<T> { pub ptr: *const T, pub len: usize }` and `StrView { pub ptr: *const u8, pub len: usize }`.
  - `FfiResult(pub u32)` transparent status code (`OK = 0`, `INVALID_COMPONENT = 1`, `ACCESS_CONFLICT = 2`, `PANIC_CAUGHT = 3`, `NULL_POINTER = 4`, `INVALID_UTF8 = 5`, `PERMISSION_DENIED = 6`, `INVALID_VERSION = 7`, `INTERNAL_ERROR = 99`).
  - `ComponentUuid { pub bytes: [u8; 16] }`.
  - `GetProcAddressFn = unsafe extern "C" fn(name: *const c_char) -> *const c_void`.
  - `RawChunkIter`: opaque C handle passed to mod system callbacks, paired with host C-ABI iteration functions:
    - `iter_next_chunk(iter: *mut RawChunkIter) -> usize` (returns `0` when exhausted, or `entity_count` for the active archetype table).
    - `iter_reset(iter: *mut RawChunkIter) -> FfiResult` (rewinds chunk cursor to `0`, resolved via `get_proc_address("iter_reset")` for nested multi-query loops).
    - `iter_column_readonly(iter: *const RawChunkIter, index: usize) -> *const u8`.
    - `iter_column_mutable(iter: *mut RawChunkIter, index: usize) -> *mut u8` (marks `ComponentTicks` as changed for all entities in the chunk; returns `null_mut()` if column is read-only or filter-only).
    - `iter_mark_changed(iter: *mut RawChunkIter, index: usize, row: usize) -> FfiResult`.
    - `iter_entities(iter: *const RawChunkIter) -> *const FfiEntity`.
    - `iter_column_stride(iter: *const RawChunkIter, index: usize) -> usize`.
  - Multi-Query & Negative Filter Extensions (resolved via `get_proc_address`):
    - `ComponentAccessMode::WITHOUT = ComponentAccessMode(3)` (`Without<T>` archetype exclusion filter).
    - `QuerySpec`, `MultiQuerySystemCallback`, `MultiQuerySystemDescriptor`, and `RegisterMultiQuerySystemFn` (`get_proc_address("register_multi_query_system")`), allowing up to 8 disjoint stack-allocated `RawChunkIter` handles in a single `DynamicCSystem` with intra-system borrow-conflict validation.
  - Runtime Dynamic Component Registration Extensions:
    - `ComponentDropFn = unsafe extern "C" fn(ptr: *mut c_void)`.
    - `ModComponentDescriptor` (`DynamicComponentDescriptor` alias): `#[repr(C)]` 64-byte descriptor (`struct_size`, `name`, `uuid`, `size`, `align`, `drop_fn: Option<ComponentDropFn>`, `vtable_ptr`) mirrored byte-for-byte with static assertions in `include/bevy_abi_mod.h`.
    - `RegisterComponentFn` exposed directly on `HostApi::register_component` (and via `get_proc_address("register_component")`) so guest mods in Rust, C99, C++, or Zig can introduce structured components unknown at host compile time.
  - `RawCommandQueue`: opaque C handle for deferred entity spawns, despawns, and component byte insertions (`CommandQueueBridge`, supporting both static `ModExposable` components and runtime-registered components via host-owned `AlignedBuffer` copies).
  - `FfiLayout` unsafe marker trait for types safe to expose via interior pointers or `Vec<T>` slices.
  - `ExposableComponent` trait exposing `COMPONENT_NAME`, `UUID`, `VTABLE`, `vtable_ptr()`, `size()`, `align()`, and `resolve_symbol()`.

### 4.2. `crates/mod_derive` (`#[derive(ModExposable)]`)
For any Bevy `#[derive(Component, ModExposable)]` struct (which remains standard `#[repr(Rust)]`):
- Supports attributes:
  - `#[mod_component(name = "...", uuid = "...", export_symbols)]`
  - `#[mod_field(skip | nested | primitive | string | vec)]`
- Rejects generic structs, enums, and unions with clear compile-time diagnostics.
- Generates `pub unsafe extern "C"` accessors (and optional `#[no_mangle]` exports):
  - **Primitives (`f32`, `i32`, `u64`, `bool`, etc.):** `<Struct>_get_<field>(ptr: *const c_void) -> <Type>` and `<Struct>_set_<field>(ptr: *mut c_void, val: <Type>)`.
  - **Nested Compound Types (`Vec3`, sub-structs implementing `FfiLayout`):** `<Struct>_ptr_<field>(ptr: *const c_void) -> *const c_void` and `<Struct>_ptr_mut_<field>(ptr: *mut c_void) -> *mut c_void`.
  - **Heap-Allocated Fields (`String`, `Vec<T>` where `T: FfiLayout + Copy`):** Never passes owned Rust `String` or `Vec` across the boundary. Generates host-allocated mutators:
    - `<Struct>_get_<string_field>(ptr: *const c_void) -> FfiSlice<u8>`
    - `<Struct>_set_<string_field>(ptr: *mut c_void, utf8_ptr: *const u8, len: usize) -> FfiResult` (validates null pointers, UTF-8 validity, and self-aliasing overlap; host allocates/resizes its own `String` and copies bytes).
- Generates `#[repr(C)] pub struct <Struct>VTable` and a `static` VTable instance with a guaranteed unique address containing `size: usize`, `align: usize` (exact `core::mem::size_of::<Self>()` byte stride for array iteration), and all generated getter/setter function pointers.
- Implements `ExposableComponent` and `FfiLayout`.

### 4.3. `crates/host_runtime` (Dynamic System Trait, Runtime Components & Parallel Execution)
- Depends on `bevy_ecs` / `bevy_app` (or `bevy`), `libloading`, and `abi_contract`.
- Implements:
  - `AbiModPlugin` & `ModHost`: Manages library loading, static `HostApi` handshake, process-wide per-inode reference-counted lifecycle locking (`SharedLibState`), and deterministic `bevy_abi_mod_shutdown` invocation on final unload before `dlclose`.
  - `ModRegistry`: Stores both compile-time `ModExposable` and runtime-registered components (`ComponentId`, `ComponentUuid`, name, size, alignment, VTable pointer, symbol resolver) and services `host_get_proc_address` with strict thread-local per-`World` symbol isolation (`CURRENT_REGISTRY`).
  - `host_register_component`: Validates layout invariants (non-empty UTF-8 name, power-of-two alignment, size a multiple of alignment and within `isize::MAX`), constructs a raw `ComponentDescriptor::new_with_layout` (`StorageType::Table`), registers or rebinds the `ComponentId` in Bevy's `World`, and wires optional foreign `ComponentDropFn` callbacks through a 64-slot static trampoline pool (`DROP_TRAMPOLINE_SLOTS`) guarded by active mod tombstone flags (`Vec<Arc<AtomicBool>>`).
  - `DynamicCSystem` (`impl bevy_ecs::system::System<In = (), Out = ()>`):
    - `System::initialize(&mut self, world: &mut World)`: resolves `ComponentId` for each requested static or runtime component and populates `FilteredAccessSet<ComponentId>` (`add_component_read` / `add_component_write`).
    - Archetype & Table Caching: tracks `ArchetypeGeneration` to incrementally update matched `TableId`s and `Access` without per-frame heap allocations (compatible with Bevy 0.16–0.20+ where `update_archetype_component_access` was unified into system execution/access updates).
    - `System::run_unsafe(&mut self, _input: (), world: UnsafeWorldCell)`: constructs `RawChunkIter` backed by `world`, the cached `TableId` list, and `world.change_tick()`; invokes the mod callback; isolates faults (`FfiResult::PANIC_CAUGHT` or caught unwind) by disabling the offending system.
    - `System::apply_deferred(&mut self, world: &mut World)`: flushes the internal `CommandQueue` (`CommandQueueBridge`).

### 4.4. `crates/guest_sdk` (Ergonomic Mod Author SDK)
- Lightweight, Bevy-independent Rust wrapper over `abi_contract`.
- Provides:
  - `export_mod!(init_fn, shutdown_fn)` macro with guest-side `catch_unwind` boundaries.
  - `register_runtime_component::<T>(host_api, host_context, c"Name")` with automatic layout extraction, deterministic `ComponentUuid::from_name`, and `core::mem::needs_drop::<T>()` drop-hook generation.
  - `GuestChunkIter`, `ChunkSlice::as_typed_slice::<T>()`, and `ChunkSliceMut::as_typed_slice::<T>()` with alignment, stride, and overflow verification for zero-copy SoA slice access.
  - `GuestCommandQueue::insert_runtime_component` for allocator-isolated deferred insertions.

### 4.5. Hot-Unloading, Tombstone Guards & Concurrency Safety
- Tracks loaded mods by `ModId` and coordinates OS shared-library images across concurrent `World`s via per-inode `SharedLibState` locks and active reference counts.
- Associates all systems and foreign drop trampolines registered by a mod instance with an `is_active: Arc<AtomicBool>` tombstone guard.
- When unloading or hot-reloading a shared library:
  1. Sets `is_active` to `false` (`Ordering::Release`), turning the mod's scheduled systems and drop hooks into safe no-ops without dangling jumps into unmapped `.text` pages, while preserving component data in Bevy's archetype tables for seamless hot-reload rebinding.
  2. Decrements the shared-library `active_count` under the per-image mutex and invokes `bevy_abi_mod_shutdown` only when the last active instance unloads (`1 -> 0`).
  3. Drops the `libloading::Library` handle under the per-image mutex.

---

## 5. Coding Standards & Rust Grounding Principles
- **Strict `unsafe` Hygiene:** `#![deny(unsafe_op_in_unsafe_fn)]` and `#![deny(improper_ctypes_definitions)]` across all crates, and inside generated macro code. Every `unsafe { ... }` block has a `// SAFETY:` justification.
- **Zero-Allocation Hot Path:** `DynamicCSystem::run_unsafe` and `iter_next_chunk` perform zero heap allocations during steady-state frame execution.
- **No Shared Allocator Assumptions:** Host frees host memory; guest frees guest memory.
- **Defensive Boundary Validation:** Null pointer checks, alignment debug assertions, self-aliasing overlap guards, UTF-8 validation on string setters, and read/write permission checks on column accessors.

---

## 6. Phased Implementation & Validation Gates

- **Phase 1: ABI Contract & Proc-Macro (`abi_contract` + `mod_derive`)**
  - Implement C-ABI types, `#[derive(ModExposable)]`, and `include/bevy_abi_mod.h`.
  - **Validation Gate 1:** Unit, aliasing, compile-fail, and C-interop tests verifying VTable layout, `size_of` stride, primitive/nested/String/Vec mutations, and defensive error handling without UB.
- **Phase 2: Host Runtime, Chunk Iterator & Change Detection (`host_runtime`)**
  - Implement `ModRegistry`, `RawChunkIter`, `RawCommandQueue`, and `DynamicCSystem`.
  - **Validation Gate 2:** Host integration test with 10,000 entities possessing `Position`, `Velocity`, and `NameTag(String)` verifying byte-stride updates, native `Changed<Position>` detection, and deferred command flushing.
- **Phase 3: Asymmetric `.dll` / `.so` / `.dylib` Compilation, Fault Isolation & Hot-Unload**
  - Build `tests/asymmetric_rust_mod` and `tests/asymmetric_c_mod`.
  - **Validation Gate 3:** Load both shared libraries into a headless Bevy App, run 100 multi-threaded ticks, verify fault isolation when a mod system panics, and hot-unload both libraries cleanly followed by 10 additional schedule ticks.
- **Phase 4: Runtime Mod Component Registration & Parallel Throughput Validation**
  - Expose runtime component descriptors (`ModComponentDescriptor`) across Rust (`guest_sdk`) and pure C99 (`include/bevy_abi_mod.h`) shared libraries without host compile-time knowledge.
  - **Validation Gate 4:** End-to-end multi-language integration tests, parallel `FilteredAccessSet` concurrency verification, deferred command burst & allocator-poisoning isolation tests, hot-unload/reload `ComponentId` rebinding & drop-trampoline safety tests, and 1k–100k entity benchmarks comparing runtime-registered components against static `ModExposable` components and native Bevy components.
