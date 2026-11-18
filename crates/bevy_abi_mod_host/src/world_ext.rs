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
use bevy_abi_mod_host_sys::{
    ComponentCollisionError, ComponentOptions, LoadOptions, LoadedMod, ModErrorPolicy,
    ModErrorPolicyConfig, ModHost, ModId, ModLoadError, ModQuarantine, ModRegistry, WorldDropGuard,
    excise_and_unload_mod_in_world, load_mod_in_world, restore_mod_state_in_world,
    rollback_unscheduled_mod_in_world, save_mod_state_in_world, unload_all_mods_in_world,
};
#[cfg(feature = "signature-verification")]
use bevy_abi_mod_host_sys::{ModTrustPolicy, default_signature_path_for, preverify_mod_on_disk};
use bevy_abi_mod_sys::{
    AbiPod, BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_SPARSE_SET, BamComponent, ModComponent,
    ScheduleId,
};
use bevy_ecs::component::{Component, StorageType};
use bevy_ecs::entity::Entity;
use bevy_ecs::event::{EntityEvent, Event};
use bevy_ecs::hierarchy::ChildOf;
use bevy_ecs::message::{Message, Messages};
use bevy_ecs::name::Name;
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::{ScheduleLabel, Schedules, SystemSet};
use bevy_ecs::world::World;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub(crate) fn ensure_hierarchy_registered(world: &mut World) {
    let child_of_id = world.register_component::<ChildOf>();
    let child_of_immutable = world
        .components()
        .get_info(child_of_id)
        .is_some_and(|info| !info.mutable());
    let _ = with_world_registry(world, |reg| {
        reg.register_hierarchy(child_of_id, child_of_immutable)
    });
}

pub(crate) fn ensure_world_mod_resources(world: &mut World) {
    if !world.contains_resource::<WorldDropGuard>() {
        world.insert_resource(WorldDropGuard::new());
    }
    if !world.contains_resource::<ModRegistry>() {
        world.insert_resource(ModRegistry::new());
    }
    if !world.contains_resource::<ModHost>() {
        world.insert_resource(ModHost::new());
    }
    if !world.contains_resource::<ModQuarantine>() {
        world.insert_resource(ModQuarantine::new());
    }
    if !world.contains_resource::<ModScheduleMap>() {
        world.insert_resource(ModScheduleMap::new());
    }
    if !world.contains_resource::<ModVisibleSets>() {
        world.insert_resource(ModVisibleSets::new());
    }
    if !world.contains_resource::<ModDependencyGraph>() {
        world.insert_resource(ModDependencyGraph::new());
    }
    #[cfg(feature = "signature-verification")]
    if !world.contains_resource::<ModTrustPolicy>() {
        world.insert_resource(ModTrustPolicy::disabled());
    }
    #[cfg(feature = "diagnostics")]
    ensure_diagnostics_resources(world);
    if !world.contains_resource::<StartupCompleted>() {
        world.insert_resource(StartupCompleted(false));
    }
    if !world.contains_resource::<Messages<ModLoaded>>() {
        world.init_resource::<Messages<ModLoaded>>();
    }
    if !world.contains_resource::<Messages<ModUnloaded>>() {
        world.init_resource::<Messages<ModUnloaded>>();
    }
    if !world.contains_resource::<Messages<ModSystemFaulted>>() {
        world.init_resource::<Messages<ModSystemFaulted>>();
    }
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
    let mut query = world.query::<(Entity, &LoadedModInfo)>();
    let matching_entities: Vec<Entity> = query
        .iter(world)
        .filter_map(|(entity, info)| (info.id == id).then_some(entity))
        .collect();
    for entity in matching_entities {
        if let Ok(entity_mut) = world.get_entity_mut(entity) {
            entity_mut.despawn();
        }
    }
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

fn load_mod_with_optional_state(
    world: &mut World,
    path: &Path,
    options: LoadOptions,
    saved_state: Option<&[u8]>,
) -> Result<ModId, ModLoadError> {
    ensure_world_mod_resources(world);
    let path_buf = path.to_path_buf();

    let (mod_id, systems) = load_mod_in_world(world, &path_buf, &options)?;

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
                drop(systems);
                rollback_unscheduled_mod_in_world(world, mod_id);
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
        drop(systems);
        rollback_unscheduled_mod_in_world(world, mod_id);
        return Err(err);
    }

    if let Some(blob) = saved_state
        && let Err(err) = restore_mod_state_in_world(world, mod_id, blob)
    {
        drop(systems);
        rollback_unscheduled_mod_in_world(world, mod_id);
        return Err(err);
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

fn unload_single_mod_in_world(world: &mut World, id: ModId) -> Result<(), ModLoadError> {
    let (mod_name, unloaded_path, removed_systems) =
        excise_and_unload_mod_in_world(world, id, |w| {
            despawn_loaded_mod_entities(w, id);
        })?;
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

fn reload_mod_cascade(
    world: &mut World,
    id: ModId,
    override_path: Option<&Path>,
    options: LoadOptions,
) -> Result<ModId, ModLoadError> {
    ensure_world_mod_resources(world);
    let active_ids: BTreeSet<ModId> = {
        let Some(host) = world.get_resource::<ModHost>() else {
            return Err(ModLoadError::ModNotFound { id });
        };
        if !host.contains_mod(id) {
            return Err(ModLoadError::ModNotFound { id });
        }
        host.loaded_mods().iter().map(LoadedMod::id).collect()
    };

    let forward_ids = world
        .get_resource::<ModDependencyGraph>()
        .map(|dag| dag.cascading_order(id, &active_ids))
        .unwrap_or_else(|| vec![id]);

    let mut plan = Vec::with_capacity(forward_ids.len());
    for tid in forward_ids {
        let (loaded_name, current_path) = {
            let host = world.resource::<ModHost>();
            let Some(loaded) = host.get_mod(tid) else {
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

    // Unload in reverse topological order (`C -> B -> A`).
    for idx in (0..plan.len()).rev() {
        unload_single_mod_in_world(world, plan[idx].old_id)?;
    }

    // Reload in forward topological order (`A -> B -> C`), restoring state on each mod.
    let mut new_root_id: Option<ModId> = None;
    for (idx, item) in plan.into_iter().enumerate() {
        let new_id = load_mod_with_optional_state(
            world,
            &item.target_path,
            item.options.clone(),
            saved_states[idx].as_deref(),
        )?;
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

    Ok(new_root_id.expect("target ModId must be present in cascade reload plan"))
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
        if let Some(info) = self.components().get_info(comp_id) {
            if info.storage_type() == StorageType::SparseSet {
                inferred_flags |= BAM_STORAGE_FLAG_SPARSE_SET;
            }
            if !info.mutable() {
                inferred_flags |= BAM_STORAGE_FLAG_IMMUTABLE;
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
        if !self.contains_resource::<Messages<M>>() {
            self.init_resource::<Messages<M>>();
        }
        let messages_res_id = self.register_component::<Messages<M>>();
        with_world_registry(self, |reg| reg.register_message::<M>(messages_res_id))
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
        if !self.contains_resource::<Schedules>() {
            self.insert_resource(Schedules::new());
        }
        if let Some(mut schedules) = self.get_resource_mut::<Schedules>() {
            let _ = schedules.entry(interned);
        }
        if !self.contains_resource::<ModScheduleMap>() {
            self.insert_resource(ModScheduleMap::new());
        }
        if let Some(mut map) = self.get_resource_mut::<ModScheduleMap>() {
            let ok = map.register(id, interned);
            debug_assert!(ok);
        }
        Ok(())
    }

    fn register_mod_visible_set<S: SystemSet>(&mut self, name: &str, set: S) -> bool {
        if !self.contains_resource::<ModVisibleSets>() {
            self.insert_resource(ModVisibleSets::new());
        }
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
        load_mod_with_optional_state(self, path.as_ref(), options, None)
    }

    fn unload_mod(&mut self, id: ModId) -> Result<(), ModLoadError> {
        ensure_world_mod_resources(self);
        let active_ids: BTreeSet<ModId> = {
            let Some(host) = self.get_resource::<ModHost>() else {
                return Err(ModLoadError::ModNotFound { id });
            };
            if !host.contains_mod(id) {
                return Err(ModLoadError::ModNotFound { id });
            }
            host.loaded_mods().iter().map(LoadedMod::id).collect()
        };

        let forward_ids = self
            .get_resource::<ModDependencyGraph>()
            .map(|dag| dag.cascading_order(id, &active_ids))
            .unwrap_or_else(|| vec![id]);

        for target_id in forward_ids.into_iter().rev() {
            unload_single_mod_in_world(self, target_id)?;
        }
        Ok(())
    }

    fn unload_all_mods(&mut self) -> &mut Self {
        ensure_world_mod_resources(self);
        unload_all_mods_in_world(self, |world, id, name, path, removed_systems| {
            despawn_loaded_mod_entities(world, id);
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
                    errors.push(err);
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

            match load_mod_with_optional_state(self, &lib_path, options.clone(), None) {
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
