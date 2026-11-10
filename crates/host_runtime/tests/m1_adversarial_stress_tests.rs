#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use abi_contract::{
    ComponentAccessDesc, ComponentAccessMode, FfiEntity, FfiResult, RawChunkIter, RawCommandQueue,
    SystemDescriptor,
};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::prelude::World;
use core::ffi::{c_char, c_void};
use host_runtime::{
    host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty, host_get_proc_address,
    host_iter_column_mutable, host_iter_column_readonly, host_iter_column_stride,
    host_iter_entities, host_iter_mark_changed, host_iter_next_chunk, host_register_system,
    AbiModPlugin, CommandQueueBridge, DynamicCSystem, HostChunkIter, ModAppExt, ModInitContext,
    ModRegistry, QueryColumnDesc,
};
use mod_derive::ModExposable;
use std::ffi::CString;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// Base test component
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
pub struct Armor {
    pub defense: i32,
}

// 20 distinct tag components for extreme archetype fragmentation stress testing
macro_rules! define_tags {
    ($($tag:ident),*) => {
        $(
            #[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
            pub struct $tag {
                pub id: u32,
            }
        )*
    };
}

define_tags!(
    Tag01, Tag02, Tag03, Tag04, Tag05, Tag06, Tag07, Tag08, Tag09, Tag10, Tag11, Tag12, Tag13,
    Tag14, Tag15, Tag16, Tag17, Tag18, Tag19, Tag20
);

// ============================================================================
// 1. Extreme Table Counts Stress Test (20 Distinct Archetypes)
// ============================================================================

static ENTITY_VISIT_COUNT: AtomicUsize = AtomicUsize::new(0);
static TABLE_VISIT_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn multi_table_stress_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let mut local_tables = 0;
    let mut local_entities = 0;

    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
        local_tables += 1;
        local_entities += count;

        let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut Position };
        if pos_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }

        for i in 0..count {
            unsafe {
                let p = &mut *pos_ptr.add(i);
                p.x += 10.0;
                p.y += 20.0;
                p.z += 30.0;
            }
        }
    }

    TABLE_VISIT_COUNT.fetch_add(local_tables, Ordering::SeqCst);
    ENTITY_VISIT_COUNT.fetch_add(local_entities, Ordering::SeqCst);
    FfiResult::OK
}

#[test]
fn test_extreme_archetype_table_counts() {
    ENTITY_VISIT_COUNT.store(0, Ordering::SeqCst);
    TABLE_VISIT_COUNT.store(0, Ordering::SeqCst);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();

    macro_rules! register_and_spawn {
        ($($tag:ident, $idx:expr);*) => {
            $(
                app.register_mod_component::<$tag>();
                // Spawn 5 entities for each archetype: (Position, TagNN)
                for i in 0..5 {
                    app.world_mut().spawn((
                        Position { x: i as f32, y: 0.0, z: 0.0 },
                        $tag { id: $idx },
                    ));
                }
            )*
        };
    }

    register_and_spawn!(
        Tag01, 1; Tag02, 2; Tag03, 3; Tag04, 4; Tag05, 5;
        Tag06, 6; Tag07, 7; Tag08, 8; Tag09, 9; Tag10, 10;
        Tag11, 11; Tag12, 12; Tag13, 13; Tag14, 14; Tag15, 15;
        Tag16, 16; Tag17, 17; Tag18, 18; Tag19, 19; Tag20, 20
    );

    // Also spawn 10 entities that do NOT match the query (Tag01 + Tag02 without Position)
    for i in 0..10 {
        app.world_mut()
            .spawn((Tag01 { id: 100 + i }, Tag02 { id: 200 + i }));
    }

    let registry = app.world().resource::<ModRegistry>().clone();
    let query_descs = vec![host_runtime::ComponentAccessDescOwned {
        name: "Position".to_string(),
        mode: ComponentAccessMode::WRITE,
    }];

    let dynamic_sys = DynamicCSystem::new(
        "multi_table_stress".to_string(),
        query_descs,
        multi_table_stress_system,
        core::ptr::null_mut(),
        Arc::new(registry),
    );

    app.add_systems(Update, dynamic_sys);

    // Run first schedule tick
    app.update();

    // 20 distinct archetypes * 5 entities = 100 entities across 20 distinct tables
    assert_eq!(
        TABLE_VISIT_COUNT.load(Ordering::SeqCst),
        20,
        "Must visit exactly 20 distinct tables"
    );
    assert_eq!(
        ENTITY_VISIT_COUNT.load(Ordering::SeqCst),
        100,
        "Must visit all 100 matching entities"
    );

    // Verify all 100 entities got mutated
    let mut query = app.world_mut().query::<&Position>();
    let mut count = 0;
    for pos in query.iter(app.world()) {
        assert_eq!(pos.y, 20.0);
        assert_eq!(pos.z, 30.0);
        count += 1;
    }
    assert_eq!(count, 100);

    // Second tick: verify steady-state zero reallocation works with same tables
    TABLE_VISIT_COUNT.store(0, Ordering::SeqCst);
    ENTITY_VISIT_COUNT.store(0, Ordering::SeqCst);
    app.update();
    assert_eq!(TABLE_VISIT_COUNT.load(Ordering::SeqCst), 20);
    assert_eq!(ENTITY_VISIT_COUNT.load(Ordering::SeqCst), 100);
}

// ============================================================================
// 2. Empty Table Iteration Stress Test (Created then Despawned)
// ============================================================================

#[test]
fn test_empty_table_iteration_and_despawn_fragmentation() {
    let mut world = World::new();
    let pos_id = world.register_component::<Position>();
    let vel_id = world.register_component::<Velocity>();
    world.register_component::<Tag01>();
    world.register_component::<Tag02>();
    world.register_component::<Tag03>();

    // Table 1: (Position, Velocity, Tag01) -> 10 entities
    let mut t1_entities = Vec::new();
    for i in 0..10 {
        t1_entities.push(
            world
                .spawn((
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
                    Tag01 { id: 1 },
                ))
                .id(),
        );
    }

    // Table 2: (Position, Velocity, Tag02) -> 10 entities
    let mut t2_entities = Vec::new();
    for i in 0..10 {
        t2_entities.push(
            world
                .spawn((
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
                    Tag02 { id: 2 },
                ))
                .id(),
        );
    }

    // Table 3: (Position, Velocity, Tag03) -> 10 entities
    let mut t3_entities = Vec::new();
    for i in 0..10 {
        t3_entities.push(
            world
                .spawn((
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
                    Tag03 { id: 3 },
                ))
                .id(),
        );
    }

    // Despawn ALL entities in Table 1 and Table 3.
    // In Bevy ECS, Table 1 and Table 3 remain registered in Tables storage but have entity_count() == 0.
    for e in t1_entities {
        world.despawn(e);
    }
    for e in t3_entities {
        world.despawn(e);
    }

    let cell = world.as_unsafe_world_cell();
    let storages = unsafe { cell.storages() };

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

    let mut matched_tables = Vec::new();
    for arch in cell.archetypes().iter() {
        if arch.contains(pos_id) && arch.contains(vel_id) {
            let tid = arch.table_id();
            if !matched_tables.contains(&tid) {
                matched_tables.push(tid);
            }
        }
    }
    assert_eq!(
        matched_tables.len(),
        3,
        "3 archetypes/tables exist matching Position + Velocity"
    );

    let mut host_iter = HostChunkIter::new(
        &storages.tables,
        &matched_tables,
        &query_columns,
        cell.change_tick(),
    );
    let raw_iter = (&mut host_iter as *mut HostChunkIter<'_>).cast::<RawChunkIter>();

    // Next chunk must skip empty Table 1 and immediately yield Table 2 (10 entities)
    let chunk1_count = unsafe { host_iter_next_chunk(raw_iter) };
    assert_eq!(
        chunk1_count, 10,
        "First yielded chunk must be Table 2 with 10 entities"
    );

    let entities_ptr = unsafe { host_iter_entities(raw_iter) };
    assert!(!entities_ptr.is_null());

    // Next chunk must skip empty Table 3 and return 0 (exhausted)
    let chunk2_count = unsafe { host_iter_next_chunk(raw_iter) };
    assert_eq!(
        chunk2_count, 0,
        "Empty Table 3 must be skipped, reaching end of iteration"
    );

    // Repeated calls after exhaustion must continue safely returning 0 without panic
    assert_eq!(unsafe { host_iter_next_chunk(raw_iter) }, 0);
    assert_eq!(unsafe { host_iter_next_chunk(raw_iter) }, 0);

    // Calling column accessors on exhausted iterator must return null safely
    assert!(unsafe { host_iter_column_readonly(raw_iter, 0) }.is_null());
    assert!(unsafe { host_iter_column_mutable(raw_iter, 0) }.is_null());
    assert!(unsafe { host_iter_entities(raw_iter) }.is_null());

    // Despawn Table 2 as well -> now ALL tables are empty
    for e in t2_entities {
        world.despawn(e);
    }
    let cell_all_empty = world.as_unsafe_world_cell();
    let storages_all_empty = unsafe { cell_all_empty.storages() };
    let mut empty_iter = HostChunkIter::new(
        &storages_all_empty.tables,
        &matched_tables,
        &query_columns,
        cell_all_empty.change_tick(),
    );
    let raw_empty = (&mut empty_iter as *mut HostChunkIter<'_>).cast::<RawChunkIter>();

    // When all matching tables have 0 entities, next_chunk returns 0 on first call
    assert_eq!(unsafe { host_iter_next_chunk(raw_empty) }, 0);
    assert!(unsafe { host_iter_column_readonly(raw_empty, 0) }.is_null());
    assert!(unsafe { host_iter_column_mutable(raw_empty, 0) }.is_null());
    assert!(unsafe { host_iter_entities(raw_empty) }.is_null());
}

// ============================================================================
// 3. Permission Enforcement: iter_column_mutable on Read-Only Components
// ============================================================================

#[test]
fn test_permission_enforcement_read_only_and_filter_only_mutation_rejected() {
    let mut world = World::new();
    let pos_id = world.register_component::<Position>();
    let vel_id = world.register_component::<Velocity>();
    let arm_id = world.register_component::<Armor>();

    world.spawn((
        Position {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        },
        Velocity {
            x: 4.0,
            y: 5.0,
            z: 6.0,
        },
        Armor { defense: 100 },
    ));

    let cell = world.as_unsafe_world_cell();
    let storages = unsafe { cell.storages() };

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
        QueryColumnDesc {
            component_id: arm_id,
            mode: ComponentAccessMode::FILTER_ONLY,
            stride: core::mem::size_of::<Armor>(),
        },
    ];

    let matched_tables = vec![cell.archetypes().iter().last().unwrap().table_id()];
    let mut host_iter = HostChunkIter::new(
        &storages.tables,
        &matched_tables,
        &query_columns,
        cell.change_tick(),
    );
    let raw_iter = (&mut host_iter as *mut HostChunkIter<'_>).cast::<RawChunkIter>();

    let count = unsafe { host_iter_next_chunk(raw_iter) };
    assert_eq!(count, 1);

    // Column 0: WRITE -> mutable allowed, readonly allowed
    assert!(!unsafe { host_iter_column_mutable(raw_iter, 0) }.is_null());
    assert!(!unsafe { host_iter_column_readonly(raw_iter, 0) }.is_null());

    // Column 1: READ -> mutable MUST return null_mut(), readonly allowed
    let vel_mut = unsafe { host_iter_column_mutable(raw_iter, 1) };
    assert!(
        vel_mut.is_null(),
        "iter_column_mutable on READ column MUST return null_mut()"
    );
    assert!(!unsafe { host_iter_column_readonly(raw_iter, 1) }.is_null());

    // Column 2: FILTER_ONLY -> mutable MUST return null_mut(), readonly MUST return null()
    let arm_mut = unsafe { host_iter_column_mutable(raw_iter, 2) };
    assert!(
        arm_mut.is_null(),
        "iter_column_mutable on FILTER_ONLY column MUST return null_mut()"
    );
    let arm_ro = unsafe { host_iter_column_readonly(raw_iter, 2) };
    assert!(
        arm_ro.is_null(),
        "iter_column_readonly on FILTER_ONLY column MUST return null()"
    );

    // Out of bounds column index: index 3, 100, usize::MAX -> MUST return null_mut()
    assert!(unsafe { host_iter_column_mutable(raw_iter, 3) }.is_null());
    assert!(unsafe { host_iter_column_mutable(raw_iter, 100) }.is_null());
    assert!(unsafe { host_iter_column_mutable(raw_iter, usize::MAX) }.is_null());
    assert!(unsafe { host_iter_column_readonly(raw_iter, 3) }.is_null());
    assert!(unsafe { host_iter_column_readonly(raw_iter, 100) }.is_null());
    assert!(unsafe { host_iter_column_readonly(raw_iter, usize::MAX) }.is_null());

    // Calling iter_mark_changed on READ column MUST return PERMISSION_DENIED
    let mark_ro_res = unsafe { host_iter_mark_changed(raw_iter, 1, 0) };
    assert_eq!(mark_ro_res, FfiResult::PERMISSION_DENIED);

    // Calling iter_mark_changed on FILTER_ONLY column MUST return PERMISSION_DENIED
    let mark_filter_res = unsafe { host_iter_mark_changed(raw_iter, 2, 0) };
    assert_eq!(mark_filter_res, FfiResult::PERMISSION_DENIED);
}

// ============================================================================
// 4. Out-of-Bounds Row Index on iter_mark_changed
// ============================================================================

#[test]
fn test_out_of_bounds_row_index_on_iter_mark_changed() {
    let mut world = World::new();
    let pos_id = world.register_component::<Position>();

    // Spawn 5 entities (rows 0, 1, 2, 3, 4)
    for i in 0..5 {
        world.spawn(Position {
            x: i as f32,
            y: 0.0,
            z: 0.0,
        });
    }

    let cell = world.as_unsafe_world_cell();
    let storages = unsafe { cell.storages() };

    let query_columns = vec![QueryColumnDesc {
        component_id: pos_id,
        mode: ComponentAccessMode::WRITE,
        stride: core::mem::size_of::<Position>(),
    }];

    let matched_tables = vec![cell.archetypes().iter().last().unwrap().table_id()];
    let mut host_iter = HostChunkIter::new(
        &storages.tables,
        &matched_tables,
        &query_columns,
        cell.change_tick(),
    );
    let raw_iter = (&mut host_iter as *mut HostChunkIter<'_>).cast::<RawChunkIter>();

    let count = unsafe { host_iter_next_chunk(raw_iter) };
    assert_eq!(count, 5);

    // Valid rows: 0 and 4 -> OK
    assert_eq!(
        unsafe { host_iter_mark_changed(raw_iter, 0, 0) },
        FfiResult::OK
    );
    assert_eq!(
        unsafe { host_iter_mark_changed(raw_iter, 0, 4) },
        FfiResult::OK
    );

    // Off-by-one boundary: row == current_entity_count (row 5) -> MUST return INTERNAL_ERROR
    let oob_exact = unsafe { host_iter_mark_changed(raw_iter, 0, 5) };
    assert_eq!(
        oob_exact,
        FfiResult::INTERNAL_ERROR,
        "row == count must return INTERNAL_ERROR"
    );

    // Extreme out-of-bounds rows
    assert_eq!(
        unsafe { host_iter_mark_changed(raw_iter, 0, 6) },
        FfiResult::INTERNAL_ERROR
    );
    assert_eq!(
        unsafe { host_iter_mark_changed(raw_iter, 0, 1000) },
        FfiResult::INTERNAL_ERROR
    );
    assert_eq!(
        unsafe { host_iter_mark_changed(raw_iter, 0, usize::MAX) },
        FfiResult::INTERNAL_ERROR
    );

    // Invalid component index
    assert_eq!(
        unsafe { host_iter_mark_changed(raw_iter, 1, 0) },
        FfiResult::INVALID_COMPONENT
    );
    assert_eq!(
        unsafe { host_iter_mark_changed(raw_iter, 999, 0) },
        FfiResult::INVALID_COMPONENT
    );

    // Null iterator pointer
    assert_eq!(
        unsafe { host_iter_mark_changed(core::ptr::null_mut(), 0, 0) },
        FfiResult::NULL_POINTER
    );
}

// ============================================================================
// 5. High-Throughput Command Queue Bridge Stress Test (10,000 Commands)
// ============================================================================

#[test]
fn test_rapid_command_queue_bridge_10k_commands() {
    let mut world = World::new();
    let pos_id = world.register_component::<Position>();

    let mut registry = ModRegistry::new();
    registry.register_component::<Position>(pos_id);

    let mut cmd_queue = bevy_ecs::world::CommandQueue::default();
    let comp_name = CString::new("Position").unwrap();

    let initial_count = world.entities().len();
    println!("Entities count right after World::new() + register_component: {initial_count}");
    const CMD_COUNT: usize = 10_000;
    let mut spawned_entities = Vec::with_capacity(CMD_COUNT);

    // 1. Rapidly queue 10,000 entity spawns + component insertions
    {
        let cell = world.as_unsafe_world_cell();
        let allocator = cell.entity_allocator();
        let mut bridge = CommandQueueBridge::new(&mut cmd_queue, allocator, &registry);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        for i in 0..CMD_COUNT {
            let entity = unsafe { host_cmd_spawn_empty(raw_queue) };
            assert_ne!(entity.0, 0);

            let pos = Position {
                x: i as f32,
                y: (i * 2) as f32,
                z: (i * 3) as f32,
            };

            let status = unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    entity,
                    comp_name.as_ptr(),
                    &pos as *const Position as *const u8,
                    core::mem::size_of::<Position>(),
                )
            };
            assert_eq!(status, FfiResult::OK);
            spawned_entities.push(entity);
        }
    }

    assert_eq!(spawned_entities.len(), CMD_COUNT);

    // Before apply, world has initial_count entities
    assert_eq!(world.entities().len(), initial_count);

    // Flush 20,000 command closures to the Bevy World
    cmd_queue.apply(&mut world);

    // In Bevy ECS, world.query::<&Position>().iter(&world).count() gives exact count of Position entities
    let live_pos_count = world.query::<&Position>().iter(&world).count();
    assert_eq!(live_pos_count, CMD_COUNT);

    for (i, &ffi_entity) in spawned_entities.iter().enumerate() {
        let bevy_entity = bevy_ecs::entity::Entity::from_bits(ffi_entity.to_bits());
        let pos = world
            .get::<Position>(bevy_entity)
            .expect("Entity must have Position");
        assert_eq!(pos.x, i as f32);
        assert_eq!(pos.y, (i * 2) as f32);
        assert_eq!(pos.z, (i * 3) as f32);
    }

    // 2. Rapidly queue 10,000 despawns
    {
        let cell = world.as_unsafe_world_cell();
        let allocator = cell.entity_allocator();
        let mut bridge = CommandQueueBridge::new(&mut cmd_queue, allocator, &registry);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        for &ffi_entity in &spawned_entities {
            let res = unsafe { host_cmd_despawn(raw_queue, ffi_entity) };
            assert_eq!(res, FfiResult::OK);
        }
    }

    // Flush despawn closures
    cmd_queue.apply(&mut world);

    // Verify all 10,000 entities have been despawned
    let remaining_pos_count = world.query::<&Position>().iter(&world).count();
    assert_eq!(
        remaining_pos_count, 0,
        "All 10,000 Position entities must be despawned"
    );
    for &ffi_entity in &spawned_entities {
        let bevy_entity = bevy_ecs::entity::Entity::from_bits(ffi_entity.to_bits());
        assert!(
            world.get_entity(bevy_entity).is_err(),
            "Entity must be despawned"
        );
    }
}

// ============================================================================
// 6. Defensive Boundary Torture & Null Pointer Fuzzing
// ============================================================================

#[test]
fn test_defensive_boundary_and_null_pointer_torture() {
    let mut world = World::new();
    let pos_id = world.register_component::<Position>();

    let mut registry = ModRegistry::new();
    registry.register_component::<Position>(pos_id);

    let mut cmd_queue = bevy_ecs::world::CommandQueue::default();
    let cell = world.as_unsafe_world_cell();
    let mut bridge = CommandQueueBridge::new(&mut cmd_queue, cell.entity_allocator(), &registry);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let valid_name = CString::new("Position").unwrap();
    let dummy_pos = Position {
        x: 1.0,
        y: 2.0,
        z: 3.0,
    };

    // --- host_get_proc_address ---
    assert!(unsafe { host_get_proc_address(core::ptr::null()) }.is_null());

    // --- host_cmd_spawn_empty ---
    assert_eq!(
        unsafe { host_cmd_spawn_empty(core::ptr::null_mut()) },
        FfiEntity(0)
    );

    // --- host_cmd_despawn ---
    assert_eq!(
        unsafe { host_cmd_despawn(core::ptr::null_mut(), FfiEntity(123)) },
        FfiResult::NULL_POINTER
    );

    // Entity bit pattern where try_from_bits fails (generation 0 returns None)
    assert_eq!(
        unsafe { host_cmd_despawn(raw_queue, FfiEntity(0)) },
        FfiResult::INTERNAL_ERROR
    );

    // Non-existent entity with valid bits returns OK (queued) and safely no-ops on flush
    assert_eq!(
        unsafe { host_cmd_despawn(raw_queue, FfiEntity(u64::MAX)) },
        FfiResult::OK
    );

    // --- host_cmd_insert_component ---
    // Null queue
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                core::ptr::null_mut(),
                FfiEntity(1),
                valid_name.as_ptr(),
                &dummy_pos as *const Position as *const u8,
                core::mem::size_of::<Position>(),
            )
        },
        FfiResult::NULL_POINTER
    );

    // Null component_name
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                FfiEntity(1),
                core::ptr::null(),
                &dummy_pos as *const Position as *const u8,
                core::mem::size_of::<Position>(),
            )
        },
        FfiResult::NULL_POINTER
    );

    // Null data when len > 0
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                FfiEntity(1),
                valid_name.as_ptr(),
                core::ptr::null(),
                12,
            )
        },
        FfiResult::NULL_POINTER
    );

    // Invalid UTF-8 in component_name
    let invalid_utf8_bytes = [0xFF, 0xFE, 0xFD, 0x00];
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                FfiEntity(1),
                invalid_utf8_bytes.as_ptr() as *const c_char,
                &dummy_pos as *const Position as *const u8,
                core::mem::size_of::<Position>(),
            )
        },
        FfiResult::INVALID_UTF8
    );

    // Invalid entity bits (generation 0) in insert
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                FfiEntity(0),
                valid_name.as_ptr(),
                &dummy_pos as *const Position as *const u8,
                core::mem::size_of::<Position>(),
            )
        },
        FfiResult::INTERNAL_ERROR
    );

    // Mismatched size (comp_meta.size != len)
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                FfiEntity(1),
                valid_name.as_ptr(),
                &dummy_pos as *const Position as *const u8,
                11, // expected 12
            )
        },
        FfiResult::INTERNAL_ERROR
    );

    // --- host_iter_* null safety ---
    assert_eq!(unsafe { host_iter_next_chunk(core::ptr::null_mut()) }, 0);
    assert!(unsafe { host_iter_column_readonly(core::ptr::null(), 0) }.is_null());
    assert!(unsafe { host_iter_column_mutable(core::ptr::null_mut(), 0) }.is_null());
    assert_eq!(
        unsafe { host_iter_mark_changed(core::ptr::null_mut(), 0, 0) },
        FfiResult::NULL_POINTER
    );
    assert!(unsafe { host_iter_entities(core::ptr::null()) }.is_null());
    assert_eq!(unsafe { host_iter_column_stride(core::ptr::null(), 0) }, 0);

    // --- host_register_system null safety ---
    assert_eq!(
        unsafe { host_register_system(core::ptr::null_mut(), core::ptr::null()) },
        FfiResult::NULL_POINTER
    );

    let mut init_ctx = ModInitContext::new(Arc::new(registry));
    let ctx_ptr = (&mut init_ctx as *mut ModInitContext).cast::<c_void>();

    assert_eq!(
        unsafe { host_register_system(ctx_ptr, core::ptr::null()) },
        FfiResult::NULL_POINTER
    );
    assert_eq!(
        unsafe {
            host_register_system(
                core::ptr::null_mut(),
                &SystemDescriptor {
                    struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
                    name: valid_name.as_ptr(),
                    query_count: 0,
                    queries: core::ptr::null(),
                    callback: multi_table_stress_system,
                    user_data: core::ptr::null_mut(),
                },
            )
        },
        FfiResult::NULL_POINTER
    );

    // Descriptor with small struct_size header (ABI mismatch simulation)
    let bad_desc = SystemDescriptor {
        struct_size: 16, // smaller than real struct
        name: valid_name.as_ptr(),
        query_count: 0,
        queries: core::ptr::null(),
        callback: multi_table_stress_system,
        user_data: core::ptr::null_mut(),
    };
    assert_eq!(
        unsafe { host_register_system(ctx_ptr, &bad_desc) },
        FfiResult::INTERNAL_ERROR
    );

    // Descriptor with query_count > 0 but queries == null
    let null_queries_desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: valid_name.as_ptr(),
        query_count: 1,
        queries: core::ptr::null(),
        callback: multi_table_stress_system,
        user_data: core::ptr::null_mut(),
    };
    assert_eq!(
        unsafe { host_register_system(ctx_ptr, &null_queries_desc) },
        FfiResult::NULL_POINTER
    );

    // Query descriptor with name == null
    let null_query_name = ComponentAccessDesc {
        name: core::ptr::null(),
        mode: ComponentAccessMode::READ,
    };
    let bad_query_name_desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: valid_name.as_ptr(),
        query_count: 1,
        queries: &null_query_name,
        callback: multi_table_stress_system,
        user_data: core::ptr::null_mut(),
    };
    assert_eq!(
        unsafe { host_register_system(ctx_ptr, &bad_query_name_desc) },
        FfiResult::NULL_POINTER
    );
}

// ============================================================================
// 7. MAX_QUERY_COMPONENTS Bounds Stress Test
// ============================================================================

#[test]
fn test_max_query_components_boundary() {
    let mut world = World::new();
    let pos_id = world.register_component::<Position>();

    world.spawn(Position {
        x: 10.0,
        y: 20.0,
        z: 30.0,
    });

    let cell = world.as_unsafe_world_cell();
    let storages = unsafe { cell.storages() };

    // Create a query descriptor array with 34 components (exceeding MAX_QUERY_COMPONENTS = 32)
    let mut query_columns = Vec::new();
    for _ in 0..34 {
        query_columns.push(QueryColumnDesc {
            component_id: pos_id,
            mode: ComponentAccessMode::WRITE,
            stride: core::mem::size_of::<Position>(),
        });
    }

    let matched_tables = vec![cell.archetypes().iter().last().unwrap().table_id()];
    let mut host_iter = HostChunkIter::new(
        &storages.tables,
        &matched_tables,
        &query_columns,
        cell.change_tick(),
    );
    let raw_iter = (&mut host_iter as *mut HostChunkIter<'_>).cast::<RawChunkIter>();

    let count = unsafe { host_iter_next_chunk(raw_iter) };
    assert_eq!(count, 1);

    // Indices 0..31 are within MAX_QUERY_COMPONENTS and must succeed
    assert!(!unsafe { host_iter_column_mutable(raw_iter, 0) }.is_null());
    assert!(!unsafe { host_iter_column_mutable(raw_iter, 31) }.is_null());

    // Index 32 and 33 exceed MAX_QUERY_COMPONENTS: MUST return null_mut() without panic or OOB read
    assert!(
        unsafe { host_iter_column_mutable(raw_iter, 32) }.is_null(),
        "Index 32 (>= MAX_QUERY_COMPONENTS) must return null_mut"
    );
    assert!(
        unsafe { host_iter_column_mutable(raw_iter, 33) }.is_null(),
        "Index 33 (>= MAX_QUERY_COMPONENTS) must return null_mut"
    );

    // Read-only accessors at and beyond boundary
    assert!(!unsafe { host_iter_column_readonly(raw_iter, 31) }.is_null());
    assert!(unsafe { host_iter_column_readonly(raw_iter, 32) }.is_null());
    assert!(unsafe { host_iter_column_readonly(raw_iter, 33) }.is_null());

    // iter_mark_changed beyond boundary
    assert_eq!(
        unsafe { host_iter_mark_changed(raw_iter, 32, 0) },
        FfiResult::INVALID_COMPONENT
    );
    assert_eq!(
        unsafe { host_iter_mark_changed(raw_iter, 33, 0) },
        FfiResult::INVALID_COMPONENT
    );
}

// ============================================================================
// 8. Dynamic Archetype Growth Across Consecutive Schedule Ticks
// ============================================================================

static SPAWN_NEW_ARCHETYPES_ONCE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static TICK2_ENTITY_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn dynamic_archetype_growth_system(
    iter: *mut RawChunkIter,
    commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let mut total_entities = 0;
    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
        total_entities += count;
    }
    TICK2_ENTITY_COUNT.store(total_entities, Ordering::SeqCst);

    // On first tick, dynamically queue spawning 10 entities in a new archetype (Tag10)
    if !SPAWN_NEW_ARCHETYPES_ONCE.swap(true, Ordering::SeqCst) {
        let tag_name = CString::new("Tag10").unwrap();
        let pos_name = CString::new("Position").unwrap();
        let pos_dummy = Position {
            x: 77.0,
            y: 88.0,
            z: 99.0,
        };
        let tag_dummy = Tag10 { id: 777 };

        for _ in 0..10 {
            let entity = unsafe { host_cmd_spawn_empty(commands) };
            let _ = unsafe {
                host_cmd_insert_component(
                    commands,
                    entity,
                    pos_name.as_ptr(),
                    &pos_dummy as *const Position as *const u8,
                    core::mem::size_of::<Position>(),
                )
            };
            let _ = unsafe {
                host_cmd_insert_component(
                    commands,
                    entity,
                    tag_name.as_ptr(),
                    &tag_dummy as *const Tag10 as *const u8,
                    core::mem::size_of::<Tag10>(),
                )
            };
        }
    }

    FfiResult::OK
}

#[test]
fn test_dynamic_archetype_growth_between_ticks() {
    SPAWN_NEW_ARCHETYPES_ONCE.store(false, Ordering::SeqCst);
    TICK2_ENTITY_COUNT.store(0, Ordering::SeqCst);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Tag01>();
    app.register_mod_component::<Tag10>();

    // Initial world: 5 entities in (Position, Tag01)
    for i in 0..5 {
        app.world_mut().spawn((
            Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            },
            Tag01 { id: 1 },
        ));
    }

    let registry = app.world().resource::<ModRegistry>().clone();
    let query_descs = vec![host_runtime::ComponentAccessDescOwned {
        name: "Position".to_string(),
        mode: ComponentAccessMode::READ,
    }];

    let dynamic_sys = DynamicCSystem::new(
        "growth_system".to_string(),
        query_descs,
        dynamic_archetype_growth_system,
        core::ptr::null_mut(),
        Arc::new(registry),
    );

    app.add_systems(Update, dynamic_sys);

    // Tick 1: processes the 5 initial entities, queues 10 entities in new archetype (Position, Tag10)
    app.update();
    assert_eq!(TICK2_ENTITY_COUNT.load(Ordering::SeqCst), 5);

    // Tick 2: system must detect archetype_generation change, update matched_tables, and process 15 entities!
    app.update();
    assert_eq!(
        TICK2_ENTITY_COUNT.load(Ordering::SeqCst),
        15,
        "System must dynamically update matched_tables when new archetypes are spawned"
    );
}

// ============================================================================
// 9. Unregistered Component Graceful Degradation in DynamicCSystem
// ============================================================================

unsafe extern "C" fn query_with_unregistered_callback(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
        // Column 0 was valid (Position), Column 1 was unregistered
        let col0 = unsafe { host_iter_column_readonly(iter, 0) };
        assert!(!col0.is_null());
        let col1 = unsafe { host_iter_column_readonly(iter, 1) };
        assert!(
            col1.is_null(),
            "Unregistered component column must return null"
        );
    }
    FfiResult::OK
}

#[test]
fn test_unregistered_component_in_dynamic_system() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();

    app.world_mut().spawn(Position {
        x: 1.0,
        y: 2.0,
        z: 3.0,
    });

    let registry = app.world().resource::<ModRegistry>().clone();
    let query_descs = vec![
        host_runtime::ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::READ,
        },
        host_runtime::ComponentAccessDescOwned {
            name: "UnregisteredGhostComponent".to_string(),
            mode: ComponentAccessMode::READ,
        },
    ];

    let dynamic_sys = DynamicCSystem::new(
        "ghost_query_system".to_string(),
        query_descs,
        query_with_unregistered_callback,
        core::ptr::null_mut(),
        Arc::new(registry),
    );

    app.add_systems(Update, dynamic_sys);

    // Must execute cleanly without crashing or panicking
    app.update();
}
