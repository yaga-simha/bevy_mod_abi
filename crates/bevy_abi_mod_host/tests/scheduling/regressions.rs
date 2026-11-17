//! Wave 1–3 scheduling, ordering, schedule-map, main-thread, and initialization regressions
//! (`L-01`, `L-05a/b/c`, `B-04`, `B-05`, `B-06`, `B-07`, `B-08`, `B-09`).

use crate::support::fixture_dylib;
use bevy_abi_mod_host::{
    AbiModPlugin, ComponentOptions, LoadOptions, ModAppExt, ModErrorPolicy, ModLoadError,
    ModNamedSystemSet, ModQuarantine, ModRegistry, ModSystemFaulted, StartupCompleted,
    SystemInitError,
};
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, GuestSystem, LoadContext, MAX_SYSTEM_QUERIES, host_register_system,
};
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::{
    AccessMode, BAM_SYSTEM_FLAG_MAIN_THREAD, BamComponent, BamQueryDesc, BamSystemCtx,
    BamSystemDesc, BamTermDesc, FfiResult, MAX_TERMS_PER_QUERY, ScheduleId,
};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::message::Messages;
use bevy_ecs::resource::Resource;
use bevy_ecs::schedule::{MultiThreadedExecutor, Schedule, ScheduleLabel, Schedules};
use bevy_ecs::system::{System, SystemAccess, SystemStateFlags};
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[repr(C)]
#[derive(Component, ModComponent, Copy, Clone, Debug, PartialEq, Eq)]
#[mod_component(name = "sched_reg::PaddedPair")]
struct PaddedPair {
    tag: u8,
    value: u64,
}

#[repr(C)]
#[derive(Resource, ModComponent, Copy, Clone, Debug, Default, PartialEq, Eq)]
#[mod_component(name = "sched_reg::CounterRes")]
struct CounterRes {
    startup_hits: u32,
    update_hits: u32,
}

#[repr(C)]
#[derive(Resource, ModComponent, Copy, Clone, Debug, Default, PartialEq, Eq)]
#[mod_component(name = "sched_reg::OrderTraceRes")]
struct OrderTraceRes {
    steps: [u32; 4],
    len: u32,
}

unsafe extern "C" fn noop_cb(_sys_ctx: *mut BamSystemCtx, _user_data: *mut c_void) -> FfiResult {
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
    app.register_mod_resource::<CounterRes>()
        .expect("register CounterRes");
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

    // Load the mod at runtime after `Startup` has completed (with shadow copy for parallel test safety).
    let mod_id = app
        .load_mod_with(&dylib_path, LoadOptions::new().with_shadow_copy(true))
        .expect("runtime load after Startup succeeds");

    // `late_startup_sys` ran immediately once during `load_mod_with`.
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

    app.unload_mod(mod_id).expect("unload succeeds");
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
    app.register_mod_resource::<OrderTraceRes>()
        .expect("register OrderTraceRes");
    app.insert_resource(OrderTraceRes::default());

    let id_a = app
        .load_mod_with(&path_a, LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_alpha");
    let id_b = app
        .load_mod_with(&path_b, LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_beta");

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
    app.unload_mod(id_b).expect("unload mod_beta");
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

    app.unload_mod(id_a).expect("unload mod_alpha");
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
    app.register_mod_resource::<OrderTraceRes>()
        .expect("register OrderTraceRes");
    app.insert_resource(OrderTraceRes::default());

    let id_p = app
        .load_mod_with(&path_p, LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_producer");
    let id_c = app
        .load_mod_with(&path_c, LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_consumer");

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

    app.unload_mod(id_c).expect("unload mod_consumer");
    app.unload_mod(id_p).expect("unload mod_producer");
}

// ============================================================================
// L-05c: Named sets are removed from Bevy's ScheduleGraph on unload
// ============================================================================

#[test]
fn regression_l05c_named_sets_removed_on_unload() {
    let dylib_path = fixture_dylib("l05c_clean_mod");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let mod_id = app
        .load_mod_with(&dylib_path, LoadOptions::new().with_shadow_copy(true))
        .expect("load l05c_clean_mod");
    app.update();

    let set1 = ModNamedSystemSet::new("l05c_clean_mod", "stage_one");
    let set2 = ModNamedSystemSet::new("l05c_clean_mod", "stage_two");
    {
        let schedules = app.world().resource::<Schedules>();
        let update = schedules.get(Update.intern()).expect("Update schedule");
        assert!(set1.is_present_in(update));
        assert!(set2.is_present_in(update));
    }

    app.unload_mod(mod_id).expect("unload l05c_clean_mod");

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
    let reg = ModRegistry::new();
    let active = Arc::new(AtomicBool::new(true));
    let mut ctx = LoadContext::with_world_and_mod_id(reg.clone(), active, &mut world, None);

    let raw_terms: Vec<BamTermDesc> = (0..=MAX_TERMS_PER_QUERY)
        .map(|i| BamTermDesc::read(BamComponent::from_raw((i + 1) as u32)))
        .collect();
    let q_desc = BamQueryDesc::new(raw_terms.as_ptr(), raw_terms.len());
    let desc = BamSystemDesc::new(
        c"too_many_sys".as_ptr(),
        &q_desc,
        1,
        noop_cb,
        core::ptr::null_mut(),
    );

    // 1. Synchronous rejection at registration time -> CAPACITY_EXCEEDED (`C-07`)
    let rc = unsafe { host_register_system(ctx.as_ffi_ptr(), &desc) };
    assert_eq!(rc, FfiResult::CAPACITY_EXCEEDED);

    // 2. Also verify `MAX_SYSTEM_QUERIES + 1` in unified `BamSystemDesc` -> CAPACITY_EXCEEDED
    let empty_specs = [BamQueryDesc::empty(); MAX_SYSTEM_QUERIES + 1];
    let mq_desc = BamSystemDesc::new(
        c"too_many_mq".as_ptr(),
        empty_specs.as_ptr(),
        empty_specs.len(),
        noop_cb,
        core::ptr::null_mut(),
    );
    let rc_mq = unsafe { host_register_system(ctx.as_ffi_ptr(), &mq_desc) };
    assert_eq!(rc_mq, FfiResult::CAPACITY_EXCEEDED);
    ctx.finish_init();

    // 3. Direct GuestSystem construction records `SystemInitError::TooManyTerms`
    let mut sys = GuestSystem::new(
        "too_many_direct",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: raw_terms,
        }],
        noop_cb,
        core::ptr::null_mut(),
        reg,
    );
    assert!(matches!(sys.initialize(&mut world), SystemAccess::None));
    assert_eq!(sys.init_error(), Some(&SystemInitError::TooManyTerms));
}

#[test]
fn regression_b04_invalid_mode() {
    let mut world = World::new();
    let pair_id = world.register_component::<PaddedPair>();
    let res_id = world.register_component::<CounterRes>();
    let mut reg = ModRegistry::new();
    let pair_h = reg
        .register_component::<PaddedPair>(pair_id, ComponentOptions::new())
        .unwrap();
    let res_h = reg.register_resource::<CounterRes>(res_id).unwrap();

    let active = Arc::new(AtomicBool::new(true));
    let mut ctx = LoadContext::with_world_and_mod_id(reg.clone(), active, &mut world, None);

    let raw_terms = [
        BamTermDesc::read(pair_h),
        BamTermDesc::new(res_h, AccessMode(99)),
    ];
    let q_desc = BamQueryDesc::new(raw_terms.as_ptr(), raw_terms.len());
    let desc = BamSystemDesc::new(
        c"bad_mode_sys".as_ptr(),
        &q_desc,
        1,
        noop_cb,
        core::ptr::null_mut(),
    );

    // Synchronous rejection at registration time -> INVALID_ARGUMENT
    let rc = unsafe { host_register_system(ctx.as_ffi_ptr(), &desc) };
    assert_eq!(rc, FfiResult::INVALID_ARGUMENT);
    ctx.finish_init();

    // Direct GuestSystem construction records `InvalidMode { index: 1 }`
    let mut sys = GuestSystem::new(
        "bad_mode_direct",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: raw_terms.to_vec(),
        }],
        noop_cb,
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

    let reg = app.world().resource::<ModRegistry>().clone();
    let ghost_handle = BamComponent::from_raw(999);
    let sys = GuestSystem::new(
        "unknown_comp_sys",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::read(ghost_handle)],
        }],
        noop_cb,
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
        Some(SystemInitError::UnknownComponent(
            "BamComponent(999)".into()
        ))
    );
}

#[test]
fn regression_b04_kind_mismatch() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));
    let pair_h = app
        .register_mod_component::<PaddedPair>(ComponentOptions::new())
        .unwrap();
    let res_h = app.register_mod_resource::<CounterRes>().unwrap();

    let reg = app.world().resource::<ModRegistry>().clone();

    // 1. Entity Component queried with `RESOURCE_READ` -> KindMismatch("sched_reg::PaddedPair")
    let sys_comp_as_res = GuestSystem::new(
        "comp_as_res_sys",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::resource_read(pair_h)],
        }],
        noop_cb,
        core::ptr::null_mut(),
        reg.clone(),
    );

    // 2. Singleton Resource queried with entity `READ` -> KindMismatch("sched_reg::CounterRes")
    let sys_res_as_comp = GuestSystem::new(
        "res_as_comp_sys",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::read(res_h)],
        }],
        noop_cb,
        core::ptr::null_mut(),
        reg,
    );

    app.add_systems(Update, (sys_comp_as_res, sys_res_as_comp));
    app.update();

    let msgs = app.world().resource::<Messages<ModSystemFaulted>>();
    let mut cursor = msgs.get_cursor();
    let errors: Vec<Option<SystemInitError>> =
        cursor.read(msgs).map(|e| e.init_error.clone()).collect();
    assert!(errors.contains(&Some(SystemInitError::KindMismatch(
        "sched_reg::PaddedPair".into()
    ))));
    assert!(errors.contains(&Some(SystemInitError::KindMismatch(
        "sched_reg::CounterRes".into()
    ))));
}

#[test]
fn regression_b04_self_conflict() {
    let mut world = World::new();
    let comp_id = world.register_component::<PaddedPair>();
    let res_id = world.register_component::<CounterRes>();
    let mut reg = ModRegistry::new();
    let pair_h = reg
        .register_component::<PaddedPair>(comp_id, ComponentOptions::new())
        .unwrap();
    let res_h = reg.register_resource::<CounterRes>(res_id).unwrap();

    let active = Arc::new(AtomicBool::new(true));
    let mut ctx = LoadContext::with_world_and_mod_id(reg.clone(), active, &mut world, None);

    // 1. Intra-query WRITE + READ on same component -> synchronous ACCESS_CONFLICT
    let raw_terms = [BamTermDesc::write(pair_h), BamTermDesc::read(pair_h)];
    let q_single = BamQueryDesc::new(raw_terms.as_ptr(), raw_terms.len());
    let desc = BamSystemDesc::new(
        c"self_conflict_single".as_ptr(),
        &q_single,
        1,
        noop_cb,
        core::ptr::null_mut(),
    );
    let rc = unsafe { host_register_system(ctx.as_ffi_ptr(), &desc) };
    assert_eq!(rc, FfiResult::ACCESS_CONFLICT);

    // 2. Cross-group RESOURCE_WRITE + RESOURCE_READ in multi-query -> synchronous ACCESS_CONFLICT
    let q0_terms = [BamTermDesc::resource_write(res_h)];
    let q1_terms = [BamTermDesc::resource_read(res_h)];
    let specs = [
        BamQueryDesc::new(q0_terms.as_ptr(), q0_terms.len()),
        BamQueryDesc::new(q1_terms.as_ptr(), q1_terms.len()),
    ];
    let mq_desc = BamSystemDesc::new(
        c"self_conflict_mq".as_ptr(),
        specs.as_ptr(),
        specs.len(),
        noop_cb,
        core::ptr::null_mut(),
    );
    let rc_mq = unsafe { host_register_system(ctx.as_ffi_ptr(), &mq_desc) };
    assert_eq!(rc_mq, FfiResult::ACCESS_CONFLICT);
    ctx.finish_init();

    // 3. Direct GuestSystem construction records `SystemInitError::SelfConflict`
    let mut sys = GuestSystem::new(
        "self_conflict_direct",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: raw_terms.to_vec(),
        }],
        noop_cb,
        core::ptr::null_mut(),
        reg,
    );
    assert!(matches!(sys.initialize(&mut world), SystemAccess::None));
    assert_eq!(
        sys.init_error(),
        Some(&SystemInitError::SelfConflict {
            a: "sched_reg::PaddedPair".into(),
            b: "sched_reg::PaddedPair".into(),
        })
    );
}

// ============================================================================
// B-06 & B-07 & B-05: Custom `ScheduleId >= 1024`, `BAM_SYSTEM_FLAG_MAIN_THREAD`,
//                     and `ModQuarantine::reenable`
// ============================================================================

#[derive(ScheduleLabel, Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct CustomPhysicsSchedule;

#[test]
fn regression_b06_b07_b05_custom_schedule_main_thread_pinning_and_quarantine_reenable() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_resource::<CounterRes>()
        .expect("register CounterRes");
    app.insert_resource(CounterRes::default());

    // Reserved range `6..1024` and built-in `< 1024` are rejected by `register_mod_schedule`.
    assert!(matches!(
        app.register_mod_schedule(ScheduleId::UPDATE, CustomPhysicsSchedule),
        Err(ModLoadError::InvalidScheduleRegistration { .. })
    ));
    assert!(matches!(
        app.register_mod_schedule(ScheduleId(500), CustomPhysicsSchedule),
        Err(ModLoadError::InvalidScheduleRegistration { .. })
    ));

    let custom_sched_id = ScheduleId::custom(1024);
    app.register_mod_schedule(custom_sched_id, CustomPhysicsSchedule)
        .expect("register custom schedule 1024");

    // Load `l01_late_startup` overriding its schedule to our custom `CustomPhysicsSchedule`.
    let dylib_path = fixture_dylib("l01_late_startup");
    let mod_id = app
        .load_mod_with(
            &dylib_path,
            LoadOptions::new()
                .with_shadow_copy(true)
                .with_schedule_override(custom_sched_id),
        )
        .expect("load mod into custom schedule");

    // Standard `app.update()` does NOT run `CustomPhysicsSchedule`.
    app.update();
    assert_eq!(
        *app.world().resource::<CounterRes>(),
        CounterRes {
            startup_hits: 0,
            update_hits: 0,
        }
    );

    // Running `CustomPhysicsSchedule` directly executes both overridden systems!
    app.world_mut().run_schedule(CustomPhysicsSchedule);
    assert_eq!(
        *app.world().resource::<CounterRes>(),
        CounterRes {
            startup_hits: 1,
            update_hits: 1,
        }
    );
    app.unload_mod(mod_id).expect("unload custom-scheduled mod");

    // B-07: Verify `BAM_SYSTEM_FLAG_MAIN_THREAD` sets `SystemStateFlags::NON_SEND` and executes
    // exclusively on the main thread inside `MultiThreadedExecutor`.
    let main_thread_id = std::thread::current().id();
    let observed_threads: Arc<Mutex<Vec<std::thread::ThreadId>>> = Arc::new(Mutex::new(Vec::new()));

    struct MainThreadProbe {
        threads: Arc<Mutex<Vec<std::thread::ThreadId>>>,
        fail_next: AtomicBool,
        runs: AtomicUsize,
    }

    unsafe extern "C" fn main_thread_cb(_sys_ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
        let probe = unsafe { &*(ud as *const MainThreadProbe) };
        probe
            .threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        probe.runs.fetch_add(1, Ordering::SeqCst);
        if probe.fail_next.swap(false, Ordering::SeqCst) {
            FfiResult::INTERNAL_ERROR
        } else {
            FfiResult::OK
        }
    }

    let probe = Arc::new(MainThreadProbe {
        threads: Arc::clone(&observed_threads),
        fail_next: AtomicBool::new(false),
        runs: AtomicUsize::new(0),
    });

    let reg = app.world().resource::<ModRegistry>().clone();
    let mut ctx = LoadContext::new(reg);
    let desc = BamSystemDesc::new(
        c"pinned_main_thread_sys".as_ptr(),
        core::ptr::null(),
        0,
        main_thread_cb,
        Arc::as_ptr(&probe) as *mut c_void,
    )
    .with_flags(BAM_SYSTEM_FLAG_MAIN_THREAD);
    assert_eq!(
        unsafe { host_register_system(ctx.as_ffi_ptr(), &desc) },
        FfiResult::OK
    );
    ctx.finish_init();

    let pinned_sys = ctx
        .registered_systems
        .pop()
        .unwrap()
        .with_mod_id(bevy_abi_mod_host::ModId(777));
    assert!(pinned_sys.is_main_thread());
    assert!(pinned_sys.flags().contains(SystemStateFlags::NON_SEND));

    app.world_mut()
        .resource_mut::<ModQuarantine>()
        .register_system(
            bevy_abi_mod_host::ModId(777),
            pinned_sys.system_name_arc(),
            pinned_sys.enabled_flag(),
        );

    let mut mt_schedule = Schedule::default();
    mt_schedule.set_executor(MultiThreadedExecutor::default());
    mt_schedule.add_systems(pinned_sys);

    for _ in 0..5 {
        mt_schedule.run(app.world_mut());
    }
    assert_eq!(probe.runs.load(Ordering::SeqCst), 5);
    assert!(
        observed_threads
            .lock()
            .unwrap()
            .iter()
            .all(|&tid| tid == main_thread_id),
        "BAM_SYSTEM_FLAG_MAIN_THREAD system must run strictly on the main thread"
    );

    // B-05 + B-09: Trigger a fault, verify quarantine + single `Messages<ModSystemFaulted>`, then re-enable.
    probe.fail_next.store(true, Ordering::SeqCst);
    mt_schedule.run(app.world_mut());
    assert_eq!(probe.runs.load(Ordering::SeqCst), 6);
    assert!(
        app.world()
            .resource::<ModQuarantine>()
            .is_quarantined(bevy_abi_mod_host::ModId(777), "pinned_main_thread_sys")
    );
    assert_eq!(
        app.world().resource::<Messages<ModSystemFaulted>>().len(),
        1
    );

    // While quarantined, subsequent schedule runs skip the callback.
    mt_schedule.run(app.world_mut());
    assert_eq!(probe.runs.load(Ordering::SeqCst), 6);

    // Re-enabling via `ModQuarantine::reenable` resumes execution!
    assert!(
        app.world()
            .resource::<ModQuarantine>()
            .reenable(bevy_abi_mod_host::ModId(777), "pinned_main_thread_sys")
    );
    mt_schedule.run(app.world_mut());
    assert_eq!(probe.runs.load(Ordering::SeqCst), 7);
}
