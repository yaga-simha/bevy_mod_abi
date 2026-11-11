# ADR 0005: Per-World `BamInstance` Lifecycle, Shadow-Copy Hot-Reload, Exclusive `&mut World` Unload, and Scoped System Sets

- **Status:** Accepted
- **Date:** 2026-11-11
- **Tracking Items:** `L-02`, `L-03`, `L-04`, `L-05` (cross-references: `L-07`, `L-08`, `S-05`, `S-06`, `T-03`)
- **Target Crates:** `bevy_abi_mod_sys`, `bevy_abi_mod_host_sys`, `bevy_abi_mod_host`, `bevy_abi_mod_guest_sys`, `bevy_abi_mod_guest`

---

## 1. Context & Problem Statement

In v0.1, mod loading, instance storage, hot-reload file handling, unloading, and system-set ordering each assumed a single `World` loading each `.cdylib` at most once:

1. **Process-global guest state (`L-02`):** `guest_sdk` stored every boxed system closure, `run_if` predicate, and `GuestPlugin` inside a process-global `static GUEST_ALLOCATIONS: Mutex<Vec<GuestAllocation>>`, and `ModHost` called `bevy_abi_mod_shutdown()` (taking zero arguments) **only** when `SharedLibState`'s OS library refcount reached zero. Loading the same `.cdylib` into two Bevy `World`s (e.g., dedicated server + client preview, or parallel integration tests `T-03`) entangled both Worlds' closures in a single `Vec` and broke per-World teardown.
2. **File locking & `dlopen` inode caching on hot-reload (`L-03`):** Only `examples/hot_reload_demo` copied `.dll`/`.dylib`/`.so` files before loading. Core `ModHost::load_mod` opened the compiler output path directly — locking the `.dll` against `cargo build` on Windows (`ERROR_SHARING_VIOLATION`) and returning the stale cached image on macOS/Linux when `dlopen` matched an already-mapped `(dev, inode)` or path.
3. **Unsound partial unload via `&mut ModHost` (`L-04`, `S-05`):** `ModHost::unload_mod(&mut self, id)` was `pub` (as were fields of `LoadedMod`). Calling `mod_host.unload_mod(id)` directly from a system with `ResMut<ModHost>` unmapped the shared library (`dlclose`) while its `DynamicCSystem` nodes remained live in Bevy's `Schedules` and its `vtable_ptr` remained live in `ModRegistry` (`S-05`) — causing an immediate segfault on the next frame!
4. **Global, conditional, leaked `ModNamedSystemSet` (`L-05`):**
   - `ModNamedSystemSet(String)` shared a flat global namespace across all mods (two mods' `"movement"` systems silently coupled).
   - Systems were added to `ModNamedSystemSet` **only** if `has_ordering` was true within the same load batch, so a later mod declaring `.after("movement")` silently ordered against an empty set.
   - Named sets were never removed on unload, leaking schedule graph nodes on every hot-reload.

---

## 2. Decision

### 2.1 Symmetric Per-World `BamInstance` Init & Shutdown (`L-02`)

#### Wire Contract (`bevy_abi_mod_sys` & `include/bevy_abi_mod.h`)

```rust
#[repr(C)]
pub struct BamInstance {
    _private: [u8; 0],
}

pub type BamInitFn = unsafe extern "C" fn(
    api: *const BamHostApi,
    load_ctx: *mut BamLoadCtx,
    instance_out: *mut *mut BamInstance,
) -> FfiResult;

pub type BamShutdownFn = unsafe extern "C" fn(instance: *mut BamInstance);
```

```c
typedef struct BamInstance BamInstance;

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void);
BAM_EXPORT BamResult          bevy_abi_mod_init(
    const BamHostApi* api,
    BamLoadCtx*       load_ctx,
    BamInstance**     instance_out
);
BAM_EXPORT void               bevy_abi_mod_shutdown(BamInstance* instance);
```

#### Ownership & Allocator Isolation Invariants (`Rule 2`)

1. **`bevy_abi_mod_init`:**
   - Validates `!api.is_null()`, `!load_ctx.is_null()`, and `!instance_out.is_null()`, initializing `*instance_out = core::ptr::null_mut()` up front.
   - Allocates a fresh per-World `GuestInstance` on the **guest heap**:

```rust
// crates/bevy_abi_mod_guest_sys/src/lifecycle.rs
pub struct GuestInstance {
    pub api: BamHostApi,
    pub host_ctx: *const BamHostCtx,
    /// Boxed system closures (`user_data`), `run_if` closures (`run_if_user_data`),
    /// and owned CString descriptor buffers whose pointers were handed to this World's `register_system`.
    pub allocations: Vec<Box<dyn std::any::Any + Send>>,
    /// Type-erased `GuestPlugin` instance so `on_shutdown(&mut self)` runs per-World.
    pub plugin_shutdown: Option<Box<dyn FnOnce() + Send>>,
}
```

   - Runs the user's `GuestPlugin::build(&mut app)` inside `catch_guest_result` (`ADR 0004`).
   - **Success (`FfiResult::OK`):** Writes `Box::into_raw(Box::new(guest_instance)).cast::<BamInstance>()` into `*instance_out` and returns `FfiResult::OK`. (C99 stateless mods may leave `*instance_out = NULL` and return `BAM_RESULT_OK`; `bevy_abi_mod_shutdown(NULL)` is a defined no-op).
   - **Failure (`res != FfiResult::OK`):** Drops `guest_instance` **immediately inside `bevy_abi_mod_init`** (freeing any closures allocated before the failure on the guest heap), leaves `*instance_out = NULL`, and returns `res`. Host `LoadTransaction` (`ADR 0001`) simultaneously rolls back any staged host registrations without invoking `bevy_abi_mod_shutdown`.
2. **`bevy_abi_mod_shutdown`:**
   - Invoked unconditionally by the host **once per successful `bevy_abi_mod_init`** when that `ModId` unloads from its `World` (or when that `World` drops), **strictly after** all `GuestSystem` nodes and `DROP_SLOTS` bindings for that `ModId` have been removed/disarmed and **strictly before** `libloading::Library` drops.
   - Inside `bevy_abi_mod_guest_sys`, `if !instance.is_null()`, reconstructs `let mut inst = unsafe { Box::from_raw(instance.cast::<GuestInstance>()) };`, runs `inst.plugin_shutdown.take()` inside `catch_guest_unit`, and drops `inst.allocations` on the guest heap.
   - `GUEST_ALLOCATIONS` is **completely deleted** from the guest crates.
3. **OS `dlopen` Serialization (`SharedLibRegistry`):**
   - A lightweight `Mutex<HashMap<PathBuf, Weak<OsLibraryHandle>>>` serializes OS-level `Library::new` / `Library::drop` calls across threads while keeping `BamInstance` 100% per-`World`.

---

### 2.2 Core Loader Shadow-Copy Hot-Reload (`L-03`)

Shadow copying moves out of demo code directly into `bevy_abi_mod_host_sys`'s loader, controlled by `LoadOptions`:

```rust
// crates/bevy_abi_mod_host_sys/src/loader/options.rs (re-exported in bevy_abi_mod_host)
#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// Copy the shared library to a unique temporary path (`<stem>.<pid>.<seq>.<ext>`)
    /// before `dlopen` / `LoadLibraryW`.
    ///
    /// Defaults to `cfg!(debug_assertions)` (`true` in dev/test builds, `false` in release builds).
    pub shadow_copy: bool,
    /// Optional directory for shadow copies. Defaults to `std::env::temp_dir().join("bevy_abi_mod")`.
    pub shadow_dir: Option<std::path::PathBuf>,
    /// Reject mods that register neither components nor systems (`ModLoadError::EmptyMod`),
    /// and treat unresolved `before`/`after` ordering targets as hard errors (`L-05`).
    pub strict: bool,
    /// Optional default schedule override applied when a system specifies `ScheduleId::UPDATE` (`0`).
    pub schedule_override: Option<ScheduleId>,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            shadow_copy: cfg!(debug_assertions),
            shadow_dir: None,
            strict: false,
            schedule_override: None,
        }
    }
}
```

#### Shadow File Lifecycle & Drop-Order Invariant
1. When `options.shadow_copy == true`:
   - Generate unique path `<shadow_dir>/<file_stem>.<pid>.<seq>.<ext>` (`static SHADOW_SEQ: AtomicU64 = AtomicU64::new(1)`).
   - Create `shadow_dir` if absent and copy bytes atomically (`std::fs::copy(source_path, &shadow_path)` -> `ModLoadError::ShadowCopyFailed { source, shadow, err }`).
   - On macOS (`target_os = "macos"`), preserve ad-hoc code signature validity by writing to a fresh file inode (never truncating an existing mapped Mach-O file in place, which triggers `SIGKILL / CODESIGNING` kernel page invalidation on Apple Silicon!).
2. **RAII Cleanup (`LoadedLibrary` field ordering):**

```rust
pub(crate) struct ShadowFileGuard(PathBuf);

impl Drop for ShadowFileGuard {
    fn drop(&mut self) {
        // Best-effort unlink; on Windows `FreeLibrary` MUST have completed first.
        let _ = std::fs::remove_file(&self.0);
    }
}

pub(crate) struct LoadedLibrary {
    /// Declared FIRST so Rust drops `library` (`dlclose` / `FreeLibrary`) BEFORE `shadow_guard`!
    library: libloading::Library,
    shadow_guard: Option<ShadowFileGuard>,
}
```

Because Rust guarantees struct fields drop in declaration order, `libloading::Library` unmaps the OS handle **before** `ShadowFileGuard::drop` unlinks the file — both on normal unload (`World::unload_mod`), on `World::drop`, and on `LoadTransaction` rollback after a failed `init`!

---

### 2.3 Exclusive `&mut World` Load/Unload Enforcement & Symbol Cleanup (`L-04`, `S-05`)

#### Encapsulation Boundary
1. `ModHost` methods that mutate loaded mods (`load_internal`, `unload_internal`, `reload_internal`) are **`pub(crate)` inside `bevy_abi_mod_host_sys`** — never `pub`. Public methods on `ModHost` are strictly read-only inspection (`loaded_mods(&self) -> impl Iterator<Item = &LoadedModInfo>`, `is_loaded(&self, id: ModId) -> bool`).
2. `LoadedMod` is `pub(crate)` with private fields (`instance`, `library`, `registered_sets`, `registered_symbols`, `drop_slots`); public callers inspect immutable `LoadedModInfo { id: ModId, name: Arc<str>, version: Arc<str>, source_path: PathBuf }`.
3. Public load/unload/reload entry points exist **only** where exclusive `&mut World` access is statically proven by Rust's borrow checker (`#![forbid(unsafe_code)]` in `bevy_abi_mod_host`):
   - **`ModWorldExt` (on `bevy_ecs::world::World`):**
     - `world.load_mod(path) -> Result<ModId, ModLoadError>`
     - `world.load_mod_with(path, options) -> Result<ModId, ModLoadError>`
     - `world.unload_mod(id) -> Result<LoadedModInfo, ModLoadError>`
     - `world.reload_mod(id) -> Result<ModId, ModLoadError>` (unloads `id` and reloads `source_path` with its stored `LoadOptions`)
   - **`ModAppExt` (on `bevy_app::App`):** Delegates directly to `self.world_mut()`.
   - **`ModCommandsExt` (on `bevy_ecs::system::Commands`):** `commands.queue_load_mod(...)`, `commands.queue_unload_mod(id)`, `commands.queue_reload_mod(id)` — enqueues an exclusive `Command` (`fn apply(self, world: &mut World)`) executed safely between schedules, emitting `ModLoaded` / `ModUnloaded` (or logging / emitting a load-error message).

#### Atomic 6-Step Unload Sequence (`unload_internal(world: &mut World, id: ModId)`)
Because `&mut World` proves zero systems are currently executing on `world`:
1. **Remove Schedules & Sets:** For every `(schedule_id, set)` recorded for `id`, remove `ModSystemSet(id)` and every owned `ModNamedSystemSet` from `Schedules` via `schedule.remove_systems_in_set(set, world, ScheduleCleanupPolicy::RemoveSetAndSystems)`.
2. **Disarm Drop Trampolines (`ADR 0001`):** Flip `entry.is_active.store(false, Ordering::Release)` and publish pruned `DropSlotSnapshot`s without `id`.
3. **Purge Dynamic Symbols & VTables (`S-05`, `L-07`):** Mutate `ModRegistry` CoW snapshot (`Arc::make_mut`) to null out `vtable_ptr` on components registered by `id` and remove all `"bam.v0.*"` symbols owned by `id`, bumping `ModRegistry.generation += 1`.
4. **Guest Instance Shutdown (`L-02`):** Call `(loaded_mod.shutdown_fn)(loaded_mod.instance)` so the guest frees its per-World `GuestInstance` allocations while `.text` and `.rodata` are still mapped.
5. **Unmap & Unlink (`L-03`):** Drop `LoadedLibrary` (`Library::drop` -> `ShadowFileGuard::drop`).
6. **Emit Lifecycle Message:** Write `ModUnloaded { info }` to `Messages<ModUnloaded>`.

Also, `ModHost::drop` (when `World` itself is dropped without explicit `unload_mod` calls) executes Steps 2, 4, and 5 for every remaining `LoadedMod` in reverse load order so guest instances and shadow files never leak on `App` exit!

---

### 2.4 Mod-Scoped `ModNamedSystemSet` & Cross-Mod Ordering (`L-05`)

#### Exact Type Definition (`bevy_abi_mod_host` / `bevy_abi_mod_host_sys`)

```rust
use std::sync::Arc;
use bevy_ecs::schedule::SystemSet;

/// Per-mod system set grouping all systems belonging to one `ModId` load session.
#[derive(SystemSet, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct ModSystemSet(pub ModId);

/// Scoped named set for intra-mod (`"movement"`) and cross-mod (`"physics::integrate"`) ordering.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModNamedSystemSet {
    pub mod_name: Arc<str>,
    pub system_name: Arc<str>,
}

impl ModNamedSystemSet {
    /// Parses an ordering target string relative to `default_mod_name`:
    /// - `"integrate"`           -> `ModNamedSystemSet { mod_name: default_mod_name, system_name: "integrate" }`
    /// - `"physics::integrate"`  -> `ModNamedSystemSet { mod_name: "physics",        system_name: "integrate" }`
    pub fn parse_target(raw: &str, default_mod_name: &Arc<str>) -> Result<Self, FfiResult> {
        if raw.is_empty() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        if let Some((ns, sys)) = raw.split_once("::") {
            if ns.is_empty() || sys.is_empty() || sys.contains("::") {
                return Err(FfiResult::INVALID_ARGUMENT);
            }
            Ok(Self {
                mod_name: Arc::from(ns),
                system_name: Arc::from(sys),
            })
        } else {
            Ok(Self {
                mod_name: Arc::clone(default_mod_name),
                system_name: Arc::from(raw),
            })
        }
    }
}
```

#### Registration, Validation & Unload Invariants
1. **Unconditional Set Membership:** Every registered `GuestSystem` with name `S` in mod `M` (`ModId`) is **unconditionally** configured `.in_set(ModSystemSet(mod_id)).in_set(ModNamedSystemSet { mod_name: M, system_name: S })` — even when `after_count == 0 && before_count == 0`.
2. **Intra-Mod & Cross-Mod Ordering:**
   - Each string in `desc.after[0..after_count]` adds `.after(ModNamedSystemSet::parse_target(target, &mod_name)?)`.
   - Each string in `desc.before[0..before_count]` adds `.before(ModNamedSystemSet::parse_target(target, &mod_name)?)`.
3. **Unresolved Target Diagnostics:**
   - `ModHost` tracks the active set of `(ScheduleId, ModNamedSystemSet)` across all currently loaded mods plus the staging `LoadTransaction`.
   - At `LoadTransaction::commit()`, any `before`/`after` target not present in that schedule logs a structured `tracing::warn!(mod_name = %M, system = %S, target = %target, "Ordering target set is not currently registered in schedule")` by default, or fails the load with `ModLoadError::UnresolvedOrderingTarget { system, target }` when `LoadOptions::strict == true`.
4. **Full Set Cleanup on Unload:**
   - Unloading `mod_id` removes `ModSystemSet(mod_id)` **and** every `ModNamedSystemSet` registered by `mod_id` from their respective schedules via `ScheduleCleanupPolicy::RemoveSetAndSystems`.

---

## 3. Alternatives Considered

| Alternative | Why Rejected |
|---|---|
| **Pass `WorldId` (`u64`) to Guest Instead of `BamInstance**` Out-Pointer** | Would force every Rust and C99 guest mod to maintain its own thread-safe global `Mutex<HashMap<u64, InstanceState>>`. Returning an opaque `BamInstance*` owned by the host per `LoadedMod` requires zero synchronization on the guest side. |
| **Keep `ModHost::unload_mod` Public and Defer Cleanup to Next Frame** | Leaving unmapped function pointers inside Bevy `Schedules` or `ModRegistry` for even a fraction of a frame violates memory safety if another schedule runs before cleanup. |
| **Copy Shadow Libraries into Target Build Directory** | Triggers infinite file-watcher rebuild loops (`cargo watch` / `notify`). Placing shadow copies in `temp_dir()/bevy_abi_mod` isolates them from source/target watchers. |

---

## 4. Consequences & Verification Criteria

1. **Multi-World isolation test (`L-02`, `T-03`):** Load the same `.cdylib` simultaneously into `World A` and `World B`; unload it from `World A`; verify `World A`'s `GuestPlugin::on_shutdown` ran once while `World B` continues executing its systems without error, then drop `World B` and verify the second `on_shutdown` runs cleanly.
2. **Shadow-copy reload test (`L-03`):** Load a fixture mod with `LoadOptions { shadow_copy: true, .. }`, overwrite the original `.dylib`/`.so`/`.dll` on disk while loaded, call `world.reload_mod(id)`, verify the updated code executes and the old `<stem>.<pid>.<seq>.<ext>` shadow file no longer exists on disk.
3. **Compile-fail / API surface check (`L-04`, `S-05`):** Verify `ModHost` exposes no public `unload_mod` method (`cargo public-api`), `world.unload_mod(id)` removes systems + nulls VTable symbols (`get_proc_address` returns `NULL` after unload), and `commands.queue_unload_mod(id)` unloads cleanly at end-of-schedule.
4. **Three ordering regression tests (`L-05`):**
   - Two mods defining `"step"` do not couple unless qualified (`"mod_a::step"`).
   - Mod B loaded *after* Mod A (where Mod A had zero ordering constraints) orders `.after("mod_a::produce")` deterministically.
   - Unloading and reloading Mod B 1,000 times leaves schedule node/set count constant.
