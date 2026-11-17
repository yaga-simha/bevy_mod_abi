//! ABI `0.2.0` SemVer handshake compatibility, `LoadOptions` strict validation,
//! single-path lifecycle `Message`s (`B-09`), truncated descriptors, and invalid lifecycle operations.

use crate::DROP_SLOT_POOL_RWLOCK;
use crate::support::{Position, Velocity, ensure_guest_mods_built, fixture_dylib};
use bevy_abi_mod_host::is_abi_version_compatible;
use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::{LoadContext, host_register_component, host_register_system};
use bevy_app::App;
use bevy_ecs::message::Messages;
use std::error::Error as _;

#[test]
fn semver_abi_compatibility_and_typed_mod_load_error_work() {
    const fn pack_ver(maj: u32, min: u32, patch: u32) -> u32 {
        (maj << 24) | (min << 16) | patch
    }

    // Host is `0.2.0` (`ABI_VERSION = 0x0002_0000`): pre-1.0 requires exact minor match (`C-06`).
    assert!(is_abi_version_compatible(ABI_VERSION, ABI_VERSION));
    assert!(is_abi_version_compatible(ABI_VERSION, pack_ver(0, 2, 99)));
    assert!(!is_abi_version_compatible(ABI_VERSION, pack_ver(0, 0, 9)));
    assert!(!is_abi_version_compatible(ABI_VERSION, pack_ver(0, 1, 99)));
    assert!(!is_abi_version_compatible(ABI_VERSION, pack_ver(0, 3, 0)));
    assert!(!is_abi_version_compatible(ABI_VERSION, pack_ver(1, 0, 0)));

    // Post-1.0 (`major >= 1`) allows `guest_minor <= host_minor`.
    assert!(is_abi_version_compatible(
        pack_ver(1, 2, 0),
        pack_ver(1, 1, 5)
    ));
    assert!(!is_abi_version_compatible(
        pack_ver(1, 1, 0),
        pack_ver(1, 2, 0)
    ));

    #[cfg(not(miri))]
    {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let err = app
            .load_mod_with(
                "/nonexistent/path/to/missing_mod.dylib",
                LoadOptions::new().with_shadow_copy(false),
            )
            .expect_err("Missing dylib without shadow-copy must return ModLoadError::DlOpen");
        assert!(matches!(err, ModLoadError::DlOpen { .. }));
        assert!(
            err.source().is_some(),
            "E-02: DlOpen must preserve source()"
        );
        assert!(err.to_string().contains("missing_mod.dylib"));

        let shadow_err = app
            .load_mod_with(
                "/nonexistent/path/to/missing_shadow.dylib",
                LoadOptions::new().with_shadow_copy(true),
            )
            .expect_err("Missing shadow source must return ModLoadError::ShadowCopy");
        assert!(matches!(shadow_err, ModLoadError::ShadowCopy { .. }));
        assert!(
            shadow_err.source().is_some(),
            "E-02: ShadowCopy must preserve io::Error source()"
        );
    }

    let _ = core::mem::size_of::<LoadedModInfo>();
    let _ = core::mem::size_of::<ModLoaded>();
    let _ = core::mem::size_of::<ModUnloaded>();
}

#[test]
fn real_dylib_loaded_mod_entity_lifecycle_messages_and_strict_validation_work() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let (_rust_dylib, c_dylib) = ensure_guest_mods_built();
    let strict_fail_dylib = fixture_dylib("s06_strict_fail");

    // 1. Strict mode validation (`LoadOptions::new().strict()`): rejects unresolved
    // ordering targets and rolls back the module cleanly (`ModHost` remains empty).
    {
        let mut strict_app = App::new();
        strict_app.add_plugins(AbiModPlugin);
        let strict_err = strict_app
            .load_mod_with(
                &strict_fail_dylib,
                LoadOptions::new().strict().with_shadow_copy(true),
            )
            .expect_err("Unresolved ordering target must fail strict validation");
        assert!(matches!(
            strict_err,
            ModLoadError::UnresolvedOrderingTarget { .. }
        ));
        assert!(strict_app.world().resource::<ModHost>().is_empty());
        assert_eq!(
            strict_app.world().resource::<Messages<ModLoaded>>().len(),
            0
        );
    }

    // 2. Load C99 mod and verify `LoadedModInfo` ECS entity (populated from `BamManifest`, `L-08`)
    // and single-path `Messages<ModLoaded>` / `Messages<ModUnloaded>` (`B-09`).
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(ComponentOptions::new().with_default())
        .unwrap();
    app.register_mod_component::<Velocity>(ComponentOptions::new().with_default())
        .unwrap();

    let mod_id = app
        .load_mod_with(&c_dylib, LoadOptions::new().with_shadow_copy(true))
        .expect("C99 mod should load");

    {
        let world = app.world_mut();
        let mut q = world.query::<&LoadedModInfo>();
        let infos: Vec<&LoadedModInfo> = q.iter(world).collect();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].id, mod_id);
        assert_eq!(infos[0].name, "asymmetric_c_mod");
        assert_eq!(infos[0].version, "0.2.0");
        assert!(infos[0].system_count >= 1);
    }
    assert_eq!(app.world().resource::<Messages<ModLoaded>>().len(), 1);

    app.update();

    app.unload_mod(mod_id).expect("C99 mod should unload");
    {
        let world = app.world_mut();
        let mut q = world.query::<&LoadedModInfo>();
        assert_eq!(q.iter(world).count(), 0);
    }
    assert_eq!(app.world().resource::<Messages<ModUnloaded>>().len(), 1);
}

#[test]
fn truncated_descriptors_and_null_callbacks_rejected_without_ub() {
    let mut load_ctx = LoadContext::new(ModRegistry::new());
    let ctx_ptr = load_ctx.as_ffi_ptr();

    // 1. Genuinely 4-byte stack allocation (`struct_size == 4`)
    let tiny_4byte_header: u32 = 4;
    let tiny_ptr = &tiny_4byte_header as *const u32;
    let mut out_comp = BamComponent::INVALID;

    // SAFETY: `read_descriptor` reads only the 4-byte `struct_size` header when `< MIN_V0_2_SIZE`.
    let sys_tiny_res = unsafe { host_register_system(ctx_ptr, tiny_ptr.cast::<BamSystemDesc>()) };
    assert_eq!(sys_tiny_res, FfiResult::INVALID_VERSION);

    // SAFETY: Same 4-byte header safety invariant.
    let comp_tiny_res = unsafe {
        host_register_component(ctx_ptr, tiny_ptr.cast::<ComponentDesc>(), &mut out_comp)
    };
    assert_eq!(comp_tiny_res, FfiResult::INVALID_VERSION);

    // 2. Zero-initialized C `BamSystemDesc` buffer with `callback == None` (`0`)
    #[repr(align(8))]
    struct AlignedBuf([u8; 128]);

    let mut zeroed_sys = AlignedBuf([0u8; 128]);
    let sys_sz = (core::mem::size_of::<BamSystemDesc>() as u32).to_ne_bytes();
    zeroed_sys.0[..4].copy_from_slice(&sys_sz);
    // SAFETY: Aligned, initialized byte buffer of size >= `BamSystemDesc` with null `name`/`callback`.
    let zeroed_sys_res =
        unsafe { host_register_system(ctx_ptr, zeroed_sys.0.as_ptr().cast::<BamSystemDesc>()) };
    assert_eq!(zeroed_sys_res, FfiResult::NULL_POINTER);

    load_ctx.finish_init();
}

#[test]
fn test_invalid_lifecycle_operations() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let err_unload = app.unload_mod(ModId(99_999));
    assert!(matches!(
        err_unload,
        Err(ModLoadError::ModNotFound { id: ModId(99_999) })
    ));

    let err_unload_zero = app.unload_mod(ModId(0));
    assert!(matches!(
        err_unload_zero,
        Err(ModLoadError::ModNotFound { id: ModId(0) })
    ));

    let err_load = app.load_mod_with(
        "non_existent_library_path.dylib",
        LoadOptions::new().with_shadow_copy(false),
    );
    assert!(matches!(err_load, Err(ModLoadError::DlOpen { .. })));

    app.unload_all_mods();
    assert!(app.world().resource::<ModHost>().is_empty());
}
