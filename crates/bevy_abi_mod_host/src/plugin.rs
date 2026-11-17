//! Bevy [`Plugin`] types for initializing the native C-ABI modding host (`ABI 0.2.0`).

pub use crate::app_ext::ModAppExt;
pub use crate::commands_ext::ModCommandsExt;
pub use crate::events::{LoadedModInfo, ModLoaded, ModUnloaded};
pub use crate::schedule_sets::{ModNamedSystemSet, ModScheduleMap, ModVisibleSets};
pub use crate::world_ext::ModWorldExt;

use crate::commands_ext::{PendingModLifecycleOps, flush_pending_mod_lifecycle_ops};
use bevy_abi_mod_host_sys::{
    ModErrorPolicy, ModErrorPolicyConfig, ModHost, ModQuarantine, ModRegistry, ModSystemFaulted,
    StartupCompleted, WorldDropGuard, mark_startup_completed,
};
use bevy_app::{App, Last, Plugin, PostStartup, Update};
use bevy_ecs::message::Messages;

pub(crate) fn ensure_startup_tracker(app: &mut App) {
    if !app.world().contains_resource::<StartupCompleted>() {
        app.insert_resource(StartupCompleted(false));
        app.add_systems(PostStartup, mark_startup_completed);
    }
}

/// Bevy [`Plugin`] initializing the native ABI modding framework with default settings.
///
/// Kept as a zero-sized unit struct so both `app.add_plugins(AbiModPlugin)` and
/// `app.add_plugins(AbiModPlugin::default())` work idiomatically, while
/// [`AbiModPlugin::with_error_policy`] configures custom fault handling.
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
        if !app.world().contains_resource::<WorldDropGuard>() {
            app.insert_resource(WorldDropGuard::new());
        }
        if !app.world().contains_resource::<ModRegistry>() {
            app.insert_resource(ModRegistry::new());
        }
        if !app.world().contains_resource::<ModHost>() {
            app.insert_resource(ModHost::new());
        }
        if !app.world().contains_resource::<ModQuarantine>() {
            app.insert_resource(ModQuarantine::new());
        }
        if !app.world().contains_resource::<ModScheduleMap>() {
            app.insert_resource(ModScheduleMap::new());
        }
        if !app.world().contains_resource::<ModVisibleSets>() {
            app.insert_resource(ModVisibleSets::new());
        }
        app.insert_resource(ModErrorPolicyConfig(self.error_policy));
        app.init_schedule(Update);
        if !app.world().contains_resource::<PendingModLifecycleOps>() {
            app.init_resource::<PendingModLifecycleOps>();
            app.add_systems(Last, flush_pending_mod_lifecycle_ops);
        }

        #[cfg(feature = "diagnostics")]
        {
            crate::diagnostics::ensure_diagnostics_resources(app.world_mut());
            app.add_systems(Last, crate::diagnostics::flush_mod_diagnostics);
        }

        ensure_startup_tracker(app);

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
        crate::world_ext::ensure_hierarchy_registered(app.world_mut());
    }
}
