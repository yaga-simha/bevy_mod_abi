//! # Bevy Plugin & Application Integration
//!
//! This module provides [`AbiModPlugin`], [`ConfiguredAbiModPlugin`], ECS mod metadata
//! ([`LoadedModInfo`]), lifecycle events/messages ([`ModLoaded`], [`ModUnloaded`]), and the
//! [`ModAppExt`] extension trait to integrate dynamic C-ABI native mods idiomatically into
//! Bevy's application lifecycle, schedules, and observer/message systems.

use bevy_abi_mod_host_sys::{
    ComponentCollisionError, DynamicCSystem, ModErrorPolicy, ModErrorPolicyConfig, ModHost, ModId,
    ModLoadError, ModRegistry, ModSchedule, ModSystemFaulted, excise_and_unload_mod_in_world,
    load_mod_in_world, register_mod_component_in_world, register_mod_component_opaque_in_world,
    register_mod_component_opaque_with_default_in_world,
    register_mod_component_with_default_in_world, register_mod_resource_in_world,
    register_mod_type_in_world, rollback_unscheduled_mod_in_world,
    try_register_mod_component_in_world, try_register_mod_resource_in_world,
};
use bevy_abi_mod_sys::{ExposableComponent, FfiResult};
use bevy_app::{App, FixedUpdate, Last, Plugin, PostUpdate, PreUpdate, Startup, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::event::Event;
use bevy_ecs::message::{Message, Messages};
use bevy_ecs::name::Name;
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::{IntoScheduleConfigs, ScheduleLabel, SystemSet};
use std::path::{Path, PathBuf};

/// Named [`SystemSet`] used to resolve intra-mod and cross-mod `after` / `before` ordering
/// constraints declared in [`bevy_abi_mod_sys::ScheduledSystemDescriptor`].
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModNamedSystemSet(pub String);

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

fn add_configured_system_to_label<S: ScheduleLabel>(
    app: &mut App,
    label: S,
    sys: DynamicCSystem,
    with_named_sets: bool,
) {
    if !with_named_sets {
        app.add_systems(label, sys);
        return;
    }

    let own_name = sys.system_name().to_string();
    let after = sys.after_systems().to_vec();
    let before = sys.before_systems().to_vec();
    let mut cfg = sys.in_set(ModNamedSystemSet(own_name));
    for dep in after {
        cfg = cfg.after(ModNamedSystemSet(dep));
    }
    for dep in before {
        cfg = cfg.before(ModNamedSystemSet(dep));
    }
    app.add_systems(label, cfg);
}

fn add_system_for_mod_schedule(
    app: &mut App,
    schedule: ModSchedule,
    sys: DynamicCSystem,
    with_named_sets: bool,
) {
    match schedule {
        ModSchedule::Update => add_configured_system_to_label(app, Update, sys, with_named_sets),
        ModSchedule::Startup => add_configured_system_to_label(app, Startup, sys, with_named_sets),
        ModSchedule::PreUpdate => {
            add_configured_system_to_label(app, PreUpdate, sys, with_named_sets)
        }
        ModSchedule::PostUpdate => {
            add_configured_system_to_label(app, PostUpdate, sys, with_named_sets)
        }
        ModSchedule::FixedUpdate => {
            add_configured_system_to_label(app, FixedUpdate, sys, with_named_sets)
        }
        ModSchedule::Last => add_configured_system_to_label(app, Last, sys, with_named_sets),
    }
}

fn add_systems_batch(app: &mut App, systems: Vec<DynamicCSystem>) {
    let has_ordering = systems
        .iter()
        .any(|s| !s.after_systems().is_empty() || !s.before_systems().is_empty());
    for sys in systems {
        let sched = sys.schedule();
        add_system_for_mod_schedule(app, sched, sys, has_ordering);
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
    /// removes all systems in [`ModSystemSet::Mod`](bevy_abi_mod_host_sys::ModSystemSet::Mod) from Bevy schedules,
    /// despawns its [`LoadedModInfo`] entity, invokes the guest teardown hook, and unmaps the library.
    fn unload_mod(&mut self, id: ModId) -> Result<(), FfiResult>;

    /// Typed variant of [`ModAppExt::unload_mod`] returning [`ModLoadError`].
    fn try_unload_mod(&mut self, id: ModId) -> Result<(), ModLoadError>;
}

impl ModAppExt for App {
    fn register_mod_component<T: Component + ExposableComponent>(&mut self) -> &mut Self {
        register_mod_component_in_world::<T>(self.world_mut());
        self
    }

    fn try_register_mod_component<T: Component + ExposableComponent>(
        &mut self,
    ) -> Result<&mut Self, ComponentCollisionError> {
        try_register_mod_component_in_world::<T>(self.world_mut())?;
        Ok(self)
    }

    fn register_mod_component_with_default<T: Component + ExposableComponent + Default>(
        &mut self,
    ) -> &mut Self {
        register_mod_component_with_default_in_world::<T>(self.world_mut());
        self
    }

    fn register_mod_component_opaque<T: Component + ExposableComponent>(&mut self) -> &mut Self {
        register_mod_component_opaque_in_world::<T>(self.world_mut());
        self
    }

    fn register_mod_component_opaque_with_default<T: Component + ExposableComponent + Default>(
        &mut self,
    ) -> &mut Self {
        register_mod_component_opaque_with_default_in_world::<T>(self.world_mut());
        self
    }

    fn register_mod_resource<R: Resource + ExposableComponent>(&mut self) -> &mut Self {
        register_mod_resource_in_world::<R>(self.world_mut());
        self
    }

    fn try_register_mod_resource<R: Resource + ExposableComponent>(
        &mut self,
    ) -> Result<&mut Self, ComponentCollisionError> {
        try_register_mod_resource_in_world::<R>(self.world_mut())?;
        Ok(self)
    }

    fn register_mod_type<T: ExposableComponent>(&mut self) -> &mut Self {
        register_mod_type_in_world::<T>(self.world_mut());
        self
    }

    fn set_mod_error_policy(&mut self, policy: ModErrorPolicy) -> &mut Self {
        self.insert_resource(ModErrorPolicyConfig(policy));
        self
    }

    fn try_load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError> {
        let path_buf = path.as_ref().to_path_buf();
        let (mod_id, systems) = load_mod_in_world(self.world_mut(), &path_buf)?;

        let system_count = systems.len();
        add_systems_batch(self, systems);

        finalize_mod_load_in_world(self.world_mut(), mod_id, path_buf, system_count);
        Ok(mod_id)
    }

    fn try_load_mod_strict<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError> {
        let path_buf = path.as_ref().to_path_buf();
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
        add_systems_batch(self, systems);

        finalize_mod_load_in_world(self.world_mut(), mod_id, path_buf, system_count);
        Ok(mod_id)
    }

    fn try_load_mod_into_schedule<S: ScheduleLabel + Clone, P: AsRef<Path>>(
        &mut self,
        schedule: S,
        path: P,
    ) -> Result<ModId, ModLoadError> {
        let path_buf = path.as_ref().to_path_buf();
        let (mod_id, systems) = load_mod_in_world(self.world_mut(), &path_buf)?;

        let system_count = systems.len();
        let has_ordering = systems
            .iter()
            .any(|s| !s.after_systems().is_empty() || !s.before_systems().is_empty());
        for sys in systems {
            add_configured_system_to_label(self, schedule.clone(), sys, has_ordering);
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
        let (unloaded_path, removed_systems) =
            excise_and_unload_mod_in_world(self.world_mut(), id, |world| {
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
            })?;

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
