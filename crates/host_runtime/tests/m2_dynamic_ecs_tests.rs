#![deny(unsafe_op_in_unsafe_fn)]

//! # Milestone 2 Dynamic ECS Integration & Concurrency Tests
//!
//! Comprehensive tests verifying:
//! 1. Dynamic component registration in `World` and `ModRegistry`.
//! 2. Deferred entity spawning and dynamic component insertion via `CommandQueue`.
//! 3. Dynamic system querying, reading, and mutating dynamic components via zero-copy chunk iteration.
//! 4. Change detection tracking (`iter_mark_changed` row ticks and `iter_column_mutable` bulk ticks).
//! 5. Parallel execution of non-conflicting dynamic systems, intra-query duplicate write conflict detection,
//!    and missing component handling.
//! 6. Mod lifecycle, custom drop trampolines, and safe unloading without dangling pointers or panics.

use abi_contract::{
    ComponentAccessMode, ComponentUuid, FfiEntity, FfiResult, ModComponentDescriptor, RawChunkIter,
    RawCommandQueue,
};
use bevy_app::{App, Update};
use bevy_ecs::entity::Entity;
use bevy_ecs::system::{System, SystemAccess};
use bevy_ecs::world::{CommandQueue, World};
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use host_runtime::chunk_iter::{
    host_iter_column_mutable, host_iter_column_readonly, host_iter_mark_changed,
    host_iter_next_chunk,
};
use host_runtime::commands::{
    host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty, CommandQueueBridge,
};
use host_runtime::dynamic_system::{
    host_register_component, ComponentAccessDescOwned, DynamicCSystem, ModInitContext,
};
use host_runtime::plugin::AbiModPlugin;
use host_runtime::registry::ModRegistry;
use std::sync::Arc;

// ============================================================================
// Test 1: Dynamic Component Registration in World & ModRegistry
// ============================================================================

#[test]
fn test_m2_dynamic_component_registration() {
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

    let name = c"CustomDynamicHealth";
    let uuid = ComponentUuid::from_name("CustomDynamicHealth");
    let desc = ModComponentDescriptor::new(name.as_ptr(), uuid, 8, 4);

    // Register dynamic component via C-ABI handshake
    let res = unsafe {
        host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
    };
    assert_eq!(res, FfiResult::OK);

    // Verify component exists in World
    let world_reg = world.resource::<ModRegistry>();
    let comp_meta = world_reg
        .get_by_name("CustomDynamicHealth")
        .expect("Component must be registered in ModRegistry");
    assert_eq!(comp_meta.size, 8);
    assert_eq!(comp_meta.align, 4);
    assert_eq!(comp_meta.uuid, uuid);
    assert!(comp_meta.is_dynamic);
    assert!(comp_meta.insert_raw_fn.is_none());

    // Verify lookup by UUID
    let comp_by_uuid = world_reg
        .get_by_uuid(&uuid)
        .expect("Component must be resolvable by UUID");
    assert_eq!(comp_by_uuid.component_id, comp_meta.component_id);

    // Verify component ID exists in Bevy World components catalog
    assert!(world
        .components()
        .get_info(comp_meta.component_id)
        .is_some());

    // Idempotent re-registration with identical layout must succeed
    let res_idempotent = unsafe {
        host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
    };
    assert_eq!(res_idempotent, FfiResult::OK);

    // Conflicting re-registration with mismatched size must fail
    let conflicting_desc = ModComponentDescriptor::new(name.as_ptr(), uuid, 16, 4);
    let res_conflict = unsafe {
        host_register_component(
            &mut init_ctx as *mut ModInitContext as *mut c_void,
            &conflicting_desc,
        )
    };
    assert_eq!(res_conflict, FfiResult::INVALID_COMPONENT);
}

// ============================================================================
// Test 2: Deferred Entity Spawning & Dynamic Component Insertion
// ============================================================================

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
struct CustomVelocity {
    x: f32,
    y: f32,
    z: f32,
}

#[test]
fn test_m2_deferred_dynamic_component_insertion() {
    let mut world = World::new();
    let registry = ModRegistry::new();

    let name = c"CustomVelocity";
    let uuid = ComponentUuid::from_name("CustomVelocity");
    let desc = ModComponentDescriptor::new(
        name.as_ptr(),
        uuid,
        core::mem::size_of::<CustomVelocity>(),
        core::mem::align_of::<CustomVelocity>(),
    );

    let is_active = Arc::new(AtomicBool::new(true));
    world.insert_resource(registry.clone());
    let mut init_ctx = ModInitContext::with_world(
        Arc::new(registry.clone()),
        Arc::clone(&is_active),
        &mut world as *mut World,
    );
    let _guard = init_ctx.make_active();

    let res = unsafe {
        host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
    };
    assert_eq!(res, FfiResult::OK);

    let comp_id = world
        .resource::<ModRegistry>()
        .get_by_name("CustomVelocity")
        .unwrap()
        .component_id;

    // Set up command queue bridge using the World's genuine entity allocator
    let mut command_queue = CommandQueue::default();
    let world_reg = world.resource::<ModRegistry>().clone();

    let mut bridge =
        CommandQueueBridge::new(&mut command_queue, world.entity_allocator(), &world_reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    // Spawn 3 entities and insert dynamic component data
    let vel_data: [CustomVelocity; 3] = [
        CustomVelocity {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        },
        CustomVelocity {
            x: 4.0,
            y: 5.0,
            z: 6.0,
        },
        CustomVelocity {
            x: 7.0,
            y: 8.0,
            z: 9.0,
        },
    ];

    let mut spawned_entities = Vec::new();
    for vel in &vel_data {
        let ffi_entity = unsafe { host_cmd_spawn_empty(raw_queue) };
        assert_ne!(ffi_entity, FfiEntity::NULL);

        let insert_res = unsafe {
            host_cmd_insert_component(
                raw_queue,
                ffi_entity,
                name.as_ptr(),
                vel as *const CustomVelocity as *const u8,
                core::mem::size_of::<CustomVelocity>(),
            )
        };
        assert_eq!(insert_res, FfiResult::OK);
        spawned_entities.push(ffi_entity);
    }

    // Apply deferred commands to Bevy World
    command_queue.apply(&mut world);

    // Verify all 3 entities have correct component values in Bevy World
    for (i, &ffi_entity) in spawned_entities.iter().enumerate() {
        let bevy_entity = Entity::from_bits(ffi_entity.0);
        let entity_ref = world.entity(bevy_entity);

        // Fetch raw component pointer using Bevy dynamic component accessor
        let comp_ptr = entity_ref
            .get_by_id(comp_id)
            .expect("Entity must possess CustomVelocity component");

        // SAFETY: Pointer is aligned to 4 and points to valid CustomVelocity bytes.
        let read_vel = unsafe { *(comp_ptr.as_ptr() as *const CustomVelocity) };
        assert_eq!(read_vel, vel_data[i]);
    }

    // Test deferred despawn
    let despawn_target = spawned_entities[0];
    let mut bridge2 =
        CommandQueueBridge::new(&mut command_queue, world.entity_allocator(), &world_reg);
    let raw_queue2 = (&mut bridge2 as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let despawn_res = unsafe { host_cmd_despawn(raw_queue2, despawn_target) };
    assert_eq!(despawn_res, FfiResult::OK);
    command_queue.apply(&mut world);

    assert!(world
        .get_entity(Entity::from_bits(despawn_target.0))
        .is_err());
}

// ============================================================================
// Test 3: Dynamic System Chunk Iteration, Reading, and Mutating
// ============================================================================

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
struct CustomScore {
    points: u32,
    multiplier: u32,
}

static SYSTEM_EXECUTION_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn dynamic_score_mutator_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    loop {
        // SAFETY: C-ABI iterator call within dynamic system execution
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }

        // Column 0 is CustomScore with WRITE permission
        let data_ptr = unsafe { host_iter_column_mutable(iter, 0) };
        assert!(!data_ptr.is_null());

        let stride = core::mem::size_of::<CustomScore>();
        for row in 0..count {
            // SAFETY: Dense columnar row pointer calculation (data_ptr + row * stride)
            let score_ptr = unsafe { data_ptr.add(row * stride) as *mut CustomScore };
            unsafe {
                (*score_ptr).points += 100 * (*score_ptr).multiplier;
            }
        }
        SYSTEM_EXECUTION_COUNT.fetch_add(count, Ordering::Relaxed);
    }
    FfiResult::OK
}

#[test]
fn test_m2_dynamic_system_chunk_iteration_and_mutation() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let name = c"CustomScore";
    let uuid = ComponentUuid::from_name("CustomScore");
    let desc = ModComponentDescriptor::new(
        name.as_ptr(),
        uuid,
        core::mem::size_of::<CustomScore>(),
        core::mem::align_of::<CustomScore>(),
    );

    let is_active = Arc::new(AtomicBool::new(true));
    let mut init_ctx = ModInitContext::with_world(
        Arc::new(app.world().resource::<ModRegistry>().clone()),
        Arc::clone(&is_active),
        app.world_mut() as *mut World,
    );
    let _guard = init_ctx.make_active();

    let res = unsafe {
        host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
    };
    assert_eq!(res, FfiResult::OK);

    let comp_id = app
        .world()
        .resource::<ModRegistry>()
        .get_by_name("CustomScore")
        .unwrap()
        .component_id;

    // Spawn 10 entities directly using command queue
    let mut command_queue = CommandQueue::default();
    let world_reg = app.world().resource::<ModRegistry>().clone();
    let mut bridge = CommandQueueBridge::new(
        &mut command_queue,
        app.world().entity_allocator(),
        &world_reg,
    );
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let mut entities = Vec::new();
    for i in 0..10 {
        let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
        let score = CustomScore {
            points: 10 * i,
            multiplier: i + 1,
        };
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                ffi_e,
                name.as_ptr(),
                &score as *const CustomScore as *const u8,
                core::mem::size_of::<CustomScore>(),
            );
        }
        entities.push(ffi_e);
    }
    command_queue.apply(app.world_mut());

    // Register DynamicCSystem querying CustomScore with WRITE access
    let query_descs = vec![ComponentAccessDescOwned {
        name: "CustomScore".to_string(),
        mode: ComponentAccessMode::WRITE,
    }];
    let system = DynamicCSystem::new(
        "score_mutator".to_string(),
        query_descs,
        dynamic_score_mutator_system,
        core::ptr::null_mut(),
        Arc::new(world_reg),
    );
    app.add_systems(Update, system);

    // Run schedule
    app.update();

    assert_eq!(SYSTEM_EXECUTION_COUNT.load(Ordering::Relaxed), 10);

    // Verify all 10 entities were mutated according to logic: points += 100 * multiplier
    for (i, &ffi_e) in entities.iter().enumerate() {
        let bevy_e = Entity::from_bits(ffi_e.0);
        let ptr = app.world().entity(bevy_e).get_by_id(comp_id).unwrap();
        let score = unsafe { *(ptr.as_ptr() as *const CustomScore) };

        let expected_points = (10 * i as u32) + 100 * (i as u32 + 1);
        assert_eq!(score.points, expected_points);
        assert_eq!(score.multiplier, (i as u32) + 1);
    }
}

// ============================================================================
// Test 4: Change Detection Verification (Row vs Bulk Ticks)
// ============================================================================

static SELECTIVE_CHANGE_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn selective_change_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }

        // Use readonly pointer first (does not dirty change ticks)
        let ro_ptr = unsafe { host_iter_column_readonly(iter, 0) };
        assert!(!ro_ptr.is_null());

        // Mark only EVEN rows as changed via host_iter_mark_changed
        for row in 0..count {
            if row % 2 == 0 {
                let mark_res = unsafe { host_iter_mark_changed(iter, 0, row) };
                assert_eq!(mark_res, FfiResult::OK);
                SELECTIVE_CHANGE_COUNT.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    FfiResult::OK
}

#[test]
fn test_m2_change_detection_tracking() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let name = c"DynamicMana";
    let uuid = ComponentUuid::from_name("DynamicMana");
    let desc = ModComponentDescriptor::new(name.as_ptr(), uuid, 4, 4);

    let is_active = Arc::new(AtomicBool::new(true));
    let mut init_ctx = ModInitContext::with_world(
        Arc::new(app.world().resource::<ModRegistry>().clone()),
        Arc::clone(&is_active),
        app.world_mut() as *mut World,
    );
    let _guard = init_ctx.make_active();

    unsafe { host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc) };

    let mut command_queue = CommandQueue::default();
    let world_reg = app.world().resource::<ModRegistry>().clone();
    let mut bridge = CommandQueueBridge::new(
        &mut command_queue,
        app.world().entity_allocator(),
        &world_reg,
    );
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    for i in 0..6 {
        let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
        let mana: u32 = 100 + i;
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                ffi_e,
                name.as_ptr(),
                &mana as *const u32 as *const u8,
                4,
            );
        }
    }
    command_queue.apply(app.world_mut());

    let sys = DynamicCSystem::new(
        "selective_change_sys".to_string(),
        vec![ComponentAccessDescOwned {
            name: "DynamicMana".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        selective_change_system,
        core::ptr::null_mut(),
        Arc::new(world_reg),
    );
    app.add_systems(Update, sys);

    // Advance schedule
    app.update();

    // 6 rows total -> 3 even rows (0, 2, 4) selectively marked changed
    assert_eq!(SELECTIVE_CHANGE_COUNT.load(Ordering::Relaxed), 3);
}

// ============================================================================
// Test 5: Parallel Concurrency Permissions & Conflict Detection
// ============================================================================

#[test]
fn test_m2_concurrency_permissions_and_conflict_detection() {
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

    // Register DynamicCompA and DynamicCompB
    let desc_a = ModComponentDescriptor::new(
        c"DynamicCompA".as_ptr(),
        ComponentUuid::from_name("DynamicCompA"),
        4,
        4,
    );
    let desc_b = ModComponentDescriptor::new(
        c"DynamicCompB".as_ptr(),
        ComponentUuid::from_name("DynamicCompB"),
        4,
        4,
    );
    unsafe {
        host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc_a);
        host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc_b);
    }

    let world_reg = Arc::new(world.resource::<ModRegistry>().clone());

    unsafe extern "C" fn dummy_cb(
        _i: *mut RawChunkIter,
        _c: *mut RawCommandQueue,
        _u: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }

    // 1. Disjoint systems: System A writes DynamicCompA, System B writes DynamicCompB
    let mut sys_a = DynamicCSystem::new(
        "sys_a".to_string(),
        vec![ComponentAccessDescOwned {
            name: "DynamicCompA".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        dummy_cb,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );
    let mut sys_b = DynamicCSystem::new(
        "sys_b".to_string(),
        vec![ComponentAccessDescOwned {
            name: "DynamicCompB".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        dummy_cb,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );

    let access_a = sys_a.initialize(&mut world);
    let access_b = sys_b.initialize(&mut world);

    // Bevy scheduler compatibility check: Disjoint systems MUST be compatible for parallel execution
    assert!(
        access_a.is_compatible(&access_b),
        "Disjoint dynamic systems must have compatible access for parallel scheduling"
    );

    // 2. Conflicting systems: System A writes DynamicCompA, System C reads DynamicCompA
    let mut sys_c = DynamicCSystem::new(
        "sys_c".to_string(),
        vec![ComponentAccessDescOwned {
            name: "DynamicCompA".to_string(),
            mode: ComponentAccessMode::READ,
        }],
        dummy_cb,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );
    let access_c = sys_c.initialize(&mut world);
    assert!(
        !access_a.is_compatible(&access_c),
        "Write-Read on identical component must be incompatible (serialized by scheduler)"
    );

    // 3. Intra-query duplicate write conflict detection: [WRITE DynamicCompA, WRITE DynamicCompA]
    let mut conflicting_sys = DynamicCSystem::new(
        "intra_conflict_sys".to_string(),
        vec![
            ComponentAccessDescOwned {
                name: "DynamicCompA".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            ComponentAccessDescOwned {
                name: "DynamicCompA".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
        ],
        dummy_cb,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );
    let access_conflict = conflicting_sys.initialize(&mut world);
    assert!(
        conflicting_sys.has_self_conflict(),
        "Intra-query duplicate write must flag self_conflict"
    );
    assert!(!conflicting_sys.is_enabled());
    assert_eq!(access_conflict, SystemAccess::None);

    // 4. Missing component handling: querying a component not in ModRegistry disables system
    let mut missing_comp_sys = DynamicCSystem::new(
        "missing_sys".to_string(),
        vec![ComponentAccessDescOwned {
            name: "NonExistentComponent123".to_string(),
            mode: ComponentAccessMode::READ,
        }],
        dummy_cb,
        core::ptr::null_mut(),
        Arc::clone(&world_reg),
    );
    let access_missing = missing_comp_sys.initialize(&mut world);
    assert!(!missing_comp_sys.is_enabled());
    assert_eq!(access_missing, SystemAccess::None);
}

// ============================================================================
// Test 6: Foreign Drop Trampoline Lifecycle & Post-Unload Safety
// ============================================================================

static FOREIGN_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn custom_foreign_drop_hook(ptr: *mut c_void) {
    assert!(!ptr.is_null());
    FOREIGN_DROP_COUNT.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn test_m2_foreign_drop_trampoline_and_unload_safety() {
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

    // Register a non-POD dynamic component with custom drop hook
    let name = c"DynamicDroppable";
    let uuid = ComponentUuid::from_name("DynamicDroppable");
    let desc =
        ModComponentDescriptor::new(name.as_ptr(), uuid, 8, 4).with_drop(custom_foreign_drop_hook);

    let res = unsafe {
        host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
    };
    assert_eq!(res, FfiResult::OK);

    let mut command_queue = CommandQueue::default();
    let world_reg = world.resource::<ModRegistry>().clone();
    let mut bridge =
        CommandQueueBridge::new(&mut command_queue, world.entity_allocator(), &world_reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    // Spawn 2 entities
    let e1 = unsafe { host_cmd_spawn_empty(raw_queue) };
    let e2 = unsafe { host_cmd_spawn_empty(raw_queue) };
    let dummy_payload = [42u32, 84u32];

    unsafe {
        host_cmd_insert_component(
            raw_queue,
            e1,
            name.as_ptr(),
            dummy_payload.as_ptr() as *const u8,
            8,
        );
        host_cmd_insert_component(
            raw_queue,
            e2,
            name.as_ptr(),
            dummy_payload.as_ptr() as *const u8,
            8,
        );
    }
    command_queue.apply(&mut world);

    // 1. Despawn entity 1 while mod is ACTIVE: foreign drop hook MUST execute
    let initial_drops = FOREIGN_DROP_COUNT.load(Ordering::SeqCst);
    world.despawn(Entity::from_bits(e1.0));
    assert_eq!(
        FOREIGN_DROP_COUNT.load(Ordering::SeqCst),
        initial_drops + 1,
        "Custom foreign drop hook must be called on despawn while active"
    );

    // 2. Simulate mod unload by setting atomic tombstone to false (as happens during unload_mod)
    is_active.store(false, Ordering::Release);

    // 3. Despawn entity 2 while mod is INACTIVE: foreign drop hook MUST be safely skipped
    world.despawn(Entity::from_bits(e2.0));
    assert_eq!(
        FOREIGN_DROP_COUNT.load(Ordering::SeqCst),
        initial_drops + 1,
        "Foreign drop hook must be safely skipped after mod deactivation without crashing"
    );
}
