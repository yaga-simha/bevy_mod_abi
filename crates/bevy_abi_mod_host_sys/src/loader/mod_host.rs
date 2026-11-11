//! `ModHost`, `LoadedMod`, and `&mut World`-scoped load/rollback/unload lifecycle operations.

use crate::drop_trampoline::prune_drop_trampolines_for_guard;
use crate::ffi::host_api::HOST_API;
use crate::ffi::proc_address::host_get_proc_address;
use crate::ffi::register::ModInitContext;
use crate::loader::error::{ModLoadError, is_abi_version_compatible};
use crate::loader::shared_lib::{
    SharedLibState, canonical_mod_name_from_path, shared_lib_state_for_path,
};
use crate::loader::transaction::LoadTransaction;
use crate::registry::ModRegistry;
use crate::schedule::{ModNamedSystemSet, ModSystemSet};
use crate::system::DynamicCSystem;
use bevy_abi_mod_sys::{ABI_VERSION_PACKED, FfiResult, ModAbiVersionFn, ModInitFn, ModShutdownFn};
use bevy_ecs::component::ComponentId;
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::{InternedScheduleLabel, ScheduleCleanupPolicy, Schedules};
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};
use libloading::{Library, Symbol};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Unique generational identifier for a loaded mod instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ModId(pub u32);

/// Read-only descriptor of a loaded native mod shared library managed by [`ModHost`].
///
/// All internal library handles and lifecycle fields are private so systems borrowing
/// `Res<ModHost>` or `ResMut<ModHost>` can inspect loaded modules without triggering `dlclose`.
pub struct LoadedMod {
    id: ModId,
    mod_name: String,
    generation: u64,
    name: String,
    path: PathBuf,
    library: Library,
    shutdown_fn: Option<ModShutdownFn>,
    is_active: Arc<AtomicBool>,
    lib_state: Arc<Mutex<SharedLibState>>,
    pub(crate) registered_named_sets: Vec<(InternedScheduleLabel, ModNamedSystemSet)>,
    pub(crate) newly_created_components: Vec<ComponentId>,
}

impl LoadedMod {
    /// Returns the unique [`ModId`] assigned to this loaded module.
    #[inline]
    pub const fn id(&self) -> ModId {
        self.id
    }

    /// Returns the display name / path string recorded when this module was loaded.
    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the canonical module name used to scope [`ModNamedSystemSet`]s.
    #[inline]
    pub fn mod_name(&self) -> &str {
        &self.mod_name
    }

    /// Returns the reload generation assigned to this module's [`ModNamedSystemSet`]s.
    #[inline]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the filesystem path from which the shared library was loaded.
    #[inline]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns `true` if this module is currently active (`!unloaded`).
    #[inline]
    pub fn is_active(&self) -> bool {
        self.is_active.load(Ordering::Acquire)
    }
}

/// Deactivates a [`LoadedMod`], prunes its drop trampolines, invokes `bevy_abi_mod_shutdown`
/// when the final active instance of that shared image unloads, and drops `Library` (`dlclose`).
///
/// # Safety
/// Caller must ensure no guest systems or foreign drop hooks from this module are concurrently
/// executing on worker threads.
unsafe fn unload_loaded_mod(loaded: LoadedMod) {
    prune_drop_trampolines_for_guard(&loaded.is_active);
    let mut state_guard = loaded
        .lib_state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state_guard.active_count = state_guard.active_count.saturating_sub(1);
    if state_guard.active_count == 0
        && let Some(shutdown) = loaded.shutdown_fn
    {
        // SAFETY: Invoking mod teardown hook while the shared library remains mapped and after the
        // final active `ModHost` reference is being released.
        unsafe {
            shutdown(core::ptr::null_mut());
        }
    }
    drop(loaded.library);
}

/// Host resource tracking loaded dynamic libraries.
///
/// Public methods on `ModHost` are strictly read-only; loading and unloading require exclusive
/// `&mut World` access (`ModAppExt` or `excise_and_unload_mod_in_world`) so `ResMut<ModHost>`
/// inside a running schedule cannot unmap executable code out from under sibling systems.
#[derive(Resource)]
pub struct ModHost {
    loaded_libraries: Vec<LoadedMod>,
    next_mod_id: u32,
    mod_generations: HashMap<String, u64>,
}

impl Default for ModHost {
    fn default() -> Self {
        Self::new()
    }
}

impl ModHost {
    /// Creates a new, empty mod host manager.
    pub fn new() -> Self {
        Self {
            loaded_libraries: Vec::new(),
            next_mod_id: 1,
            mod_generations: HashMap::new(),
        }
    }

    /// Returns the current reload generation for `mod_name` (`0` on initial load).
    pub fn mod_generation(&self, mod_name: &str) -> u64 {
        if let Some(active) = self
            .loaded_libraries
            .iter()
            .rev()
            .find(|m| m.is_active() && m.mod_name == mod_name)
        {
            return active.generation;
        }
        *self.mod_generations.get(mod_name).unwrap_or(&0)
    }

    /// Core loader shared by `World`-backed mod loading.
    ///
    /// # Safety
    /// - `path` must point to a trusted, valid mod shared library.
    /// - If `world_ptr` is non-null, it must point to an exclusively borrowed `World`.
    pub(crate) unsafe fn load_mod_core(
        &mut self,
        path: &Path,
        registry: Arc<ModRegistry>,
        world_ptr: *mut World,
    ) -> Result<(ModId, Vec<DynamicCSystem>), ModLoadError> {
        let lib_state = shared_lib_state_for_path(path);
        let mut state_guard = lib_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let mod_id = ModId(self.next_mod_id);
        self.next_mod_id += 1;

        let pre_registry = if !world_ptr.is_null() {
            // SAFETY: `world_ptr` points to an exclusively borrowed `World` with no active inner alias.
            let world = unsafe { &*world_ptr };
            world
                .get_resource::<ModRegistry>()
                .cloned()
                .unwrap_or_else(|| (*registry).clone())
        } else {
            (*registry).clone()
        };

        // SAFETY: Caller guarantees `path` points to a valid dynamic library safe to load.
        let lib = unsafe { Library::new(path) }.map_err(|err| ModLoadError::DlOpen {
            path: path.to_path_buf(),
            message: err.to_string(),
        })?;

        let is_active = Arc::new(AtomicBool::new(true));
        // Declared *after* `lib` so Rust's reverse drop order runs `LoadTransaction::drop`
        // (disarming drop trampolines and rolling back `ModRegistry`) *before* `dlclose`.
        let tx = LoadTransaction::new(
            world_ptr,
            mod_id,
            Arc::clone(&is_active),
            pre_registry.clone(),
        );

        // SAFETY: If `bevy_abi_mod_abi_version` is exported, it has signature `unsafe extern "C" fn() -> u32`.
        if let Ok(ver_fn) = unsafe { lib.get::<ModAbiVersionFn>(b"bevy_abi_mod_abi_version\0") } {
            // SAFETY: Invoking side-effect-free version query symbol exported by the mod.
            let guest_version = unsafe { ver_fn() };
            if !is_abi_version_compatible(ABI_VERSION_PACKED, guest_version) {
                return Err(ModLoadError::IncompatibleAbiVersion {
                    path: path.to_path_buf(),
                    host_version: ABI_VERSION_PACKED,
                    guest_version,
                });
            }
        }

        // SAFETY: Resolving standard C-ABI entry point `bevy_abi_mod_init`.
        let init_fn: Symbol<ModInitFn> =
            unsafe { lib.get(b"bevy_abi_mod_init\0") }.map_err(|err| {
                ModLoadError::MissingEntryPoint {
                    path: path.to_path_buf(),
                    symbol: "bevy_abi_mod_init",
                    message: err.to_string(),
                }
            })?;

        // SAFETY: Resolving optional C-ABI teardown point `bevy_abi_mod_shutdown`.
        let shutdown_fn: Option<ModShutdownFn> = unsafe { lib.get(b"bevy_abi_mod_shutdown\0") }
            .ok()
            .map(|sym| *sym);

        let mut init_ctx = ModInitContext::with_world_and_mod_id(
            Arc::clone(&registry),
            Arc::clone(&is_active),
            world_ptr,
            Some(mod_id),
        );

        let status = {
            let _reg_guard = registry.make_current_internal();
            let _init_guard = init_ctx.make_active();
            let ctx_ptr = (&mut init_ctx as *mut ModInitContext).cast::<c_void>();
            // SAFETY: Passing valid `host_get_proc_address`, static `HOST_API`, and active `ModInitContext`.
            unsafe { init_fn(host_get_proc_address, &HOST_API, ctx_ptr) }
        };

        if status == FfiResult::PANIC_CAUGHT {
            return Err(ModLoadError::InitPanicked {
                path: path.to_path_buf(),
            });
        }
        if status.is_err() {
            return Err(ModLoadError::InitFailed {
                path: path.to_path_buf(),
                status,
            });
        }

        let newly_created_components: Vec<ComponentId> = init_ctx
            .registered_components
            .iter()
            .map(|c| c.component_id)
            .filter(|&cid| pre_registry.get_by_id(cid).is_none())
            .collect();

        for sys in &mut init_ctx.registered_systems {
            sys.set_registry(Arc::clone(&init_ctx.registry));
            sys.set_mod_id(mod_id);
        }

        tx.commit();
        state_guard.active_count += 1;
        drop(state_guard);

        let mod_name = canonical_mod_name_from_path(path);
        let generation = *self.mod_generations.get(&mod_name).unwrap_or(&0);
        self.loaded_libraries.push(LoadedMod {
            id: mod_id,
            mod_name,
            generation,
            name: path.to_string_lossy().to_string(),
            path: path.to_path_buf(),
            library: lib,
            shutdown_fn,
            is_active,
            lib_state,
            registered_named_sets: Vec::new(),
            newly_created_components,
        });

        Ok((mod_id, init_ctx.registered_systems))
    }

    /// Loads a native mod library against `world` (crate-internal; external callers use `load_mod_in_world`).
    ///
    /// # Safety
    /// Caller must ensure `path` points to a trusted shared library.
    pub(crate) unsafe fn try_load_mod_tracked_with_world<P: AsRef<Path>>(
        &mut self,
        path: P,
        world: &mut World,
    ) -> Result<(ModId, Vec<DynamicCSystem>), ModLoadError> {
        if !world.contains_resource::<ModRegistry>() {
            world.insert_resource(ModRegistry::new());
        }
        let registry = Arc::new(world.get_resource::<ModRegistry>().unwrap().clone());
        let world_ptr = world as *mut World;
        // SAFETY: Caller upholds library validity; `world_ptr` points to exclusive `&mut World`.
        unsafe { self.load_mod_core(path.as_ref(), registry, world_ptr) }
    }

    /// Returns `true` if a mod with the given [`ModId`] is currently loaded.
    pub fn contains_mod(&self, id: ModId) -> bool {
        self.loaded_libraries.iter().any(|m| m.id == id)
    }

    /// Borrows the [`LoadedMod`] entry for `id`, if currently loaded.
    pub fn get_mod(&self, id: ModId) -> Option<&LoadedMod> {
        self.loaded_libraries.iter().find(|m| m.id == id)
    }

    /// Returns a slice of all currently loaded mod records.
    pub fn loaded_mods(&self) -> &[LoadedMod] {
        &self.loaded_libraries
    }

    /// Returns the number of currently loaded mod libraries.
    pub fn len(&self) -> usize {
        self.loaded_libraries.len()
    }

    /// Returns `true` if no mod libraries are currently loaded.
    pub fn is_empty(&self) -> bool {
        self.loaded_libraries.is_empty()
    }

    pub(crate) fn get_mod_mut(&mut self, id: ModId) -> Option<&mut LoadedMod> {
        self.loaded_libraries.iter_mut().find(|m| m.id == id)
    }

    pub(crate) fn deactivate_mod(&self, id: ModId) {
        if let Some(m) = self.loaded_libraries.iter().find(|m| m.id == id) {
            prune_drop_trampolines_for_guard(&m.is_active);
        }
    }

    /// Crate-internal unload of a single module after schedule excision and registry eviction.
    ///
    /// # Safety
    /// Caller must ensure `id` is not concurrently executing on any worker thread.
    pub(crate) unsafe fn try_unload_mod(&mut self, id: ModId) -> Result<PathBuf, ModLoadError> {
        let pos = self
            .loaded_libraries
            .iter()
            .position(|m| m.id == id)
            .ok_or(ModLoadError::ModNotFound { id })?;
        let loaded = self.loaded_libraries.remove(pos);
        if !self
            .loaded_libraries
            .iter()
            .any(|m| m.is_active() && m.mod_name == loaded.mod_name)
        {
            *self
                .mod_generations
                .entry(loaded.mod_name.clone())
                .or_insert(0) += 1;
        }
        let path = loaded.path.clone();
        // SAFETY: Caller guarantees unloading this mod is safe with respect to external threads.
        unsafe {
            unload_loaded_mod(loaded);
        }
        Ok(path)
    }

    /// Crate-internal LIFO teardown of all remaining loaded modules (used on `Drop`).
    ///
    /// # Safety
    /// Caller must ensure no external threads are executing code from any loaded library.
    pub(crate) unsafe fn unload_all(&mut self) {
        for loaded in self.loaded_libraries.drain(..).rev() {
            // SAFETY: Caller has exclusive ownership of `ModHost` during teardown.
            unsafe {
                unload_loaded_mod(loaded);
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

/// Ensures [`ModHost`] and [`ModRegistry`] exist in `world` and loads the mod at `path`,
/// returning its [`ModId`] and unscheduled [`DynamicCSystem`]s.
pub fn load_mod_in_world(
    world: &mut World,
    path: &Path,
) -> Result<(ModId, Vec<DynamicCSystem>), ModLoadError> {
    if !world.contains_resource::<ModHost>() {
        world.insert_resource(ModHost::new());
    }
    if !world.contains_resource::<ModRegistry>() {
        world.insert_resource(ModRegistry::new());
    }
    world.resource_scope::<ModHost, _>(|world, mut mod_host| {
        // SAFETY: `world` is exclusively borrowed (`&mut World`) while loading the dynamic library at `path`.
        unsafe { mod_host.try_load_mod_tracked_with_world(path, world) }
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

/// Rolls back and unloads an unscheduled mod (`id`) after strict validation failure, disarming
/// drop trampolines and rolling back `ModRegistry` entries *before* `dlclose`.
pub fn rollback_unscheduled_mod_in_world(world: &mut World, id: ModId) {
    let newly_created = if let Some(mod_host) = world.get_resource::<ModHost>() {
        mod_host.deactivate_mod(id);
        mod_host
            .get_mod(id)
            .map(|m| m.newly_created_components.clone())
            .unwrap_or_default()
    } else {
        return;
    };

    if let Some(mut registry) = world.get_resource_mut::<ModRegistry>() {
        registry.rollback_mod(id, &newly_created);
    }

    if let Some(mut mod_host) = world.get_resource_mut::<ModHost>() {
        // SAFETY: `world` is exclusively borrowed, drop trampolines and registry entries are
        // already rolled back above, and none of `id`'s systems were added to any schedule.
        let _ = unsafe { mod_host.try_unload_mod(id) };
    }
}

/// Deactivates `id`, excises all [`ModSystemSet::Mod`]`(id)` systems and owned [`ModNamedSystemSet`]s
/// from `world`'s [`Schedules`], evicts guest-owned VTables/symbols in [`ModRegistry`], runs
/// `before_dlclose(world)`, and finally invokes guest teardown and `dlclose`.
pub fn excise_and_unload_mod_in_world<F: FnOnce(&mut World)>(
    world: &mut World,
    id: ModId,
    before_dlclose: F,
) -> Result<(PathBuf, usize), ModLoadError> {
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
                // Remove owned `ModNamedSystemSet`s first while their systems are still attached,
                // then remove `ModSystemSet::Mod(id)` so both set containers are cleanly excised.
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

    before_dlclose(world);

    let unloaded_path = {
        let mut mod_host = world
            .get_resource_mut::<ModHost>()
            .ok_or(ModLoadError::ModNotFound { id })?;
        // SAFETY: `world` is exclusively borrowed, drop trampolines are disarmed, schedule sets
        // are excised, and guest VTable/symbol pointers are evicted prior to `dlclose`.
        unsafe { mod_host.try_unload_mod(id)? }
    };

    Ok((unloaded_path, removed_systems))
}

/// Unloads all currently loaded mods in `world` in LIFO order under exclusive `&mut World` access.
pub fn unload_all_mods_in_world<F>(world: &mut World, mut on_each_unloaded: F)
where
    F: FnMut(&mut World, ModId, PathBuf, usize),
{
    let ids_lifo: Vec<ModId> = world
        .get_resource::<ModHost>()
        .map(|h| h.loaded_mods().iter().rev().map(LoadedMod::id).collect())
        .unwrap_or_default();

    for id in ids_lifo {
        if let Ok((path, removed)) = excise_and_unload_mod_in_world(world, id, |_| {}) {
            on_each_unloaded(world, id, path, removed);
        }
    }
}
