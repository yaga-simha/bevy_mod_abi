//! Parallel disjoint execution, borrow/resource conflict detection, and fault isolation (`ABI 0.2.0`).

use crate::support::{NameTag, Position, TempDylibCopy, Velocity, ensure_guest_mods_built};
use bevy_abi_mod_host::{
    AbiModPlugin, ComponentOptions, LoadOptions, ModAppExt, ModErrorPolicy, ModId, ModQuarantine,
    ModRegistry, ModSystemFaulted,
};
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, GuestSystem, LoadContext, host_iter_column_mutable,
    host_iter_column_readonly, host_iter_next_chunk, host_register_component, host_register_system,
    host_sys_query,
};
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::{
    AccessMode, BamComponent, BamIter, BamQueryDesc, BamSystemCtx, BamSystemDesc, BamTermDesc,
    ComponentDesc, ComponentUuid, FfiResult, ScheduleId,
};
use bevy_app::{App, Startup, Update};
use bevy_ecs::component::Component;
use bevy_ecs::message::Messages;
use bevy_ecs::resource::Resource;
use bevy_ecs::schedule::{
    IntoScheduleConfigs, LogLevel, MultiThreadedExecutor, Schedule, ScheduleBuildSettings,
};
use bevy_ecs::system::{Query, Res, RunSystemOnce, System, SystemAccess};
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use libloading::{Library, Symbol};
use std::sync::Arc;

#[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
#[repr(C)]
#[mod_component(name = "sched_conflicts::StaticPosition")]
struct StaticPosition {
    x: f32,
    y: f32,
    z: f32,
}

#[repr(C)]
#[derive(Resource, ModComponent, Debug, Clone, Copy, PartialEq, Default)]
#[mod_component(name = "sched_conflicts::SimConfig")]
struct SimConfig {
    dt: f32,
    step_count: u32,
    banner_len_observed: u32,
}

unsafe extern "C" fn noop_callback(
    _sys_ctx: *mut BamSystemCtx,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

fn register_inline_system(app: &mut App, desc: &BamSystemDesc) -> FfiResult {
    let registry = app.world().resource::<ModRegistry>().clone();
    let mut ctx = LoadContext::with_world_and_mod_id(
        registry,
        Arc::new(AtomicBool::new(true)),
        app.world_mut() as *mut World,
        None,
    );
    let status = unsafe { host_register_system(ctx.as_ffi_ptr(), desc) };
    ctx.finish_init();
    for sys in ctx.registered_systems.drain(..) {
        app.add_systems(Update, sys);
    }
    status
}

#[test]
fn test_disjoint_systems_concurrency_execution() {
    let mut world = World::new();
    let registry = ModRegistry::new();
    world.insert_resource(registry.clone());

    let is_active = Arc::new(AtomicBool::new(true));
    let mut load_ctx = LoadContext::with_world_and_mod_id(
        registry,
        Arc::clone(&is_active),
        &mut world as *mut World,
        None,
    );

    let desc_a = ComponentDesc::new(
        c"sched_conflicts::DisjointCompA".as_ptr(),
        ComponentUuid::from_parts("sched_conflicts::DisjointCompA", 4, 4, 0),
        4,
        4,
    );
    let desc_b = ComponentDesc::new(
        c"sched_conflicts::DisjointCompB".as_ptr(),
        ComponentUuid::from_parts("sched_conflicts::DisjointCompB", 4, 4, 0),
        4,
        4,
    );
    let mut comp_a = BamComponent::INVALID;
    let mut comp_b = BamComponent::INVALID;
    unsafe {
        assert_eq!(
            host_register_component(load_ctx.as_ffi_ptr(), &desc_a, &mut comp_a),
            FfiResult::OK
        );
        assert_eq!(
            host_register_component(load_ctx.as_ffi_ptr(), &desc_b, &mut comp_b),
            FfiResult::OK
        );
    }
    load_ctx.finish_init();

    let world_reg = world.resource::<ModRegistry>().clone();

    let mut sys_a = GuestSystem::new(
        "sys_concurrent_a",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(comp_a)],
        }],
        noop_callback,
        core::ptr::null_mut(),
        world_reg.clone(),
    );
    let mut sys_b = GuestSystem::new(
        "sys_concurrent_b",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(comp_b)],
        }],
        noop_callback,
        core::ptr::null_mut(),
        world_reg,
    );

    let access_a = sys_a.initialize(&mut world);
    let access_b = sys_b.initialize(&mut world);
    assert!(
        access_a.is_compatible(&access_b),
        "Disjoint dynamic systems must have compatible access for parallel execution"
    );

    let mut schedule = Schedule::default();
    schedule.set_executor(MultiThreadedExecutor::default());
    schedule.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedule.add_systems((sys_a, sys_b));
    assert!(
        schedule.initialize(&mut world).is_ok(),
        "Disjoint dynamic systems must initialize in MultiThreadedExecutor without ambiguity error"
    );
    schedule.run(&mut world);
}

#[test]
fn test_compatible_read_read_and_disjoint_systems() {
    let mut world = World::new();
    let pos_id = world.register_component::<Position>();
    let vel_id = world.register_component::<Velocity>();

    let mut registry = ModRegistry::new();
    let pos_h = registry
        .register_component::<Position>(pos_id, ComponentOptions::new())
        .unwrap();
    let vel_h = registry
        .register_component::<Velocity>(vel_id, ComponentOptions::new())
        .unwrap();

    let query_pos_read = vec![BamQuerySpecOwned {
        flags: 0,
        terms: vec![BamTermDesc::read(pos_h)],
    }];
    let mut sys_r1 = GuestSystem::new(
        "sys_r1",
        query_pos_read.clone(),
        noop_callback,
        core::ptr::null_mut(),
        registry.clone(),
    );
    let mut sys_r2 = GuestSystem::new(
        "sys_r2",
        query_pos_read,
        noop_callback,
        core::ptr::null_mut(),
        registry.clone(),
    );

    let access_r1 = sys_r1.initialize(&mut world);
    let access_r2 = sys_r2.initialize(&mut world);
    if let (SystemAccess::Shared(s1), SystemAccess::Shared(s2)) = (&access_r1, &access_r2) {
        assert!(s1.is_compatible(s2));
    }

    let mut schedule_readers = Schedule::default();
    schedule_readers.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedule_readers.add_systems((sys_r1, sys_r2));
    assert!(schedule_readers.initialize(&mut world).is_ok());

    let sys_pos = GuestSystem::new(
        "sys_pos",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(pos_h)],
        }],
        noop_callback,
        core::ptr::null_mut(),
        registry.clone(),
    );
    let sys_vel = GuestSystem::new(
        "sys_vel",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(vel_h)],
        }],
        noop_callback,
        core::ptr::null_mut(),
        registry,
    );

    let mut schedule_disjoint = Schedule::default();
    schedule_disjoint.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedule_disjoint.add_systems((sys_pos, sys_vel));
    assert!(schedule_disjoint.initialize(&mut world).is_ok());
}

unsafe extern "C" fn sys_pos_kinematics(
    sys_ctx: *mut BamSystemCtx,
    _user_data: *mut c_void,
) -> FfiResult {
    let mut iter: *mut BamIter = core::ptr::null_mut();
    let st = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if st.is_err() {
        return st;
    }
    let mut count = 0usize;
    while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
        let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut Position };
        let vel_ptr = unsafe { host_iter_column_readonly(iter, 1) as *const Velocity };
        if pos_ptr.is_null() || vel_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        for i in 0..count {
            unsafe {
                let p = &mut *pos_ptr.add(i);
                let v = &*vel_ptr.add(i);
                p.x += v.x;
                p.y += v.y;
            }
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn sys_tag_mutator(
    sys_ctx: *mut BamSystemCtx,
    _user_data: *mut c_void,
) -> FfiResult {
    let mut iter: *mut BamIter = core::ptr::null_mut();
    let st = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if st.is_err() {
        return st;
    }
    let mut count = 0usize;
    while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
        let tag_ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut NameTag };
        if tag_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        for i in 0..count {
            unsafe {
                let tag = &mut *tag_ptr.add(i);
                tag.label.push('!');
            }
        }
    }
    FfiResult::OK
}

#[test]
fn test_disjoint_systems_multi_threaded_parallel_execution() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();
    let vel_h = app
        .register_mod_component::<Velocity>(ComponentOptions::new())
        .unwrap();
    let tag_h = app
        .register_mod_component::<NameTag>(ComponentOptions::new())
        .unwrap();

    let registry = app.world().resource::<ModRegistry>().clone();

    let sys_a = GuestSystem::new(
        "sys_a_kinematics",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(pos_h), BamTermDesc::read(vel_h)],
        }],
        sys_pos_kinematics,
        core::ptr::null_mut(),
        registry.clone(),
    );
    let sys_b = GuestSystem::new(
        "sys_b_tagger",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(tag_h)],
        }],
        sys_tag_mutator,
        core::ptr::null_mut(),
        registry,
    );

    app.add_systems(Update, (sys_a, sys_b));

    const NUM_ENTITIES: usize = 2000;
    for i in 0..NUM_ENTITIES {
        app.world_mut().spawn((
            Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 2.0,
                z: 0.0,
            },
            NameTag {
                label: "agent".to_string(),
            },
        ));
    }

    const TICKS: usize = 30;
    for _ in 0..TICKS {
        app.update();
    }

    let mut query = app.world_mut().query::<(&Position, &NameTag)>();
    let expected_suffix = "!".repeat(TICKS);
    let mut verified = 0;
    for (pos, tag) in query.iter(app.world()) {
        assert_eq!(pos.y, 2.0 * TICKS as f32);
        assert_eq!(tag.label, format!("agent{expected_suffix}"));
        verified += 1;
    }
    assert_eq!(verified, NUM_ENTITIES);
}

#[test]
fn test_write_write_and_write_read_conflict_detection() {
    let mut world = World::new();
    let registry = ModRegistry::new();
    world.insert_resource(registry.clone());

    let is_active = Arc::new(AtomicBool::new(true));
    let mut load_ctx = LoadContext::with_world_and_mod_id(
        registry,
        Arc::clone(&is_active),
        &mut world as *mut World,
        None,
    );

    let desc = ComponentDesc::new(
        c"sched_conflicts::DynamicHealth".as_ptr(),
        ComponentUuid::from_parts("sched_conflicts::DynamicHealth", 8, 4, 0),
        8,
        4,
    );
    let mut health_h = BamComponent::INVALID;
    let reg_res = unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut health_h) };
    assert_eq!(reg_res, FfiResult::OK);
    load_ctx.finish_init();

    let world_reg = world.resource::<ModRegistry>().clone();

    // 1. WRITE + WRITE conflict
    let mut sys_w1 = GuestSystem::new(
        "sys_w1",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(health_h)],
        }],
        noop_callback,
        core::ptr::null_mut(),
        world_reg.clone(),
    );
    let mut sys_w2 = GuestSystem::new(
        "sys_w2",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(health_h)],
        }],
        noop_callback,
        core::ptr::null_mut(),
        world_reg.clone(),
    );

    let access_w1 = sys_w1.initialize(&mut world);
    let access_w2 = sys_w2.initialize(&mut world);
    if let (SystemAccess::Shared(s1), SystemAccess::Shared(s2)) = (&access_w1, &access_w2) {
        assert!(!s1.is_compatible(s2));
    } else {
        panic!("Expected SystemAccess::Shared");
    }

    let mut schedule_ambig = Schedule::default();
    schedule_ambig.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedule_ambig.add_systems((sys_w1, sys_w2));
    assert!(schedule_ambig.initialize(&mut world).is_err());

    // 2. Ordered with .chain() MUST succeed
    let mut schedule_chained = Schedule::default();
    schedule_chained.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    let sys_w1_b = GuestSystem::new(
        "sys_w1_b",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(health_h)],
        }],
        noop_callback,
        core::ptr::null_mut(),
        world_reg.clone(),
    );
    let sys_w2_b = GuestSystem::new(
        "sys_w2_b",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(health_h)],
        }],
        noop_callback,
        core::ptr::null_mut(),
        world_reg.clone(),
    );
    schedule_chained.add_systems((sys_w1_b, sys_w2_b).chain());
    assert!(schedule_chained.initialize(&mut world).is_ok());

    // 3. WRITE + READ conflict
    let mut sys_writer = GuestSystem::new(
        "sys_writer",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(health_h)],
        }],
        noop_callback,
        core::ptr::null_mut(),
        world_reg.clone(),
    );
    let mut sys_reader = GuestSystem::new(
        "sys_reader",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::read(health_h)],
        }],
        noop_callback,
        core::ptr::null_mut(),
        world_reg,
    );
    let acc_w = sys_writer.initialize(&mut world);
    let acc_r = sys_reader.initialize(&mut world);
    assert!(!acc_w.is_compatible(&acc_r));

    let mut schedule_rw = Schedule::default();
    schedule_rw.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedule_rw.add_systems((sys_writer, sys_reader));
    assert!(schedule_rw.initialize(&mut world).is_err());
}

#[test]
fn test_native_and_dynamic_system_borrow_interaction() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let pos_h = app
        .register_mod_component::<StaticPosition>(ComponentOptions::new())
        .unwrap();

    let world_reg = app.world().resource::<ModRegistry>().clone();

    fn native_writer_system(mut query: Query<&mut StaticPosition>) {
        for mut pos in query.iter_mut() {
            pos.x += 1.0;
        }
    }

    fn native_reader_system(query: Query<&StaticPosition>) {
        for pos in query.iter() {
            let _ = pos.x;
        }
    }

    let dynamic_writer = GuestSystem::new(
        "dyn_writer",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(pos_h)],
        }],
        noop_callback,
        core::ptr::null_mut(),
        world_reg.clone(),
    );

    let mut schedule = Schedule::default();
    schedule.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedule.add_systems((native_writer_system, dynamic_writer));
    assert!(schedule.initialize(app.world_mut()).is_err());

    let dynamic_reader = GuestSystem::new(
        "dyn_reader",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::read(pos_h)],
        }],
        noop_callback,
        core::ptr::null_mut(),
        world_reg,
    );
    let mut schedule_readers = Schedule::default();
    schedule_readers.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedule_readers.add_systems((native_reader_system, dynamic_reader));
    assert!(schedule_readers.initialize(app.world_mut()).is_ok());
}

#[test]
#[should_panic]
fn test_ecs_resource_write_conflicts_with_native_bevy_res_under_strict_ambiguity() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let sim_h = app.register_mod_resource::<SimConfig>().unwrap();
    app.insert_resource(SimConfig::default());

    app.edit_schedule(Update, |schedule| {
        schedule.set_build_settings(ScheduleBuildSettings {
            ambiguity_detection: LogLevel::Error,
            ..Default::default()
        });
    });

    let terms = [BamTermDesc::resource_write(sim_h)];
    let queries = [BamQueryDesc::new(terms.as_ptr(), terms.len())];
    let desc = BamSystemDesc::new(
        c"unord_res_writer".as_ptr(),
        queries.as_ptr(),
        queries.len(),
        noop_callback,
        core::ptr::null_mut(),
    );
    assert_eq!(register_inline_system(&mut app, &desc), FfiResult::OK);

    app.add_systems(Update, |res: Res<SimConfig>| {
        let _ = res.dt;
    });

    app.update();
}

#[test]
fn test_intra_query_conflict_detection() {
    let mut world = World::new();
    let registry = ModRegistry::new();
    world.insert_resource(registry.clone());

    let is_active = Arc::new(AtomicBool::new(true));
    let mut load_ctx = LoadContext::with_world_and_mod_id(
        registry,
        Arc::clone(&is_active),
        &mut world as *mut World,
        None,
    );

    let desc = ComponentDesc::new(
        c"sched_conflicts::CombatStat".as_ptr(),
        ComponentUuid::from_parts("sched_conflicts::CombatStat", 4, 4, 0),
        4,
        4,
    );
    let mut stat_h = BamComponent::INVALID;
    unsafe {
        assert_eq!(
            host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut stat_h),
            FfiResult::OK
        );
    }
    load_ctx.finish_init();
    let world_reg = world.resource::<ModRegistry>().clone();

    for (label, modes) in [
        ("sys_ww", [AccessMode::WRITE, AccessMode::WRITE]),
        ("sys_wr", [AccessMode::WRITE, AccessMode::READ]),
        ("sys_rw", [AccessMode::READ, AccessMode::WRITE]),
    ] {
        let mut sys = GuestSystem::new(
            label,
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![
                    BamTermDesc::new(stat_h, modes[0]),
                    BamTermDesc::new(stat_h, modes[1]),
                ],
            }],
            noop_callback,
            core::ptr::null_mut(),
            world_reg.clone(),
        );
        let acc = sys.initialize(&mut world);
        assert!(sys.has_self_conflict());
        assert!(!sys.is_enabled());
        assert_eq!(acc, SystemAccess::None);
    }

    for (label, modes) in [
        ("sys_mq_ww", [AccessMode::WRITE, AccessMode::WRITE]),
        ("sys_mq_wr", [AccessMode::WRITE, AccessMode::READ]),
    ] {
        let mut sys_mq = GuestSystem::new(
            label,
            vec![
                BamQuerySpecOwned {
                    flags: 0,
                    terms: vec![BamTermDesc::new(stat_h, modes[0])],
                },
                BamQuerySpecOwned {
                    flags: 0,
                    terms: vec![BamTermDesc::new(stat_h, modes[1])],
                },
            ],
            noop_callback,
            core::ptr::null_mut(),
            world_reg.clone(),
        );
        let acc_mq = sys_mq.initialize(&mut world);
        assert!(sys_mq.has_self_conflict());
        assert!(!sys_mq.is_enabled());
        assert_eq!(acc_mq, SystemAccess::None);
    }
}

#[test]
fn test_unknown_component_disables_single_and_multi_query_systems() {
    let mut world = World::new();
    let registry = ModRegistry::new();
    world.insert_resource(registry.clone());

    let is_active = Arc::new(AtomicBool::new(true));
    let mut load_ctx = LoadContext::with_world_and_mod_id(
        registry,
        Arc::clone(&is_active),
        &mut world as *mut World,
        None,
    );

    let desc = ComponentDesc::new(
        c"sched_conflicts::KnownComp".as_ptr(),
        ComponentUuid::from_parts("sched_conflicts::KnownComp", 4, 4, 0),
        4,
        4,
    );
    let mut known_h = BamComponent::INVALID;
    unsafe {
        assert_eq!(
            host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut known_h),
            FfiResult::OK
        );
    }
    load_ctx.finish_init();
    let world_reg = world.resource::<ModRegistry>().clone();
    let ghost_h = BamComponent::from_raw(999);

    let mut sys_unknown = GuestSystem::new(
        "sys_unknown",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::read(ghost_h)],
        }],
        noop_callback,
        core::ptr::null_mut(),
        world_reg.clone(),
    );
    assert_eq!(sys_unknown.initialize(&mut world), SystemAccess::None);
    assert!(!sys_unknown.is_enabled());

    let mut sys_partial_unknown = GuestSystem::new(
        "sys_partial_unknown",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::read(known_h), BamTermDesc::read(ghost_h)],
        }],
        noop_callback,
        core::ptr::null_mut(),
        world_reg.clone(),
    );
    assert_eq!(
        sys_partial_unknown.initialize(&mut world),
        SystemAccess::None
    );
    assert!(!sys_partial_unknown.is_enabled());

    let mut sys_mq_partial = GuestSystem::new(
        "sys_mq_partial",
        vec![
            BamQuerySpecOwned {
                flags: 0,
                terms: vec![BamTermDesc::read(known_h)],
            },
            BamQuerySpecOwned {
                flags: 0,
                terms: vec![BamTermDesc::read(ghost_h)],
            },
        ],
        noop_callback,
        core::ptr::null_mut(),
        world_reg,
    );
    assert_eq!(sys_mq_partial.initialize(&mut world), SystemAccess::None);
    assert!(!sys_mq_partial.is_enabled());

    let mut schedule = Schedule::default();
    schedule.add_systems((sys_unknown, sys_partial_unknown, sys_mq_partial));
    schedule.run(&mut world);
}

// ============================================================================
// Fault Isolation, Error Policies & Panic Containment
// ============================================================================

struct CallbackStats {
    invocations: AtomicUsize,
    should_fail: bool,
    fail_with_status: FfiResult,
}

unsafe extern "C" fn stateful_test_callback(
    _sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let stats = unsafe { &*(user_data as *const CallbackStats) };
    stats.invocations.fetch_add(1, Ordering::SeqCst);
    if stats.should_fail {
        stats.fail_with_status
    } else {
        FfiResult::OK
    }
}

#[test]
fn test_fault_isolation_panic_caught_and_system_disabled() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();

    app.world_mut().spawn(Position {
        x: 1.0,
        y: 2.0,
        z: 3.0,
    });

    let registry = app.world().resource::<ModRegistry>().clone();
    let stats = Box::new(CallbackStats {
        invocations: AtomicUsize::new(0),
        should_fail: true,
        fail_with_status: FfiResult::PANIC_CAUGHT,
    });
    let stats_ptr = Box::into_raw(stats);

    let sys = GuestSystem::new(
        "faulting_system",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(pos_h)],
        }],
        stateful_test_callback,
        stats_ptr as *mut c_void,
        registry,
    );

    app.add_systems(Update, sys);

    app.update();
    let stats_ref = unsafe { &*stats_ptr };
    assert_eq!(stats_ref.invocations.load(Ordering::SeqCst), 1);

    for _ in 2..=10 {
        app.update();
    }
    assert_eq!(stats_ref.invocations.load(Ordering::SeqCst), 1);

    let mut q = app.world_mut().query::<&Position>();
    let pos = q.single(app.world()).unwrap();
    assert_eq!(
        *pos,
        Position {
            x: 1.0,
            y: 2.0,
            z: 3.0
        }
    );

    unsafe {
        drop(Box::from_raw(stats_ptr));
    }
}

#[test]
fn test_fault_isolation_error_statuses() {
    for status in [
        FfiResult::INTERNAL_ERROR,
        FfiResult::ACCESS_CONFLICT,
        FfiResult::PERMISSION_DENIED,
        FfiResult(9999),
    ] {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        app.world_mut().spawn(Position::default());

        let registry = app.world().resource::<ModRegistry>().clone();
        let stats = Box::into_raw(Box::new(CallbackStats {
            invocations: AtomicUsize::new(0),
            should_fail: true,
            fail_with_status: status,
        }));

        let sys = GuestSystem::new(
            format!("err_system_{}", status.0),
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![BamTermDesc::read(pos_h)],
            }],
            stateful_test_callback,
            stats as *mut c_void,
            registry,
        );

        app.add_systems(Update, sys);
        app.update();
        app.update();

        let count = unsafe { (*stats).invocations.load(Ordering::SeqCst) };
        assert_eq!(count, 1);

        unsafe {
            drop(Box::from_raw(stats));
        }
    }
}

#[test]
fn test_multi_system_fault_isolation_healthy_survives() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();
    let vel_h = app
        .register_mod_component::<Velocity>(ComponentOptions::new())
        .unwrap();

    app.world_mut().spawn((
        Position::default(),
        Velocity {
            x: 1.0,
            y: 1.0,
            z: 1.0,
        },
    ));

    let registry = app.world().resource::<ModRegistry>().clone();

    let stats_faulty = Box::into_raw(Box::new(CallbackStats {
        invocations: AtomicUsize::new(0),
        should_fail: true,
        fail_with_status: FfiResult::PANIC_CAUGHT,
    }));
    let sys_faulty = GuestSystem::new(
        "sys_faulty",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(pos_h)],
        }],
        stateful_test_callback,
        stats_faulty as *mut c_void,
        registry.clone(),
    );

    let stats_healthy = Box::into_raw(Box::new(CallbackStats {
        invocations: AtomicUsize::new(0),
        should_fail: false,
        fail_with_status: FfiResult::OK,
    }));
    let sys_healthy = GuestSystem::new(
        "sys_healthy",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(vel_h)],
        }],
        stateful_test_callback,
        stats_healthy as *mut c_void,
        registry,
    );

    app.add_systems(Update, (sys_faulty, sys_healthy).chain());

    for _ in 1..=5 {
        app.update();
    }

    assert_eq!(
        unsafe { (*stats_faulty).invocations.load(Ordering::SeqCst) },
        1
    );
    assert_eq!(
        unsafe { (*stats_healthy).invocations.load(Ordering::SeqCst) },
        5
    );

    unsafe {
        drop(Box::from_raw(stats_faulty));
        drop(Box::from_raw(stats_healthy));
    }
}

#[test]
fn test_cross_runtime_panic_containment() {
    let (rust_dylib, _c_dylib) = ensure_guest_mods_built();
    // Use an isolated `TempDylibCopy` without `shadow_copy` so `Library::new` below resolves
    // `asymmetric_rust_mod_trigger_panic` in the exact `.bss` image mapped by `app.load_mod_with`,
    // without touching any shared `.bss` across concurrent parallel tests!
    let isolated_rust_dylib = TempDylibCopy::new(&rust_dylib, "sched_panic_containment");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(ComponentOptions::new().with_default())
        .unwrap();
    app.register_mod_component::<Velocity>(ComponentOptions::new().with_default())
        .unwrap();

    for i in 0..10 {
        app.world_mut().spawn((
            Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 2.0,
                z: 0.0,
            },
        ));
    }

    let r_id = app
        .load_mod_with(
            isolated_rust_dylib.path(),
            LoadOptions::new().with_shadow_copy(false),
        )
        .expect("load isolated rust mod");

    for _ in 0..3 {
        app.update();
    }

    for pos in app.world_mut().query::<&Position>().iter(app.world()) {
        assert_eq!(pos.y, 6.0);
    }

    unsafe {
        let lib = Library::new(isolated_rust_dylib.path()).expect("open isolated rust dylib");
        let trigger: Symbol<unsafe extern "C" fn()> = lib
            .get(b"asymmetric_rust_mod_trigger_panic\0")
            .expect("resolve trigger");
        trigger();
    }

    app.update();
    app.update();

    // `rust_panicking` was quarantined while `rust_kinematics` kept running across both frames!
    assert!(
        app.world()
            .resource::<ModQuarantine>()
            .is_quarantined(r_id, "rust_panicking")
    );
    for pos in app.world_mut().query::<&Position>().iter(app.world()) {
        assert_eq!(pos.y, 10.0);
    }

    // Verify single-path `Messages<ModSystemFaulted>` recorded `panicked == true`.
    {
        let msgs = app.world().resource::<Messages<ModSystemFaulted>>();
        let mut cursor = msgs.get_cursor();
        let faults: Vec<&ModSystemFaulted> = cursor.read(msgs).collect();
        assert_eq!(faults.len(), 1);
        assert_eq!(faults[0].mod_id, Some(r_id));
        assert_eq!(faults[0].system_name, "rust_panicking");
        assert_eq!(faults[0].status, FfiResult::PANIC_CAUGHT);
        assert!(faults[0].panicked);
    }

    // Re-enabling `rust_panicking` via `ModQuarantine::reenable` lets it run cleanly again
    // (since `PANIC_TRIGGER.swap(false)` already cleared the trigger on the faulting tick).
    assert!(
        app.world()
            .resource::<ModQuarantine>()
            .reenable(r_id, "rust_panicking")
    );
    app.update();
    assert!(
        !app.world()
            .resource::<ModQuarantine>()
            .is_quarantined(r_id, "rust_panicking")
    );

    app.unload_mod(r_id).unwrap();
}

unsafe extern "C" fn dynamic_panicking_callback(
    _sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let flag = unsafe { &*(user_data as *const AtomicBool) };
    if flag.load(Ordering::SeqCst) {
        FfiResult::PANIC_CAUGHT
    } else {
        FfiResult::OK
    }
}

unsafe extern "C" fn dynamic_unwinding_callback(
    _sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let flag = unsafe { &*(user_data as *const AtomicBool) };
    if flag.load(Ordering::SeqCst) {
        // Simulates guest-side `catch_unwind` inside a Rust guest `.cdylib`.
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("Simulated guest-side unwinding panic caught inside callback");
        }));
        if res.is_err() {
            return FfiResult::PANIC_CAUGHT;
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn dynamic_healthy_counter_callback(
    _sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let counter = unsafe { &*(user_data as *const AtomicUsize) };
    counter.fetch_add(1, Ordering::SeqCst);
    FfiResult::OK
}

#[test]
fn test_consecutive_and_retriggered_panics() {
    let mut app = App::new();
    let registry = ModRegistry::new();

    let panic_trigger_1 = Arc::new(AtomicBool::new(false));
    let panic_trigger_2 = Arc::new(AtomicBool::new(false));
    let panic_trigger_3 = Arc::new(AtomicBool::new(false));
    let unwinding_trigger = Arc::new(AtomicBool::new(false));
    let healthy_counter = Arc::new(AtomicUsize::new(0));

    let sys1 = GuestSystem::new(
        "panic_sys_1",
        Vec::new(),
        dynamic_panicking_callback,
        Arc::as_ptr(&panic_trigger_1) as *mut c_void,
        registry.clone(),
    );
    let sys2 = GuestSystem::new(
        "panic_sys_2",
        Vec::new(),
        dynamic_panicking_callback,
        Arc::as_ptr(&panic_trigger_2) as *mut c_void,
        registry.clone(),
    );
    let sys3 = GuestSystem::new(
        "panic_sys_3",
        Vec::new(),
        dynamic_panicking_callback,
        Arc::as_ptr(&panic_trigger_3) as *mut c_void,
        registry.clone(),
    );
    let sys_unwind = GuestSystem::new(
        "unwind_sys",
        Vec::new(),
        dynamic_unwinding_callback,
        Arc::as_ptr(&unwinding_trigger) as *mut c_void,
        registry.clone(),
    );
    let sys_healthy = GuestSystem::new(
        "healthy_sys",
        Vec::new(),
        dynamic_healthy_counter_callback,
        Arc::as_ptr(&healthy_counter) as *mut c_void,
        registry.clone(),
    );

    app.add_systems(Update, (sys1, sys2, sys3, sys_unwind, sys_healthy));

    for _ in 0..5 {
        app.update();
    }
    assert_eq!(healthy_counter.load(Ordering::SeqCst), 5);

    panic_trigger_1.store(true, Ordering::SeqCst);
    app.update();
    assert_eq!(healthy_counter.load(Ordering::SeqCst), 6);

    panic_trigger_2.store(true, Ordering::SeqCst);
    app.update();
    assert_eq!(healthy_counter.load(Ordering::SeqCst), 7);

    panic_trigger_3.store(true, Ordering::SeqCst);
    app.update();
    assert_eq!(healthy_counter.load(Ordering::SeqCst), 8);

    unwinding_trigger.store(true, Ordering::SeqCst);
    app.update();
    assert_eq!(healthy_counter.load(Ordering::SeqCst), 9);

    for _ in 0..50 {
        app.update();
    }
    assert_eq!(healthy_counter.load(Ordering::SeqCst), 59);

    let mut retrigger_sys = GuestSystem::new(
        "retrigger_sys",
        Vec::new(),
        dynamic_panicking_callback,
        Arc::as_ptr(&panic_trigger_1) as *mut c_void,
        registry,
    );

    let mut world = World::new();
    System::initialize(&mut retrigger_sys, &mut world);

    for _ in 0..30 {
        assert!(retrigger_sys.is_enabled());
        let _ = unsafe { System::run_unsafe(&mut retrigger_sys, (), world.as_unsafe_world_cell()) };
        assert!(!retrigger_sys.is_enabled());
        retrigger_sys.set_enabled(true);
    }
}

unsafe extern "C" fn failing_guest_cb(
    _sys_ctx: *mut BamSystemCtx,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::INTERNAL_ERROR
}

unsafe extern "C" fn count_runs_cb(
    _sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    unsafe { (*(user_data as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst) };
    FfiResult::OK
}

#[test]
fn mod_schedule_startup_error_policy_report_to_bevy_and_fault_messages_work() {
    let startup_runs = Arc::new(AtomicUsize::new(0));
    let reg = ModRegistry::new();
    let mut load_ctx = LoadContext::new(reg.clone());
    let desc = BamSystemDesc::new(
        c"startup_guest_sys".as_ptr(),
        core::ptr::null(),
        0,
        count_runs_cb,
        Arc::as_ptr(&startup_runs) as *mut c_void,
    )
    .with_schedule(ScheduleId::STARTUP);
    assert_eq!(
        unsafe { host_register_system(load_ctx.as_ffi_ptr(), &desc) },
        FfiResult::OK
    );
    load_ctx.finish_init();
    let startup_sys = load_ctx.registered_systems.pop().unwrap();
    assert_eq!(startup_sys.schedule(), ScheduleId::STARTUP);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_error_policy(
        ModErrorPolicy::ReportToBevy,
    ));
    app.add_systems(Startup, startup_sys);

    app.update();
    app.update();
    app.update();
    assert_eq!(startup_runs.load(Ordering::SeqCst), 1);

    let fault_sys = GuestSystem::new(
        "faulting_sys",
        vec![],
        failing_guest_cb,
        core::ptr::null_mut(),
        reg,
    )
    .with_mod_id(ModId(42));

    let run_res = app.world_mut().run_system_once(fault_sys);
    assert!(run_res.is_err());
    assert!(run_res.unwrap_err().to_string().contains("faulting_sys"));

    // Emitted once through `Messages<ModSystemFaulted>` (no duplicate observer `Event` write).
    let msgs = app.world().resource::<Messages<ModSystemFaulted>>();
    assert_eq!(msgs.len(), 1);
    let mut cursor = msgs.get_cursor();
    let events: Vec<&ModSystemFaulted> = cursor.read(msgs).collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].mod_id, Some(ModId(42)));
    assert_eq!(events[0].system_name, "faulting_sys");
    assert_eq!(events[0].status, FfiResult::INTERNAL_ERROR);
    assert!(!events[0].panicked);
}
