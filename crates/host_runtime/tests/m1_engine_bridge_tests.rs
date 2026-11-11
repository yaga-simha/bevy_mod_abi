#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use abi_contract::{
    ComponentAccessMode, ComponentUuid, ExposableComponent, FfiEntity, FfiResult, HostApi,
    ModComponentDescriptor, RawChunkIter, RawCommandQueue,
};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::prelude::World;
use bevy_ecs::query::Changed;
use bevy_ecs::schedule::IntoScheduleConfigs;
use bevy_ecs::system::System;
use core::ffi::c_void;
use host_runtime::{
    host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty, host_get_proc_address,
    host_iter_column_mutable, host_iter_column_readonly, host_iter_column_stride,
    host_iter_entities, host_iter_mark_changed, host_iter_next_chunk, host_register_component,
    AbiModPlugin, CommandQueueBridge, DynamicCSystem, HostChunkIter, ModAppExt, ModRegistry,
    QueryColumnDesc, HOST_API,
};
use mod_derive::ModExposable;
use std::ffi::CString;
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

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Armor {
    pub defense: i32,
}

// ============================================================================
// 1. ModRegistry Tests
// ============================================================================

#[test]
fn test_mod_registry_registration_and_lookup() {
    let mut world = World::new();
    let pos_id = world.register_component::<Position>();
    let vel_id = world.register_component::<Velocity>();

    let mut registry = ModRegistry::new();
    registry.register_component::<Position>(pos_id);
    registry.register_component::<Velocity>(vel_id);

    // Lookup by ID
    let pos_meta = registry
        .get_by_id(pos_id)
        .expect("Position should be found by ID");
    assert_eq!(pos_meta.name, "Position");
    assert_eq!(pos_meta.size, core::mem::size_of::<Position>());
    assert_eq!(pos_meta.align, core::mem::align_of::<Position>());
    assert_eq!(pos_meta.vtable_ptr, Position::erased_vtable_ptr());

    // Lookup by Name
    let pos_by_name = registry
        .get_by_name("Position")
        .expect("Position found by name");
    assert_eq!(pos_by_name.component_id, pos_id);

    // Lookup by UUID
    let pos_by_uuid = registry
        .get_by_uuid(&Position::UUID)
        .expect("Position found by UUID");
    assert_eq!(pos_by_uuid.component_id, pos_id);

    // Symbol resolution
    assert_eq!(
        registry.resolve_symbol("Position"),
        Some(Position::erased_vtable_ptr())
    );
    assert_eq!(
        registry.resolve_symbol("Position_vtable"),
        Some(Position::erased_vtable_ptr())
    );
    assert!(registry.resolve_symbol("Position_get_x").is_some());
    assert!(registry.resolve_symbol("NonExistent").is_none());
}

#[test]
fn test_host_get_proc_address_scoped_and_global() {
    let mut world = World::new();
    let health_id = world.register_component::<Health>();

    let mut registry = ModRegistry::new();
    registry.register_component::<Health>(health_id);
    let reg_arc = Arc::new(registry);

    let name_c = CString::new("Health_vtable").unwrap();
    let get_x_c = CString::new("Health_get_current").unwrap();
    let invalid_c = CString::new("Unknown_symbol").unwrap();

    // 1. Scoped thread-local lookup
    {
        let _guard = reg_arc.make_current();
        let ptr = unsafe { host_get_proc_address(name_c.as_ptr()) };
        assert_eq!(ptr, Health::erased_vtable_ptr());

        let fn_ptr = unsafe { host_get_proc_address(get_x_c.as_ptr()) };
        assert!(!fn_ptr.is_null());

        let null_ptr = unsafe { host_get_proc_address(invalid_c.as_ptr()) };
        assert!(null_ptr.is_null());

        let null_arg = unsafe { host_get_proc_address(core::ptr::null()) };
        assert!(null_arg.is_null());
    }

    // 2. Global fallback lookup (global symbols were populated on registration)
    let global_ptr = unsafe { host_get_proc_address(name_c.as_ptr()) };
    assert_eq!(global_ptr, Health::erased_vtable_ptr());
}

// ============================================================================
// 2. CommandQueueBridge Tests
// ============================================================================

#[test]
fn test_command_queue_bridge_spawn_insert_despawn() {
    let mut world = World::new();
    let pos_id = world.register_component::<Position>();

    let mut registry = ModRegistry::new();
    registry.register_component::<Position>(pos_id);

    let mut cmd_queue = bevy_ecs::world::CommandQueue::default();
    let pos_data = Position {
        x: 10.0,
        y: 20.0,
        z: 30.0,
    };
    let comp_name = CString::new("Position").unwrap();

    // 1. Queue spawn and insert
    let ffi_entity = {
        let cell = world.as_unsafe_world_cell();
        let allocator = cell.entity_allocator();
        let mut bridge = CommandQueueBridge::new(&mut cmd_queue, allocator, &registry);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        let entity = unsafe { host_cmd_spawn_empty(raw_queue) };
        assert_ne!(entity.0, 0);

        let status = unsafe {
            host_cmd_insert_component(
                raw_queue,
                entity,
                comp_name.as_ptr(),
                &pos_data as *const Position as *const u8,
                core::mem::size_of::<Position>(),
            )
        };
        assert_eq!(status, FfiResult::OK);
        entity
    };

    // Before apply, entity does not exist in world
    let bevy_entity = bevy_ecs::entity::Entity::from_bits(ffi_entity.to_bits());
    assert!(world.get_entity(bevy_entity).is_err());

    // 2. Flush command queue
    cmd_queue.apply(&mut world);

    // Verify entity was spawned with Position component
    assert!(world.get_entity(bevy_entity).is_ok());
    let fetched_pos = world
        .get::<Position>(bevy_entity)
        .expect("Position component inserted");
    assert_eq!(*fetched_pos, pos_data);

    // 3. Queue despawn
    {
        let cell = world.as_unsafe_world_cell();
        let allocator = cell.entity_allocator();
        let mut bridge2 = CommandQueueBridge::new(&mut cmd_queue, allocator, &registry);
        let raw_queue2 = (&mut bridge2 as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();
        let despawn_status = unsafe { host_cmd_despawn(raw_queue2, ffi_entity) };
        assert_eq!(despawn_status, FfiResult::OK);
    }

    cmd_queue.apply(&mut world);
    assert!(world.get_entity(bevy_entity).is_err());
}

#[test]
fn test_command_queue_bridge_defensive_validation() {
    let mut world = World::new();
    let registry = ModRegistry::new();
    let mut cmd_queue = bevy_ecs::world::CommandQueue::default();
    let cell = world.as_unsafe_world_cell();
    let mut bridge = CommandQueueBridge::new(&mut cmd_queue, cell.entity_allocator(), &registry);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let comp_name = CString::new("Position").unwrap();
    let dummy = [0u8; 12];

    // Null queue pointer
    assert_eq!(
        unsafe { host_cmd_spawn_empty(core::ptr::null_mut()) },
        FfiEntity(0)
    );
    assert_eq!(
        unsafe { host_cmd_despawn(core::ptr::null_mut(), FfiEntity(1)) },
        FfiResult::NULL_POINTER
    );
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                core::ptr::null_mut(),
                FfiEntity(1),
                comp_name.as_ptr(),
                dummy.as_ptr(),
                12,
            )
        },
        FfiResult::NULL_POINTER
    );

    // Null component name
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                FfiEntity(1),
                core::ptr::null(),
                dummy.as_ptr(),
                12,
            )
        },
        FfiResult::NULL_POINTER
    );

    // Unregistered component name
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                FfiEntity(1),
                comp_name.as_ptr(),
                dummy.as_ptr(),
                12,
            )
        },
        FfiResult::INVALID_COMPONENT
    );
}

// ============================================================================
// 3. HostChunkIter & RawChunkIter Tests
// ============================================================================

#[test]
fn test_host_chunk_iter_traversal_and_permissions() {
    let mut world = World::new();
    let pos_id = world.register_component::<Position>();
    let vel_id = world.register_component::<Velocity>();
    let _armor_id = world.register_component::<Armor>();

    // Spawn 10 entities in Table 1 (Position + Velocity)
    for i in 0..10 {
        world.spawn((
            Position {
                x: i as f32,
                y: i as f32 * 2.0,
                z: i as f32 * 3.0,
            },
            Velocity {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            },
        ));
    }

    // Spawn 5 entities in Table 2 (Position + Velocity + Armor)
    for i in 0..5 {
        world.spawn((
            Position {
                x: 100.0 + i as f32,
                y: 200.0,
                z: 300.0,
            },
            Velocity {
                x: 0.1,
                y: 0.2,
                z: 0.3,
            },
            Armor { defense: 50 },
        ));
    }

    // Spawn 3 entities in Table 3 (Position only, should not match query)
    for i in 0..3 {
        world.spawn(Position {
            x: 500.0 + i as f32,
            y: 0.0,
            z: 0.0,
        });
    }

    let cell = world.as_unsafe_world_cell();
    let storages = unsafe { cell.storages() };

    // Query: Position (WRITE), Velocity (READ)
    let query_columns = vec![
        QueryColumnDesc {
            component_id: pos_id,
            mode: ComponentAccessMode::WRITE,
            stride: core::mem::size_of::<Position>(),
        },
        QueryColumnDesc {
            component_id: vel_id,
            mode: ComponentAccessMode::READ,
            stride: core::mem::size_of::<Velocity>(),
        },
    ];

    // Collect matching tables from archetypes
    let mut matched_tables = Vec::new();
    for arch in cell.archetypes().iter() {
        if arch.contains(pos_id) && arch.contains(vel_id) {
            let tid = arch.table_id();
            if !matched_tables.contains(&tid) {
                matched_tables.push(tid);
            }
        }
    }
    assert_eq!(matched_tables.len(), 2);

    let mut host_iter = HostChunkIter::new(
        &storages.tables,
        &matched_tables,
        &query_columns,
        cell.change_tick(),
    );
    let raw_iter = (&mut host_iter as *mut HostChunkIter<'_>).cast::<RawChunkIter>();

    let mut total_entities = 0;
    loop {
        let count = unsafe { host_iter_next_chunk(raw_iter) };
        if count == 0 {
            break;
        }
        total_entities += count;

        // Check entities slice
        let entities_ptr = unsafe { host_iter_entities(raw_iter) };
        assert!(!entities_ptr.is_null());

        // Check column 0 (Position: WRITE)
        let pos_mut = unsafe { host_iter_column_mutable(raw_iter, 0) };
        assert!(!pos_mut.is_null());
        let stride_0 = unsafe { host_iter_column_stride(raw_iter, 0) };
        assert_eq!(stride_0, core::mem::size_of::<Position>());

        // Check column 1 (Velocity: READ)
        let vel_ro = unsafe { host_iter_column_readonly(raw_iter, 1) };
        assert!(!vel_ro.is_null());
        let stride_1 = unsafe { host_iter_column_stride(raw_iter, 1) };
        assert_eq!(stride_1, core::mem::size_of::<Velocity>());

        // Defensive check: requesting mutable pointer on READ column MUST return null_mut
        let vel_mut_denied = unsafe { host_iter_column_mutable(raw_iter, 1) };
        assert!(
            vel_mut_denied.is_null(),
            "READ column must not allow mutable access"
        );

        // Mark individual row 0 changed
        let mark_res = unsafe { host_iter_mark_changed(raw_iter, 0, 0) };
        assert_eq!(mark_res, FfiResult::OK);

        // Mutate position of entity 0 using byte stride
        let pos = unsafe { &mut *(pos_mut as *mut Position) };
        pos.x += 1000.0;
    }

    assert_eq!(total_entities, 15);
}

// ============================================================================
// 4. DynamicCSystem Execution & Scheduler Verification
// ============================================================================

unsafe extern "C" fn kinematics_guest_system(
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
                p.z += v.z;
            }
        }
    }
    FfiResult::OK
}

#[test]
fn test_dynamic_c_system_in_bevy_schedule() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    // Spawn 100 entities
    for i in 0..100 {
        app.world_mut().spawn((
            Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            },
        ));
    }

    // Register DynamicCSystem
    let registry = app.world().resource::<ModRegistry>().clone();
    let query_descs = vec![
        host_runtime::ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::WRITE,
        },
        host_runtime::ComponentAccessDescOwned {
            name: "Velocity".to_string(),
            mode: ComponentAccessMode::READ,
        },
    ];

    let dynamic_sys = DynamicCSystem::new(
        "kinematics".to_string(),
        query_descs,
        kinematics_guest_system,
        core::ptr::null_mut(),
        Arc::new(registry),
    );

    app.add_systems(Update, dynamic_sys);

    // Run one update tick
    app.update();

    // Verify all 100 entities were updated by the guest C-ABI system
    let mut query = app.world_mut().query::<(&Position, &Velocity)>();
    for (pos, vel) in query.iter(app.world()) {
        assert_eq!(pos.y, 2.0);
        assert_eq!(pos.z, 3.0);
        assert_eq!(vel.x, 1.0);
    }
}

// ============================================================================
// 5. Change Detection Soundness Test
// ============================================================================

#[derive(bevy_ecs::prelude::Resource, Default)]
struct ChangedCounter(usize);

fn count_changed_positions(
    query: bevy_ecs::system::Query<&Position, Changed<Position>>,
    mut counter: bevy_ecs::prelude::ResMut<ChangedCounter>,
) {
    counter.0 = query.iter().count();
}

#[test]
fn test_change_detection_with_native_bevy_query() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_resource::<ChangedCounter>();
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    for i in 0..20 {
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

    let registry = app.world().resource::<ModRegistry>().clone();
    let query_descs = vec![
        host_runtime::ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::WRITE,
        },
        host_runtime::ComponentAccessDescOwned {
            name: "Velocity".to_string(),
            mode: ComponentAccessMode::READ,
        },
    ];

    let dynamic_sys = DynamicCSystem::new(
        "kinematics_mutator".to_string(),
        query_descs,
        kinematics_guest_system,
        core::ptr::null_mut(),
        Arc::new(registry),
    );

    // Chain dynamic mod system followed by native Bevy observer system
    app.add_systems(Update, (dynamic_sys, count_changed_positions).chain());

    // Execute schedule tick
    app.update();

    let count = app.world().resource::<ChangedCounter>().0;
    assert_eq!(
        count, 20,
        "All 20 entities mutated via iter_column_mutable must trigger Changed<Position> in native Bevy system"
    );
}

// ============================================================================
// 6. Cross-Runtime Fault Containment Test
// ============================================================================

unsafe extern "C" fn panic_caught_guest_system(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    // Guest SDK catches unwind inside guest and returns PANIC_CAUGHT status code
    FfiResult::PANIC_CAUGHT
}

unsafe extern "C" fn internal_error_guest_system(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::INTERNAL_ERROR
}

#[test]
fn test_dynamic_system_panic_and_error_containment() {
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

    // 1. Guest returning FfiResult::PANIC_CAUGHT must be isolated and disabled
    let mut panicking_sys = DynamicCSystem::new(
        "panic_caught_sys".to_string(),
        query_descs.clone(),
        panic_caught_guest_system,
        core::ptr::null_mut(),
        Arc::new(registry.clone()),
    );
    assert!(panicking_sys.is_enabled());

    let _ = panicking_sys.initialize(app.world_mut());
    let cell = app.world_mut().as_unsafe_world_cell();
    let res = unsafe { panicking_sys.run_unsafe((), cell) };
    assert!(res.is_ok());
    assert!(
        !panicking_sys.is_enabled(),
        "System returning PANIC_CAUGHT must be disabled"
    );

    // 2. Guest returning error status must also be disabled
    let mut err_sys = DynamicCSystem::new(
        "err_sys".to_string(),
        query_descs,
        internal_error_guest_system,
        core::ptr::null_mut(),
        Arc::new(registry),
    );
    let _ = err_sys.initialize(app.world_mut());
    let cell2 = app.world_mut().as_unsafe_world_cell();
    let res2 = unsafe { err_sys.run_unsafe((), cell2) };
    assert!(res2.is_ok());
    assert!(
        !err_sys.is_enabled(),
        "System returning error status must be disabled"
    );
}

// ============================================================================
// 10. Dynamic Component Registration C-ABI Tests
// ============================================================================

#[test]
fn test_host_register_component_defensive_validation() {
    let dummy_ctx = 0xdead_beef_usize as *mut c_void;
    let name = c"CustomRuntimeComponent".as_ptr();
    let uuid = ComponentUuid::from_name("CustomRuntimeComponent");

    // 1. Valid descriptor and host context
    let valid_desc = ModComponentDescriptor::new(name, uuid, 16, 8);
    let res = unsafe { host_register_component(dummy_ctx, &valid_desc) };
    assert_eq!(res, FfiResult::OK);

    // 2. Null host context
    let res = unsafe { host_register_component(core::ptr::null_mut(), &valid_desc) };
    assert_eq!(res, FfiResult::NULL_POINTER);

    // 3. Null descriptor pointer
    let res = unsafe { host_register_component(dummy_ctx, core::ptr::null()) };
    assert_eq!(res, FfiResult::NULL_POINTER);

    // 4. Truncated descriptor struct_size
    let mut truncated_desc = valid_desc;
    truncated_desc.struct_size = 32;
    let res = unsafe { host_register_component(dummy_ctx, &truncated_desc) };
    assert_eq!(res, FfiResult::INVALID_VERSION);

    // 5. Forward-compatible descriptor struct_size (>= 64)
    let mut extended_desc = valid_desc;
    extended_desc.struct_size = 128;
    let res = unsafe { host_register_component(dummy_ctx, &extended_desc) };
    assert_eq!(res, FfiResult::OK);

    // 6. Null component name
    let null_name_desc = ModComponentDescriptor::new(core::ptr::null(), uuid, 16, 8);
    let res = unsafe { host_register_component(dummy_ctx, &null_name_desc) };
    assert_eq!(res, FfiResult::NULL_POINTER);

    // 7. Invalid UTF-8 in component name
    let invalid_utf8_bytes = [0xff, 0xfe, 0x00];
    let bad_utf8_desc = ModComponentDescriptor::new(
        invalid_utf8_bytes.as_ptr() as *const core::ffi::c_char,
        uuid,
        16,
        8,
    );
    let res = unsafe { host_register_component(dummy_ctx, &bad_utf8_desc) };
    assert_eq!(res, FfiResult::INVALID_UTF8);

    // 8. Invalid zero alignment
    let mut zero_align = valid_desc;
    zero_align.align = 0;
    let res = unsafe { host_register_component(dummy_ctx, &zero_align) };
    assert_eq!(res, FfiResult::INTERNAL_ERROR);

    // 9. Invalid non-power-of-two alignment
    let mut non_pot_align = valid_desc;
    non_pot_align.align = 6;
    let res = unsafe { host_register_component(dummy_ctx, &non_pot_align) };
    assert_eq!(res, FfiResult::INTERNAL_ERROR);

    // 10. Invalid misaligned size (size % align != 0)
    let mut misaligned = valid_desc;
    misaligned.size = 15;
    misaligned.align = 8;
    let res = unsafe { host_register_component(dummy_ctx, &misaligned) };
    assert_eq!(res, FfiResult::INTERNAL_ERROR);

    // 11. Valid Zero-Sized Type (ZST)
    let zst_desc = ModComponentDescriptor::new(name, uuid, 0, 1);
    let res = unsafe { host_register_component(dummy_ctx, &zst_desc) };
    assert_eq!(res, FfiResult::OK);

    // 12. Valid over-aligned ZST
    let overaligned_zst = ModComponentDescriptor::new(name, uuid, 0, 16);
    let res = unsafe { host_register_component(dummy_ctx, &overaligned_zst) };
    assert_eq!(res, FfiResult::OK);
}

#[test]
fn test_host_get_proc_address_resolves_register_component() {
    let registry = ModRegistry::new();

    // Registry symbol table lookup
    assert_eq!(
        registry.resolve_symbol("register_component"),
        Some(host_register_component as *const c_void)
    );
    assert_eq!(
        registry.resolve_symbol("host_register_component"),
        Some(host_register_component as *const c_void)
    );

    // Global / thread-local host_get_proc_address resolution
    let reg_arc = Arc::new(registry);
    let _guard = reg_arc.make_current();

    let sym_ptr = unsafe { host_get_proc_address(c"register_component".as_ptr()) };
    assert_eq!(sym_ptr, host_register_component as *const c_void);

    let host_sym_ptr = unsafe { host_get_proc_address(c"host_register_component".as_ptr()) };
    assert_eq!(host_sym_ptr, host_register_component as *const c_void);
}

#[test]
fn test_host_api_vtable_and_register_component_offsets() {
    // 64-bit layout assertions
    assert_eq!(core::mem::size_of::<HostApi>(), 72);
    assert_eq!(core::mem::align_of::<HostApi>(), 8);
    assert_eq!(HOST_API.struct_size, 72);

    assert_eq!(core::mem::offset_of!(HostApi, register_component), 64);

    // Ensure HOST_API function pointer points to host_register_component
    assert_eq!(
        HOST_API.register_component as *const () as usize,
        host_register_component as *const () as usize
    );

    // Direct invocation via HOST_API
    let dummy_ctx = 0xfeed_beef_usize as *mut c_void;
    let name = c"HealthPoints".as_ptr();
    let uuid = ComponentUuid::from_name("HealthPoints");
    let desc = ModComponentDescriptor::new(name, uuid, 4, 4);

    let res = unsafe { (HOST_API.register_component)(dummy_ctx, &desc) };
    assert_eq!(res, FfiResult::OK);
}
