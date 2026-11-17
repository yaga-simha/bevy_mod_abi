//! Multi-archetype chunk iteration, change-detection granularity, and high-volume deferred command queues (`ABI 0.2.0`).

mod archetype_and_permission_boundaries {
    use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModRegistry};
    use bevy_abi_mod_host_sys::{
        BamQuerySpecOwned, GuestSystem, HostCommands, HostCommandsStorage, HostIter, LoadContext,
        QueryPlan, QueryTermMeta, host_cmd_despawn, host_cmd_insert, host_cmd_spawn_empty,
        host_get_proc_address, host_iter_column_info, host_iter_column_mutable,
        host_iter_column_readonly, host_iter_entities, host_iter_mark_changed_range,
        host_iter_next_chunk, host_register_system, host_sys_commands, host_sys_query,
    };
    use bevy_abi_mod_macros::ModComponent;
    use bevy_abi_mod_sys::{
        AccessMode, BamColumnInfo, BamCommands, BamComponent, BamIter, BamQueryDesc, BamSystemCtx,
        BamSystemDesc, BamTermDesc, ComponentUuid, FfiEntity, FfiResult, MAX_TERMS_PER_QUERY,
        ScheduleId,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::change_detection::Tick;
    use bevy_ecs::component::Component;
    use bevy_ecs::prelude::World;
    use bevy_ecs::system::System;
    use core::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[repr(C)]
    #[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
    #[mod_component(name = "ffi::chunk_bounds::Position")]
    pub struct Position {
        pub x: f32,
        pub y: f32,
        pub z: f32,
    }

    #[repr(C)]
    #[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
    #[mod_component(name = "ffi::chunk_bounds::Velocity")]
    pub struct Velocity {
        pub x: f32,
        pub y: f32,
        pub z: f32,
    }

    #[repr(C)]
    #[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
    #[mod_component(name = "ffi::chunk_bounds::Armor")]
    pub struct Armor {
        pub defense: i32,
    }

    macro_rules! define_tags {
        ($($tag:ident => $cname:literal),* $(,)?) => {
            $(
                #[repr(C)]
                #[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
                #[mod_component(name = $cname)]
                pub struct $tag {
                    pub id: u32,
                }
            )*
        };
    }

    define_tags!(
        Tag01 => "ffi::chunk_bounds::Tag01",
        Tag02 => "ffi::chunk_bounds::Tag02",
        Tag03 => "ffi::chunk_bounds::Tag03",
        Tag04 => "ffi::chunk_bounds::Tag04",
        Tag05 => "ffi::chunk_bounds::Tag05",
        Tag06 => "ffi::chunk_bounds::Tag06",
        Tag07 => "ffi::chunk_bounds::Tag07",
        Tag08 => "ffi::chunk_bounds::Tag08",
        Tag09 => "ffi::chunk_bounds::Tag09",
        Tag10 => "ffi::chunk_bounds::Tag10",
        Tag11 => "ffi::chunk_bounds::Tag11",
        Tag12 => "ffi::chunk_bounds::Tag12",
        Tag13 => "ffi::chunk_bounds::Tag13",
        Tag14 => "ffi::chunk_bounds::Tag14",
        Tag15 => "ffi::chunk_bounds::Tag15",
        Tag16 => "ffi::chunk_bounds::Tag16",
        Tag17 => "ffi::chunk_bounds::Tag17",
        Tag18 => "ffi::chunk_bounds::Tag18",
        Tag19 => "ffi::chunk_bounds::Tag19",
        Tag20 => "ffi::chunk_bounds::Tag20",
    );

    struct MultiTableCounters {
        table_visits: AtomicUsize,
        entity_visits: AtomicUsize,
    }

    unsafe extern "C" fn multi_table_stress_system(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let counters = unsafe { &*(user_data as *const MultiTableCounters) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }

        let mut local_tables = 0usize;
        let mut local_entities = 0usize;
        let mut count = 0usize;

        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
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

        counters
            .table_visits
            .fetch_add(local_tables, Ordering::SeqCst);
        counters
            .entity_visits
            .fetch_add(local_entities, Ordering::SeqCst);
        FfiResult::OK
    }

    #[test]
    fn test_extreme_archetype_table_counts() {
        let counters = Box::new(MultiTableCounters {
            table_visits: AtomicUsize::new(0),
            entity_visits: AtomicUsize::new(0),
        });

        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();

        macro_rules! register_and_spawn {
            ($($tag:ident, $idx:expr);* $(;)?) => {
                $(
                    app.register_mod_component::<$tag>(ComponentOptions::new()).unwrap();
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
            Tag16, 16; Tag17, 17; Tag18, 18; Tag19, 19; Tag20, 20;
        );

        for i in 0..10 {
            app.world_mut()
                .spawn((Tag01 { id: 100 + i }, Tag02 { id: 200 + i }));
        }

        let registry = app.world().resource::<ModRegistry>().clone();
        let sys = GuestSystem::new(
            "multi_table_stress",
            vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                pos_h,
                AccessMode::WRITE,
            )])],
            multi_table_stress_system,
            (&*counters as *const MultiTableCounters)
                .cast_mut()
                .cast::<c_void>(),
            registry,
        );
        app.add_systems(Update, sys);

        app.update();
        assert_eq!(counters.table_visits.load(Ordering::SeqCst), 20);
        assert_eq!(counters.entity_visits.load(Ordering::SeqCst), 100);

        let mut query = app.world_mut().query::<&Position>();
        let mut count = 0;
        for pos in query.iter(app.world()) {
            assert_eq!(pos.y, 20.0);
            assert_eq!(pos.z, 30.0);
            count += 1;
        }
        assert_eq!(count, 100);

        counters.table_visits.store(0, Ordering::SeqCst);
        counters.entity_visits.store(0, Ordering::SeqCst);
        app.update();
        assert_eq!(counters.table_visits.load(Ordering::SeqCst), 20);
        assert_eq!(counters.entity_visits.load(Ordering::SeqCst), 100);
    }

    #[test]
    fn test_empty_table_iteration_and_despawn_fragmentation() {
        let mut world = World::new();
        let pos_id = world.register_component::<Position>();
        let vel_id = world.register_component::<Velocity>();
        world.register_component::<Tag01>();
        world.register_component::<Tag02>();
        world.register_component::<Tag03>();

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

        for e in t1_entities {
            world.despawn(e);
        }
        for e in t3_entities {
            world.despawn(e);
        }

        let plan = QueryPlan::from_terms(vec![
            QueryTermMeta {
                component: BamComponent::from_raw(1),
                component_id: pos_id,
                mode: AccessMode::WRITE,
                uuid: ComponentUuid::from_canonical_name("ffi::chunk_bounds::Position"),
                size: core::mem::size_of::<Position>(),
                align: core::mem::align_of::<Position>(),
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: BamComponent::from_raw(2),
                component_id: vel_id,
                mode: AccessMode::READ,
                uuid: ComponentUuid::from_canonical_name("ffi::chunk_bounds::Velocity"),
                size: core::mem::size_of::<Velocity>(),
                align: core::mem::align_of::<Velocity>(),
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
        ]);

        let mut matched_tables = Vec::new();
        {
            let cell = world.as_unsafe_world_cell();
            for arch in cell.archetypes().iter() {
                if arch.contains(pos_id) && arch.contains(vel_id) {
                    let tid = arch.table_id();
                    if !matched_tables.contains(&tid) {
                        matched_tables.push(tid);
                    }
                }
            }
            assert_eq!(matched_tables.len(), 3);

            let storages = unsafe { cell.storages() };
            let mut host_iter = HostIter::new_dense(
                &storages.tables,
                &matched_tables,
                &plan,
                Tick::new(0),
                cell.change_tick(),
            );
            let raw_iter = host_iter.as_ffi_ptr();

            let mut chunk1_count = 0usize;
            assert!(unsafe { host_iter_next_chunk(raw_iter, &mut chunk1_count) }.get());
            assert_eq!(chunk1_count, 10);
            assert!(!unsafe { host_iter_entities(raw_iter) }.is_null());

            let mut chunk2_count = 99usize;
            assert!(!unsafe { host_iter_next_chunk(raw_iter, &mut chunk2_count) }.get());
            assert_eq!(chunk2_count, 0);

            assert!(!unsafe { host_iter_next_chunk(raw_iter, &mut chunk2_count) }.get());
            assert_eq!(chunk2_count, 0);

            assert!(unsafe { host_iter_column_readonly(raw_iter, 0) }.is_null());
            assert!(unsafe { host_iter_column_mutable(raw_iter, 0) }.is_null());
            assert!(unsafe { host_iter_entities(raw_iter) }.is_null());
        }

        for e in t2_entities {
            world.despawn(e);
        }

        let cell_all_empty = world.as_unsafe_world_cell();
        let storages_all_empty = unsafe { cell_all_empty.storages() };
        let mut empty_iter = HostIter::new_dense(
            &storages_all_empty.tables,
            &matched_tables,
            &plan,
            Tick::new(0),
            cell_all_empty.change_tick(),
        );
        let raw_empty = empty_iter.as_ffi_ptr();

        let mut empty_len = 99usize;
        assert!(!unsafe { host_iter_next_chunk(raw_empty, &mut empty_len) }.get());
        assert_eq!(empty_len, 0);
        assert!(unsafe { host_iter_column_readonly(raw_empty, 0) }.is_null());
        assert!(unsafe { host_iter_column_mutable(raw_empty, 0) }.is_null());
        assert!(unsafe { host_iter_entities(raw_empty) }.is_null());
    }

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

        let plan = QueryPlan::from_terms(vec![
            QueryTermMeta {
                component: BamComponent::from_raw(1),
                component_id: pos_id,
                mode: AccessMode::WRITE,
                uuid: ComponentUuid::from_canonical_name("ffi::chunk_bounds::Position"),
                size: core::mem::size_of::<Position>(),
                align: core::mem::align_of::<Position>(),
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: BamComponent::from_raw(2),
                component_id: vel_id,
                mode: AccessMode::READ,
                uuid: ComponentUuid::from_canonical_name("ffi::chunk_bounds::Velocity"),
                size: core::mem::size_of::<Velocity>(),
                align: core::mem::align_of::<Velocity>(),
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: BamComponent::from_raw(3),
                component_id: arm_id,
                mode: AccessMode::FILTER_ONLY,
                uuid: ComponentUuid::from_canonical_name("ffi::chunk_bounds::Armor"),
                size: core::mem::size_of::<Armor>(),
                align: core::mem::align_of::<Armor>(),
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
        ]);

        let matched_tables = vec![cell.archetypes().iter().last().unwrap().table_id()];
        let mut host_iter = HostIter::new_dense(
            &storages.tables,
            &matched_tables,
            &plan,
            Tick::new(0),
            cell.change_tick(),
        );
        let raw_iter = host_iter.as_ffi_ptr();

        let mut count = 0usize;
        assert!(unsafe { host_iter_next_chunk(raw_iter, &mut count) }.get());
        assert_eq!(count, 1);

        assert!(!unsafe { host_iter_column_mutable(raw_iter, 0) }.is_null());
        assert!(!unsafe { host_iter_column_readonly(raw_iter, 0) }.is_null());

        assert!(unsafe { host_iter_column_mutable(raw_iter, 1) }.is_null());
        assert!(!unsafe { host_iter_column_readonly(raw_iter, 1) }.is_null());

        assert!(unsafe { host_iter_column_mutable(raw_iter, 2) }.is_null());
        assert!(unsafe { host_iter_column_readonly(raw_iter, 2) }.is_null());

        assert!(unsafe { host_iter_column_mutable(raw_iter, 3) }.is_null());
        assert!(unsafe { host_iter_column_mutable(raw_iter, 100) }.is_null());
        assert!(unsafe { host_iter_column_mutable(raw_iter, usize::MAX) }.is_null());
        assert!(unsafe { host_iter_column_readonly(raw_iter, 3) }.is_null());
        assert!(unsafe { host_iter_column_readonly(raw_iter, 100) }.is_null());
        assert!(unsafe { host_iter_column_readonly(raw_iter, usize::MAX) }.is_null());

        assert_eq!(
            unsafe { host_iter_mark_changed_range(raw_iter, 1, 0, 1) },
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            unsafe { host_iter_mark_changed_range(raw_iter, 2, 0, 1) },
            FfiResult::PERMISSION_DENIED
        );
    }

    #[test]
    fn test_out_of_bounds_row_index_on_iter_mark_changed() {
        let mut world = World::new();
        let pos_id = world.register_component::<Position>();

        for i in 0..5 {
            world.spawn(Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            });
        }

        let cell = world.as_unsafe_world_cell();
        let storages = unsafe { cell.storages() };

        let plan = QueryPlan::from_terms(vec![QueryTermMeta {
            component: BamComponent::from_raw(1),
            component_id: pos_id,
            mode: AccessMode::WRITE,
            uuid: ComponentUuid::from_canonical_name("ffi::chunk_bounds::Position"),
            size: core::mem::size_of::<Position>(),
            align: core::mem::align_of::<Position>(),
            storage_flags: 0,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        }]);

        let matched_tables = vec![cell.archetypes().iter().last().unwrap().table_id()];
        let mut host_iter = HostIter::new_dense(
            &storages.tables,
            &matched_tables,
            &plan,
            Tick::new(0),
            cell.change_tick(),
        );
        let raw_iter = host_iter.as_ffi_ptr();

        let mut count = 0usize;
        assert!(unsafe { host_iter_next_chunk(raw_iter, &mut count) }.get());
        assert_eq!(count, 5);

        assert_eq!(
            unsafe { host_iter_mark_changed_range(raw_iter, 0, 0, 1) },
            FfiResult::OK
        );
        assert_eq!(
            unsafe { host_iter_mark_changed_range(raw_iter, 0, 4, 1) },
            FfiResult::OK
        );

        assert_eq!(
            unsafe { host_iter_mark_changed_range(raw_iter, 0, 5, 1) },
            FfiResult::OUT_OF_RANGE
        );
        assert_eq!(
            unsafe { host_iter_mark_changed_range(raw_iter, 0, 6, 1) },
            FfiResult::OUT_OF_RANGE
        );
        assert_eq!(
            unsafe { host_iter_mark_changed_range(raw_iter, 0, 1000, 1) },
            FfiResult::OUT_OF_RANGE
        );
        assert_eq!(
            unsafe { host_iter_mark_changed_range(raw_iter, 0, usize::MAX, 1) },
            FfiResult::OUT_OF_RANGE
        );

        assert_eq!(
            unsafe { host_iter_mark_changed_range(raw_iter, 1, 0, 1) },
            FfiResult::OUT_OF_RANGE
        );
        assert_eq!(
            unsafe { host_iter_mark_changed_range(raw_iter, 999, 0, 1) },
            FfiResult::OUT_OF_RANGE
        );

        assert_eq!(
            unsafe { host_iter_mark_changed_range(core::ptr::null_mut(), 0, 0, 1) },
            FfiResult::NULL_POINTER
        );
    }

    #[test]
    fn test_rapid_command_queue_bridge_10k_commands() {
        let mut world = World::new();
        let pos_id = world.register_component::<Position>();

        let mut registry = ModRegistry::new();
        let pos_h = registry
            .register_component::<Position>(pos_id, ComponentOptions::new())
            .unwrap();
        let snapshot = registry.snapshot_arc();

        let initial_count = world.entities().len();
        const CMD_COUNT: usize = 10_000;
        let mut spawned_entities = Vec::with_capacity(CMD_COUNT);
        let mut storage = HostCommandsStorage::new();

        {
            let cell = world.as_unsafe_world_cell();
            let mut cmds = HostCommands::new(&mut storage, cell.entity_allocator(), &snapshot);
            let raw_cmds = cmds.as_ffi_ptr();

            for i in 0..CMD_COUNT {
                let mut entity = FfiEntity::NULL;
                assert_eq!(
                    unsafe { host_cmd_spawn_empty(raw_cmds, &mut entity) },
                    FfiResult::OK
                );
                assert!(!entity.is_null());

                let pos = Position {
                    x: i as f32,
                    y: (i * 2) as f32,
                    z: (i * 3) as f32,
                };
                assert_eq!(
                    unsafe {
                        host_cmd_insert(
                            raw_cmds,
                            entity,
                            pos_h,
                            &pos as *const Position as *const u8,
                            core::mem::size_of::<Position>(),
                        )
                    },
                    FfiResult::OK
                );
                spawned_entities.push(entity);
            }
        }

        assert_eq!(spawned_entities.len(), CMD_COUNT);
        assert_eq!(world.entities().len(), initial_count);

        storage.apply_to_world(&mut world, &snapshot);

        assert_eq!(world.query::<&Position>().iter(&world).count(), CMD_COUNT);
        for (i, &ffi_entity) in spawned_entities.iter().enumerate() {
            let bevy_entity = bevy_ecs::entity::Entity::from_bits(ffi_entity.to_bits());
            let pos = world.get::<Position>(bevy_entity).unwrap();
            assert_eq!(pos.x, i as f32);
            assert_eq!(pos.y, (i * 2) as f32);
            assert_eq!(pos.z, (i * 3) as f32);
        }

        {
            let cell = world.as_unsafe_world_cell();
            let mut cmds = HostCommands::new(&mut storage, cell.entity_allocator(), &snapshot);
            let raw_cmds = cmds.as_ffi_ptr();
            for &ffi_entity in &spawned_entities {
                assert_eq!(
                    unsafe { host_cmd_despawn(raw_cmds, ffi_entity) },
                    FfiResult::OK
                );
            }
        }

        storage.apply_to_world(&mut world, &snapshot);
        assert_eq!(world.query::<&Position>().iter(&world).count(), 0);
        for &ffi_entity in &spawned_entities {
            let bevy_entity = bevy_ecs::entity::Entity::from_bits(ffi_entity.to_bits());
            assert!(world.get_entity(bevy_entity).is_err());
        }
    }

    #[test]
    fn test_defensive_boundary_and_null_pointer_torture() {
        let mut world = World::new();
        let pos_id = world.register_component::<Position>();

        let mut registry = ModRegistry::new();
        let pos_h = registry
            .register_component::<Position>(pos_id, ComponentOptions::new())
            .unwrap();
        let snapshot = registry.snapshot_arc();

        let mut storage = HostCommandsStorage::new();
        let cell = world.as_unsafe_world_cell();
        let mut cmds = HostCommands::new(&mut storage, cell.entity_allocator(), &snapshot);
        let raw_cmds = cmds.as_ffi_ptr();

        let dummy_pos = Position {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        };

        assert!(unsafe { host_get_proc_address(core::ptr::null(), core::ptr::null()) }.is_null());

        let mut out_ent = FfiEntity::NULL;
        assert_eq!(
            unsafe { host_cmd_spawn_empty(core::ptr::null_mut(), &mut out_ent) },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe { host_cmd_spawn_empty(raw_cmds, core::ptr::null_mut()) },
            FfiResult::NULL_POINTER
        );

        let mut valid_entity = FfiEntity::NULL;
        assert_eq!(
            unsafe { host_cmd_spawn_empty(raw_cmds, &mut valid_entity) },
            FfiResult::OK
        );

        assert_eq!(
            unsafe { host_cmd_despawn(core::ptr::null_mut(), valid_entity) },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe { host_cmd_despawn(raw_cmds, FfiEntity::NULL) },
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            unsafe {
                host_cmd_despawn(
                    raw_cmds,
                    FfiEntity::from_bits(bevy_ecs::entity::Entity::PLACEHOLDER.to_bits()),
                )
            },
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            unsafe { host_cmd_despawn(raw_cmds, FfiEntity::from_bits(u64::MAX)) },
            FfiResult::OK
        );

        assert_eq!(
            unsafe {
                host_cmd_insert(
                    core::ptr::null_mut(),
                    valid_entity,
                    pos_h,
                    &dummy_pos as *const Position as *const u8,
                    core::mem::size_of::<Position>(),
                )
            },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe {
                host_cmd_insert(
                    raw_cmds,
                    valid_entity,
                    BamComponent::INVALID,
                    &dummy_pos as *const Position as *const u8,
                    core::mem::size_of::<Position>(),
                )
            },
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            unsafe { host_cmd_insert(raw_cmds, valid_entity, pos_h, core::ptr::null(), 12) },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe {
                host_cmd_insert(
                    raw_cmds,
                    FfiEntity::NULL,
                    pos_h,
                    &dummy_pos as *const Position as *const u8,
                    core::mem::size_of::<Position>(),
                )
            },
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            unsafe {
                host_cmd_insert(
                    raw_cmds,
                    valid_entity,
                    pos_h,
                    &dummy_pos as *const Position as *const u8,
                    11,
                )
            },
            FfiResult::INVALID_ARGUMENT
        );

        let mut out_len = 0usize;
        let mut col_info = core::mem::MaybeUninit::<BamColumnInfo>::uninit();
        assert!(!unsafe { host_iter_next_chunk(core::ptr::null_mut(), &mut out_len) }.get());
        assert!(unsafe { host_iter_column_readonly(core::ptr::null(), 0) }.is_null());
        assert!(unsafe { host_iter_column_mutable(core::ptr::null_mut(), 0) }.is_null());
        assert_eq!(
            unsafe { host_iter_mark_changed_range(core::ptr::null_mut(), 0, 0, 1) },
            FfiResult::NULL_POINTER
        );
        assert!(unsafe { host_iter_entities(core::ptr::null()) }.is_null());
        assert_eq!(
            unsafe { host_iter_column_info(core::ptr::null(), 0, col_info.as_mut_ptr()) },
            FfiResult::NULL_POINTER
        );

        assert_eq!(
            unsafe { host_register_system(core::ptr::null_mut(), core::ptr::null()) },
            FfiResult::NULL_POINTER
        );

        let mut load_ctx = LoadContext::new(registry);
        let load_ptr = load_ctx.as_ffi_ptr();

        assert_eq!(
            unsafe { host_register_system(load_ptr, core::ptr::null()) },
            FfiResult::NULL_POINTER
        );

        let valid_sys_desc = BamSystemDesc {
            struct_size: core::mem::size_of::<BamSystemDesc>() as u32,
            flags: 0,
            name: c"valid_sys".as_ptr(),
            queries: core::ptr::null(),
            query_count: 0,
            callback: Some(multi_table_stress_system),
            user_data: core::ptr::null_mut(),
            schedule: ScheduleId::UPDATE,
            reserved0: 0,
            after: core::ptr::null(),
            after_count: 0,
            before: core::ptr::null(),
            before_count: 0,
            run_if: None,
            run_if_user_data: core::ptr::null_mut(),
            in_sets: core::ptr::null(),
            in_set_count: 0,
        };
        assert_eq!(
            unsafe { host_register_system(core::ptr::null_mut(), &valid_sys_desc) },
            FfiResult::NULL_POINTER
        );

        let mut truncated_desc = valid_sys_desc;
        truncated_desc.struct_size = 16;
        assert_eq!(
            unsafe { host_register_system(load_ptr, &truncated_desc) },
            FfiResult::INVALID_VERSION
        );

        let mut null_queries_desc = valid_sys_desc;
        null_queries_desc.query_count = 1;
        null_queries_desc.queries = core::ptr::null();
        assert_eq!(
            unsafe { host_register_system(load_ptr, &null_queries_desc) },
            FfiResult::NULL_POINTER
        );

        let bad_term = BamTermDesc::new(BamComponent::INVALID, AccessMode::READ);
        let bad_q = BamQueryDesc::new(&bad_term, 1);
        let mut bad_comp_desc = valid_sys_desc;
        bad_comp_desc.queries = &bad_q;
        bad_comp_desc.query_count = 1;
        assert_eq!(
            unsafe { host_register_system(load_ptr, &bad_comp_desc) },
            FfiResult::INVALID_COMPONENT
        );
    }

    #[test]
    fn test_max_query_components_boundary() {
        let mut world = World::new();
        let pos_id = world.register_component::<Position>();
        let mut registry = ModRegistry::new();
        let pos_h = registry
            .register_component::<Position>(pos_id, ComponentOptions::new())
            .unwrap();

        world.spawn(Position {
            x: 10.0,
            y: 20.0,
            z: 30.0,
        });

        let mut terms_34 = Vec::with_capacity(34);
        for _ in 0..34 {
            terms_34.push(BamTermDesc::new(pos_h, AccessMode::READ));
        }
        let q_over = BamQueryDesc::new(terms_34.as_ptr(), terms_34.len());
        let sys_over = BamSystemDesc {
            struct_size: core::mem::size_of::<BamSystemDesc>() as u32,
            flags: 0,
            name: c"over_max_terms".as_ptr(),
            queries: &q_over,
            query_count: 1,
            callback: Some(multi_table_stress_system),
            user_data: core::ptr::null_mut(),
            schedule: ScheduleId::UPDATE,
            reserved0: 0,
            after: core::ptr::null(),
            after_count: 0,
            before: core::ptr::null(),
            before_count: 0,
            run_if: None,
            run_if_user_data: core::ptr::null_mut(),
            in_sets: core::ptr::null(),
            in_set_count: 0,
        };
        let mut load_ctx = LoadContext::new(registry);
        assert_eq!(
            unsafe { host_register_system(load_ctx.as_ffi_ptr(), &sys_over) },
            FfiResult::CAPACITY_EXCEEDED
        );

        let cell = world.as_unsafe_world_cell();
        let storages = unsafe { cell.storages() };

        let mut max_terms = Vec::with_capacity(MAX_TERMS_PER_QUERY);
        for i in 0..MAX_TERMS_PER_QUERY {
            max_terms.push(QueryTermMeta {
                component: pos_h,
                component_id: pos_id,
                mode: if i == 0 {
                    AccessMode::WRITE
                } else {
                    AccessMode::READ
                },
                uuid: ComponentUuid::from_canonical_name("ffi::chunk_bounds::Position"),
                size: core::mem::size_of::<Position>(),
                align: core::mem::align_of::<Position>(),
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            });
        }
        let plan = QueryPlan::from_terms(max_terms);

        let matched_tables = vec![cell.archetypes().iter().last().unwrap().table_id()];
        let mut host_iter = HostIter::new_dense(
            &storages.tables,
            &matched_tables,
            &plan,
            Tick::new(0),
            cell.change_tick(),
        );
        let raw_iter = host_iter.as_ffi_ptr();

        let mut count = 0usize;
        assert!(unsafe { host_iter_next_chunk(raw_iter, &mut count) }.get());
        assert_eq!(count, 1);

        assert!(!unsafe { host_iter_column_mutable(raw_iter, 0) }.is_null());
        assert!(!unsafe { host_iter_column_readonly(raw_iter, 31) }.is_null());

        assert!(unsafe { host_iter_column_mutable(raw_iter, 32) }.is_null());
        assert!(unsafe { host_iter_column_mutable(raw_iter, 33) }.is_null());
        assert!(unsafe { host_iter_column_readonly(raw_iter, 32) }.is_null());
        assert!(unsafe { host_iter_column_readonly(raw_iter, 33) }.is_null());

        assert_eq!(
            unsafe { host_iter_mark_changed_range(raw_iter, 32, 0, 1) },
            FfiResult::OUT_OF_RANGE
        );
        assert_eq!(
            unsafe { host_iter_mark_changed_range(raw_iter, 33, 0, 1) },
            FfiResult::OUT_OF_RANGE
        );

        let mut info = core::mem::MaybeUninit::<BamColumnInfo>::uninit();
        assert_eq!(
            unsafe { host_iter_column_info(raw_iter, 31, info.as_mut_ptr()) },
            FfiResult::OK
        );
        assert_eq!(
            unsafe { host_iter_column_info(raw_iter, 32, info.as_mut_ptr()) },
            FfiResult::OUT_OF_RANGE
        );
    }

    struct ArchetypeGrowthCtx {
        spawned_once: AtomicBool,
        entity_count: AtomicUsize,
        pos_h: BamComponent,
        tag10_h: BamComponent,
    }

    unsafe extern "C" fn dynamic_archetype_growth_system(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let ctx = unsafe { &*(user_data as *const ArchetypeGrowthCtx) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }

        let mut total_entities = 0usize;
        let mut count = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            total_entities += count;
        }
        ctx.entity_count.store(total_entities, Ordering::SeqCst);

        if !ctx.spawned_once.swap(true, Ordering::SeqCst) {
            let mut commands: *mut BamCommands = core::ptr::null_mut();
            let rc_cmd = unsafe { host_sys_commands(sys_ctx, &mut commands) };
            if rc_cmd.is_err() {
                return rc_cmd;
            }
            let pos_dummy = Position {
                x: 77.0,
                y: 88.0,
                z: 99.0,
            };
            let tag_dummy = Tag10 { id: 777 };

            for _ in 0..10 {
                let mut entity = FfiEntity::NULL;
                let _ = unsafe { host_cmd_spawn_empty(commands, &mut entity) };
                let _ = unsafe {
                    host_cmd_insert(
                        commands,
                        entity,
                        ctx.pos_h,
                        &pos_dummy as *const Position as *const u8,
                        core::mem::size_of::<Position>(),
                    )
                };
                let _ = unsafe {
                    host_cmd_insert(
                        commands,
                        entity,
                        ctx.tag10_h,
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
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        app.register_mod_component::<Tag01>(ComponentOptions::new())
            .unwrap();
        let tag10_h = app
            .register_mod_component::<Tag10>(ComponentOptions::new())
            .unwrap();

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

        let ctx = Box::new(ArchetypeGrowthCtx {
            spawned_once: AtomicBool::new(false),
            entity_count: AtomicUsize::new(0),
            pos_h,
            tag10_h,
        });

        let registry = app.world().resource::<ModRegistry>().clone();
        let sys = GuestSystem::new(
            "growth_system",
            vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                pos_h,
                AccessMode::READ,
            )])],
            dynamic_archetype_growth_system,
            (&*ctx as *const ArchetypeGrowthCtx)
                .cast_mut()
                .cast::<c_void>(),
            registry,
        );
        app.add_systems(Update, sys);

        app.update();
        assert_eq!(ctx.entity_count.load(Ordering::SeqCst), 5);

        app.update();
        assert_eq!(ctx.entity_count.load(Ordering::SeqCst), 15);
    }

    unsafe extern "C" fn query_with_unregistered_callback(
        _sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let invoked = unsafe { &*(user_data as *const AtomicBool) };
        invoked.store(true, Ordering::SeqCst);
        FfiResult::OK
    }

    #[test]
    fn test_unregistered_component_in_dynamic_system() {
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

        let invoked = Box::new(AtomicBool::new(false));
        let registry = app.world().resource::<ModRegistry>().clone();
        let mut ghost_sys = GuestSystem::new(
            "ghost_query_system",
            vec![BamQuerySpecOwned::new(vec![
                BamTermDesc::new(pos_h, AccessMode::READ),
                BamTermDesc::new(BamComponent::from_raw(999), AccessMode::READ),
            ])],
            query_with_unregistered_callback,
            (&*invoked as *const AtomicBool).cast_mut().cast::<c_void>(),
            registry,
        );

        ghost_sys.initialize(app.world_mut());
        assert!(ghost_sys.init_error().is_some());
        assert!(!ghost_sys.is_enabled());

        app.add_systems(Update, ghost_sys);
        app.update();
        assert!(!invoked.load(Ordering::SeqCst));
    }
}

mod chunk_and_command_bursts {
    use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModRegistry};
    use bevy_abi_mod_host_sys::{
        BamQuerySpecOwned, GuestSystem, HostCommands, HostCommandsStorage, host_cmd_despawn,
        host_cmd_insert, host_cmd_spawn_empty, host_iter_column_info, host_iter_column_mutable,
        host_iter_column_mutable_untracked, host_iter_column_readonly, host_iter_entities,
        host_iter_mark_changed_range, host_iter_next_chunk, host_sys_commands, host_sys_query,
    };
    use bevy_abi_mod_macros::ModComponent;
    use bevy_abi_mod_sys::{
        AccessMode, BamColumnInfo, BamCommands, BamComponent, BamIter, BamSystemCtx, BamTermDesc,
        FfiEntity, FfiResult,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::prelude::{ResMut, Resource, World};
    use bevy_ecs::query::Changed;
    use bevy_ecs::schedule::IntoScheduleConfigs;
    use bevy_ecs::system::Query;
    use core::ffi::c_void;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[repr(C)]
    #[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
    #[mod_component(name = "ffi::chunk_bursts::Position")]
    pub struct Position {
        pub x: f32,
        pub y: f32,
        pub z: f32,
    }

    #[repr(C)]
    #[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
    #[mod_component(name = "ffi::chunk_bursts::Velocity")]
    pub struct Velocity {
        pub x: f32,
        pub y: f32,
        pub z: f32,
    }

    #[repr(C)]
    #[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
    #[mod_component(name = "ffi::chunk_bursts::Armor")]
    pub struct Armor {
        pub defense: i32,
    }

    macro_rules! define_stress_tags {
        ($($tag:ident => $cname:literal),* $(,)?) => {
            $(
                #[repr(C)]
                #[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
                #[mod_component(name = $cname)]
                pub struct $tag {
                    pub id: u32,
                }
            )*
        };
    }

    define_stress_tags!(
        TagA => "ffi::chunk_bursts::TagA",
        TagB => "ffi::chunk_bursts::TagB",
        TagC => "ffi::chunk_bursts::TagC",
        TagD => "ffi::chunk_bursts::TagD",
        TagE => "ffi::chunk_bursts::TagE",
    );

    struct EmptyTableStats {
        chunks_visited: usize,
        entities_visited: usize,
        readonly_null_after_exhaust: bool,
        mutable_null_after_exhaust: bool,
        entities_null_after_exhaust: bool,
        mark_err_after_exhaust: bool,
    }

    unsafe extern "C" fn empty_table_stress_system(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let stats = unsafe { &mut *(user_data as *mut EmptyTableStats) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }

        let mut count = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            stats.chunks_visited += 1;
            stats.entities_visited += count;
        }

        let ro = unsafe { host_iter_column_readonly(iter, 0) };
        let rw = unsafe { host_iter_column_mutable(iter, 0) };
        let ents = unsafe { host_iter_entities(iter) };
        let mark_res = unsafe { host_iter_mark_changed_range(iter, 0, 0, 1) };

        stats.readonly_null_after_exhaust = ro.is_null();
        stats.mutable_null_after_exhaust = rw.is_null();
        stats.entities_null_after_exhaust = ents.is_null();
        stats.mark_err_after_exhaust = mark_res == FfiResult::OUT_OF_RANGE;

        for _ in 0..5 {
            let mut rem = 99usize;
            assert!(!unsafe { host_iter_next_chunk(iter, &mut rem) }.get());
            assert_eq!(rem, 0);
        }

        FfiResult::OK
    }

    #[test]
    fn test_chunk_iteration_empty_world_and_empty_tables() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let vel_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();

        let mut stats = EmptyTableStats {
            chunks_visited: 0,
            entities_visited: 0,
            readonly_null_after_exhaust: false,
            mutable_null_after_exhaust: false,
            entities_null_after_exhaust: false,
            mark_err_after_exhaust: false,
        };

        let registry = app.world().resource::<ModRegistry>().clone();
        let sys = GuestSystem::new(
            "empty_stress",
            vec![BamQuerySpecOwned::new(vec![
                BamTermDesc::new(pos_h, AccessMode::WRITE),
                BamTermDesc::new(vel_h, AccessMode::READ),
            ])],
            empty_table_stress_system,
            (&mut stats as *mut EmptyTableStats).cast::<c_void>(),
            registry,
        );
        app.add_systems(Update, sys);
        app.update();

        assert_eq!(stats.chunks_visited, 0);
        assert_eq!(stats.entities_visited, 0);
        assert!(stats.readonly_null_after_exhaust);
        assert!(stats.mutable_null_after_exhaust);
        assert!(stats.entities_null_after_exhaust);
        assert!(stats.mark_err_after_exhaust);
    }

    #[test]
    fn test_chunk_iteration_alternating_empty_and_non_empty_tables() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        app.register_mod_component::<TagA>(ComponentOptions::new())
            .unwrap();
        app.register_mod_component::<TagB>(ComponentOptions::new())
            .unwrap();
        app.register_mod_component::<TagC>(ComponentOptions::new())
            .unwrap();
        app.register_mod_component::<TagD>(ComponentOptions::new())
            .unwrap();
        app.register_mod_component::<TagE>(ComponentOptions::new())
            .unwrap();

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

        for e in t_a_ents.into_iter().chain(t_c_ents).chain(t_e_ents) {
            app.world_mut().despawn(e);
        }

        struct ChunkAudit {
            chunk_sizes: Vec<usize>,
            total_entities: usize,
        }

        unsafe extern "C" fn audit_system(
            sys_ctx: *mut BamSystemCtx,
            user_data: *mut c_void,
        ) -> FfiResult {
            let audit = unsafe { &mut *(user_data as *mut ChunkAudit) };
            let mut iter: *mut BamIter = core::ptr::null_mut();
            let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
            if rc.is_err() {
                return rc;
            }
            let mut count = 0usize;
            while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
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
        let sys = GuestSystem::new(
            "audit_sys",
            vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                pos_h,
                AccessMode::WRITE,
            )])],
            audit_system,
            (&mut audit as *mut ChunkAudit).cast::<c_void>(),
            registry,
        );
        app.add_systems(Update, sys);
        app.update();

        assert_eq!(audit.total_entities, 30);
        assert_eq!(audit.chunk_sizes.len(), 2);
        assert!(audit.chunk_sizes.contains(&7));
        assert!(audit.chunk_sizes.contains(&23));

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
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let vel_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();

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

        struct SingleStats {
            stride_pos: usize,
            stride_vel: usize,
            count: usize,
            entity_matches: bool,
            mark_row0_ok: bool,
            mark_row1_err: bool,
        }

        struct SingleContext {
            target_entity: Entity,
            stats: SingleStats,
        }

        unsafe extern "C" fn single_entity_system(
            sys_ctx: *mut BamSystemCtx,
            user_data: *mut c_void,
        ) -> FfiResult {
            let ctx = unsafe { &mut *(user_data as *mut SingleContext) };
            let mut iter: *mut BamIter = core::ptr::null_mut();
            let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
            if rc.is_err() {
                return rc;
            }

            let mut info_pos = core::mem::MaybeUninit::<BamColumnInfo>::uninit();
            let mut info_vel = core::mem::MaybeUninit::<BamColumnInfo>::uninit();
            assert_eq!(
                unsafe { host_iter_column_info(iter, 0, info_pos.as_mut_ptr()) },
                FfiResult::OK
            );
            assert_eq!(
                unsafe { host_iter_column_info(iter, 1, info_vel.as_mut_ptr()) },
                FfiResult::OK
            );
            ctx.stats.stride_pos = unsafe { info_pos.assume_init() }.size;
            ctx.stats.stride_vel = unsafe { info_vel.assume_init() }.size;

            let mut count = 0usize;
            assert!(unsafe { host_iter_next_chunk(iter, &mut count) }.get());
            ctx.stats.count = count;
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
                ctx.stats.entity_matches = ent_bits == ctx.target_entity.to_bits();
            }

            ctx.stats.mark_row0_ok =
                unsafe { host_iter_mark_changed_range(iter, 0, 0, 1) } == FfiResult::OK;
            ctx.stats.mark_row1_err =
                unsafe { host_iter_mark_changed_range(iter, 0, 1, 1) } == FfiResult::OUT_OF_RANGE;

            let mut rem = 99usize;
            assert!(!unsafe { host_iter_next_chunk(iter, &mut rem) }.get());
            assert_eq!(rem, 0);
            FfiResult::OK
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
        let sys = GuestSystem::new(
            "single_sys",
            vec![BamQuerySpecOwned::new(vec![
                BamTermDesc::new(pos_h, AccessMode::WRITE),
                BamTermDesc::new(vel_h, AccessMode::READ),
            ])],
            single_entity_system,
            (&mut ctx as *mut SingleContext).cast::<c_void>(),
            registry,
        );
        app.add_systems(Update, sys);
        app.update();

        assert_eq!(ctx.stats.count, 1);
        assert_eq!(ctx.stats.stride_pos, core::mem::size_of::<Position>());
        assert_eq!(ctx.stats.stride_vel, core::mem::size_of::<Velocity>());
        assert!(ctx.stats.entity_matches);
        assert!(ctx.stats.mark_row0_ok);
        assert!(ctx.stats.mark_row1_err);

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
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let vel_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();
        app.register_mod_component::<Armor>(ComponentOptions::new())
            .unwrap();

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
            sys_ctx: *mut BamSystemCtx,
            user_data: *mut c_void,
        ) -> FfiResult {
            let stats = unsafe { &mut *(user_data as *mut Scale20kStats) };
            let mut iter: *mut BamIter = core::ptr::null_mut();
            let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
            if rc.is_err() {
                return rc;
            }

            let mut info_pos = core::mem::MaybeUninit::<BamColumnInfo>::uninit();
            let mut info_vel = core::mem::MaybeUninit::<BamColumnInfo>::uninit();
            assert_eq!(
                unsafe { host_iter_column_info(iter, 0, info_pos.as_mut_ptr()) },
                FfiResult::OK
            );
            assert_eq!(
                unsafe { host_iter_column_info(iter, 1, info_vel.as_mut_ptr()) },
                FfiResult::OK
            );
            assert_eq!(
                unsafe { info_pos.assume_init() }.size,
                core::mem::size_of::<Position>()
            );
            assert_eq!(
                unsafe { info_vel.assume_init() }.size,
                core::mem::size_of::<Velocity>()
            );

            let mut count = 0usize;
            while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
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
        let sys = GuestSystem::new(
            "scale_20k_sys",
            vec![BamQuerySpecOwned::new(vec![
                BamTermDesc::new(pos_h, AccessMode::WRITE),
                BamTermDesc::new(vel_h, AccessMode::READ),
            ])],
            scale_20k_system,
            (&mut stats as *mut Scale20kStats).cast::<c_void>(),
            registry,
        );
        app.add_systems(Update, sys);
        app.update();

        assert_eq!(stats.total_entities, MONOLITHIC_COUNT);
        assert_eq!(stats.chunks, 1);

        let mut query = app.world_mut().query::<(&Position, &Armor)>();
        let mut verified = 0usize;
        for (pos, armor) in query.iter(app.world()) {
            let i = pos.x - 1.0;
            assert_eq!(pos.y, 2.0);
            assert_eq!(pos.z, 3.0);
            assert_eq!(armor.defense, ((i as usize) % 100) as i32);
            verified += 1;
        }
        assert_eq!(verified, MONOLITHIC_COUNT);
    }

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
        mode: usize,
        marked_entities: Vec<Entity>,
    }

    unsafe extern "C" fn change_precision_system(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let ctx = unsafe { &mut *(user_data as *mut ChangePrecisionContext) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }

        let mut count = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            let ents = unsafe { host_iter_entities(iter) };
            assert!(!ents.is_null());

            match ctx.mode {
                0 => {
                    let _ptr = unsafe { host_iter_column_readonly(iter, 0) };
                }
                1 => {
                    let _ptr = unsafe { host_iter_column_mutable_untracked(iter, 0) };
                    for row in 0..count {
                        if row % 10 == 0 {
                            let res = unsafe { host_iter_mark_changed_range(iter, 0, row, 1) };
                            assert_eq!(res, FfiResult::OK);
                            let ent_bits = unsafe { (*ents.add(row)).to_bits() };
                            ctx.marked_entities.push(Entity::from_bits(ent_bits));
                        }
                    }
                }
                2 => {
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
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();

        for i in 0..TOTAL_ENTITIES {
            app.world_mut().spawn(Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            });
        }

        let mut ctx = Box::new(ChangePrecisionContext {
            mode: 0,
            marked_entities: Vec::new(),
        });
        let ctx_ptr = (&mut *ctx as *mut ChangePrecisionContext).cast::<c_void>();

        let registry = app.world().resource::<ModRegistry>().clone();
        let sys = GuestSystem::new(
            "change_precision_sys",
            vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                pos_h,
                AccessMode::WRITE,
            )])],
            change_precision_system,
            ctx_ptr,
            registry,
        );
        app.add_systems(Update, (sys, inspect_changed_positions).chain());

        app.update();
        assert_eq!(
            app.world()
                .resource::<ChangedPositionInspector>()
                .changed_count,
            TOTAL_ENTITIES
        );

        ctx.mode = 0;
        app.update();
        assert_eq!(
            app.world()
                .resource::<ChangedPositionInspector>()
                .changed_count,
            0
        );

        ctx.mode = 1;
        ctx.marked_entities.clear();
        app.update();

        let inspector = app.world().resource::<ChangedPositionInspector>();
        assert_eq!(inspector.changed_count, 100);
        assert_eq!(ctx.marked_entities.len(), 100);
        for expected_ent in &ctx.marked_entities {
            assert!(inspector.changed_entities.contains(expected_ent));
        }

        ctx.mode = 0;
        ctx.marked_entities.clear();
        app.update();
        assert_eq!(
            app.world()
                .resource::<ChangedPositionInspector>()
                .changed_count,
            0
        );

        ctx.mode = 2;
        app.update();
        assert_eq!(
            app.world()
                .resource::<ChangedPositionInspector>()
                .changed_count,
            TOTAL_ENTITIES
        );

        ctx.mode = 0;
        app.update();
        assert_eq!(
            app.world()
                .resource::<ChangedPositionInspector>()
                .changed_count,
            0
        );
    }

    #[test]
    fn test_change_detection_multi_archetype_isolation() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.init_resource::<ChangedPositionInspector>();
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let vel_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();
        app.register_mod_component::<Armor>(ComponentOptions::new())
            .unwrap();
        app.register_mod_component::<TagA>(ComponentOptions::new())
            .unwrap();

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

        unsafe extern "C" fn kinematics_only_system(
            sys_ctx: *mut BamSystemCtx,
            user_data: *mut c_void,
        ) -> FfiResult {
            let mutate_active = unsafe { &*(user_data as *const AtomicUsize) };
            if mutate_active.load(Ordering::SeqCst) == 0 {
                return FfiResult::OK;
            }
            let mut iter: *mut BamIter = core::ptr::null_mut();
            let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
            if rc.is_err() {
                return rc;
            }
            let mut count = 0usize;
            while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
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

        let mutate_active = Box::new(AtomicUsize::new(0));
        let registry = app.world().resource::<ModRegistry>().clone();
        let sys = GuestSystem::new(
            "kinematics_only",
            vec![BamQuerySpecOwned::new(vec![
                BamTermDesc::new(pos_h, AccessMode::WRITE),
                BamTermDesc::new(vel_h, AccessMode::READ),
            ])],
            kinematics_only_system,
            (&*mutate_active as *const AtomicUsize)
                .cast_mut()
                .cast::<c_void>(),
            registry,
        );
        app.add_systems(Update, (sys, inspect_changed_positions).chain());

        mutate_active.store(0, Ordering::SeqCst);
        app.update();
        assert_eq!(
            app.world()
                .resource::<ChangedPositionInspector>()
                .changed_count,
            900
        );

        app.update();
        assert_eq!(
            app.world()
                .resource::<ChangedPositionInspector>()
                .changed_count,
            0
        );

        mutate_active.store(1, Ordering::SeqCst);
        app.update();

        let inspector = app.world().resource::<ChangedPositionInspector>();
        assert_eq!(inspector.changed_count, 300);
        for changed_e in &inspector.changed_entities {
            assert!(arch1_ents.contains(changed_e));
            assert!(!arch2_ents.contains(changed_e));
            assert!(!arch3_ents.contains(changed_e));
        }
    }

    #[test]
    fn test_command_queue_bridge_burst_10k_interleaved_ops() {
        let mut world = World::new();
        let pos_id = world.register_component::<Position>();
        let vel_id = world.register_component::<Velocity>();
        let armor_id = world.register_component::<Armor>();

        let mut registry = ModRegistry::new();
        let pos_h = registry
            .register_component::<Position>(pos_id, ComponentOptions::new())
            .unwrap();
        let vel_h = registry
            .register_component::<Velocity>(vel_id, ComponentOptions::new())
            .unwrap();
        let armor_h = registry
            .register_component::<Armor>(armor_id, ComponentOptions::new())
            .unwrap();
        let snapshot = registry.snapshot_arc();

        const BURST_CYCLES: usize = 2_500;
        let mut surviving_entities = Vec::with_capacity(BURST_CYCLES);
        let mut dead_entities = Vec::with_capacity(BURST_CYCLES);
        let mut storage = HostCommandsStorage::new();

        {
            let cell = world.as_unsafe_world_cell();
            let mut cmds = HostCommands::new(&mut storage, cell.entity_allocator(), &snapshot);
            let raw_cmds = cmds.as_ffi_ptr();

            for i in 0..BURST_CYCLES {
                let mut e_surv = FfiEntity::NULL;
                assert_eq!(
                    unsafe { host_cmd_spawn_empty(raw_cmds, &mut e_surv) },
                    FfiResult::OK
                );
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
                assert_eq!(
                    unsafe {
                        host_cmd_insert(
                            raw_cmds,
                            e_surv,
                            pos_h,
                            &pos as *const Position as *const u8,
                            core::mem::size_of::<Position>(),
                        )
                    },
                    FfiResult::OK
                );
                assert_eq!(
                    unsafe {
                        host_cmd_insert(
                            raw_cmds,
                            e_surv,
                            vel_h,
                            &vel as *const Velocity as *const u8,
                            core::mem::size_of::<Velocity>(),
                        )
                    },
                    FfiResult::OK
                );
                surviving_entities.push(e_surv);

                let mut e_die = FfiEntity::NULL;
                assert_eq!(
                    unsafe { host_cmd_spawn_empty(raw_cmds, &mut e_die) },
                    FfiResult::OK
                );
                let armor = Armor { defense: 999 };
                assert_eq!(
                    unsafe {
                        host_cmd_insert(
                            raw_cmds,
                            e_die,
                            armor_h,
                            &armor as *const Armor as *const u8,
                            core::mem::size_of::<Armor>(),
                        )
                    },
                    FfiResult::OK
                );
                assert_eq!(unsafe { host_cmd_despawn(raw_cmds, e_die) }, FfiResult::OK);
                dead_entities.push(e_die);
            }
        }

        storage.apply_to_world(&mut world, &snapshot);

        assert_eq!(
            world.query::<&Position>().iter(&world).count(),
            BURST_CYCLES
        );
        assert_eq!(
            world.query::<&Velocity>().iter(&world).count(),
            BURST_CYCLES
        );
        assert_eq!(world.query::<&Armor>().iter(&world).count(), 0);

        for (i, &ffi_e) in surviving_entities.iter().enumerate() {
            let bevy_e = Entity::from_bits(ffi_e.to_bits());
            let pos = world.get::<Position>(bevy_e).unwrap();
            let vel = world.get::<Velocity>(bevy_e).unwrap();
            assert_eq!(pos.x, i as f32);
            assert_eq!(pos.y, (i * 2) as f32);
            assert_eq!(pos.z, (i * 3) as f32);
            assert_eq!(vel.x, 10.0);
            assert_eq!(vel.y, 20.0);
            assert_eq!(vel.z, 30.0);
        }

        for &ffi_e in &dead_entities {
            let bevy_e = Entity::from_bits(ffi_e.to_bits());
            assert!(world.get_entity(bevy_e).is_err());
        }
    }

    #[test]
    fn test_command_queue_bridge_multi_system_schedule_concurrency() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let vel_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();

        unsafe extern "C" fn spawner_pos_system(
            sys_ctx: *mut BamSystemCtx,
            user_data: *mut c_void,
        ) -> FfiResult {
            let pos_h = BamComponent(user_data as usize as u32);
            let mut commands: *mut BamCommands = core::ptr::null_mut();
            let rc = unsafe { host_sys_commands(sys_ctx, &mut commands) };
            if rc.is_err() {
                return rc;
            }
            for i in 0..500 {
                let mut ent = FfiEntity::NULL;
                let _ = unsafe { host_cmd_spawn_empty(commands, &mut ent) };
                let pos = Position {
                    x: i as f32,
                    y: 1.0,
                    z: 2.0,
                };
                let _ = unsafe {
                    host_cmd_insert(
                        commands,
                        ent,
                        pos_h,
                        &pos as *const Position as *const u8,
                        core::mem::size_of::<Position>(),
                    )
                };
            }
            FfiResult::OK
        }

        unsafe extern "C" fn spawner_vel_system(
            sys_ctx: *mut BamSystemCtx,
            user_data: *mut c_void,
        ) -> FfiResult {
            let vel_h = BamComponent(user_data as usize as u32);
            let mut commands: *mut BamCommands = core::ptr::null_mut();
            let rc = unsafe { host_sys_commands(sys_ctx, &mut commands) };
            if rc.is_err() {
                return rc;
            }
            for _ in 0..500 {
                let mut ent = FfiEntity::NULL;
                let _ = unsafe { host_cmd_spawn_empty(commands, &mut ent) };
                let vel = Velocity {
                    x: 5.0,
                    y: 5.0,
                    z: 5.0,
                };
                let _ = unsafe {
                    host_cmd_insert(
                        commands,
                        ent,
                        vel_h,
                        &vel as *const Velocity as *const u8,
                        core::mem::size_of::<Velocity>(),
                    )
                };
            }
            FfiResult::OK
        }

        let registry = app.world().resource::<ModRegistry>().clone();
        let sys_a = GuestSystem::new(
            "sys_a",
            Vec::new(),
            spawner_pos_system,
            (pos_h.0 as usize) as *mut c_void,
            registry.clone(),
        );
        let sys_b = GuestSystem::new(
            "sys_b",
            Vec::new(),
            spawner_vel_system,
            (vel_h.0 as usize) as *mut c_void,
            registry,
        );

        app.add_systems(Update, (sys_a, sys_b));

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
        let pos_h = registry
            .register_component::<Position>(pos_id, ComponentOptions::new())
            .unwrap();
        let snapshot = registry.snapshot_arc();

        let mut storage = HostCommandsStorage::new();
        let cell = world.as_unsafe_world_cell();
        let mut cmds = HostCommands::new(&mut storage, cell.entity_allocator(), &snapshot);
        let raw_cmds = cmds.as_ffi_ptr();

        let mut ent = FfiEntity::NULL;
        assert_eq!(
            unsafe { host_cmd_spawn_empty(raw_cmds, &mut ent) },
            FfiResult::OK
        );
        let pos = Position {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        };

        assert_eq!(
            unsafe {
                host_cmd_insert(
                    core::ptr::null_mut(),
                    ent,
                    pos_h,
                    &pos as *const Position as *const u8,
                    core::mem::size_of::<Position>(),
                )
            },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe { host_cmd_spawn_empty(core::ptr::null_mut(), &mut ent) },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe { host_cmd_despawn(core::ptr::null_mut(), ent) },
            FfiResult::NULL_POINTER
        );

        assert_eq!(
            unsafe {
                host_cmd_insert(
                    raw_cmds,
                    ent,
                    BamComponent::INVALID,
                    &pos as *const Position as *const u8,
                    core::mem::size_of::<Position>(),
                )
            },
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            unsafe {
                host_cmd_insert(
                    raw_cmds,
                    ent,
                    pos_h,
                    core::ptr::null(),
                    core::mem::size_of::<Position>(),
                )
            },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe {
                host_cmd_insert(
                    raw_cmds,
                    ent,
                    BamComponent::from_raw(999),
                    &pos as *const Position as *const u8,
                    core::mem::size_of::<Position>(),
                )
            },
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            unsafe {
                host_cmd_insert(
                    raw_cmds,
                    ent,
                    pos_h,
                    &pos as *const Position as *const u8,
                    core::mem::size_of::<Position>() - 1,
                )
            },
            FfiResult::INVALID_ARGUMENT
        );

        assert_eq!(
            unsafe {
                host_cmd_insert(
                    raw_cmds,
                    FfiEntity::NULL,
                    pos_h,
                    &pos as *const Position as *const u8,
                    core::mem::size_of::<Position>(),
                )
            },
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            unsafe { host_cmd_despawn(raw_cmds, FfiEntity::NULL) },
            FfiResult::INVALID_ENTITY
        );

        let non_existent_ent =
            FfiEntity::from_bits(Entity::from_raw_u32(999_999).unwrap().to_bits());
        assert_eq!(
            unsafe {
                host_cmd_insert(
                    raw_cmds,
                    non_existent_ent,
                    pos_h,
                    &pos as *const Position as *const u8,
                    core::mem::size_of::<Position>(),
                )
            },
            FfiResult::OK
        );
        assert_eq!(
            unsafe { host_cmd_despawn(raw_cmds, non_existent_ent) },
            FfiResult::OK
        );

        drop(cmds);
        storage.apply_to_world(&mut world, &snapshot);
        assert!(
            world
                .get_entity(Entity::from_bits(non_existent_ent.to_bits()))
                .is_err()
        );
    }
}

mod high_volume_chunks_and_commands {
    use crate::support::*;
    use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModRegistry};
    use bevy_abi_mod_host_sys::{
        BamQuerySpecOwned, GuestSystem, HostCommands, HostCommandsStorage, host_cmd_despawn,
        host_cmd_insert, host_cmd_spawn_empty, host_iter_column_mutable,
        host_iter_column_mutable_untracked, host_iter_column_readonly, host_iter_entities,
        host_iter_mark_changed_range, host_iter_next_chunk, host_iter_reset, host_sys_commands,
        host_sys_query,
    };
    use bevy_abi_mod_sys::{
        AccessMode, BamCommands, BamComponent, BamIter, BamSystemCtx, BamTermDesc, FfiEntity,
        FfiResult,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::entity::Entity;
    use bevy_ecs::prelude::{ResMut, Resource};
    use bevy_ecs::query::Changed;
    use bevy_ecs::schedule::IntoScheduleConfigs;
    use bevy_ecs::system::Query;
    use core::ffi::c_void;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

    unsafe extern "C" fn zero_entity_callback(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let visits = unsafe { &*(user_data as *const AtomicUsize) };
        visits.fetch_add(1, Ordering::SeqCst);

        let mut iter: *mut BamIter = core::ptr::null_mut();
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }

        let mut count = 99usize;
        assert!(!unsafe { host_iter_next_chunk(iter, &mut count) }.get());
        assert_eq!(count, 0);
        assert!(unsafe { host_iter_entities(iter) }.is_null());
        assert!(unsafe { host_iter_column_readonly(iter, 0) }.is_null());
        assert!(unsafe { host_iter_column_mutable(iter, 1) }.is_null());
        FfiResult::OK
    }

    struct SingleVisitState {
        visits: AtomicUsize,
        low_bits: AtomicU32,
    }

    unsafe extern "C" fn single_entity_callback(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let state = unsafe { &*(user_data as *const SingleVisitState) };
        state.visits.fetch_add(1, Ordering::SeqCst);

        let mut iter: *mut BamIter = core::ptr::null_mut();
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }

        let mut count = 0usize;
        assert!(unsafe { host_iter_next_chunk(iter, &mut count) }.get());
        assert_eq!(count, 1);

        let entities_ptr = unsafe { host_iter_entities(iter) };
        assert!(!entities_ptr.is_null());
        let entity = unsafe { *entities_ptr };
        state
            .low_bits
            .store(entity.to_bits() as u32, Ordering::SeqCst);

        let pos_ptr = unsafe { host_iter_column_readonly(iter, 0) } as *const Position;
        assert!(!pos_ptr.is_null());
        assert_eq!(
            unsafe { *pos_ptr },
            Position {
                x: 42.0,
                y: 100.0,
                z: -5.0,
            }
        );

        let vel_ptr = unsafe { host_iter_column_mutable(iter, 1) } as *mut Velocity;
        assert!(!vel_ptr.is_null());
        unsafe {
            (*vel_ptr).x += 10.0;
            (*vel_ptr).y += 20.0;
            (*vel_ptr).z += 30.0;
        }

        let mut next_count = 99usize;
        assert!(!unsafe { host_iter_next_chunk(iter, &mut next_count) }.get());
        assert_eq!(next_count, 0);
        FfiResult::OK
    }

    #[test]
    fn test_stress_chunk_iteration_zero_and_single_entity() {
        {
            let zero_visits = Box::new(AtomicUsize::new(0));
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            let pos_h = app
                .register_mod_component::<Position>(ComponentOptions::new())
                .unwrap();
            let vel_h = app
                .register_mod_component::<Velocity>(ComponentOptions::new())
                .unwrap();

            let registry = app.world().resource::<ModRegistry>().clone();
            let system = GuestSystem::new(
                "zero_entity_system",
                vec![BamQuerySpecOwned::new(vec![
                    BamTermDesc::new(pos_h, AccessMode::READ),
                    BamTermDesc::new(vel_h, AccessMode::WRITE),
                ])],
                zero_entity_callback,
                (&*zero_visits as *const AtomicUsize)
                    .cast_mut()
                    .cast::<c_void>(),
                registry,
            );
            app.add_systems(Update, system);

            for _ in 0..5 {
                app.update();
            }
            assert_eq!(zero_visits.load(Ordering::SeqCst), 5);
        }

        {
            let state = Box::new(SingleVisitState {
                visits: AtomicUsize::new(0),
                low_bits: AtomicU32::new(0),
            });
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            let pos_h = app
                .register_mod_component::<Position>(ComponentOptions::new())
                .unwrap();
            let vel_h = app
                .register_mod_component::<Velocity>(ComponentOptions::new())
                .unwrap();

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
            let system = GuestSystem::new(
                "single_entity_system",
                vec![BamQuerySpecOwned::new(vec![
                    BamTermDesc::new(pos_h, AccessMode::READ),
                    BamTermDesc::new(vel_h, AccessMode::WRITE),
                ])],
                single_entity_callback,
                (&*state as *const SingleVisitState)
                    .cast_mut()
                    .cast::<c_void>(),
                registry,
            );
            app.add_systems(Update, system);

            app.update();
            assert_eq!(state.visits.load(Ordering::SeqCst), 1);
            assert_eq!(
                state.low_bits.load(Ordering::SeqCst),
                (e.to_bits() & 0xFFFF_FFFF) as u32
            );

            let vel = *app.world().get::<Velocity>(e).unwrap();
            assert_eq!(
                vel,
                Velocity {
                    x: 11.0,
                    y: 22.0,
                    z: 33.0,
                }
            );
        }
    }

    struct MultiChunk25kStats {
        total_entities: AtomicUsize,
        total_chunks: AtomicUsize,
    }

    unsafe extern "C" fn multi_chunk_scale_callback(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let stats = unsafe { &*(user_data as *const MultiChunk25kStats) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }

        let mut total_entities = 0usize;
        let mut chunks = 0usize;
        let mut count = 0usize;

        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
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

            for i in 0..count {
                let p = unsafe { *pos_ptr.add(i) };
                let v = unsafe { &mut *vel_ptr.add(i) };
                v.x += 1.0;
                v.y += p.x * 0.001;
            }
        }

        stats.total_entities.store(total_entities, Ordering::SeqCst);
        stats.total_chunks.store(chunks, Ordering::SeqCst);

        assert_eq!(unsafe { host_iter_reset(iter) }, FfiResult::OK);
        let mut re_count = 0usize;
        let mut re_chunks = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            re_chunks += 1;
            re_count += count;
        }
        assert_eq!(re_count, total_entities);
        assert_eq!(re_chunks, chunks);
        FfiResult::OK
    }

    #[test]
    fn test_stress_chunk_iteration_multi_chunk_archetypes_25k() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let vel_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();
        app.register_mod_component::<CombatHealth>(ComponentOptions::new())
            .unwrap();
        app.register_mod_component::<Mass>(ComponentOptions::new())
            .unwrap();
        app.register_mod_component::<Armor>(ComponentOptions::new())
            .unwrap();

        const PER_ARCH: usize = 5_000;
        let mut spawned_entities = Vec::with_capacity(PER_ARCH * 5);

        for i in 0..PER_ARCH {
            spawned_entities.push(
                app.world_mut()
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
                    .id(),
            );
        }
        for i in 0..PER_ARCH {
            spawned_entities.push(
                app.world_mut()
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
                    .id(),
            );
        }
        for i in 0..PER_ARCH {
            spawned_entities.push(
                app.world_mut()
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
                    .id(),
            );
        }
        for i in 0..PER_ARCH {
            spawned_entities.push(
                app.world_mut()
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
                    .id(),
            );
        }
        for i in 0..PER_ARCH {
            spawned_entities.push(
                app.world_mut()
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
                    .id(),
            );
        }

        assert_eq!(spawned_entities.len(), 25_000);

        let stats = Box::new(MultiChunk25kStats {
            total_entities: AtomicUsize::new(0),
            total_chunks: AtomicUsize::new(0),
        });
        let registry = app.world().resource::<ModRegistry>().clone();
        let system = GuestSystem::new(
            "multi_chunk_25k_system",
            vec![BamQuerySpecOwned::new(vec![
                BamTermDesc::new(pos_h, AccessMode::READ),
                BamTermDesc::new(vel_h, AccessMode::WRITE),
            ])],
            multi_chunk_scale_callback,
            (&*stats as *const MultiChunk25kStats)
                .cast_mut()
                .cast::<c_void>(),
            registry,
        );
        app.add_systems(Update, system);
        app.update();

        assert_eq!(stats.total_entities.load(Ordering::SeqCst), 25_000);
        assert_eq!(stats.total_chunks.load(Ordering::SeqCst), 5);

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

    unsafe extern "C" fn access_permissions_callback(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let verified = unsafe { &*(user_data as *const AtomicBool) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }

        let mut count = 0usize;
        if !unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            return FfiResult::OK;
        }
        assert_eq!(count, 1);

        assert!(!unsafe { host_iter_column_readonly(iter, 0) }.is_null());
        assert!(unsafe { host_iter_column_mutable(iter, 0) }.is_null());
        assert_eq!(
            unsafe { host_iter_mark_changed_range(iter, 0, 0, 1) },
            FfiResult::PERMISSION_DENIED
        );

        assert!(!unsafe { host_iter_column_readonly(iter, 1) }.is_null());
        assert!(!unsafe { host_iter_column_mutable(iter, 1) }.is_null());
        assert_eq!(
            unsafe { host_iter_mark_changed_range(iter, 1, 0, 1) },
            FfiResult::OK
        );

        assert!(unsafe { host_iter_column_readonly(iter, 2) }.is_null());
        assert!(unsafe { host_iter_column_mutable(iter, 2) }.is_null());

        assert!(unsafe { host_iter_column_readonly(iter, 3) }.is_null());
        assert!(unsafe { host_iter_column_mutable(iter, 3) }.is_null());

        assert!(unsafe { host_iter_column_readonly(iter, 4) }.is_null());
        assert!(unsafe { host_iter_column_mutable(iter, 32) }.is_null());
        assert_eq!(
            unsafe { host_iter_mark_changed_range(iter, 100, 0, 1) },
            FfiResult::OUT_OF_RANGE
        );
        assert_eq!(
            unsafe { host_iter_mark_changed_range(iter, 1, 999_999, 1) },
            FfiResult::OUT_OF_RANGE
        );

        verified.store(true, Ordering::SeqCst);
        FfiResult::OK
    }

    #[test]
    fn test_stress_component_access_permissions() {
        let verified = Box::new(AtomicBool::new(false));

        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let vel_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();
        let health_h = app
            .register_mod_component::<CombatHealth>(ComponentOptions::new())
            .unwrap();
        let mass_h = app
            .register_mod_component::<Mass>(ComponentOptions::new())
            .unwrap();

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
        let system = GuestSystem::new(
            "access_permissions_system",
            vec![BamQuerySpecOwned::new(vec![
                BamTermDesc::new(pos_h, AccessMode::READ),
                BamTermDesc::new(vel_h, AccessMode::WRITE),
                BamTermDesc::new(health_h, AccessMode::FILTER_ONLY),
                BamTermDesc::new(mass_h, AccessMode::WITHOUT),
            ])],
            access_permissions_callback,
            (&*verified as *const AtomicBool)
                .cast_mut()
                .cast::<c_void>(),
            registry,
        );
        app.add_systems(Update, system);

        app.update();
        assert!(verified.load(Ordering::SeqCst));
    }

    #[derive(Resource, Default)]
    struct ChangedVelCount(usize);

    fn count_changed_velocities(
        query: Query<&Velocity, Changed<Velocity>>,
        mut out: ResMut<ChangedVelCount>,
    ) {
        out.0 = query.iter().count();
    }

    const TARGET_ROW: usize = 7;

    unsafe extern "C" fn blanket_mutation_callback(
        sys_ctx: *mut BamSystemCtx,
        _user_data: *mut c_void,
    ) -> FfiResult {
        let mut iter: *mut BamIter = core::ptr::null_mut();
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }
        let mut count = 0usize;
        if unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            let vel_ptr = unsafe { host_iter_column_mutable(iter, 0) } as *mut Velocity;
            assert!(!vel_ptr.is_null());
            for i in 0..count {
                unsafe {
                    (*vel_ptr.add(i)).x += 1.0;
                }
            }
        }
        FfiResult::OK
    }

    unsafe extern "C" fn selective_mutation_callback(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let mode = unsafe { &*(user_data as *const AtomicUsize) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }
        let mut count = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            if mode.load(Ordering::SeqCst) == 1 && count > TARGET_ROW {
                let vel_ptr =
                    unsafe { host_iter_column_mutable_untracked(iter, 0) } as *mut Velocity;
                assert!(!vel_ptr.is_null());
                unsafe {
                    (*vel_ptr.add(TARGET_ROW)).x = 999.0;
                }
                assert_eq!(
                    unsafe { host_iter_mark_changed_range(iter, 0, TARGET_ROW, 1) },
                    FfiResult::OK
                );
            } else {
                let _ = unsafe { host_iter_column_readonly(iter, 0) };
            }
        }
        FfiResult::OK
    }

    #[test]
    fn test_stress_change_detection_granularity() {
        {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            app.init_resource::<ChangedVelCount>();
            let vel_h = app
                .register_mod_component::<Velocity>(ComponentOptions::new())
                .unwrap();

            for i in 0..50 {
                app.world_mut().spawn(Velocity {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                });
            }

            let registry = app.world().resource::<ModRegistry>().clone();
            let system = GuestSystem::new(
                "blanket_system",
                vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                    vel_h,
                    AccessMode::WRITE,
                )])],
                blanket_mutation_callback,
                core::ptr::null_mut(),
                registry,
            );
            app.add_systems(Update, (system, count_changed_velocities).chain());

            app.update();
            app.update();
            assert_eq!(app.world().resource::<ChangedVelCount>().0, 50);
        }

        {
            let mode = Box::new(AtomicUsize::new(0));
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            app.init_resource::<ChangedVelCount>();
            let vel_h = app
                .register_mod_component::<Velocity>(ComponentOptions::new())
                .unwrap();

            let mut entities = Vec::new();
            for i in 0..50 {
                entities.push(
                    app.world_mut()
                        .spawn(Velocity {
                            x: i as f32,
                            y: 0.0,
                            z: 0.0,
                        })
                        .id(),
                );
            }

            let registry = app.world().resource::<ModRegistry>().clone();
            let system = GuestSystem::new(
                "selective_system",
                vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                    vel_h,
                    AccessMode::WRITE,
                )])],
                selective_mutation_callback,
                (&*mode as *const AtomicUsize).cast_mut().cast::<c_void>(),
                registry,
            );
            app.add_systems(Update, (system, count_changed_velocities).chain());

            app.update();
            assert_eq!(app.world().resource::<ChangedVelCount>().0, 50);

            app.update();
            assert_eq!(app.world().resource::<ChangedVelCount>().0, 0);

            mode.store(1, Ordering::SeqCst);
            app.update();
            assert_eq!(app.world().resource::<ChangedVelCount>().0, 1);

            let target_vel = *app.world().get::<Velocity>(entities[TARGET_ROW]).unwrap();
            assert_eq!(target_vel.x, 999.0);
        }
    }

    const BATCH_SIZE: usize = 2_000;

    struct MassCmdContext {
        pos_h: BamComponent,
        health_h: BamComponent,
        vel_h: BamComponent,
        spawned_ids: Mutex<Vec<FfiEntity>>,
    }

    unsafe extern "C" fn mass_spawn_callback(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let ctx = unsafe { &*(user_data as *const MassCmdContext) };
        let mut commands: *mut BamCommands = core::ptr::null_mut();
        let rc = unsafe { host_sys_commands(sys_ctx, &mut commands) };
        if rc.is_err() {
            return rc;
        }
        let mut ids = Vec::with_capacity(BATCH_SIZE);
        for i in 0..BATCH_SIZE {
            let mut ffi_ent = FfiEntity::NULL;
            assert_eq!(
                unsafe { host_cmd_spawn_empty(commands, &mut ffi_ent) },
                FfiResult::OK
            );
            let pos = Position {
                x: i as f32,
                y: (i * 2) as f32,
                z: 0.0,
            };
            assert_eq!(
                unsafe {
                    host_cmd_insert(
                        commands,
                        ffi_ent,
                        ctx.pos_h,
                        &pos as *const Position as *const u8,
                        core::mem::size_of::<Position>(),
                    )
                },
                FfiResult::OK
            );
            let health = CombatHealth {
                current: 100,
                max: 100,
            };
            assert_eq!(
                unsafe {
                    host_cmd_insert(
                        commands,
                        ffi_ent,
                        ctx.health_h,
                        &health as *const CombatHealth as *const u8,
                        core::mem::size_of::<CombatHealth>(),
                    )
                },
                FfiResult::OK
            );
            ids.push(ffi_ent);
        }
        *ctx.spawned_ids.lock().unwrap() = ids;
        FfiResult::OK
    }

    unsafe extern "C" fn mass_despawn_and_insert_callback(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let ctx = unsafe { &*(user_data as *const MassCmdContext) };
        let mut commands: *mut BamCommands = core::ptr::null_mut();
        let rc = unsafe { host_sys_commands(sys_ctx, &mut commands) };
        if rc.is_err() {
            return rc;
        }
        let ids = ctx.spawned_ids.lock().unwrap().clone();
        for (idx, &ffi_ent) in ids.iter().enumerate() {
            if idx % 2 == 0 {
                assert_eq!(
                    unsafe { host_cmd_despawn(commands, ffi_ent) },
                    FfiResult::OK
                );
            } else {
                let vel = Velocity {
                    x: idx as f32,
                    y: -1.0,
                    z: 1.0,
                };
                assert_eq!(
                    unsafe {
                        host_cmd_insert(
                            commands,
                            ffi_ent,
                            ctx.vel_h,
                            &vel as *const Velocity as *const u8,
                            core::mem::size_of::<Velocity>(),
                        )
                    },
                    FfiResult::OK
                );
            }
        }
        FfiResult::OK
    }

    #[test]
    fn test_stress_deferred_commands_high_volume() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let health_h = app
            .register_mod_component::<CombatHealth>(ComponentOptions::new())
            .unwrap();
        let vel_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();

        let ctx = Box::new(MassCmdContext {
            pos_h,
            health_h,
            vel_h,
            spawned_ids: Mutex::new(Vec::new()),
        });

        let registry = app.world().resource::<ModRegistry>().clone();
        let spawn_system = GuestSystem::new(
            "mass_spawn",
            Vec::new(),
            mass_spawn_callback,
            (&*ctx as *const MassCmdContext).cast_mut().cast::<c_void>(),
            registry,
        );
        app.add_systems(Update, spawn_system);
        app.update();

        let spawned_ids = ctx.spawned_ids.lock().unwrap().clone();
        assert_eq!(spawned_ids.len(), BATCH_SIZE);

        for (i, &ffi_ent) in spawned_ids.iter().enumerate() {
            let bevy_ent = Entity::try_from_bits(ffi_ent.to_bits()).unwrap();
            let pos = *app.world().get::<Position>(bevy_ent).unwrap();
            let health = *app.world().get::<CombatHealth>(bevy_ent).unwrap();
            assert_eq!(pos.x, i as f32);
            assert_eq!(health.current, 100);
        }

        let mut app2 = App::new();
        app2.add_plugins(AbiModPlugin);
        let pos_h2 = app2
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let health_h2 = app2
            .register_mod_component::<CombatHealth>(ComponentOptions::new())
            .unwrap();
        let vel_h2 = app2
            .register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();

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

        let ctx2 = Box::new(MassCmdContext {
            pos_h: pos_h2,
            health_h: health_h2,
            vel_h: vel_h2,
            spawned_ids: Mutex::new(spawned_ids.clone()),
        });
        let registry2 = app2.world().resource::<ModRegistry>().clone();
        let despawn_system = GuestSystem::new(
            "mass_despawn",
            Vec::new(),
            mass_despawn_and_insert_callback,
            (&*ctx2 as *const MassCmdContext)
                .cast_mut()
                .cast::<c_void>(),
            registry2,
        );
        app2.add_systems(Update, despawn_system);
        app2.update();

        for (idx, &ffi_ent) in spawned_ids.iter().enumerate() {
            let bevy_ent = Entity::try_from_bits(ffi_ent.to_bits()).unwrap();
            if idx % 2 == 0 {
                assert!(app2.world().get_entity(bevy_ent).is_err());
            } else {
                assert!(app2.world().get::<Position>(bevy_ent).is_some());
                assert!(app2.world().get::<CombatHealth>(bevy_ent).is_some());
                let vel = *app2.world().get::<Velocity>(bevy_ent).unwrap();
                assert_eq!(vel.x, idx as f32);
            }
        }
    }

    #[test]
    fn test_stress_deferred_commands_adversarial_inputs() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();

        let registry = app.world().resource::<ModRegistry>().clone();
        let snapshot = registry.snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        let cell = app.world_mut().as_unsafe_world_cell();
        let mut cmds = HostCommands::new(&mut storage, cell.entity_allocator(), &snapshot);
        let raw_cmds = cmds.as_ffi_ptr();

        let mut valid_entity = FfiEntity::NULL;
        assert_eq!(
            unsafe { host_cmd_spawn_empty(raw_cmds, &mut valid_entity) },
            FfiResult::OK
        );
        assert!(!valid_entity.is_null());

        let dummy_pos = Position {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        };

        assert_eq!(
            unsafe { host_cmd_spawn_empty(core::ptr::null_mut(), &mut valid_entity) },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe { host_cmd_despawn(core::ptr::null_mut(), valid_entity) },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe {
                host_cmd_insert(
                    core::ptr::null_mut(),
                    valid_entity,
                    pos_h,
                    &dummy_pos as *const Position as *const u8,
                    core::mem::size_of::<Position>(),
                )
            },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe {
                host_cmd_insert(
                    raw_cmds,
                    valid_entity,
                    BamComponent::INVALID,
                    &dummy_pos as *const Position as *const u8,
                    core::mem::size_of::<Position>(),
                )
            },
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            unsafe { host_cmd_insert(raw_cmds, valid_entity, pos_h, core::ptr::null(), 12) },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe {
                host_cmd_insert(
                    raw_cmds,
                    valid_entity,
                    BamComponent::from_raw(999),
                    &dummy_pos as *const Position as *const u8,
                    12,
                )
            },
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            unsafe {
                host_cmd_insert(
                    raw_cmds,
                    valid_entity,
                    pos_h,
                    &dummy_pos as *const Position as *const u8,
                    999,
                )
            },
            FfiResult::INVALID_ARGUMENT
        );

        assert_eq!(
            unsafe { host_cmd_despawn(raw_cmds, FfiEntity::NULL) },
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            unsafe {
                host_cmd_despawn(
                    raw_cmds,
                    FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits()),
                )
            },
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            unsafe { host_cmd_despawn(raw_cmds, FfiEntity::from_bits(u64::MAX)) },
            FfiResult::OK
        );
    }
}

mod command_overwrites_and_stack_scramble {
    use bevy_abi_mod_host::ModRegistry;
    use bevy_abi_mod_host_sys::{
        HostCommands, HostCommandsStorage, LoadContext, host_cmd_despawn, host_cmd_insert,
        host_cmd_spawn_empty, host_register_component,
    };
    use bevy_abi_mod_sys::{BamComponent, ComponentDesc, ComponentUuid, FfiEntity, FfiResult};
    use bevy_ecs::entity::Entity;
    use bevy_ecs::world::World;
    use core::sync::atomic::AtomicBool;
    use std::sync::Arc;

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

    fn register_dynamic_in_world(
        world: &mut World,
        registry: &mut ModRegistry,
        desc: &ComponentDesc,
    ) -> BamComponent {
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            registry.clone(),
            Arc::new(AtomicBool::new(true)),
            world as *mut World,
            None,
        );
        let mut out = BamComponent::INVALID;
        assert_eq!(
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), desc, &mut out) },
            FfiResult::OK
        );
        *registry = load_ctx.registry.clone();
        load_ctx.finish_init();
        out
    }

    #[test]
    fn test_cmd_insert_rapid_overwrites_on_same_entity() {
        let mut world = World::new();
        let mut registry = ModRegistry::new();

        let desc = ComponentDesc::new(
            c"ffi::overwrites::RustModCustomData".as_ptr(),
            ComponentUuid::from_parts(
                "ffi::overwrites::RustModCustomData",
                core::mem::size_of::<RustModCustomData>(),
                core::mem::align_of::<RustModCustomData>(),
                0,
            ),
            core::mem::size_of::<RustModCustomData>(),
            core::mem::align_of::<RustModCustomData>(),
        );
        let comp_h = register_dynamic_in_world(&mut world, &mut registry, &desc);
        let snapshot = registry.snapshot_arc();
        let comp_id = snapshot.get_by_handle(comp_h).unwrap().component_id;

        let mut storage = HostCommandsStorage::new();
        let mut ffi_e = FfiEntity::NULL;
        {
            let cell = world.as_unsafe_world_cell();
            let mut cmds = HostCommands::new(&mut storage, cell.entity_allocator(), &snapshot);
            let raw_cmds = cmds.as_ffi_ptr();
            assert_eq!(
                unsafe { host_cmd_spawn_empty(raw_cmds, &mut ffi_e) },
                FfiResult::OK
            );

            for val in [10.0f32, 25.0, 50.0, 75.0, 99.0] {
                let data = RustModCustomData {
                    power: val,
                    shield: val as u32 * 2,
                };
                assert_eq!(
                    unsafe {
                        host_cmd_insert(
                            raw_cmds,
                            ffi_e,
                            comp_h,
                            &data as *const RustModCustomData as *const u8,
                            core::mem::size_of::<RustModCustomData>(),
                        )
                    },
                    FfiResult::OK
                );
            }
        }

        storage.apply_to_world(&mut world, &snapshot);

        let bevy_entity = Entity::try_from_bits(ffi_e.to_bits()).unwrap();
        let entity_ref = world.entity(bevy_entity);
        let final_data = unsafe {
            *(entity_ref.get_by_id(comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        assert_eq!(
            final_data,
            RustModCustomData {
                power: 99.0,
                shield: 198,
            }
        );
    }

    #[test]
    fn test_cmd_insert_and_despawn_before_flush() {
        let mut world = World::new();
        let mut registry = ModRegistry::new();

        let desc = ComponentDesc::new(
            c"ffi::overwrites::CModCustomData".as_ptr(),
            ComponentUuid::from_parts(
                "ffi::overwrites::CModCustomData",
                core::mem::size_of::<CModCustomData>(),
                core::mem::align_of::<CModCustomData>(),
                0,
            ),
            core::mem::size_of::<CModCustomData>(),
            core::mem::align_of::<CModCustomData>(),
        );
        let comp_h = register_dynamic_in_world(&mut world, &mut registry, &desc);
        let snapshot = registry.snapshot_arc();

        let mut storage = HostCommandsStorage::new();
        let mut ffi_e = FfiEntity::NULL;
        {
            let cell = world.as_unsafe_world_cell();
            let mut cmds = HostCommands::new(&mut storage, cell.entity_allocator(), &snapshot);
            let raw_cmds = cmds.as_ffi_ptr();
            assert_eq!(
                unsafe { host_cmd_spawn_empty(raw_cmds, &mut ffi_e) },
                FfiResult::OK
            );
            let c_data = CModCustomData {
                mana: 42.0,
                level: 7,
            };
            assert_eq!(
                unsafe {
                    host_cmd_insert(
                        raw_cmds,
                        ffi_e,
                        comp_h,
                        &c_data as *const CModCustomData as *const u8,
                        core::mem::size_of::<CModCustomData>(),
                    )
                },
                FfiResult::OK
            );
            assert_eq!(unsafe { host_cmd_despawn(raw_cmds, ffi_e) }, FfiResult::OK);
        }

        storage.apply_to_world(&mut world, &snapshot);

        let bevy_entity = Entity::try_from_bits(ffi_e.to_bits()).unwrap();
        assert!(world.get_entity(bevy_entity).is_err());
    }

    #[test]
    fn test_cmd_insert_on_invalid_or_dead_entity() {
        let mut world = World::new();
        let mut registry = ModRegistry::new();

        let desc = ComponentDesc::new(
            c"ffi::overwrites::CModCustomDataDead".as_ptr(),
            ComponentUuid::from_parts(
                "ffi::overwrites::CModCustomDataDead",
                core::mem::size_of::<CModCustomData>(),
                core::mem::align_of::<CModCustomData>(),
                0,
            ),
            core::mem::size_of::<CModCustomData>(),
            core::mem::align_of::<CModCustomData>(),
        );
        let comp_h = register_dynamic_in_world(&mut world, &mut registry, &desc);
        let snapshot = registry.snapshot_arc();

        let mut storage = HostCommandsStorage::new();
        {
            let cell = world.as_unsafe_world_cell();
            let mut cmds = HostCommands::new(&mut storage, cell.entity_allocator(), &snapshot);
            let raw_cmds = cmds.as_ffi_ptr();

            let dead_entity =
                FfiEntity::from_bits(Entity::from_raw_u32(777_777).unwrap().to_bits());
            let c_data = CModCustomData {
                mana: 1.0,
                level: 1,
            };
            assert_eq!(
                unsafe {
                    host_cmd_insert(
                        raw_cmds,
                        dead_entity,
                        comp_h,
                        &c_data as *const CModCustomData as *const u8,
                        core::mem::size_of::<CModCustomData>(),
                    )
                },
                FfiResult::OK
            );
        }

        storage.apply_to_world(&mut world, &snapshot);
    }

    #[test]
    fn test_cmd_insert_boundary_sizes_and_alignments() {
        let mut world = World::new();
        let mut registry = ModRegistry::new();

        let desc_zst = ComponentDesc::new(
            c"ffi::overwrites::ZstComponent".as_ptr(),
            ComponentUuid::from_parts("ffi::overwrites::ZstComponent", 0, 1, 0),
            0,
            1,
        );
        let zst_h = register_dynamic_in_world(&mut world, &mut registry, &desc_zst);

        const LARGE_SIZE: usize = 65_536;
        let desc_large = ComponentDesc::new(
            c"ffi::overwrites::LargeComponent".as_ptr(),
            ComponentUuid::from_parts("ffi::overwrites::LargeComponent", LARGE_SIZE, 16, 0),
            LARGE_SIZE,
            16,
        );
        let large_h = register_dynamic_in_world(&mut world, &mut registry, &desc_large);

        let snapshot = registry.snapshot_arc();
        let zst_id = snapshot.get_by_handle(zst_h).unwrap().component_id;
        let large_id = snapshot.get_by_handle(large_h).unwrap().component_id;

        let mut storage = HostCommandsStorage::new();
        let mut ffi_e = FfiEntity::NULL;
        {
            let cell = world.as_unsafe_world_cell();
            let mut cmds = HostCommands::new(&mut storage, cell.entity_allocator(), &snapshot);
            let raw_cmds = cmds.as_ffi_ptr();
            assert_eq!(
                unsafe { host_cmd_spawn_empty(raw_cmds, &mut ffi_e) },
                FfiResult::OK
            );
            assert_eq!(
                unsafe { host_cmd_insert(raw_cmds, ffi_e, zst_h, core::ptr::null(), 0) },
                FfiResult::OK
            );

            let mut large_buf = vec![0x5Au8; LARGE_SIZE];
            large_buf[0] = 0x11;
            large_buf[LARGE_SIZE - 1] = 0x99;
            assert_eq!(
                unsafe {
                    host_cmd_insert(raw_cmds, ffi_e, large_h, large_buf.as_ptr(), LARGE_SIZE)
                },
                FfiResult::OK
            );
        }

        storage.apply_to_world(&mut world, &snapshot);

        let bevy_entity = Entity::try_from_bits(ffi_e.to_bits()).unwrap();
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
        let mut registry = ModRegistry::new();

        let desc = ComponentDesc::new(
            c"ffi::overwrites::StackScrambleData".as_ptr(),
            ComponentUuid::from_parts(
                "ffi::overwrites::StackScrambleData",
                core::mem::size_of::<RustModCustomData>(),
                core::mem::align_of::<RustModCustomData>(),
                0,
            ),
            core::mem::size_of::<RustModCustomData>(),
            core::mem::align_of::<RustModCustomData>(),
        );
        let comp_h = register_dynamic_in_world(&mut world, &mut registry, &desc);
        let snapshot = registry.snapshot_arc();
        let comp_id = snapshot.get_by_handle(comp_h).unwrap().component_id;

        let mut storage = HostCommandsStorage::new();
        let mut ffi_e = FfiEntity::NULL;
        {
            let cell = world.as_unsafe_world_cell();
            let mut cmds = HostCommands::new(&mut storage, cell.entity_allocator(), &snapshot);
            let raw_cmds = cmds.as_ffi_ptr();
            assert_eq!(
                unsafe { host_cmd_spawn_empty(raw_cmds, &mut ffi_e) },
                FfiResult::OK
            );

            let mut stack_data = RustModCustomData {
                power: 777.0,
                shield: 888,
            };
            assert_eq!(
                unsafe {
                    host_cmd_insert(
                        raw_cmds,
                        ffi_e,
                        comp_h,
                        &stack_data as *const RustModCustomData as *const u8,
                        core::mem::size_of::<RustModCustomData>(),
                    )
                },
                FfiResult::OK
            );

            stack_data.power = -9999.0;
            stack_data.shield = 0xDEAD_BEEF;
            assert_eq!(stack_data.shield, 0xDEAD_BEEF);
        }

        storage.apply_to_world(&mut world, &snapshot);

        let bevy_entity = Entity::try_from_bits(ffi_e.to_bits()).unwrap();
        let entity_ref = world.entity(bevy_entity);
        let read_data = unsafe {
            *(entity_ref.get_by_id(comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        assert_eq!(
            read_data,
            RustModCustomData {
                power: 777.0,
                shield: 888,
            }
        );
    }
}

mod multi_archetype_scaling_oracle {
    use crate::support::*;
    use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, LoadOptions, ModAppExt};
    use bevy_app::App;
    use std::collections::HashMap;

    #[test]
    fn test_extreme_multi_archetype_scaling_and_arithmetic_oracle() {
        let (rust_dylib, c_dylib) = ensure_guest_mods_built();

        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        app.register_mod_component::<Position>(ComponentOptions::new().with_default())
            .unwrap();
        app.register_mod_component::<Velocity>(ComponentOptions::new().with_default())
            .unwrap();
        app.register_mod_component::<CombatHealth>(ComponentOptions::new().with_default())
            .unwrap();
        app.register_mod_component::<NameTag>(ComponentOptions::new().with_default())
            .unwrap();
        app.register_mod_component::<MetadataTag>(ComponentOptions::new().with_default())
            .unwrap();
        app.register_mod_component::<Mass>(ComponentOptions::new().with_default())
            .unwrap();
        app.register_mod_component::<Armor>(ComponentOptions::new().with_default())
            .unwrap();

        let mut expected_arithmetic = HashMap::new();
        let mut entity_archetype_mask = HashMap::new();
        let mut total_spawned = 0usize;

        for mask in 0u32..32 {
            let count = match mask {
                0 => 0,
                1 => 1,
                2 => 7,
                3 => 13,
                4 => 64,
                _ => 500,
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

        assert!(total_spawned > 13_000);

        let r_id = app
            .load_mod_with(&rust_dylib, LoadOptions::new().with_shadow_copy(true))
            .expect("load rust mod");
        let c_id = app
            .load_mod_with(&c_dylib, LoadOptions::new().with_shadow_copy(true))
            .expect("load c mod");

        const TICKS: usize = 10;
        for tick in 1..=TICKS {
            app.update();

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
        let mut verified_count = 0usize;
        for (&entity, &expected) in &expected_arithmetic {
            let actual = world.get::<Position>(entity).unwrap();
            assert_eq!(actual.x, expected.x);
            assert_eq!(actual.y, expected.y);
            assert_eq!(actual.z, expected.z);
            verified_count += 1;
        }
        assert_eq!(verified_count, expected_arithmetic.len());

        app.unload_mod(r_id).unwrap();
        app.unload_mod(c_id).unwrap();
    }

    #[test]
    fn test_100k_scale_fragmented_archetype_traversal_and_arithmetic() {
        let (rust_dylib, c_dylib) = ensure_guest_mods_built();

        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        app.register_mod_component::<Position>(ComponentOptions::new().with_default())
            .unwrap();
        app.register_mod_component::<Velocity>(ComponentOptions::new().with_default())
            .unwrap();
        app.register_mod_component::<Mass>(ComponentOptions::new().with_default())
            .unwrap();
        app.register_mod_component::<Armor>(ComponentOptions::new().with_default())
            .unwrap();

        const SCALE: usize = 100_000;
        const ARCHETYPES: usize = 16;
        const PER_ARCH: usize = SCALE / ARCHETYPES;

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

        let r_id = app
            .load_mod_with(&rust_dylib, LoadOptions::new().with_shadow_copy(true))
            .unwrap();
        let c_id = app
            .load_mod_with(&c_dylib, LoadOptions::new().with_shadow_copy(true))
            .unwrap();

        for _ in 0..5 {
            app.update();
        }

        let mut kin_count = 0usize;
        let mut non_kin_count = 0usize;

        let mut query = app
            .world_mut()
            .query::<(&Position, Option<&Velocity>, Option<&Mass>)>();
        for (pos, vel, mass) in query.iter(app.world()) {
            if vel.is_some() {
                assert_eq!(pos.y, 15.0);
                assert_eq!(pos.z, 5.0);
                kin_count += 1;
            } else if mass.is_some() {
                assert_eq!(pos.y, 0.0);
                assert_eq!(pos.z, 5.0);
                non_kin_count += 1;
            }
        }

        assert_eq!(kin_count, 50_000);
        assert_eq!(non_kin_count, 50_000);

        app.unload_mod(r_id).unwrap();
        app.unload_mod(c_id).unwrap();
    }
}

mod change_detection_archetype_isolation {
    use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModRegistry};
    use bevy_abi_mod_host_sys::{
        BamQuerySpecOwned, GuestSystem, host_iter_column_mutable,
        host_iter_column_mutable_untracked, host_iter_column_readonly,
        host_iter_mark_changed_range, host_iter_next_chunk, host_sys_query,
    };
    use bevy_abi_mod_macros::ModComponent;
    use bevy_abi_mod_sys::{AccessMode, BamIter, BamSystemCtx, BamTermDesc, FfiResult};
    use bevy_app::{App, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::prelude::Resource;
    use bevy_ecs::query::Changed;
    use bevy_ecs::schedule::IntoScheduleConfigs;
    use bevy_ecs::system::{Query, ResMut};
    use core::ffi::c_void;

    #[repr(C)]
    #[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
    #[mod_component(name = "ffi::cd_isolation::Position")]
    pub struct Position {
        pub x: f32,
        pub y: f32,
        pub z: f32,
    }

    #[repr(C)]
    #[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
    #[mod_component(name = "ffi::cd_isolation::Velocity")]
    pub struct Velocity {
        pub x: f32,
        pub y: f32,
        pub z: f32,
    }

    #[repr(C)]
    #[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
    #[mod_component(name = "ffi::cd_isolation::Health")]
    pub struct Health {
        pub current: i32,
        pub max: i32,
    }

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
        mode: usize,
    }

    unsafe extern "C" fn change_sim_callback(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let ctx = unsafe { &*(user_data as *const ChangeSimContext) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }
        let mut count = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            match ctx.mode {
                1 => {
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
                    let _ = unsafe { host_iter_column_mutable_untracked(iter, 0) };
                    if count > 7 {
                        let _ = unsafe { host_iter_mark_changed_range(iter, 0, 3, 1) };
                        let _ = unsafe { host_iter_mark_changed_range(iter, 0, 7, 1) };
                    }
                }
                _ => {
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
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();

        for i in 0..10 {
            app.world_mut().spawn(Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            });
        }

        let registry = app.world().resource::<ModRegistry>().clone();
        let mut sim_ctx = Box::new(ChangeSimContext { mode: 0 });
        let sys = GuestSystem::new(
            "change_sim",
            vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                pos_h,
                AccessMode::WRITE,
            )])],
            change_sim_callback,
            (&mut *sim_ctx as *mut ChangeSimContext).cast::<c_void>(),
            registry,
        );
        app.add_systems(Update, (sys, track_changed_positions).chain());

        app.update();
        assert_eq!(app.world().resource::<ChangeTracker>().changed_count, 10);

        sim_ctx.mode = 0;
        app.update();
        assert_eq!(app.world().resource::<ChangeTracker>().changed_count, 0);

        sim_ctx.mode = 1;
        app.update();
        assert_eq!(app.world().resource::<ChangeTracker>().changed_count, 10);

        sim_ctx.mode = 0;
        app.update();
        assert_eq!(app.world().resource::<ChangeTracker>().changed_count, 0);

        sim_ctx.mode = 2;
        app.update();
        assert_eq!(app.world().resource::<ChangeTracker>().changed_count, 2);
    }

    #[test]
    fn test_change_detection_multi_table_archetype_isolation() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.init_resource::<ChangeTracker>();
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let vel_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();
        app.register_mod_component::<Health>(ComponentOptions::new())
            .unwrap();

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
        let mut sim_ctx = Box::new(ChangeSimContext { mode: 0 });
        let sys = GuestSystem::new(
            "kinematics_mutator",
            vec![BamQuerySpecOwned::new(vec![
                BamTermDesc::new(pos_h, AccessMode::WRITE),
                BamTermDesc::new(vel_h, AccessMode::READ),
            ])],
            change_sim_callback,
            (&mut *sim_ctx as *mut ChangeSimContext).cast::<c_void>(),
            registry,
        );
        app.add_systems(Update, (sys, track_changed_positions).chain());

        app.update();
        assert_eq!(app.world().resource::<ChangeTracker>().changed_count, 10);

        sim_ctx.mode = 0;
        app.update();
        assert_eq!(app.world().resource::<ChangeTracker>().changed_count, 0);

        sim_ctx.mode = 1;
        app.update();
        assert_eq!(app.world().resource::<ChangeTracker>().changed_count, 5);
    }

    #[test]
    fn test_change_detection_read_mode_defensive_guard() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.init_resource::<ChangeTracker>();
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();

        app.world_mut().spawn(Position {
            x: 10.0,
            y: 20.0,
            z: 30.0,
        });

        unsafe extern "C" fn illegal_mutator_callback(
            sys_ctx: *mut BamSystemCtx,
            _user_data: *mut c_void,
        ) -> FfiResult {
            let mut iter: *mut BamIter = core::ptr::null_mut();
            let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
            if rc.is_err() {
                return rc;
            }
            let mut count = 0usize;
            while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
                assert!(unsafe { host_iter_column_mutable(iter, 0) }.is_null());
                assert_eq!(
                    unsafe { host_iter_mark_changed_range(iter, 0, 0, 1) },
                    FfiResult::PERMISSION_DENIED
                );
            }
            FfiResult::OK
        }

        let registry = app.world().resource::<ModRegistry>().clone();
        let sys = GuestSystem::new(
            "read_only_sys",
            vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                pos_h,
                AccessMode::READ,
            )])],
            illegal_mutator_callback,
            core::ptr::null_mut(),
            registry,
        );
        app.add_systems(Update, (sys, track_changed_positions).chain());

        app.update();
        assert_eq!(app.world().resource::<ChangeTracker>().changed_count, 1);

        app.update();
        assert_eq!(app.world().resource::<ChangeTracker>().changed_count, 0);
    }
}
