//! [`ModAppExt`] extension methods on Bevy [`App`] for registering types and managing mods.

use crate::events::{LoadedModInfo, ModLoaded, ModUnloaded};
use crate::plugin::ensure_startup_tracker;
use crate::schedule_sets::{
    run_late_startup_systems_once, schedule_mod_systems, schedule_mod_systems_into_label,
};
use bevy_abi_mod_host_sys::{
    ComponentCollisionError, ModErrorPolicy, ModErrorPolicyConfig, ModId, ModLoadError,
    ModRegistry, canonical_mod_name_from_path, excise_and_unload_mod_in_world, load_mod_in_world,
    rollback_unscheduled_mod_in_world, unload_all_mods_in_world,
};
use bevy_abi_mod_sys::{ExposableComponent, FfiResult};
use bevy_app::App;
use bevy_ecs::component::{Component, ComponentId};
use bevy_ecs::entity::Entity;
use bevy_ecs::message::Messages;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::ScheduleLabel;
use bevy_ecs::world::World;
use std::path::{Path, PathBuf};

fn with_mod_registry<R>(app: &mut App, f: impl FnOnce(&mut ModRegistry) -> R) -> R {
    let world = app.world_mut();
    if let Some(mut registry) = world.get_resource_mut::<ModRegistry>() {
        f(&mut registry)
    } else {
        let mut registry = ModRegistry::new();
        let out = f(&mut registry);
        world.insert_resource(registry);
        out
    }
}

fn register_ecs_entry<T: Component, R>(
    app: &mut App,
    f: impl FnOnce(&mut ModRegistry, ComponentId) -> R,
) -> R {
    let comp_id = app.world_mut().register_component::<T>();
    with_mod_registry(app, |reg| f(reg, comp_id))
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

fn emit_mod_unloaded(world: &mut World, id: ModId, path: PathBuf, removed_systems: usize) {
    let unloaded_event = ModUnloaded {
        id,
        path: Some(path),
        removed_systems,
    };
    if let Some(mut msgs) = world.get_resource_mut::<Messages<ModUnloaded>>() {
        msgs.write(unloaded_event.clone());
    }
    world.trigger(unloaded_event);
}

fn finalize_mod_load_in_world(
    world: &mut World,
    mod_id: ModId,
    path: PathBuf,
    system_count: usize,
) -> Entity {
    let short_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.to_string_lossy().into_owned());

    let entity = world
        .spawn((
            Name::new(format!("mod:{short_name}")),
            LoadedModInfo {
                id: mod_id,
                name: short_name,
                path: path.clone(),
                system_count,
            },
        ))
        .id();

    let loaded_event = ModLoaded {
        id: mod_id,
        entity,
        path,
        system_count,
    };
    if let Some(mut msgs) = world.get_resource_mut::<Messages<ModLoaded>>() {
        msgs.write(loaded_event.clone());
    }
    world.trigger(loaded_event);
    entity
}

/// Extension methods on Bevy [`App`] for registering mod-exposable components and loading native mods.
///
/// Loading and unloading shared libraries require exclusive [`World`] / [`App`] access so guest code
/// cannot be unmapped (`dlclose`) while parallel systems are executing:
///
/// ```compile_fail
/// use bevy_abi_mod_host::{ModHost, ModId};
/// use bevy_ecs::system::ResMut;
///
/// fn bad_parallel_unload_system(mut host: ResMut<ModHost>) {
///     // `ModHost` does not expose `unload_mod` or `try_unload_mod` to `ResMut<ModHost>` systems.
///     let _ = host.try_unload_mod(ModId(1));
/// }
/// ```
pub trait ModAppExt {
    /// Register an exposable component type with Bevy ECS and the native mod registry.
    fn register_mod_component<T: Component + ExposableComponent>(&mut self) -> &mut Self;

    /// Fallible variant of [`ModAppExt::register_mod_component`] returning [`ComponentCollisionError`].
    fn try_register_mod_component<T: Component + ExposableComponent>(
        &mut self,
    ) -> Result<&mut Self, ComponentCollisionError>;

    /// Register an exposable component type along with its host-side [`Default`] constructor
    /// so guest mods may invoke `cmd_insert_default` (including on opaque non-POD `String`/`Vec` types).
    fn register_mod_component_with_default<T: Component + ExposableComponent + Default>(
        &mut self,
    ) -> &mut Self;

    /// Register an exposable component type as VTable-only (disallowing raw byte insertion via
    /// `cmd_insert_component`, e.g. for types containing `bool` or domain invariants).
    fn register_mod_component_opaque<T: Component + ExposableComponent>(&mut self) -> &mut Self;

    /// Register an exposable VTable-only component type along with its host-side [`Default`] constructor.
    fn register_mod_component_opaque_with_default<T: Component + ExposableComponent + Default>(
        &mut self,
    ) -> &mut Self;

    /// Register a singleton Bevy [`Resource`] type with the native mod registry so guest mods can
    /// declare `RESOURCE_READ` / `RESOURCE_WRITE` and access it via `res_readonly` / `res_mutable`.
    fn register_mod_resource<R: Resource + ExposableComponent>(&mut self) -> &mut Self;

    /// Fallible variant of [`ModAppExt::register_mod_resource`] returning [`ComponentCollisionError`].
    fn try_register_mod_resource<R: Resource + ExposableComponent>(
        &mut self,
    ) -> Result<&mut Self, ComponentCollisionError>;

    /// Register a nested/compound `ModExposable` helper type (without requiring `Component`)
    /// so its VTable and field accessors are exposed via `GetProcAddressFn`.
    fn register_mod_type<T: ExposableComponent>(&mut self) -> &mut Self;

    /// Sets the default [`ModErrorPolicy`] for guest systems in this [`App`].
    fn set_mod_error_policy(&mut self, policy: ModErrorPolicy) -> &mut Self;

    /// Load a native shared library mod into the Bevy App's schedules (`Update` by default,
    /// or per-system schedule if registered via `register_system_in_schedule`).
    fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<&mut Self, FfiResult>;

    /// Load a native shared library mod, returning its tracked [`ModId`].
    fn load_mod_tracked<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, FfiResult>;

    /// Typed variant of [`ModAppExt::load_mod_tracked`] returning [`ModLoadError`].
    fn try_load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError>;

    /// Loads a native shared library mod and fails fast with [`ModLoadError::MissingComponents`]
    /// (unloading the library cleanly) if any guest system queries an unregistered component.
    fn try_load_mod_strict<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError>;

    /// Loads a native shared library mod and places all of its registered systems into `schedule`.
    fn load_mod_into_schedule<S: ScheduleLabel + Clone, P: AsRef<Path>>(
        &mut self,
        schedule: S,
        path: P,
    ) -> Result<ModId, FfiResult>;

    /// Typed variant of [`ModAppExt::load_mod_into_schedule`] returning [`ModLoadError`].
    fn try_load_mod_into_schedule<S: ScheduleLabel + Clone, P: AsRef<Path>>(
        &mut self,
        schedule: S,
        path: P,
    ) -> Result<ModId, ModLoadError>;

    /// Unload a loaded mod by its [`ModId`]: deactivates systems via atomic tombstone flags,
    /// removes all systems and scoped [`ModNamedSystemSet`](crate::ModNamedSystemSet)s from Bevy
    /// schedules, evicts guest VTables/symbols, despawns its [`LoadedModInfo`] entity, invokes
    /// the guest teardown hook, and unmaps the library.
    fn unload_mod(&mut self, id: ModId) -> Result<(), FfiResult>;

    /// Typed variant of [`ModAppExt::unload_mod`] returning [`ModLoadError`].
    fn try_unload_mod(&mut self, id: ModId) -> Result<(), ModLoadError>;

    /// Unloads all currently loaded mods in reverse load order under exclusive [`World`] access.
    fn unload_all_mods(&mut self) -> &mut Self;
}

impl ModAppExt for App {
    fn register_mod_component<T: Component + ExposableComponent>(&mut self) -> &mut Self {
        register_ecs_entry::<T, ()>(self, ModRegistry::register_component::<T>);
        self
    }

    fn try_register_mod_component<T: Component + ExposableComponent>(
        &mut self,
    ) -> Result<&mut Self, ComponentCollisionError> {
        register_ecs_entry::<T, _>(self, ModRegistry::try_register_component::<T>)?;
        Ok(self)
    }

    fn register_mod_component_with_default<T: Component + ExposableComponent + Default>(
        &mut self,
    ) -> &mut Self {
        register_ecs_entry::<T, ()>(self, ModRegistry::register_component_with_default::<T>);
        self
    }

    fn register_mod_component_opaque<T: Component + ExposableComponent>(&mut self) -> &mut Self {
        register_ecs_entry::<T, ()>(self, ModRegistry::register_component_opaque::<T>);
        self
    }

    fn register_mod_component_opaque_with_default<T: Component + ExposableComponent + Default>(
        &mut self,
    ) -> &mut Self {
        register_ecs_entry::<T, ()>(
            self,
            ModRegistry::register_component_opaque_with_default::<T>,
        );
        self
    }

    fn register_mod_resource<R: Resource + ExposableComponent>(&mut self) -> &mut Self {
        register_ecs_entry::<R, ()>(self, ModRegistry::register_resource::<R>);
        self
    }

    fn try_register_mod_resource<R: Resource + ExposableComponent>(
        &mut self,
    ) -> Result<&mut Self, ComponentCollisionError> {
        register_ecs_entry::<R, _>(self, ModRegistry::try_register_resource::<R>)?;
        Ok(self)
    }

    fn register_mod_type<T: ExposableComponent>(&mut self) -> &mut Self {
        with_mod_registry(self, ModRegistry::register_type::<T>);
        self
    }

    fn set_mod_error_policy(&mut self, policy: ModErrorPolicy) -> &mut Self {
        self.insert_resource(ModErrorPolicyConfig(policy));
        self
    }

    fn try_load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        let path_buf = path.as_ref().to_path_buf();
        let mod_name = canonical_mod_name_from_path(&path_buf);
        let (mod_id, systems) = load_mod_in_world(self.world_mut(), &path_buf)?;

        let system_count = systems.len();
        let late_startup = schedule_mod_systems(self, mod_id, &mod_name, systems);

        finalize_mod_load_in_world(self.world_mut(), mod_id, path_buf, system_count);
        run_late_startup_systems_once(self, &mod_name, late_startup);
        Ok(mod_id)
    }

    fn try_load_mod_strict<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        let path_buf = path.as_ref().to_path_buf();
        let mod_name = canonical_mod_name_from_path(&path_buf);
        let (mod_id, systems) = load_mod_in_world(self.world_mut(), &path_buf)?;

        if let Some(registry) = self.world().get_resource::<ModRegistry>() {
            for sys in &systems {
                if let Err(missing) = sys.validate_against_registry(registry) {
                    let sys_name = sys.system_name().to_string();
                    drop(systems);
                    rollback_unscheduled_mod_in_world(self.world_mut(), mod_id);
                    return Err(ModLoadError::MissingComponents {
                        path: path_buf,
                        system_name: sys_name,
                        missing,
                    });
                }
            }
        }

        let system_count = systems.len();
        let late_startup = schedule_mod_systems(self, mod_id, &mod_name, systems);

        finalize_mod_load_in_world(self.world_mut(), mod_id, path_buf, system_count);
        run_late_startup_systems_once(self, &mod_name, late_startup);
        Ok(mod_id)
    }

    fn try_load_mod_into_schedule<S: ScheduleLabel + Clone, P: AsRef<Path>>(
        &mut self,
        schedule: S,
        path: P,
    ) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        let path_buf = path.as_ref().to_path_buf();
        let mod_name = canonical_mod_name_from_path(&path_buf);
        let (mod_id, systems) = load_mod_in_world(self.world_mut(), &path_buf)?;

        let system_count = systems.len();
        schedule_mod_systems_into_label(self, schedule, mod_id, &mod_name, systems);

        finalize_mod_load_in_world(self.world_mut(), mod_id, path_buf, system_count);
        Ok(mod_id)
    }

    fn load_mod_into_schedule<S: ScheduleLabel + Clone, P: AsRef<Path>>(
        &mut self,
        schedule: S,
        path: P,
    ) -> Result<ModId, FfiResult> {
        self.try_load_mod_into_schedule(schedule, path)
            .map_err(ModLoadError::into)
    }

    fn load_mod_tracked<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, FfiResult> {
        self.try_load_mod(path).map_err(ModLoadError::into)
    }

    fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<&mut Self, FfiResult> {
        self.load_mod_tracked(path)?;
        Ok(self)
    }

    fn try_unload_mod(&mut self, id: ModId) -> Result<(), ModLoadError> {
        let (unloaded_path, removed_systems) =
            excise_and_unload_mod_in_world(self.world_mut(), id, |world| {
                despawn_loaded_mod_entities(world, id);
            })?;

        emit_mod_unloaded(self.world_mut(), id, unloaded_path, removed_systems);
        Ok(())
    }

    fn unload_mod(&mut self, id: ModId) -> Result<(), FfiResult> {
        self.try_unload_mod(id).map_err(ModLoadError::into)
    }

    fn unload_all_mods(&mut self) -> &mut Self {
        unload_all_mods_in_world(self.world_mut(), |world, id, path, removed_systems| {
            despawn_loaded_mod_entities(world, id);
            emit_mod_unloaded(world, id, path, removed_systems);
        });
        self
    }
}
