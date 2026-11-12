//! Parallel disjoint execution, borrow/resource conflict detection, and fault isolation.

use crate::support::{NameTag, Position, Velocity, ensure_guest_mods_built, reset_rust_mod_panic};
use bevy_abi_mod_host::{
    AbiModPlugin, ModAppExt, ModErrorPolicy, ModId, ModRegistry, ModSystemFaulted,
};
use bevy_abi_mod_host_sys::{
    ComponentAccessDescOwned, DynamicCSystem, ModInitContext, ModSchedule,
    host_iter_column_mutable, host_iter_column_readonly, host_iter_next_chunk,
    host_register_component, host_register_system, host_register_system_in_schedule,
};
use bevy_abi_mod_macros::ModExposable;
use bevy_abi_mod_sys::{
    ComponentAccessDesc, ComponentAccessMode, ComponentUuid, FfiResult, ModComponentDescriptor,
    RawChunkIter, RawCommandQueue, SystemDescriptor,
};
use bevy_app::{App, Startup, Update};
use bevy_ecs::component::Component;
use bevy_ecs::message::Messages;
use bevy_ecs::observer::On;
use bevy_ecs::resource::Resource;
use bevy_ecs::schedule::{
    IntoScheduleConfigs, LogLevel, MultiThreadedExecutor, Schedule, ScheduleBuildSettings,
};
use bevy_ecs::system::{Query, Res, RunSystemOnce, System, SystemAccess};
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use libloading::{Library, Symbol};
use std::sync::{Arc, Mutex};

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
struct StaticPosition {
    x: f32,
    y: f32,
    z: f32,
}

#[repr(C)]
#[derive(Resource, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
struct SimConfig {
    dt: f32,
    step_count: u32,
    banner_len_observed: u32,
}

unsafe extern "C" fn noop_callback(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn noop_multi_callback(
    _queries: *const *mut RawChunkIter,
    _count: usize,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

fn register_inline_system(app: &mut App, desc: &SystemDescriptor) -> FfiResult {
    let registry = Arc::new(app.world().resource::<ModRegistry>().clone());
    let mut ctx = ModInitContext::new(registry);
    ctx.world = app.world_mut() as *mut World;
    let status = {
        let _guard = ctx.make_active();
        let ctx_ptr = (&mut ctx as *mut ModInitContext).cast::<c_void>();
        unsafe { host_register_system(ctx_ptr, desc) }
    };
    for sys in ctx.registered_systems.drain(..) {
        app.add_systems(Update, sys);
    }
    status
}

#[test]
fn test_disjoint_systems_concurrency_execution() {
    let mut world = World::new();
    let registry = Arc::new(ModRegistry::new());
    world.insert_resource((*registry).clone());

    let is_active = Arc::new(AtomicBool::new(true));
    let mut init_ctx = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active),
        &mut world as *mut World,
    );
    let _guard = init_ctx.make_active();

    let desc_a = ModComponentDescriptor::new(
        c"DisjointCompA".as_ptr(),
        ComponentUuid::from_name("DisjointCompA"),
        4,
        4,
    );
    let desc_b = ModComponentDescriptor::new(
        c"DisjointCompB".as_ptr(),
        ComponentUuid::from_name("DisjointCompB"),
        4,
        4,
    );
    unsafe {
        host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc_a);
        host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc_b);
    }

    let world_reg = Arc::new(world.resource::<ModRegistry>().clone());

    let mut sys_a = DynamicCSystem::new(
        "sys_concurrent_a".to_string(),
        vec![ComponentAccessDescOwned {
            name: "DisjointCompA".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );
    let mut sys_b = DynamicCSystem::new(
        "sys_concurrent_b".to_string(),
        vec![ComponentAccessDescOwned {
            name: "DisjointCompB".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
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
    registry.register_component::<Position>(pos_id);
    registry.register_component::<Velocity>(vel_id);
    let reg_arc = Arc::new(registry);

    let query_pos_read = vec![ComponentAccessDescOwned {
        name: "Position".to_string(),
        mode: ComponentAccessMode::READ,
    }];
    let mut sys_r1 = DynamicCSystem::new(
        "sys_r1".to_string(),
        query_pos_read.clone(),
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
    );
    let mut sys_r2 = DynamicCSystem::new(
        "sys_r2".to_string(),
        query_pos_read,
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
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

    let sys_pos = DynamicCSystem::new(
        "sys_pos".to_string(),
        vec![ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
    );
    let sys_vel = DynamicCSystem::new(
        "sys_vel".to_string(),
        vec![ComponentAccessDescOwned {
            name: "Velocity".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
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
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
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
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
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
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<NameTag>();

    let registry = Arc::new(app.world().resource::<ModRegistry>().clone());

    let sys_a = DynamicCSystem::new(
        "sys_a_kinematics".to_string(),
        vec![
            ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            ComponentAccessDescOwned {
                name: "Velocity".to_string(),
                mode: ComponentAccessMode::READ,
            },
        ],
        sys_pos_kinematics,
        core::ptr::null_mut(),
        Arc::clone(&registry),
    );
    let sys_b = DynamicCSystem::new(
        "sys_b_tagger".to_string(),
        vec![ComponentAccessDescOwned {
            name: "NameTag".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        sys_tag_mutator,
        core::ptr::null_mut(),
        Arc::clone(&registry),
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
    let registry = Arc::new(ModRegistry::new());
    world.insert_resource((*registry).clone());

    let is_active = Arc::new(AtomicBool::new(true));
    let mut init_ctx = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active),
        &mut world as *mut World,
    );
    let _guard = init_ctx.make_active();

    let desc = ModComponentDescriptor::new(
        c"DynamicHealth".as_ptr(),
        ComponentUuid::from_name("DynamicHealth"),
        8,
        4,
    );
    let reg_res = unsafe {
        host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
    };
    assert_eq!(reg_res, FfiResult::OK);

    let world_reg = Arc::new(world.resource::<ModRegistry>().clone());

    // 1. WRITE + WRITE conflict
    let mut sys_w1 = DynamicCSystem::new(
        "sys_w1".to_string(),
        vec![ComponentAccessDescOwned {
            name: "DynamicHealth".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );
    let mut sys_w2 = DynamicCSystem::new(
        "sys_w2".to_string(),
        vec![ComponentAccessDescOwned {
            name: "DynamicHealth".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
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
    let sys_w1_b = DynamicCSystem::new(
        "sys_w1_b".to_string(),
        vec![ComponentAccessDescOwned {
            name: "DynamicHealth".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );
    let sys_w2_b = DynamicCSystem::new(
        "sys_w2_b".to_string(),
        vec![ComponentAccessDescOwned {
            name: "DynamicHealth".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );
    schedule_chained.add_systems((sys_w1_b, sys_w2_b).chain());
    assert!(schedule_chained.initialize(&mut world).is_ok());

    // 3. WRITE + READ conflict
    let mut sys_writer = DynamicCSystem::new(
        "sys_writer".to_string(),
        vec![ComponentAccessDescOwned {
            name: "DynamicHealth".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );
    let mut sys_reader = DynamicCSystem::new(
        "sys_reader".to_string(),
        vec![ComponentAccessDescOwned {
            name: "DynamicHealth".to_string(),
            mode: ComponentAccessMode::READ,
        }],
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
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
    app.register_mod_component::<StaticPosition>();

    let world_reg = Arc::new(app.world().resource::<ModRegistry>().clone());

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

    let dynamic_writer = DynamicCSystem::new(
        "dyn_writer".to_string(),
        vec![ComponentAccessDescOwned {
            name: "StaticPosition".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );

    let mut schedule = Schedule::default();
    schedule.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedule.add_systems((native_writer_system, dynamic_writer));
    assert!(schedule.initialize(app.world_mut()).is_err());

    let dynamic_reader = DynamicCSystem::new(
        "dyn_reader".to_string(),
        vec![ComponentAccessDescOwned {
            name: "StaticPosition".to_string(),
            mode: ComponentAccessMode::READ,
        }],
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
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
    app.register_mod_resource::<SimConfig>();
    app.insert_resource(SimConfig::default());

    app.edit_schedule(Update, |schedule| {
        schedule.set_build_settings(ScheduleBuildSettings {
            ambiguity_detection: LogLevel::Error,
            ..Default::default()
        });
    });

    let queries = [ComponentAccessDesc::resource_write(c"SimConfig".as_ptr())];
    let desc = SystemDescriptor::new(
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
    let registry = Arc::new(ModRegistry::new());
    world.insert_resource((*registry).clone());

    let is_active = Arc::new(AtomicBool::new(true));
    let mut init_ctx = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active),
        &mut world as *mut World,
    );
    let _guard = init_ctx.make_active();

    let desc = ModComponentDescriptor::new(
        c"CombatStat".as_ptr(),
        ComponentUuid::from_name("CombatStat"),
        4,
        4,
    );
    unsafe {
        host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc);
    }
    let world_reg = Arc::new(world.resource::<ModRegistry>().clone());

    for (label, modes) in [
        (
            "sys_ww",
            [ComponentAccessMode::WRITE, ComponentAccessMode::WRITE],
        ),
        (
            "sys_wr",
            [ComponentAccessMode::WRITE, ComponentAccessMode::READ],
        ),
        (
            "sys_rw",
            [ComponentAccessMode::READ, ComponentAccessMode::WRITE],
        ),
    ] {
        let mut sys = DynamicCSystem::new(
            label.to_string(),
            vec![
                ComponentAccessDescOwned {
                    name: "CombatStat".to_string(),
                    mode: modes[0],
                },
                ComponentAccessDescOwned {
                    name: "CombatStat".to_string(),
                    mode: modes[1],
                },
            ],
            noop_callback,
            core::ptr::null_mut(),
            Arc::clone(&world_reg),
        );
        let acc = sys.initialize(&mut world);
        assert!(sys.has_self_conflict());
        assert!(!sys.is_enabled());
        assert_eq!(acc, SystemAccess::None);
    }

    for (label, modes) in [
        (
            "sys_mq_ww",
            [ComponentAccessMode::WRITE, ComponentAccessMode::WRITE],
        ),
        (
            "sys_mq_wr",
            [ComponentAccessMode::WRITE, ComponentAccessMode::READ],
        ),
    ] {
        let mut sys_mq = DynamicCSystem::new_multi_query(
            label.to_string(),
            vec![
                vec![ComponentAccessDescOwned {
                    name: "CombatStat".to_string(),
                    mode: modes[0],
                }],
                vec![ComponentAccessDescOwned {
                    name: "CombatStat".to_string(),
                    mode: modes[1],
                }],
            ],
            noop_multi_callback,
            core::ptr::null_mut(),
            Arc::clone(&world_reg),
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
    let registry = Arc::new(ModRegistry::new());
    world.insert_resource((*registry).clone());

    let is_active = Arc::new(AtomicBool::new(true));
    let mut init_ctx = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active),
        &mut world as *mut World,
    );
    let _guard = init_ctx.make_active();

    let desc = ModComponentDescriptor::new(
        c"KnownComp".as_ptr(),
        ComponentUuid::from_name("KnownComp"),
        4,
        4,
    );
    unsafe {
        host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc);
    }
    let world_reg = Arc::new(world.resource::<ModRegistry>().clone());

    let mut sys_unknown = DynamicCSystem::new(
        "sys_unknown".to_string(),
        vec![ComponentAccessDescOwned {
            name: "GhostComponentDoesNotExist".to_string(),
            mode: ComponentAccessMode::READ,
        }],
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );
    assert_eq!(sys_unknown.initialize(&mut world), SystemAccess::None);
    assert!(!sys_unknown.is_enabled());

    let mut sys_partial_unknown = DynamicCSystem::new(
        "sys_partial_unknown".to_string(),
        vec![
            ComponentAccessDescOwned {
                name: "KnownComp".to_string(),
                mode: ComponentAccessMode::READ,
            },
            ComponentAccessDescOwned {
                name: "MissingGhostComp".to_string(),
                mode: ComponentAccessMode::READ,
            },
        ],
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );
    assert_eq!(
        sys_partial_unknown.initialize(&mut world),
        SystemAccess::None
    );
    assert!(!sys_partial_unknown.is_enabled());

    let mut sys_mq_partial = DynamicCSystem::new_multi_query(
        "sys_mq_partial".to_string(),
        vec![
            vec![ComponentAccessDescOwned {
                name: "KnownComp".to_string(),
                mode: ComponentAccessMode::READ,
            }],
            vec![ComponentAccessDescOwned {
                name: "MissingGhostComp".to_string(),
                mode: ComponentAccessMode::READ,
            }],
        ],
        noop_multi_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
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
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
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
    app.register_mod_component::<Position>();

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

    let sys = DynamicCSystem::new(
        "faulting_system".to_string(),
        vec![ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        stateful_test_callback,
        stats_ptr as *mut c_void,
        Arc::new(registry),
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
        app.register_mod_component::<Position>();
        app.world_mut().spawn(Position::default());

        let registry = app.world().resource::<ModRegistry>().clone();
        let stats = Box::into_raw(Box::new(CallbackStats {
            invocations: AtomicUsize::new(0),
            should_fail: true,
            fail_with_status: status,
        }));

        let sys = DynamicCSystem::new(
            format!("err_system_{}", status.0),
            vec![ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::READ,
            }],
            stateful_test_callback,
            stats as *mut c_void,
            Arc::new(registry),
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
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

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
    let sys_faulty = DynamicCSystem::new(
        "sys_faulty".to_string(),
        vec![ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        stateful_test_callback,
        stats_faulty as *mut c_void,
        Arc::new(registry.clone()),
    );

    let stats_healthy = Box::into_raw(Box::new(CallbackStats {
        invocations: AtomicUsize::new(0),
        should_fail: false,
        fail_with_status: FfiResult::OK,
    }));
    let sys_healthy = DynamicCSystem::new(
        "sys_healthy".to_string(),
        vec![ComponentAccessDescOwned {
            name: "Velocity".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        stateful_test_callback,
        stats_healthy as *mut c_void,
        Arc::new(registry),
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
    reset_rust_mod_panic(&rust_dylib);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

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

    let r_id = app.load_mod_tracked(&rust_dylib).expect("load rust mod");

    for _ in 0..3 {
        app.update();
    }

    for pos in app.world_mut().query::<&Position>().iter(app.world()) {
        assert_eq!(pos.y, 6.0);
    }

    unsafe {
        let lib = Library::new(&rust_dylib).expect("open rust dylib");
        let trigger: Symbol<unsafe extern "C" fn()> = lib
            .get(b"asymmetric_rust_mod_trigger_panic\0")
            .expect("resolve trigger");
        trigger();
    }

    app.update();
    app.update();

    for pos in app.world_mut().query::<&Position>().iter(app.world()) {
        assert_eq!(pos.y, 10.0);
    }

    app.unload_mod(r_id).unwrap();
    reset_rust_mod_panic(&rust_dylib);
}

unsafe extern "C" fn dynamic_panicking_callback(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
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
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult {
    let flag = unsafe { &*(user_data as *const AtomicBool) };
    if flag.load(Ordering::SeqCst) {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("Host-side unwinding stress panic inside dynamic system callback");
        }));
        if res.is_err() {
            return FfiResult::PANIC_CAUGHT;
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn dynamic_healthy_counter_callback(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult {
    let counter = unsafe { &*(user_data as *const AtomicUsize) };
    counter.fetch_add(1, Ordering::SeqCst);
    FfiResult::OK
}

#[test]
fn test_consecutive_and_retriggered_panics() {
    let mut app = App::new();
    let registry = Arc::new(ModRegistry::new());

    let panic_trigger_1 = Arc::new(AtomicBool::new(false));
    let panic_trigger_2 = Arc::new(AtomicBool::new(false));
    let panic_trigger_3 = Arc::new(AtomicBool::new(false));
    let unwinding_trigger = Arc::new(AtomicBool::new(false));
    let healthy_counter = Arc::new(AtomicUsize::new(0));

    let sys1 = DynamicCSystem::new(
        "panic_sys_1".to_string(),
        Vec::new(),
        dynamic_panicking_callback,
        Arc::as_ptr(&panic_trigger_1) as *mut c_void,
        Arc::clone(&registry),
    );
    let sys2 = DynamicCSystem::new(
        "panic_sys_2".to_string(),
        Vec::new(),
        dynamic_panicking_callback,
        Arc::as_ptr(&panic_trigger_2) as *mut c_void,
        Arc::clone(&registry),
    );
    let sys3 = DynamicCSystem::new(
        "panic_sys_3".to_string(),
        Vec::new(),
        dynamic_panicking_callback,
        Arc::as_ptr(&panic_trigger_3) as *mut c_void,
        Arc::clone(&registry),
    );
    let sys_unwind = DynamicCSystem::new(
        "unwind_sys".to_string(),
        Vec::new(),
        dynamic_unwinding_callback,
        Arc::as_ptr(&unwinding_trigger) as *mut c_void,
        Arc::clone(&registry),
    );
    let sys_healthy = DynamicCSystem::new(
        "healthy_sys".to_string(),
        Vec::new(),
        dynamic_healthy_counter_callback,
        Arc::as_ptr(&healthy_counter) as *mut c_void,
        Arc::clone(&registry),
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

    let mut retrigger_sys = DynamicCSystem::new(
        "retrigger_sys".to_string(),
        Vec::new(),
        dynamic_panicking_callback,
        Arc::as_ptr(&panic_trigger_1) as *mut c_void,
        Arc::clone(&registry),
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
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::INTERNAL_ERROR
}

unsafe extern "C" fn count_runs_cb(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult {
    unsafe { (*(user_data as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst) };
    FfiResult::OK
}

#[test]
fn mod_schedule_startup_error_policy_report_to_bevy_and_fault_events_work() {
    let startup_runs = Arc::new(AtomicUsize::new(0));
    let reg = Arc::new(ModRegistry::new());
    let mut init_ctx = ModInitContext::new(Arc::clone(&reg));
    let _guard = init_ctx.make_active();
    let desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: c"startup_guest_sys".as_ptr(),
        queries: core::ptr::null(),
        query_count: 0,
        callback: count_runs_cb,
        user_data: Arc::as_ptr(&startup_runs) as *mut c_void,
    };
    assert_eq!(
        unsafe {
            host_register_system_in_schedule(
                (&mut init_ctx as *mut ModInitContext).cast::<c_void>(),
                &desc,
                ModSchedule::Startup as u32,
            )
        },
        FfiResult::OK
    );
    drop(_guard);
    let startup_sys = init_ctx.registered_systems.pop().unwrap();
    assert_eq!(startup_sys.schedule(), ModSchedule::Startup);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_error_policy(
        ModErrorPolicy::ReportToBevy,
    ));
    app.add_systems(Startup, startup_sys);

    app.update();
    app.update();
    app.update();
    assert_eq!(startup_runs.load(Ordering::SeqCst), 1);

    let observed_faults = Arc::new(Mutex::new(Vec::new()));
    let obs_clone = Arc::clone(&observed_faults);
    app.world_mut()
        .add_observer(move |trigger: On<ModSystemFaulted>| {
            obs_clone.lock().unwrap().push(trigger.event().clone());
        });

    let fault_sys = DynamicCSystem::new(
        "faulting_sys".to_string(),
        vec![],
        failing_guest_cb,
        core::ptr::null_mut(),
        reg,
    )
    .with_mod_id(ModId(42));

    let run_res = app.world_mut().run_system_once(fault_sys);
    assert!(run_res.is_err());
    assert!(run_res.unwrap_err().to_string().contains("faulting_sys"));

    let obs = observed_faults.lock().unwrap();
    assert_eq!(obs.len(), 1);
    assert_eq!(obs[0].mod_id, Some(ModId(42)));
    assert_eq!(obs[0].status, FfiResult::INTERNAL_ERROR);

    let msgs = app.world().resource::<Messages<ModSystemFaulted>>();
    assert_eq!(msgs.len(), 1);
}
