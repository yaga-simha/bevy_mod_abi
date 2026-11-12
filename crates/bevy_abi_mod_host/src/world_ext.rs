//! [`ModWorldExt`] exclusive `&mut World` API for registering components/resources/schedules
//! and loading or unloading native mods (`B-01`, `B-05`, `B-06`, `B-08`, `B-09`, `L-03`, `L-04`).

use crate::events::{LoadedModInfo, ModLoaded, ModSystemFaulted, ModUnloaded};
use crate::schedule_sets::{
    ModScheduleMap, StartupCompleted, prevalidate_mod_schedules_and_ordering,
    run_late_startup_systems_in_world, schedule_mod_systems_in_world,
};
use bevy_abi_mod_host_sys::{
    ComponentCollisionError, ComponentOptions, LoadOptions, ModErrorPolicy, ModErrorPolicyConfig,
    ModHost, ModId, ModLoadError, ModQuarantine, ModRegistry, WorldDropGuard,
    excise_and_unload_mod_in_world, load_mod_in_world, rollback_unscheduled_mod_in_world,
    unload_all_mods_in_world,
};
use bevy_abi_mod_sys::{
    BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_SPARSE_SET, BamComponent, ModComponent, ScheduleId,
};
use bevy_ecs::component::{Component, StorageType};
use bevy_ecs::entity::Entity;
use bevy_ecs::message::Messages;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::{ScheduleLabel, Schedules};
use bevy_ecs::world::World;
use std::path::{Path, PathBuf};

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

/// Exclusive `&mut World` extension trait for exposing Bevy ECS types and managing native C-ABI
/// mods at startup or inside exclusive Bevy systems (`B-01`, `L-04`).
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

    /// Registers a non-component [`ModComponent`] helper/nested struct type so its static VTable
    /// and field accessors are resolvable via `get_proc_address`.
    fn register_mod_type<T: ModComponent>(&mut self) -> &mut Self;

    /// Maps a game-defined custom [`ScheduleId`] (`id.0 >= ScheduleId::CUSTOM_BASE` = `1024`) to a
    /// Bevy [`ScheduleLabel`] in [`ModScheduleMap`] (`B-06`).
    fn register_mod_schedule<S: ScheduleLabel>(
        &mut self,
        id: ScheduleId,
        label: S,
    ) -> Result<(), ModLoadError>;

    /// Sets the default [`ModErrorPolicy`] applied to newly initialized guest systems in this [`World`].
    fn set_mod_error_policy(&mut self, policy: ModErrorPolicy) -> &mut Self;

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

    fn set_mod_error_policy(&mut self, policy: ModErrorPolicy) -> &mut Self {
        self.insert_resource(ModErrorPolicyConfig(policy));
        self
    }

    fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError> {
        self.load_mod_with(path, LoadOptions::default())
    }

    fn load_mod_with<P: AsRef<Path>>(
        &mut self,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        ensure_world_mod_resources(self);
        let path_buf = path.as_ref().to_path_buf();

        let (mod_id, systems) = load_mod_in_world(self, &path_buf, &options)?;

        let (mod_name, mod_version) = self
            .get_resource::<ModHost>()
            .and_then(|h| h.get_mod(mod_id))
            .map(|m| (m.name().to_string(), m.version().to_string()))
            .unwrap_or_else(|| ("unnamed_mod".to_string(), "0.0.0".to_string()));

        if options.strict
            && let Some(registry) = self.get_resource::<ModRegistry>()
        {
            for sys in &systems {
                if let Err(missing) = sys.validate_against_registry(registry) {
                    let sys_name = sys.system_name().to_string();
                    drop(systems);
                    rollback_unscheduled_mod_in_world(self, mod_id);
                    return Err(ModLoadError::MissingComponents {
                        path: path_buf,
                        system_name: sys_name,
                        missing,
                    });
                }
            }
        }

        if let Err(err) = prevalidate_mod_schedules_and_ordering(
            self,
            &path_buf,
            &mod_name,
            options.strict,
            &systems,
        ) {
            drop(systems);
            rollback_unscheduled_mod_in_world(self, mod_id);
            return Err(err);
        }

        let system_count = systems.len();
        let late_startup = schedule_mod_systems_in_world(self, mod_id, &mod_name, systems);

        finalize_mod_load_in_world(
            self,
            mod_id,
            mod_name.clone(),
            mod_version,
            path_buf,
            system_count,
        );
        run_late_startup_systems_in_world(self, &mod_name, late_startup);

        Ok(mod_id)
    }

    fn unload_mod(&mut self, id: ModId) -> Result<(), ModLoadError> {
        ensure_world_mod_resources(self);
        let (mod_name, unloaded_path, removed_systems) =
            excise_and_unload_mod_in_world(self, id, |world| {
                despawn_loaded_mod_entities(world, id);
            })?;

        emit_mod_unloaded_once(self, id, &mod_name, unloaded_path, removed_systems);
        Ok(())
    }

    fn unload_all_mods(&mut self) -> &mut Self {
        ensure_world_mod_resources(self);
        unload_all_mods_in_world(self, |world, id, name, path, removed_systems| {
            despawn_loaded_mod_entities(world, id);
            emit_mod_unloaded_once(world, id, &name, path, removed_systems);
        });
        self
    }
}
