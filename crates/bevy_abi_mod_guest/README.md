# `bevy_abi_mod_guest`

Safe (`#![forbid(unsafe_code)]`), **zero-Bevy-dependency** Rust SDK for authoring native `.cdylib` mods targeting [`bevy_abi_mod`](../bevy_abi_mod) (`ABI 0.3.0`, `v0.3.0`).

## Features

- **`default = []`**: Core safe SDK with zero proc-macro dependencies.
- **`derive`**: Re-exports `#[derive(ModComponent, AbiPod)]` from [`bevy_abi_mod_macros`](../bevy_abi_mod_macros).

## Highlights

- **Per-World Isolation (`export_mod!` & `GuestPlugin`):** Allocates an independent `GuestInstance` per Bevy `World` on the guest heap—zero process-global mutable state.
- **Full 18-Slot ABI `0.3.0` Coverage:** Safe wrappers for structured logging (`log_diagnostic`, `guest_log!`), fault reporting (`report_fault`), strict UUID resolution (`resolve_component_by_uuid`), layout/field inspection (`component_layout_info`, `verify_component_layout`), monotonic symbol interning (`intern_symbol`), persistent named objects (`bind_named_object`), non-destructive query counting (`ChunkIter::count`), ancestor cascades (`ChunkIter::up_cascade` / `TypedChunkIter::up_cascade`), zero-arena read-only parallel chunks (`par_for_each_chunk_readonly` / `par_chunks_readonly` with compile-time `Q::IS_READ_ONLY` enforcement), typed relationships (`RelationDescBuilder`, `register_relation`, `insert_relation`, `remove_relation`), decoupled scheduling (`SystemBuilder::register_handle`, `ScheduleSetBuilder`, `configure_schedule_set`, `mount_system`), RAII command transactions (`GuestTransaction` / `commands.transaction(...)`), and live schema migration (`migrate_component_column` / `migrate_pod_component_column`).
- **Unified `GuestApp::system` Builder & Expanded Typed Queries:** Declares up to 8 typed tuple queries with **1..=16 query terms** (`QueryTuple`) and tuple-generic **`Or<(T0..T7)>`** (2..=8 branches, preserving `Or2`/`Or3` aliases), `Allow<T>`, `Has<T>`, `RemovedComponents` cursors, singleton `GuestRes<R>` / `GuestResMut<R>` with `is_added()` / `is_changed()` ticks, schedules, `.in_set()` / `.before()` / `.after()` / `.after_weak()` / `.ambiguous_with*()` ordering (including `"host::<name>"`), `.main_thread()`, and `.run_if(...)` predicates.
- **Deferred Commands, Dynamic Components, Messages & Observers:** `GuestCommands` (`spawn`, `spawn_persistent`, `spawn_batch`, `FfiChildOf`, `insert`, `insert_if_new`, `insert_default`, `remove`, `despawn`, `despawn_all`, `trigger`, `trigger_ex`, `begin_transaction`, `insert_relation`, `remove_relation`), `entity_children`, batch `Messages<M>` read/write (POD and witness-backed non-POD), `GuestComponentOptions` (5 hooks, `clone`, `require`, `schema_fingerprint`, `BAM_FIELDS`), and global/`EntityEvent`/lifecycle/`Ready`/propagating `ObserverBuilder` observers.
- **Subsystem Bridges (`bridges`):** Ergonomic wrappers for Bevy standard types (`std_types`), `mod://<ns>/...` assets (`GuestAssetServer`), `States` (`GuestStateBridge`), reflection/BRP/settings (`reflect`), and BSN scenes (`GuestScene`).
- **Parallel Chunk Processing & Hot Reload:** Work-stealing `SystemContext::par_chunks` / `par_chunks_readonly` plus optional `GuestPlugin::save_state` / `restore_state` byte-snapshot callbacks.
