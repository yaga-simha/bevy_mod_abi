//! [`ModWorldExt`] exclusive `&mut World` API for registering components/resources/messages/schedules
//! and loading, reloading, or unloading native mods.

#[cfg(feature = "diagnostics")]
use crate::diagnostics::{ensure_diagnostics_resources, register_mod_diagnostics_in_world};
use crate::events::{LoadedModInfo, ModLoaded, ModSystemFaulted, ModUnloaded};
pub use crate::mod_dag::{DirectoryLoadReport, ModDagError, ModDependencyGraph, ModTomlManifest};
use crate::mod_dag::{discover_mod_manifests, resolve_dependency_dag};
use crate::schedule_sets::{
    ModScheduleMap, ModVisibleSets, StartupCompleted, prevalidate_mod_schedules_and_ordering,
    run_late_startup_systems_in_world, schedule_mod_systems_in_world,
};
pub use bevy_abi_mod_host_sys::loader::ModScoped;
use bevy_abi_mod_host_sys::loader::{
    ReloadColumnSnapshot, cleanup_orphaned_mod_scoped_entities, ensure_symbol_tables_in_world,
    excise_and_unload_mod_for_reload_in_world, migrate_mod_scoped_entities,
    restore_mod_dynamic_columns_after_reload, snapshot_mod_dynamic_columns_for_reload,
};
use bevy_abi_mod_host_sys::registry::catalog::ensure_relation_graph_in_world;
use bevy_abi_mod_host_sys::schedule::ModScheduleSetRegistry;
use bevy_abi_mod_host_sys::{
    ComponentCollisionError, ComponentOptions, GuestSystem, LoadOptions, LoadedMod, ModErrorPolicy,
    ModErrorPolicyConfig, ModHost, ModId, ModLoadError, ModQuarantine, ModRegistry,
    PersistentMessageCursors, WorldDropGuard, ensure_children_registered_in_world,
    ensure_dynamic_hook_map_in_world, excise_and_unload_mod_in_world, load_mod_in_world,
    restore_mod_state_in_world, rollback_unscheduled_mod_in_world, save_mod_state_in_world,
    unload_all_mods_in_world,
};
#[cfg(feature = "signature-verification")]
use bevy_abi_mod_host_sys::{ModTrustPolicy, default_signature_path_for, preverify_mod_on_disk};
use bevy_abi_mod_sys::{
    AbiPod, BAM_COMPONENT_FLAG_SPARSE_SET, BAM_COMPONENT_FLAG_SUMMARY_TICK,
    BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_SPARSE_SET, BAM_STORAGE_FLAG_SUMMARY_TICK,
    BamComponent, ModComponent, ScheduleId,
};
use bevy_ecs::component::{Component, StorageType};
use bevy_ecs::entity::Entity;
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::event::{EntityEvent, Event};
use bevy_ecs::hierarchy::ChildOf;
use bevy_ecs::message::{Message, Messages};
use bevy_ecs::name::Name;
use bevy_ecs::prelude::Resource;
use bevy_ecs::query::Allow;
use bevy_ecs::schedule::{ScheduleLabel, Schedules, SystemSet};
use bevy_ecs::world::World;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Callback invoked synchronously under exclusive `&mut World` access whenever a mod is unloaded
/// or torn down during a hot-reload.
pub type ModUnloadHookFn = fn(&mut World, ModId);

/// Safe registry of companion-crate cleanup callbacks executed when a mod unloads or reloads.
///
/// Extension crates (such as `bevy_abi_mod_render` or `bevy_abi_mod_bridges`) register their
/// per-`ModId` cleanup hooks via [`register_mod_unload_hook_in_world`] without introducing upward
/// dependencies into headless `bevy_abi_mod_host`.
#[derive(Resource, Default, Debug, Clone)]
pub struct ModUnloadHooks {
    hooks: Vec<(&'static str, ModUnloadHookFn)>,
}

impl ModUnloadHooks {
    /// Creates an empty [`ModUnloadHooks`] registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `hook` under a stable `key`. Repeated registrations with the same `key` replace
    /// the prior callback idempotently while preserving deterministic order.
    pub fn register(&mut self, key: &'static str, hook: ModUnloadHookFn) {
        if let Some((_, existing)) = self.hooks.iter_mut().find(|(k, _)| *k == key) {
            *existing = hook;
        } else {
            self.hooks.push((key, hook));
        }
    }

    /// Removes the hook registered under `key`, returning `true` if one existed.
    pub fn unregister(&mut self, key: &'static str) -> bool {
        if let Some(pos) = self.hooks.iter().position(|(k, _)| *k == key) {
            self.hooks.remove(pos);
            true
        } else {
            false
        }
    }

    /// Returns the number of registered unload hooks.
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// Returns `true` if no unload hooks are registered.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    fn callbacks(&self) -> Vec<ModUnloadHookFn> {
        self.hooks.iter().map(|(_, f)| *f).collect()
    }
}

/// Registers an idempotent mod-unload cleanup callback inside `world`'s [`ModUnloadHooks`]
/// resource, initializing the resource if absent.
pub fn register_mod_unload_hook_in_world(
    world: &mut World,
    key: &'static str,
    hook: ModUnloadHookFn,
) {
    world.init_resource::<ModUnloadHooks>();
    if let Some(mut hooks) = world.get_resource_mut::<ModUnloadHooks>() {
        hooks.register(key, hook);
    }
}

fn run_mod_unload_hooks(world: &mut World, id: ModId) {
    let callbacks = world
        .get_resource::<ModUnloadHooks>()
        .map(ModUnloadHooks::callbacks)
        .unwrap_or_default();
    for hook in callbacks {
        hook(world, id);
    }
}

pub(crate) fn ensure_hierarchy_registered(world: &mut World) {
    let child_of_id = world.register_component::<ChildOf>();
    let child_of_immutable = world
        .components()
        .get_info(child_of_id)
        .is_some_and(|info| !info.mutable());
    let _ = with_world_registry(world, |reg| {
        reg.register_hierarchy(child_of_id, child_of_immutable)
    });
    // Register Bevy's built-in `Children` upfront (handle `2`) alongside `ChildOf` (handle `1`) so
    // zero-copy `sys_entity_children` traversal never mutates the registry mid-session.
    ensure_children_registered_in_world(world);
}

pub(crate) fn ensure_world_mod_resources(world: &mut World) {
    world.init_resource::<WorldDropGuard>();
    ensure_dynamic_hook_map_in_world(world);
    world.init_resource::<PersistentMessageCursors>();
    world.init_resource::<ModRegistry>();
    world.init_resource::<ModHost>();
    world.init_resource::<ModQuarantine>();
    world.init_resource::<ModScheduleMap>();
    world.init_resource::<ModVisibleSets>();
    world.init_resource::<ModScheduleSetRegistry>();
    world.init_resource::<ModDependencyGraph>();
    world.init_resource::<ModUnloadHooks>();
    world.init_resource::<StartupCompleted>();
    ensure_symbol_tables_in_world(world);
    ensure_relation_graph_in_world(world);
    #[cfg(feature = "signature-verification")]
    world.init_resource::<ModTrustPolicy>();
    #[cfg(feature = "diagnostics")]
    ensure_diagnostics_resources(world);
    world.init_resource::<Messages<ModLoaded>>();
    world.init_resource::<Messages<ModUnloaded>>();
    world.init_resource::<Messages<ModSystemFaulted>>();
    ensure_hierarchy_registered(world);
}

fn with_world_registry<R>(world: &mut World, f: impl FnOnce(&mut ModRegistry) -> R) -> R {
    if let Some(mut registry) = world.get_resource_mut::<ModRegistry>() {
        f(&mut registry)
    } else {
        let mut registry = ModRegistry::new();
        let out = f(&mut registry);
        world.insert_resource(registry);
        out
    }
}

fn despawn_loaded_mod_entities(world: &mut World, id: ModId) {
    world.despawn_all_where::<&LoadedModInfo, Allow<Disabled>>(|info| info.id == id);
}

fn emit_mod_unloaded_once(
    world: &mut World,
    id: ModId,
    name: &str,
    path: PathBuf,
    removed_systems: usize,
) {
    if let Some(mut msgs) = world.get_resource_mut::<Messages<ModUnloaded>>() {
        msgs.write(ModUnloaded {
            id,
            name: name.to_string(),
            path: Some(path),
            removed_systems,
        });
    }
}

fn finalize_mod_load_in_world(
    world: &mut World,
    mod_id: ModId,
    name: String,
    version: String,
    path: PathBuf,
    system_count: usize,
) -> Entity {
    #[cfg(feature = "diagnostics")]
    register_mod_diagnostics_in_world(world, &name);

    let entity = world
        .spawn((
            Name::new(format!("mod:{name}")),
            LoadedModInfo {
                id: mod_id,
                name: name.clone(),
                version: version.clone(),
                path: path.clone(),
                system_count,
                fault_count: 0,
            },
        ))
        .id();

    #[cfg(feature = "signature-verification")]
    if let Some(verified) = world
        .get_resource::<ModHost>()
        .and_then(|host| host.get_mod(mod_id))
        .and_then(|loaded| loaded.verified_signature())
        .cloned()
        && let Ok(mut entity_mut) = world.get_entity_mut(entity)
    {
        entity_mut.insert(verified);
    }

    if let Some(mut host) = world.get_resource_mut::<ModHost>() {
        host.set_mod_entity_and_system_count(mod_id, entity, system_count);
    }

    if let Some(mut msgs) = world.get_resource_mut::<Messages<ModLoaded>>() {
        msgs.write(ModLoaded {
            id: mod_id,
            entity,
            name,
            version,
            path,
            system_count,
        });
    }

    entity
}

fn rollback_and_cleanup_reload(
    world: &mut World,
    mod_id: ModId,
    systems: Vec<GuestSystem>,
    orphan_old_id: Option<ModId>,
) {
    drop(systems);
    rollback_unscheduled_mod_in_world(world, mod_id);
    if let Some(old_id) = orphan_old_id {
        cleanup_orphaned_mod_scoped_entities(world, old_id);
    }
}

fn load_mod_with_optional_state_and_migration(
    world: &mut World,
    path: &Path,
    options: LoadOptions,
    saved_state: Option<&[u8]>,
    reload_migration: Option<(ModId, ReloadColumnSnapshot)>,
) -> Result<ModId, ModLoadError> {
    ensure_world_mod_resources(world);
    let path_buf = path.to_path_buf();
    let orphan_old_id = reload_migration.as_ref().map(|(old_id, _)| *old_id);

    let (mod_id, systems) = match load_mod_in_world(world, &path_buf, &options) {
        Ok(res) => res,
        Err(err) => {
            if let Some(old_id) = orphan_old_id {
                cleanup_orphaned_mod_scoped_entities(world, old_id);
            }
            return Err(err);
        }
    };

    let (mod_name, mod_version) = world
        .get_resource::<ModHost>()
        .and_then(|h| h.get_mod(mod_id))
        .map(|m| (m.name().to_string(), m.version().to_string()))
        .unwrap_or_else(|| ("unnamed_mod".to_string(), "0.0.0".to_string()));

    if options.strict
        && let Some(registry) = world.get_resource::<ModRegistry>()
    {
        for sys in &systems {
            if let Err(missing) = sys.validate_against_registry(registry) {
                let sys_name = sys.system_name().to_string();
                rollback_and_cleanup_reload(world, mod_id, systems, orphan_old_id);
                return Err(ModLoadError::MissingComponents {
                    path: path_buf,
                    system_name: sys_name,
                    missing,
                });
            }
        }
    }

    if let Err(err) = prevalidate_mod_schedules_and_ordering(
        world,
        &path_buf,
        &mod_name,
        options.strict,
        &systems,
    ) {
        rollback_and_cleanup_reload(world, mod_id, systems, orphan_old_id);
        return Err(err);
    }

    if let Some(blob) = saved_state
        && let Err(err) = restore_mod_state_in_world(world, mod_id, blob)
    {
        rollback_and_cleanup_reload(world, mod_id, systems, orphan_old_id);
        return Err(err);
    }

    if let Some((old_id, col_snapshot)) = reload_migration {
        if let Err(err) = restore_mod_dynamic_columns_after_reload(world, mod_id, col_snapshot) {
            rollback_and_cleanup_reload(world, mod_id, systems, Some(old_id));
            return Err(err);
        }
        migrate_mod_scoped_entities(world, old_id, mod_id);
    }

    let system_count = systems.len();
    let late_startup = schedule_mod_systems_in_world(world, mod_id, &mod_name, systems);

    finalize_mod_load_in_world(
        world,
        mod_id,
        mod_name.clone(),
        mod_version,
        path_buf.clone(),
        system_count,
    );
    let (dag_name, sidecar_deps) = path_buf
        .parent()
        .map(|dir| dir.join("mod.toml"))
        .filter(|p| p.is_file())
        .and_then(|toml_path| ModTomlManifest::from_file(&toml_path).ok())
        .map(|manifest| {
            let deps = manifest
                .dependencies
                .keys()
                .chain(manifest.optional_dependencies.keys())
                .cloned()
                .collect::<Vec<String>>();
            (manifest.name, deps)
        })
        .unwrap_or_else(|| (mod_name.clone(), Vec::new()));
    if let Some(mut dag) = world.get_resource_mut::<ModDependencyGraph>() {
        dag.register_loaded_mod(mod_id, dag_name, path_buf, options, sidecar_deps);
    }
    run_late_startup_systems_in_world(world, &mod_name, late_startup);

    Ok(mod_id)
}

fn unload_single_mod_in_world(
    world: &mut World,
    id: ModId,
    for_reload: bool,
) -> Result<(), ModLoadError> {
    let (mod_name, unloaded_path, removed_systems) = if for_reload {
        excise_and_unload_mod_for_reload_in_world(world, id, |w| {
            despawn_loaded_mod_entities(w, id);
        })?
    } else {
        excise_and_unload_mod_in_world(world, id, |w| {
            despawn_loaded_mod_entities(w, id);
        })?
    };
    run_mod_unload_hooks(world, id);
    if let Some(mut dag) = world.get_resource_mut::<ModDependencyGraph>() {
        dag.unregister_mod(id);
    }
    emit_mod_unloaded_once(world, id, &mod_name, unloaded_path, removed_systems);
    Ok(())
}

struct CascadeReloadEntry {
    old_id: ModId,
    name: String,
    target_path: PathBuf,
    options: LoadOptions,
    dependencies: Vec<String>,
}

fn resolve_cascade_ids(world: &World, id: ModId) -> Result<Vec<ModId>, ModLoadError> {
    let active_ids: BTreeSet<ModId> = {
        let Some(host) = world
            .get_resource::<ModHost>()
            .filter(|h| h.contains_mod(id))
        else {
            return Err(ModLoadError::ModNotFound { id });
        };
        host.loaded_mods().iter().map(LoadedMod::id).collect()
    };
    Ok(world
        .get_resource::<ModDependencyGraph>()
        .map(|dag| dag.cascading_order(id, &active_ids))
        .unwrap_or_else(|| vec![id]))
}

fn reload_mod_cascade(
    world: &mut World,
    id: ModId,
    override_path: Option<&Path>,
    options: LoadOptions,
) -> Result<ModId, ModLoadError> {
    ensure_world_mod_resources(world);
    let forward_ids = resolve_cascade_ids(world, id)?;

    let mut plan = Vec::with_capacity(forward_ids.len());
    for tid in forward_ids {
        let (loaded_name, current_path) = {
            let Some(loaded) = world
                .get_resource::<ModHost>()
                .and_then(|host| host.get_mod(tid))
            else {
                return Err(ModLoadError::ModNotFound { id: tid });
            };
            (
                loaded.name().to_string(),
                loaded.source_path().to_path_buf(),
            )
        };
        let (dag_name, recorded_opts, dependencies) = world
            .get_resource::<ModDependencyGraph>()
            .and_then(|dag| dag.get_node(tid))
            .map(|n| (n.name.clone(), n.options.clone(), n.dependencies.clone()))
            .unwrap_or_else(|| (loaded_name, LoadOptions::default(), Vec::new()));

        let target_path = if tid == id {
            override_path.map(Path::to_path_buf).unwrap_or(current_path)
        } else {
            current_path
        };
        let target_opts = if tid == id {
            options.clone()
        } else {
            recorded_opts.with_shadow_copy(true)
        };

        #[cfg(feature = "signature-verification")]
        {
            let override_arg = (tid == id).then_some(target_path.as_path());
            preverify_reload_target(world, tid, override_arg, &target_opts)?;
        }

        plan.push(CascadeReloadEntry {
            old_id: tid,
            name: dag_name,
            target_path,
            options: target_opts,
            dependencies,
        });
    }

    // Save state in reverse topological order (`C -> B -> A`) BEFORE unloading any mod so a
    // `save_state` failure leaves the running dependency subgraph untouched.
    let mut saved_states: Vec<Option<Vec<u8>>> = vec![None; plan.len()];
    for idx in (0..plan.len()).rev() {
        let (_path, blob) = save_mod_state_in_world(world, plan[idx].old_id)?;
        saved_states[idx] = blob;
    }

    // Snapshot live dynamic component columns & unload in reverse topological order (`C -> B -> A`),
    // preserving `ModScoped(old_id)` entities across the reload.
    let mut column_snapshots: Vec<Option<ReloadColumnSnapshot>> = vec![None; plan.len()];
    for idx in (0..plan.len()).rev() {
        let col_snap = snapshot_mod_dynamic_columns_for_reload(world, plan[idx].old_id)?;
        column_snapshots[idx] = Some(col_snap);
        unload_single_mod_in_world(world, plan[idx].old_id, true)?;
    }

    // Reload in forward topological order (`A -> B -> C`), restoring state and migrating
    // dynamic columns + `ModScoped` ownership onto each reloaded `ModId`.
    let mut new_root_id: Option<ModId> = None;
    let mut plan_iter = plan.into_iter().enumerate();
    while let Some((idx, item)) = plan_iter.next() {
        let col_snap = column_snapshots[idx].take().unwrap_or_default();
        let new_id = match load_mod_with_optional_state_and_migration(
            world,
            &item.target_path,
            item.options.clone(),
            saved_states[idx].as_deref(),
            Some((item.old_id, col_snap)),
        ) {
            Ok(id) => id,
            Err(err) => {
                for (_, remaining) in plan_iter {
                    cleanup_orphaned_mod_scoped_entities(world, remaining.old_id);
                }
                return Err(err);
            }
        };
        if let Some(mut dag) = world.get_resource_mut::<ModDependencyGraph>() {
            dag.register_loaded_mod(
                new_id,
                item.name,
                item.target_path,
                item.options,
                item.dependencies,
            );
        }
        if item.old_id == id {
            new_root_id = Some(new_id);
        }
    }

    new_root_id.ok_or(ModLoadError::ModNotFound { id })
}

/// Exclusive `&mut World` extension trait for exposing Bevy ECS types and managing native C-ABI
/// mods at startup or inside exclusive Bevy systems.
pub trait ModWorldExt {
    /// Registers a Bevy [`Component`] implementing [`ModComponent`] with [`ComponentOptions`],
    /// returning its 1-based [`BamComponent`] handle.
    fn register_mod_component<T: Component + ModComponent>(
        &mut self,
        options: ComponentOptions<T>,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Registers a singleton Bevy [`Resource`] implementing [`ModComponent`], returning its
    /// 1-based [`BamComponent`] handle.
    fn register_mod_resource<R: Resource + ModComponent>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Registers a buffered POD Bevy [`Message`] type (`Messages<M>`), initializing `Messages<M>`
    /// if absent and returning its 1-based [`BamComponent`] handle.
    fn register_mod_message<M: Message + ModComponent + AbiPod>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Registers a VTable-backed non-POD Bevy [`Message`] type (`Messages<M>`, e.g. containing
    /// `String` or `Vec<T>` fields), initializing `Messages<M>` if absent and returning its 1-based
    /// [`BamComponent`] handle. Guests read items through the `"bam.v0.<M>.export_fields"` witness
    /// and send field packets reconstructed on the host allocator via `"bam.v0.<M>.import_field"`.
    fn register_mod_non_pod_message<M: Message + ModComponent + Default>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Registers a triggerable host Bevy [`Event`] implementing [`ModComponent`] + [`Copy`],
    /// returning its 1-based [`BamComponent`] handle for deferred triggers and guest observers.
    fn register_mod_event<E>(&mut self) -> Result<BamComponent, ComponentCollisionError>
    where
        E: Event<Trigger<'static>: Default> + ModComponent + Copy;

    /// Registers a triggerable entity-targeted Bevy [`EntityEvent`] implementing [`ModComponent`]
    /// + [`Copy`], returning its 1-based [`BamComponent`] handle for deferred triggers and observers.
    fn register_mod_entity_event<E>(&mut self) -> Result<BamComponent, ComponentCollisionError>
    where
        E: EntityEvent + Event + ModComponent + Copy,
        for<'a> E::Trigger<'a>: Default;

    /// Registers a non-component [`ModComponent`] helper/nested struct type so its static VTable
    /// and field accessors are resolvable via `get_proc_address`.
    fn register_mod_type<T: ModComponent>(&mut self) -> &mut Self;

    /// Maps a game-defined custom [`ScheduleId`] (`id.0 >= ScheduleId::CUSTOM_BASE` = `1024`) to a
    /// Bevy [`ScheduleLabel`] in [`ModScheduleMap`].
    fn register_mod_schedule<S: ScheduleLabel>(
        &mut self,
        id: ScheduleId,
        label: S,
    ) -> Result<(), ModLoadError>;

    /// Exposes a host [`SystemSet`] under `"host::<name>"` in [`ModVisibleSets`] so guest systems
    /// can declare `.after(c"host::<name>")` or `.before(c"host::<name>")`.
    fn register_mod_visible_set<S: SystemSet>(&mut self, name: &str, set: S) -> bool;

    /// Sets the default [`ModErrorPolicy`] applied to newly initialized guest systems in this [`World`].
    fn set_mod_error_policy(&mut self, policy: ModErrorPolicy) -> &mut Self;

    /// Configures the [`ModTrustPolicy`] enforced when loading or reloading native mods in this
    /// [`World`].
    #[cfg(feature = "signature-verification")]
    fn set_mod_trust_policy(&mut self, policy: ModTrustPolicy) -> &mut Self;

    /// Re-evaluates all currently loaded mods against the active [`ModTrustPolicy`] (after loading
    /// a KRL, revoking keys/serials/`key_id`s, or rotating anchors/principals) and unloads any mod
    /// whose verification receipt is no longer trusted, returning the evicted [`ModId`]s.
    #[cfg(feature = "signature-verification")]
    fn evict_revoked_mods(&mut self) -> Vec<ModId>;

    /// Loads a native shared-library mod using [`LoadOptions::default()`], returning its [`ModId`].
    fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError>;

    /// Loads a native shared-library mod with explicit [`LoadOptions`] (shadow copy, schedule
    /// override, strict validation, per-mod error policy), returning its [`ModId`].
    fn load_mod_with<P: AsRef<Path>>(
        &mut self,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;

    /// Unloads a loaded mod by [`ModId`] under exclusive [`World`] access: excises schedule sets,
    /// disarms drop trampolines, evicts guest VTables/symbols, despawns its [`LoadedModInfo`]
    /// entity, invokes `bevy_abi_mod_shutdown(instance)`, unmaps the library, unlinks any shadow
    /// copy, and emits [`ModUnloaded`].
    fn unload_mod(&mut self, id: ModId) -> Result<(), ModLoadError>;

    /// Unloads all currently loaded mods in LIFO order under exclusive [`World`] access.
    fn unload_all_mods(&mut self) -> &mut Self;

    /// Hot-reloads `id` from its original source path with shadow-copying enabled, snapshotting
    /// optional `BamManifest::save_state` bytes before `shutdown` and invoking `restore_state`
    /// on the new instance.
    fn reload_mod(&mut self, id: ModId) -> Result<ModId, ModLoadError>;

    /// Hot-reloads `id` from its original source path with explicit [`LoadOptions`], transferring
    /// optional `save_state` / `restore_state` bytes.
    fn reload_mod_with(&mut self, id: ModId, options: LoadOptions) -> Result<ModId, ModLoadError>;

    /// Hot-reloads `id` from a replacement shared-library `path` with explicit [`LoadOptions`],
    /// transferring optional `save_state` / `restore_state` bytes.
    fn reload_mod_from<P: AsRef<Path>>(
        &mut self,
        id: ModId,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;

    /// Discovers `mod.toml` manifests inside `dir`, validates SemVer constraints, quarantines
    /// cyclic dependency subgraphs, and loads all valid acyclic mods in Kahn topological order
    /// using [`LoadOptions::default()`].
    fn load_mods_dir<P: AsRef<Path>>(&mut self, dir: P)
    -> Result<DirectoryLoadReport, ModDagError>;

    /// Discovers `mod.toml` manifests inside `dir`, validates SemVer constraints, quarantines
    /// cyclic dependency subgraphs, and loads all valid acyclic mods in Kahn topological order
    /// using `options`.
    fn load_mods_dir_with<P: AsRef<Path>>(
        &mut self,
        dir: P,
        options: LoadOptions,
    ) -> Result<DirectoryLoadReport, ModDagError>;
}

impl ModWorldExt for World {
    fn register_mod_component<T: Component + ModComponent>(
        &mut self,
        options: ComponentOptions<T>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let comp_id = self.register_component::<T>();
        let mut inferred_flags = 0u32;
        let wants_summary_tick = (options.extra_storage_flags() & BAM_STORAGE_FLAG_SUMMARY_TICK)
            != 0
            || (T::COMPONENT_FLAGS & BAM_COMPONENT_FLAG_SUMMARY_TICK) != 0;
        let wants_sparse_set = (options.extra_storage_flags() & BAM_STORAGE_FLAG_SPARSE_SET) != 0
            || (T::COMPONENT_FLAGS & BAM_COMPONENT_FLAG_SPARSE_SET) != 0;
        if let Some(info) = self.components().get_info(comp_id) {
            if wants_summary_tick && (info.storage_type() != StorageType::Table || wants_sparse_set)
            {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: T::CANONICAL_NAME.to_string(),
                    reason: "BAM_STORAGE_FLAG_SUMMARY_TICK requires Table storage and cannot be combined with SparseSet".to_string(),
                });
            }
            if wants_summary_tick && !info.summary_tick() {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: T::CANONICAL_NAME.to_string(),
                    reason: "BAM_STORAGE_FLAG_SUMMARY_TICK requested, but Bevy Component does not enable #[component(summary_tick)]".to_string(),
                });
            }
            if info.summary_tick() && wants_sparse_set {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: T::CANONICAL_NAME.to_string(),
                    reason: "component enables #[component(summary_tick)] (Table storage) and cannot be registered with BAM_STORAGE_FLAG_SPARSE_SET".to_string(),
                });
            }
            if info.storage_type() == StorageType::SparseSet {
                inferred_flags |= BAM_STORAGE_FLAG_SPARSE_SET;
            }
            if !info.mutable() {
                inferred_flags |= BAM_STORAGE_FLAG_IMMUTABLE;
            }
            if info.summary_tick() {
                inferred_flags |= BAM_STORAGE_FLAG_SUMMARY_TICK;
            }
        }
        with_world_registry(self, |reg| {
            reg.register_component_with_world_flags::<T>(comp_id, options, inferred_flags)
        })
    }

    fn register_mod_resource<R: Resource + ModComponent>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let comp_id = self.register_component::<R>();
        with_world_registry(self, |reg| reg.register_resource::<R>(comp_id))
    }

    fn register_mod_message<M: Message + ModComponent + AbiPod>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError> {
        self.init_resource::<Messages<M>>();
        let messages_res_id = self.register_component::<Messages<M>>();
        with_world_registry(self, |reg| reg.register_message::<M>(messages_res_id))
    }

    fn register_mod_non_pod_message<M: Message + ModComponent + Default>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError> {
        self.init_resource::<Messages<M>>();
        let messages_res_id = self.register_component::<Messages<M>>();
        with_world_registry(self, |reg| {
            reg.register_non_pod_message::<M>(messages_res_id)
        })
    }

    fn register_mod_event<E>(&mut self) -> Result<BamComponent, ComponentCollisionError>
    where
        E: Event<Trigger<'static>: Default> + ModComponent + Copy,
    {
        let event_key = self.register_event_key::<E>();
        with_world_registry(self, |reg| reg.register_event::<E>(event_key))
    }

    fn register_mod_entity_event<E>(&mut self) -> Result<BamComponent, ComponentCollisionError>
    where
        E: EntityEvent + Event + ModComponent + Copy,
        for<'a> E::Trigger<'a>: Default,
    {
        let event_key = self.register_event_key::<E>();
        with_world_registry(self, |reg| reg.register_entity_event::<E>(event_key))
    }

    fn register_mod_type<T: ModComponent>(&mut self) -> &mut Self {
        with_world_registry(self, ModRegistry::register_type::<T>);
        self
    }

    fn register_mod_schedule<S: ScheduleLabel>(
        &mut self,
        id: ScheduleId,
        label: S,
    ) -> Result<(), ModLoadError> {
        if !id.is_custom() {
            return Err(ModLoadError::InvalidScheduleRegistration { schedule: id });
        }
        let interned = label.intern();
        self.init_resource::<Schedules>();
        if let Some(mut schedules) = self.get_resource_mut::<Schedules>() {
            let _ = schedules.entry(interned);
        }
        self.init_resource::<ModScheduleMap>();
        if let Some(mut map) = self.get_resource_mut::<ModScheduleMap>() {
            let ok = map.register(id, interned);
            debug_assert!(ok);
        }
        Ok(())
    }

    fn register_mod_visible_set<S: SystemSet>(&mut self, name: &str, set: S) -> bool {
        self.init_resource::<ModVisibleSets>();
        self.get_resource_mut::<ModVisibleSets>()
            .is_some_and(|mut sets| sets.register(name, set))
    }

    fn set_mod_error_policy(&mut self, policy: ModErrorPolicy) -> &mut Self {
        self.insert_resource(ModErrorPolicyConfig(policy));
        self
    }

    #[cfg(feature = "signature-verification")]
    fn set_mod_trust_policy(&mut self, policy: ModTrustPolicy) -> &mut Self {
        self.insert_resource(policy);
        self
    }

    #[cfg(feature = "signature-verification")]
    fn evict_revoked_mods(&mut self) -> Vec<ModId> {
        ensure_world_mod_resources(self);
        let Some(policy) = self
            .get_resource::<ModTrustPolicy>()
            .filter(|p| p.is_enforced())
            .cloned()
        else {
            return Vec::new();
        };

        let to_evict: Vec<ModId> = self
            .get_resource::<ModHost>()
            .map(|host| {
                host.loaded_mods()
                    .iter()
                    .filter_map(|loaded| {
                        let still_valid = match loaded.verified_signature() {
                            Some(receipt) => {
                                policy.verify_receipt_still_trusted(receipt).is_ok()
                                    && policy
                                        .verify_manifest_principal(receipt, loaded.name())
                                        .is_ok()
                            }
                            None => false,
                        };
                        (!still_valid).then_some(loaded.id())
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut evicted = Vec::with_capacity(to_evict.len());
        for id in to_evict {
            if self.unload_mod(id).is_ok() {
                evicted.push(id);
            }
        }
        evicted
    }

    fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError> {
        self.load_mod_with(path, LoadOptions::default())
    }

    fn load_mod_with<P: AsRef<Path>>(
        &mut self,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        load_mod_with_optional_state_and_migration(self, path.as_ref(), options, None, None)
    }

    fn unload_mod(&mut self, id: ModId) -> Result<(), ModLoadError> {
        ensure_world_mod_resources(self);
        let forward_ids = resolve_cascade_ids(self, id)?;
        for target_id in forward_ids.into_iter().rev() {
            unload_single_mod_in_world(self, target_id, false)?;
        }
        Ok(())
    }

    fn unload_all_mods(&mut self) -> &mut Self {
        ensure_world_mod_resources(self);
        unload_all_mods_in_world(self, |world, id, name, path, removed_systems| {
            despawn_loaded_mod_entities(world, id);
            run_mod_unload_hooks(world, id);
            emit_mod_unloaded_once(world, id, &name, path, removed_systems);
        });
        if let Some(mut dag) = self.get_resource_mut::<ModDependencyGraph>() {
            dag.clear();
        }
        self
    }

    fn reload_mod(&mut self, id: ModId) -> Result<ModId, ModLoadError> {
        self.reload_mod_with(id, LoadOptions::default().with_shadow_copy(true))
    }

    fn reload_mod_with(&mut self, id: ModId, options: LoadOptions) -> Result<ModId, ModLoadError> {
        reload_mod_cascade(self, id, None, options)
    }

    fn reload_mod_from<P: AsRef<Path>>(
        &mut self,
        id: ModId,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        reload_mod_cascade(self, id, Some(path.as_ref()), options)
    }

    fn load_mods_dir<P: AsRef<Path>>(
        &mut self,
        dir: P,
    ) -> Result<DirectoryLoadReport, ModDagError> {
        self.load_mods_dir_with(dir, LoadOptions::default())
    }

    fn load_mods_dir_with<P: AsRef<Path>>(
        &mut self,
        dir: P,
        options: LoadOptions,
    ) -> Result<DirectoryLoadReport, ModDagError> {
        ensure_world_mod_resources(self);
        let (manifests, mut errors) = discover_mod_manifests(dir.as_ref())?;

        let mut already_loaded: BTreeMap<String, semver::Version> = BTreeMap::new();
        let mut loaded_names: BTreeSet<String> = BTreeSet::new();
        if let Some(host) = self.get_resource::<ModHost>() {
            for loaded in host.loaded_mods() {
                loaded_names.insert(loaded.name().to_string());
                if let Ok(ver) = semver::Version::parse(loaded.version()) {
                    already_loaded.insert(loaded.name().to_string(), ver);
                }
            }
        }

        let resolution = resolve_dependency_dag(manifests, &already_loaded);
        errors.extend(resolution.diagnostics);

        let mut loaded = Vec::new();
        let mut failed_mods: BTreeSet<String> = BTreeSet::new();

        for manifest in resolution.load_order {
            let mut missing_dep: Option<(String, semver::VersionReq)> = None;
            for (dep, req) in &manifest.dependencies {
                if failed_mods.contains(dep) || !loaded_names.contains(dep) {
                    missing_dep = Some((dep.clone(), req.clone()));
                    break;
                }
            }
            if let Some((dep, req)) = missing_dep {
                errors.push(ModDagError::MissingDependency {
                    mod_name: manifest.name.clone(),
                    dependency: dep,
                    required: req,
                });
                failed_mods.insert(manifest.name);
                continue;
            }

            let lib_path = match manifest.resolve_library_path() {
                Ok(p) => p,
                Err(err) => {
                    errors.push(err.into());
                    failed_mods.insert(manifest.name);
                    continue;
                }
            };

            let mut active_deps: Vec<String> = manifest.dependencies.keys().cloned().collect();
            for opt_dep in manifest.optional_dependencies.keys() {
                if loaded_names.contains(opt_dep) && !active_deps.contains(opt_dep) {
                    active_deps.push(opt_dep.clone());
                }
            }

            match load_mod_with_optional_state_and_migration(
                self,
                &lib_path,
                options.clone(),
                None,
                None,
            ) {
                Ok(mod_id) => {
                    if let Some(mut dag) = self.get_resource_mut::<ModDependencyGraph>() {
                        dag.register_loaded_mod(
                            mod_id,
                            manifest.name.clone(),
                            lib_path,
                            options.clone(),
                            active_deps,
                        );
                    }
                    loaded_names.insert(manifest.name.clone());
                    loaded.push((manifest.name, mod_id));
                }
                Err(source) => {
                    errors.push(ModDagError::LoadFailed {
                        mod_name: manifest.name.clone(),
                        path: lib_path,
                        source: Box::new(source),
                    });
                    failed_mods.insert(manifest.name);
                }
            }
        }

        Ok(DirectoryLoadReport {
            loaded,
            quarantined_cycles: resolution.quarantined_cycles,
            errors,
        })
    }
}

#[cfg(feature = "signature-verification")]
fn preverify_reload_target(
    world: &World,
    id: ModId,
    override_path: Option<&Path>,
    options: &LoadOptions,
) -> Result<(), ModLoadError> {
    let Some(policy) = world
        .get_resource::<ModTrustPolicy>()
        .filter(|p| p.is_enforced())
    else {
        return Ok(());
    };
    let loaded = world
        .get_resource::<ModHost>()
        .and_then(|h| h.get_mod(id))
        .ok_or(ModLoadError::ModNotFound { id })?;
    let target_path = override_path.unwrap_or_else(|| loaded.source_path());
    let receipt = preverify_mod_on_disk(target_path, options, policy)?;
    if let Err(source) = policy.verify_manifest_principal(&receipt, loaded.name()) {
        let signature_path = options
            .signature_path
            .clone()
            .unwrap_or_else(|| default_signature_path_for(target_path));
        return Err(ModLoadError::SignatureVerification {
            path: target_path.to_path_buf(),
            signature_path,
            source,
        });
    }
    Ok(())
}
