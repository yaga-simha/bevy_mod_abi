//! [`ModAppExt`] extension methods on Bevy [`App`], delegating to [`ModWorldExt`] (`B-01`).

use crate::plugin::ensure_startup_tracker;
use crate::world_ext::ModWorldExt;
use bevy_abi_mod_host_sys::{
    ComponentCollisionError, ComponentOptions, LoadOptions, ModErrorPolicy, ModId, ModLoadError,
};
use bevy_abi_mod_sys::{BamComponent, ModComponent, ScheduleId};
use bevy_app::App;
use bevy_ecs::component::Component;
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::ScheduleLabel;
use std::path::Path;

/// Extension methods on Bevy [`App`] for exposing components/resources/schedules and loading or
/// unloading native C-ABI mods (`B-01`).
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

    /// Registers a non-component [`ModComponent`] helper/nested struct type so its static VTable
    /// and field accessors are resolvable via `get_proc_address`.
    fn register_mod_type<T: ModComponent>(&mut self) -> &mut Self;

    /// Maps a game-defined custom [`ScheduleId`] (`>= 1024`) to a Bevy [`ScheduleLabel`].
    fn register_mod_schedule<S: ScheduleLabel>(
        &mut self,
        id: ScheduleId,
        label: S,
    ) -> Result<(), ModLoadError>;

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
}
