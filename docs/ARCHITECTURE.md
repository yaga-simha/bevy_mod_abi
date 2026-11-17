# Architecture & Feature Implementation Summary (`ABI 0.2.0`)

This document summarizes how every subsystem and capability in **`bevy_abi_mod`** is built across the workspace. Wire-level byte layouts are specified in [`spec.md`](../spec.md).

---

## 1. Layered Workspace (`-sys` vs. `#![forbid(unsafe_code)]`)

| Crate | Safety Boundary | `bevy*` Deps | How It Fits |
|---|---|---|---|
| `bevy_abi_mod_sys` | `#![no_std]`, zero external deps | Never | Single source of truth for `ABI 0.2.0` `#[repr(C)]` descriptors, primitives, traits (`ModComponent`, `AbiPod`, `RawInsertable`), and [`include/bevy_abi_mod.h`](../include/bevy_abi_mod.h) generation. |
| `bevy_abi_mod_macros` | `#![forbid(unsafe_code)]` (codegen host) | Never | `#[derive(ModComponent, AbiPod)]` hygienic VTable, field accessor, schema hash, and C99/Zig declaration emitters. |
| `bevy_abi_mod_host_sys` | Isolates all host `unsafe` | Yes | Implements `HOST_API` (`extern "C"` entry points), `HostIter`, `CommandArena` / `HostCommands`, `GuestSystem`, CoW `ModRegistry`, `ModHost`, `LoadTransaction`, and `DROP_SLOTS` / `HOOK_SLOTS`. |
| `bevy_abi_mod_host` | **`#![forbid(unsafe_code)]`** | Yes | Public Bevy plugin (`AbiModPlugin`), `ModAppExt`, `ModWorldExt`, `ModCommandsExt`, `LoadOptions`, `ComponentOptions`, `ModVisibleSets`, `ModQuarantine`, and lifecycle `Message`s. |
| `bevy_abi_mod` | **`#![forbid(unsafe_code)]`** | Yes | Top-level Bevy host facade re-exporting `bevy_abi_mod_host`, derive macros, `bevy_abi_mod_sys`, and `prelude`. |
| `bevy_abi_mod_guest_sys` | Isolates all guest `unsafe` | Never | Per-`World` `GuestInstance`, panic containment wrappers, `extern "C"` trampolines, validated column slices/rows, and borrow guards. |
| `bevy_abi_mod_guest` | **`#![forbid(unsafe_code)]`** | Never | Idiomatic Rust guest SDK (`GuestPlugin`, `GuestApp`, `SystemBuilder`, `ChunkIter`, `Chunk`, `GuestRes`, `GuestCommands`, `export_mod!`). |

---

## 2. How Core Features Are Built

### 2.1 Extensible Wire Contract & Single-Source Polyglot Header
- Every extensible `#[repr(C)]` struct (`BamManifest`, 38-slot `BamHostApi`, `BamSystemDesc`, `BamObserverDesc`, `BamQueryDesc`, `BamTermDesc`, `ComponentDesc`, `BamColumnInfo`, `VTableHeader`) begins with `struct_size: u32` and freezes `MIN_V0_2_SIZE` via `core::mem::offset_of!`.
- Readers (`read_extensible_struct` / `read_descriptor` on Rust, `bam_host_api_copy_bounded` in C99) zero-extend older callers and truncate unknown trailing fields; optional callbacks use `Option<unsafe extern "C" fn(...)>` niche optimization, and unknown `flags` / non-zero `reserved0` fields are rejected synchronously.
- `cargo xtask header` emits [`include/bevy_abi_mod.h`](../include/bevy_abi_mod.h) directly from Rust metadata with compile-time 64-bit and 32-bit `BAM_STATIC_ASSERT` layout guards; `cargo xtask gen-component-headers` emits component C headers via `ModComponent::C_DECL`.

### 2.2 Hygienic `#[derive(ModComponent, AbiPod)]` VTables & Overlap Safety
- `#[derive(AbiPod)]` statically asserts cross-ABI safety (`#[repr(C)]` and every field implements `AbiPod`, excluding `bool`, `char`, `u128`/`i128`, `usize`/`isize`, and Rust enums). `#[derive(ModComponent)]` is independent of `bevy_ecs::component::Component` and supports non-POD types (`String`, `Vec<T>`, `#[mod_component(opaque)]`).
- Macro-generated VTables prepend `VTableHeader` (`struct_size`, `abi_version`, `uuid`, `size`, `align`), place generated `extern "C"` shims inside anonymous `const _: ()` scopes using `proc-macro-crate` resolved paths, return `FfiResult` from every setter, and copy overlapping `StrView`/`FfiSlice<T>` slices through temporary host buffers before mutating `String`/`Vec` fields in place.

### 2.3 Pre-Resolved `BamComponent` Handles, `SipHash-2-4-128` UUIDs & CoW `ModRegistry`
- Registered types use canonical `"<namespace>::<Name>"` strings hashed via streaming `const` `SipHash-2-4-128` over name, size, alignment, and field schema fingerprints (`ComponentUuid`, mirrored by `bam_uuid_from_parts` in C99). Unqualified dynamic names from guests are auto-prefixed with `"<manifest.name>::<Name>"`.
- `resolve_component` runs once during `bevy_abi_mod_init` and returns a 1-based `BamComponent(u32)` handle mapped onto `RegistrySnapshot::entries[(h - 1) as usize]` via O(1) bounds-checked lookup. `ModRegistry` maintains a generation-countered CoW `Arc<RegistrySnapshot>` so `GuestSystem` refreshes only when `generation` changes.

### 2.4 Zero-Copy SoA Chunk Iteration, Tick Skipping, Parallel Chunks & Random Entity Access
- `HostIter` walks `QueryState::matched_tables()` (or `matched_archetypes()` when sparse columns are queried) over `UnsafeWorldCell`:
  - Dense `Table` columns expose contiguous byte/typed slices (`&[T]` / `&mut [T]`) with zero copies; `SparseSet` columns batch entire archetype runs into reusable `*const u8` / `*mut u8` pointer arrays (`iter_column_rows_readonly` / `iter_column_rows_mutable`).
  - `CHANGED` and `ADDED` terms resolve `ComponentTicks` slices once per table and skip whole tables whose summary tick predates `last_run`. Mutable writes update per-chunk `marked_changed_mask` bitmasks at most once per chunk, alongside untracked mutable slices paired with `iter_mark_changed_range`.
  - `par_for_each_chunk` fans chunk slices across Bevy's `ComputeTaskPool` (`scope`) with per-task panic containment and non-reentrant `ACCESS_CONFLICT` guards.
  - `sys_entity_get` / `sys_entity_get_mut` provide random per-entity lookup strictly constrained by the system's declared `FilteredAccessSet` and query filters.

### 2.5 `EntityWorldMut` Deferred Commands (`CommandArena`, Hooks, Required Components & Hierarchy)
- Each `GuestSystem` owns a reusable bump-allocated `CommandArena` (`MaybeUninit<u8>` slabs) and flat `RecordedCommand` tape, copying raw `RawInsertable` payloads via `ptr::copy_nonoverlapping` so padding bytes never form `&[u8]` slices and steady-state frames allocate zero heap memory after warmup.
- `apply_to_world` reserves entities upfront via `Entities::reserve_entity()` and flushes spawn/insert/remove/despawn operations through Bevy `EntityWorldMut` (`insert_by_id` / `insert_by_ids` / `remove_by_id`). This guarantees native `ComponentHooks`, `#[require(...)]` Required Components, and `ChildOf` / `Children` hierarchy synchronization (`BAM_CHILD_OF_NAME` / `FfiChildOf`) execute automatically.

### 2.6 Resource Ticks, Buffered `Messages<M>` & `EntityEvent` Observers
- Pre-resolved `AccessMode::RESOURCE_READ` / `RESOURCE_WRITE` terms register resource locks on `FilteredAccessSet` and expose `res_ptr`, `res_mut_ptr`, and `res_ticks` (`BamChangeTicks`, typed `GuestRes<R>` / `GuestResMut<R>`) with proper Bevy tick change tracking.
- Buffered `Messages<M>` (`register_mod_message::<M>()` with `MESSAGE_READ` / `MESSAGE_WRITE`) maintain independent per-system `MessageCursor<M>` offsets supporting single (`msg_send`) and batched (`msg_read_batch` / `msg_write_batch`) transfers.
- Global `Event`s (`register_mod_event`) and entity-targeted `EntityEvent`s (`register_mod_entity_event`) support deferred `cmd_trigger` emission (`world.trigger()`) and dynamic observer callbacks (`register_observer` / `BamObserverDesc`) bound to either `BAM_ENTITY_NULL` or a specific entity and cleaned up on mod unload.

### 2.7 Deterministic Bevy 0.20 Scheduling & Host-Visible System Sets
- `BamSystemDesc` specifies `ScheduleId` (`STARTUP`, `FIXED_UPDATE`, `UPDATE`, `POST_UPDATE`, or host-mapped custom schedules `>= CUSTOM_BASE`), `.in_set()` membership, and `.before()` / `.after()` constraints resolving within-mod (`"sys"`), across mods (`"other_mod::sys"`), and against host-exported `SystemSet`s (`ModVisibleSets` via `"host::<name>"`), eliminating non-determinism under Bevy 0.20 schedule randomization.
- Systems join `ModSystemSet(mod_id)` and `ModNamedSystemSet { mod_name, system_name }` (cleanly removed on unload while host sets stay intact). Runtime-loaded `Startup` systems execute once immediately if `StartupCompleted` is already set.

### 2.8 Runtime Dynamic Components, Lock-Free Drop/Hook Trampolines & Transactional Rollback
- Guests define private `#[repr(C)]` ECS components at runtime via `register_component` backed by `ComponentDescriptor::new_with_layout` (`name`, `size`, `align`, optional `drop_fn`, optional `on_add` / `on_remove` hooks, `SPARSE_SET` / `IMMUTABLE` flags).
- Foreign destructors and hooks bind into 1,024-slot monomorphized `ArcSwapOption` trampoline tables (`DROP_SLOTS` / `HOOK_SLOTS`). `WorldDropGuard` disarms slots when a mod unloads or a `World` drops, reclaiming slots to `FREE_LIST` only when both `SLOT_CLOSING` and `live_values == 0` hold (Fenced `SeqCst`), while `LoadTransaction` rolls back registrations if `bevy_abi_mod_init` or strict ordering validation fails before `dlclose`.

### 2.9 Per-World Isolation, Hot Reload State Persistence, Panic Policy & Diagnostics
- Each load session creates an isolated `BamInstance*` (`bevy_abi_mod_init` / `bevy_abi_mod_shutdown`), optionally shadow-copies shared libraries (`LoadOptions::shadow_copy`) to avoid Windows DLL locks and macOS code-signing invalidation, and transfers serialized guest state across `reload_mod` via `BamManifest::save_state` / `restore_state`.
- All FFI function pointers are `extern "C"` (`nounwind`, zero host `catch_unwind`). Default `panic = "unwind"` Rust guests catch panics in `bevy_abi_mod_guest_sys` (`FfiResult::PANIC_CAUGHT`), discard uncommitted commands, emit `ModSystemFaulted`, and apply `ModErrorPolicy` (`Quarantine` / `Log` / `ReportToBevy`), while `panic = "abort"` guests abort deterministically at the panic site.
- Every `GuestSystem` records `tracing::debug_span!("bam_guest_system", mod_name, system, chunks, entities, commands)` metrics and optionally publishes `bevy_diagnostic::DiagnosticsStore` counters under the `diagnostics` feature.
