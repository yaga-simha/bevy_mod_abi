//! ABI SemVer handshake compatibility, `try_load_mod_strict`, lifecycle events,
//! truncated descriptors, and invalid lifecycle operations.

use crate::support::{Position, Velocity, ensure_guest_mods_built, reset_rust_mod_panic};
use bevy_abi_mod_host::{
    AbiModPlugin, LoadedModInfo, ModAppExt, ModHost, ModId, ModLoadError, ModLoaded, ModRegistry,
    ModUnloaded, is_abi_version_compatible,
};
use bevy_abi_mod_host_sys::{
    ModInitContext, host_register_component, host_register_multi_query_system,
    host_register_scheduled_system, host_register_system,
};
use bevy_abi_mod_sys::{
    ABI_VERSION_PACKED, FfiResult, ModComponentDescriptor, MultiQuerySystemDescriptor,
    ScheduledMultiQuerySystemDescriptor, ScheduledSystemDescriptor, SystemDescriptor,
};
use bevy_app::App;
use bevy_ecs::message::Messages;
use bevy_ecs::observer::On;
use core::ffi::c_void;
use std::sync::{Arc, Mutex};

#[test]
fn semver_abi_compatibility_and_typed_mod_load_error_work() {
    const fn pack_ver(maj: u32, min: u32, patch: u32) -> u32 {
        (maj << 24) | (min << 16) | patch
    }

    // Host is 0.1.0 (`ABI_VERSION_PACKED`): pre-1.0 requires exact minor match (`C-06`)
    assert!(is_abi_version_compatible(
        ABI_VERSION_PACKED,
        ABI_VERSION_PACKED
    ));
    assert!(!is_abi_version_compatible(
        ABI_VERSION_PACKED,
        pack_ver(0, 0, 9)
    ));
    assert!(is_abi_version_compatible(
        ABI_VERSION_PACKED,
        pack_ver(0, 1, 99)
    ));
    assert!(!is_abi_version_compatible(
        ABI_VERSION_PACKED,
        pack_ver(0, 2, 0)
    ));
    assert!(!is_abi_version_compatible(
        ABI_VERSION_PACKED,
        pack_ver(1, 0, 0)
    ));

    #[cfg(not(miri))]
    let err = {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.try_load_mod("/nonexistent/path/to/missing_mod.dylib")
            .expect_err("Missing dylib must return ModLoadError::DlOpen")
    };
    #[cfg(miri)]
    let err = ModLoadError::DlOpen {
        path: std::path::PathBuf::from("/nonexistent/path/to/missing_mod.dylib"),
        message: String::from("dlopen unavailable under Miri isolation"),
    };
    assert!(matches!(err, ModLoadError::DlOpen { .. }));
    assert_eq!(FfiResult::from(err.clone()), FfiResult::INTERNAL_ERROR);
    assert!(err.to_string().contains("missing_mod.dylib"));

    let _ = core::mem::size_of::<LoadedModInfo>();
    let _ = core::mem::size_of::<ModLoaded>();
    let _ = core::mem::size_of::<ModUnloaded>();
}

#[test]
fn real_dylib_loaded_mod_entity_lifecycle_events_and_strict_validation_work() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    // 1. Strict mode validation: when `CombatHealth_vtable` symbol is exposed so `asymmetric_rust_mod`
    // registers `rust_combat` querying `"CombatHealth"`, WITHOUT registering `CombatHealth` as an ECS
    // component in `ModRegistry`, `try_load_mod_strict` must fail fast with `ModLoadError::MissingComponents`
    // and cleanly unload the library.
    {
        let mut strict_app = App::new();
        strict_app.add_plugins(AbiModPlugin);
        strict_app.register_mod_component::<Position>();
        strict_app.register_mod_component::<Velocity>();
        // SAFETY: Stub non-null symbol address is only inspected for nullness during `mod_init`
        // before strict validation aborts and unloads the module without running systems.
        unsafe {
            strict_app
                .world_mut()
                .resource_mut::<ModRegistry>()
                .register_custom_symbol("CombatHealth_vtable", core::ptr::without_provenance(1));
        }

        let strict_err = strict_app
            .try_load_mod_strict(&rust_dylib)
            .expect_err("Unregistered CombatHealth component must fail strict validation");
        match strict_err {
            ModLoadError::MissingComponents { ref missing, .. } => {
                assert!(missing.iter().any(|c| c == "CombatHealth"));
            }
            other => panic!("Expected ModLoadError::MissingComponents, got {other:?}"),
        }
        assert_eq!(strict_app.world().resource::<ModHost>().len(), 0);
    }

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    // Load C99 mod and verify `LoadedModInfo` ECS entity + `ModLoaded` / `ModUnloaded` messages & observers!
    let loaded_obs = Arc::new(Mutex::new(Vec::new()));
    let unloaded_obs = Arc::new(Mutex::new(Vec::new()));
    let l_clone = Arc::clone(&loaded_obs);
    let u_clone = Arc::clone(&unloaded_obs);

    app.world_mut().add_observer(move |trigger: On<ModLoaded>| {
        l_clone.lock().unwrap().push(trigger.event().clone());
    });
    app.world_mut()
        .add_observer(move |trigger: On<ModUnloaded>| {
            u_clone.lock().unwrap().push(trigger.event().clone());
        });

    let mod_id = app.try_load_mod(&c_dylib).expect("C99 mod should load");

    {
        let world = app.world_mut();
        let mut q = world.query::<&LoadedModInfo>();
        let infos: Vec<&LoadedModInfo> = q.iter(world).collect();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].id, mod_id);
        assert!(infos[0].system_count >= 1);
    }
    assert_eq!(loaded_obs.lock().unwrap().len(), 1);
    assert_eq!(app.world().resource::<Messages<ModLoaded>>().len(), 1);

    app.update();

    app.try_unload_mod(mod_id).expect("C99 mod should unload");
    {
        let world = app.world_mut();
        let mut q = world.query::<&LoadedModInfo>();
        assert_eq!(q.iter(world).count(), 0);
    }
    let u_events = unloaded_obs.lock().unwrap();
    assert_eq!(u_events.len(), 1);
    assert_eq!(u_events[0].id, mod_id);
    assert!(u_events[0].removed_systems >= 1);
    assert_eq!(app.world().resource::<Messages<ModUnloaded>>().len(), 1);
}

#[test]
fn truncated_descriptors_and_null_callbacks_rejected_without_ub() {
    let mut init_ctx = ModInitContext::new(Arc::new(ModRegistry::new()));
    let _guard = init_ctx.make_active();
    let ctx_ptr = (&mut init_ctx as *mut ModInitContext).cast::<c_void>();

    // 1. Genuinely 4-byte stack allocation (`struct_size == 4`)
    let tiny_4byte_header: u32 = 4;
    let tiny_ptr = &tiny_4byte_header as *const u32;

    // SAFETY: `read_descriptor` reads only the 4-byte `struct_size` header when `< MIN_V1_SIZE`.
    assert_eq!(
        unsafe { host_register_system(ctx_ptr, tiny_ptr.cast::<SystemDescriptor>()) },
        FfiResult::INVALID_VERSION
    );
    // SAFETY: Same 4-byte header safety invariant.
    assert_eq!(
        unsafe {
            host_register_scheduled_system(ctx_ptr, tiny_ptr.cast::<ScheduledSystemDescriptor>())
        },
        FfiResult::INVALID_VERSION
    );
    // SAFETY: Same 4-byte header safety invariant.
    assert_eq!(
        unsafe {
            host_register_multi_query_system(ctx_ptr, tiny_ptr.cast::<MultiQuerySystemDescriptor>())
        },
        FfiResult::INVALID_VERSION
    );
    // SAFETY: Same 4-byte header safety invariant.
    assert_eq!(
        unsafe { host_register_component(ctx_ptr, tiny_ptr.cast::<ModComponentDescriptor>()) },
        FfiResult::INVALID_VERSION
    );

    // 2. Zero-initialized C descriptor buffers with `callback == NULL` (`0`)
    #[repr(align(8))]
    struct AlignedBuf([u8; 128]);

    let mut zeroed_sys = AlignedBuf([0u8; 128]);
    let sys_sz = (core::mem::size_of::<SystemDescriptor>() as u32).to_ne_bytes();
    zeroed_sys.0[..4].copy_from_slice(&sys_sz);
    // SAFETY: Aligned, initialized byte buffer of size >= SystemDescriptor with null callback.
    assert_eq!(
        unsafe { host_register_system(ctx_ptr, zeroed_sys.0.as_ptr().cast::<SystemDescriptor>()) },
        FfiResult::NULL_POINTER
    );

    let mut zeroed_sched = AlignedBuf([0u8; 128]);
    let sched_sz = (core::mem::size_of::<ScheduledSystemDescriptor>() as u32).to_ne_bytes();
    zeroed_sched.0[..4].copy_from_slice(&sched_sz);
    // SAFETY: Aligned, initialized byte buffer of size >= ScheduledSystemDescriptor with null callback.
    assert_eq!(
        unsafe {
            host_register_scheduled_system(
                ctx_ptr,
                zeroed_sched.0.as_ptr().cast::<ScheduledSystemDescriptor>(),
            )
        },
        FfiResult::NULL_POINTER
    );

    let mut zeroed_multi = AlignedBuf([0u8; 128]);
    let multi_sz =
        (core::mem::size_of::<ScheduledMultiQuerySystemDescriptor>() as u32).to_ne_bytes();
    zeroed_multi.0[..4].copy_from_slice(&multi_sz);
    // SAFETY: Aligned, initialized byte buffer of size >= MultiQuerySystemDescriptor with null callback.
    assert_eq!(
        unsafe {
            host_register_multi_query_system(
                ctx_ptr,
                zeroed_multi.0.as_ptr().cast::<MultiQuerySystemDescriptor>(),
            )
        },
        FfiResult::NULL_POINTER
    );
}

#[test]
fn test_invalid_lifecycle_operations() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let err_unload = app.unload_mod(ModId(99999));
    assert_eq!(err_unload, Err(FfiResult::NOT_FOUND));

    let err_unload_zero = app.unload_mod(ModId(0));
    assert_eq!(err_unload_zero, Err(FfiResult::NOT_FOUND));

    let err_load = app.load_mod_tracked("non_existent_library_path.dylib");
    assert_eq!(err_load, Err(FfiResult::INTERNAL_ERROR));

    app.unload_all_mods();
    assert!(app.world().resource::<ModHost>().is_empty());
}
