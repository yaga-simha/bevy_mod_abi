# `bevy_abi_mod_guest`

Safe (`#![forbid(unsafe_code)]`), **zero-Bevy-dependency** Rust SDK for authoring native `.cdylib` mods targeting [`bevy_abi_mod`](../bevy_abi_mod) (`ABI 0.2.0`).

## Features

- **`default = []`**: Core safe SDK with zero proc-macro dependencies.
- **`derive`**: Re-exports `#[derive(ModComponent, AbiPod)]` from [`bevy_abi_mod_macros`](../bevy_abi_mod_macros).

## Highlights

- **Per-World Isolation (`export_mod!` & `GuestPlugin`):** Allocates an independent `GuestInstance` per Bevy `World` on the guest heap—zero process-global mutable state.
- **Unified `GuestApp::system` Builder:** Declares up to 8 queries (`Table` slices, `SparseSet` row pointers, `With`/`Without`, `Added`/`Changed`), singleton `GuestRes<R>` / `GuestResMut<R>` with `is_added()` / `is_changed()` tick checks, schedules, `.in_set()` / `.before()` / `.after()` ordering (including `"host::<name>"`), `.main_thread()`, and `.run_if(...)` predicates.
- **Deferred Commands, Messages & Observers:** `GuestCommands` (batched `spawn`, `FfiChildOf`, `insert`, `insert_default`, `remove`, `despawn`, `trigger`), batch `Messages<M>` read/write, and dynamic `Event` / `EntityEvent` observers (`GuestApp::observe`).
- **Parallel Chunk Processing:** Work-stealing `SystemContext::par_chunks` / `ChunkIter::par_for_each` dispatched across host worker threads (`ComputeTaskPool`).
- **State-Preserving Hot Reload:** Optional `GuestPlugin::save_state` and `GuestPlugin::restore_state` byte-snapshot callbacks.
