# Lifecycle, Quarantine & Shadow-Copy Hot Reload

Hot-reloading native machine code inside an active multi-threaded ECS requires careful orchestration of OS dynamic linkers, Bevy schedules, companion subsystems, and foreign component destructors.

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
2. Rolls back any staged `DROP_PAGES` foreign destructor bindings, `DynamicComponentHookMap` hooks, and `required_init_descs` initializers, returning newly allocated trampoline slots to the free list.
3. Restores the pre-load `Arc<RegistrySnapshot>` in `ModRegistry`.
4. Unmaps the library without calling `bevy_abi_mod_shutdown` (per `spec.md` §4.4, guests clean up partial state before returning `Err` from `init`).

---

## 3. Deterministic 5-Step Unload Sequence (`world.unload_mod(id)`)

When `world.unload_mod(id)` (or `app.unload_mod(id)`) runs under exclusive `&mut World` access:
1. **Schedule Excision:** Removes `ModSystemSet::Mod(id)`, owned `ModNamedSystemSet`s, and `ModScheduleSetRegistry` entries from Bevy `Schedules` (`ScheduleCleanupPolicy::RemoveSetAndSystems`).
2. **Companion `ModUnloadHooks`, Entity Cleanup & Drop-Trampoline Disarm:** Runs every companion teardown hook registered in `ModUnloadHooks` (detaching `.wesl` materials/mesh shaders, purging tracked retained UI nodes/stale `InputFocus`, reloading overridden `AssetOverlayPolicy` paths, clearing budget telemetry), despawns `ModScoped(id)` entities (preserving `cmd_spawn_persistent` and `bind_named_object` entities), releases mod-pinned asset/scene handles, clears `DynamicComponentHookMap` entries, and sets `is_active = false` on `DROP_PAGES` (`ArcSwapOption`) while **preserving** the Bevy `ComponentId` and persistent entity columns so a reloaded version of the mod seamlessly rebinds to live entities.
3. **Symbol & VTable Purge:** Publishes a new CoW `RegistrySnapshot` that nulls guest-owned `vtable_ptr`s and removes `"bam.v0.*"` symbols owned by `id`, preventing stale pointers into unmapped `.rodata`.
4. **Per-Instance Guest Shutdown:** Invokes `bevy_abi_mod_shutdown(instance)` while `.text` and `.rodata` are still mapped so the guest frees its closures and heap state on its own allocator.
5. **Library Unmap & Telemetry:** Drops `LoadedLibrary` (`dlclose` + shadow unlink), despawns the `LoadedModInfo` entity, and emits `Messages<ModUnloaded>`.

---

## 4. Zero-Copy Reload, Schema Column Migration & Live Watch (`world.reload_mod`)

`world.reload_mod(id)` (or `app.reload_mod` / `commands.reload_mod`, `bam/hot_reload` BRP, or live `ModHotReloadPlugin` paired with `cargo bam watch --out-dir`) hot-swaps a mod cleanly without dropping live entity state:
1. **Pre-Verification (`signature-verification`):** `reload_mod_with` / `reload_mod_from` run `preverify_mod_on_disk` and check principal affinity **before** unloading the running module (hosts can also call `evict_revoked_mods()` after updating a `SSHKRL` revocation list).
2. **Guest State Snapshot:** Calls the active guest's optional `GuestPlugin::save_state(&self) -> Result<Vec<u8>, FfiResult>` (`BamManifest::save_state`), streaming bytes across FFI via `BamStateWriterFn` into a host `Vec<u8>`.
3. **Zero-Copy Fingerprint Retention:** Dynamic `AbiPod` components stamped with a non-zero `SCHEMA_FINGERPRINT` whose fingerprint is unchanged across the reload retain their Bevy `ComponentId` and `BlobVec`/`SparseSet` column storage in place with **`0` bytes copied** (`0.117 ms` for `100,000` entities).
4. **Live Schema Migration (`migrate_component_column`):** When a dynamic `AbiPod` schema changes (`v1`→`v2`), the reloaded mod invokes `migrate_component_column` (`app.migrate_pod_component_column` in Rust) during `init` to transform all live active and `Disabled` rows via one contiguous aligned buffer per table (**`0` per-entity heap allocations**, atomic abort on error), while witness-backed non-POD structs migrate field-by-field automatically.
5. **Restore & Persistent Objects:** Interned `BamSymbol` handles (`BamSymbolTable`), persistent `ModNamedObjects` (`bind_named_object`), and `PersistentMessageCursors` remain valid across reloads, and `restore_state` receives the snapshotted byte blob immediately after `init`.

---

## 5. Observing Lifecycle, Fault & Budget `Messages`

```rust
use bevy_mod_abi::prelude::*;
use bevy_ecs::message::MessageReader;

fn monitor_mod_lifecycle(
    mut loaded: MessageReader<ModLoaded>,
    mut unloaded: MessageReader<ModUnloaded>,
    mut faulted: MessageReader<ModSystemFaulted>,
    mut breaches: MessageReader<ModBudgetBreach>,
) {
    for ev in loaded.read() {
        println!("Loaded {} v{} ({:?})", ev.name, ev.version, ev.id);
    }
    for ev in faulted.read() {
        eprintln!("Fault in {}: status={} err={:?}", ev.system_name, ev.status, ev.last_error);
    }
    for ev in breaches.read() {
        eprintln!("Budget breach in {:?}: {:?}", ev.mod_id, ev.kind);
    }
    for ev in unloaded.read() {
        println!("Unloaded {} (removed {} systems)", ev.name, ev.removed_systems);
    }
}
```
