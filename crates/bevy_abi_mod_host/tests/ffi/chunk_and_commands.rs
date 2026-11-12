//! Multi-archetype chunk iteration, change-detection granularity, and high-volume deferred command queues.

mod archetype_and_permission_boundaries {
    #![allow(unused_imports, dead_code)]
    use bevy_abi_mod_host::{AbiModPlugin, ModAppExt, ModRegistry};
    use bevy_abi_mod_host_sys::{
        CommandQueueBridge, DynamicCSystem, HostChunkIter, ModInitContext, QueryColumnDesc,
        host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty, host_get_proc_address,
        host_iter_column_mutable, host_iter_column_readonly, host_iter_column_stride,
        host_iter_entities, host_iter_mark_changed, host_iter_next_chunk, host_register_system,
    };
    use bevy_abi_mod_macros::ModExposable;
    use bevy_abi_mod_sys::{
        ComponentAccessDesc, ComponentAccessMode, FfiEntity, FfiResult, RawChunkIter,
        RawCommandQueue, SystemDescriptor,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::prelude::World;
    use core::ffi::{c_char, c_void};
    use std::ffi::CString;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        let query_descs = vec![bevy_abi_mod_host_sys::ComponentAccessDescOwned {
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

        // Off-by-one boundary: row == current_entity_count (row 5) -> MUST return OUT_OF_RANGE
        let oob_exact = unsafe { host_iter_mark_changed(raw_iter, 0, 5) };
        assert_eq!(
            oob_exact,
            FfiResult::OUT_OF_RANGE,
            "row == count must return OUT_OF_RANGE"
        );

        // Extreme out-of-bounds rows
        assert_eq!(
            unsafe { host_iter_mark_changed(raw_iter, 0, 6) },
            FfiResult::OUT_OF_RANGE
        );
        assert_eq!(
            unsafe { host_iter_mark_changed(raw_iter, 0, 1000) },
            FfiResult::OUT_OF_RANGE
        );
        assert_eq!(
            unsafe { host_iter_mark_changed(raw_iter, 0, usize::MAX) },
            FfiResult::OUT_OF_RANGE
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
        let mut bridge =
            CommandQueueBridge::new(&mut cmd_queue, cell.entity_allocator(), &registry);
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

        // Entity bit pattern where try_from_bits fails (generation 0) or Entity::PLACEHOLDER
        assert_eq!(
            unsafe { host_cmd_despawn(raw_queue, FfiEntity(0)) },
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            unsafe {
                host_cmd_despawn(
                    raw_queue,
                    FfiEntity::from_bits(bevy_ecs::entity::Entity::PLACEHOLDER.to_bits()),
                )
            },
            FfiResult::INVALID_ENTITY
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
            FfiResult::INVALID_ENTITY
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
            FfiResult::INVALID_ARGUMENT
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
        let _active_guard = init_ctx.make_active();
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
            FfiResult::INVALID_VERSION
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
        let query_descs = vec![bevy_abi_mod_host_sys::ComponentAccessDescOwned {
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
            bevy_abi_mod_host_sys::ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::READ,
            },
            bevy_abi_mod_host_sys::ComponentAccessDescOwned {
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
}

mod chunk_and_command_bursts {
    #![allow(unused_imports, dead_code)]
    use bevy_abi_mod_host::{AbiModPlugin, ModAppExt, ModRegistry};
    use bevy_abi_mod_host_sys::{
        CommandQueueBridge, DynamicCSystem, host_cmd_despawn, host_cmd_insert_component,
        host_cmd_spawn_empty, host_iter_column_mutable, host_iter_column_readonly,
        host_iter_column_stride, host_iter_entities, host_iter_mark_changed, host_iter_next_chunk,
    };
    use bevy_abi_mod_macros::ModExposable;
    use bevy_abi_mod_sys::{
        ComponentAccessMode, FfiEntity, FfiResult, RawChunkIter, RawCommandQueue,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::prelude::{ResMut, Resource, World};
    use bevy_ecs::query::Changed;
    use bevy_ecs::schedule::IntoScheduleConfigs;
    use bevy_ecs::system::Query;
    use core::ffi::c_void;
    use std::ffi::CString;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Test components
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

    macro_rules! define_stress_tags {
        ($($tag:ident),*) => {
            $(
                #[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
                pub struct $tag {
                    pub id: u32,
                }
            )*
        };
    }

    define_stress_tags!(TagA, TagB, TagC, TagD, TagE);

    // ============================================================================
    // SUITE 1: CHUNK ITERATION SCALING & BOUNDARIES (0, 1, 20,000 ENTITIES)
    // ============================================================================

    struct EmptyTableStats {
        chunks_visited: usize,
        entities_visited: usize,
        readonly_null_after_exhaust: bool,
        mutable_null_after_exhaust: bool,
        entities_null_after_exhaust: bool,
        mark_err_after_exhaust: bool,
    }

    unsafe extern "C" fn empty_table_stress_system(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        user_data: *mut c_void,
    ) -> FfiResult {
        let stats = unsafe { &mut *(user_data as *mut EmptyTableStats) };

        loop {
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }
            stats.chunks_visited += 1;
            stats.entities_visited += count;
        }

        // Defensive check after exhaustion: calling accessors must safely return null / err
        let ro = unsafe { host_iter_column_readonly(iter, 0) };
        let rw = unsafe { host_iter_column_mutable(iter, 0) };
        let ents = unsafe { host_iter_entities(iter) };
        let mark_res = unsafe { host_iter_mark_changed(iter, 0, 0) };

        stats.readonly_null_after_exhaust = ro.is_null();
        stats.mutable_null_after_exhaust = rw.is_null();
        stats.entities_null_after_exhaust = ents.is_null();
        stats.mark_err_after_exhaust = mark_res == FfiResult::OUT_OF_RANGE;

        // Repeated next_chunk calls after exhaustion must stay 0
        for _ in 0..5 {
            assert_eq!(unsafe { host_iter_next_chunk(iter) }, 0);
        }

        FfiResult::OK
    }

    #[test]
    fn test_chunk_iteration_empty_world_and_empty_tables() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();

        let registry = app.world().resource::<ModRegistry>().clone();
        let query_descs = vec![
            bevy_abi_mod_host_sys::ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            bevy_abi_mod_host_sys::ComponentAccessDescOwned {
                name: "Velocity".to_string(),
                mode: ComponentAccessMode::READ,
            },
        ];

        let mut stats = EmptyTableStats {
            chunks_visited: 0,
            entities_visited: 0,
            readonly_null_after_exhaust: false,
            mutable_null_after_exhaust: false,
            entities_null_after_exhaust: false,
            mark_err_after_exhaust: false,
        };

        let dynamic_sys = DynamicCSystem::new(
            "empty_stress".to_string(),
            query_descs,
            empty_table_stress_system,
            &mut stats as *mut EmptyTableStats as *mut c_void,
            Arc::new(registry),
        );

        app.add_systems(Update, dynamic_sys);

        // Frame 1: Completely empty world (0 entities)
        app.update();

        assert_eq!(stats.chunks_visited, 0, "Empty world must yield 0 chunks");
        assert_eq!(
            stats.entities_visited, 0,
            "Empty world must yield 0 entities"
        );
        assert!(stats.readonly_null_after_exhaust);
        assert!(stats.mutable_null_after_exhaust);
        assert!(stats.entities_null_after_exhaust);
        assert!(stats.mark_err_after_exhaust);
    }

    #[test]
    fn test_chunk_iteration_alternating_empty_and_non_empty_tables() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Position>();
        app.register_mod_component::<TagA>();
        app.register_mod_component::<TagB>();
        app.register_mod_component::<TagC>();
        app.register_mod_component::<TagD>();
        app.register_mod_component::<TagE>();

        // Table A: (Position, TagA) -> 10 entities, then despawn all
        let mut t_a_ents = Vec::new();
        for i in 0..10 {
            t_a_ents.push(
                app.world_mut()
                    .spawn((
                        Position {
                            x: i as f32,
                            y: 0.0,
                            z: 0.0,
                        },
                        TagA { id: 1 },
                    ))
                    .id(),
            );
        }
        // Table B: (Position, TagB) -> 7 entities (KEEP ALIVE)
        for i in 0..7 {
            app.world_mut().spawn((
                Position {
                    x: 100.0 + i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                TagB { id: 2 },
            ));
        }
        // Table C: (Position, TagC) -> 15 entities, then despawn all
        let mut t_c_ents = Vec::new();
        for i in 0..15 {
            t_c_ents.push(
                app.world_mut()
                    .spawn((
                        Position {
                            x: 200.0 + i as f32,
                            y: 0.0,
                            z: 0.0,
                        },
                        TagC { id: 3 },
                    ))
                    .id(),
            );
        }
        // Table D: (Position, TagD) -> 23 entities (KEEP ALIVE)
        for i in 0..23 {
            app.world_mut().spawn((
                Position {
                    x: 300.0 + i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                TagD { id: 4 },
            ));
        }
        // Table E: (Position, TagE) -> 30 entities, then despawn all
        let mut t_e_ents = Vec::new();
        for i in 0..30 {
            t_e_ents.push(
                app.world_mut()
                    .spawn((
                        Position {
                            x: 400.0 + i as f32,
                            y: 0.0,
                            z: 0.0,
                        },
                        TagE { id: 5 },
                    ))
                    .id(),
            );
        }

        // Despawn Tables A, C, and E so they become empty tables in Bevy storage
        for e in t_a_ents {
            app.world_mut().despawn(e);
        }
        for e in t_c_ents {
            app.world_mut().despawn(e);
        }
        for e in t_e_ents {
            app.world_mut().despawn(e);
        }

        struct ChunkAudit {
            chunk_sizes: Vec<usize>,
            total_entities: usize,
        }

        unsafe extern "C" fn audit_system(
            iter: *mut RawChunkIter,
            _commands: *mut RawCommandQueue,
            user_data: *mut c_void,
        ) -> FfiResult {
            let audit = unsafe { &mut *(user_data as *mut ChunkAudit) };
            loop {
                let count = unsafe { host_iter_next_chunk(iter) };
                if count == 0 {
                    break;
                }
                audit.chunk_sizes.push(count);
                audit.total_entities += count;

                let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut Position };
                assert!(!pos_ptr.is_null());
                for i in 0..count {
                    unsafe {
                        (*pos_ptr.add(i)).y += 55.0;
                    }
                }
            }
            FfiResult::OK
        }

        let mut audit = ChunkAudit {
            chunk_sizes: Vec::new(),
            total_entities: 0,
        };

        let registry = app.world().resource::<ModRegistry>().clone();
        let query_descs = vec![bevy_abi_mod_host_sys::ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::WRITE,
        }];

        let dynamic_sys = DynamicCSystem::new(
            "audit_sys".to_string(),
            query_descs,
            audit_system,
            &mut audit as *mut ChunkAudit as *mut c_void,
            Arc::new(registry),
        );

        app.add_systems(Update, dynamic_sys);
        app.update();

        // Must have visited exactly 2 non-empty tables: sizes 7 and 23
        assert_eq!(
            audit.total_entities, 30,
            "Must visit exactly 30 living entities"
        );
        assert_eq!(
            audit.chunk_sizes.len(),
            2,
            "Must visit exactly 2 non-empty chunks"
        );
        assert!(audit.chunk_sizes.contains(&7));
        assert!(audit.chunk_sizes.contains(&23));

        // Verify all 30 entities received mutation y == 55.0
        let mut pos_q = app.world_mut().query::<&Position>();
        let mut verified = 0;
        for pos in pos_q.iter(app.world()) {
            assert_eq!(pos.y, 55.0);
            verified += 1;
        }
        assert_eq!(verified, 30);
    }

    #[test]
    fn test_chunk_iteration_single_entity_tables() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();

        let e = app
            .world_mut()
            .spawn((
                Position {
                    x: 1.0,
                    y: 2.0,
                    z: 3.0,
                },
                Velocity {
                    x: 10.0,
                    y: 20.0,
                    z: 30.0,
                },
            ))
            .id();

        #[repr(C)]
        struct SingleStats {
            stride_pos: usize,
            stride_vel: usize,
            count: usize,
            entity_matches: bool,
            mark_row0_ok: bool,
            mark_row1_err: bool,
        }

        unsafe extern "C" fn single_entity_system(
            iter: *mut RawChunkIter,
            _commands: *mut RawCommandQueue,
            user_data: *mut c_void,
        ) -> FfiResult {
            let ctx = unsafe { &mut *(user_data as *mut SingleContext) };
            let stats = &mut ctx.stats;
            stats.stride_pos = unsafe { host_iter_column_stride(iter, 0) };
            stats.stride_vel = unsafe { host_iter_column_stride(iter, 1) };

            let count = unsafe { host_iter_next_chunk(iter) };
            stats.count = count;
            assert_eq!(count, 1);

            let ents = unsafe { host_iter_entities(iter) };
            let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut Position };
            let vel_ptr = unsafe { host_iter_column_readonly(iter, 1) as *const Velocity };

            assert!(!ents.is_null());
            assert!(!pos_ptr.is_null());
            assert!(!vel_ptr.is_null());

            unsafe {
                let ent_bits = (*ents).to_bits();
                let pos = &mut *pos_ptr;
                let vel = &*vel_ptr;
                pos.x += vel.x;
                pos.y += vel.y;
                pos.z += vel.z;

                stats.entity_matches = ent_bits == ctx.target_entity.to_bits();
            }

            // Row 0 mark changed must succeed
            let r0 = unsafe { host_iter_mark_changed(iter, 0, 0) };
            stats.mark_row0_ok = r0 == FfiResult::OK;

            // Row 1 mark changed must fail (chunk size is 1)
            let r1 = unsafe { host_iter_mark_changed(iter, 0, 1) };
            stats.mark_row1_err = r1 == FfiResult::OUT_OF_RANGE;

            assert_eq!(unsafe { host_iter_next_chunk(iter) }, 0);
            FfiResult::OK
        }

        #[repr(C)]
        struct SingleContext {
            target_entity: Entity,
            stats: SingleStats,
        }

        let mut ctx = SingleContext {
            target_entity: e,
            stats: SingleStats {
                stride_pos: 0,
                stride_vel: 0,
                count: 0,
                entity_matches: false,
                mark_row0_ok: false,
                mark_row1_err: false,
            },
        };

        let registry = app.world().resource::<ModRegistry>().clone();
        let dynamic_sys = DynamicCSystem::new(
            "single_sys".to_string(),
            vec![
                bevy_abi_mod_host_sys::ComponentAccessDescOwned {
                    name: "Position".to_string(),
                    mode: ComponentAccessMode::WRITE,
                },
                bevy_abi_mod_host_sys::ComponentAccessDescOwned {
                    name: "Velocity".to_string(),
                    mode: ComponentAccessMode::READ,
                },
            ],
            single_entity_system,
            &mut ctx as *mut SingleContext as *mut c_void,
            Arc::new(registry),
        );

        app.add_systems(Update, dynamic_sys);
        app.update();

        assert_eq!(ctx.stats.count, 1);
        assert_eq!(ctx.stats.stride_pos, core::mem::size_of::<Position>());
        assert_eq!(ctx.stats.stride_vel, core::mem::size_of::<Velocity>());
        assert!(ctx.stats.entity_matches);
        assert!(ctx.stats.mark_row0_ok);
        assert!(ctx.stats.mark_row1_err);

        // Verify entity mutated
        let updated_pos = app.world().get::<Position>(e).unwrap();
        assert_eq!(updated_pos.x, 11.0);
        assert_eq!(updated_pos.y, 22.0);
        assert_eq!(updated_pos.z, 33.0);
    }

    #[test]
    fn test_chunk_iteration_20k_entities_monolithic_and_fragmented() {
        const MONOLITHIC_COUNT: usize = 20_000;

        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();
        app.register_mod_component::<Armor>();

        // 1. Spawn 20,000 entities in monolithic archetype (Position, Velocity, Armor)
        for i in 0..MONOLITHIC_COUNT {
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
                Armor {
                    defense: (i % 100) as i32,
                },
            ));
        }

        struct Scale20kStats {
            total_entities: usize,
            chunks: usize,
        }

        unsafe extern "C" fn scale_20k_system(
            iter: *mut RawChunkIter,
            _commands: *mut RawCommandQueue,
            user_data: *mut c_void,
        ) -> FfiResult {
            let stats = unsafe { &mut *(user_data as *mut Scale20kStats) };
            let stride_pos = unsafe { host_iter_column_stride(iter, 0) };
            let stride_vel = unsafe { host_iter_column_stride(iter, 1) };
            assert_eq!(stride_pos, core::mem::size_of::<Position>());
            assert_eq!(stride_vel, core::mem::size_of::<Velocity>());

            loop {
                let count = unsafe { host_iter_next_chunk(iter) };
                if count == 0 {
                    break;
                }
                stats.chunks += 1;
                stats.total_entities += count;

                let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut Position };
                let vel_ptr = unsafe { host_iter_column_readonly(iter, 1) as *const Velocity };
                let ents_ptr = unsafe { host_iter_entities(iter) };

                assert!(!pos_ptr.is_null());
                assert!(!vel_ptr.is_null());
                assert!(!ents_ptr.is_null());

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

        let mut stats = Scale20kStats {
            total_entities: 0,
            chunks: 0,
        };

        let registry = app.world().resource::<ModRegistry>().clone();
        let dynamic_sys = DynamicCSystem::new(
            "scale_20k_sys".to_string(),
            vec![
                bevy_abi_mod_host_sys::ComponentAccessDescOwned {
                    name: "Position".to_string(),
                    mode: ComponentAccessMode::WRITE,
                },
                bevy_abi_mod_host_sys::ComponentAccessDescOwned {
                    name: "Velocity".to_string(),
                    mode: ComponentAccessMode::READ,
                },
            ],
            scale_20k_system,
            &mut stats as *mut Scale20kStats as *mut c_void,
            Arc::new(registry),
        );

        app.add_systems(Update, dynamic_sys);
        app.update();

        assert_eq!(
            stats.total_entities, MONOLITHIC_COUNT,
            "Must process all 20,000 entities"
        );
        assert_eq!(stats.chunks, 1, "Monolithic table must be a single chunk");

        // Oracle verification across all 20,000 entities
        let mut query = app.world_mut().query::<(&Position, &Armor)>();
        let mut verified = 0usize;
        for (pos, armor) in query.iter(app.world()) {
            let i = pos.x - 1.0; // was i, incremented by v.x (1.0)
            assert_eq!(pos.y, 2.0);
            assert_eq!(pos.z, 3.0);
            assert_eq!(armor.defense, ((i as usize) % 100) as i32);
            verified += 1;
        }
        assert_eq!(verified, MONOLITHIC_COUNT);
    }

    // ============================================================================
    // SUITE 2: CHANGE DETECTION BITMASK EMPIRICAL PRECISION & ISOLATION
    // ============================================================================

    #[derive(Resource, Default)]
    struct ChangedPositionInspector {
        changed_count: usize,
        changed_entities: Vec<Entity>,
    }

    fn inspect_changed_positions(
        query: Query<(Entity, &Position), Changed<Position>>,
        mut inspector: ResMut<ChangedPositionInspector>,
    ) {
        inspector.changed_entities = query.iter().map(|(e, _)| e).collect();
        inspector.changed_count = inspector.changed_entities.len();
    }

    struct ChangePrecisionContext {
        mode: usize, // 0 = clean read, 1 = mark every 10th row, 2 = column_mutable all
        marked_entities: Vec<Entity>,
    }

    unsafe extern "C" fn change_precision_system(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        user_data: *mut c_void,
    ) -> FfiResult {
        let ctx = unsafe { &mut *(user_data as *mut ChangePrecisionContext) };

        loop {
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }

            let ents = unsafe { host_iter_entities(iter) };
            assert!(!ents.is_null());

            match ctx.mode {
                0 => {
                    // Clean mode: READ-ONLY pointer access. Even with WRITE permission on column 0,
                    // calling host_iter_column_readonly must NOT mark any change ticks!
                    let _ptr = unsafe { host_iter_column_readonly(iter, 0) };
                }
                1 => {
                    // Selective row marking: mark every 10th row (row % 10 == 0)
                    let _ptr = unsafe { host_iter_column_readonly(iter, 0) };
                    for row in 0..count {
                        if row % 10 == 0 {
                            let res = unsafe { host_iter_mark_changed(iter, 0, row) };
                            assert_eq!(res, FfiResult::OK);
                            let ent_bits = unsafe { (*ents.add(row)).to_bits() };
                            let bevy_ent = Entity::from_bits(ent_bits);
                            ctx.marked_entities.push(bevy_ent);
                        }
                    }
                }
                2 => {
                    // Chunk-wide mutation: host_iter_column_mutable marks all entities changed
                    let ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut Position };
                    assert!(!ptr.is_null());
                    for row in 0..count {
                        unsafe {
                            (*ptr.add(row)).x += 1.0;
                        }
                    }
                }
                _ => {}
            }
        }

        FfiResult::OK
    }

    #[test]
    fn test_change_detection_bitmask_precision_clean_vs_mutated() {
        const TOTAL_ENTITIES: usize = 1_000;

        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.init_resource::<ChangedPositionInspector>();
        app.register_mod_component::<Position>();

        let mut all_spawned = Vec::with_capacity(TOTAL_ENTITIES);
        for i in 0..TOTAL_ENTITIES {
            let e = app
                .world_mut()
                .spawn(Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                })
                .id();
            all_spawned.push(e);
        }

        let mut ctx = Box::new(ChangePrecisionContext {
            mode: 0,
            marked_entities: Vec::new(),
        });
        let ctx_ptr = &mut *ctx as *mut ChangePrecisionContext as *mut c_void;

        let registry = app.world().resource::<ModRegistry>().clone();
        let dynamic_sys = DynamicCSystem::new(
            "change_precision_sys".to_string(),
            vec![bevy_abi_mod_host_sys::ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::WRITE,
            }],
            change_precision_system,
            ctx_ptr,
            Arc::new(registry),
        );

        app.add_systems(Update, (dynamic_sys, inspect_changed_positions).chain());

        // Frame 1: Flush initial spawns (all 1,000 are newly Added -> detected as Changed)
        app.update();
        assert_eq!(
            app.world()
                .resource::<ChangedPositionInspector>()
                .changed_count,
            TOTAL_ENTITIES,
            "Spawn tick must report all 1,000 entities as changed"
        );

        // Frame 2: Clean frame (mode = 0). System has WRITE access, but does NOT mutate.
        // Downstream query MUST observe EXACTLY 0 changed entities!
        ctx.mode = 0;
        app.update();
        assert_eq!(
            app.world()
                .resource::<ChangedPositionInspector>()
                .changed_count,
            0,
            "Clean frame without mutation must observe exactly 0 Changed<Position> entities"
        );

        // Frame 3: Selective row marking (mode = 1). Marks rows 0, 10, 20, ..., 990 = 100 entities.
        ctx.mode = 1;
        ctx.marked_entities.clear();
        app.update();

        let inspector = app.world().resource::<ChangedPositionInspector>();
        assert_eq!(
            inspector.changed_count, 100,
            "Selective row marking must trigger Changed<Position> on exactly 100 entities"
        );
        assert_eq!(ctx.marked_entities.len(), 100);

        // Verify set equivalence: observed changed entities match marked entities exactly
        for expected_ent in &ctx.marked_entities {
            assert!(
                inspector.changed_entities.contains(expected_ent),
                "Marked entity {expected_ent:?} must be present in Changed<Position> results"
            );
        }

        // Frame 4: Clean frame again (mode = 0). Change ticks must have reset.
        ctx.mode = 0;
        ctx.marked_entities.clear();
        app.update();
        assert_eq!(
            app.world()
                .resource::<ChangedPositionInspector>()
                .changed_count,
            0,
            "Subsequent clean frame must cleanly observe 0 Changed<Position> entities"
        );

        // Frame 5: Chunk-wide mutation via host_iter_column_mutable (mode = 2).
        ctx.mode = 2;
        app.update();
        assert_eq!(
            app.world()
                .resource::<ChangedPositionInspector>()
                .changed_count,
            TOTAL_ENTITIES,
            "host_iter_column_mutable must trigger Changed<Position> on all 1,000 entities"
        );

        // Frame 6: Clean frame again.
        ctx.mode = 0;
        app.update();
        assert_eq!(
            app.world()
                .resource::<ChangedPositionInspector>()
                .changed_count,
            0,
            "Post-mutation clean frame must reset to 0 Changed<Position> entities"
        );
    }

    #[test]
    fn test_change_detection_multi_archetype_isolation() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.init_resource::<ChangedPositionInspector>();
        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();
        app.register_mod_component::<Armor>();
        app.register_mod_component::<TagA>();

        // Archetype 1: (Position, Velocity) -> 300 entities (Target of system)
        let mut arch1_ents = Vec::new();
        for i in 0..300 {
            arch1_ents.push(
                app.world_mut()
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
                    ))
                    .id(),
            );
        }

        // Archetype 2: (Position, Armor) -> 300 entities (Must NOT be modified)
        let mut arch2_ents = Vec::new();
        for i in 0..300 {
            arch2_ents.push(
                app.world_mut()
                    .spawn((
                        Position {
                            x: 1000.0 + i as f32,
                            y: 0.0,
                            z: 0.0,
                        },
                        Armor { defense: 50 },
                    ))
                    .id(),
            );
        }

        // Archetype 3: (Position, TagA) -> 300 entities (Must NOT be modified)
        let mut arch3_ents = Vec::new();
        for i in 0..300 {
            arch3_ents.push(
                app.world_mut()
                    .spawn((
                        Position {
                            x: 2000.0 + i as f32,
                            y: 0.0,
                            z: 0.0,
                        },
                        TagA { id: 77 },
                    ))
                    .id(),
            );
        }

        static MUTATE_ACTIVE: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn kinematics_only_system(
            iter: *mut RawChunkIter,
            _commands: *mut RawCommandQueue,
            _user_data: *mut c_void,
        ) -> FfiResult {
            if MUTATE_ACTIVE.load(Ordering::SeqCst) == 0 {
                return FfiResult::OK;
            }

            loop {
                let count = unsafe { host_iter_next_chunk(iter) };
                if count == 0 {
                    break;
                }
                let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut Position };
                if !pos_ptr.is_null() {
                    for i in 0..count {
                        unsafe {
                            (*pos_ptr.add(i)).x += 10.0;
                        }
                    }
                }
            }
            FfiResult::OK
        }

        let registry = app.world().resource::<ModRegistry>().clone();
        let dynamic_sys = DynamicCSystem::new(
            "kinematics_only".to_string(),
            vec![
                bevy_abi_mod_host_sys::ComponentAccessDescOwned {
                    name: "Position".to_string(),
                    mode: ComponentAccessMode::WRITE,
                },
                bevy_abi_mod_host_sys::ComponentAccessDescOwned {
                    name: "Velocity".to_string(),
                    mode: ComponentAccessMode::READ,
                },
            ],
            kinematics_only_system,
            core::ptr::null_mut(),
            Arc::new(registry),
        );

        app.add_systems(Update, (dynamic_sys, inspect_changed_positions).chain());

        // Frame 1: Flush initial spawns (all 900 entities changed)
        MUTATE_ACTIVE.store(0, Ordering::SeqCst);
        app.update();
        assert_eq!(
            app.world()
                .resource::<ChangedPositionInspector>()
                .changed_count,
            900
        );

        // Frame 2: Idle frame (0 changed)
        app.update();
        assert_eq!(
            app.world()
                .resource::<ChangedPositionInspector>()
                .changed_count,
            0
        );

        // Frame 3: Activate mutation on (Position, Velocity)
        MUTATE_ACTIVE.store(1, Ordering::SeqCst);
        app.update();

        let inspector = app.world().resource::<ChangedPositionInspector>();
        assert_eq!(
            inspector.changed_count, 300,
            "Exactly 300 entities in Archetype 1 (Position, Velocity) should trigger Changed<Position>"
        );

        // Validate that every changed entity belongs to Archetype 1, and none to Arch 2 or Arch 3
        for changed_e in &inspector.changed_entities {
            assert!(arch1_ents.contains(changed_e));
            assert!(!arch2_ents.contains(changed_e));
            assert!(!arch3_ents.contains(changed_e));
        }
    }

    // ============================================================================
    // SUITE 3: COMMANDQUEUEBRIDGE BURST CONCURRENCY & INTERLEAVED OPS
    // ============================================================================

    #[test]
    fn test_command_queue_bridge_burst_10k_interleaved_ops() {
        let mut world = World::new();
        let pos_id = world.register_component::<Position>();
        let vel_id = world.register_component::<Velocity>();
        let armor_id = world.register_component::<Armor>();

        let mut registry = ModRegistry::new();
        registry.register_component::<Position>(pos_id);
        registry.register_component::<Velocity>(vel_id);
        registry.register_component::<Armor>(armor_id);

        let mut cmd_queue = bevy_ecs::world::CommandQueue::default();
        let pos_name = CString::new("Position").unwrap();
        let vel_name = CString::new("Velocity").unwrap();
        let armor_name = CString::new("Armor").unwrap();

        const BURST_CYCLES: usize = 2_500;
        // Each cycle performs 4 operations:
        // 1. Spawn E_survive, insert Position, insert Velocity
        // 2. Spawn E_die, insert Position, despawn E_die immediately
        // Total ops = 2,500 * 5 = 12,500 queued operations!

        let mut surviving_entities = Vec::with_capacity(BURST_CYCLES);
        let mut dead_entities = Vec::with_capacity(BURST_CYCLES);

        {
            let cell = world.as_unsafe_world_cell();
            let allocator = cell.entity_allocator();
            let mut bridge = CommandQueueBridge::new(&mut cmd_queue, allocator, &registry);
            let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

            for i in 0..BURST_CYCLES {
                // A. Spawn surviving entity
                let e_surv = unsafe { host_cmd_spawn_empty(raw_queue) };
                assert_ne!(e_surv.0, 0);

                let pos = Position {
                    x: i as f32,
                    y: (i * 2) as f32,
                    z: (i * 3) as f32,
                };
                let vel = Velocity {
                    x: 10.0,
                    y: 20.0,
                    z: 30.0,
                };

                let res_p = unsafe {
                    host_cmd_insert_component(
                        raw_queue,
                        e_surv,
                        pos_name.as_ptr(),
                        &pos as *const Position as *const u8,
                        core::mem::size_of::<Position>(),
                    )
                };
                assert_eq!(res_p, FfiResult::OK);

                let res_v = unsafe {
                    host_cmd_insert_component(
                        raw_queue,
                        e_surv,
                        vel_name.as_ptr(),
                        &vel as *const Velocity as *const u8,
                        core::mem::size_of::<Velocity>(),
                    )
                };
                assert_eq!(res_v, FfiResult::OK);

                surviving_entities.push(e_surv);

                // B. Spawn ephemeral entity, insert component, then despawn in same queue
                let e_die = unsafe { host_cmd_spawn_empty(raw_queue) };
                assert_ne!(e_die.0, 0);

                let armor = Armor { defense: 999 };
                let res_a = unsafe {
                    host_cmd_insert_component(
                        raw_queue,
                        e_die,
                        armor_name.as_ptr(),
                        &armor as *const Armor as *const u8,
                        core::mem::size_of::<Armor>(),
                    )
                };
                assert_eq!(res_a, FfiResult::OK);

                let res_d = unsafe { host_cmd_despawn(raw_queue, e_die) };
                assert_eq!(res_d, FfiResult::OK);

                dead_entities.push(e_die);
            }
        }

        // Flush 12,500 operations to Bevy World
        cmd_queue.apply(&mut world);

        // Oracle Verification:
        // 1. Surviving entities: exactly BURST_CYCLES live entities with Position and Velocity
        let pos_count = world.query::<&Position>().iter(&world).count();
        let vel_count = world.query::<&Velocity>().iter(&world).count();
        assert_eq!(pos_count, BURST_CYCLES);
        assert_eq!(vel_count, BURST_CYCLES);

        for (i, &ffi_e) in surviving_entities.iter().enumerate() {
            let bevy_e = Entity::from_bits(ffi_e.to_bits());
            let pos = world
                .get::<Position>(bevy_e)
                .expect("Surviving entity must have Position");
            let vel = world
                .get::<Velocity>(bevy_e)
                .expect("Surviving entity must have Velocity");

            assert_eq!(pos.x, i as f32);
            assert_eq!(pos.y, (i * 2) as f32);
            assert_eq!(pos.z, (i * 3) as f32);
            assert_eq!(vel.x, 10.0);
            assert_eq!(vel.y, 20.0);
            assert_eq!(vel.z, 30.0);
        }

        // 2. Dead entities: zero must exist in the world, zero Armor components
        let armor_count = world.query::<&Armor>().iter(&world).count();
        assert_eq!(
            armor_count, 0,
            "All entities with Armor were despawned; count must be 0"
        );

        for &ffi_e in &dead_entities {
            let bevy_e = Entity::from_bits(ffi_e.to_bits());
            assert!(
                world.get_entity(bevy_e).is_err(),
                "Despawned ephemeral entity {bevy_e:?} must no longer exist in Bevy World"
            );
        }
    }

    #[test]
    fn test_command_queue_bridge_multi_system_schedule_concurrency() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();

        // System A: Spawns 500 entities with Position each tick
        unsafe extern "C" fn spawner_pos_system(
            _iter: *mut RawChunkIter,
            commands: *mut RawCommandQueue,
            _user_data: *mut c_void,
        ) -> FfiResult {
            let name = CString::new("Position").unwrap();
            for i in 0..500 {
                let ent = unsafe { host_cmd_spawn_empty(commands) };
                let pos = Position {
                    x: i as f32,
                    y: 1.0,
                    z: 2.0,
                };
                unsafe {
                    host_cmd_insert_component(
                        commands,
                        ent,
                        name.as_ptr(),
                        &pos as *const Position as *const u8,
                        core::mem::size_of::<Position>(),
                    );
                }
            }
            FfiResult::OK
        }

        // System B: Spawns 500 entities with Velocity each tick
        unsafe extern "C" fn spawner_vel_system(
            _iter: *mut RawChunkIter,
            commands: *mut RawCommandQueue,
            _user_data: *mut c_void,
        ) -> FfiResult {
            let name = CString::new("Velocity").unwrap();
            for _i in 0..500 {
                let ent = unsafe { host_cmd_spawn_empty(commands) };
                let vel = Velocity {
                    x: 5.0,
                    y: 5.0,
                    z: 5.0,
                };
                unsafe {
                    host_cmd_insert_component(
                        commands,
                        ent,
                        name.as_ptr(),
                        &vel as *const Velocity as *const u8,
                        core::mem::size_of::<Velocity>(),
                    );
                }
            }
            FfiResult::OK
        }

        let registry = app.world().resource::<ModRegistry>().clone();
        let reg_arc = Arc::new(registry);

        let sys_a = DynamicCSystem::new(
            "sys_a".to_string(),
            Vec::new(),
            spawner_pos_system,
            core::ptr::null_mut(),
            Arc::clone(&reg_arc),
        );
        let sys_b = DynamicCSystem::new(
            "sys_b".to_string(),
            Vec::new(),
            spawner_vel_system,
            core::ptr::null_mut(),
            Arc::clone(&reg_arc),
        );

        app.add_systems(Update, (sys_a, sys_b));

        // Run 3 ticks: 3 * 500 = 1,500 Position entities, 3 * 500 = 1,500 Velocity entities
        for _ in 1..=3 {
            app.update();
        }

        let pos_count = app
            .world_mut()
            .query::<&Position>()
            .iter(app.world())
            .count();
        let vel_count = app
            .world_mut()
            .query::<&Velocity>()
            .iter(app.world())
            .count();

        assert_eq!(pos_count, 1_500);
        assert_eq!(vel_count, 1_500);
    }

    #[test]
    fn test_command_queue_bridge_defensive_error_handling() {
        let mut world = World::new();
        let pos_id = world.register_component::<Position>();

        let mut registry = ModRegistry::new();
        registry.register_component::<Position>(pos_id);

        let mut cmd_queue = bevy_ecs::world::CommandQueue::default();
        let cell = world.as_unsafe_world_cell();
        let allocator = cell.entity_allocator();
        let mut bridge = CommandQueueBridge::new(&mut cmd_queue, allocator, &registry);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        let ent = unsafe { host_cmd_spawn_empty(raw_queue) };
        let valid_name = CString::new("Position").unwrap();
        let unreg_name = CString::new("NonExistentComponent").unwrap();
        let pos = Position {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        };

        // 1. Null queue
        let r1 = unsafe {
            host_cmd_insert_component(
                core::ptr::null_mut(),
                ent,
                valid_name.as_ptr(),
                &pos as *const Position as *const u8,
                core::mem::size_of::<Position>(),
            )
        };
        assert_eq!(r1, FfiResult::NULL_POINTER);

        let r_spawn_null = unsafe { host_cmd_spawn_empty(core::ptr::null_mut()) };
        assert_eq!(r_spawn_null.0, 0);

        let r_despawn_null = unsafe { host_cmd_despawn(core::ptr::null_mut(), ent) };
        assert_eq!(r_despawn_null, FfiResult::NULL_POINTER);

        // 2. Null component name
        let r2 = unsafe {
            host_cmd_insert_component(
                raw_queue,
                ent,
                core::ptr::null(),
                &pos as *const Position as *const u8,
                core::mem::size_of::<Position>(),
            )
        };
        assert_eq!(r2, FfiResult::NULL_POINTER);

        // 3. Null data with len > 0
        let r3 = unsafe {
            host_cmd_insert_component(
                raw_queue,
                ent,
                valid_name.as_ptr(),
                core::ptr::null(),
                core::mem::size_of::<Position>(),
            )
        };
        assert_eq!(r3, FfiResult::NULL_POINTER);

        // 4. Unregistered component name
        let r4 = unsafe {
            host_cmd_insert_component(
                raw_queue,
                ent,
                unreg_name.as_ptr(),
                &pos as *const Position as *const u8,
                core::mem::size_of::<Position>(),
            )
        };
        assert_eq!(r4, FfiResult::INVALID_COMPONENT);

        // 5. Size mismatch
        let r5 = unsafe {
            host_cmd_insert_component(
                raw_queue,
                ent,
                valid_name.as_ptr(),
                &pos as *const Position as *const u8,
                core::mem::size_of::<Position>() - 1,
            )
        };
        assert_eq!(r5, FfiResult::INVALID_ARGUMENT);

        // 6. Invalid entity bits (0 returns None from Entity::try_from_bits)
        let invalid_ent = FfiEntity(0);
        assert!(Entity::try_from_bits(invalid_ent.0).is_none());

        let r6 = unsafe {
            host_cmd_insert_component(
                raw_queue,
                invalid_ent,
                valid_name.as_ptr(),
                &pos as *const Position as *const u8,
                core::mem::size_of::<Position>(),
            )
        };
        assert_eq!(r6, FfiResult::INVALID_ENTITY);

        let r7 = unsafe { host_cmd_despawn(raw_queue, invalid_ent) };
        assert_eq!(r7, FfiResult::INVALID_ENTITY);

        // 7. Non-existent entity that has valid bits: queues successfully, safely skips when applied
        let non_existent_ent = FfiEntity(Entity::from_raw_u32(999_999).unwrap().to_bits());
        let r8 = unsafe {
            host_cmd_insert_component(
                raw_queue,
                non_existent_ent,
                valid_name.as_ptr(),
                &pos as *const Position as *const u8,
                core::mem::size_of::<Position>(),
            )
        };
        assert_eq!(r8, FfiResult::OK);

        let r9 = unsafe { host_cmd_despawn(raw_queue, non_existent_ent) };
        assert_eq!(r9, FfiResult::OK);

        // Flush to world: must NOT panic, and non_existent_ent must not exist
        cmd_queue.apply(&mut world);
        let bevy_non_existent = Entity::from_bits(non_existent_ent.0);
        assert!(world.get_entity(bevy_non_existent).is_err());
    }
}

mod high_volume_chunks_and_commands {
    #![allow(unused_imports, dead_code)]
    //! Empirical Adversarial Verification & Stress Test Suite for Milestone 5
    //!
    //! Rigorous stress testing of:
    //! 1. Chunk iteration boundary conditions (0 entities, 1 entity, multi-chunk archetypes >16,384 entities).
    //! 2. Component access permission enforcement (READ vs WRITE vs FILTER_ONLY vs WITHOUT, invalid column indices).
    //! 3. Change detection granularity (blanket chunk mutation vs selective row-level `host_iter_mark_changed`).
    //! 4. High-volume deferred commands execution (2,000 rapid spawns, inserts, and 1,000 despawns).
    //! 5. Command bridge error resilience (null pointers, bad entity bits, mismatched sizes, unregistered components, invalid UTF-8).
    //! 6. Multi-query safety & nested iteration (self-conflict detection, compatible multi-queries, nested loops with `host_iter_reset`).
    //! 7. Cross-language binary struct memory layout matching (C99 vs Rust repr(C) byte-for-byte).
    //! 8. Cross-language concurrent runtime stress (Example 3 C99 + Rust mods running simultaneously under heavy entity loads and dynamic reload cycles).

    use crate::support::*;
    use bevy_abi_mod_host::{AbiModPlugin, ModAppExt, ModRegistry};
    use bevy_abi_mod_host_sys::{
        CommandQueueBridge, ComponentAccessDescOwned, DynamicCSystem, host_cmd_despawn,
        host_cmd_insert_component, host_cmd_spawn_empty, host_iter_column_mutable,
        host_iter_column_readonly, host_iter_entities, host_iter_mark_changed,
        host_iter_next_chunk, host_iter_reset,
    };
    use bevy_abi_mod_macros::ModExposable;
    use bevy_abi_mod_sys::{
        ComponentAccessMode, FfiEntity, FfiResult, RawChunkIter, RawCommandQueue,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::query::Changed;
    use bevy_ecs::schedule::IntoScheduleConfigs;
    use bevy_ecs::system::{Query, System};
    use std::ffi::{CString, c_char, c_void};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

    // ==============================================================================
    // Component Definitions for Challenger Tests
    // ==============================================================================

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
    #[repr(C)]
    pub struct DroneTag {
        pub squad: u32,
    }

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
    #[repr(C)]
    pub struct PlayerTag {
        pub id: u32,
    }

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
    #[repr(C)]
    pub struct ColorRgba {
        pub r: f32,
        pub g: f32,
        pub b: f32,
        pub a: f32,
    }

    // ==============================================================================
    // TEST 1: Chunk Iteration Boundaries (0 Entities & 1 Entity)
    // ==============================================================================

    static ZERO_ENTITY_VISITS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn zero_entity_callback(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        ZERO_ENTITY_VISITS.fetch_add(1, Ordering::SeqCst);

        // With 0 entities, next_chunk must immediately return 0
        let count = unsafe { host_iter_next_chunk(iter) };
        assert_eq!(count, 0, "Empty world chunk iteration must return 0");

        // All column pointers and entity pointers must be null
        let readonly_ptr = unsafe { host_iter_column_readonly(iter, 0) };
        assert!(
            readonly_ptr.is_null(),
            "Readonly column pointer must be null when chunk has 0 entities"
        );

        let mutable_ptr = unsafe { host_iter_column_mutable(iter, 1) };
        assert!(
            mutable_ptr.is_null(),
            "Mutable column pointer must be null when chunk has 0 entities"
        );

        let entities_ptr = unsafe { host_iter_entities(iter) };
        assert!(
            entities_ptr.is_null(),
            "Entities pointer must be null when chunk has 0 entities"
        );

        // Iter reset must succeed cleanly
        let reset_res = unsafe { host_iter_reset(iter) };
        assert_eq!(reset_res, FfiResult::OK);

        // Iterating again after reset must still return 0
        let count_again = unsafe { host_iter_next_chunk(iter) };
        assert_eq!(count_again, 0);

        FfiResult::OK
    }

    static SINGLE_ENTITY_VISITS: AtomicUsize = AtomicUsize::new(0);
    static SINGLE_ENTITY_ID_BITS: AtomicU32 = AtomicU32::new(0);

    unsafe extern "C" fn single_entity_callback(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        SINGLE_ENTITY_VISITS.fetch_add(1, Ordering::SeqCst);

        let count = unsafe { host_iter_next_chunk(iter) };
        assert_eq!(
            count, 1,
            "Single entity world chunk iteration must return exactly 1"
        );

        // Entity pointer must be non-null and valid
        let entities_ptr = unsafe { host_iter_entities(iter) };
        assert!(!entities_ptr.is_null(), "Entities pointer must not be null");
        let entity = unsafe { *entities_ptr };
        SINGLE_ENTITY_ID_BITS.store(entity.0 as u32, Ordering::SeqCst);

        // Read column 0 (Position)
        let pos_ptr = unsafe { host_iter_column_readonly(iter, 0) } as *const Position;
        assert!(!pos_ptr.is_null(), "Position pointer must be non-null");
        let pos = unsafe { *pos_ptr };
        assert_eq!(
            pos,
            Position {
                x: 42.0,
                y: 100.0,
                z: -5.0
            }
        );

        // Mutate column 1 (Velocity)
        let vel_ptr = unsafe { host_iter_column_mutable(iter, 1) } as *mut Velocity;
        assert!(!vel_ptr.is_null(), "Velocity pointer must be non-null");
        unsafe {
            (*vel_ptr).x += 10.0;
            (*vel_ptr).y += 20.0;
            (*vel_ptr).z += 30.0;
        }

        // Exhaust iterator
        let next_count = unsafe { host_iter_next_chunk(iter) };
        assert_eq!(
            next_count, 0,
            "Single entity world must be exhausted on second next_chunk"
        );

        FfiResult::OK
    }

    #[test]
    fn test_stress_chunk_iteration_zero_and_single_entity() {
        // --- Part A: Zero entities ---
        {
            ZERO_ENTITY_VISITS.store(0, Ordering::SeqCst);
            let mut app = App::new();
            app.add_plugins(AbiModPlugin::default());
            app.register_mod_component::<Position>();
            app.register_mod_component::<Velocity>();

            let registry = app.world().resource::<ModRegistry>().clone();
            let query_descs = vec![
                ComponentAccessDescOwned {
                    name: "Position".to_string(),
                    mode: ComponentAccessMode::READ,
                },
                ComponentAccessDescOwned {
                    name: "Velocity".to_string(),
                    mode: ComponentAccessMode::WRITE,
                },
            ];

            let system = DynamicCSystem::new(
                "zero_entity_system".to_string(),
                query_descs,
                zero_entity_callback,
                core::ptr::null_mut(),
                Arc::new(registry),
            );
            app.add_systems(Update, system);

            for _ in 0..5 {
                app.update();
            }
            assert_eq!(ZERO_ENTITY_VISITS.load(Ordering::SeqCst), 5);
        }

        // --- Part B: Single entity ---
        {
            SINGLE_ENTITY_VISITS.store(0, Ordering::SeqCst);
            let mut app = App::new();
            app.add_plugins(AbiModPlugin::default());
            app.register_mod_component::<Position>();
            app.register_mod_component::<Velocity>();

            let e = app
                .world_mut()
                .spawn((
                    Position {
                        x: 42.0,
                        y: 100.0,
                        z: -5.0,
                    },
                    Velocity {
                        x: 1.0,
                        y: 2.0,
                        z: 3.0,
                    },
                ))
                .id();

            let registry = app.world().resource::<ModRegistry>().clone();
            let query_descs = vec![
                ComponentAccessDescOwned {
                    name: "Position".to_string(),
                    mode: ComponentAccessMode::READ,
                },
                ComponentAccessDescOwned {
                    name: "Velocity".to_string(),
                    mode: ComponentAccessMode::WRITE,
                },
            ];

            let system = DynamicCSystem::new(
                "single_entity_system".to_string(),
                query_descs,
                single_entity_callback,
                core::ptr::null_mut(),
                Arc::new(registry),
            );
            app.add_systems(Update, system);

            app.update();
            assert_eq!(SINGLE_ENTITY_VISITS.load(Ordering::SeqCst), 1);
            assert_eq!(
                SINGLE_ENTITY_ID_BITS.load(Ordering::SeqCst),
                (e.to_bits() & 0xFFFF_FFFF) as u32
            );

            let vel = *app.world().get::<Velocity>(e).unwrap();
            assert_eq!(
                vel,
                Velocity {
                    x: 11.0,
                    y: 22.0,
                    z: 33.0
                }
            );
        }
    }

    // ==============================================================================
    // TEST 2: Multi-Chunk Archetypes (>16,384 entities: 25,000 entities across 5 archetypes)
    // ==============================================================================

    static MULTI_CHUNK_TOTAL_ENTITIES: AtomicUsize = AtomicUsize::new(0);
    static MULTI_CHUNK_TOTAL_CHUNKS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn multi_chunk_scale_callback(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        let mut total_entities = 0;
        let mut chunks = 0;

        loop {
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }
            chunks += 1;
            total_entities += count;

            let entities_ptr = unsafe { host_iter_entities(iter) };
            assert!(!entities_ptr.is_null());

            let pos_ptr = unsafe { host_iter_column_readonly(iter, 0) } as *const Position;
            assert!(!pos_ptr.is_null());
            assert_eq!((pos_ptr as usize) % core::mem::align_of::<Position>(), 0);

            let vel_ptr = unsafe { host_iter_column_mutable(iter, 1) } as *mut Velocity;
            assert!(!vel_ptr.is_null());
            assert_eq!((vel_ptr as usize) % core::mem::align_of::<Velocity>(), 0);

            // Mutate each entity's velocity
            for i in 0..count {
                let p = unsafe { *pos_ptr.add(i) };
                let v = unsafe { &mut *vel_ptr.add(i) };
                v.x += 1.0;
                v.y += p.x * 0.001;
            }
        }

        MULTI_CHUNK_TOTAL_ENTITIES.store(total_entities, Ordering::SeqCst);
        MULTI_CHUNK_TOTAL_CHUNKS.store(chunks, Ordering::SeqCst);

        // Test iterator reset
        let reset_status = unsafe { host_iter_reset(iter) };
        assert_eq!(reset_status, FfiResult::OK);

        let mut re_count = 0;
        let mut re_chunks = 0;
        loop {
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }
            re_chunks += 1;
            re_count += count;
        }
        assert_eq!(
            re_count, total_entities,
            "Iterator reset must re-traverse identical entity count"
        );
        assert_eq!(
            re_chunks, chunks,
            "Iterator reset must re-traverse identical chunk count"
        );

        FfiResult::OK
    }

    #[test]
    fn test_stress_chunk_iteration_multi_chunk_archetypes_25k() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin::default());
        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();
        app.register_mod_component::<CombatHealth>();
        app.register_mod_component::<Mass>();
        app.register_mod_component::<Armor>();

        // Spawn 25,000 entities partitioned across 5 distinct archetypes (5,000 entities each)
        const PER_ARCH: usize = 5_000;
        let mut spawned_entities = Vec::with_capacity(PER_ARCH * 5);

        // Archetype 1: Position, Velocity
        for i in 0..PER_ARCH {
            let e = app
                .world_mut()
                .spawn((
                    Position {
                        x: i as f32,
                        y: 0.0,
                        z: 0.0,
                    },
                    Velocity {
                        x: 10.0,
                        y: 20.0,
                        z: 30.0,
                    },
                ))
                .id();
            spawned_entities.push(e);
        }
        // Archetype 2: Position, Velocity, CombatHealth
        for i in 0..PER_ARCH {
            let e = app
                .world_mut()
                .spawn((
                    Position {
                        x: i as f32,
                        y: 0.0,
                        z: 0.0,
                    },
                    Velocity {
                        x: 10.0,
                        y: 20.0,
                        z: 30.0,
                    },
                    CombatHealth {
                        current: 100,
                        max: 100,
                    },
                ))
                .id();
            spawned_entities.push(e);
        }
        // Archetype 3: Position, Velocity, Mass
        for i in 0..PER_ARCH {
            let e = app
                .world_mut()
                .spawn((
                    Position {
                        x: i as f32,
                        y: 0.0,
                        z: 0.0,
                    },
                    Velocity {
                        x: 10.0,
                        y: 20.0,
                        z: 30.0,
                    },
                    Mass { value: 50.0 },
                ))
                .id();
            spawned_entities.push(e);
        }
        // Archetype 4: Position, Velocity, Armor
        for i in 0..PER_ARCH {
            let e = app
                .world_mut()
                .spawn((
                    Position {
                        x: i as f32,
                        y: 0.0,
                        z: 0.0,
                    },
                    Velocity {
                        x: 10.0,
                        y: 20.0,
                        z: 30.0,
                    },
                    Armor { defense: 25 },
                ))
                .id();
            spawned_entities.push(e);
        }
        // Archetype 5: Position, Velocity, CombatHealth, Armor
        for i in 0..PER_ARCH {
            let e = app
                .world_mut()
                .spawn((
                    Position {
                        x: i as f32,
                        y: 0.0,
                        z: 0.0,
                    },
                    Velocity {
                        x: 10.0,
                        y: 20.0,
                        z: 30.0,
                    },
                    CombatHealth {
                        current: 200,
                        max: 200,
                    },
                    Armor { defense: 50 },
                ))
                .id();
            spawned_entities.push(e);
        }

        assert_eq!(spawned_entities.len(), 25_000);

        let registry = app.world().resource::<ModRegistry>().clone();
        let query_descs = vec![
            ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::READ,
            },
            ComponentAccessDescOwned {
                name: "Velocity".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
        ];

        let system = DynamicCSystem::new(
            "multi_chunk_25k_system".to_string(),
            query_descs,
            multi_chunk_scale_callback,
            core::ptr::null_mut(),
            Arc::new(registry),
        );
        app.add_systems(Update, system);

        app.update();

        assert_eq!(MULTI_CHUNK_TOTAL_ENTITIES.load(Ordering::SeqCst), 25_000);
        assert_eq!(MULTI_CHUNK_TOTAL_CHUNKS.load(Ordering::SeqCst), 5);

        // Verify mutations across sample entities
        for &e in &[
            spawned_entities[0],
            spawned_entities[5000],
            spawned_entities[10000],
            spawned_entities[24999],
        ] {
            let v = *app.world().get::<Velocity>(e).unwrap();
            assert_eq!(v.x, 11.0);
        }
    }

    // ==============================================================================
    // TEST 3: Component Access Permission Enforcement
    // ==============================================================================

    static PERMISSIONS_VERIFIED: AtomicBool = AtomicBool::new(false);

    unsafe extern "C" fn access_permissions_callback(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            return FfiResult::OK;
        }

        // Col 0: Position (READ)
        let pos_readonly = unsafe { host_iter_column_readonly(iter, 0) };
        assert!(
            !pos_readonly.is_null(),
            "Readonly access on READ column must succeed"
        );
        let pos_mut = unsafe { host_iter_column_mutable(iter, 0) };
        assert!(
            pos_mut.is_null(),
            "Mutable access on READ column must be DENIED (null)"
        );
        let pos_mark = unsafe { host_iter_mark_changed(iter, 0, 0) };
        assert_eq!(
            pos_mark,
            FfiResult::PERMISSION_DENIED,
            "Mark changed on READ column must return PERMISSION_DENIED"
        );

        // Col 1: Velocity (WRITE)
        let vel_readonly = unsafe { host_iter_column_readonly(iter, 1) };
        assert!(
            !vel_readonly.is_null(),
            "Readonly access on WRITE column must succeed"
        );
        let vel_mut = unsafe { host_iter_column_mutable(iter, 1) };
        assert!(
            !vel_mut.is_null(),
            "Mutable access on WRITE column must succeed"
        );
        let vel_mark = unsafe { host_iter_mark_changed(iter, 1, 0) };
        assert_eq!(
            vel_mark,
            FfiResult::OK,
            "Mark changed on WRITE column must succeed"
        );

        // Col 2: CombatHealth (FILTER_ONLY)
        let health_readonly = unsafe { host_iter_column_readonly(iter, 2) };
        assert!(
            health_readonly.is_null(),
            "Read access on FILTER_ONLY column must return null"
        );
        let health_mut = unsafe { host_iter_column_mutable(iter, 2) };
        assert!(
            health_mut.is_null(),
            "Mutable access on FILTER_ONLY column must return null"
        );

        // Col 3: Mass (WITHOUT)
        let mass_readonly = unsafe { host_iter_column_readonly(iter, 3) };
        assert!(
            mass_readonly.is_null(),
            "Read access on WITHOUT column must return null"
        );
        let mass_mut = unsafe { host_iter_column_mutable(iter, 3) };
        assert!(
            mass_mut.is_null(),
            "Mutable access on WITHOUT column must return null"
        );

        // Out of bounds indices
        let oob_readonly = unsafe { host_iter_column_readonly(iter, 4) };
        assert!(oob_readonly.is_null());
        let oob_mut = unsafe { host_iter_column_mutable(iter, 32) };
        assert!(oob_mut.is_null());
        let oob_mark = unsafe { host_iter_mark_changed(iter, 100, 0) };
        assert_eq!(oob_mark, FfiResult::INVALID_COMPONENT);

        // Row out of bounds
        let row_oob_mark = unsafe { host_iter_mark_changed(iter, 1, 999_999) };
        assert_eq!(row_oob_mark, FfiResult::OUT_OF_RANGE);

        PERMISSIONS_VERIFIED.store(true, Ordering::SeqCst);
        FfiResult::OK
    }

    #[test]
    fn test_stress_component_access_permissions() {
        PERMISSIONS_VERIFIED.store(false, Ordering::SeqCst);

        let mut app = App::new();
        app.add_plugins(AbiModPlugin::default());
        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();
        app.register_mod_component::<CombatHealth>();
        app.register_mod_component::<Mass>();

        // Entity matching: Position, Velocity, CombatHealth (WITHOUT Mass)
        app.world_mut().spawn((
            Position {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            },
            Velocity {
                x: 0.1,
                y: 0.2,
                z: 0.3,
            },
            CombatHealth {
                current: 100,
                max: 100,
            },
        ));

        // Entity that must be filtered out because it has Mass
        app.world_mut().spawn((
            Position {
                x: 99.0,
                y: 99.0,
                z: 99.0,
            },
            Velocity {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            CombatHealth {
                current: 100,
                max: 100,
            },
            Mass { value: 10.0 },
        ));

        let registry = app.world().resource::<ModRegistry>().clone();
        let query_descs = vec![
            ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::READ,
            },
            ComponentAccessDescOwned {
                name: "Velocity".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            ComponentAccessDescOwned {
                name: "CombatHealth".to_string(),
                mode: ComponentAccessMode::FILTER_ONLY,
            },
            ComponentAccessDescOwned {
                name: "Mass".to_string(),
                mode: ComponentAccessMode::WITHOUT,
            },
        ];

        let system = DynamicCSystem::new(
            "access_permissions_system".to_string(),
            query_descs,
            access_permissions_callback,
            core::ptr::null_mut(),
            Arc::new(registry),
        );
        app.add_systems(Update, system);

        app.update();
        assert!(PERMISSIONS_VERIFIED.load(Ordering::SeqCst));
    }

    // ==============================================================================
    // TEST 4: Change Detection Granularity (Blanket vs Selective row-level)
    // ==============================================================================

    static BLANKET_CHANGE_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn blanket_mutation_callback(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count > 0 {
            // Calling column_mutable marks entire chunk as changed
            let vel_ptr = unsafe { host_iter_column_mutable(iter, 0) } as *mut Velocity;
            assert!(!vel_ptr.is_null());
            for i in 0..count {
                unsafe {
                    (*vel_ptr.add(i)).x += 1.0;
                }
            }
            BLANKET_CHANGE_CALL_COUNT.fetch_add(count, Ordering::SeqCst);
        }
        FfiResult::OK
    }

    static SELECTIVE_CHANGE_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);
    static SELECTIVE_MODE: AtomicUsize = AtomicUsize::new(0); // 0 = no-op, 1 = selective mark row 7
    const TARGET_ROW: usize = 7;

    unsafe extern "C" fn selective_mutation_callback(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        loop {
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }

            if SELECTIVE_MODE.load(Ordering::SeqCst) == 1 && count > TARGET_ROW {
                let vel_ptr = unsafe { host_iter_column_readonly(iter, 0) } as *mut Velocity;
                assert!(!vel_ptr.is_null());
                unsafe {
                    (*vel_ptr.add(TARGET_ROW)).x = 999.0;
                }
                let res = unsafe { host_iter_mark_changed(iter, 0, TARGET_ROW) };
                assert_eq!(res, FfiResult::OK);
                SELECTIVE_CHANGE_CALL_COUNT.fetch_add(1, Ordering::SeqCst);
            } else {
                // Readonly access without marking ticks
                let _ = unsafe { host_iter_column_readonly(iter, 0) };
            }
        }
        FfiResult::OK
    }

    static CHANGED_VELOCITY_DETECTIONS: AtomicUsize = AtomicUsize::new(0);

    fn count_changed_velocities(query: Query<&Velocity, Changed<Velocity>>) {
        let count = query.iter().count();
        CHANGED_VELOCITY_DETECTIONS.store(count, Ordering::SeqCst);
    }

    #[test]
    fn test_stress_change_detection_granularity() {
        // --- Phase 1: Blanket change detection ---
        {
            CHANGED_VELOCITY_DETECTIONS.store(0, Ordering::SeqCst);
            let mut app = App::new();
            app.add_plugins(AbiModPlugin::default());
            app.register_mod_component::<Velocity>();

            for i in 0..50 {
                app.world_mut().spawn(Velocity {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                });
            }

            let registry = app.world().resource::<ModRegistry>().clone();
            let system = DynamicCSystem::new(
                "blanket_system".to_string(),
                vec![ComponentAccessDescOwned {
                    name: "Velocity".to_string(),
                    mode: ComponentAccessMode::WRITE,
                }],
                blanket_mutation_callback,
                core::ptr::null_mut(),
                Arc::new(registry),
            );
            app.add_systems(Update, system);
            app.add_systems(Update, count_changed_velocities);

            // Tick 1: baseline
            app.update();
            // Tick 2: mutation via column_mutable -> all 50 entities detected
            app.update();
            assert_eq!(CHANGED_VELOCITY_DETECTIONS.load(Ordering::SeqCst), 50);
        }

        // --- Phase 2: Selective row-level change detection ---
        {
            CHANGED_VELOCITY_DETECTIONS.store(0, Ordering::SeqCst);
            SELECTIVE_MODE.store(0, Ordering::SeqCst);
            let mut app = App::new();
            app.add_plugins(AbiModPlugin::default());
            app.register_mod_component::<Velocity>();

            let mut entities = Vec::new();
            for i in 0..50 {
                let e = app
                    .world_mut()
                    .spawn(Velocity {
                        x: i as f32,
                        y: 0.0,
                        z: 0.0,
                    })
                    .id();
                entities.push(e);
            }

            let registry = app.world().resource::<ModRegistry>().clone();
            let system = DynamicCSystem::new(
                "selective_system".to_string(),
                vec![ComponentAccessDescOwned {
                    name: "Velocity".to_string(),
                    mode: ComponentAccessMode::WRITE,
                }],
                selective_mutation_callback,
                core::ptr::null_mut(),
                Arc::new(registry),
            );
            app.add_systems(Update, (system, count_changed_velocities).chain());

            // Frame 1: Initial spawn causes Bevy to detect all 50 entities as added/changed
            app.update();
            assert_eq!(CHANGED_VELOCITY_DETECTIONS.load(Ordering::SeqCst), 50);

            // Frame 2: Mode = 0 (no mutation). Must produce 0 changed entities (clean baseline)
            app.update();
            assert_eq!(
                CHANGED_VELOCITY_DETECTIONS.load(Ordering::SeqCst),
                0,
                "Non-mutating frame must produce 0 Changed<Velocity> detections"
            );

            // Frame 3: Mode = 1 (selectively mark ONLY TARGET_ROW changed)
            SELECTIVE_MODE.store(1, Ordering::SeqCst);
            app.update();

            // Exactly 1 entity must be detected as Changed<Velocity>!
            assert_eq!(
                CHANGED_VELOCITY_DETECTIONS.load(Ordering::SeqCst),
                1,
                "Selective change detection must flag exactly 1 entity, not the entire chunk"
            );

            let target_vel = *app.world().get::<Velocity>(entities[TARGET_ROW]).unwrap();
            assert_eq!(target_vel.x, 999.0);
        }
    }

    // ==============================================================================
    // TEST 5: High-Volume Deferred Commands Execution (2,000 Spawns, Inserts, 1,000 Despawns)
    // ==============================================================================

    const BATCH_SIZE: usize = 2_000;
    static SPAWNED_FFI_IDS: std::sync::Mutex<Vec<FfiEntity>> = std::sync::Mutex::new(Vec::new());

    unsafe extern "C" fn mass_spawn_callback(
        _iter: *mut RawChunkIter,
        commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        assert!(!commands.is_null());
        let mut ids = Vec::with_capacity(BATCH_SIZE);

        let pos_name = CString::new("Position").unwrap();
        let health_name = CString::new("CombatHealth").unwrap();

        for i in 0..BATCH_SIZE {
            let ffi_ent = unsafe { host_cmd_spawn_empty(commands) };
            assert_ne!(ffi_ent.0, 0);

            let pos = Position {
                x: i as f32,
                y: (i * 2) as f32,
                z: 0.0,
            };
            let res_pos = unsafe {
                host_cmd_insert_component(
                    commands,
                    ffi_ent,
                    pos_name.as_ptr(),
                    &pos as *const Position as *const u8,
                    core::mem::size_of::<Position>(),
                )
            };
            assert_eq!(res_pos, FfiResult::OK);

            let health = CombatHealth {
                current: 100,
                max: 100,
            };
            let res_health = unsafe {
                host_cmd_insert_component(
                    commands,
                    ffi_ent,
                    health_name.as_ptr(),
                    &health as *const CombatHealth as *const u8,
                    core::mem::size_of::<CombatHealth>(),
                )
            };
            assert_eq!(res_health, FfiResult::OK);

            ids.push(ffi_ent);
        }

        *SPAWNED_FFI_IDS.lock().unwrap() = ids;
        FfiResult::OK
    }

    unsafe extern "C" fn mass_despawn_and_insert_callback(
        _iter: *mut RawChunkIter,
        commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        assert!(!commands.is_null());
        let ids = SPAWNED_FFI_IDS.lock().unwrap().clone();
        let vel_name = CString::new("Velocity").unwrap();

        for (idx, &ffi_ent) in ids.iter().enumerate() {
            if idx % 2 == 0 {
                // Despawn even indices
                let res = unsafe { host_cmd_despawn(commands, ffi_ent) };
                assert_eq!(res, FfiResult::OK);
            } else {
                // Insert Velocity on odd indices
                let vel = Velocity {
                    x: idx as f32,
                    y: -1.0,
                    z: 1.0,
                };
                let res = unsafe {
                    host_cmd_insert_component(
                        commands,
                        ffi_ent,
                        vel_name.as_ptr(),
                        &vel as *const Velocity as *const u8,
                        core::mem::size_of::<Velocity>(),
                    )
                };
                assert_eq!(res, FfiResult::OK);
            }
        }
        FfiResult::OK
    }

    #[test]
    fn test_stress_deferred_commands_high_volume() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin::default());
        app.register_mod_component::<Position>();
        app.register_mod_component::<CombatHealth>();
        app.register_mod_component::<Velocity>();

        let registry = app.world().resource::<ModRegistry>().clone();

        // System 1: Mass spawn
        let spawn_system = DynamicCSystem::new(
            "mass_spawn".to_string(),
            Vec::new(),
            mass_spawn_callback,
            core::ptr::null_mut(),
            Arc::new(registry),
        );
        app.add_systems(Update, spawn_system);

        // Tick 1: Spawns queued and applied
        app.update();

        let spawned_ids = SPAWNED_FFI_IDS.lock().unwrap().clone();
        assert_eq!(spawned_ids.len(), BATCH_SIZE);

        // Verify all 2,000 entities exist with components
        for (i, &ffi_ent) in spawned_ids.iter().enumerate() {
            let bevy_ent = Entity::try_from_bits(ffi_ent.to_bits()).unwrap();
            let pos = *app
                .world()
                .get::<Position>(bevy_ent)
                .expect("Entity must have Position");
            let health = *app
                .world()
                .get::<CombatHealth>(bevy_ent)
                .expect("Entity must have CombatHealth");
            assert_eq!(pos.x, i as f32);
            assert_eq!(health.current, 100);
        }

        // Replace system with mass despawn/insert
        let mut app2 = App::new();
        app2.add_plugins(AbiModPlugin::default());
        app2.register_mod_component::<Position>();
        app2.register_mod_component::<CombatHealth>();
        app2.register_mod_component::<Velocity>();

        // Copy world entities over
        for (i, &ffi_ent) in spawned_ids.iter().enumerate() {
            let e = app2
                .world_mut()
                .spawn((
                    Position {
                        x: i as f32,
                        y: (i * 2) as f32,
                        z: 0.0,
                    },
                    CombatHealth {
                        current: 100,
                        max: 100,
                    },
                ))
                .id();
            assert_eq!(e.to_bits(), ffi_ent.to_bits());
        }

        let registry2 = app2.world().resource::<ModRegistry>().clone();
        let despawn_system = DynamicCSystem::new(
            "mass_despawn".to_string(),
            Vec::new(),
            mass_despawn_and_insert_callback,
            core::ptr::null_mut(),
            Arc::new(registry2),
        );
        app2.add_systems(Update, despawn_system);

        // Tick 2: Despawns and inserts queued and applied
        app2.update();

        // Verify results
        for (idx, &ffi_ent) in spawned_ids.iter().enumerate() {
            let bevy_ent = Entity::try_from_bits(ffi_ent.to_bits()).unwrap();
            if idx % 2 == 0 {
                // Despawned: entity mut should fail
                assert!(
                    app2.world().get_entity(bevy_ent).is_err(),
                    "Even entity {} must be despawned",
                    idx
                );
            } else {
                // Surviving: must have Position, CombatHealth, and Velocity
                assert!(app2.world().get::<Position>(bevy_ent).is_some());
                assert!(app2.world().get::<CombatHealth>(bevy_ent).is_some());
                let vel = *app2
                    .world()
                    .get::<Velocity>(bevy_ent)
                    .expect("Odd entity must have Velocity");
                assert_eq!(vel.x, idx as f32);
            }
        }
    }

    // ==============================================================================
    // TEST 6: Command Bridge Error Resilience (Adversarial Inputs)
    // ==============================================================================

    #[test]
    fn test_stress_deferred_commands_adversarial_inputs() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin::default());
        app.register_mod_component::<Position>();

        let mut queue = bevy_ecs::world::CommandQueue::default();
        let registry = app.world().resource::<ModRegistry>().clone();
        let cell = app.world_mut().as_unsafe_world_cell();

        let mut bridge = CommandQueueBridge::new(&mut queue, cell.entity_allocator(), &registry);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        let valid_entity = unsafe { host_cmd_spawn_empty(raw_queue) };
        assert_ne!(valid_entity.0, 0);

        let dummy_pos = Position {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        };
        let pos_name = CString::new("Position").unwrap();
        let unknown_name = CString::new("NonExistentComponent").unwrap();

        // 1. Null queue pointer
        assert_eq!(
            unsafe { host_cmd_spawn_empty(core::ptr::null_mut()) },
            FfiEntity(0)
        );
        assert_eq!(
            unsafe { host_cmd_despawn(core::ptr::null_mut(), valid_entity) },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe {
                host_cmd_insert_component(
                    core::ptr::null_mut(),
                    valid_entity,
                    pos_name.as_ptr(),
                    &dummy_pos as *const Position as *const u8,
                    core::mem::size_of::<Position>(),
                )
            },
            FfiResult::NULL_POINTER
        );

        // 2. Null component name
        assert_eq!(
            unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    valid_entity,
                    core::ptr::null(),
                    &dummy_pos as *const Position as *const u8,
                    core::mem::size_of::<Position>(),
                )
            },
            FfiResult::NULL_POINTER
        );

        // 3. Null data pointer with len > 0
        assert_eq!(
            unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    valid_entity,
                    pos_name.as_ptr(),
                    core::ptr::null(),
                    12,
                )
            },
            FfiResult::NULL_POINTER
        );

        // 4. Unregistered component name
        assert_eq!(
            unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    valid_entity,
                    unknown_name.as_ptr(),
                    &dummy_pos as *const Position as *const u8,
                    12,
                )
            },
            FfiResult::INVALID_COMPONENT
        );

        // 5. Component size mismatch (len != registered size)
        assert_eq!(
            unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    valid_entity,
                    pos_name.as_ptr(),
                    &dummy_pos as *const Position as *const u8,
                    999, // Expected 12
                )
            },
            FfiResult::INVALID_ARGUMENT
        );

        // 6. Invalid entity bits on despawn:
        // Generation 0 and Entity::PLACEHOLDER return INVALID_ENTITY
        assert_eq!(
            unsafe { host_cmd_despawn(raw_queue, FfiEntity(0)) },
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            unsafe {
                host_cmd_despawn(
                    raw_queue,
                    FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits()),
                )
            },
            FfiResult::INVALID_ENTITY
        );
        // Non-existent entity with non-zero generation returns OK (queued safely) and safely no-ops on flush
        assert_eq!(
            unsafe { host_cmd_despawn(raw_queue, FfiEntity(u64::MAX)) },
            FfiResult::OK
        );

        // 7. Invalid UTF-8 in component name
        let invalid_utf8_name = [0xFFu8, 0xFEu8, 0x00u8];
        assert_eq!(
            unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    valid_entity,
                    invalid_utf8_name.as_ptr() as *const c_char,
                    &dummy_pos as *const Position as *const u8,
                    12,
                )
            },
            FfiResult::INVALID_UTF8
        );
    }
}

mod command_overwrites_and_stack_scramble {
    #![allow(unused_imports, dead_code)]
    //! # Milestone 3 Empirical Challenger Stress Suite
    //!
    //! Written by Challenger 2 for Milestone 3: Multi-Language Guest SDK & Mod Implementations.
    //! Empirical Focus Areas:
    //! 1. Drop Trampoline Pool Conservation:
    //!    - Verify POD components (`RustModCustomData`, `CModCustomData`) consume ZERO slots in `DROP_TRAMPOLINE_SLOTS` (64-slot pool).
    //!    - Verify >64 distinct POD components can be registered without exhausting the pool.
    //!    - Verify non-POD components consume exactly 1 slot each, and slot 65 exhausts the pool.
    //!    - Verify slot deduplication / reuse upon component re-registration with identical UUID (mod reload).
    //!    - Verify panic containment in drop trampolines: catching unwinds cleanly without unwinding into the host or crashing.
    //! 2. Deferred Command Queue Stress & Host Allocator Isolation (Rule 2):
    //!    - Repeated rapid insertions and value overwrites on the same entity.
    //!    - Immediate entity despawning before command queue flush.
    //!    - Insertion on invalid / dead `FfiEntity`.
    //!    - Boundary sizes (0 bytes / ZST, 1 byte, 64 KB, power-of-two alignments).
    //!    - Stack memory scrambling defense (verifying host copy semantics and no dangling guest stack pointers).
    //! 3. Mod Unload & Lifecycle Stress:
    //!    - Full multi-language dynamic mod loading (`asymmetric_rust_mod` and `asymmetric_c_mod`).
    //!    - Continuous updates, clean mod unload, post-unload updates without segfaults or panics.
    //!    - Post-unload entity despawns.
    //!    - Repeated mod load/update/unload cycling stress (5 full cycles).

    use crate::support::*;
    use bevy_abi_mod_host::ModAppExt;
    use bevy_abi_mod_host::plugin::AbiModPlugin;
    use bevy_abi_mod_host_sys::registry::ModRegistry;
    use bevy_abi_mod_host_sys::{
        CommandQueueBridge, ModInitContext, host_cmd_despawn, host_cmd_insert_component,
        host_cmd_spawn_empty, host_register_component, reset_drop_trampolines_for_testing,
    };
    use bevy_abi_mod_sys::{
        ComponentUuid, FfiEntity, FfiResult, ModComponentDescriptor, RawCommandQueue,
    };
    use bevy_app::App;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::world::{CommandQueue, World};
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    static GLOBAL_DROP_TEST_LOCK: Mutex<()> = Mutex::new(());

    // ============================================================================
    // Types for Drop Trampoline Testing
    // ============================================================================

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct RustModCustomData {
        pub power: f32,
        pub shield: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct CModCustomData {
        pub mana: f32,
        pub level: i32,
    }

    static NON_POD_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn non_pod_drop_hook(ptr: *mut c_void) {
        if !ptr.is_null() {
            NON_POD_DROP_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }

    static PANICKING_DROP_INVOKED: AtomicBool = AtomicBool::new(false);

    unsafe extern "C" fn panicking_drop_trampoline_with_catch_unwind(ptr: *mut c_void) {
        if !ptr.is_null() {
            PANICKING_DROP_INVOKED.store(true, Ordering::SeqCst);
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                panic!("Simulated non-POD drop hook panic in guest runtime");
            }));
        }
    }

    // ============================================================================
    // 2. Deferred Command Queue Stress & Host Allocator Isolation (Rule 2)
    // ============================================================================

    #[test]
    fn test_cmd_insert_rapid_overwrites_on_same_entity() {
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
            c"RustModCustomData".as_ptr(),
            ComponentUuid::from_name("RustModCustomData"),
            core::mem::size_of::<RustModCustomData>(),
            core::mem::align_of::<RustModCustomData>(),
        );
        unsafe {
            host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
        };

        let reg = world.resource::<ModRegistry>().clone();
        let comp_id = reg.get_by_name("RustModCustomData").unwrap().component_id;

        let mut cmd_queue = CommandQueue::default();
        let mut bridge = CommandQueueBridge::new(&mut cmd_queue, world.entity_allocator(), &reg);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };

        // Issue 5 sequential overwrites
        for val in [10.0f32, 25.0, 50.0, 75.0, 99.0] {
            let data = RustModCustomData {
                power: val,
                shield: val as u32 * 2,
            };
            let status = unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    ffi_e,
                    c"RustModCustomData".as_ptr(),
                    &data as *const RustModCustomData as *const u8,
                    core::mem::size_of::<RustModCustomData>(),
                )
            };
            assert_eq!(status, FfiResult::OK);
        }

        cmd_queue.apply(&mut world);

        let bevy_entity = Entity::try_from_bits(ffi_e.0).unwrap();
        let entity_ref = world.entity(bevy_entity);
        let final_data = unsafe {
            *(entity_ref.get_by_id(comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        assert_eq!(
            final_data,
            RustModCustomData {
                power: 99.0,
                shield: 198
            },
            "Final overwrite must be present in archetype table"
        );
    }

    #[test]
    fn test_cmd_insert_and_despawn_before_flush() {
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
            c"CModCustomData".as_ptr(),
            ComponentUuid::from_name("CModCustomData"),
            core::mem::size_of::<CModCustomData>(),
            core::mem::align_of::<CModCustomData>(),
        );
        unsafe {
            host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
        };

        let reg = world.resource::<ModRegistry>().clone();
        let mut cmd_queue = CommandQueue::default();
        let mut bridge = CommandQueueBridge::new(&mut cmd_queue, world.entity_allocator(), &reg);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
        let c_data = CModCustomData {
            mana: 123.45,
            level: 42,
        };

        let res_insert = unsafe {
            host_cmd_insert_component(
                raw_queue,
                ffi_e,
                c"CModCustomData".as_ptr(),
                &c_data as *const CModCustomData as *const u8,
                core::mem::size_of::<CModCustomData>(),
            )
        };
        assert_eq!(res_insert, FfiResult::OK);

        // Despawn in the same frame BEFORE flushing
        let res_despawn = unsafe { host_cmd_despawn(raw_queue, ffi_e) };
        assert_eq!(res_despawn, FfiResult::OK);

        // Applying commands must not panic, and must drop AlignedBuffer cleanly
        cmd_queue.apply(&mut world);

        let bevy_entity = Entity::try_from_bits(ffi_e.0).unwrap();
        assert!(
            world.get_entity(bevy_entity).is_err(),
            "Entity must be despawned cleanly"
        );
    }

    #[test]
    fn test_cmd_insert_on_invalid_or_dead_entity() {
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
            c"CModCustomData".as_ptr(),
            ComponentUuid::from_name("CModCustomData"),
            core::mem::size_of::<CModCustomData>(),
            core::mem::align_of::<CModCustomData>(),
        );
        unsafe {
            host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
        };

        let reg = world.resource::<ModRegistry>().clone();
        let mut cmd_queue = CommandQueue::default();
        let mut bridge = CommandQueueBridge::new(&mut cmd_queue, world.entity_allocator(), &reg);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        // Entity that was never allocated
        let dead_entity = FfiEntity(0xDEAD_CAFE);
        let c_data = CModCustomData {
            mana: 1.0,
            level: 1,
        };
        let res = unsafe {
            host_cmd_insert_component(
                raw_queue,
                dead_entity,
                c"CModCustomData".as_ptr(),
                &c_data as *const CModCustomData as *const u8,
                core::mem::size_of::<CModCustomData>(),
            )
        };
        assert_eq!(res, FfiResult::OK);

        // Applying queue must safely skip the non-existent entity without crashing or leaking
        cmd_queue.apply(&mut world);
    }

    #[test]
    fn test_cmd_insert_boundary_sizes_and_alignments() {
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

        // 1. Zero-Sized Type (ZST)
        let desc_zst = ModComponentDescriptor::new(
            c"ZstComponent".as_ptr(),
            ComponentUuid::from_name("ZstComponent"),
            0,
            1,
        );
        unsafe {
            host_register_component(
                &mut init_ctx as *mut ModInitContext as *mut c_void,
                &desc_zst,
            )
        };

        // 2. Large 64 KB component
        const LARGE_SIZE: usize = 65536;
        let desc_large = ModComponentDescriptor::new(
            c"LargeComponent".as_ptr(),
            ComponentUuid::from_name("LargeComponent"),
            LARGE_SIZE,
            16,
        );
        unsafe {
            host_register_component(
                &mut init_ctx as *mut ModInitContext as *mut c_void,
                &desc_large,
            )
        };

        let reg = world.resource::<ModRegistry>().clone();
        let zst_id = reg.get_by_name("ZstComponent").unwrap().component_id;
        let large_id = reg.get_by_name("LargeComponent").unwrap().component_id;

        let mut cmd_queue = CommandQueue::default();
        let mut bridge = CommandQueueBridge::new(&mut cmd_queue, world.entity_allocator(), &reg);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };

        // Insert ZST (null pointer allowed when len == 0)
        let res_zst = unsafe {
            host_cmd_insert_component(
                raw_queue,
                ffi_e,
                c"ZstComponent".as_ptr(),
                core::ptr::null(),
                0,
            )
        };
        assert_eq!(res_zst, FfiResult::OK);

        // Insert Large Component with distinct byte pattern
        let mut large_buf = vec![0x5Au8; LARGE_SIZE];
        large_buf[0] = 0x11;
        large_buf[LARGE_SIZE - 1] = 0x99;

        let res_large = unsafe {
            host_cmd_insert_component(
                raw_queue,
                ffi_e,
                c"LargeComponent".as_ptr(),
                large_buf.as_ptr(),
                LARGE_SIZE,
            )
        };
        assert_eq!(res_large, FfiResult::OK);

        cmd_queue.apply(&mut world);

        let bevy_entity = Entity::try_from_bits(ffi_e.0).unwrap();
        let entity_ref = world.entity(bevy_entity);

        assert!(entity_ref.contains_id(zst_id));
        assert!(entity_ref.contains_id(large_id));

        let table_large_ptr = entity_ref.get_by_id(large_id).unwrap().as_ptr();
        let table_large_slice = unsafe { core::slice::from_raw_parts(table_large_ptr, LARGE_SIZE) };
        assert_eq!(table_large_slice[0], 0x11);
        assert_eq!(table_large_slice[LARGE_SIZE - 1], 0x99);
        assert_eq!(table_large_slice[100], 0x5A);
    }

    #[test]
    fn test_allocator_isolation_stack_memory_scramble_defense() {
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
            c"RustModCustomData".as_ptr(),
            ComponentUuid::from_name("RustModCustomData"),
            core::mem::size_of::<RustModCustomData>(),
            core::mem::align_of::<RustModCustomData>(),
        );
        unsafe {
            host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
        };

        let reg = world.resource::<ModRegistry>().clone();
        let comp_id = reg.get_by_name("RustModCustomData").unwrap().component_id;

        let mut cmd_queue = CommandQueue::default();
        let mut bridge = CommandQueueBridge::new(&mut cmd_queue, world.entity_allocator(), &reg);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };

        // Scope stack variable and overwrite it before flush
        {
            let mut stack_data = RustModCustomData {
                power: 777.0,
                shield: 888,
            };

            let status = unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    ffi_e,
                    c"RustModCustomData".as_ptr(),
                    &stack_data as *const RustModCustomData as *const u8,
                    core::mem::size_of::<RustModCustomData>(),
                )
            };
            assert_eq!(status, FfiResult::OK);

            // Adversarially scramble stack memory!
            stack_data.power = -9999.0;
            stack_data.shield = 0xDEADBEEF;
            assert_eq!(stack_data.shield, 0xDEADBEEF);
        }

        // Now apply commands to world: host must read from its own host-allocated copy!
        cmd_queue.apply(&mut world);

        let bevy_entity = Entity::try_from_bits(ffi_e.0).unwrap();
        let entity_ref = world.entity(bevy_entity);
        let read_data = unsafe {
            *(entity_ref.get_by_id(comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };

        assert_eq!(
            read_data,
            RustModCustomData {
                power: 777.0,
                shield: 888
            },
            "Host allocator isolation (Rule 2) verified: host memory was isolated from guest stack mutation"
        );
    }
}

mod multi_archetype_scaling_oracle {
    #![allow(unused_imports, dead_code)]
    use crate::support::*;
    use bevy_abi_mod_host::{AbiModPlugin, ModAppExt};
    use bevy_app::App;
    use std::collections::HashMap;

    /// Test 1: Extreme multi-archetype scaling (32 distinct archetypes, 13,000+ entities,
    /// empty table edge cases, single-entity table edge cases, prime entity counts,
    /// mid-stream dynamic archetype additions, and mathematical arithmetic oracle).
    #[test]
    fn test_extreme_multi_archetype_scaling_and_arithmetic_oracle() {
        let (rust_dylib, c_dylib) = ensure_guest_mods_built();
        reset_rust_mod_panic(&rust_dylib);

        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();
        app.register_mod_component::<CombatHealth>();
        app.register_mod_component::<NameTag>();
        app.register_mod_component::<MetadataTag>();
        app.register_mod_component::<Mass>();
        app.register_mod_component::<Armor>();

        // We will construct 32 distinct archetypes using a 5-bit mask for supplementary components:
        // Bit 0: Velocity
        // Bit 1: CombatHealth
        // Bit 2: NameTag
        // Bit 3: Mass
        // Bit 4: Armor
        let mut expected_arithmetic = HashMap::new();
        let mut entity_archetype_mask = HashMap::new();
        let mut total_spawned = 0;

        for mask in 0u32..32 {
            let count = match mask {
                0 => 0,   // Empty table edge case
                1 => 1,   // Single-entity table edge case
                2 => 7,   // Prime count edge case
                3 => 13,  // Prime count edge case
                4 => 64,  // Power-of-two alignment
                _ => 500, // Large chunk
            };

            for i in 0..count {
                let id = (mask * 10_000 + i) as usize;
                let init_pos = Position {
                    x: id as f32,
                    y: 0.0,
                    z: 0.0,
                };

                let entity_id = match mask {
                    0 => unreachable!(),
                    1 => app
                        .world_mut()
                        .spawn((
                            init_pos,
                            Velocity {
                                x: 1.0,
                                y: 2.0,
                                z: 0.0,
                            },
                        ))
                        .id(),
                    2 => app
                        .world_mut()
                        .spawn((
                            init_pos,
                            CombatHealth {
                                current: 1000,
                                max: 1000,
                            },
                        ))
                        .id(),
                    3 => app
                        .world_mut()
                        .spawn((
                            init_pos,
                            NameTag {
                                label: format!("tag_{id}"),
                            },
                        ))
                        .id(),
                    4 => app.world_mut().spawn((init_pos, Mass { value: 10.0 })).id(),
                    5 => app
                        .world_mut()
                        .spawn((
                            init_pos,
                            Velocity {
                                x: 1.0,
                                y: 2.0,
                                z: 0.0,
                            },
                            Armor { defense: 50 },
                        ))
                        .id(),
                    6 => app
                        .world_mut()
                        .spawn((
                            init_pos,
                            Velocity {
                                x: 1.0,
                                y: 2.0,
                                z: 0.0,
                            },
                            CombatHealth {
                                current: 1000,
                                max: 1000,
                            },
                            NameTag {
                                label: format!("hero_{id}"),
                            },
                        ))
                        .id(),
                    7 => app
                        .world_mut()
                        .spawn((init_pos, Mass { value: 20.0 }, Armor { defense: 100 }))
                        .id(),
                    _ => {
                        let has_vel = (mask & 1) != 0;
                        let has_health = (mask & 2) != 0;
                        let has_tag = (mask & 4) != 0;
                        let has_mass = (mask & 8) != 0;
                        let has_armor = (mask & 16) != 0;

                        let mut entity_cmds = app.world_mut().spawn(init_pos);
                        if has_vel {
                            entity_cmds.insert(Velocity {
                                x: 1.0,
                                y: 2.0,
                                z: 0.0,
                            });
                        }
                        if has_health {
                            entity_cmds.insert(CombatHealth {
                                current: 1000,
                                max: 1000,
                            });
                        }
                        if has_tag {
                            entity_cmds.insert(NameTag {
                                label: format!("ent_{id}"),
                            });
                        }
                        if has_mass {
                            entity_cmds.insert(Mass { value: 5.0 });
                        }
                        if has_armor {
                            entity_cmds.insert(Armor { defense: 10 });
                        }
                        entity_cmds.id()
                    }
                };

                entity_archetype_mask.insert(entity_id, mask);
                expected_arithmetic.insert(entity_id, init_pos);
                total_spawned += 1;
            }
        }

        assert!(
            total_spawned > 13_000,
            "Spawned entity count must be > 13,000 across 32 archetypes"
        );

        let r_id = app.load_mod_tracked(&rust_dylib).expect("load rust mod");
        let c_id = app.load_mod_tracked(&c_dylib).expect("load c mod");

        const TICKS: usize = 10;
        for tick in 1..=TICKS {
            app.update();

            // On tick 5: edge condition - mid-stream archetype mutation (dynamic entity additions)
            if tick == 5 {
                for j in 0..200 {
                    let id = 999_000 + j;
                    let init_pos = Position {
                        x: id as f32,
                        y: 0.0,
                        z: 0.0,
                    };
                    let e = app
                        .world_mut()
                        .spawn((
                            init_pos,
                            Velocity {
                                x: 1.0,
                                y: 2.0,
                                z: 0.0,
                            },
                            Mass { value: 99.0 },
                            Armor { defense: 99 },
                            MetadataTag {
                                label: format!("mid_stream_{j}"),
                            },
                        ))
                        .id();

                    entity_archetype_mask.insert(e, 0xFF);
                    let mut expected_after_5 = init_pos;
                    expected_after_5.x += 5.0 * 1.0;
                    expected_after_5.y += 5.0 * 2.0;
                    expected_after_5.z += 5.0 * 1.0;
                    expected_arithmetic.insert(e, expected_after_5);
                }
            }
        }

        for (&entity, &mask) in &entity_archetype_mask {
            if mask == 0xFF {
                continue;
            }

            let init = expected_arithmetic[&entity];
            let has_vel = match mask {
                1 | 5 | 6 => true,
                2 | 3 | 4 | 7 => false,
                _ => (mask & 1) != 0,
            };

            let mut expected = init;
            if has_vel {
                expected.x += (TICKS as f32) * 1.0;
                expected.y += (TICKS as f32) * 2.0;
            }
            expected.z += (TICKS as f32) * 1.0;

            expected_arithmetic.insert(entity, expected);
        }

        let world = app.world();
        let mut verified_count = 0;

        for (&entity, &expected) in &expected_arithmetic {
            let actual = world
                .get::<Position>(entity)
                .expect("Entity must have Position");
            assert_eq!(
                actual.x, expected.x,
                "Entity {:?} (mask {}) x-coord mismatch",
                entity, entity_archetype_mask[&entity]
            );
            assert_eq!(
                actual.y, expected.y,
                "Entity {:?} (mask {}) y-coord mismatch",
                entity, entity_archetype_mask[&entity]
            );
            assert_eq!(
                actual.z, expected.z,
                "Entity {:?} (mask {}) z-coord mismatch",
                entity, entity_archetype_mask[&entity]
            );
            verified_count += 1;
        }

        assert_eq!(
            verified_count,
            expected_arithmetic.len(),
            "All entities must be verified against arithmetic oracle"
        );

        app.unload_mod(r_id).unwrap();
        app.unload_mod(c_id).unwrap();
    }

    /// Test 2: Scale stress test across 100,000 entities in 16 fragmented archetypes.
    /// Tests chunk iteration traversal across 100,000 entities with disjoint query matching:
    /// 50,000 entities match (Position + Velocity) while 50,000 entities match (Position + Mass without Velocity).
    /// Verifies that chunk iteration processes all 100k entities without memory corruption or cross-archetype bleed.
    #[test]
    fn test_100k_scale_fragmented_archetype_traversal_and_arithmetic() {
        let (rust_dylib, c_dylib) = ensure_guest_mods_built();
        reset_rust_mod_panic(&rust_dylib);

        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();
        app.register_mod_component::<Mass>();
        app.register_mod_component::<Armor>();

        const SCALE: usize = 100_000;
        const ARCHETYPES: usize = 16;
        const PER_ARCH: usize = SCALE / ARCHETYPES;

        // Spawn 100,000 entities across 16 archetypes:
        // Even archetypes (0, 2, 4, ... 14): Position + Velocity + Armor(i) -> 50,000 entities
        // Odd archetypes (1, 3, 5, ... 15): Position + Mass(i) (NO Velocity) -> 50,000 entities
        for arch in 0..ARCHETYPES {
            let is_kinematic = arch % 2 == 0;
            for i in 0..PER_ARCH {
                let id = arch * PER_ARCH + i;
                let p = Position {
                    x: id as f32,
                    y: 0.0,
                    z: 0.0,
                };
                if is_kinematic {
                    app.world_mut().spawn((
                        p,
                        Velocity {
                            x: 1.0,
                            y: 3.0,
                            z: 0.0,
                        },
                        Armor {
                            defense: arch as i32,
                        },
                    ));
                } else {
                    app.world_mut().spawn((p, Mass { value: arch as f32 }));
                }
            }
        }

        let r_id = app.load_mod_tracked(&rust_dylib).unwrap();
        let c_id = app.load_mod_tracked(&c_dylib).unwrap();

        // Run 5 ticks
        for _ in 0..5 {
            app.update();
        }

        // Verify 50,000 kinematic entities:
        // Rust mod: pos.x += 5 * 1.0 = 5.0, pos.y += 5 * 3.0 = 15.0
        // C mod: pos.z += 5 * 1.0 = 5.0 (since vz == 0.0)
        let mut kin_count = 0;
        let mut non_kin_count = 0;

        let mut query = app
            .world_mut()
            .query::<(&Position, Option<&Velocity>, Option<&Mass>)>();
        for (pos, vel, mass) in query.iter(app.world()) {
            if vel.is_some() {
                assert_eq!(pos.y, 15.0, "Kinematic entity pos.y must be 15.0");
                assert_eq!(pos.z, 5.0, "Kinematic entity pos.z must be 5.0");
                kin_count += 1;
            } else if mass.is_some() {
                assert_eq!(pos.y, 0.0, "Non-kinematic entity pos.y must remain 0.0");
                assert_eq!(
                    pos.z, 5.0,
                    "Non-kinematic entity pos.z must be 5.0 from C mod"
                );
                non_kin_count += 1;
            }
        }

        assert_eq!(
            kin_count, 50_000,
            "Must have exactly 50,000 kinematic entities"
        );
        assert_eq!(
            non_kin_count, 50_000,
            "Must have exactly 50,000 non-kinematic entities"
        );

        app.unload_mod(r_id).unwrap();
        app.unload_mod(c_id).unwrap();
    }
}

mod change_detection_archetype_isolation {
    #![allow(unused_imports, dead_code)]
    use bevy_abi_mod_host::{AbiModPlugin, ModAppExt, ModRegistry};
    use bevy_abi_mod_host_sys::{
        DynamicCSystem, host_iter_column_mutable, host_iter_column_readonly,
        host_iter_mark_changed, host_iter_next_chunk,
    };
    use bevy_abi_mod_macros::ModExposable;
    use bevy_abi_mod_sys::{ComponentAccessMode, FfiResult, RawChunkIter, RawCommandQueue};
    use bevy_app::{App, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::prelude::{Resource, World};
    use bevy_ecs::query::Changed;
    use bevy_ecs::schedule::{IntoScheduleConfigs, LogLevel, Schedule, ScheduleBuildSettings};
    use bevy_ecs::system::{Query, ResMut, System, SystemAccess};
    use core::ffi::c_void;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
            vec![bevy_abi_mod_host_sys::ComponentAccessDescOwned {
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
                bevy_abi_mod_host_sys::ComponentAccessDescOwned {
                    name: "Position".to_string(),
                    mode: ComponentAccessMode::WRITE,
                },
                bevy_abi_mod_host_sys::ComponentAccessDescOwned {
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
            vec![bevy_abi_mod_host_sys::ComponentAccessDescOwned {
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
}
