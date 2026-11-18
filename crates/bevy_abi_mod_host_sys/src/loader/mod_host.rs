//! `ModHost`, `LoadedMod`, `LoadedModInfo`, lifecycle `Message`s, and `&mut World`-scoped
//! load/rollback/unload operations.

use crate::drop_trampoline::{
    StagedDropBinding, StagedHookBinding, WorldDropGuard, prune_drop_trampolines_for_guard,
};
use crate::ffi::host_api::HOST_API;
use crate::ffi::observers_ext::spawn_registered_observers_in_world;
use crate::ffi::register::LoadContext;
use crate::loader::error::{ModLoadError, is_abi_version_compatible};
use crate::loader::shared_lib::{
    LoadOptions, LoadedLibrary, SharedLibState, shared_lib_state_for_path,
};
#[cfg(feature = "signature-verification")]
use crate::loader::signing::{ModTrustPolicy, VerifiedModSignature, default_signature_path_for};
use crate::loader::transaction::LoadTransaction;
use crate::registry::{ModRegistry, RegistrySnapshot};
use crate::schedule::{ModNamedSystemSet, ModSystemSet};
use crate::sync_ptr::GuestInstancePtr;
use crate::system::{GuestSystem, ModQuarantine};
use bevy_abi_mod_sys::{
    ABI_VERSION, BamInitFn, BamInstance, BamManifest, BamManifestFn, BamRestoreStateFn,
    BamSaveStateFn, BamShutdownFn, FfiResult, read_descriptor,
};
use bevy_ecs::component::{Component, ComponentId};
use bevy_ecs::entity::Entity;
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::message::Message;
use bevy_ecs::prelude::Resource;
use bevy_ecs::query::{Allow, Without};
use bevy_ecs::resource::IsResource;
use bevy_ecs::schedule::{InternedScheduleLabel, ScheduleCleanupPolicy, Schedules};
use bevy_ecs::world::World;
use bevy_platform::collections::{HashMap, HashSet};
use core::ffi::{CStr, c_void};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use libloading::Symbol;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

static NEXT_MOD_ID: AtomicU64 = AtomicU64::new(1);

/// Monotonic 64-bit identifier assigned to each loaded mod instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModId(pub u64);

impl ModId {
    #[inline]
    fn allocate() -> Self {
        Self(NEXT_MOD_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// ECS marker [`Component`] automatically attached to entities spawned by a mod's [`crate::HostCommands`]
/// unless opted out via `spawn_persistent` / `spawn_empty_persistent`.
///
/// All entities carrying `ModScoped(id)` (including `Disabled` entities, excluding `IsResource`
/// singleton entities) are bulk-despawned automatically when `id` is unloaded.
#[derive(Component, Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModScoped(pub ModId);

/// ECS [`Component`] attached to the entity spawned for each active native mod.
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
    /// Cumulative number of runtime or initialization faults recorded for this mod.
    pub fault_count: usize,
}

/// Buffered Bevy [`Message`] emitted once whenever a native mod is loaded.
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

/// Buffered Bevy [`Message`] emitted once whenever a native mod is unloaded.
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

/// Read-only descriptor of an active loaded mod managed by [`ModHost`].
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
    /// Cumulative fault count for this mod.
    pub fault_count: usize,
    /// Associated [`LoadedModInfo`] entity in the `World`, once spawned.
    pub entity: Option<Entity>,

    generation: u64,
    pre_snapshot: Arc<RegistrySnapshot>,
    staged_drop_bindings: Vec<StagedDropBinding>,
    staged_hook_bindings: Vec<StagedHookBinding>,
    library: LoadedLibrary,
    shutdown_fn: BamShutdownFn,
    save_state_fn: Option<BamSaveStateFn>,
    restore_state_fn: Option<BamRestoreStateFn>,
    instance: GuestInstancePtr,
    is_active: Arc<AtomicBool>,
    lib_state: Arc<Mutex<SharedLibState>>,
    #[cfg(feature = "signature-verification")]
    verified_signature: Option<VerifiedModSignature>,
    pub(crate) registered_named_sets: Vec<(InternedScheduleLabel, ModNamedSystemSet)>,
    pub(crate) observer_entities: Vec<Entity>,
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

    /// Returns the cumulative number of faults recorded for this mod.
    #[inline]
    pub const fn fault_count(&self) -> usize {
        self.fault_count
    }

    /// Returns `true` if this module is currently active (`!unloaded`).
    #[inline]
    pub fn is_active(&self) -> bool {
        self.is_active.load(Ordering::Acquire)
    }

    /// Returns the verified cryptographic signature metadata for this mod, if signature
    /// verification was enforced during load.
    #[cfg(feature = "signature-verification")]
    #[inline]
    pub const fn verified_signature(&self) -> Option<&VerifiedModSignature> {
        self.verified_signature.as_ref()
    }
}

/// Disarms `loaded`, invokes `bevy_abi_mod_shutdown(instance)` for this `World` instance, and
/// drops `LoadedLibrary` (`dlclose` followed by shadow-file unlink) under `lib_state`.
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
        // `*mut BamInstance` written by this session's `bevy_abi_mod_init`.
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

    /// Increments both `LoadedMod::fault_count` and the `LoadedModInfo::fault_count` ECS component.
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
            // Once scheduled in `World`, fresh slots are live and must only be pruned (never
            // prematurely freed before `WorldDropGuard::drop` + `live_values == 0`) on unload.
            loaded.staged_drop_bindings.clear();
            loaded.staged_hook_bindings.clear();
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
        #[cfg(feature = "signature-verification")] trust_policy: Option<&ModTrustPolicy>,
    ) -> Result<(ModId, Vec<GuestSystem>), ModLoadError> {
        #[cfg(feature = "signature-verification")]
        // SAFETY: Stage shadow copy (verified bytes when enforced) and open shared library via `libloading`.
        let (loaded_lib, actual_path, verified_signature) =
            unsafe { LoadedLibrary::open(source_path, options, trust_policy)? };
        #[cfg(not(feature = "signature-verification"))]
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

        // 1. Mandatory `bevy_abi_mod_manifest`.
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

        #[cfg(feature = "signature-verification")]
        if let (Some(policy), Some(receipt)) = (trust_policy, verified_signature.as_ref())
            && let Err(source) = policy.verify_manifest_principal(receipt, manifest_name_str)
        {
            let signature_path = options
                .signature_path
                .clone()
                .unwrap_or_else(|| default_signature_path_for(source_path));
            return Err(ModLoadError::SignatureVerification {
                path: source_path.to_path_buf(),
                signature_path,
                source,
            });
        }

        // 2. Mandatory `bevy_abi_mod_init` and `bevy_abi_mod_shutdown`.
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
        )
        .with_staged_drop_bindings(tx.staged_drop_bindings())
        .with_staged_hook_bindings(tx.staged_hook_bindings());

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

        // Ensure `World`'s `ModRegistry` reflects any components registered during `init`,
        // and spawn any dynamic observers registered during `init`.
        let mut observer_entities = Vec::new();
        if !world_ptr.is_null() {
            // SAFETY: `world_ptr` is exclusively borrowed and `load_ctx` holds no live `&mut World`.
            let world = unsafe { &mut *world_ptr };
            world.insert_resource(load_ctx.registry.clone());
            observer_entities = spawn_registered_observers_in_world(
                world,
                core::mem::take(&mut load_ctx.registered_observers),
                &load_ctx.registry,
                mod_id,
            );
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

        let (staged_drop_bindings, staged_hook_bindings) = tx.commit();
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
            staged_drop_bindings,
            staged_hook_bindings,
            library: loaded_lib,
            shutdown_fn,
            save_state_fn: manifest.save_state,
            restore_state_fn: manifest.restore_state,
            instance: GuestInstancePtr::new(instance_ptr),
            is_active,
            lib_state,
            #[cfg(feature = "signature-verification")]
            verified_signature,
            registered_named_sets: Vec::new(),
            observer_entities,
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
        #[cfg(feature = "signature-verification")]
        let trust_policy = world.get_resource::<ModTrustPolicy>().cloned();
        let world_ptr = world as *mut World;
        #[cfg(feature = "signature-verification")]
        {
            // SAFETY: `world` is exclusively borrowed (`&mut World`) while loading `path`.
            unsafe {
                mod_host.load_mod_core(path, options, registry, world_ptr, trust_policy.as_ref())
            }
        }
        #[cfg(not(feature = "signature-verification"))]
        {
            // SAFETY: `world` is exclusively borrowed (`&mut World`) while loading `path`.
            unsafe { mod_host.load_mod_core(path, options, registry, world_ptr) }
        }
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
    let rollback_data = world.get_resource_mut::<ModHost>().and_then(|mut h| {
        h.get_mod_mut(id).map(|m| {
            let staged = core::mem::take(&mut m.staged_drop_bindings);
            let staged_hooks = core::mem::take(&mut m.staged_hook_bindings);
            let observers = core::mem::take(&mut m.observer_entities);
            (
                staged,
                staged_hooks,
                observers,
                Arc::clone(&m.is_active),
                Arc::clone(&m.pre_snapshot),
            )
        })
    });

    if let Some((staged, staged_hooks, observers, is_active, snap)) = rollback_data {
        for obs_entity in observers {
            if let Ok(e) = world.get_entity_mut(obs_entity) {
                e.despawn();
            }
        }
        if let Some(mut drop_guard) = world.get_resource_mut::<WorldDropGuard>() {
            drop_guard.rollback_staged_bindings(&staged);
            drop_guard.rollback_staged_hook_bindings(&staged_hooks);
        }
        prune_drop_trampolines_for_guard(&is_active);
        if let Some(mut registry) = world.get_resource_mut::<ModRegistry>() {
            registry.restore_snapshot(snap);
        }
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
/// 1. Excises `ModSystemSet::Mod(id)` and owned `ModNamedSystemSet`s from Bevy `Schedules`.
/// 2. Bulk-despawns all [`ModScoped(id)`](ModScoped) entities (including `Disabled` entities) and
///    removes mod-owned singleton `IsResource` resources by `ComponentId`.
/// 3. Runs `before_dlclose(world)`, disarms `is_active`, prunes `DROP_SLOTS`, and recycles any
///    drop slots with `live_values == 0` (incrementing generation).
/// 4. Evicts guest-owned VTables/symbols in `ModRegistry` and removes `id` from `ModQuarantine`.
/// 5. Invokes `bevy_abi_mod_shutdown(instance)` and drops `LoadedLibrary` (`dlclose` + shadow unlink).
/// 6. Releases any mod-retained assets on `HostAssetBackend`.
pub fn excise_and_unload_mod_in_world<F: FnOnce(&mut World)>(
    world: &mut World,
    id: ModId,
    before_dlclose: F,
) -> Result<(Arc<str>, PathBuf, usize), ModLoadError> {
    let (is_active, owned_named_sets, shared_named_sets, observer_entities) = {
        let Some(mut mod_host) = world.get_resource_mut::<ModHost>() else {
            return Err(ModLoadError::ModNotFound { id });
        };
        let Some(idx) = mod_host.loaded_libraries.iter().position(|m| m.id == id) else {
            return Err(ModLoadError::ModNotFound { id });
        };
        let is_active = Arc::clone(&mod_host.loaded_libraries[idx].is_active);
        let taken = core::mem::take(&mut mod_host.loaded_libraries[idx].registered_named_sets);
        let observers = core::mem::take(&mut mod_host.loaded_libraries[idx].observer_entities);
        let shared: HashSet<(InternedScheduleLabel, ModNamedSystemSet)> = mod_host
            .loaded_libraries
            .iter()
            .enumerate()
            .filter(|&(i, m)| i != idx && m.is_active())
            .flat_map(|(_, m)| m.registered_named_sets.iter().cloned())
            .collect();
        (is_active, taken, shared, observers)
    };

    for obs_entity in observer_entities {
        if let Ok(e) = world.get_entity_mut(obs_entity) {
            e.despawn();
        }
    }

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

    let owned_resource_ids: Vec<ComponentId> = world
        .get_resource::<ModRegistry>()
        .map(|reg| {
            reg.snapshot()
                .entries()
                .iter()
                .filter(|entry| entry.owner == Some(id) && entry.is_resource())
                .map(|entry| entry.component_id)
                .collect()
        })
        .unwrap_or_default();

    world.despawn_all_where::<(&ModScoped,), (Allow<Disabled>, Without<IsResource>)>(|(s,)| {
        s.0 == id
    });

    for res_comp_id in owned_resource_ids {
        world.remove_resource_by_id(res_comp_id);
    }

    before_dlclose(world);

    prune_drop_trampolines_for_guard(&is_active);
    if let Some(mut drop_guard) = world.get_resource_mut::<WorldDropGuard>() {
        drop_guard.recycle_slots_for_guard(&is_active);
    }

    if let Some(mut registry) = world.get_resource_mut::<ModRegistry>() {
        registry.evict_mod(id);
    }
    if let Some(mut quarantine) = world.get_resource_mut::<ModQuarantine>() {
        quarantine.unregister_mod(id);
    }

    let (mod_name, unloaded_path, still_has_same_name) = {
        let mut mod_host = world
            .get_resource_mut::<ModHost>()
            .ok_or(ModLoadError::ModNotFound { id })?;
        // SAFETY: `world` is exclusively borrowed, drop trampolines are disarmed, schedule sets
        // are excised, and guest VTable/symbol pointers are evicted prior to `shutdown` + `dlclose`.
        let (name, path) = unsafe { mod_host.try_unload_mod(id, true)? };
        let shared = mod_host
            .loaded_libraries
            .iter()
            .any(|m| m.is_active() && m.name == name);
        (name, path, shared)
    };

    if !still_has_same_name
        && let Some(registry) = world.get_resource::<ModRegistry>()
        && let Some(asset_backend) = registry.asset_backend()
    {
        asset_backend.release_all_for_owner(mod_name.as_ref());
    }

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

unsafe extern "C" fn host_state_vec_writer(
    writer_ctx: *mut c_void,
    bytes: *const u8,
    len: usize,
) -> FfiResult {
    if writer_ctx.is_null() || (len > 0 && bytes.is_null()) {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: `writer_ctx` is a valid stack pointer to `&mut Vec<u8>` owned by `save_mod_state_in_world`.
    let vec_out = unsafe { &mut *writer_ctx.cast::<Vec<u8>>() };
    if len > 0 {
        // SAFETY: `bytes` is non-null and readable for `len` bytes during `BamSaveStateFn`.
        let slice = unsafe { core::slice::from_raw_parts(bytes, len) };
        vec_out.extend_from_slice(slice);
    }
    FfiResult::OK
}

/// Snapshots optional hot-reload state (`BamManifest::save_state`) from loaded mod `id` into a
/// host-owned `Vec<u8>`, returning `(source_path, saved_blob)`.
///
/// Returns `Ok((source_path, None))` when the mod did not declare a `save_state` callback.
pub fn save_mod_state_in_world(
    world: &World,
    id: ModId,
) -> Result<(PathBuf, Option<Vec<u8>>), ModLoadError> {
    let Some(host) = world.get_resource::<ModHost>() else {
        return Err(ModLoadError::ModNotFound { id });
    };
    let Some(loaded) = host.get_mod(id) else {
        return Err(ModLoadError::ModNotFound { id });
    };
    let source_path = loaded.source_path.clone();
    let Some(save_fn) = loaded.save_state_fn else {
        return Ok((source_path, None));
    };

    let mut blob = Vec::new();
    let writer_ctx = (&raw mut blob).cast::<c_void>();
    // SAFETY: `loaded` is active and mapped in memory; `loaded.instance` is this `World`'s
    // `*mut BamInstance`; `host_state_vec_writer` appends copied bytes into host-owned `blob`.
    let status = unsafe {
        save_fn(
            loaded.instance.as_ptr(),
            Some(host_state_vec_writer),
            writer_ctx,
        )
    };
    if status.is_err() {
        return Err(ModLoadError::StateSaveFailed {
            id,
            mod_name: loaded.name.to_string(),
            status,
        });
    }
    Ok((source_path, Some(blob)))
}

/// Restores a previously captured state blob (`bytes`) into newly loaded mod `id` via
/// `BamManifest::restore_state`.
///
/// If the new mod does not declare `restore_state`, this is a no-op (`Ok(())`).
pub fn restore_mod_state_in_world(
    world: &mut World,
    id: ModId,
    bytes: &[u8],
) -> Result<(), ModLoadError> {
    let (restore_opt, instance_ptr, mod_name, path) = {
        let Some(host) = world.get_resource::<ModHost>() else {
            return Err(ModLoadError::ModNotFound { id });
        };
        let Some(loaded) = host.get_mod(id) else {
            return Err(ModLoadError::ModNotFound { id });
        };
        (
            loaded.restore_state_fn,
            loaded.instance.as_ptr(),
            loaded.name.to_string(),
            loaded.source_path.clone(),
        )
    };
    let Some(restore_fn) = restore_opt else {
        return Ok(());
    };
    let ptr = if bytes.is_empty() {
        core::ptr::null()
    } else {
        bytes.as_ptr()
    };
    // SAFETY: `id` was just initialized in `world` and remains mapped; `bytes` is host-owned and
    // readable for `bytes.len()` bytes across the `restore_fn` call.
    let status = unsafe { restore_fn(instance_ptr, ptr, bytes.len()) };
    if status.is_err() {
        return Err(ModLoadError::StateRestoreFailed {
            path,
            mod_name,
            status,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{ModRegistry, RegisteredComponent};
    use crate::sync_ptr::SymbolAddress;
    use bevy_abi_mod_sys::{BAM_STORAGE_FLAG_RESOURCE, BamComponent, ComponentUuid};
    use bevy_ecs::component::{ComponentCloneBehavior, ComponentDescriptor, StorageType};
    use bevy_ecs::ptr::OwningPtr;
    use bevy_ecs::resource::IsResource;
    use core::alloc::Layout;

    #[test]
    fn excise_unload_removes_mod_owned_is_resource_and_preserves_host_resources() {
        #[derive(Resource)]
        struct HostRes(u32);

        static SHUTDOWN_COUNT: AtomicU64 = AtomicU64::new(0);
        unsafe extern "C" fn test_shutdown(_inst: *mut BamInstance) {
            SHUTDOWN_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        SHUTDOWN_COUNT.store(0, Ordering::SeqCst);
        let mut world = World::new();
        world.insert_resource(HostRes(77));
        world.insert_resource(WorldDropGuard::new());
        world.insert_resource(ModHost::new());
        world.insert_resource(ModRegistry::new());
        world.insert_resource(ModQuarantine::new());

        // Register a mod-owned `IsResource` singleton resource in Bevy `World`.
        // SAFETY: Valid non-drop `(8, 8)` layout matching `u64`.
        let mod_res_id = unsafe {
            let desc = ComponentDescriptor::new_with_layout(
                "test_mod::ModOwnedRes".to_string(),
                StorageType::Table,
                Layout::from_size_align(8, 8).unwrap(),
                None,
                true,
                true,
                ComponentCloneBehavior::Default,
                None,
            );
            world.register_component_with_descriptor(desc)
        };
        let res_ent = world.spawn(IsResource::new(mod_res_id)).id();
        // SAFETY: `mod_res_id` has `(8, 8)` layout matching `u64`.
        unsafe {
            OwningPtr::make(1234u64, |ptr| {
                world.entity_mut(res_ent).insert_by_id(mod_res_id, ptr);
            });
        }
        assert!(world.contains_resource_by_id(mod_res_id));

        let mod_id = ModId::allocate();
        let mod_name: Arc<str> = Arc::from("test_mod");

        // Record `mod_res_id` in `ModRegistry` as owned by `mod_id` with `BAM_STORAGE_FLAG_RESOURCE`.
        {
            let mut reg = world.resource_mut::<ModRegistry>();
            let mut snap = (*reg.snapshot_arc()).clone();
            let handle = BamComponent::from_raw((snap.entries.len() + 1) as u32);
            let res_name: Arc<str> = Arc::from("test_mod::ModOwnedRes");
            let uuid = ComponentUuid::from_parts("test_mod::ModOwnedRes", 8, 8, 0);
            snap.entries.push(RegisteredComponent {
                handle,
                name: Arc::clone(&res_name),
                uuid,
                component_id: mod_res_id,
                size: 8,
                align: 8,
                storage_flags: BAM_STORAGE_FLAG_RESOURCE,
                is_abi_pod: true,
                is_dynamic: true,
                vtable_ptr: SymbolAddress::NULL,
                insert_raw_fn: None,
                insert_default_fn: None,
                drop_fn: None,
                drop_slot: None,
                hook_slot: None,
                has_on_add_hook: false,
                has_on_remove_hook: false,
                create_msg_cursor_fn: None,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
                validates_entity_payload: false,
                synthesizes_ffi_entity_payload: false,
                event_key: None,
                trigger_event_fn: None,
                extract_event_target_fn: None,
                extract_relationship_targets_fn: None,
                owner: Some(mod_id),
            });
            snap.by_name.insert(res_name, handle);
            snap.by_uuid.insert(uuid, handle);
            snap.by_component_id.insert(mod_res_id, handle);
            reg.restore_snapshot(Arc::new(snap));
        }

        // Open a harmless system shared library handle for `LoadedMod` and push into `ModHost`.
        #[cfg(target_os = "macos")]
        let sys_lib_path = Path::new("/usr/lib/libSystem.B.dylib");
        #[cfg(target_os = "linux")]
        let sys_lib_path = Path::new("libc.so.6");
        #[cfg(target_os = "windows")]
        let sys_lib_path = Path::new("kernel32.dll");

        let opts = LoadOptions::new().with_shadow_copy(false);
        #[cfg(feature = "signature-verification")]
        // SAFETY: Opening standard OS system library without invoking guest code.
        let (loaded_lib, loaded_path, verified_signature) =
            unsafe { LoadedLibrary::open(sys_lib_path, &opts, None) }.expect("open system lib");
        #[cfg(not(feature = "signature-verification"))]
        // SAFETY: Opening standard OS system library without invoking guest code.
        let (loaded_lib, loaded_path) =
            unsafe { LoadedLibrary::open(sys_lib_path, &opts) }.expect("open system lib");

        let lib_state = shared_lib_state_for_path(&loaded_path);
        lib_state.lock().unwrap().active_count += 1;
        let pre_snapshot = world.resource::<ModRegistry>().snapshot_arc();
        world
            .resource_mut::<ModHost>()
            .loaded_libraries
            .push(LoadedMod {
                id: mod_id,
                name: mod_name,
                version: Arc::from("0.1.0"),
                source_path: sys_lib_path.to_path_buf(),
                loaded_path,
                system_count: 0,
                fault_count: 0,
                entity: None,
                generation: 0,
                pre_snapshot,
                staged_drop_bindings: Vec::new(),
                staged_hook_bindings: Vec::new(),
                library: loaded_lib,
                shutdown_fn: test_shutdown,
                save_state_fn: None,
                restore_state_fn: None,
                instance: GuestInstancePtr::new(core::ptr::null_mut()),
                is_active: Arc::new(AtomicBool::new(true)),
                lib_state,
                #[cfg(feature = "signature-verification")]
                verified_signature,
                registered_named_sets: Vec::new(),
                observer_entities: Vec::new(),
            });

        // Unload via `excise_and_unload_mod_in_world` and verify mod-owned `IsResource` is evicted!
        excise_and_unload_mod_in_world(&mut world, mod_id, |_| {})
            .expect("excise_and_unload_mod_in_world succeeds");

        assert!(!world.contains_resource_by_id(mod_res_id));
        assert!(world.contains_resource::<HostRes>());
        assert_eq!(world.resource::<HostRes>().0, 77);
        assert!(!world.resource::<ModHost>().contains_mod(mod_id));
        assert_eq!(SHUTDOWN_COUNT.load(Ordering::SeqCst), 1);
    }
}
