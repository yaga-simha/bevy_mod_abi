# Specification: `bevy_abi_mod`
**Asymmetric, Zero-Serialization C-ABI Modding Framework for Bevy ECS**

---

## 1. Objective & Architectural Vision

Build a production-grade, high-performance native modding framework for **Bevy Engine (v0.20+)** that dynamically loads and unloads asymmetrically compiled shared libraries (`.dll` / `.so` / `.dylib`) at runtime.

**The Core Constraint:** The host game and guest mods MUST share **zero Rust ABI assumptions**. A mod compiled with a different `rustc` version, different optimization flags, a separate heap allocator, or written in C99/C++/Zig must be able to:
1. Resolve host component VTables at load time via a single symbol-resolver handshake (`GetProcAddressFn`).
2. Register native ECS systems into Bevy's multi-threaded schedules (`Update`, `FixedUpdate`, etc.) with full read/write access declarations (`FilteredAccessSet`) so Bevy's parallel executor runs them concurrently without data races.
3. Iterate over contiguous Bevy Archetype `Table` chunks at native hardware speed using opaque pointers, byte strides, and C-ABI VTable getters/setters—**with zero JSON/string serialization**.

---

## 2. Prior Art & Design Inspiration

* **Godot GDExtension (`gdextension_interface.h`):** Load-time initialization via a single passed `get_proc_address` function pointer that populates static function-pointer structs (`VTables`) inside the plugin, avoiding OS-specific `dlsym`/`GetProcAddress` symbol-export headaches.
* **Flecs C99 ECS API (`ecs_iter_t`):** Exposing contiguous Structure-of-Arrays (SoA) table columns to C callbacks via an iterator struct containing `count`, component column pointers, and byte strides.
* **Unreal Engine Gameplay Modules:** Clean lifecycle management (`StartupModule` / `ShutdownModule`) enabling deterministic teardown and hot-reloading without dangling vtable pointers.
* **Bevy Dynamic ECS Internals:** Utilizing `bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell`, `ComponentId`, `ArchetypeComponentId`, `FilteredAccessSet`, and `CommandQueue` to construct a custom dynamic `bevy_ecs::system::System` implementation.

---

## 3. Workspace Layout

```text
bevy_abi_mod/
├── Cargo.toml                  # Workspace definition
├── spec.md                     # Technical specification
├── gemini.md                   # Agent workflow guidelines & instructions
├── crates/
│   ├── abi_contract/           # Zero-dependency `#![no_std]` C-ABI header types (`#[repr(C)]` structs, function pointer types, status codes). NO `bevy` dependency.
│   ├── mod_derive/             # Proc-macro crate providing `#[derive(ModExposable)]` for host components.
│   ├── host_runtime/           # Bevy plugin (`AbiModPlugin`), VTable registry, `DynamicCSystem` (`impl System`), `ChunkIterator`, `libloading` lifecycle manager.
│   └── guest_sdk/              # Lightweight, Bevy-independent Rust wrapper for mod authors (wraps raw VTables and `ChunkIterator` into ergonomic safe iterators).
└── tests/
    ├── asymmetric_rust_mod/    # Standalone Cargo project (OWN Cargo.lock, NOT in workspace members) compiled to `.cdylib` during integration tests.
    └── asymmetric_c_mod/       # Pure C99 source file compiled via `cc` into a shared library to prove cross-language ABI stability.
```

---

## 4. Detailed Subsystem Requirements

### 4.1. `crates/abi_contract` (The Immutable C-ABI Boundary)
- Compiles with `#![no_std]`, `#![deny(unsafe_op_in_unsafe_fn)]`, and `#![deny(improper_ctypes_definitions)]`.
- Defines `#[repr(C)]` types:
  - `FfiEntity(pub u64)` (maps to/from `bevy_ecs::entity::Entity::to_bits()`) and `EntityId { index, generation }`.
  - `FfiSlice<T> { ptr: *const T, len: usize }` and `StrView { ptr: *const u8, len: usize }`.
  - `FfiResult` enum (`Ok = 0`, `InvalidComponent = 1`, `AccessConflict = 2`, `PanicCaught = 3`, `NullPointer = 4`, `InvalidUtf8 = 5`, `PermissionDenied = 6`, `InvalidVersion = 7`, `InternalError = 99`).
  - `GetProcAddressFn = unsafe extern "C-unwind" fn(name: *const c_char) -> *const c_void`.
  - `RawChunkIter`: opaque C handle passed to mod system callbacks, paired with host C-ABI iteration functions:
    - `iter_next_chunk(iter: *mut RawChunkIter) -> usize` (returns `0` when exhausted, or `entity_count` for the active archetype table).
    - `iter_column_readonly(iter: *const RawChunkIter, index: usize) -> *const u8`.
    - `iter_column_mutable(iter: *mut RawChunkIter, index: usize) -> *mut u8` (marks `ComponentTicks` as changed for all entities in the chunk, or returns `null_mut()` if column is read-only).
    - `iter_mark_changed(iter: *mut RawChunkIter, index: usize, row: usize) -> FfiResult`.
  - `RawCommandQueue`: opaque C handle for deferred entity/component commands.
  - `ExposableComponent` trait exposing `COMPONENT_NAME`, `UUID`, `VTABLE`, `size()`, `align()`, and `resolve_symbol()`.

### 4.2. `crates/mod_derive` (`#[derive(ModExposable)]`)
For any Bevy `#[derive(Component, ModExposable)]` struct (which remains standard `#[repr(Rust)]`):
- Generates `#[no_mangle] pub unsafe extern "C-unwind"` Accessors:
  - **Primitives (`f32`, `i32`, `u64`, `bool`, etc.):** `<Struct>_get_<field>(ptr: *const c_void) -> <Type>` and `<Struct>_set_<field>(ptr: *mut c_void, val: <Type>)`.
  - **Nested Compound Types (`Vec3`, sub-structs):** `<Struct>_ptr_<field>(ptr: *const c_void) -> *const c_void` and `<Struct>_ptr_mut_<field>(ptr: *mut c_void) -> *mut c_void`.
  - **Heap-Allocated Fields (`String`, `Vec<T>`):** Never passes owned Rust `String` or `Vec` across the boundary. Generates host-allocated mutators:
    - `<Struct>_get_<string_field>(ptr: *const c_void) -> FfiSlice<u8>`
    - `<Struct>_set_<string_field>(ptr: *mut c_void, utf8_ptr: *const u8, len: usize)` (validates null pointers and UTF-8 validity; host allocates/resizes its own `String` and copies bytes).
- Generates `#[repr(C)] pub struct <Struct>VTable` containing `size: usize`, `align: usize` (exact `core::mem::size_of::<Self>()` byte stride for array iteration), and all generated getter/setter function pointers.
- Implements `ExposableComponent` registering the component's string name, UUID, static VTable pointer, and symbol lookup table.

### 4.3. `crates/host_runtime` (Dynamic System Trait & Parallel Execution)
- Implements `ModRegistry`, `RawChunkIter`, `RawCommandQueue`, and `DynamicCSystem` (`impl bevy_ecs::system::System<In = (), Out = ()>`):
  - `System::initialize(&mut self, world: &mut World)`: resolves `ComponentId` for each requested component name and populates `FilteredAccessSet<ComponentId>`.
  - `System::update_archetype_component_access(&mut self, world: UnsafeWorldCell)`: caches matched `TableId`s and updates `Access<ArchetypeComponentId>`.
  - `System::run_unsafe(&mut self, _input: (), world: UnsafeWorldCell)`: constructs `RawChunkIter` with zero heap allocations, wraps `.dll` callback in `std::panic::catch_unwind`, and marks faulted mods disabled if a panic occurs.
  - `System::apply_deferred(&mut self, world: &mut World)`: flushes the internal `CommandQueue`.

### 4.4. Hot-Unloading & Lifecycle Safety
- Tracks loaded mods by `ModId`.
- Associates all systems registered by a mod with a dedicated `SystemSet` or run-condition flag.
- On unload/hot-reload: disables/removes systems, invokes `bevy_mod_shutdown()`, and drops `libloading::Library`.

---

## 5. Coding Standards & Rust Grounding Principles
- **Strict `unsafe` Hygiene:** `#![deny(unsafe_op_in_unsafe_fn)]` and `#![deny(improper_ctypes_definitions)]` across all crates, with `// SAFETY:` comments on every `unsafe` block.
- **Zero-Allocation Hot Path:** `DynamicCSystem::run_unsafe` and `iter_next_chunk` perform zero heap allocations during steady-state execution.
- **No Shared Allocator Assumptions:** Host frees host memory; guest frees guest memory.
- **Defensive Boundary Validation:** Null pointer checks, UTF-8 validation on string setters, and read/write permission checks on column accessors.

---

## 6. Phased Implementation & Validation Gates
- **Phase 1 (Complete):** ABI Contract & Proc-Macro (`abi_contract` + `mod_derive`) — validated by `crates/mod_derive/tests/validation_gate_1.rs`.
- **Phase 2:** Host Runtime, Chunk Iterator & Change Detection (`host_runtime`) — 10,000 entities integration test with `Position`, `Velocity`, `NameTag(String)`, `Changed<Position>`, and deferred command flushing.
- **Phase 3:** Asymmetric `.dll` / `.so` / `.dylib` Compilation, Fault Isolation & Hot-Unload (`tests/asymmetric_rust_mod` + `tests/asymmetric_c_mod`).
