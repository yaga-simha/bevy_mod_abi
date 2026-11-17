# Lifecycle, Quarantine & Shadow-Copy Hot Reload

Hot-reloading native machine code inside an active multi-threaded ECS requires careful orchestration of OS dynamic linkers, Bevy schedules, and foreign component destructors.

---

## 1. Why Core Shadow Copying Matters (`LoadOptions::shadow_copy`)

Rebuilding a shared library at the same file path while the game is running hits three OS-level traps:
1. **POSIX Inode / Path Caching:** `dlopen("mods/libfoo.so")` returns a cached handle to the old in-memory image if the path or inode matches an already-loaded library.
2. **Windows File Locking:** `LoadLibraryW` locks both the `.dll` and its `.pdb` debug symbols on disk, causing `cargo build` or `clang` to fail with `Access Denied`.
3. **macOS Apple Silicon Code-Signing `SIGKILL`:** Overwriting a mapped Mach-O `.dylib` in place (`cp` over an active inode) invalidates kernel page-table code-signature hashes and immediately kills the host process with `SIGKILL` (`CODESIGNING` fault).

Enabling `LoadOptions { shadow_copy: true, shadow_dir: None, .. }` solves all three in the core loader:
- Each load copies `<stem>.<ext>` to a unique `<shadow_dir>/<stem>.<pid>.<seq>.<ext>` file before invoking `dlopen` / `LoadLibraryW`.
- On unload, `LoadedLibrary` drops `libloading::Library` (`dlclose` / `FreeLibrary`) **strictly before** `ShadowFileGuard` unlinks the temporary shadow file.

---

## 2. Transactional Init Rollback (`LoadTransaction`)

If `bevy_abi_mod_init` returns a non-`OK` `FfiResult`—or if `LoadOptions::strict == true` rejects an unresolved schedule or ordering target—`LoadTransaction::drop` atomically:
1. Marks the session's `is_active` tombstone `false` (`Ordering::Release`).
2. Rolls back any staged `DROP_SLOTS` foreign destructor bindings and returns newly allocated trampoline slots to the free list.
3. Restores the pre-load `Arc<RegistrySnapshot>` in `ModRegistry`.
4. Unmaps the library without calling `bevy_abi_mod_shutdown` (per `spec.md` §4.4, guests clean up partial state before returning `Err` from `init`).

---

## 3. Deterministic 5-Step Unload Sequence (`world.unload_mod(id)`)

When `world.unload_mod(id)` (or `app.unload_mod(id)`) runs under exclusive `&mut World` access:
1. **Schedule Excision:** Removes `ModSystemSet(id)` and every `ModNamedSystemSet` owned by `id` from Bevy `Schedules` (`ScheduleCleanupPolicy::RemoveSetAndSystems`).
2. **Drop-Trampoline Disarm:** Sets `is_active = false` and prunes `id`'s foreign `ComponentDropFn` pointer from `DROP_SLOTS` (`ArcSwapOption`), while **preserving** the Bevy `ComponentId` and existing entity table columns so a reloaded version of the mod seamlessly rebinds to live entities!
3. **Symbol & VTable Purge:** Publishes a new CoW `RegistrySnapshot` that nulls guest-owned `vtable_ptr`s and removes `"bam.v0.*"` symbols owned by `id`, preventing stale pointers into unmapped `.rodata`.
4. **Per-Instance Guest Shutdown:** Invokes `bevy_abi_mod_shutdown(instance)` while `.text` and `.rodata` are still mapped so the guest frees its closures and heap state on its own allocator.
5. **Library Unmap & Telemetry:** Drops `LoadedLibrary` (`dlclose` + shadow unlink), despawns the `LoadedModInfo` entity, and emits `Messages<ModUnloaded>`.

---

## 4. Observing Lifecycle & Fault `Messages`

```rust
use bevy_abi_mod::prelude::*;
use bevy_ecs::message::MessageReader;

fn monitor_mod_lifecycle(
    mut loaded: MessageReader<ModLoaded>,
    mut unloaded: MessageReader<ModUnloaded>,
    mut faulted: MessageReader<ModSystemFaulted>,
) {
    for ev in loaded.read() {
        println!("Loaded {} v{} ({:?})", ev.name, ev.version, ev.id);
    }
    for ev in faulted.read() {
        eprintln!("Fault in {}: status={}", ev.system_name, ev.status);
    }
    for ev in unloaded.read() {
        println!("Unloaded {} (removed {} systems)", ev.name, ev.removed_systems);
    }
}
```
