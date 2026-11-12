//! Wave 1 scheduling & initialization regressions (`L-01`, `L-05a/b/c`, and `B-04`).

use crate::support::fixture_dylib;
use bevy_abi_mod_host::{
    AbiModPlugin, ModAppExt, ModErrorPolicy, ModNamedSystemSet, ModRegistry, ModSystemFaulted,
    StartupCompleted, SystemInitError,
};
use bevy_abi_mod_host_sys::{
    ComponentAccessDescOwned, DynamicCSystem, MAX_QUERY_COMPONENTS, MAX_SYSTEM_QUERIES,
    ModInitContext, host_register_multi_query_system, host_register_system,
};
use bevy_abi_mod_macros::ModExposable;
use bevy_abi_mod_sys::{
    ComponentAccessDesc, ComponentAccessMode, FfiResult, MultiQuerySystemDescriptor, QuerySpec,
    RawChunkIter, RawCommandQueue, SystemDescriptor,
};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::message::Messages;
use bevy_ecs::resource::Resource;
use bevy_ecs::schedule::{ScheduleLabel, Schedules};
use bevy_ecs::system::{System, SystemAccess};
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::sync::atomic::AtomicBool;
use std::ffi::CString;
use std::sync::Arc;

#[repr(C)]
#[derive(Component, ModExposable, Copy, Clone, Debug, PartialEq, Eq)]
struct PaddedPair {
    tag: u8,
    value: u64,
}

#[repr(C)]
#[derive(Resource, ModExposable, Copy, Clone, Debug, Default, PartialEq, Eq)]
struct CounterRes {
    startup_hits: u32,
    update_hits: u32,
}

#[repr(C)]
#[derive(Resource, ModExposable, Copy, Clone, Debug, Default, PartialEq, Eq)]
struct OrderTraceRes {
    steps: [u32; 4],
    len: u32,
}

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

// ============================================================================
// L-01: Runtime-loaded Startup systems execute immediately once
// ============================================================================

#[test]
fn regression_l01_runtime_loaded_startup_runs_once() {
    let dylib_path = fixture_dylib("l01_late_startup");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_resource::<CounterRes>();
    app.insert_resource(CounterRes::default());

    // Advance the app once so `Startup` and `PostStartup` have already executed.
    app.update();
    assert_eq!(
        app.world().resource::<StartupCompleted>(),
        &StartupCompleted(true)
    );
    assert_eq!(
        *app.world().resource::<CounterRes>(),
        CounterRes {
            startup_hits: 0,
            update_hits: 0,
        }
    );

    // Load the mod at runtime after `Startup` has completed.
    let mod_id = app
        .try_load_mod(&dylib_path)
        .expect("runtime load after Startup succeeds");

    // `late_startup_sys` ran immediately once during `try_load_mod`.
    assert_eq!(
        *app.world().resource::<CounterRes>(),
        CounterRes {
            startup_hits: 1,
            update_hits: 0,
        }
    );

    // Subsequent updates run `regular_update_sys` each tick, never re-running `late_startup_sys`.
    for expected_updates in 1..=3 {
        app.update();
        assert_eq!(
            *app.world().resource::<CounterRes>(),
            CounterRes {
                startup_hits: 1,
                update_hits: expected_updates,
            }
        );
    }

    app.try_unload_mod(mod_id).expect("unload succeeds");
}

// ============================================================================
// L-05a: Cross-mod system name isolation
// ============================================================================

#[test]
fn regression_l05a_cross_mod_name_isolation() {
    let path_a = fixture_dylib("mod_alpha");
    let path_b = fixture_dylib("mod_beta");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_resource::<OrderTraceRes>();
    app.insert_resource(OrderTraceRes::default());

    let id_a = app.try_load_mod(&path_a).expect("load mod_alpha");
    let id_b = app.try_load_mod(&path_b).expect("load mod_beta");

    let set_a_step = ModNamedSystemSet::new("mod_alpha", "step");
    let set_b_step = ModNamedSystemSet::new("mod_beta", "step");
    {
        let schedules = app.world().resource::<Schedules>();
        let update_sched = schedules.get(Update.intern()).expect("Update schedule");
        assert!(set_a_step.is_present_in(update_sched));
        assert!(set_b_step.is_present_in(update_sched));
    }

    app.update();
    let trace = *app.world().resource::<OrderTraceRes>();
    assert_eq!(trace.len, 4);
    let pos_10 = trace.steps.iter().position(|&x| x == 10).unwrap();
    let pos_11 = trace.steps.iter().position(|&x| x == 11).unwrap();
    let pos_20 = trace.steps.iter().position(|&x| x == 20).unwrap();
    let pos_21 = trace.steps.iter().position(|&x| x == 21).unwrap();
    assert!(
        pos_10 < pos_11,
        "mod_alpha step must precede mod_alpha finish"
    );
    assert!(
        pos_20 < pos_21,
        "mod_beta step must precede mod_beta finish"
    );

    // Unloading `mod_beta` must NOT remove `mod_alpha`'s `"step"` set!
    app.try_unload_mod(id_b).expect("unload mod_beta");
    {
        let schedules = app.world().resource::<Schedules>();
        let update_sched = schedules.get(Update.intern()).expect("Update schedule");
        assert!(set_a_step.is_present_in(update_sched));
        assert!(!set_b_step.is_present_in(update_sched));
    }

    app.insert_resource(OrderTraceRes::default());
    app.update();
    let trace_after = *app.world().resource::<OrderTraceRes>();
    assert_eq!(trace_after.len, 2);
    assert_eq!(&trace_after.steps[..2], &[10, 11]);

    app.try_unload_mod(id_a).expect("unload mod_alpha");
}

// ============================================================================
// L-05b: Later mod ordering `after("earlier_mod::produce")` works when earlier
//        mod registered zero ordering constraints of its own
// ============================================================================

#[test]
fn regression_l05b_later_mod_after_earlier_system() {
    let path_p = fixture_dylib("mod_producer");
    let path_c = fixture_dylib("mod_consumer");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_resource::<OrderTraceRes>();
    app.insert_resource(OrderTraceRes::default());

    let id_p = app.try_load_mod(&path_p).expect("load mod_producer");
    let id_c = app.try_load_mod(&path_c).expect("load mod_consumer");

    for _ in 0..5 {
        app.insert_resource(OrderTraceRes::default());
        app.update();
        let trace = *app.world().resource::<OrderTraceRes>();
        assert_eq!(trace.len, 2);
        assert_eq!(
            &trace.steps[..2],
            &[1, 2],
            "mod_producer::produce must always run before mod_consumer::consume"
        );
    }

    app.try_unload_mod(id_c).expect("unload mod_consumer");
    app.try_unload_mod(id_p).expect("unload mod_producer");
}

// ============================================================================
// L-05c: Named sets are removed from Bevy's ScheduleGraph on unload
// ============================================================================

#[test]
fn regression_l05c_named_sets_removed_on_unload() {
    let dylib_path = fixture_dylib("l05c_clean_mod");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let mod_id = app.try_load_mod(&dylib_path).expect("load l05c_clean_mod");
    app.update();

    let set1 = ModNamedSystemSet::new("l05c_clean_mod", "stage_one");
    let set2 = ModNamedSystemSet::new("l05c_clean_mod", "stage_two");
    {
        let schedules = app.world().resource::<Schedules>();
        let update = schedules.get(Update.intern()).expect("Update schedule");
        assert!(set1.is_present_in(update));
        assert!(set2.is_present_in(update));
    }

    app.try_unload_mod(mod_id).expect("unload l05c_clean_mod");

    {
        let schedules = app.world().resource::<Schedules>();
        let update = schedules.get(Update.intern()).expect("Update schedule");
        assert!(
            !set1.is_present_in(update),
            "stage_one ModNamedSystemSet must be removed on unload"
        );
        assert!(
            !set2.is_present_in(update),
            "stage_two ModNamedSystemSet must be removed on unload"
        );
    }
}

// ============================================================================
// B-04: Explicit SystemInitError variants (synchronous + ModSystemFaulted)
// ============================================================================

#[test]
fn regression_b04_too_many_terms() {
    let mut world = World::new();
    let reg = Arc::new(ModRegistry::new());
    let active = Arc::new(AtomicBool::new(true));
    let mut ctx = ModInitContext::with_world(Arc::clone(&reg), active, &mut world);
    let _guard = ctx.make_active();

    let c_names: Vec<CString> = (0..=MAX_QUERY_COMPONENTS)
        .map(|i| CString::new(format!("C{i}")).unwrap())
        .collect();
    let raw_terms: Vec<ComponentAccessDesc> = c_names
        .iter()
        .map(|s| ComponentAccessDesc::read(s.as_ptr()))
        .collect();
    let desc = SystemDescriptor::new(
        c"too_many_sys".as_ptr(),
        raw_terms.as_ptr(),
        raw_terms.len(),
        noop_single_cb,
        core::ptr::null_mut(),
    );

    // 1. Synchronous rejection at registration time -> OUT_OF_RANGE
    let rc = unsafe { host_register_system((&mut ctx as *mut ModInitContext).cast(), &desc) };
    assert_eq!(rc, FfiResult::OUT_OF_RANGE);

    // 2. Also verify `MAX_SYSTEM_QUERIES + 1` in multi-query -> OUT_OF_RANGE
    let empty_specs = [QuerySpec {
        queries: core::ptr::null(),
        query_count: 0,
    }; MAX_SYSTEM_QUERIES + 1];
    let mq_desc = MultiQuerySystemDescriptor::new(
        c"too_many_mq".as_ptr(),
        empty_specs.as_ptr(),
        empty_specs.len(),
        noop_multi_cb,
        core::ptr::null_mut(),
    );
    let rc_mq = unsafe {
        host_register_multi_query_system((&mut ctx as *mut ModInitContext).cast(), &mq_desc)
    };
    assert_eq!(rc_mq, FfiResult::OUT_OF_RANGE);

    // 3. Direct DynamicCSystem construction records `SystemInitError::TooManyTerms`
    let owned_terms: Vec<ComponentAccessDescOwned> = (0..=MAX_QUERY_COMPONENTS)
        .map(|i| ComponentAccessDescOwned {
            name: format!("C{i}"),
            mode: ComponentAccessMode::READ,
        })
        .collect();
    let mut sys = DynamicCSystem::new(
        "too_many_direct".into(),
        owned_terms,
        noop_single_cb,
        core::ptr::null_mut(),
        reg,
    );
    assert!(matches!(sys.initialize(&mut world), SystemAccess::None));
    assert_eq!(sys.init_error(), Some(&SystemInitError::TooManyTerms));
}

#[test]
fn regression_b04_invalid_mode() {
    let mut world = World::new();
    let reg = Arc::new(ModRegistry::new());
    let active = Arc::new(AtomicBool::new(true));
    let mut ctx = ModInitContext::with_world(Arc::clone(&reg), active, &mut world);
    let _guard = ctx.make_active();

    let raw_terms = [
        ComponentAccessDesc::read(c"PaddedPair".as_ptr()),
        ComponentAccessDesc {
            name: c"CounterRes".as_ptr(),
            mode: ComponentAccessMode(99),
        },
    ];
    let desc = SystemDescriptor::new(
        c"bad_mode_sys".as_ptr(),
        raw_terms.as_ptr(),
        raw_terms.len(),
        noop_single_cb,
        core::ptr::null_mut(),
    );

    // Synchronous rejection at registration time -> INVALID_ARGUMENT
    let rc = unsafe { host_register_system((&mut ctx as *mut ModInitContext).cast(), &desc) };
    assert_eq!(rc, FfiResult::INVALID_ARGUMENT);

    // Direct DynamicCSystem construction records `InvalidMode { index: 1 }`
    let mut sys = DynamicCSystem::new(
        "bad_mode_direct".into(),
        vec![
            ComponentAccessDescOwned {
                name: "PaddedPair".into(),
                mode: ComponentAccessMode::READ,
            },
            ComponentAccessDescOwned {
                name: "CounterRes".into(),
                mode: ComponentAccessMode(99),
            },
        ],
        noop_single_cb,
        core::ptr::null_mut(),
        reg,
    );
    assert!(matches!(sys.initialize(&mut world), SystemAccess::None));
    assert_eq!(
        sys.init_error(),
        Some(&SystemInitError::InvalidMode { index: 1 })
    );
}

#[test]
fn regression_b04_unknown_component() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));

    let reg = Arc::new(app.world().resource::<ModRegistry>().clone());
    let sys = DynamicCSystem::new(
        "unknown_comp_sys".into(),
        vec![ComponentAccessDescOwned {
            name: "GhostComponent".into(),
            mode: ComponentAccessMode::READ,
        }],
        noop_single_cb,
        core::ptr::null_mut(),
        reg,
    );
    app.add_systems(Update, sys);
    app.update();

    let msgs = app.world().resource::<Messages<ModSystemFaulted>>();
    let mut cursor = msgs.get_cursor();
    let events: Vec<&ModSystemFaulted> = cursor.read(msgs).collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].system_name, "unknown_comp_sys");
    assert_eq!(events[0].status, FfiResult::INVALID_COMPONENT);
    assert_eq!(
        events[0].init_error,
        Some(SystemInitError::UnknownComponent("GhostComponent".into()))
    );
}

#[test]
fn regression_b04_kind_mismatch() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));
    app.register_mod_component::<PaddedPair>();
    app.register_mod_resource::<CounterRes>();

    let reg = Arc::new(app.world().resource::<ModRegistry>().clone());

    // 1. Entity Component queried with `RESOURCE_READ` -> KindMismatch("PaddedPair")
    let sys_comp_as_res = DynamicCSystem::new(
        "comp_as_res_sys".into(),
        vec![ComponentAccessDescOwned {
            name: "PaddedPair".into(),
            mode: ComponentAccessMode::RESOURCE_READ,
        }],
        noop_single_cb,
        core::ptr::null_mut(),
        Arc::clone(&reg),
    );

    // 2. Singleton Resource queried with entity `READ` -> KindMismatch("CounterRes")
    let sys_res_as_comp = DynamicCSystem::new(
        "res_as_comp_sys".into(),
        vec![ComponentAccessDescOwned {
            name: "CounterRes".into(),
            mode: ComponentAccessMode::READ,
        }],
        noop_single_cb,
        core::ptr::null_mut(),
        reg,
    );

    app.add_systems(Update, (sys_comp_as_res, sys_res_as_comp));
    app.update();

    let msgs = app.world().resource::<Messages<ModSystemFaulted>>();
    let mut cursor = msgs.get_cursor();
    let errors: Vec<Option<SystemInitError>> =
        cursor.read(msgs).map(|e| e.init_error.clone()).collect();
    assert!(errors.contains(&Some(SystemInitError::KindMismatch("PaddedPair".into()))));
    assert!(errors.contains(&Some(SystemInitError::KindMismatch("CounterRes".into()))));
}

#[test]
fn regression_b04_self_conflict() {
    let mut world = World::new();
    let comp_id = world.register_component::<PaddedPair>();
    let mut registry = ModRegistry::new();
    registry.register_component::<PaddedPair>(comp_id);
    let reg = Arc::new(registry);
    let active = Arc::new(AtomicBool::new(true));
    let mut ctx = ModInitContext::with_world(Arc::clone(&reg), active, &mut world);
    let _guard = ctx.make_active();

    // 1. Intra-query WRITE + READ on same component -> synchronous ACCESS_CONFLICT
    let raw_terms = [
        ComponentAccessDesc::write(c"PaddedPair".as_ptr()),
        ComponentAccessDesc::read(c"PaddedPair".as_ptr()),
    ];
    let desc = SystemDescriptor::new(
        c"self_conflict_single".as_ptr(),
        raw_terms.as_ptr(),
        raw_terms.len(),
        noop_single_cb,
        core::ptr::null_mut(),
    );
    let rc = unsafe { host_register_system((&mut ctx as *mut ModInitContext).cast(), &desc) };
    assert_eq!(rc, FfiResult::ACCESS_CONFLICT);

    // 2. Cross-group RESOURCE_WRITE + RESOURCE_READ in multi-query -> synchronous ACCESS_CONFLICT
    let q0 = [ComponentAccessDesc::resource_write(c"CounterRes".as_ptr())];
    let q1 = [ComponentAccessDesc::resource_read(c"CounterRes".as_ptr())];
    let specs = [
        QuerySpec {
            queries: q0.as_ptr(),
            query_count: 1,
        },
        QuerySpec {
            queries: q1.as_ptr(),
            query_count: 1,
        },
    ];
    let mq_desc = MultiQuerySystemDescriptor::new(
        c"self_conflict_mq".as_ptr(),
        specs.as_ptr(),
        specs.len(),
        noop_multi_cb,
        core::ptr::null_mut(),
    );
    let rc_mq = unsafe {
        host_register_multi_query_system((&mut ctx as *mut ModInitContext).cast(), &mq_desc)
    };
    assert_eq!(rc_mq, FfiResult::ACCESS_CONFLICT);

    // 3. Direct DynamicCSystem construction records `SystemInitError::SelfConflict`
    let mut sys = DynamicCSystem::new(
        "self_conflict_direct".into(),
        vec![
            ComponentAccessDescOwned {
                name: "PaddedPair".into(),
                mode: ComponentAccessMode::WRITE,
            },
            ComponentAccessDescOwned {
                name: "PaddedPair".into(),
                mode: ComponentAccessMode::READ,
            },
        ],
        noop_single_cb,
        core::ptr::null_mut(),
        reg,
    );
    assert!(matches!(sys.initialize(&mut world), SystemAccess::None));
    assert_eq!(
        sys.init_error(),
        Some(&SystemInitError::SelfConflict {
            a: "PaddedPair".into(),
            b: "PaddedPair".into(),
        })
    );
}
