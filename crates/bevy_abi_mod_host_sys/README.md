# `bevy_abi_mod_host_sys`

Low-level host FFI execution engine, registry, and dynamic library loader for [`bevy_abi_mod`](../bevy_abi_mod) (`ABI 0.2.0`).

Isolates all host-side `unsafe` code behind audited safety invariants (`#![deny(improper_ctypes_definitions, clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]`) so [`bevy_abi_mod_host`](../bevy_abi_mod_host) and [`bevy_abi_mod`](../bevy_abi_mod) enforce `#![forbid(unsafe_code)]`:

- **`HOST_API` (`BamHostApi`)**: Static `extern "C"` function table populated for guest modules.
- **`ModRegistry`**: Generation-tracked copy-on-write (`Arc<RegistrySnapshot>`) handle, UUID, `"bam.v0.*"` symbol, and VTable registry.
- **`GuestSystem`**: Implements Bevy's `System` trait over `BamSystemDesc`, populating `FilteredAccessSet<ComponentId>` on `UnsafeWorldCell` and integrating with Bevy `0.20+` schedules, `.in_set()` / `.before()` / `.after()` constraints, and `NON_SEND` main-thread pinning.
- **`HostIter` & `host_par_for_each_chunk`**: Zero-copy `Table` column slices, batched `SparseSet` indirect row pointers, resource change-detection ticks (`BamChangeTicks`), and parallel chunk dispatch.
- **`HostCommands` & `CommandArena`**: Per-system bump-allocated deferred command queue applying spawns, inserts, removes, despawns, and triggers via `EntityWorldMut` (`ComponentHooks`, `#[require(...)]`, `ChildOf` / `Children`).
- **`DROP_SLOTS` & `WorldDropGuard`**: `ArcSwapOption` foreign component drop trampolines with two-condition reclamation preventing use-after-`dlclose`.
- **`ModHost` & `LoadTransaction`**: Shadow-copy library loading (`LoadOptions`), transactional init rollback, deterministic 5-step unload, and state-preserving hot reload (`save_state` / `restore_state`).

Application authors should depend on `bevy_abi_mod` rather than using this `-sys` crate directly.
