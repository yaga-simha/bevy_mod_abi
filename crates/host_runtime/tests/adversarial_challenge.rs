#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use abi_contract::{ComponentAccessMode, FfiResult, RawChunkIter, RawCommandQueue};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::{Resource, World};
use bevy_ecs::query::Changed;
use bevy_ecs::schedule::{IntoScheduleConfigs, LogLevel, Schedule, ScheduleBuildSettings};
use bevy_ecs::system::{Query, ResMut, System, SystemAccess};
use core::ffi::c_void;
use host_runtime::{
    host_iter_column_mutable, host_iter_column_readonly, host_iter_mark_changed,
    host_iter_next_chunk, AbiModPlugin, DynamicCSystem, ModAppExt, ModRegistry,
};
use mod_derive::ModExposable;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Health {
    pub current: i32,
    pub max: i32,
}

unsafe extern "C" fn noop_system(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

// ============================================================================
// SUITE 1: DYNAMIC SYSTEM BORROW CONFLICT DETECTION
// ============================================================================

#[test]
fn test_borrow_conflict_write_write_detection() {
    let mut world = World::new();
    let pos_id = world.register_component::<Position>();

    let mut registry = ModRegistry::new();
    registry.register_component::<Position>(pos_id);
    let reg_arc = Arc::new(registry);

    let query_write = vec![host_runtime::ComponentAccessDescOwned {
        name: "Position".to_string(),
        mode: ComponentAccessMode::WRITE,
    }];

    let mut sys_w1 = DynamicCSystem::new(
        "sys_w1".to_string(),
        query_write.clone(),
        noop_system,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
    );
    let mut sys_w2 = DynamicCSystem::new(
        "sys_w2".to_string(),
        query_write,
        noop_system,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
    );

    let access_w1 = sys_w1.initialize(&mut world);
    let access_w2 = sys_w2.initialize(&mut world);

    // 1. FilteredAccessSet level compatibility check
    if let (SystemAccess::Shared(s1), SystemAccess::Shared(s2)) = (&access_w1, &access_w2) {
        assert!(
            !s1.is_compatible(s2),
            "Two systems with WRITE access on Position must NOT be compatible"
        );
    } else {
        panic!("Expected SystemAccess::Shared");
    }

    // 2. Bevy Schedule Ambiguity Detection check
    let mut schedule = Schedule::default();
    schedule.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedule.add_systems((sys_w1, sys_w2));
    let init_res = schedule.initialize(&mut world);
    assert!(
        init_res.is_err(),
        "Bevy scheduler must detect unconstrained WRITE + WRITE borrow conflict as ambiguity error"
    );

    // 3. Resolving ambiguity with explicit .chain() order must succeed
    let mut chained_schedule = Schedule::default();
    chained_schedule.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    let sys_w1_b = DynamicCSystem::new(
        "sys_w1_b".to_string(),
        vec![host_runtime::ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        noop_system,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
    );
    let sys_w2_b = DynamicCSystem::new(
        "sys_w2_b".to_string(),
        vec![host_runtime::ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        noop_system,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
    );
    chained_schedule.add_systems((sys_w1_b, sys_w2_b).chain());
    assert!(
        chained_schedule.initialize(&mut world).is_ok(),
        "Chained conflicting systems must satisfy scheduler ordering"
    );
}

#[test]
fn test_borrow_conflict_write_read_detection() {
    let mut world = World::new();
    let pos_id = world.register_component::<Position>();

    let mut registry = ModRegistry::new();
    registry.register_component::<Position>(pos_id);
    let reg_arc = Arc::new(registry);

    let query_write = vec![host_runtime::ComponentAccessDescOwned {
        name: "Position".to_string(),
        mode: ComponentAccessMode::WRITE,
    }];
    let query_read = vec![host_runtime::ComponentAccessDescOwned {
        name: "Position".to_string(),
        mode: ComponentAccessMode::READ,
    }];

    let mut sys_w = DynamicCSystem::new(
        "sys_w".to_string(),
        query_write,
        noop_system,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
    );
    let mut sys_r = DynamicCSystem::new(
        "sys_r".to_string(),
        query_read,
        noop_system,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
    );

    let access_w = sys_w.initialize(&mut world);
    let access_r = sys_r.initialize(&mut world);

    // 1. Access set compatibility
    if let (SystemAccess::Shared(sw), SystemAccess::Shared(sr)) = (&access_w, &access_r) {
        assert!(
            !sw.is_compatible(sr),
            "WRITE and READ on same component must NOT be compatible"
        );
    } else {
        panic!("Expected SystemAccess::Shared");
    }

    // 2. Bevy Schedule Ambiguity Detection check
    let mut schedule = Schedule::default();
    schedule.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedule.add_systems((sys_w, sys_r));
    assert!(
        schedule.initialize(&mut world).is_err(),
        "Bevy scheduler must detect unconstrained WRITE + READ borrow conflict as ambiguity error"
    );
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

    // Case A: Read Position + Read Position (compatible)
    let query_pos_read = vec![host_runtime::ComponentAccessDescOwned {
        name: "Position".to_string(),
        mode: ComponentAccessMode::READ,
    }];
    let mut sys_r1 = DynamicCSystem::new(
        "sys_r1".to_string(),
        query_pos_read.clone(),
        noop_system,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
    );
    let mut sys_r2 = DynamicCSystem::new(
        "sys_r2".to_string(),
        query_pos_read,
        noop_system,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
    );

    let access_r1 = sys_r1.initialize(&mut world);
    let access_r2 = sys_r2.initialize(&mut world);
    if let (SystemAccess::Shared(s1), SystemAccess::Shared(s2)) = (&access_r1, &access_r2) {
        assert!(
            s1.is_compatible(s2),
            "Two readers on Position must be compatible"
        );
    }

    let mut schedule_readers = Schedule::default();
    schedule_readers.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedule_readers.add_systems((sys_r1, sys_r2));
    assert!(
        schedule_readers.initialize(&mut world).is_ok(),
        "Two concurrent reader systems must not trigger ambiguity errors"
    );

    // Case B: Write Position + Write Velocity (disjoint components, compatible)
    let query_pos_write = vec![host_runtime::ComponentAccessDescOwned {
        name: "Position".to_string(),
        mode: ComponentAccessMode::WRITE,
    }];
    let query_vel_write = vec![host_runtime::ComponentAccessDescOwned {
        name: "Velocity".to_string(),
        mode: ComponentAccessMode::WRITE,
    }];
    let mut sys_pos = DynamicCSystem::new(
        "sys_pos".to_string(),
        query_pos_write,
        noop_system,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
    );
    let mut sys_vel = DynamicCSystem::new(
        "sys_vel".to_string(),
        query_vel_write,
        noop_system,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
    );

    let access_pos = sys_pos.initialize(&mut world);
    let access_vel = sys_vel.initialize(&mut world);
    if let (SystemAccess::Shared(sp), SystemAccess::Shared(sv)) = (&access_pos, &access_vel) {
        assert!(
            sp.is_compatible(sv),
            "Disjoint component writes must be compatible"
        );
    }

    let mut schedule_disjoint = Schedule::default();
    schedule_disjoint.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedule_disjoint.add_systems((sys_pos, sys_vel));
    assert!(
        schedule_disjoint.initialize(&mut world).is_ok(),
        "Disjoint component writes must not trigger ambiguity errors"
    );
}

// ============================================================================
// SUITE 2: FAULT ISOLATION & SYSTEM DISABLING
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
    let query_descs = vec![host_runtime::ComponentAccessDescOwned {
        name: "Position".to_string(),
        mode: ComponentAccessMode::WRITE,
    }];

    let stats = Box::new(CallbackStats {
        invocations: AtomicUsize::new(0),
        should_fail: true,
        fail_with_status: FfiResult::PANIC_CAUGHT,
    });
    let stats_ptr = Box::into_raw(stats);

    let sys = DynamicCSystem::new(
        "faulting_system".to_string(),
        query_descs,
        stateful_test_callback,
        stats_ptr as *mut c_void,
        Arc::new(registry),
    );

    app.add_systems(Update, sys);

    // Frame 1: system executes, returns PANIC_CAUGHT, gets disabled
    app.update();
    let stats_ref = unsafe { &*stats_ptr };
    assert_eq!(stats_ref.invocations.load(Ordering::SeqCst), 1);

    // Frames 2..10: system is disabled, must NEVER be invoked again
    for _ in 2..=10 {
        app.update();
    }
    assert_eq!(
        stats_ref.invocations.load(Ordering::SeqCst),
        1,
        "Disabled system must not be invoked on subsequent frames"
    );

    // Verify World consistency
    let mut q = app.world_mut().query::<&Position>();
    let pos = q.single(app.world()).unwrap();
    assert_eq!(pos.x, 1.0);
    assert_eq!(pos.y, 2.0);
    assert_eq!(pos.z, 3.0);

    // Clean up
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
        FfiResult(9999), // arbitrary unknown error
    ] {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Position>();
        app.world_mut().spawn(Position {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        });

        let registry = app.world().resource::<ModRegistry>().clone();
        let query_descs = vec![host_runtime::ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::READ,
        }];

        let stats = Box::into_raw(Box::new(CallbackStats {
            invocations: AtomicUsize::new(0),
            should_fail: true,
            fail_with_status: status,
        }));

        let sys = DynamicCSystem::new(
            format!("err_system_{}", status.0),
            query_descs,
            stateful_test_callback,
            stats as *mut c_void,
            Arc::new(registry),
        );

        app.add_systems(Update, sys);
        app.update(); // Frame 1: fails
        app.update(); // Frame 2: should be disabled

        let count = unsafe { (*stats).invocations.load(Ordering::SeqCst) };
        assert_eq!(
            count, 1,
            "System returning status code {} must be disabled after 1 invocation",
            status.0
        );

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
        Position {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        },
        Velocity {
            x: 1.0,
            y: 1.0,
            z: 1.0,
        },
    ));

    let registry = app.world().resource::<ModRegistry>().clone();

    // System A: Faulty (returns PANIC_CAUGHT on Position)
    let stats_faulty = Box::into_raw(Box::new(CallbackStats {
        invocations: AtomicUsize::new(0),
        should_fail: true,
        fail_with_status: FfiResult::PANIC_CAUGHT,
    }));
    let sys_faulty = DynamicCSystem::new(
        "sys_faulty".to_string(),
        vec![host_runtime::ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        stateful_test_callback,
        stats_faulty as *mut c_void,
        Arc::new(registry.clone()),
    );

    // System B: Healthy (updates Velocity)
    let stats_healthy = Box::into_raw(Box::new(CallbackStats {
        invocations: AtomicUsize::new(0),
        should_fail: false,
        fail_with_status: FfiResult::OK,
    }));
    let sys_healthy = DynamicCSystem::new(
        "sys_healthy".to_string(),
        vec![host_runtime::ComponentAccessDescOwned {
            name: "Velocity".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        stateful_test_callback,
        stats_healthy as *mut c_void,
        Arc::new(registry),
    );

    // Add both systems with explicit ordering
    app.add_systems(Update, (sys_faulty, sys_healthy).chain());

    // Run 5 frames
    for _ in 1..=5 {
        app.update();
    }

    let faulty_invocations = unsafe { (*stats_faulty).invocations.load(Ordering::SeqCst) };
    let healthy_invocations = unsafe { (*stats_healthy).invocations.load(Ordering::SeqCst) };

    assert_eq!(
        faulty_invocations, 1,
        "Faulty system must be disabled after frame 1"
    );
    assert_eq!(
        healthy_invocations, 5,
        "Healthy system must continue executing across all 5 frames without degradation"
    );

    unsafe {
        drop(Box::from_raw(stats_faulty));
        drop(Box::from_raw(stats_healthy));
    }
}

// ============================================================================
// SUITE 3: CHANGE DETECTION RELIABILITY
// ============================================================================

#[derive(Resource, Default)]
struct ChangeTracker {
    changed_count: usize,
    changed_entities: Vec<Entity>,
}

fn track_changed_positions(
    query: Query<(Entity, &Position), Changed<Position>>,
    mut tracker: ResMut<ChangeTracker>,
) {
    tracker.changed_entities = query.iter().map(|(e, _)| e).collect();
    tracker.changed_count = tracker.changed_entities.len();
}

struct ChangeSimContext {
    mode: usize, // 0 = no mutate, 1 = mutate all via column_mutable, 2 = fine-grained mark rows 3 and 7
}

unsafe extern "C" fn change_sim_callback(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult {
    let ctx = unsafe { &*(user_data as *const ChangeSimContext) };
    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }

        match ctx.mode {
            1 => {
                // Mutate all via column_mutable
                let ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut Position };
                if !ptr.is_null() {
                    for i in 0..count {
                        unsafe {
                            (*ptr.add(i)).x += 1.0;
                        }
                    }
                }
            }
            2 => {
                // Fine-grained row marking: only mark row 3 and row 7
                if count > 7 {
                    let _ = unsafe { host_iter_mark_changed(iter, 0, 3) };
                    let _ = unsafe { host_iter_mark_changed(iter, 0, 7) };
                }
            }
            _ => {
                // Read-only access: no mutation, no tick marking
                let _ptr = unsafe { host_iter_column_readonly(iter, 0) };
            }
        }
    }
    FfiResult::OK
}

#[test]
fn test_change_detection_multi_frame_reliability() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_resource::<ChangeTracker>();
    app.register_mod_component::<Position>();

    // Spawn 10 entities
    for i in 0..10 {
        app.world_mut().spawn(Position {
            x: i as f32,
            y: 0.0,
            z: 0.0,
        });
    }

    let registry = app.world().resource::<ModRegistry>().clone();
    let sim_ctx = Box::into_raw(Box::new(ChangeSimContext { mode: 0 }));

    let dynamic_sys = DynamicCSystem::new(
        "change_sim".to_string(),
        vec![host_runtime::ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        change_sim_callback,
        sim_ctx as *mut c_void,
        Arc::new(registry),
    );

    app.add_systems(Update, (dynamic_sys, track_changed_positions).chain());

    // Frame 1: Spawned entities are detected as added/changed by Bevy
    app.update();
    assert_eq!(app.world().resource::<ChangeTracker>().changed_count, 10);

    // Frame 2: mode = 0 (no mutation). Downstream query MUST observe 0 changed entities!
    unsafe {
        (*sim_ctx).mode = 0;
    }
    app.update();
    assert_eq!(
        app.world().resource::<ChangeTracker>().changed_count,
        0,
        "Frame without mutation must produce 0 Changed<Position> events"
    );

    // Frame 3: mode = 1 (mutate all via host_iter_column_mutable). MUST observe 10 changed entities!
    unsafe {
        (*sim_ctx).mode = 1;
    }
    app.update();
    assert_eq!(
        app.world().resource::<ChangeTracker>().changed_count,
        10,
        "Frame with host_iter_column_mutable must trigger Changed<Position> on all 10 entities"
    );

    // Frame 4: mode = 0 (no mutation again). MUST observe 0 changed entities!
    unsafe {
        (*sim_ctx).mode = 0;
    }
    app.update();
    assert_eq!(
        app.world().resource::<ChangeTracker>().changed_count,
        0,
        "Subsequent non-mutating frame must cleanly reset Changed<Position> to 0"
    );

    // Frame 5: mode = 2 (fine-grained mark rows 3 and 7 only). MUST observe EXACTLY 2 changed entities!
    unsafe {
        (*sim_ctx).mode = 2;
    }
    app.update();
    assert_eq!(
        app.world().resource::<ChangeTracker>().changed_count,
        2,
        "host_iter_mark_changed on rows 3 and 7 must trigger Changed<Position> on exactly 2 entities"
    );

    unsafe {
        drop(Box::from_raw(sim_ctx));
    }
}

#[test]
fn test_change_detection_multi_table_archetype_isolation() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_resource::<ChangeTracker>();
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<Health>();

    // Table A (Position + Velocity): 5 entities
    for i in 0..5 {
        app.world_mut().spawn((
            Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 1.0,
                z: 1.0,
            },
        ));
    }

    // Table B (Position + Health): 5 entities
    for i in 0..5 {
        app.world_mut().spawn((
            Position {
                x: 100.0 + i as f32,
                y: 0.0,
                z: 0.0,
            },
            Health {
                current: 100,
                max: 100,
            },
        ));
    }

    let registry = app.world().resource::<ModRegistry>().clone();
    let sim_ctx = Box::into_raw(Box::new(ChangeSimContext { mode: 0 }));

    // System queries Position (WRITE) and Velocity (READ).
    // MUST match ONLY Table A! Table B must not be matched or modified.
    let dynamic_sys = DynamicCSystem::new(
        "kinematics_mutator".to_string(),
        vec![
            host_runtime::ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            host_runtime::ComponentAccessDescOwned {
                name: "Velocity".to_string(),
                mode: ComponentAccessMode::READ,
            },
        ],
        change_sim_callback,
        sim_ctx as *mut c_void,
        Arc::new(registry),
    );

    app.add_systems(Update, (dynamic_sys, track_changed_positions).chain());

    // Frame 1: Flush initial spawns
    app.update();
    assert_eq!(app.world().resource::<ChangeTracker>().changed_count, 10);

    // Frame 2: Idle frame
    unsafe {
        (*sim_ctx).mode = 0;
    }
    app.update();
    assert_eq!(app.world().resource::<ChangeTracker>().changed_count, 0);

    // Frame 3: Mutate matched tables (Table A only)
    unsafe {
        (*sim_ctx).mode = 1;
    }
    app.update();
    assert_eq!(
        app.world().resource::<ChangeTracker>().changed_count,
        5,
        "Only entities in Table A (Position + Velocity) should trigger Changed<Position>"
    );

    unsafe {
        drop(Box::from_raw(sim_ctx));
    }
}

#[test]
fn test_change_detection_read_mode_defensive_guard() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_resource::<ChangeTracker>();
    app.register_mod_component::<Position>();

    app.world_mut().spawn(Position {
        x: 10.0,
        y: 20.0,
        z: 30.0,
    });

    let registry = app.world().resource::<ModRegistry>().clone();

    // Guest callback that attempts to illegally mutate / mark changed on a READ column
    unsafe extern "C" fn illegal_mutator_callback(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        loop {
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }

            // Attempt mutable access on READ column: MUST return null
            let ptr_mut = unsafe { host_iter_column_mutable(iter, 0) };
            assert!(
                ptr_mut.is_null(),
                "host_iter_column_mutable must return null on READ column"
            );

            // Attempt mark changed on READ column: MUST return PERMISSION_DENIED
            let mark_res = unsafe { host_iter_mark_changed(iter, 0, 0) };
            assert_eq!(
                mark_res,
                FfiResult::PERMISSION_DENIED,
                "host_iter_mark_changed must return PERMISSION_DENIED on READ column"
            );
        }
        FfiResult::OK
    }

    let dynamic_sys = DynamicCSystem::new(
        "read_only_sys".to_string(),
        vec![host_runtime::ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::READ,
        }],
        illegal_mutator_callback,
        core::ptr::null_mut(),
        Arc::new(registry),
    );

    app.add_systems(Update, (dynamic_sys, track_changed_positions).chain());

    // Frame 1: Initial spawn
    app.update();
    assert_eq!(app.world().resource::<ChangeTracker>().changed_count, 1);

    // Frame 2: Illegal mutation attempts rejected; Changed<Position> MUST be 0!
    app.update();
    assert_eq!(
        app.world().resource::<ChangeTracker>().changed_count,
        0,
        "Rejected mutation attempts must not update change ticks"
    );
}
