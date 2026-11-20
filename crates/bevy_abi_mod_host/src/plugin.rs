//! Bevy [`Plugin`] types for initializing the native C-ABI modding host (`ABI 0.3.0`).

pub use crate::app_ext::ModAppExt;
pub use crate::budgets::{
    ModBudgetBreach, ModBudgetBreachKind, ModBudgetExt, ModBudgetLimits, ModBudgetPolicy,
    ModBudgetSnapshot, ModBudgetTelemetry,
};
pub use crate::commands_ext::ModCommandsExt;
pub use crate::events::{LoadedModInfo, ModLoaded, ModUnloaded};
pub use crate::schedule_sets::{ModNamedSystemSet, ModScheduleMap, ModVisibleSets};
pub use crate::world_ext::ModWorldExt;

use crate::budgets::{
    BudgetSystemsInstalled, cleanup_mod_budgets_in_world, ensure_budget_resources,
    flush_mod_budgets, sync_mod_budget_policy,
};
use crate::commands_ext::{PendingModLifecycleOps, flush_pending_mod_lifecycle_ops};
use crate::schedule_sets::ModSystemSet;
use crate::world_ext::{ensure_world_mod_resources, register_mod_unload_hook_in_world};
#[cfg(feature = "signature-verification")]
use bevy_abi_mod_host_sys::ModTrustPolicy;
use bevy_abi_mod_host_sys::{
    ModErrorPolicy, ModErrorPolicyConfig, ModSystemFaulted, StartupCompleted,
    mark_startup_completed,
};
use bevy_app::{App, First, Last, Plugin, PostStartup, Update};
use bevy_ecs::message::{Message, Messages};
use bevy_ecs::schedule::IntoScheduleConfigs;

pub(crate) fn ensure_startup_tracker(app: &mut App) {
    if !app.world().contains_resource::<StartupCompleted>() {
        app.insert_resource(StartupCompleted(false));
        app.add_systems(PostStartup, mark_startup_completed);
    }
}

fn ensure_app_message<M: Message>(app: &mut App) {
    if !app.world().contains_resource::<Messages<M>>() {
        app.add_message::<M>();
    }
}

/// Bevy [`Plugin`] initializing the native ABI modding framework with default settings.
///
/// Kept as a zero-sized unit struct so both `app.add_plugins(AbiModPlugin)` and
/// `app.add_plugins(AbiModPlugin::default())` work idiomatically, while
/// [`AbiModPlugin::with_error_policy`] (and `AbiModPlugin::with_trust_policy` when
/// `signature-verification` is enabled) configure custom policies.
#[derive(Default, Debug, Clone, Copy)]
pub struct AbiModPlugin;

impl AbiModPlugin {
    /// Returns a configured plugin using the specified [`ModErrorPolicy`].
    pub const fn with_error_policy(policy: ModErrorPolicy) -> ConfiguredAbiModPlugin {
        ConfiguredAbiModPlugin {
            error_policy: policy,
            budget_policy: None,
            #[cfg(feature = "signature-verification")]
            trust_policy: None,
        }
    }

    /// Returns a configured plugin enforcing the specified [`ModBudgetPolicy`].
    pub fn with_budget_policy(policy: ModBudgetPolicy) -> ConfiguredAbiModPlugin {
        ConfiguredAbiModPlugin {
            error_policy: ModErrorPolicy::default(),
            budget_policy: Some(policy),
            #[cfg(feature = "signature-verification")]
            trust_policy: None,
        }
    }

    /// Returns a configured plugin enforcing the given [`ModTrustPolicy`] at load time.
    #[cfg(feature = "signature-verification")]
    pub fn with_trust_policy(policy: ModTrustPolicy) -> ConfiguredAbiModPlugin {
        ConfiguredAbiModPlugin {
            error_policy: ModErrorPolicy::default(),
            budget_policy: None,
            trust_policy: Some(policy),
        }
    }
}

impl Plugin for AbiModPlugin {
    fn build(&self, app: &mut App) {
        ConfiguredAbiModPlugin::default().build(app);
    }
}

/// Configurable variant of [`AbiModPlugin`] created via [`AbiModPlugin::with_error_policy`],
/// [`AbiModPlugin::with_budget_policy`], or `AbiModPlugin::with_trust_policy`.
#[derive(Default, Debug, Clone)]
pub struct ConfiguredAbiModPlugin {
    /// Default [`ModErrorPolicy`] inserted into [`ModErrorPolicyConfig`].
    pub error_policy: ModErrorPolicy,
    /// Optional [`ModBudgetPolicy`] inserted into `World` on plugin build.
    pub budget_policy: Option<ModBudgetPolicy>,
    /// Optional [`ModTrustPolicy`] inserted into `World` on plugin build.
    #[cfg(feature = "signature-verification")]
    pub trust_policy: Option<ModTrustPolicy>,
}

impl ConfiguredAbiModPlugin {
    /// Overrides the [`ModErrorPolicy`] applied to guest systems.
    #[must_use]
    pub const fn with_error_policy(mut self, policy: ModErrorPolicy) -> Self {
        self.error_policy = policy;
        self
    }

    /// Overrides the [`ModBudgetPolicy`] inserted into `World` when building the plugin.
    #[must_use]
    pub fn with_budget_policy(mut self, policy: ModBudgetPolicy) -> Self {
        self.budget_policy = Some(policy);
        self
    }

    /// Overrides the [`ModTrustPolicy`] inserted into `World` when building the plugin.
    #[cfg(feature = "signature-verification")]
    #[must_use]
    pub fn with_trust_policy(mut self, policy: ModTrustPolicy) -> Self {
        self.trust_policy = Some(policy);
        self
    }
}

impl Plugin for ConfiguredAbiModPlugin {
    fn build(&self, app: &mut App) {
        // Install `App`-level message update systems and `PostStartup` tracker BEFORE
        // `ensure_world_mod_resources` initializes bare `World` resources.
        ensure_app_message::<ModBudgetBreach>(app);
        ensure_app_message::<ModLoaded>(app);
        ensure_app_message::<ModUnloaded>(app);
        ensure_app_message::<ModSystemFaulted>(app);
        ensure_startup_tracker(app);

        #[cfg(feature = "signature-verification")]
        if let Some(policy) = &self.trust_policy {
            app.insert_resource(policy.clone());
        }
        app.insert_resource(ModErrorPolicyConfig(self.error_policy));
        if let Some(policy) = &self.budget_policy {
            app.insert_resource(policy.clone());
        }

        ensure_world_mod_resources(app.world_mut());
        ensure_budget_resources(app.world_mut());
        register_mod_unload_hook_in_world(
            app.world_mut(),
            "bevy_abi_mod_host::budgets",
            cleanup_mod_budgets_in_world,
        );
        app.init_schedule(Update);
        if !app.world().contains_resource::<BudgetSystemsInstalled>() {
            app.init_resource::<BudgetSystemsInstalled>();
            app.add_systems(First, sync_mod_budget_policy.before(ModSystemSet::All));
            app.add_systems(Last, flush_mod_budgets);
        }
        if !app.world().contains_resource::<PendingModLifecycleOps>() {
            app.init_resource::<PendingModLifecycleOps>();
            app.add_systems(Last, flush_pending_mod_lifecycle_ops);
        }

        #[cfg(feature = "diagnostics")]
        app.add_systems(
            Last,
            crate::diagnostics::flush_mod_diagnostics.after(flush_mod_budgets),
        );
    }
}
