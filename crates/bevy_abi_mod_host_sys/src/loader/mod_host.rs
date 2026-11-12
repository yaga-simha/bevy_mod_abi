//! `ModHost`, `LoadedMod`, `LoadedModInfo`, lifecycle `Message`s, and `&mut World`-scoped
//! load/rollback/unload operations (`L-02`, `L-03`, `L-08`, `B-05`, `B-09`).

use crate::drop_trampoline::{WorldDropGuard, prune_drop_trampolines_for_guard};
use crate::ffi::host_api::HOST_API;
use crate::ffi::register::LoadContext;
use crate::loader::error::{ModLoadError, is_abi_version_compatible};
use crate::loader::shared_lib::{
    LoadOptions, LoadedLibrary, SharedLibState, shared_lib_state_for_path,
};
use crate::loader::transaction::LoadTransaction;
use crate::registry::{ModRegistry, RegistrySnapshot};
use crate::schedule::{ModNamedSystemSet, ModSystemSet};
use crate::sync_ptr::GuestInstancePtr;
use crate::system::{GuestSystem, ModQuarantine};
use bevy_abi_mod_sys::{
    ABI_VERSION, BamInitFn, BamInstance, BamManifest, BamManifestFn, BamShutdownFn, FfiResult,
    read_descriptor,
};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::message::Message;
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::{InternedScheduleLabel, ScheduleCleanupPolicy, Schedules};
use bevy_ecs::world::World;
use bevy_platform::collections::{HashMap, HashSet};
use core::ffi::{CStr, c_void};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use libloading::Symbol;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

static NEXT_MOD_ID: AtomicU64 = AtomicU64::new(1);

/// Monotonic 64-bit identifier assigned to each loaded mod instance (`L-08`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModId(pub u64);

impl ModId {
    #[inline]
    fn allocate() -> Self {
        Self(NEXT_MOD_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// ECS [`Component`] attached to the entity spawned for each active native mod (`B-05`, `L-08`).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LoadedModInfo {
    /// Assigned monotonic [`ModId`].
    pub id: ModId,
    /// Canonical mod name declared in `BamManifest::name`.
    pub name: String,
    /// SemVer string declared in `BamManifest::version`.
    pub version: String,
    /// Original source filesystem path passed to `load_mod`.
    pub path: PathBuf,
    /// Number of guest systems registered by this mod.
    pub system_count: usize,
    /// Cumulative number of runtime or initialization faults recorded for this mod (`B-05`).
    pub fault_count: usize,
}

/// Buffered Bevy [`Message`] emitted once whenever a native mod is loaded (`B-09`).
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ModLoaded {
    /// Assigned [`ModId`].
    pub id: ModId,
    /// ECS entity carrying [`LoadedModInfo`] + `Name`.
    pub entity: Entity,
    /// Canonical mod name from `BamManifest`.
    pub name: String,
    /// Mod version string from `BamManifest`.
    pub version: String,
    /// Source filesystem path passed to `load_mod`.
    pub path: PathBuf,
    /// Number of dynamic systems scheduled.
    pub system_count: usize,
}

/// Buffered Bevy [`Message`] emitted once whenever a native mod is unloaded (`B-09`).
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ModUnloaded {
    /// Unloaded [`ModId`].
    pub id: ModId,
    /// Canonical mod name from `BamManifest`.
    pub name: String,
    /// Source filesystem path of the unloaded library, if known.
    pub path: Option<PathBuf>,
    /// Number of dynamic systems excised from Bevy schedules.
    pub removed_systems: usize,
}

/// Read-only descriptor of an active loaded mod managed by [`ModHost`] (`L-02`, `L-08`, `B-05`).
///
/// Internal OS library and guest instance handles remain private so systems borrowing `Res<ModHost>`
/// can inspect loaded modules without triggering `dlclose` during parallel schedule execution.
pub struct LoadedMod {
    /// Unique monotonic [`ModId`].
    pub id: ModId,
    /// Canonical mod name from `BamManifest::name`.
    pub name: Arc<str>,
    /// Mod version from `BamManifest::version`.
    pub version: Arc<str>,
    /// Original source filesystem path requested by the host.
    pub source_path: PathBuf,
    /// Actual mapped filesystem path (the shadow copy path when `shadow_copy == true`).
    pub loaded_path: PathBuf,
    /// Number of systems registered by this mod.
    pub system_count: usize,
    /// Cumulative fault count for this mod (`B-05`).
    pub fault_count: usize,
    /// Associated [`LoadedModInfo`] entity in the `World`, once spawned.
    pub entity: Option<Entity>,

    generation: u64,
    pre_snapshot: Arc<RegistrySnapshot>,
    library: LoadedLibrary,
    shutdown_fn: BamShutdownFn,
    instance: GuestInstancePtr,
    is_active: Arc<AtomicBool>,
    lib_state: Arc<Mutex<SharedLibState>>,
    pub(crate) registered_named_sets: Vec<(InternedScheduleLabel, ModNamedSystemSet)>,
}

impl LoadedMod {
    /// Returns the unique [`ModId`] assigned to this loaded module.
    #[inline]
    pub const fn id(&self) -> ModId {
        self.id
    }

    /// Returns the canonical mod name declared in `BamManifest`.
    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Alias for [`Self::name`] used when scoping [`ModNamedSystemSet`]s.
    #[inline]
    pub fn mod_name(&self) -> &str {
        &self.name
    }

    /// Returns the version string declared in `BamManifest`.
    #[inline]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Returns the reload generation assigned to this module's [`ModNamedSystemSet`]s.
    #[inline]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the original source path passed to `load_mod`.
    #[inline]
    pub fn path(&self) -> &Path {
        &self.source_path
    }

    /// Returns the original source path passed to `load_mod`.
    #[inline]
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    /// Returns the actual OS-mapped path (shadow file path when `shadow_copy` is enabled).
    #[inline]
    pub fn loaded_path(&self) -> &Path {
        &self.loaded_path
    }

    /// Returns the number of guest systems registered by this mod.
    #[inline]
    pub const fn system_count(&self) -> usize {
        self.system_count
    }

    /// Returns the cumulative number of faults recorded for this mod (`B-05`).
    #[inline]
    pub const fn fault_count(&self) -> usize {
        self.fault_count
    }

    /// Returns `true` if this module is currently active (`!unloaded`).
    #[inline]
    pub fn is_active(&self) -> bool {
        self.is_active.load(Ordering::Acquire)
    }
}

/// Disarms `loaded`, invokes `bevy_abi_mod_shutdown(instance)` for this `World` instance, and
/// drops `LoadedLibrary` (`dlclose` followed by shadow-file unlink) under `lib_state` (`L-02`, `L-03`).
///
/// # Safety
/// Caller must ensure no guest systems or foreign drop hooks from this module are concurrently
/// executing on worker threads.
unsafe fn unload_loaded_mod(loaded: LoadedMod, invoke_shutdown: bool) {
    prune_drop_trampolines_for_guard(&loaded.is_active);
    let mut state_guard = loaded
        .lib_state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if invoke_shutdown {
        // SAFETY: `loaded.library` is still mapped in memory and `loaded.instance` is the exact
        // `*mut BamInstance` written by this session's `bevy_abi_mod_init` (`L-02`).
        unsafe {
            (loaded.shutdown_fn)(loaded.instance.as_ptr());
        }
    }

    state_guard.active_count = state_guard.active_count.saturating_sub(1);
    drop(loaded.library);
    drop(state_guard);
}

/// Host resource tracking loaded native mod instances for a single Bevy [`World`].
///
/// Public methods on `ModHost` are strictly read-only; loading and unloading require exclusive
/// `&mut World` access (`ModWorldExt`, `ModAppExt`, or deferred `ModCommandsExt`).
#[derive(Resource, Default)]
pub struct ModHost {
    loaded_libraries: Vec<LoadedMod>,
    mod_generations: HashMap<Arc<str>, u64>,
}

impl ModHost {
    /// Creates a new, empty mod host manager.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the current reload generation for `mod_name` (`0` on initial load).
    pub fn mod_generation(&self, mod_name: &str) -> u64 {
        if let Some(active) = self
            .loaded_libraries
            .iter()
            .rev()
            .find(|m| m.is_active() && m.name.as_ref() == mod_name)
        {
            return active.generation;
        }
        *self.mod_generations.get(mod_name).unwrap_or(&0)
    }

    /// Returns `true` if a mod with the given [`ModId`] is currently loaded.
    #[inline]
    pub fn contains_mod(&self, id: ModId) -> bool {
        self.loaded_libraries.iter().any(|m| m.id == id)
    }

    /// Borrows the [`LoadedMod`] entry for `id`, if currently loaded.
    #[inline]
    pub fn get_mod(&self, id: ModId) -> Option<&LoadedMod> {
        self.loaded_libraries.iter().find(|m| m.id == id)
    }

    /// Returns a slice of all currently loaded mod records.
    #[inline]
    pub fn loaded_mods(&self) -> &[LoadedMod] {
        &self.loaded_libraries
    }

    /// Returns the number of currently loaded mod libraries.
    #[inline]
    pub fn len(&self) -> usize {
        self.loaded_libraries.len()
    }

    /// Returns `true` if no mod libraries are currently loaded.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.loaded_libraries.is_empty()
    }

    pub(crate) fn get_mod_mut(&mut self, id: ModId) -> Option<&mut LoadedMod> {
        self.loaded_libraries.iter_mut().find(|m| m.id == id)
    }

    /// Increments the fault counter for `id` on `ModHost` and returns the associated `LoadedModInfo` entity.
    pub(crate) fn increment_fault_count(&mut self, id: ModId) -> Option<Entity> {
        let loaded = self.get_mod_mut(id)?;
        loaded.fault_count = loaded.fault_count.saturating_add(1);
        loaded.entity
    }

    /// Increments both `LoadedMod::fault_count` and the `LoadedModInfo::fault_count` ECS component (`B-05`).
    pub(crate) fn record_fault_in_world(world: &mut World, id: ModId) {
        let entity_opt = world
            .get_resource_mut::<Self>()
            .and_then(|mut host| host.increment_fault_count(id));
        if let Some(entity) = entity_opt
            && let Some(mut info) = world.get_mut::<LoadedModInfo>(entity)
        {
            info.fault_count = info.fault_count.saturating_add(1);
        }
    }

    /// Associates the spawned `LoadedModInfo` `Entity` and scheduled `system_count` with `id`.
    pub fn set_mod_entity_and_system_count(
        &mut self,
        id: ModId,
        entity: Entity,
        system_count: usize,
    ) {
        if let Some(loaded) = self.get_mod_mut(id) {
            loaded.entity = Some(entity);
            loaded.system_count = system_count;
        }
    }

    /// Core loader opening `source_path` (with optional shadow copy), validating `bevy_abi_mod_manifest`,
    /// running `bevy_abi_mod_init` against `LoadContext`, and capturing `*mut BamInstance`.
    ///
    /// # Safety
    /// - `source_path` must point to a trusted native mod shared library.
    /// - `world_ptr` must point to an exclusively borrowed `World`.
    pub(crate) unsafe fn load_mod_core(
        &mut self,
        source_path: &Path,
        options: &LoadOptions,
        registry: ModRegistry,
        world_ptr: *mut World,
    ) -> Result<(ModId, Vec<GuestSystem>), ModLoadError> {
        // SAFETY: Stage shadow copy if requested and open shared library via `libloading`.
        let (loaded_lib, actual_path) = unsafe { LoadedLibrary::open(source_path, options)? };

        let lib_state = shared_lib_state_for_path(&actual_path);
        let mut state_guard = lib_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let mod_id = ModId::allocate();
        let pre_snapshot = registry.snapshot_arc();
        let is_active = Arc::new(AtomicBool::new(true));

        // Declared *after* `loaded_lib` so Rust's reverse local drop order runs `LoadTransaction::drop`
        // (disarming drop trampolines and restoring `pre_snapshot`) *before* `dlclose` + shadow unlink.
        let tx = LoadTransaction::new(world_ptr, Arc::clone(&is_active), Arc::clone(&pre_snapshot));

        // 1. Mandatory `bevy_abi_mod_manifest` (`L-08`).
        // SAFETY: Resolving mandatory `bevy_abi_mod_manifest` symbol with signature `extern "C" fn() -> *const BamManifest`.
        let manifest_fn: Symbol<BamManifestFn> = unsafe {
            loaded_lib
                .library
                .get(b"bevy_abi_mod_manifest\0")
                .map_err(|source| ModLoadError::MissingManifest {
                    path: source_path.to_path_buf(),
                    source,
                })?
        };

        // SAFETY: Invoking side-effect-free manifest descriptor getter.
        let manifest_ptr = unsafe { manifest_fn() };
        if manifest_ptr.is_null() {
            return Err(ModLoadError::InvalidManifest {
                path: source_path.to_path_buf(),
                status: FfiResult::NULL_POINTER,
                detail: "bevy_abi_mod_manifest() returned null".to_string(),
            });
        }

        // SAFETY: `read_descriptor` checks alignment and `struct_size >= BamManifest::MIN_V0_2_SIZE`.
        let manifest = unsafe { read_descriptor::<BamManifest>(manifest_ptr.cast::<c_void>()) }
            .map_err(|status| ModLoadError::InvalidManifest {
                path: source_path.to_path_buf(),
                status,
                detail: "BamManifest struct_size is smaller than MIN_V0_2_SIZE".to_string(),
            })?;

        if !is_abi_version_compatible(ABI_VERSION, manifest.abi_version) {
            return Err(ModLoadError::IncompatibleAbiVersion {
                path: source_path.to_path_buf(),
                host_version: ABI_VERSION,
                guest_version: manifest.abi_version,
            });
        }

        let manifest_status = manifest.validate_for_host(ABI_VERSION);
        if manifest_status.is_err() {
            return Err(ModLoadError::InvalidManifest {
                path: source_path.to_path_buf(),
                status: manifest_status,
                detail: "BamManifest failed structural/flag validation".to_string(),
            });
        }

        // SAFETY: `manifest.validate_for_host` verified `!manifest.name.is_null()` and `!manifest.version.is_null()`.
        let manifest_name_str =
            unsafe { CStr::from_ptr(manifest.name) }
                .to_str()
                .map_err(|_| ModLoadError::InvalidManifest {
                    path: source_path.to_path_buf(),
                    status: FfiResult::INVALID_UTF8,
                    detail: "BamManifest `name` is not valid UTF-8".to_string(),
                })?;
        if manifest_name_str.is_empty() {
            return Err(ModLoadError::InvalidManifest {
                path: source_path.to_path_buf(),
                status: FfiResult::INVALID_ARGUMENT,
                detail: "BamManifest `name` must not be empty".to_string(),
            });
        }

        // SAFETY: `manifest.version` was checked non-null above.
        let manifest_version_str = unsafe { CStr::from_ptr(manifest.version) }
            .to_str()
            .map_err(|_| ModLoadError::InvalidManifest {
                path: source_path.to_path_buf(),
                status: FfiResult::INVALID_UTF8,
                detail: "BamManifest `version` is not valid UTF-8".to_string(),
            })?;
        if manifest_version_str.is_empty() {
            return Err(ModLoadError::InvalidManifest {
                path: source_path.to_path_buf(),
                status: FfiResult::INVALID_ARGUMENT,
                detail: "BamManifest `version` must not be empty".to_string(),
            });
        }

        let manifest_name: Arc<str> = Arc::from(manifest_name_str);
        let manifest_version: Arc<str> = Arc::from(manifest_version_str);

        // 2. Mandatory `bevy_abi_mod_init` and `bevy_abi_mod_shutdown` (`L-02`).
        // SAFETY: Resolving `bevy_abi_mod_init`.
        let init_fn: Symbol<BamInitFn> = unsafe {
            loaded_lib
                .library
                .get(b"bevy_abi_mod_init\0")
                .map_err(|source| ModLoadError::MissingEntryPoint {
                    path: source_path.to_path_buf(),
                    symbol: "bevy_abi_mod_init",
                    source,
                })?
        };

        // SAFETY: Resolving `bevy_abi_mod_shutdown`.
        let shutdown_fn: BamShutdownFn = unsafe {
            *loaded_lib
                .library
                .get::<BamShutdownFn>(b"bevy_abi_mod_shutdown\0")
                .map_err(|source| ModLoadError::MissingEntryPoint {
                    path: source_path.to_path_buf(),
                    symbol: "bevy_abi_mod_shutdown",
                    source,
                })?
        };

        let mut load_ctx = LoadContext::with_manifest_name(
            registry,
            Arc::clone(&is_active),
            world_ptr,
            Some(mod_id),
            Some(Arc::clone(&manifest_name)),
        );

        let mut instance_ptr: *mut BamInstance = core::ptr::null_mut();
        // SAFETY: Invoking guest `bevy_abi_mod_init` with static `HOST_API`, stack-scoped `LoadContext`
        // (poisoned immediately afterwards via `finish_init`), and valid `&mut instance_ptr`.
        let status = unsafe { init_fn(&HOST_API, load_ctx.as_ffi_ptr(), &mut instance_ptr) };
        let diagnostic = load_ctx.host_ctx.last_error_message();
        load_ctx.finish_init();

        if status == FfiResult::PANIC_CAUGHT {
            return Err(ModLoadError::InitPanicked {
                path: source_path.to_path_buf(),
                diagnostic,
            });
        }
        if status.is_err() {
            return Err(ModLoadError::InitFailed {
                path: source_path.to_path_buf(),
                status,
                diagnostic,
            });
        }

        // Ensure `World`'s `ModRegistry` reflects any components registered during `init`.
        if !world_ptr.is_null() {
            // SAFETY: `world_ptr` is exclusively borrowed and `load_ctx` holds no live `&mut World`.
            let world = unsafe { &mut *world_ptr };
            world.insert_resource(load_ctx.registry.clone());
        }

        for sys in &mut load_ctx.registered_systems {
            sys.set_registry(load_ctx.registry.clone());
            sys.set_mod_id(mod_id);
            sys.set_mod_namespace(Some(Arc::clone(&manifest_name)));
            if let Some(sched_override) = options.schedule_override {
                sys.set_schedule(sched_override);
            }
            if let Some(policy) = options.error_policy {
                sys.set_error_policy(policy);
            }
        }

        tx.commit();
        state_guard.active_count += 1;
        drop(state_guard);

        let systems = core::mem::take(&mut load_ctx.registered_systems);
        let generation = *self
            .mod_generations
            .get(manifest_name.as_ref())
            .unwrap_or(&0);
        let system_count = systems.len();
        self.loaded_libraries.push(LoadedMod {
            id: mod_id,
            name: manifest_name,
            version: manifest_version,
            source_path: source_path.to_path_buf(),
            loaded_path: actual_path,
            system_count,
            fault_count: 0,
            entity: None,
            generation,
            pre_snapshot,
            library: loaded_lib,
            shutdown_fn,
            instance: GuestInstancePtr::new(instance_ptr),
            is_active,
            lib_state,
            registered_named_sets: Vec::new(),
        });

        Ok((mod_id, systems))
    }

    /// Crate-internal unload of a single module after schedule excision and registry eviction.
    ///
    /// # Safety
    /// Caller must ensure `id` is not concurrently executing on any worker thread.
    pub(crate) unsafe fn try_unload_mod(
        &mut self,
        id: ModId,
        invoke_shutdown: bool,
    ) -> Result<(Arc<str>, PathBuf), ModLoadError> {
        let pos = self
            .loaded_libraries
            .iter()
            .position(|m| m.id == id)
            .ok_or(ModLoadError::ModNotFound { id })?;
        let loaded = self.loaded_libraries.remove(pos);
        if !self
            .loaded_libraries
            .iter()
            .any(|m| m.is_active() && m.name == loaded.name)
        {
            *self
                .mod_generations
                .entry(Arc::clone(&loaded.name))
                .or_insert(0) += 1;
        }
        let mod_name = Arc::clone(&loaded.name);
        let source_path = loaded.source_path.clone();
        // SAFETY: Caller guarantees unloading this mod is safe with respect to worker threads.
        unsafe {
            unload_loaded_mod(loaded, invoke_shutdown);
        }
        Ok((mod_name, source_path))
    }

    /// Crate-internal LIFO teardown of all remaining loaded modules (used on `Drop`).
    ///
    /// # Safety
    /// Caller must ensure no external threads are executing code from any loaded library.
    pub(crate) unsafe fn unload_all(&mut self) {
        for loaded in self.loaded_libraries.drain(..).rev() {
            // SAFETY: Caller has exclusive ownership of `ModHost` during teardown.
            unsafe {
                unload_loaded_mod(loaded, true);
            }
        }
    }
}

impl Drop for ModHost {
    fn drop(&mut self) {
        // SAFETY: `ModHost` is being dropped with exclusive `&mut self` ownership (e.g. `World` drop).
        unsafe {
            self.unload_all();
        }
    }
}

/// Ensures [`ModHost`], [`ModRegistry`], [`ModQuarantine`], and [`WorldDropGuard`] exist in `world`
/// and loads the mod at `path` using `options`, returning its [`ModId`] and unscheduled [`GuestSystem`]s.
pub fn load_mod_in_world(
    world: &mut World,
    path: &Path,
    options: &LoadOptions,
) -> Result<(ModId, Vec<GuestSystem>), ModLoadError> {
    if !world.contains_resource::<WorldDropGuard>() {
        world.insert_resource(WorldDropGuard::new());
    }
    if !world.contains_resource::<ModHost>() {
        world.insert_resource(ModHost::new());
    }
    if !world.contains_resource::<ModRegistry>() {
        world.insert_resource(ModRegistry::new());
    }
    if !world.contains_resource::<ModQuarantine>() {
        world.insert_resource(ModQuarantine::new());
    }
    world.resource_scope::<ModHost, _>(|world, mut mod_host| {
        let registry = world
            .get_resource::<ModRegistry>()
            .cloned()
            .unwrap_or_default();
        let world_ptr = world as *mut World;
        // SAFETY: `world` is exclusively borrowed (`&mut World`) while loading `path`.
        unsafe { mod_host.load_mod_core(path, options, registry, world_ptr) }
    })
}

/// Records the `(InternedScheduleLabel, ModNamedSystemSet)` pairs attached for `id` so they are
/// automatically excised from `Schedules` when `id` unloads.
pub fn record_mod_named_sets_in_world(
    world: &mut World,
    id: ModId,
    sets: Vec<(InternedScheduleLabel, ModNamedSystemSet)>,
) {
    if let Some(mut mod_host) = world.get_resource_mut::<ModHost>()
        && let Some(loaded) = mod_host.get_mod_mut(id)
    {
        loaded.registered_named_sets.extend(sets);
    }
}

/// Rolls back and unloads an unscheduled mod (`id`) after post-init schedule/strict validation
/// failure: disarms drop trampolines, restores the pre-load `RegistrySnapshot`, invokes
/// `bevy_abi_mod_shutdown(instance)` (since `bevy_abi_mod_init` succeeded), and unmaps the library.
pub fn rollback_unscheduled_mod_in_world(world: &mut World, id: ModId) {
    let pre_snap = world
        .get_resource::<ModHost>()
        .and_then(|h| h.get_mod(id))
        .map(|m| {
            prune_drop_trampolines_for_guard(&m.is_active);
            Arc::clone(&m.pre_snapshot)
        });

    if let Some(snap) = pre_snap
        && let Some(mut registry) = world.get_resource_mut::<ModRegistry>()
    {
        registry.restore_snapshot(snap);
    }

    if let Some(mut quarantine) = world.get_resource_mut::<ModQuarantine>() {
        quarantine.unregister_mod(id);
    }

    if let Some(mut mod_host) = world.get_resource_mut::<ModHost>() {
        // SAFETY: `world` is exclusively borrowed, drop trampolines and registry entries are
        // rolled back above, and none of `id`'s systems were added to any schedule.
        let _ = unsafe { mod_host.try_unload_mod(id, true) };
    }
}

/// Deterministic unload sequence (`world.unload_mod(id)`):
/// 1. Disarms `is_active` and prunes `id` from `DROP_SLOTS`.
/// 2. Excises `ModSystemSet::Mod(id)` and owned `ModNamedSystemSet`s from Bevy `Schedules`.
/// 3. Evicts guest-owned VTables/symbols in `ModRegistry` and removes `id` from `ModQuarantine`.
/// 4. Runs `before_dlclose(world)`.
/// 5. Invokes `bevy_abi_mod_shutdown(instance)` and drops `LoadedLibrary` (`dlclose` + shadow unlink).
pub fn excise_and_unload_mod_in_world<F: FnOnce(&mut World)>(
    world: &mut World,
    id: ModId,
    before_dlclose: F,
) -> Result<(Arc<str>, PathBuf, usize), ModLoadError> {
    let (owned_named_sets, shared_named_sets) = {
        let Some(mut mod_host) = world.get_resource_mut::<ModHost>() else {
            return Err(ModLoadError::ModNotFound { id });
        };
        let Some(idx) = mod_host.loaded_libraries.iter().position(|m| m.id == id) else {
            return Err(ModLoadError::ModNotFound { id });
        };
        prune_drop_trampolines_for_guard(&mod_host.loaded_libraries[idx].is_active);
        let taken = core::mem::take(&mut mod_host.loaded_libraries[idx].registered_named_sets);
        let shared: HashSet<(InternedScheduleLabel, ModNamedSystemSet)> = mod_host
            .loaded_libraries
            .iter()
            .enumerate()
            .filter(|&(i, m)| i != idx && m.is_active())
            .flat_map(|(_, m)| m.registered_named_sets.iter().cloned())
            .collect();
        (taken, shared)
    };

    let mut removed_systems = 0usize;
    if let Some(schedules) = world.get_resource::<Schedules>() {
        let mod_set = ModSystemSet::Mod(id);
        let mut target_labels: Vec<InternedScheduleLabel> = schedules
            .iter()
            .filter(|(_, schedule)| {
                let sets = &schedule.graph().system_sets;
                sets.get_key(bevy_ecs::schedule::SystemSet::intern(&mod_set))
                    .and_then(|k| sets.get(k))
                    .is_some()
            })
            .map(|(_, schedule)| schedule.label())
            .collect();
        let mut seen_labels: HashSet<InternedScheduleLabel> =
            target_labels.iter().copied().collect();
        for &(label, _) in &owned_named_sets {
            if seen_labels.insert(label) {
                target_labels.push(label);
            }
        }

        for label in target_labels {
            let removed_sched = world
                .get_resource_mut::<Schedules>()
                .and_then(|mut s| s.remove_temporarily(label));
            if let Some(mut schedule) = removed_sched {
                for (set_label, named_set) in &owned_named_sets {
                    if *set_label == label
                        && !shared_named_sets.contains(&(label, named_set.clone()))
                        && named_set.is_present_in(&schedule)
                        && let Ok(n) = schedule.remove_systems_in_set(
                            named_set.clone(),
                            world,
                            ScheduleCleanupPolicy::RemoveSetAndSystems,
                        )
                    {
                        removed_systems += n;
                    }
                }

                let mod_set_present = {
                    let sets = &schedule.graph().system_sets;
                    sets.get_key(bevy_ecs::schedule::SystemSet::intern(&mod_set))
                        .and_then(|k| sets.get(k))
                        .is_some()
                };
                if mod_set_present
                    && let Ok(n) = schedule.remove_systems_in_set(
                        mod_set,
                        world,
                        ScheduleCleanupPolicy::RemoveSetAndSystems,
                    )
                {
                    removed_systems += n;
                }

                if let Some(mut schedules) = world.get_resource_mut::<Schedules>() {
                    schedules.reinsert(schedule);
                }
            }
        }
    }

    if let Some(mut registry) = world.get_resource_mut::<ModRegistry>() {
        registry.evict_mod(id);
    }
    if let Some(mut quarantine) = world.get_resource_mut::<ModQuarantine>() {
        quarantine.unregister_mod(id);
    }

    before_dlclose(world);

    let (mod_name, unloaded_path) = {
        let mut mod_host = world
            .get_resource_mut::<ModHost>()
            .ok_or(ModLoadError::ModNotFound { id })?;
        // SAFETY: `world` is exclusively borrowed, drop trampolines are disarmed, schedule sets
        // are excised, and guest VTable/symbol pointers are evicted prior to `shutdown` + `dlclose`.
        unsafe { mod_host.try_unload_mod(id, true)? }
    };

    Ok((mod_name, unloaded_path, removed_systems))
}

/// Unloads all currently loaded mods in `world` in LIFO order under exclusive `&mut World` access.
pub fn unload_all_mods_in_world<F>(world: &mut World, mut on_each_unloaded: F)
where
    F: FnMut(&mut World, ModId, Arc<str>, PathBuf, usize),
{
    let ids_lifo: Vec<ModId> = world
        .get_resource::<ModHost>()
        .map(|h| h.loaded_mods().iter().rev().map(LoadedMod::id).collect())
        .unwrap_or_default();

    for id in ids_lifo {
        if let Ok((name, path, removed)) = excise_and_unload_mod_in_world(world, id, |_| {}) {
            on_each_unloaded(world, id, name, path, removed);
        }
    }
}
