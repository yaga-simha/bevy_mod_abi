# `bevy_abi_mod_host_sys`

Low-level host FFI execution engine, registry, and dynamic library loader for [`bevy_abi_mod`](../bevy_abi_mod) (`ABI 0.3.0`).

Isolates all host-side `unsafe` code behind documented safety invariants (`#![deny(improper_ctypes_definitions, clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]`) so [`bevy_abi_mod_host`](../bevy_abi_mod_host) and [`bevy_abi_mod`](../bevy_abi_mod) enforce `#![forbid(unsafe_code)]`:

- **`HOST_API` (`BamHostApi`)**: 68-slot `extern "C"` function table populated for guest modules (including [`v03_ext`](./src/ffi/v03_ext.rs) handlers and reserved slots).
- **`ModRegistry`**: Generation-tracked copy-on-write (`Arc<RegistrySnapshot>`) handle, UUID, `"bam.v0.*"` symbol, field reflection, and VTable registry.
- **`GuestSystem`**: Implements Bevy's `System` trait over `BamSystemDesc`, populating `FilteredAccessSet<ComponentId>` on `UnsafeWorldCell` and integrating with Bevy `0.20+` schedules, `.in_set()` / `.before()` / `.after()` constraints, and `NON_SEND` main-thread pinning.
- **`HostIter` & `host_par_for_each_chunk`**: Zero-copy `Table` column slices, batched `SparseSet` indirect row pointers, non-destructive entity counts, resource change-detection ticks (`BamChangeTicks`), and parallel chunk dispatch.
- **`HostCommands` & `CommandArena`**: Per-system bump-allocated deferred command queue with LIFO checkpoints/rollback, applying spawns, inserts, removes, despawns, and triggers via `EntityWorldMut` (`ComponentHooks`, `#[require(...)]`, `ChildOf` / `Children`).
- **`DROP_PAGES`, `WorldDropGuard`, & `DynamicComponentHookMap`**: Wait-free `ArcSwapOption` paged drop trampolines (up to `4,096` slots) with two-condition reclamation plus 5-hook lifecycle & `clone_fn` dispatch preventing use-after-`dlclose`.
- **`bridge_helpers`**: Safe host-side witness field introspection/migration, typed `Reflect` helpers, and cross-`World` (`SubApp`) component/resource extraction.
- **`ModHost` & `LoadTransaction`**: Shadow-copy library loading (`LoadOptions`), optional `signature-verification` delegated to [`bevy_abi_mod_trust`](../bevy_abi_mod_trust) (`.sig`, `.bam_sig`, `.bampack`, CA/SSHKRL), transactional init rollback, deterministic 5-step unload, and state/witness hot reload (`save_state` / `restore_state`).

Application authors should depend on `bevy_abi_mod` rather than using this `-sys` crate directly.
