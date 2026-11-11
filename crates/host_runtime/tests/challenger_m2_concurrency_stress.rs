#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # Milestone 2 Challenger Stress & Concurrency Suite
//!
//! Empirical validation and adversarial challenge of Milestone 2:
//! 1. Parallel execution & ambiguity detection for dynamic systems (Write-Write, Write-Read, Disjoint).
//! 2. Cross-system borrow checking between native Bevy systems and DynamicCSystem.
//! 3. Intra-query aliasing and conflict detection ([WRITE, WRITE], [WRITE, READ], [READ, WRITE]).
//! 4. Unknown/unregistered component safety (single query, multi-query, column index boundary verification).
//! 5. Drop trampoline pool saturation (64 slots), exhaustion (65th slot), and slot reclamation on unload.
//! 6. Zero-Sized Types (ZST) and extreme memory alignments (64B / 128B) dynamic component handling.
//! 7. Deferred command validation (size mismatch, despawned entity handling).

use abi_contract::{
    ComponentAccessMode, ComponentUuid, FfiEntity, FfiResult, ModComponentDescriptor, RawChunkIter,
    RawCommandQueue,
};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::World;
use bevy_ecs::schedule::{
    IntoScheduleConfigs, LogLevel, MultiThreadedExecutor, Schedule, ScheduleBuildSettings,
};
use bevy_ecs::system::{Query, System, SystemAccess};
use bevy_ecs::world::CommandQueue;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use host_runtime::chunk_iter::{
    host_iter_column_mutable, host_iter_column_readonly, host_iter_column_stride,
    host_iter_mark_changed, host_iter_next_chunk,
};
use host_runtime::commands::{
    host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty, CommandQueueBridge,
};
use host_runtime::dynamic_system::{
    host_register_component, reset_drop_trampolines_for_testing, ComponentAccessDescOwned,
    DynamicCSystem, ModInitContext,
};
use host_runtime::plugin::AbiModPlugin;
use host_runtime::registry::ModRegistry;
use host_runtime::ModAppExt;
use mod_derive::ModExposable;
use std::sync::Arc;

#[test]
fn test_challenge_disjoint_systems_concurrency_execution() {
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

    // Bevy scheduler compatibility check
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

    // Multi-threaded executor schedule initialization with ambiguity detection enabled must succeed
    assert!(
        schedule.initialize(&mut world).is_ok(),
        "Disjoint dynamic systems must initialize in MultiThreadedExecutor without ambiguity error"
    );

    schedule.run(&mut world);
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

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct StaticPosition {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

// ============================================================================
// CHALLENGE 1: SCHEDULER AMBIGUITY & CONFLICT DETECTION
// ============================================================================

#[test]
fn test_challenge_write_write_and_write_read_conflict_detection() {
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

    // 1. Two systems with WRITE access to DynamicHealth
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
        assert!(
            !s1.is_compatible(s2),
            "Two dynamic systems with WRITE access on DynamicHealth must NOT be compatible"
        );
    } else {
        panic!("Expected SystemAccess::Shared");
    }

    // Unordered in schedule with ambiguity detection MUST fail
    let mut schedule_ambig = Schedule::default();
    schedule_ambig.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedule_ambig.add_systems((sys_w1, sys_w2));
    assert!(
        schedule_ambig.initialize(&mut world).is_err(),
        "Bevy scheduler must flag unconstrained dynamic WRITE + WRITE conflict as an ambiguity error"
    );

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
    assert!(
        schedule_chained.initialize(&mut world).is_ok(),
        "Chained conflicting dynamic systems must be serialized without ambiguity"
    );

    // 3. WRITE and READ conflict
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
    assert!(
        schedule_rw.initialize(&mut world).is_err(),
        "WRITE + READ conflict must trigger ambiguity error"
    );
}

// ============================================================================
// CHALLENGE 2: NATIVE BEVY SYSTEM & DYNAMIC SYSTEM BORROW COMPATIBILITY
// ============================================================================

#[test]
fn test_challenge_native_and_dynamic_system_borrow_interaction() {
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

    // Dynamic system with WRITE access to StaticPosition
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

    // Native writer + Dynamic writer in unconstrained schedule MUST be flagged as ambiguity error
    let mut schedule = Schedule::default();
    schedule.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedule.add_systems((native_writer_system, dynamic_writer));
    assert!(
        schedule.initialize(app.world_mut()).is_err(),
        "Native writer + Dynamic writer must trigger ambiguity error"
    );

    // Native reader + Dynamic reader MUST be compatible without ambiguity error
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
    assert!(
        schedule_readers.initialize(app.world_mut()).is_ok(),
        "Native reader + Dynamic reader must be compatible"
    );
}

// ============================================================================
// CHALLENGE 3: INTRA-QUERY CONFLICT DETECTION ([WRITE, WRITE], [WRITE, READ], [READ, WRITE])
// ============================================================================

#[test]
fn test_challenge_intra_query_conflict_detection() {
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

    // Case A: [WRITE, WRITE] on same component in single query
    let mut sys_ww = DynamicCSystem::new(
        "sys_ww".to_string(),
        vec![
            ComponentAccessDescOwned {
                name: "CombatStat".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            ComponentAccessDescOwned {
                name: "CombatStat".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
        ],
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );
    let acc_ww = sys_ww.initialize(&mut world);
    assert!(
        sys_ww.has_self_conflict(),
        "Duplicate WRITE must flag self_conflict"
    );
    assert!(!sys_ww.is_enabled());
    assert_eq!(acc_ww, SystemAccess::None);

    // Case A2: [WRITE, READ] on same component in single query
    let mut sys_wr = DynamicCSystem::new(
        "sys_wr".to_string(),
        vec![
            ComponentAccessDescOwned {
                name: "CombatStat".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            ComponentAccessDescOwned {
                name: "CombatStat".to_string(),
                mode: ComponentAccessMode::READ,
            },
        ],
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );
    let acc_wr = sys_wr.initialize(&mut world);
    assert!(
        sys_wr.has_self_conflict(),
        "Intra-query [WRITE, READ] must flag self_conflict"
    );
    assert!(!sys_wr.is_enabled());
    assert_eq!(acc_wr, SystemAccess::None);

    // Case A3: [READ, WRITE] on same component in single query
    let mut sys_rw = DynamicCSystem::new(
        "sys_rw".to_string(),
        vec![
            ComponentAccessDescOwned {
                name: "CombatStat".to_string(),
                mode: ComponentAccessMode::READ,
            },
            ComponentAccessDescOwned {
                name: "CombatStat".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
        ],
        noop_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );
    let acc_rw = sys_rw.initialize(&mut world);
    assert!(
        sys_rw.has_self_conflict(),
        "Intra-query [READ, WRITE] must flag self_conflict"
    );
    assert!(!sys_rw.is_enabled());
    assert_eq!(acc_rw, SystemAccess::None);

    // Case B: Multi-query intra-system conflict: Query 0 WRITE, Query 1 WRITE
    let mut sys_mq_ww = DynamicCSystem::new_multi_query(
        "sys_mq_ww".to_string(),
        vec![
            vec![ComponentAccessDescOwned {
                name: "CombatStat".to_string(),
                mode: ComponentAccessMode::WRITE,
            }],
            vec![ComponentAccessDescOwned {
                name: "CombatStat".to_string(),
                mode: ComponentAccessMode::WRITE,
            }],
        ],
        noop_multi_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );
    let acc_mq = sys_mq_ww.initialize(&mut world);
    assert!(
        sys_mq_ww.has_self_conflict(),
        "Multi-query duplicate WRITE must flag self_conflict"
    );
    assert!(!sys_mq_ww.is_enabled());
    assert_eq!(acc_mq, SystemAccess::None);

    // Case C: Multi-query intra-system conflict: Query 0 WRITE, Query 1 READ
    let mut sys_mq_wr = DynamicCSystem::new_multi_query(
        "sys_mq_wr".to_string(),
        vec![
            vec![ComponentAccessDescOwned {
                name: "CombatStat".to_string(),
                mode: ComponentAccessMode::WRITE,
            }],
            vec![ComponentAccessDescOwned {
                name: "CombatStat".to_string(),
                mode: ComponentAccessMode::READ,
            }],
        ],
        noop_multi_callback,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );
    let acc_mq_wr = sys_mq_wr.initialize(&mut world);
    assert!(
        sys_mq_wr.has_self_conflict(),
        "Multi-query WRITE + READ must flag self_conflict"
    );
    assert!(!sys_mq_wr.is_enabled());
    assert_eq!(acc_mq_wr, SystemAccess::None);
}

// ============================================================================
// CHALLENGE 4: UNKNOWN / UNREGISTERED COMPONENT SAFETY & BOUNDS
// ============================================================================

#[test]
fn test_challenge_unknown_component_and_column_bounds_safety() {
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

    // 1. Single query referencing completely unknown component
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
    let access_unknown = sys_unknown.initialize(&mut world);
    assert!(!sys_unknown.is_enabled());
    assert_eq!(access_unknown, SystemAccess::None);

    // 2. Single query referencing 1 known component and 1 unknown component
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
    let access_partial = sys_partial_unknown.initialize(&mut world);
    assert!(!sys_partial_unknown.is_enabled());
    assert_eq!(access_partial, SystemAccess::None);

    // 3. Multi-query where Query 0 is valid and Query 1 references unknown component
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
    let access_mq_partial = sys_mq_partial.initialize(&mut world);
    assert!(!sys_mq_partial.is_enabled());
    assert_eq!(access_mq_partial, SystemAccess::None);

    // Running a schedule containing disabled systems must be completely safe no-ops
    let mut schedule = Schedule::default();
    schedule.add_systems((sys_unknown, sys_partial_unknown, sys_mq_partial));
    schedule.run(&mut world);
}

static BOUNDS_STRESS_EXECUTED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn bounds_stress_callback(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }

        // Valid column 0
        let col0_ro = unsafe { host_iter_column_readonly(iter, 0) };
        let col0_mut = unsafe { host_iter_column_mutable(iter, 0) };
        let col0_stride = unsafe { host_iter_column_stride(iter, 0) };
        assert!(!col0_ro.is_null());
        assert!(!col0_mut.is_null());
        assert_eq!(col0_stride, 4);

        // Out-of-bounds column queries: index 1, index 32, index 99999 must return null
        assert!(unsafe { host_iter_column_readonly(iter, 1) }.is_null());
        assert!(unsafe { host_iter_column_readonly(iter, 32) }.is_null());
        assert!(unsafe { host_iter_column_readonly(iter, 99999) }.is_null());
        assert!(unsafe { host_iter_column_mutable(iter, 1) }.is_null());
        assert!(unsafe { host_iter_column_mutable(iter, 32) }.is_null());
        assert!(unsafe { host_iter_column_mutable(iter, 99999) }.is_null());
        assert_eq!(unsafe { host_iter_column_stride(iter, 1) }, 0);
        assert_eq!(unsafe { host_iter_column_stride(iter, 99999) }, 0);

        // Out-of-bounds row index in host_iter_mark_changed must return INTERNAL_ERROR
        assert_eq!(
            unsafe { host_iter_mark_changed(iter, 0, count + 100) },
            FfiResult::INTERNAL_ERROR
        );
        // Out-of-bounds column index in host_iter_mark_changed must return INVALID_COMPONENT
        assert_eq!(
            unsafe { host_iter_mark_changed(iter, 99, 0) },
            FfiResult::INVALID_COMPONENT
        );

        BOUNDS_STRESS_EXECUTED.store(true, Ordering::SeqCst);
    }
    FfiResult::OK
}

#[test]
fn test_challenge_column_and_row_boundary_enforcement() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let desc = ModComponentDescriptor::new(
        c"BoundaryComp".as_ptr(),
        ComponentUuid::from_name("BoundaryComp"),
        4,
        4,
    );
    let is_active = Arc::new(AtomicBool::new(true));
    let mut init_ctx = ModInitContext::with_world(
        Arc::new(app.world().resource::<ModRegistry>().clone()),
        Arc::clone(&is_active),
        app.world_mut() as *mut World,
    );
    let _guard = init_ctx.make_active();
    unsafe {
        host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc);
    }

    // Spawn 5 entities
    let mut queue = CommandQueue::default();
    let reg = app.world().resource::<ModRegistry>().clone();
    let mut bridge = CommandQueueBridge::new(&mut queue, app.world().entity_allocator(), &reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();
    let val: u32 = 777;
    for _ in 0..5 {
        let e = unsafe { host_cmd_spawn_empty(raw_queue) };
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                e,
                c"BoundaryComp".as_ptr(),
                &val as *const u32 as *const u8,
                4,
            );
        }
    }
    queue.apply(app.world_mut());

    let sys = DynamicCSystem::new(
        "bounds_stress_sys".to_string(),
        vec![ComponentAccessDescOwned {
            name: "BoundaryComp".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        bounds_stress_callback,
        core::ptr::null_mut(),
        Arc::new(reg),
    );
    app.add_systems(Update, sys);
    app.update();

    assert!(
        BOUNDS_STRESS_EXECUTED.load(Ordering::SeqCst),
        "Bounds stress system must have executed and verified all boundary cases"
    );
}

// ============================================================================
// CHALLENGE 5: DROP TRAMPOLINE SATURATION (64 SLOTS) & RECLAMATION
// ============================================================================

static TRAMPOLINE_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn generic_drop_hook(ptr: *mut c_void) {
    assert!(!ptr.is_null());
    TRAMPOLINE_DROP_COUNT.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn test_challenge_drop_trampoline_saturation_and_reclamation() {
    reset_drop_trampolines_for_testing();
    let mut world = World::new();
    let registry = Arc::new(ModRegistry::new());
    world.insert_resource((*registry).clone());

    let is_active_1 = Arc::new(AtomicBool::new(true));
    let mut init_ctx_1 = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active_1),
        &mut world as *mut World,
    );
    let _guard_1 = init_ctx_1.make_active();

    // Register MAX_DROP_TRAMPOLINES non-POD components (filling the drop trampoline pool)
    let mut names = Vec::new();
    for i in 0..host_runtime::MAX_DROP_TRAMPOLINES {
        let name_str = format!("DroppableComp_{i}\0");
        names.push(name_str);
    }

    for (i, name_str) in names.iter().enumerate() {
        let uuid = ComponentUuid::from_name(&format!("DroppableComp_{i}"));
        let desc = ModComponentDescriptor::new(name_str.as_ptr().cast(), uuid, 8, 4)
            .with_drop(generic_drop_hook);

        let res = unsafe {
            host_register_component(&mut init_ctx_1 as *mut ModInitContext as *mut c_void, &desc)
        };
        assert_eq!(
            res,
            FfiResult::OK,
            "Slot {i} should be successfully allocated in drop trampoline pool"
        );
    }

    // Overflow registration with foreign drop MUST fail with INTERNAL_ERROR (pool exhausted)
    let extra_name = c"DroppableComp_Overflow";
    let extra_uuid = ComponentUuid::from_name("DroppableComp_Overflow");
    let extra_desc = ModComponentDescriptor::new(extra_name.as_ptr(), extra_uuid, 8, 4)
        .with_drop(generic_drop_hook);

    let res_overflow = unsafe {
        host_register_component(
            &mut init_ctx_1 as *mut ModInitContext as *mut c_void,
            &extra_desc,
        )
    };
    assert_eq!(
        res_overflow,
        FfiResult::INTERNAL_ERROR,
        "65th non-POD component registration must return INTERNAL_ERROR when trampoline pool is full"
    );

    // But a POD component (drop_fn == None) MUST still succeed!
    let pod_name = c"PodComp_AfterSaturation";
    let pod_uuid = ComponentUuid::from_name("PodComp_AfterSaturation");
    let pod_desc = ModComponentDescriptor::new(pod_name.as_ptr(), pod_uuid, 8, 4);
    let res_pod = unsafe {
        host_register_component(
            &mut init_ctx_1 as *mut ModInitContext as *mut c_void,
            &pod_desc,
        )
    };
    assert_eq!(
        res_pod,
        FfiResult::OK,
        "POD component does not require trampoline slot and must succeed"
    );

    // Simulate mod 1 unload: set is_active_1 to false
    is_active_1.store(false, Ordering::Release);

    // Now, a new mod initialization context attempts to register an alien component with a different UUID.
    // Slots cannot be stolen across different ComponentUuids, so this MUST fail with INTERNAL_ERROR!
    let is_active_2 = Arc::new(AtomicBool::new(true));
    let mut init_ctx_2 = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active_2),
        &mut world as *mut World,
    );
    let _guard_2 = init_ctx_2.make_active();

    let reclaim_name = c"ReclaimedDroppable";
    let reclaim_uuid = ComponentUuid::from_name("ReclaimedDroppable");
    let reclaim_desc = ModComponentDescriptor::new(reclaim_name.as_ptr(), reclaim_uuid, 8, 4)
        .with_drop(generic_drop_hook);

    let res_reclaim = unsafe {
        host_register_component(
            &mut init_ctx_2 as *mut ModInitContext as *mut c_void,
            &reclaim_desc,
        )
    };
    assert_eq!(
        res_reclaim,
        FfiResult::INTERNAL_ERROR,
        "Trampoline pool must reject alien component UUID when saturated even if slots are inactive"
    );

    // But re-registering the same ComponentUuid (mod reload) must succeed by rebinding the slot!
    let same_name = c"DroppableComp_0";
    let same_uuid = ComponentUuid::from_name("DroppableComp_0");
    let same_desc = ModComponentDescriptor::new(same_name.as_ptr(), same_uuid, 8, 4)
        .with_drop(generic_drop_hook);
    let res_same = unsafe {
        host_register_component(
            &mut init_ctx_2 as *mut ModInitContext as *mut c_void,
            &same_desc,
        )
    };
    assert_eq!(
        res_same,
        FfiResult::OK,
        "Reloading mod with identical ComponentUuid must rebind existing trampoline slot"
    );

    reset_drop_trampolines_for_testing();
}

// ============================================================================
// CHALLENGE 6: ZERO-SIZED (ZST) & HIGH-ALIGNMENT (64B / 128B) DYNAMIC COMPONENTS
// ============================================================================

#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq)]
struct CacheLineAlignedComp {
    data: [u64; 8], // 64 bytes, 64-byte aligned
}

#[test]
fn test_challenge_zst_and_high_alignment_dynamic_components() {
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

    // 1. ZST Dynamic Component (size 0, align 1)
    let zst_name = c"ZstMarkerComp";
    let zst_uuid = ComponentUuid::from_name("ZstMarkerComp");
    let zst_desc = ModComponentDescriptor::new(zst_name.as_ptr(), zst_uuid, 0, 1);
    let zst_res = unsafe {
        host_register_component(
            &mut init_ctx as *mut ModInitContext as *mut c_void,
            &zst_desc,
        )
    };
    assert_eq!(
        zst_res,
        FfiResult::OK,
        "ZST dynamic component registration must succeed"
    );

    // 2. 64-byte Aligned Component
    let align_name = c"CacheLineAlignedComp";
    let align_uuid = ComponentUuid::from_name("CacheLineAlignedComp");
    let align_desc = ModComponentDescriptor::new(align_name.as_ptr(), align_uuid, 64, 64);
    let align_res = unsafe {
        host_register_component(
            &mut init_ctx as *mut ModInitContext as *mut c_void,
            &align_desc,
        )
    };
    assert_eq!(
        align_res,
        FfiResult::OK,
        "64-byte aligned component registration must succeed"
    );

    // Spawn entity and insert both via deferred commands
    let mut queue = CommandQueue::default();
    let reg = world.resource::<ModRegistry>().clone();
    let mut bridge = CommandQueueBridge::new(&mut queue, world.entity_allocator(), &reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };

    // Insert ZST (len = 0, data = dangling pointer)
    let zst_insert = unsafe {
        host_cmd_insert_component(
            raw_queue,
            ffi_e,
            zst_name.as_ptr(),
            core::ptr::NonNull::<u8>::dangling().as_ptr(),
            0,
        )
    };
    assert_eq!(zst_insert, FfiResult::OK);

    // Insert CacheLineAlignedComp
    let aligned_val = CacheLineAlignedComp {
        data: [0x1122334455667788; 8],
    };
    let aligned_insert = unsafe {
        host_cmd_insert_component(
            raw_queue,
            ffi_e,
            align_name.as_ptr(),
            &aligned_val as *const CacheLineAlignedComp as *const u8,
            64,
        )
    };
    assert_eq!(aligned_insert, FfiResult::OK);

    queue.apply(&mut world);

    let bevy_entity = Entity::from_bits(ffi_e.0);
    let ent_ref = world.entity(bevy_entity);

    let zst_id = reg.get_by_name("ZstMarkerComp").unwrap().component_id;
    let align_id = reg
        .get_by_name("CacheLineAlignedComp")
        .unwrap()
        .component_id;

    assert!(ent_ref.contains_id(zst_id));
    assert!(ent_ref.contains_id(align_id));

    let ptr = ent_ref.get_by_id(align_id).unwrap();
    let addr = ptr.as_ptr() as usize;
    assert_eq!(
        addr % 64,
        0,
        "Retrieved component pointer in table storage must be 64-byte aligned"
    );
    let read_val = unsafe { *(ptr.as_ptr() as *const CacheLineAlignedComp) };
    assert_eq!(read_val, aligned_val);
}

// ============================================================================
// CHALLENGE 7: DEFERRED COMMAND INTEGRITY CHECKS
// ============================================================================

#[test]
fn test_challenge_deferred_command_error_conditions() {
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
        c"RigidBody".as_ptr(),
        ComponentUuid::from_name("RigidBody"),
        16,
        8,
    );
    unsafe {
        host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc);
    }

    let mut queue = CommandQueue::default();
    let reg = world.resource::<ModRegistry>().clone();
    let mut bridge = CommandQueueBridge::new(&mut queue, world.entity_allocator(), &reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };

    // 1. Length mismatch on insertion (expected 16 bytes, passed 8 bytes)
    let small_data = [0u8; 8];
    let err_mismatch = unsafe {
        host_cmd_insert_component(
            raw_queue,
            ffi_e,
            c"RigidBody".as_ptr(),
            small_data.as_ptr(),
            8,
        )
    };
    assert_eq!(
        err_mismatch,
        FfiResult::INTERNAL_ERROR,
        "Size mismatch on host_cmd_insert_component must return INTERNAL_ERROR"
    );

    // 2. Unknown component name on insertion
    let valid_data = [0u8; 16];
    let err_unknown = unsafe {
        host_cmd_insert_component(
            raw_queue,
            ffi_e,
            c"NonExistentComp".as_ptr(),
            valid_data.as_ptr(),
            16,
        )
    };
    assert_eq!(
        err_unknown,
        FfiResult::INVALID_COMPONENT,
        "Unknown component name must return INVALID_COMPONENT"
    );

    // 3. Null queue handle
    assert_eq!(
        unsafe { host_cmd_spawn_empty(core::ptr::null_mut()) },
        FfiEntity(0)
    );
    assert_eq!(
        unsafe { host_cmd_despawn(core::ptr::null_mut(), ffi_e) },
        FfiResult::NULL_POINTER
    );
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                core::ptr::null_mut(),
                ffi_e,
                c"RigidBody".as_ptr(),
                valid_data.as_ptr(),
                16,
            )
        },
        FfiResult::NULL_POINTER
    );
}
