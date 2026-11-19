# `bevy_abi_mod_guest`

Safe (`#![forbid(unsafe_code)]`), **zero-Bevy-dependency** Rust SDK for authoring native `.cdylib` mods targeting [`bevy_abi_mod`](../bevy_abi_mod) (`ABI 0.3.0`).

## Features

- **`default = []`**: Core safe SDK with zero proc-macro dependencies.
- **`derive`**: Re-exports `#[derive(ModComponent, AbiPod)]` from [`bevy_abi_mod_macros`](../bevy_abi_mod_macros).

## Highlights

- **Per-World Isolation (`export_mod!` & `GuestPlugin`):** Allocates an independent `GuestInstance` per Bevy `World` on the guest heap—zero process-global mutable state.
- **Unified `GuestApp::system` Builder & Typed Queries:** Declares up to 8 typed tuple queries (`query_typed::<(&mut Pos, &Vel, With<A>, Without<B>, Allow<C>, Has<D>, Or2<...>)>()`), `RemovedComponents` cursors, singleton `GuestRes<R>` / `GuestResMut<R>` with `is_added()` / `is_changed()` tick checks, schedules, `.in_set()` / `.before()` / `.after()` / `.after_weak()` / `.ambiguous_with*()` ordering (including `"host::<name>"`), `.main_thread()`, and `.run_if(...)` predicates.
- **Deferred Commands, Dynamic Components, Messages & Observers:** `GuestCommands` (`spawn`, `spawn_persistent`, `spawn_batch`, `FfiChildOf`, `insert`, `insert_if_new`, `insert_default`, `remove`, `despawn`, `despawn_all`, `trigger`, `trigger_ex`), `entity_children`, batch `Messages<M>` read/write (POD and witness-backed non-POD), `GuestComponentOptions` (5 hooks, `clone`, `require`), and global/`EntityEvent`/lifecycle/`Ready`/propagating `ObserverBuilder` observers.
- **Subsystem Bridges (`bridges`):** Ergonomic wrappers for Bevy standard types (`std_types`), `mod://<ns>/...` assets (`GuestAssetServer`), `States` (`GuestStateBridge`), reflection/BRP/settings (`reflect`), and BSN scenes (`GuestScene`).
- **Parallel Chunk Processing & Hot Reload:** Work-stealing `SystemContext::par_chunks` / `ChunkIter::par_for_each` plus optional `GuestPlugin::save_state` / `restore_state` byte-snapshot callbacks.
