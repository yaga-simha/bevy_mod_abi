#![forbid(unsafe_code)]

//! Safe idiomatic Bevy host crate for `bevy_abi_mod` (`ABI 0.2.0`).
//!
//! `bevy_abi_mod_host` provides Bevy ECS (`v0.20+`) integration (`ModWorldExt`, `ModAppExt`,
//! `ModCommandsExt`, `AbiModPlugin`) for loading, executing, quarantining, and hot-unloading
//! native C-ABI mods compiled in Rust, C99, C++, or Zig.
//!
//! All `unsafe` FFI internals are isolated inside `bevy_abi_mod_host_sys`; this crate enforces
//! `#![forbid(unsafe_code)]`.

pub mod app_ext;
pub mod commands_ext;
pub mod events;
pub mod plugin;
pub mod schedule_sets;
pub mod world_ext;

pub use bevy_abi_mod_macros::{AbiPod, ModComponent};
pub use bevy_abi_mod_sys;

pub use app_ext::ModAppExt;
pub use bevy_abi_mod_host_sys::{
    ComponentCollisionError, ComponentOptions, LoadOptions, LoadedMod, ModErrorPolicy,
    ModErrorPolicyConfig, ModHost, ModId, ModLoadError, ModQuarantine, ModRegistry,
    RegisteredComponent, RegistrySnapshot, WorldDropGuard, is_abi_version_compatible,
};
pub use commands_ext::ModCommandsExt;
pub use events::{
    LoadedModInfo, ModLoaded, ModSystemError, ModSystemFaulted, ModUnloaded, SystemInitError,
};
pub use plugin::{AbiModPlugin, ConfiguredAbiModPlugin};
pub use schedule_sets::{ModNamedSystemSet, ModScheduleMap, ModSystemSet, StartupCompleted};
pub use world_ext::ModWorldExt;

/// Convenient prelude module for Bevy games hosting native C-ABI mods (`ABI 0.2.0`).
pub mod prelude {
    pub use crate::app_ext::ModAppExt;
    pub use crate::commands_ext::ModCommandsExt;
    pub use crate::events::{
        LoadedModInfo, ModLoaded, ModSystemError, ModSystemFaulted, ModUnloaded, SystemInitError,
    };
    pub use crate::plugin::{AbiModPlugin, ConfiguredAbiModPlugin};
    pub use crate::schedule_sets::{
        ModNamedSystemSet, ModScheduleMap, ModSystemSet, StartupCompleted,
    };
    pub use crate::world_ext::ModWorldExt;
    pub use crate::{
        AbiPod, ComponentCollisionError, ComponentOptions, LoadOptions, LoadedMod, ModComponent,
        ModErrorPolicy, ModHost, ModId, ModLoadError, ModQuarantine, ModRegistry,
    };
    pub use bevy_abi_mod_sys::{
        ABI_VERSION, AccessMode, BamColumnInfo, BamComponent, BamQueryDesc, BamRunIfFn,
        BamSystemDesc, BamTermDesc, ComponentDesc, ComponentUuid, FfiBool, FfiEntity, FfiResult,
        FfiSlice, RawInsertable, ScheduleId, StrView, VTableHeader,
    };
}

#[cfg(test)]
mod tests {
    use crate::prelude::*;
    use bevy_abi_mod_host_sys::GuestSystem;
    use bevy_abi_mod_sys::{
        BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_RESOURCE, BAM_STORAGE_FLAG_SPARSE_SET,
        BamSystemCtx,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::message::Messages;
    use bevy_ecs::prelude::Resource;
    use bevy_ecs::schedule::ScheduleLabel;
    use bevy_ecs::system::Commands;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicUsize, Ordering};

    #[repr(C)]
    #[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
    #[mod_component(name = "host_unit::Health")]
    struct Health {
        hp: f32,
        max_hp: f32,
    }

    #[derive(Component, Default, Debug, ModComponent)]
    #[mod_component(name = "host_unit::UnitTag")]
    struct UnitTag {
        label: String,
        active: bool,
    }

    #[repr(C)]
    #[derive(Resource, Clone, Copy, Default, Debug, ModComponent)]
    #[mod_component(name = "host_unit::Gravity")]
    struct Gravity {
        g: f32,
    }

    #[derive(ScheduleLabel, Clone, Copy, Debug, PartialEq, Eq, Hash)]
    struct SimulationStep;

    #[test]
    fn world_and_app_ext_register_components_resources_and_custom_schedules() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        let health_h = app
            .register_mod_component::<Health>(
                ComponentOptions::new().with_raw_insert().with_default(),
            )
            .unwrap();
        let tag_h = app
            .register_mod_component::<UnitTag>(
                ComponentOptions::new()
                    .with_default()
                    .immutable()
                    .sparse_set(),
            )
            .unwrap();
        let grav_h = app.register_mod_resource::<Gravity>().unwrap();

        assert_eq!(health_h, BamComponent::from_raw(1));
        assert_eq!(tag_h, BamComponent::from_raw(2));
        assert_eq!(grav_h, BamComponent::from_raw(3));

        let reg = app.world().resource::<ModRegistry>();
        let health_meta = reg.get_by_handle(health_h).unwrap();
        assert!(health_meta.insert_raw_fn.is_some());
        assert!(health_meta.insert_default_fn.is_some());

        let tag_meta = reg.get_by_handle(tag_h).unwrap();
        assert!(tag_meta.insert_raw_fn.is_none());
        assert!(tag_meta.insert_default_fn.is_some());
        assert_eq!(
            tag_meta.storage_flags & (BAM_STORAGE_FLAG_IMMUTABLE | BAM_STORAGE_FLAG_SPARSE_SET),
            BAM_STORAGE_FLAG_IMMUTABLE | BAM_STORAGE_FLAG_SPARSE_SET
        );

        let grav_meta = reg.get_by_handle(grav_h).unwrap();
        assert_eq!(
            grav_meta.storage_flags & BAM_STORAGE_FLAG_RESOURCE,
            BAM_STORAGE_FLAG_RESOURCE
        );

        // Built-in schedule IDs (`< 1024`) rejected by `register_mod_schedule`; custom `>= 1024` accepted.
        assert!(matches!(
            app.register_mod_schedule(ScheduleId::UPDATE, SimulationStep),
            Err(ModLoadError::InvalidScheduleRegistration { .. })
        ));
        let custom_id = ScheduleId::custom(1024);
        app.register_mod_schedule(custom_id, SimulationStep)
            .unwrap();
        assert!(app.world().resource::<ModScheduleMap>().contains(custom_id));
    }

    #[test]
    fn quarantine_reenable_and_single_message_emission() {
        static CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

        extern "C" fn faulting_once_cb(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
            let prev = CALL_COUNT.fetch_add(1, Ordering::SeqCst);
            if prev == 0 {
                FfiResult::INTERNAL_ERROR
            } else {
                FfiResult::OK
            }
        }

        CALL_COUNT.store(0, Ordering::SeqCst);
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        let mod_id = ModId(42);
        let reg = app.world().resource::<ModRegistry>().clone();
        let sys = GuestSystem::new(
            "faulting_once",
            Vec::new(),
            faulting_once_cb,
            core::ptr::null_mut(),
            reg,
        )
        .with_mod_id(mod_id);

        app.world_mut()
            .resource_mut::<ModQuarantine>()
            .register_system(mod_id, sys.system_name_arc(), sys.enabled_flag());
        app.add_systems(Update, sys);

        // Frame 1: runs and faults -> quarantined; emits exactly ONE `ModSystemFaulted` message (`B-09`).
        app.update();
        assert_eq!(CALL_COUNT.load(Ordering::SeqCst), 1);
        assert!(
            app.world()
                .resource::<ModQuarantine>()
                .is_quarantined(mod_id, "faulting_once")
        );
        assert_eq!(
            app.world().resource::<Messages<ModSystemFaulted>>().len(),
            1
        );

        // Frame 2: system remains quarantined -> callback not called.
        app.update();
        assert_eq!(CALL_COUNT.load(Ordering::SeqCst), 1);

        // Re-enable via `ModQuarantine::reenable` (`B-05`) -> Frame 3 executes cleanly!
        assert!(
            app.world()
                .resource::<ModQuarantine>()
                .reenable(mod_id, "faulting_once")
        );
        app.update();
        assert_eq!(CALL_COUNT.load(Ordering::SeqCst), 2);
        assert!(
            !app.world()
                .resource::<ModQuarantine>()
                .is_quarantined(mod_id, "faulting_once")
        );
    }

    #[test]
    fn deferred_mod_commands_ext_executes_cleanly_on_flush() {
        fn trigger_deferred_ops(mut commands: Commands) {
            commands.unload_mod(ModId(999_999));
        }

        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.add_systems(Update, trigger_deferred_ops);
        app.update();
        assert!(app.world().resource::<ModHost>().is_empty());
    }
}
