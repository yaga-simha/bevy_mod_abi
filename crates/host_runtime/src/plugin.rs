#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # Bevy Plugin & Application Integration
//!
//! This module provides [`AbiModPlugin`], [`ConfiguredAbiModPlugin`], ECS mod metadata
//! ([`LoadedModInfo`]), lifecycle events/messages ([`ModLoaded`], [`ModUnloaded`]), and the
//! [`ModAppExt`] extension trait to integrate dynamic C-ABI native mods idiomatically into
//! Bevy's application lifecycle, schedules, and observer/message systems.
//!
//! ## Architecture
//! - **Resource & Message Initialization**: Inserts [`ModRegistry`], [`ModHost`], and
//!   [`ModErrorPolicyConfig`], and registers [`ModLoaded`], [`ModUnloaded`], and
//!   [`ModSystemFaulted`] messages.
//! - **ECS Entity Representation**: Every loaded mod spawns a Bevy entity carrying [`Name`]
//!   and [`LoadedModInfo`], which is automatically despawned when the mod unloads.
//! - **Schedule & `SystemSet` Integration**: Routes dynamic systems to [`ModSchedule`]
//!   (`Update`, `Startup`, `PreUpdate`, `PostUpdate`, `FixedUpdate`, `Last`) or any custom
//!   [`ScheduleLabel`], tagging them with [`ModSystemSet::All`] and [`ModSystemSet::Mod`].
//! - **Leak-Free Hot-Reload Unloading**: [`ModAppExt::unload_mod`] removes all systems in
//!   [`ModSystemSet::Mod`] from Bevy's [`Schedules`] via [`ScheduleCleanupPolicy::RemoveSetAndSystems`]
//!   before invoking `bevy_abi_mod_shutdown` and unmapping the shared library.

use crate::dynamic_system::{
    DynamicCSystem, ModErrorPolicy, ModErrorPolicyConfig, ModSchedule, ModSystemFaulted,
    ModSystemSet,
};
use crate::host::{ModHost, ModId, ModLoadError};
use crate::registry::ModRegistry;
use abi_contract::{ExposableComponent, FfiResult};
use bevy_app::{App, FixedUpdate, Last, Plugin, PostUpdate, PreUpdate, Startup, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::event::Event;
use bevy_ecs::message::{Message, Messages};
use bevy_ecs::name::Name;
use bevy_ecs::schedule::{InternedScheduleLabel, ScheduleCleanupPolicy, ScheduleLabel, Schedules};
use std::path::{Path, PathBuf};

/// ECS [`Component`] attached to the entity spawned for each active native mod.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LoadedModInfo {
    /// Assigned [`ModId`].
    pub id: ModId,
    /// Human-readable mod name (file stem or path display).
    pub name: String,
    /// Filesystem path of the shared library.
    pub path: PathBuf,
    /// Number of dynamic systems registered by this mod.
    pub system_count: usize,
}

/// Bevy [`Event`] (for [`Observer`](bevy_ecs::observer::Observer)s) and buffered [`Message`]
/// emitted whenever a native mod is loaded into a Bevy [`App`].
#[derive(Event, Message, Debug, Clone, PartialEq, Eq)]
pub struct ModLoaded {
    /// Assigned [`ModId`].
    pub id: ModId,
    /// ECS entity representing this loaded mod (`LoadedModInfo` + `Name`).
    pub entity: Entity,
    /// Filesystem path of the loaded library.
    pub path: PathBuf,
    /// Number of dynamic systems registered into Bevy schedules.
    pub system_count: usize,
}

/// Bevy [`Event`] (for [`Observer`](bevy_ecs::observer::Observer)s) and buffered [`Message`]
/// emitted whenever a native mod is unloaded from a Bevy [`App`].
#[derive(Event, Message, Debug, Clone, PartialEq, Eq)]
pub struct ModUnloaded {
    /// Unloaded [`ModId`].
    pub id: ModId,
    /// Filesystem path of the unloaded library, if known.
    pub path: Option<PathBuf>,
    /// Number of dynamic systems excised from Bevy schedules.
    pub removed_systems: usize,
}

/// Bevy Plugin initializing the native ABI modding framework with default settings.
///
/// Kept as a zero-sized unit struct so both `app.add_plugins(AbiModPlugin)` and
/// `app.add_plugins(AbiModPlugin::default())` work idiomatically, while
/// [`AbiModPlugin::with_error_policy`] configures custom error handling.
#[derive(Default, Debug, Clone, Copy)]
pub struct AbiModPlugin;

impl AbiModPlugin {
    /// Returns a configured plugin using the specified [`ModErrorPolicy`].
    pub const fn with_error_policy(policy: ModErrorPolicy) -> ConfiguredAbiModPlugin {
        ConfiguredAbiModPlugin {
            error_policy: policy,
        }
    }
}

impl Plugin for AbiModPlugin {
    fn build(&self, app: &mut App) {
        ConfiguredAbiModPlugin::default().build(app);
    }
}

/// Configurable variant of [`AbiModPlugin`] created via [`AbiModPlugin::with_error_policy`].
#[derive(Default, Debug, Clone, Copy)]
pub struct ConfiguredAbiModPlugin {
    /// Default [`ModErrorPolicy`] inserted into [`ModErrorPolicyConfig`].
    pub error_policy: ModErrorPolicy,
}

impl Plugin for ConfiguredAbiModPlugin {
    fn build(&self, app: &mut App) {
        if !app.world().contains_resource::<ModRegistry>() {
            app.insert_resource(ModRegistry::new());
        }
        if !app.world().contains_resource::<ModHost>() {
            app.insert_resource(ModHost::new());
        }
        app.insert_resource(ModErrorPolicyConfig(self.error_policy));

        if !app.world().contains_resource::<Messages<ModLoaded>>() {
            app.add_message::<ModLoaded>();
        }
        if !app.world().contains_resource::<Messages<ModUnloaded>>() {
            app.add_message::<ModUnloaded>();
        }
        if !app
            .world()
            .contains_resource::<Messages<ModSystemFaulted>>()
        {
            app.add_message::<ModSystemFaulted>();
        }
    }
}

fn ensure_mod_infrastructure(app: &mut App) {
    if !app.world().contains_resource::<ModHost>() {
        app.insert_resource(ModHost::new());
    }
    if !app.world().contains_resource::<ModRegistry>() {
        app.insert_resource(ModRegistry::new());
    }
}

fn add_system_for_mod_schedule(app: &mut App, schedule: ModSchedule, sys: DynamicCSystem) {
    match schedule {
        ModSchedule::Update => {
            app.add_systems(Update, sys);
        }
        ModSchedule::Startup => {
            app.add_systems(Startup, sys);
        }
        ModSchedule::PreUpdate => {
            app.add_systems(PreUpdate, sys);
        }
        ModSchedule::PostUpdate => {
            app.add_systems(PostUpdate, sys);
        }
        ModSchedule::FixedUpdate => {
            app.add_systems(FixedUpdate, sys);
        }
        ModSchedule::Last => {
            app.add_systems(Last, sys);
        }
    }
}

fn finalize_mod_load_in_world(
    world: &mut bevy_ecs::world::World,
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
pub trait ModAppExt {
    /// Register an exposable component type with Bevy ECS and the native mod registry.
    fn register_mod_component<T: Component + ExposableComponent>(&mut self) -> &mut Self;

    /// Register an exposable component type as VTable-only (disallowing raw byte insertion via
    /// `cmd_insert_component`, e.g. for types containing `bool` or domain invariants).
    fn register_mod_component_opaque<T: Component + ExposableComponent>(&mut self) -> &mut Self;

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
    /// removes all systems in [`ModSystemSet::Mod`] from Bevy schedules, despawns its
    /// [`LoadedModInfo`] entity, invokes the guest teardown hook, and unmaps the library.
    fn unload_mod(&mut self, id: ModId) -> Result<(), FfiResult>;

    /// Typed variant of [`ModAppExt::unload_mod`] returning [`ModLoadError`].
    fn try_unload_mod(&mut self, id: ModId) -> Result<(), ModLoadError>;
}

impl ModAppExt for App {
    fn register_mod_component<T: Component + ExposableComponent>(&mut self) -> &mut Self {
        let comp_id = self.world_mut().register_component::<T>();
        if let Some(mut registry) = self.world_mut().get_resource_mut::<ModRegistry>() {
            registry.register_component::<T>(comp_id);
        } else {
            let mut registry = ModRegistry::new();
            registry.register_component::<T>(comp_id);
            self.insert_resource(registry);
        }
        self
    }

    fn register_mod_component_opaque<T: Component + ExposableComponent>(&mut self) -> &mut Self {
        let comp_id = self.world_mut().register_component::<T>();
        if let Some(mut registry) = self.world_mut().get_resource_mut::<ModRegistry>() {
            registry.register_component_opaque::<T>(comp_id);
        } else {
            let mut registry = ModRegistry::new();
            registry.register_component_opaque::<T>(comp_id);
            self.insert_resource(registry);
        }
        self
    }

    fn register_mod_type<T: ExposableComponent>(&mut self) -> &mut Self {
        if let Some(mut registry) = self.world_mut().get_resource_mut::<ModRegistry>() {
            registry.register_type::<T>();
        } else {
            let mut registry = ModRegistry::new();
            registry.register_type::<T>();
            self.insert_resource(registry);
        }
        self
    }

    fn set_mod_error_policy(&mut self, policy: ModErrorPolicy) -> &mut Self {
        self.insert_resource(ModErrorPolicyConfig(policy));
        self
    }

    fn try_load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError> {
        ensure_mod_infrastructure(self);
        let path_buf = path.as_ref().to_path_buf();

        let (mod_id, systems) =
            self.world_mut()
                .resource_scope::<ModHost, _>(|world, mut mod_host| {
                    // SAFETY: Loading dynamic library specified by path with mutable World access.
                    unsafe { mod_host.try_load_mod_tracked_with_world(&path_buf, world) }
                })?;

        let system_count = systems.len();
        for sys in systems {
            let sched = sys.schedule();
            add_system_for_mod_schedule(self, sched, sys);
        }

        finalize_mod_load_in_world(self.world_mut(), mod_id, path_buf, system_count);
        Ok(mod_id)
    }

    fn try_load_mod_strict<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError> {
        ensure_mod_infrastructure(self);
        let path_buf = path.as_ref().to_path_buf();

        let (mod_id, systems) =
            self.world_mut()
                .resource_scope::<ModHost, _>(|world, mut mod_host| {
                    // SAFETY: Loading dynamic library specified by path with mutable World access.
                    unsafe { mod_host.try_load_mod_tracked_with_world(&path_buf, world) }
                })?;

        if let Some(registry) = self.world().get_resource::<ModRegistry>() {
            for sys in &systems {
                if let Err(missing) = sys.validate_against_registry(registry) {
                    let sys_name = sys.system_name().to_string();
                    drop(systems);
                    if let Some(mut mod_host) = self.world_mut().get_resource_mut::<ModHost>() {
                        // SAFETY: Immediately unloading failed mod before any systems were scheduled.
                        let _ = unsafe { mod_host.try_unload_mod(mod_id) };
                    }
                    return Err(ModLoadError::MissingComponents {
                        path: path_buf,
                        system_name: sys_name,
                        missing,
                    });
                }
            }
        }

        let system_count = systems.len();
        for sys in systems {
            let sched = sys.schedule();
            add_system_for_mod_schedule(self, sched, sys);
        }

        finalize_mod_load_in_world(self.world_mut(), mod_id, path_buf, system_count);
        Ok(mod_id)
    }

    fn try_load_mod_into_schedule<S: ScheduleLabel + Clone, P: AsRef<Path>>(
        &mut self,
        schedule: S,
        path: P,
    ) -> Result<ModId, ModLoadError> {
        ensure_mod_infrastructure(self);
        let path_buf = path.as_ref().to_path_buf();

        let (mod_id, systems) =
            self.world_mut()
                .resource_scope::<ModHost, _>(|world, mut mod_host| {
                    // SAFETY: Loading dynamic library specified by path with mutable World access.
                    unsafe { mod_host.try_load_mod_tracked_with_world(&path_buf, world) }
                })?;

        let system_count = systems.len();
        for sys in systems {
            self.add_systems(schedule.clone(), sys);
        }

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
        // 1. Verify mod exists and flip atomic tombstone to false before touching schedules.
        {
            let Some(mod_host) = self.world().get_resource::<ModHost>() else {
                return Err(ModLoadError::ModNotFound { id });
            };
            if !mod_host.contains_mod(id) {
                return Err(ModLoadError::ModNotFound { id });
            }
            mod_host.deactivate_mod(id);
        }

        // 2. Excise all systems in `ModSystemSet::Mod(id)` across Bevy schedules while library is still mapped.
        // Extract matching schedules individually via `Schedules::remove` + `reinsert` (mirroring Bevy's
        // `World::schedule_scope`) so `Schedules` remains present in `World` during `Schedule::initialize`.
        let mut removed_systems = 0usize;
        if let Some(schedules) = self.world().get_resource::<Schedules>() {
            let target_labels: Vec<InternedScheduleLabel> = schedules
                .iter()
                .filter(|(_, schedule)| {
                    schedule
                        .graph()
                        .system_sets
                        .contains(ModSystemSet::Mod(id))
                })
                .map(|(_, schedule)| schedule.label())
                .collect();

            for label in target_labels {
                let removed_sched = self
                    .world_mut()
                    .get_resource_mut::<Schedules>()
                    .and_then(|mut s| s.remove_temporarily(label));
                if let Some(mut schedule) = removed_sched {
                    if let Ok(n) = schedule.remove_systems_in_set(
                        ModSystemSet::Mod(id),
                        self.world_mut(),
                        ScheduleCleanupPolicy::RemoveSetAndSystems,
                    ) {
                        removed_systems += n;
                    }
                    if let Some(mut schedules) = self.world_mut().get_resource_mut::<Schedules>() {
                        schedules.reinsert(schedule);
                    }
                }
            }
        }

        // 3. Despawn any ECS entities representing this loaded mod (`LoadedModInfo`).
        {
            let world = self.world_mut();
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

        // 4. Invoke guest shutdown hook and drop shared library handle.
        let unloaded_path = {
            let mut mod_host =
                self.world_mut()
                    .get_resource_mut::<ModHost>()
                    .ok_or(ModLoadError::ModNotFound { id })?;
            // SAFETY: Systems were deactivated and removed from schedules above.
            unsafe { mod_host.try_unload_mod(id)? }
        };

        // 5. Emit `ModUnloaded` buffered message and observer event.
        let unloaded_event = ModUnloaded {
            id,
            path: Some(unloaded_path),
            removed_systems,
        };
        let world = self.world_mut();
        if let Some(mut msgs) = world.get_resource_mut::<Messages<ModUnloaded>>() {
            msgs.write(unloaded_event.clone());
        }
        world.trigger(unloaded_event);

        Ok(())
    }

    fn unload_mod(&mut self, id: ModId) -> Result<(), FfiResult> {
        self.try_unload_mod(id).map_err(ModLoadError::into)
    }
}
