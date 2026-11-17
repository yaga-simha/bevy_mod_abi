//! [`ModAppExt`] extension methods on Bevy [`App`], delegating to [`ModWorldExt`].

use crate::plugin::ensure_startup_tracker;
use crate::world_ext::ModWorldExt;
use bevy_abi_mod_host_sys::{
    ComponentCollisionError, ComponentOptions, LoadOptions, ModErrorPolicy, ModId, ModLoadError,
};
use bevy_abi_mod_sys::{AbiPod, BamComponent, ModComponent, ScheduleId};
use bevy_app::App;
use bevy_ecs::component::Component;
use bevy_ecs::event::{EntityEvent, Event};
use bevy_ecs::message::{Message, Messages};
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::{ScheduleLabel, SystemSet};
use std::path::Path;

/// Extension methods on Bevy [`App`] for exposing components/resources/messages/schedules/sets and
/// loading, reloading, or unloading native C-ABI mods.
///
/// Loading and unloading shared libraries require exclusive [`World`](bevy_ecs::world::World) or
/// [`App`] access (or deferred [`ModCommandsExt`](crate::ModCommandsExt)) so guest code cannot be
/// unmapped (`dlclose`) while parallel systems are executing:
///
/// ```compile_fail
/// use bevy_abi_mod_host::{ModHost, ModId};
/// use bevy_ecs::system::ResMut;
///
/// fn bad_parallel_unload_system(mut host: ResMut<ModHost>) {
///     // `ModHost` does not expose `unload_mod` to `ResMut<ModHost>` systems.
///     let _ = host.unload_mod(ModId(1));
/// }
/// ```
pub trait ModAppExt {
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

    /// Registers a buffered POD Bevy [`Message`] type (`Messages<M>`), adding `Messages<M>` frame
    /// maintenance to this [`App`] and returning its 1-based [`BamComponent`] handle.
    fn register_mod_message<M: Message + ModComponent + AbiPod>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Registers a triggerable host Bevy [`Event`] implementing [`ModComponent`] + [`Copy`],
    /// returning its 1-based [`BamComponent`] handle.
    fn register_mod_event<E>(&mut self) -> Result<BamComponent, ComponentCollisionError>
    where
        E: Event<Trigger<'static>: Default> + ModComponent + Copy;

    /// Registers a triggerable entity-targeted Bevy [`EntityEvent`] implementing [`ModComponent`]
    /// + [`Copy`], returning its 1-based [`BamComponent`] handle.
    fn register_mod_entity_event<E>(&mut self) -> Result<BamComponent, ComponentCollisionError>
    where
        E: EntityEvent + Event + ModComponent + Copy,
        for<'a> E::Trigger<'a>: Default;

    /// Registers a non-component [`ModComponent`] helper/nested struct type so its static VTable
    /// and field accessors are resolvable via `get_proc_address`.
    fn register_mod_type<T: ModComponent>(&mut self) -> &mut Self;

    /// Maps a game-defined custom [`ScheduleId`] (`>= 1024`) to a Bevy [`ScheduleLabel`].
    fn register_mod_schedule<S: ScheduleLabel>(
        &mut self,
        id: ScheduleId,
        label: S,
    ) -> Result<(), ModLoadError>;

    /// Exposes a host [`SystemSet`] under `"host::<name>"` in [`crate::ModVisibleSets`] so guest
    /// systems can declare `.after(c"host::<name>")` or `.before(c"host::<name>")`.
    fn register_mod_visible_set<S: SystemSet>(&mut self, name: &str, set: S) -> &mut Self;

    /// Sets the default [`ModErrorPolicy`] for guest systems in this [`App`].
    fn set_mod_error_policy(&mut self, policy: ModErrorPolicy) -> &mut Self;

    /// Loads a native shared-library mod using [`LoadOptions::default()`], returning its [`ModId`].
    fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError>;

    /// Loads a native shared-library mod with explicit [`LoadOptions`], returning its [`ModId`].
    fn load_mod_with<P: AsRef<Path>>(
        &mut self,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;

    /// Unloads a loaded mod by [`ModId`] under exclusive [`App`] access.
    fn unload_mod(&mut self, id: ModId) -> Result<(), ModLoadError>;

    /// Unloads all currently loaded mods in LIFO order.
    fn unload_all_mods(&mut self) -> &mut Self;

    /// Hot-reloads `id` from its original source path with shadow-copying enabled, transferring
    /// optional `save_state` / `restore_state` bytes.
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
}

impl ModAppExt for App {
    #[inline]
    fn register_mod_component<T: Component + ModComponent>(
        &mut self,
        options: ComponentOptions<T>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        self.world_mut().register_mod_component::<T>(options)
    }

    #[inline]
    fn register_mod_resource<R: Resource + ModComponent>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError> {
        self.world_mut().register_mod_resource::<R>()
    }

    #[inline]
    fn register_mod_message<M: Message + ModComponent + AbiPod>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError> {
        if !self.world().contains_resource::<Messages<M>>() {
            self.add_message::<M>();
        }
        self.world_mut().register_mod_message::<M>()
    }

    #[inline]
    fn register_mod_event<E>(&mut self) -> Result<BamComponent, ComponentCollisionError>
    where
        E: Event<Trigger<'static>: Default> + ModComponent + Copy,
    {
        self.world_mut().register_mod_event::<E>()
    }

    #[inline]
    fn register_mod_entity_event<E>(&mut self) -> Result<BamComponent, ComponentCollisionError>
    where
        E: EntityEvent + Event + ModComponent + Copy,
        for<'a> E::Trigger<'a>: Default,
    {
        self.world_mut().register_mod_entity_event::<E>()
    }

    #[inline]
    fn register_mod_type<T: ModComponent>(&mut self) -> &mut Self {
        self.world_mut().register_mod_type::<T>();
        self
    }

    #[inline]
    fn register_mod_schedule<S: ScheduleLabel>(
        &mut self,
        id: ScheduleId,
        label: S,
    ) -> Result<(), ModLoadError> {
        self.world_mut().register_mod_schedule(id, label)
    }

    #[inline]
    fn register_mod_visible_set<S: SystemSet>(&mut self, name: &str, set: S) -> &mut Self {
        let _ = self.world_mut().register_mod_visible_set(name, set);
        self
    }

    #[inline]
    fn set_mod_error_policy(&mut self, policy: ModErrorPolicy) -> &mut Self {
        self.world_mut().set_mod_error_policy(policy);
        self
    }

    #[inline]
    fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError> {
        self.load_mod_with(path, LoadOptions::default())
    }

    #[inline]
    fn load_mod_with<P: AsRef<Path>>(
        &mut self,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        self.world_mut().load_mod_with(path, options)
    }

    #[inline]
    fn unload_mod(&mut self, id: ModId) -> Result<(), ModLoadError> {
        self.world_mut().unload_mod(id)
    }

    #[inline]
    fn unload_all_mods(&mut self) -> &mut Self {
        self.world_mut().unload_all_mods();
        self
    }

    #[inline]
    fn reload_mod(&mut self, id: ModId) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        self.world_mut().reload_mod(id)
    }

    #[inline]
    fn reload_mod_with(&mut self, id: ModId, options: LoadOptions) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        self.world_mut().reload_mod_with(id, options)
    }

    #[inline]
    fn reload_mod_from<P: AsRef<Path>>(
        &mut self,
        id: ModId,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        self.world_mut().reload_mod_from(id, path, options)
    }
}
