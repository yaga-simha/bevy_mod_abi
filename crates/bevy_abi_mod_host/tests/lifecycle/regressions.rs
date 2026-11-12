//! Wave 1 lifecycle regressions (`S-05`, `S-06`, `S-07`).

use crate::support::fixture_dylib;
use bevy_abi_mod_host::{AbiModPlugin, ModAppExt, ModHost, ModLoadError, ModRegistry};
use bevy_abi_mod_host_sys::{
    CurrentRegistryGuard, HOST_API, MOD_INIT_CONTEXT_MAGIC, ModInitContext, ModInitContextHeader,
    host_get_proc_address, host_register_component, host_register_conditional_multi_query_system,
    host_register_conditional_system, host_register_multi_query_system,
    host_register_scheduled_system, host_register_system, host_register_system_in_schedule,
};
use bevy_abi_mod_sys::{
    ComponentUuid, ConditionalMultiQuerySystemDescriptor, ConditionalSystemDescriptor,
    FFI_BOOL_TRUE, FfiBool, FfiResult, ModComponentDescriptor, MultiQuerySystemDescriptor,
    QuerySpec, RawChunkIter, RawCommandQueue, ScheduleLabelId, ScheduledMultiQuerySystemDescriptor,
    ScheduledSystemDescriptor, SystemDescriptor,
};
use bevy_app::App;
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::sync::atomic::AtomicBool;
use std::sync::Arc;

unsafe extern "C" fn noop_single_cb(
    _iter: *mut RawChunkIter,
    _cmds: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn noop_multi_cb(
    _iters: *const *mut RawChunkIter,
    _count: usize,
    _cmds: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn always_true_cond(_user_data: *mut c_void) -> FfiBool {
    FFI_BOOL_TRUE
}

// ============================================================================
// S-05: Clear guest VTables & symbols on unload without global symbol leakage
// ============================================================================

#[test]
fn regression_s05_stale_vtable_after_unload() {
    let dylib_path = fixture_dylib("s05_vtable_mod");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let mod_id = app
        .try_load_mod(&dylib_path)
        .expect("load s05_vtable_mod succeeds");

    // Register an extra mod-owned symbol to verify symbol eviction as well.
    let sentinel_word: u64 = 0xABCD;
    unsafe {
        app.world_mut()
            .resource_mut::<ModRegistry>()
            .register_mod_symbol(
                "s05_custom_fn",
                (&sentinel_word as *const u64).cast::<c_void>(),
                mod_id,
            );
    }

    let comp_id_before = {
        let reg = app.world().resource::<ModRegistry>();
        let comp = reg
            .get_by_name("S05GuestComp")
            .expect("S05GuestComp registered");
        assert!(!comp.vtable_ptr.is_null());
        assert!(comp.drop_fn.is_some());
        assert_eq!(comp.owner, Some(mod_id));
        assert!(reg.resolve_symbol("S05GuestComp_vtable").is_some());
        assert!(reg.resolve_symbol("s05_custom_fn").is_some());

        // Without an active `CURRENT_REGISTRY` guard, `host_get_proc_address` queries `GLOBAL_SYMBOLS`,
        // which MUST NOT contain guest-owned VTable pointers!
        let leaked_global = unsafe { host_get_proc_address(c"S05GuestComp_vtable".as_ptr()) };
        assert!(
            leaked_global.is_null(),
            "Guest VTable pointer must never be inserted into process-global GLOBAL_SYMBOLS"
        );

        // With `CURRENT_REGISTRY` active, resolution succeeds while loaded.
        let reg_arc = Arc::new(reg.clone());
        let _guard: CurrentRegistryGuard = reg_arc.make_current();
        let active_sym = unsafe { host_get_proc_address(c"S05GuestComp_vtable".as_ptr()) };
        assert!(!active_sym.is_null());

        comp.component_id
    };

    // Unload the mod (`dlclose`).
    app.try_unload_mod(mod_id)
        .expect("unload s05_vtable_mod succeeds");

    {
        let reg = app.world().resource::<ModRegistry>();
        let comp = reg
            .get_by_name("S05GuestComp")
            .expect("ComponentId binding preserved across unload for hot-reload");
        assert_eq!(comp.component_id, comp_id_before);
        assert!(
            comp.vtable_ptr.is_null(),
            "vtable_ptr must be nulled on unload"
        );
        assert!(comp.drop_fn.is_none(), "drop_fn must be cleared on unload");
        assert!(comp.owner.is_none(), "owner must be cleared on unload");
        assert!(
            reg.resolve_symbol("S05GuestComp_vtable").is_none(),
            "guest VTable symbol must be evicted on unload"
        );
        assert!(
            reg.resolve_symbol("s05_custom_fn").is_none(),
            "mod-owned custom symbol must be evicted on unload"
        );
    }
}

// ============================================================================
// S-06: Failed init / strict validation rolls back before dlclose
// ============================================================================

#[test]
fn regression_s06_init_failure_rolls_back() {
    let dylib_fail = fixture_dylib("s06_fail_init");
    let dylib_strict = fixture_dylib("s06_strict_fail");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let err = app
        .try_load_mod(&dylib_fail)
        .expect_err("init returning INVALID_ARGUMENT must fail");
    assert!(matches!(
        err,
        ModLoadError::InitFailed {
            status: FfiResult::INVALID_ARGUMENT,
            ..
        }
    ));

    let reg = app.world().resource::<ModRegistry>();
    assert!(
        reg.get_by_name("S06RollbackComp").is_none(),
        "LoadTransaction must roll back newly registered dynamic component before dlclose"
    );
    assert!(reg.resolve_symbol("S06RollbackComp_vtable").is_none());
    assert!(app.world().resource::<ModHost>().is_empty());

    // Verify `try_load_mod_strict` rolls back registered dynamic components and drop hooks
    // when a system references an unregistered component.
    let strict_err = app
        .try_load_mod_strict(&dylib_strict)
        .expect_err("strict load must reject missing component");
    assert!(matches!(strict_err, ModLoadError::MissingComponents { .. }));

    let reg_after = app.world().resource::<ModRegistry>();
    assert!(reg_after.get_by_name("S06StrictTempComp").is_none());
    assert!(
        reg_after
            .resolve_symbol("S06StrictTempComp_vtable")
            .is_none()
    );
    assert!(app.world().resource::<ModHost>().is_empty());
}

// ============================================================================
// S-07: ModInitContext magic + generation + active-pointer validation
// ============================================================================

#[test]
fn regression_s07_register_outside_init_rejected() {
    let mut world = World::new();
    let reg = Arc::new(ModRegistry::new());
    let active = Arc::new(AtomicBool::new(true));

    let comp_desc =
        ModComponentDescriptor::new(c"S07Dyn".as_ptr(), ComponentUuid::from_name("S07Dyn"), 4, 4);
    let sys_desc = SystemDescriptor::new(
        c"s07_sys".as_ptr(),
        core::ptr::null(),
        0,
        noop_single_cb,
        core::ptr::null_mut(),
    );
    let sched_desc = ScheduledSystemDescriptor::from_base(sys_desc, ScheduleLabelId::UPDATE);
    let cond_desc = ConditionalSystemDescriptor::from_scheduled(
        sched_desc,
        Some(always_true_cond),
        core::ptr::null_mut(),
    );
    let specs = [QuerySpec {
        queries: core::ptr::null(),
        query_count: 0,
    }];
    let mq_desc = MultiQuerySystemDescriptor::new(
        c"s07_mq".as_ptr(),
        specs.as_ptr(),
        1,
        noop_multi_cb,
        core::ptr::null_mut(),
    );
    let cond_mq_desc = ConditionalMultiQuerySystemDescriptor::from_scheduled(
        ScheduledMultiQuerySystemDescriptor::from_base(mq_desc, ScheduleLabelId::UPDATE),
        Some(always_true_cond),
        core::ptr::null_mut(),
    );

    let assert_all_entry_points_return = |ctx_ptr: *mut c_void, expected: FfiResult| unsafe {
        assert_eq!((HOST_API.register_component)(ctx_ptr, &comp_desc), expected);
        assert_eq!((HOST_API.register_system)(ctx_ptr, &sys_desc), expected);
        assert_eq!(host_register_component(ctx_ptr, &comp_desc), expected);
        assert_eq!(host_register_system(ctx_ptr, &sys_desc), expected);
        assert_eq!(
            host_register_system_in_schedule(ctx_ptr, &sys_desc, ScheduleLabelId::UPDATE.0),
            expected
        );
        assert_eq!(
            host_register_scheduled_system(ctx_ptr, &sched_desc),
            expected
        );
        assert_eq!(
            host_register_conditional_system(ctx_ptr, &cond_desc),
            expected
        );
        assert_eq!(
            host_register_multi_query_system(ctx_ptr, &mq_desc),
            expected
        );
        assert_eq!(
            host_register_conditional_multi_query_system(ctx_ptr, &cond_mq_desc),
            expected
        );
    };

    // 1. Null pointer -> NULL_POINTER
    assert_all_entry_points_return(core::ptr::null_mut(), FfiResult::NULL_POINTER);

    // 2. Unactivated ModInitContext -> INVALID_STATE
    let mut unactivated =
        ModInitContext::with_world(Arc::clone(&reg), Arc::clone(&active), &mut world);
    let unactivated_ptr = (&mut unactivated as *mut ModInitContext).cast::<c_void>();
    assert_all_entry_points_return(unactivated_ptr, FfiResult::INVALID_STATE);

    // 3. Stale pointer stashed across init and used after guard drops -> INVALID_STATE
    {
        let mut ctx = ModInitContext::with_world(Arc::clone(&reg), Arc::clone(&active), &mut world);
        let ptr = (&mut ctx as *mut ModInitContext).cast::<c_void>();
        {
            let _guard = ctx.make_active();
            assert_eq!(
                unsafe { host_register_system(ptr, &sys_desc) },
                FfiResult::OK
            );
        }
        assert_all_entry_points_return(ptr, FfiResult::INVALID_STATE);
    }

    // 4. Forged header / different context pointer while another init is active -> INVALID_STATE
    let mut real_ctx =
        ModInitContext::with_world(Arc::clone(&reg), Arc::clone(&active), &mut world);
    let _active_guard = real_ctx.make_active();

    #[repr(C)]
    struct ForgedHeader {
        magic: u64,
        generation: u32,
        _pad: u32,
    }
    assert_eq!(
        core::mem::size_of::<ForgedHeader>(),
        core::mem::size_of::<ModInitContextHeader>()
    );
    let mut forged = ForgedHeader {
        magic: MOD_INIT_CONTEXT_MAGIC,
        generation: 999_999,
        _pad: 0,
    };
    let forged_ptr = (&mut forged as *mut ForgedHeader).cast::<c_void>();
    assert_all_entry_points_return(forged_ptr, FfiResult::INVALID_STATE);
}
