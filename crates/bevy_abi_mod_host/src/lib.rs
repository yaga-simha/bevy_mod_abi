#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Safe idiomatic Bevy host crate for `bevy_abi_mod` (`ABI 0.2.0`).
//!
//! `bevy_abi_mod_host` provides Bevy ECS (`v0.20+`) integration (`ModWorldExt`, `ModAppExt`,
//! `ModCommandsExt`, `AbiModPlugin`) for loading, executing, quarantining, and hot-unloading
//! native C-ABI mods compiled in Rust, C99, C++, or Zig.
//!
//! All `unsafe` FFI internals are isolated inside `bevy_abi_mod_host_sys`; this crate enforces
//! `#![forbid(unsafe_code)]`.
//!
//! # Optional Features
//!
//! - `strict-ffi-checks`: Extra FFI pointer, alignment, and handle-generation validation.
//! - `diagnostics`: Publishes per-mod and aggregate `bevy_diagnostic` counters.
//! - `signature-verification`: Pure-Rust OpenSSH `SSHSIG` + Ed25519 CA certificate verification
//!   of shared libraries prior to `dlopen` / `LoadLibraryW` (`ModTrustPolicy`, `VerifiedModSignature`).
//! - `bridge-std-types`: Zero-copy / VTable host bridges for Bevy `Transform`, `GlobalTransform`,
//!   `Visibility`, `Name`, `Time`, `ButtonInput<KeyCode>`, and `ButtonInput<MouseButton>`.
//! - `bridge-asset`: Isolated `mod://<namespace>/...` Bevy `AssetSource` + refcounted `FfiAssetHandle`
//!   host backend (`ModAssetPlugin`).
//! - `bridge-state`: Bevy `States` exposure (`ModStateExt::expose_mod_state`), transition schedules,
//!   and `in_state` run conditions.
//! - `bridge-reflect`: Dynamic `bevy_reflect` registration, Bevy Remote Protocol (`bevy_remote`)
//!   methods, and `bevy_settings` exposure for C-ABI components/resources.
//! - `bridge-bsn`: Bevy Scene Notation (`bevy_scene`) spawning and `Ready` observer integration.
//! - `sub-app-extract`: Safe `SubApp` (`RenderApp`) extraction, custom `Shader`, and custom
//!   `Material` / `Material2d` modding support.
//! - `hot-reload-watcher`: Debounced filesystem hot-reload watcher (`ModHotReloadPlugin`).
//!
//! # Quickstart
//!
//! ```no_run
//! use bevy_abi_mod_host::prelude::*;
//! use bevy_app::{App, Update};
//! use bevy_ecs::component::Component;
//! use bevy_ecs::schedule::SystemSet;
//!
//! #[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
//! struct PhysicsSet;
//!
//! #[repr(C)]
//! #[derive(Component, Clone, Copy, Default, ModComponent)]
//! #[mod_component(name = "game::Position")]
//! struct Position {
//!     x: f32,
//!     y: f32,
//! }
//!
//! let mut app = App::new();
//! app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));
//! app.register_mod_component::<Position>(ComponentOptions::new().with_raw_insert().with_default())
//!     .unwrap();
//! app.register_mod_visible_set("physics", PhysicsSet);
//! app.load_mod_with("mods/libmovement.dylib", LoadOptions::default()).unwrap();
//! ```

pub mod app_ext;
pub mod bridges;
pub mod commands_ext;
#[cfg(feature = "diagnostics")]
pub mod diagnostics;
pub mod events;
pub mod mod_dag;
pub mod plugin;
pub mod schedule_sets;
#[cfg(feature = "sub-app-extract")]
pub mod sub_app_materials;
pub mod watcher;
pub mod world_ext;

pub use bevy_abi_mod_macros::{AbiPod, ModComponent};
pub use bevy_abi_mod_sys;

pub use app_ext::ModAppExt;
pub use bevy_abi_mod_host_sys::PersistentMessageCursors;
pub use bevy_abi_mod_host_sys::loader::{
    ExportedFieldRecord, ReloadColumnSnapshot, SnapshotComponentColumn, SnapshotEntityFields,
};
pub use bevy_abi_mod_host_sys::{
    BinaryFormat, ComponentCollisionError, ComponentOptions, LibraryResidencyInfo, LoadOptions,
    LoadedMod, ModErrorPolicy, ModErrorPolicyConfig, ModHost, ModId, ModLoadError, ModQuarantine,
    ModRegistry, ModScoped, RegisteredComponent, RegistrySnapshot, TargetEntity,
    WitnessFieldDescriptor, WorldDropGuard, apply_entity_component_fields, apply_resource_fields,
    ensure_component_in_target_world, export_entity_component_fields, export_resource_fields,
    extract_component_between_worlds, extract_resource_between_worlds,
    insert_entity_component_from_fields, insert_or_apply_entity_component_fields,
    inspect_component_witness_fields, inspect_component_witness_fields_in_registry,
    inspect_library_residency, inspect_library_residency_file, is_abi_version_compatible,
    read_entity_pod_bytes, write_entity_pod_bytes,
};
#[cfg(feature = "signature-verification")]
pub use bevy_abi_mod_host_sys::{
    CertificateValidityPolicy, DEFAULT_KRL_SIGNATURE_NAMESPACE, DEFAULT_MAX_MOD_BINARY_BYTES,
    DEFAULT_SIGNATURE_NAMESPACE, KrlBuilder, MAX_KRL_BYTES, MAX_SIGNATURE_SIDECAR_BYTES,
    ModSigningError, ModTrustConfigError, ModTrustPolicy, SignatureEnforcement,
    SignatureVerificationError, VerifiedModSignature, default_signature_path_for,
    preverify_mod_on_disk, sign_mod_bytes_with_certificate, sign_mod_bytes_with_key,
    sign_mod_bytes_with_openssh_certificate,
};
#[cfg(feature = "bridge-reflect")]
pub use bevy_abi_mod_host_sys::{
    empty_reflect_unchecked_mut, reflect_entity_component, reflect_entity_component_mut,
};
#[cfg(all(
    feature = "bridge-std-types",
    feature = "bridge-asset",
    feature = "bridge-state"
))]
pub use bridges::ModBridgesPlugin;
#[cfg(feature = "bridge-reflect")]
pub use bridges::{
    BRP_METHOD_GET_COMPONENTS, BRP_METHOD_GET_RESOURCES, BRP_METHOD_INSERT_COMPONENTS,
    BRP_METHOD_LIST_COMPONENTS, BRP_METHOD_MUTATE_COMPONENTS, BRP_METHOD_MUTATE_RESOURCES,
    ModReflectAppExt, ModReflectError, ModReflectPlugin, ModReflectRegistry, ModReflectRemoteExt,
    ModReflectWorldExt, ModSettingsError, ModSettingsRegistry, TypedReflectBridgeEntry,
    WitnessReflectSchema, handle_mod_brp_request, install_mod_brp_methods,
};
#[cfg(feature = "bridge-bsn")]
pub use bridges::{
    CANONICAL_READY_NAME, HostSceneRegistry, ModBsnAppExt, ModBsnPlugin, ModBsnWorldExt,
    SCENE_PATCH_ALIAS,
};
#[cfg(feature = "bridge-std-types")]
pub use bridges::{
    FfiGlobalTransform, FfiKeyCode, FfiKeyboardInput, FfiMouseButton, FfiMouseInput, FfiTimeFixed,
    FfiTimeReal, FfiTimeVirtual, FfiTransform, FfiVisibility, StdTypesBridgePlugin,
    register_std_types_bridges,
};
#[cfg(feature = "bridge-asset")]
pub use bridges::{ModAssetAppExt, ModAssetPlugin, ModAssetRegistry, ModAssetWorldExt};
#[cfg(feature = "bridge-state")]
pub use bridges::{ModNextState, ModStateError, ModStateExt, ModStateSpec};
pub use commands_ext::ModCommandsExt;
pub use events::{
    LoadedModInfo, ModLoaded, ModSystemError, ModSystemFaulted, ModUnloaded, SystemInitError,
};
pub use mod_dag::{
    DagResolution, DirectoryLoadReport, ModDagError, ModDagNode, ModDependencyGraph,
    ModTomlManifest, discover_mod_manifests, host_abi_semver, resolve_dependency_dag,
};
pub use plugin::{AbiModPlugin, ConfiguredAbiModPlugin};
pub use schedule_sets::{
    ModNamedSystemSet, ModScheduleMap, ModSystemSet, ModVisibleSets, StartupCompleted,
    configure_guest_system_in_world, configure_weak_set_chain,
};
#[cfg(feature = "sub-app-extract")]
pub use sub_app_materials::{
    ModMaterialError, ModShaderBufferInit, ModShaderBufferSpec, ModSpriteMaterialEntry,
    ModSpriteMaterialSpec, ModStandardMaterialEntry, ModStandardMaterialSpec, ModSubAppConfig,
    ModSubAppExt, ModSubAppPlugin, ModSubAppSchedule, ModSubAppSystems, ModSubWorldExt,
    ModWeslMaterialPlugin, ModWeslMaterialRegistry, WeslShaderAssetSpec, WeslShaderSource,
    cleanup_mod_wesl_materials_in_world, register_mod_custom_asset_in_world,
    register_mod_shader_buffer_in_world, register_mod_sprite_material_in_world,
    register_mod_standard_material_in_world, register_mod_wesl_shader_in_world,
    update_mod_shader_buffer_in_world,
};
#[cfg(all(feature = "sub-app-extract", feature = "bridge-reflect"))]
pub use sub_app_materials::{
    ModMaterialParams, ModSpriteExtension2d, register_mod_extended_sprite_material_in_world,
};
#[cfg(feature = "hot-reload-watcher")]
pub use watcher::{
    ModHotReloadAction, ModHotReloadHistory, ModHotReloadOutcome, ModHotReloadPlugin,
    ModHotReloadWatcher,
};
pub use world_ext::ModWorldExt;

/// Convenient prelude module for Bevy games hosting native C-ABI mods (`ABI 0.2.0`).
pub mod prelude {
    pub use crate::app_ext::ModAppExt;
    #[cfg(all(
        feature = "bridge-std-types",
        feature = "bridge-asset",
        feature = "bridge-state"
    ))]
    pub use crate::bridges::ModBridgesPlugin;
    #[cfg(feature = "bridge-std-types")]
    pub use crate::bridges::{
        FfiGlobalTransform, FfiKeyCode, FfiKeyboardInput, FfiMouseButton, FfiMouseInput,
        FfiTimeFixed, FfiTimeReal, FfiTimeVirtual, FfiTransform, FfiVisibility,
        StdTypesBridgePlugin,
    };
    #[cfg(feature = "bridge-bsn")]
    pub use crate::bridges::{HostSceneRegistry, ModBsnAppExt, ModBsnPlugin, ModBsnWorldExt};
    #[cfg(feature = "bridge-asset")]
    pub use crate::bridges::{ModAssetAppExt, ModAssetPlugin, ModAssetRegistry, ModAssetWorldExt};
    #[cfg(feature = "bridge-state")]
    pub use crate::bridges::{ModNextState, ModStateError, ModStateExt, ModStateSpec};
    #[cfg(feature = "bridge-reflect")]
    pub use crate::bridges::{
        ModReflectAppExt, ModReflectError, ModReflectPlugin, ModReflectRegistry,
        ModReflectRemoteExt, ModReflectWorldExt, ModSettingsError, ModSettingsRegistry,
    };
    pub use crate::commands_ext::ModCommandsExt;
    #[cfg(feature = "signature-verification")]
    pub use crate::events::VerifiedModSignature;
    pub use crate::events::{
        LoadedModInfo, ModLoaded, ModSystemError, ModSystemFaulted, ModUnloaded, SystemInitError,
    };
    pub use crate::mod_dag::{
        DirectoryLoadReport, ModDagError, ModDependencyGraph, ModTomlManifest,
    };
    pub use crate::plugin::{AbiModPlugin, ConfiguredAbiModPlugin};
    pub use crate::schedule_sets::{
        ModNamedSystemSet, ModScheduleMap, ModSystemSet, ModVisibleSets, StartupCompleted,
        configure_guest_system_in_world, configure_weak_set_chain,
    };
    #[cfg(feature = "sub-app-extract")]
    pub use crate::sub_app_materials::{
        ModMaterialError, ModShaderBufferInit, ModShaderBufferSpec, ModSpriteMaterialSpec,
        ModStandardMaterialSpec, ModSubAppConfig, ModSubAppExt, ModSubAppPlugin, ModSubAppSchedule,
        ModSubAppSystems, ModSubWorldExt, ModWeslMaterialPlugin, ModWeslMaterialRegistry,
        WeslShaderAssetSpec, WeslShaderSource,
    };
    #[cfg(all(feature = "sub-app-extract", feature = "bridge-reflect"))]
    pub use crate::sub_app_materials::{ModMaterialParams, ModSpriteExtension2d};
    #[cfg(feature = "hot-reload-watcher")]
    pub use crate::watcher::{
        ModHotReloadAction, ModHotReloadHistory, ModHotReloadOutcome, ModHotReloadPlugin,
        ModHotReloadWatcher,
    };
    pub use crate::world_ext::ModWorldExt;
    pub use crate::{
        AbiPod, ComponentCollisionError, ComponentOptions, LoadOptions, LoadedMod, ModComponent,
        ModErrorPolicy, ModHost, ModId, ModLoadError, ModQuarantine, ModRegistry, ModScoped,
        PersistentMessageCursors, TargetEntity,
    };
    #[cfg(feature = "signature-verification")]
    pub use crate::{
        CertificateValidityPolicy, DEFAULT_KRL_SIGNATURE_NAMESPACE, DEFAULT_MAX_MOD_BINARY_BYTES,
        DEFAULT_SIGNATURE_NAMESPACE, KrlBuilder, MAX_KRL_BYTES, MAX_SIGNATURE_SIDECAR_BYTES,
        ModSigningError, ModTrustConfigError, ModTrustPolicy, SignatureEnforcement,
        SignatureVerificationError, default_signature_path_for, preverify_mod_on_disk,
        sign_mod_bytes_with_certificate, sign_mod_bytes_with_key,
        sign_mod_bytes_with_openssh_certificate,
    };
    pub use bevy_abi_mod_sys::{
        ABI_VERSION, AccessMode, BAM_COMPONENT_FLAG_SUMMARY_TICK, BAM_OBSERVER_FLAG_PROPAGATE,
        BAM_SYSTEM_FLAG_AMBIGUOUS_WITH_ALL, BAM_TRIGGER_FLAG_PROPAGATE, BamChangeTicks,
        BamColumnInfo, BamComponent, BamEventKind, BamObserverDesc, BamObserverFn,
        BamObserverMutFn, BamQueryDesc, BamRunIfFn, BamSystemDesc, BamTermDesc, BamTriggerDesc,
        ComponentDesc, ComponentUuid, FfiAssetHandle, FfiAssetLoadState, FfiBool, FfiChildOf,
        FfiEntity, FfiOption, FfiResult, FfiSlice, RawInsertable, ScheduleId, StrView,
        VTableHeader,
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

        // Handles `1` (`ChildOf`) and `2` (`Children`) are reserved by `AbiModPlugin`.
        assert_eq!(health_h, BamComponent::from_raw(3));
        assert_eq!(tag_h, BamComponent::from_raw(4));
        assert_eq!(grav_h, BamComponent::from_raw(5));

        let reg = app.world().resource::<ModRegistry>();
        assert_eq!(
            reg.get_by_name(bevy_abi_mod_sys::BAM_CHILD_OF_NAME)
                .map(|c| c.handle),
            Some(BamComponent::from_raw(1))
        );
        assert_eq!(
            reg.get_by_name(bevy_abi_mod_sys::BAM_CHILDREN_NAME)
                .map(|c| c.handle),
            Some(BamComponent::from_raw(2))
        );
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

        // Frame 1: runs and faults -> quarantined; emits exactly ONE `ModSystemFaulted` message.
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

        // Re-enable via `ModQuarantine::reenable` -> Frame 3 executes cleanly!
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
