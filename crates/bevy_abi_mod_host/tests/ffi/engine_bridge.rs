//! Host `ModRegistry`, `AbiModPlugin`, `GuestSystem`, `HostCommands`, and 10k-entity SoA traversal (`ABI 0.2.0`).

mod registry_and_command_bridge {
    use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModComponent, ModRegistry};
    use bevy_abi_mod_host_sys::{
        BamQuerySpecOwned, GuestSystem, HOST_API, HostCommands, HostCommandsStorage, HostIter,
        LoadContext, QueryPlan, QueryTermMeta, host_cmd_despawn, host_cmd_insert, host_cmd_spawn,
        host_cmd_spawn_empty, host_get_proc_address, host_iter_column_info,
        host_iter_column_mutable, host_iter_column_readonly, host_iter_entities,
        host_iter_mark_changed_range, host_iter_next_chunk, host_load_host_ctx,
        host_register_component, host_sys_query,
    };
    use bevy_abi_mod_sys::{
        ABI_VERSION, AccessMode, BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET,
        BamColumnInfo, BamComponent, BamHostApi, BamIter, BamSystemCtx, BamTermDesc, ComponentDesc,
        ComponentUuid, FfiEntity, FfiResult, ModComponent as _,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::change_detection::Tick;
    use bevy_ecs::component::Component;
    use bevy_ecs::prelude::World;
    use bevy_ecs::query::Changed;
    use bevy_ecs::schedule::IntoScheduleConfigs;
    use bevy_ecs::system::System;
    use core::ffi::c_void;
    use core::mem::MaybeUninit;
    use core::sync::atomic::AtomicBool;
    use std::sync::Arc;

    #[repr(C)]
    #[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
    #[mod_component(name = "bridge::Position")]
    pub struct Position {
        pub x: f32,
        pub y: f32,
        pub z: f32,
    }

    #[repr(C)]
    #[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
    #[mod_component(name = "bridge::Velocity")]
    pub struct Velocity {
        pub x: f32,
        pub y: f32,
        pub z: f32,
    }

    #[repr(C)]
    #[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
    #[mod_component(name = "bridge::Health")]
    pub struct Health {
        pub current: i32,
        pub max: i32,
    }

    #[repr(C)]
    #[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
    #[mod_component(name = "bridge::Armor")]
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
        let pos_h = registry
            .register_component::<Position>(pos_id, ComponentOptions::new())
            .expect("register Position");
        let vel_h = registry
            .register_component::<Velocity>(vel_id, ComponentOptions::new())
            .expect("register Velocity");

        assert_eq!(pos_h, BamComponent::from_raw(1));
        assert_eq!(vel_h, BamComponent::from_raw(2));

        // Lookup by Handle & ID
        let pos_meta = registry
            .get_by_id(pos_id)
            .expect("Position should be found by ID");
        assert_eq!(pos_meta.handle, pos_h);
        assert_eq!(pos_meta.name.as_ref(), "bridge::Position");
        assert_eq!(pos_meta.size, core::mem::size_of::<Position>());
        assert_eq!(pos_meta.align, core::mem::align_of::<Position>());
        assert_eq!(pos_meta.vtable_ptr.as_ptr(), Position::erased_vtable_ptr());

        // Lookup by Name
        let pos_by_name = registry
            .get_by_name("bridge::Position")
            .expect("Position found by name");
        assert_eq!(pos_by_name.component_id, pos_id);

        // Lookup by UUID
        let pos_by_uuid = registry
            .get_by_uuid(&Position::UUID)
            .expect("Position found by UUID");
        assert_eq!(pos_by_uuid.component_id, pos_id);

        // `"bam.v0.*"` symbol resolution (`C-04`)
        let snap = registry.snapshot();
        assert_eq!(
            snap.resolve_symbol("bam.v0.bridge::Position.vtable"),
            Some(Position::erased_vtable_ptr())
        );
        assert!(
            snap.resolve_symbol("bam.v0.bridge::Position.get_x")
                .is_some()
        );
        assert!(snap.resolve_symbol("bridge::Position").is_none());
        assert!(snap.resolve_symbol("bam.v0.NonExistent").is_none());
    }

    #[test]
    fn test_host_get_proc_address_scoped_and_global() {
        let mut world_a = World::new();
        let health_id = world_a.register_component::<Health>();

        let mut registry_a = ModRegistry::new();
        registry_a
            .register_component::<Health>(health_id, ComponentOptions::new())
            .expect("register Health");

        let registry_b = ModRegistry::new();

        let mut load_a = LoadContext::new(registry_a);
        let mut load_b = LoadContext::new(registry_b);

        // 1. Scoped `BamHostCtx` lookup on `load_a` resolves `"bam.v0.*"` symbols, while `load_b`
        //    stays completely isolated (zero process-global symbol table in ABI v0.2, `C-04`).
        unsafe {
            let ctx_a = host_load_host_ctx(load_a.as_ffi_ptr());
            let ctx_b = host_load_host_ctx(load_b.as_ffi_ptr());

            let ptr_a = host_get_proc_address(ctx_a, c"bam.v0.bridge::Health.vtable".as_ptr());
            assert_eq!(ptr_a, Health::erased_vtable_ptr());

            let fn_ptr =
                host_get_proc_address(ctx_a, c"bam.v0.bridge::Health.get_current".as_ptr());
            assert!(!fn_ptr.is_null());

            assert!(host_get_proc_address(ctx_a, c"bam.v0.Unknown".as_ptr()).is_null());
            assert!(host_get_proc_address(ctx_a, core::ptr::null()).is_null());
            assert!(
                host_get_proc_address(core::ptr::null(), c"bam.v0.bridge::Health.vtable".as_ptr())
                    .is_null()
            );

            // Unregistered in `ctx_b`: zero global leakage!
            assert!(
                host_get_proc_address(ctx_b, c"bam.v0.bridge::Health.vtable".as_ptr()).is_null()
            );
        }

        // 2. Poisoning `load_a` via `finish_init` immediately invalidates `ctx_a` (`S-08`).
        let stale_ctx_a = unsafe { host_load_host_ctx(load_a.as_ffi_ptr()) };
        load_a.finish_init();
        load_b.finish_init();
        assert!(
            unsafe { host_get_proc_address(stale_ctx_a, c"bam.v0.bridge::Health.vtable".as_ptr()) }
                .is_null()
        );
    }

    // ============================================================================
    // 2. HostCommands (`CommandArena`) Tests
    // ============================================================================

    #[test]
    fn test_command_queue_bridge_spawn_insert_despawn() {
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
        let snap = registry.snapshot_arc();

        let mut storage = HostCommandsStorage::new();
        let pos_data = Position {
            x: 10.0,
            y: 20.0,
            z: 30.0,
        };
        let vel_data = Velocity {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        };

        // 1. Queue `spawn_empty` + `insert` AND single-archetype `spawn` batch (`P-05`)
        let (ffi_entity, batched_entity) = {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let raw_cmds = cmds.as_ffi_ptr();

            let mut entity = FfiEntity::NULL;
            assert_eq!(
                unsafe { host_cmd_spawn_empty(raw_cmds, &mut entity) },
                FfiResult::OK
            );
            assert!(!entity.is_null());

            let status = unsafe {
                host_cmd_insert(
                    raw_cmds,
                    entity,
                    pos_h,
                    (&pos_data as *const Position).cast::<u8>(),
                    core::mem::size_of::<Position>(),
                )
            };
            assert_eq!(status, FfiResult::OK);

            let batch_comps = [pos_h, vel_h];
            let batch_ptrs = [
                (&pos_data as *const Position).cast::<u8>(),
                (&vel_data as *const Velocity).cast::<u8>(),
            ];
            let mut e_batch = FfiEntity::NULL;
            assert_eq!(
                unsafe {
                    host_cmd_spawn(
                        raw_cmds,
                        batch_comps.as_ptr(),
                        batch_ptrs.as_ptr(),
                        2,
                        &mut e_batch,
                    )
                },
                FfiResult::OK
            );
            (entity, e_batch)
        };

        // Before apply, entities are reserved in allocator but not yet spawned in World
        let bevy_entity = bevy_ecs::entity::Entity::from_bits(ffi_entity.to_bits());
        let bevy_batch = bevy_ecs::entity::Entity::from_bits(batched_entity.to_bits());
        assert!(world.get_entity(bevy_entity).is_err());
        assert!(world.get_entity(bevy_batch).is_err());

        // 2. Flush command storage
        storage.apply_to_world(&mut world, &snap);

        assert_eq!(world.get::<Position>(bevy_entity), Some(&pos_data));
        assert_eq!(world.get::<Position>(bevy_batch), Some(&pos_data));
        assert_eq!(world.get::<Velocity>(bevy_batch), Some(&vel_data));

        // 3. Queue despawn
        {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let raw_cmds = cmds.as_ffi_ptr();
            assert_eq!(
                unsafe { host_cmd_despawn(raw_cmds, ffi_entity) },
                FfiResult::OK
            );
            assert_eq!(
                unsafe { host_cmd_despawn(raw_cmds, batched_entity) },
                FfiResult::OK
            );
        }

        storage.apply_to_world(&mut world, &snap);
        assert!(world.get_entity(bevy_entity).is_err());
        assert!(world.get_entity(bevy_batch).is_err());
    }

    #[test]
    fn test_command_queue_bridge_defensive_validation() {
        let world = World::new();
        let registry = ModRegistry::new();
        let snap = registry.snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        let raw_cmds = cmds.as_ffi_ptr();

        let dummy = [0u8; 12];
        let mut out_ent = FfiEntity::NULL;

        // Null pointers
        assert_eq!(
            unsafe { host_cmd_spawn_empty(core::ptr::null_mut(), &mut out_ent) },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe { host_cmd_spawn_empty(raw_cmds, core::ptr::null_mut()) },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe { host_cmd_despawn(core::ptr::null_mut(), FfiEntity(1)) },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe {
                host_cmd_insert(
                    core::ptr::null_mut(),
                    FfiEntity(1),
                    BamComponent::from_raw(1),
                    dummy.as_ptr(),
                    12,
                )
            },
            FfiResult::NULL_POINTER
        );

        // Invalid / unregistered `BamComponent` handles
        assert_eq!(
            unsafe {
                host_cmd_insert(
                    raw_cmds,
                    FfiEntity(1),
                    BamComponent::INVALID,
                    dummy.as_ptr(),
                    12,
                )
            },
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            unsafe {
                host_cmd_insert(
                    raw_cmds,
                    FfiEntity(1),
                    BamComponent::from_raw(99),
                    dummy.as_ptr(),
                    12,
                )
            },
            FfiResult::INVALID_COMPONENT
        );
    }

    // ============================================================================
    // 3. HostIter & BamIter Tests
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

        let this_run = world.increment_change_tick();
        let cell = world.as_unsafe_world_cell();
        let storages = unsafe { cell.storages() };

        let plan = QueryPlan::from_terms(vec![
            QueryTermMeta {
                component: BamComponent::from_raw(1),
                component_id: pos_id,
                mode: AccessMode::WRITE,
                uuid: Position::UUID,
                size: core::mem::size_of::<Position>(),
                align: core::mem::align_of::<Position>(),
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
            },
            QueryTermMeta {
                component: BamComponent::from_raw(2),
                component_id: vel_id,
                mode: AccessMode::READ,
                uuid: Velocity::UUID,
                size: core::mem::size_of::<Velocity>(),
                align: core::mem::align_of::<Velocity>(),
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
            },
        ]);

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

        let mut host_iter = HostIter::new_dense(
            &storages.tables,
            &matched_tables,
            &plan,
            Tick::new(0),
            this_run,
        );
        let raw_iter = host_iter.as_ffi_ptr();

        let mut total_entities = 0;
        let mut count = 0usize;
        while unsafe { host_iter_next_chunk(raw_iter, &mut count) }.get() {
            total_entities += count;

            let entities_ptr = unsafe { host_iter_entities(raw_iter) };
            assert!(!entities_ptr.is_null());

            // Check column 0 (`Position`: `WRITE`) + `iter_column_info` (`C-13`)
            let pos_mut = unsafe { host_iter_column_mutable(raw_iter, 0) };
            assert!(!pos_mut.is_null());
            let mut info_0 = MaybeUninit::<BamColumnInfo>::uninit();
            assert_eq!(
                unsafe { host_iter_column_info(raw_iter, 0, info_0.as_mut_ptr()) },
                FfiResult::OK
            );
            let info_0 = unsafe { info_0.assume_init() };
            assert_eq!(info_0.size, core::mem::size_of::<Position>());
            assert_eq!(info_0.uuid, Position::UUID);

            // Check column 1 (`Velocity`: `READ`)
            let vel_ro = unsafe { host_iter_column_readonly(raw_iter, 1) };
            assert!(!vel_ro.is_null());
            let mut info_1 = MaybeUninit::<BamColumnInfo>::uninit();
            assert_eq!(
                unsafe { host_iter_column_info(raw_iter, 1, info_1.as_mut_ptr()) },
                FfiResult::OK
            );
            assert_eq!(
                unsafe { info_1.assume_init() }.size,
                core::mem::size_of::<Velocity>()
            );

            // Requesting mutable pointer on READ column MUST return null_mut
            let vel_mut_denied = unsafe { host_iter_column_mutable(raw_iter, 1) };
            assert!(
                vel_mut_denied.is_null(),
                "READ column must not allow mutable access"
            );

            // Mark individual row 0 changed
            let mark_res = unsafe { host_iter_mark_changed_range(raw_iter, 0, 0, 1) };
            assert_eq!(mark_res, FfiResult::OK);

            let pos = unsafe { &mut *(pos_mut as *mut Position) };
            pos.x += 1000.0;
        }

        assert_eq!(total_entities, 15);
    }

    // ============================================================================
    // 4. GuestSystem Execution & Scheduler Verification
    // ============================================================================

    unsafe extern "C" fn kinematics_guest_system(
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
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let vel_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();

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

        let registry = app.world().resource::<ModRegistry>().clone();
        let query_specs = vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(pos_h), BamTermDesc::read(vel_h)],
        }];

        let guest_sys = GuestSystem::new(
            "kinematics",
            query_specs,
            kinematics_guest_system,
            core::ptr::null_mut(),
            registry,
        );

        app.add_systems(Update, guest_sys);
        app.update();

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
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let vel_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();

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

        // Consume initial spawn ticks before registering guest mutator
        app.update();

        let registry = app.world().resource::<ModRegistry>().clone();
        let query_specs = vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(pos_h), BamTermDesc::read(vel_h)],
        }];

        let guest_sys = GuestSystem::new(
            "kinematics_mutator",
            query_specs,
            kinematics_guest_system,
            core::ptr::null_mut(),
            registry,
        );

        app.add_systems(Update, (guest_sys, count_changed_positions).chain());
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
        _sys_ctx: *mut BamSystemCtx,
        _user_data: *mut c_void,
    ) -> FfiResult {
        FfiResult::PANIC_CAUGHT
    }

    unsafe extern "C" fn internal_error_guest_system(
        _sys_ctx: *mut BamSystemCtx,
        _user_data: *mut c_void,
    ) -> FfiResult {
        FfiResult::INTERNAL_ERROR
    }

    #[test]
    fn test_dynamic_system_panic_and_error_containment() {
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
        let query_specs = vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::write(pos_h)],
        }];

        let mut panicking_sys = GuestSystem::new(
            "panic_caught_sys",
            query_specs.clone(),
            panic_caught_guest_system,
            core::ptr::null_mut(),
            registry.clone(),
        );
        assert!(panicking_sys.is_enabled());

        let _ = panicking_sys.initialize(app.world_mut());
        let res = panicking_sys.run((), app.world_mut());
        assert!(res.is_ok());
        assert!(
            !panicking_sys.is_enabled(),
            "System returning PANIC_CAUGHT must be quarantined"
        );

        let mut err_sys = GuestSystem::new(
            "err_sys",
            query_specs,
            internal_error_guest_system,
            core::ptr::null_mut(),
            registry,
        );
        let _ = err_sys.initialize(app.world_mut());
        let res2 = err_sys.run((), app.world_mut());
        assert!(res2.is_ok());
        assert!(
            !err_sys.is_enabled(),
            "System returning error status must be quarantined"
        );
    }

    // ============================================================================
    // 7. Dynamic Component Registration C-ABI Tests
    // ============================================================================

    unsafe fn call_register_component_fresh(desc: &ComponentDesc) -> FfiResult {
        let mut world = World::new();
        let reg = ModRegistry::new();
        world.insert_resource(reg.clone());
        let active = Arc::new(AtomicBool::new(true));
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(reg, active, &mut world as *mut World, None);
        let mut out = BamComponent::INVALID;
        let res = unsafe { host_register_component(load_ctx.as_ffi_ptr(), desc, &mut out) };
        load_ctx.finish_init();
        res
    }

    #[test]
    fn test_host_register_component_defensive_validation() {
        let name = c"bridge::CustomRuntimeComponent".as_ptr();
        let uuid = ComponentUuid::from_parts("bridge::CustomRuntimeComponent", 16, 8, 0);

        // 1. Valid descriptor and host context
        let valid_desc = ComponentDesc::new(name, uuid, 16, 8);
        assert_eq!(
            unsafe { call_register_component_fresh(&valid_desc) },
            FfiResult::OK
        );

        // 2. Null pointers (`load_ctx`, `desc`, `out_component`)
        let mut out = BamComponent::INVALID;
        assert_eq!(
            unsafe { host_register_component(core::ptr::null_mut(), &valid_desc, &mut out) },
            FfiResult::NULL_POINTER
        );

        let mut world = World::new();
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            ModRegistry::new(),
            Arc::new(AtomicBool::new(true)),
            &mut world as *mut World,
            None,
        );
        assert_eq!(
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), core::ptr::null(), &mut out) },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe {
                host_register_component(load_ctx.as_ffi_ptr(), &valid_desc, core::ptr::null_mut())
            },
            FfiResult::NULL_POINTER
        );
        load_ctx.finish_init();

        // 3. Truncated descriptor `struct_size`
        let mut truncated_desc = valid_desc;
        truncated_desc.struct_size = (ComponentDesc::MIN_V0_2_SIZE - 1) as u32;
        assert_eq!(
            unsafe { call_register_component_fresh(&truncated_desc) },
            FfiResult::INVALID_VERSION
        );

        // 4. Forward-compatible descriptor `struct_size` (`>= MIN_V0_2_SIZE`)
        let mut extended_desc = valid_desc;
        extended_desc.struct_size = 128;
        assert_eq!(
            unsafe { call_register_component_fresh(&extended_desc) },
            FfiResult::OK
        );

        // 5. Unknown flags rejected (`C-09`)
        let bad_flags_desc = valid_desc.with_flags(1 << 31);
        assert_eq!(
            unsafe { call_register_component_fresh(&bad_flags_desc) },
            FfiResult::UNSUPPORTED
        );

        // 6. Known flags (`SPARSE_SET | IMMUTABLE`) accepted (`B-08`)
        let flagged_desc =
            valid_desc.with_flags(BAM_COMPONENT_FLAG_SPARSE_SET | BAM_COMPONENT_FLAG_IMMUTABLE);
        assert_eq!(
            unsafe { call_register_component_fresh(&flagged_desc) },
            FfiResult::OK
        );

        // 7. Null / empty / invalid UTF-8 component name
        let null_name_desc = ComponentDesc::new(core::ptr::null(), uuid, 16, 8);
        assert_eq!(
            unsafe { call_register_component_fresh(&null_name_desc) },
            FfiResult::NULL_POINTER
        );
        let empty_name_desc = ComponentDesc::new(c"".as_ptr(), uuid, 16, 8);
        assert_eq!(
            unsafe { call_register_component_fresh(&empty_name_desc) },
            FfiResult::INVALID_ARGUMENT
        );
        let invalid_utf8_bytes = [0xffu8, 0xfe, 0x00];
        let bad_utf8_desc = ComponentDesc::new(invalid_utf8_bytes.as_ptr().cast(), uuid, 16, 8);
        assert_eq!(
            unsafe { call_register_component_fresh(&bad_utf8_desc) },
            FfiResult::INVALID_UTF8
        );

        // 8. Invalid zero / non-power-of-two / misaligned layouts
        let mut zero_align = valid_desc;
        zero_align.align = 0;
        assert_eq!(
            unsafe { call_register_component_fresh(&zero_align) },
            FfiResult::INVALID_ARGUMENT
        );

        let mut non_pot_align = valid_desc;
        non_pot_align.align = 6;
        assert_eq!(
            unsafe { call_register_component_fresh(&non_pot_align) },
            FfiResult::INVALID_ARGUMENT
        );

        let mut misaligned = valid_desc;
        misaligned.size = 15;
        misaligned.align = 8;
        assert_eq!(
            unsafe { call_register_component_fresh(&misaligned) },
            FfiResult::INVALID_ARGUMENT
        );

        // 9. Valid Zero-Sized Types (ZST)
        assert_eq!(
            unsafe { call_register_component_fresh(&ComponentDesc::new(name, uuid, 0, 1)) },
            FfiResult::OK
        );
        assert_eq!(
            unsafe { call_register_component_fresh(&ComponentDesc::new(name, uuid, 0, 16)) },
            FfiResult::OK
        );
    }

    #[test]
    fn test_host_get_proc_address_resolves_register_component() {
        // In ABI v0.2 (`C-03`, `C-04`), all 27 core host APIs live directly on `HOST_API`,
        // `ModRegistry::new()` registers zero built-in function symbols, and `host_get_proc_address`
        // strictly serves `"bam.v0.*"` custom symbols and component VTables.
        assert!(HOST_API.register_component.is_some());
        assert_eq!(
            HOST_API.register_component.unwrap() as *const () as usize,
            host_register_component as *const () as usize
        );

        let mut registry = ModRegistry::new();
        assert_eq!(registry.snapshot().symbol_count(), 0);
        registry.register_type::<Position>();

        let mut load_ctx = LoadContext::new(registry);
        unsafe {
            let hctx = host_load_host_ctx(load_ctx.as_ffi_ptr());
            assert!(host_get_proc_address(hctx, c"register_component".as_ptr()).is_null());
            assert!(host_get_proc_address(hctx, c"host_register_component".as_ptr()).is_null());
            assert_eq!(
                host_get_proc_address(hctx, c"bam.v0.bridge::Position.vtable".as_ptr()),
                Position::erased_vtable_ptr()
            );
        }
        load_ctx.finish_init();
    }

    #[test]
    fn test_host_api_vtable_and_register_component_offsets() {
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(BamHostApi::MIN_V0_2_SIZE, 224);
            assert_eq!(core::mem::size_of::<BamHostApi>(), 264);
            assert_eq!(core::mem::align_of::<BamHostApi>(), 8);
            assert_eq!(HOST_API.struct_size, 264);
            assert_eq!(core::mem::offset_of!(BamHostApi, register_component), 32);
        }
        assert_eq!(HOST_API.abi_version, ABI_VERSION);
        assert_eq!(
            HOST_API.register_component.unwrap() as *const () as usize,
            host_register_component as *const () as usize
        );

        // Direct invocation via `HOST_API.register_component`
        let mut world = World::new();
        let reg = ModRegistry::new();
        world.insert_resource(reg.clone());
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::new(AtomicBool::new(true)),
            &mut world as *mut World,
            None,
        );
        let desc = ComponentDesc::new(
            c"bridge::HealthPoints".as_ptr(),
            ComponentUuid::from_parts("bridge::HealthPoints", 4, 4, 0),
            4,
            4,
        );
        let mut out = BamComponent::INVALID;
        let res = unsafe {
            (HOST_API.register_component.unwrap())(load_ctx.as_ffi_ptr(), &desc, &mut out)
        };
        assert_eq!(res, FfiResult::OK);
        assert_eq!(out, BamComponent::from_raw(1));
        load_ctx.finish_init();
    }
}

mod plugin_and_typed_commands {
    use bevy_abi_mod_host::{
        AbiModPlugin, ComponentOptions, ModAppExt, ModComponent, ModHost, ModRegistry,
    };
    use bevy_abi_mod_host_sys::{
        HostCommands, HostCommandsStorage, host_cmd_insert, host_cmd_spawn_empty,
    };
    use bevy_abi_mod_sys::{FfiEntity, FfiResult};
    use bevy_app::App;
    use bevy_ecs::component::Component;

    #[repr(C)]
    #[derive(Component, ModComponent, Copy, Clone, Debug, PartialEq)]
    #[mod_component(name = "plugin_cmd::Health")]
    struct Health {
        hp: f32,
        max_hp: f32,
    }

    #[repr(C)]
    #[derive(Component, ModComponent, Copy, Clone, Debug, PartialEq)]
    #[mod_component(name = "plugin_cmd::Velocity")]
    struct Velocity {
        vx: f32,
        vy: f32,
    }

    #[test]
    fn test_abi_mod_plugin_traits() {
        let plugin = AbiModPlugin::default();
        let debug_str = format!("{plugin:?}");
        assert_eq!(debug_str, "AbiModPlugin");

        let plugin_copy = plugin;
        let plugin_clone = plugin.clone();

        let mut app = App::new();
        app.add_plugins(plugin_copy);
        assert!(app.world().get_resource::<ModRegistry>().is_some());
        assert!(app.world().get_resource::<ModHost>().is_some());

        app.update();
        let _ = plugin_clone;
    }

    #[test]
    fn test_abi_mod_plugin_default_construction_and_app_lifecycle() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin::default());

        assert!(app.world().contains_resource::<ModRegistry>());
        assert!(app.world().contains_resource::<ModHost>());

        app.register_mod_component::<Health>(ComponentOptions::new())
            .unwrap();
        app.register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();

        {
            let registry = app.world().resource::<ModRegistry>();
            assert!(registry.get_by_name("plugin_cmd::Health").is_some());
            assert!(registry.get_by_name("plugin_cmd::Velocity").is_some());
            assert_eq!(
                registry.get_by_name("plugin_cmd::Health").unwrap().size,
                core::mem::size_of::<Health>()
            );
        }

        for _ in 0..100 {
            app.update();
        }
    }

    #[test]
    fn test_deferred_command_bridge_with_plugin_registered_types() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin::default());
        let health_h = app
            .register_mod_component::<Health>(ComponentOptions::new())
            .unwrap();

        let mut storage = HostCommandsStorage::new();
        let world = app.world_mut();
        let snap = world.resource::<ModRegistry>().snapshot_arc();

        let spawned_ffi = {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let raw_cmds = cmds.as_ffi_ptr();

            let mut ent = FfiEntity::NULL;
            assert_eq!(
                unsafe { host_cmd_spawn_empty(raw_cmds, &mut ent) },
                FfiResult::OK
            );
            assert!(!ent.is_null());

            let health = Health {
                hp: 75.0,
                max_hp: 100.0,
            };
            let insert_res = unsafe {
                host_cmd_insert(
                    raw_cmds,
                    ent,
                    health_h,
                    (&health as *const Health).cast::<u8>(),
                    core::mem::size_of::<Health>(),
                )
            };
            assert_eq!(insert_res, FfiResult::OK);
            ent
        };

        storage.apply_to_world(world, &snap);

        let bevy_entity = bevy_ecs::entity::Entity::from_bits(spawned_ffi.to_bits());
        assert_eq!(
            world.get::<Health>(bevy_entity),
            Some(&Health {
                hp: 75.0,
                max_hp: 100.0,
            })
        );
    }
}

mod runtime_10k_soa_pipeline {
    use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModComponent, ModRegistry};
    use bevy_abi_mod_host_sys::{
        BamQuerySpecOwned, GuestSystem, host_cmd_despawn, host_cmd_insert, host_cmd_spawn_empty,
        host_iter_column_info, host_iter_column_mutable, host_iter_column_readonly,
        host_iter_entities, host_iter_next_chunk, host_sys_commands, host_sys_query,
    };
    use bevy_abi_mod_sys::{
        BamColumnInfo, BamCommands, BamComponent, BamIter, BamSystemCtx, BamTermDesc, FfiEntity,
        FfiResult,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::prelude::{ResMut, Resource};
    use bevy_ecs::query::Changed;
    use bevy_ecs::schedule::IntoScheduleConfigs;
    use bevy_ecs::system::Query;
    use core::ffi::c_void;
    use core::mem::MaybeUninit;

    #[repr(C)]
    #[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
    #[mod_component(name = "soa10k::Position")]
    pub struct Position {
        pub x: f32,
        pub y: f32,
        pub z: f32,
    }

    #[repr(C)]
    #[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
    #[mod_component(name = "soa10k::Velocity")]
    pub struct Velocity {
        pub x: f32,
        pub y: f32,
        pub z: f32,
    }

    #[derive(Component, Debug, Clone, PartialEq, ModComponent)]
    #[mod_component(name = "soa10k::NameTag")]
    pub struct NameTag {
        pub label: String,
    }

    #[derive(Resource, Default)]
    struct ChangedPositionCounter(usize);

    fn count_changed_positions(
        query: Query<&Position, Changed<Position>>,
        mut counter: ResMut<ChangedPositionCounter>,
    ) {
        counter.0 = query.iter().count();
    }

    struct Runtime10kUserData {
        pos_comp: BamComponent,
        total_processed: usize,
        first_despawned: FfiEntity,
        spawned_entity: FfiEntity,
    }

    unsafe extern "C" fn host_runtime_10k_guest_system(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let data = unsafe { &mut *(user_data as *mut Runtime10kUserData) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        let mut commands: *mut BamCommands = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
            FfiResult::OK
        );
        assert_eq!(
            unsafe { host_sys_commands(sys_ctx, &mut commands) },
            FfiResult::OK
        );

        // 1. Verify column sizes via `host_iter_column_info` (`C-13`)
        let mut info0 = MaybeUninit::<BamColumnInfo>::uninit();
        let mut info1 = MaybeUninit::<BamColumnInfo>::uninit();
        let mut info2 = MaybeUninit::<BamColumnInfo>::uninit();
        unsafe {
            assert_eq!(
                host_iter_column_info(iter, 0, info0.as_mut_ptr()),
                FfiResult::OK
            );
            assert_eq!(
                host_iter_column_info(iter, 1, info1.as_mut_ptr()),
                FfiResult::OK
            );
            assert_eq!(
                host_iter_column_info(iter, 2, info2.as_mut_ptr()),
                FfiResult::OK
            );
            assert_eq!(info0.assume_init().size, core::mem::size_of::<Position>());
            assert_eq!(info1.assume_init().size, core::mem::size_of::<Velocity>());
            assert_eq!(info2.assume_init().size, core::mem::size_of::<NameTag>());
        }

        let mut total_processed = 0usize;
        let mut first_entity_to_despawn = FfiEntity::NULL;

        let mut count = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            let entities_ptr = unsafe { host_iter_entities(iter) };
            let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut Position };
            let vel_ptr = unsafe { host_iter_column_readonly(iter, 1) as *const Velocity };
            let tag_ptr = unsafe { host_iter_column_readonly(iter, 2) as *const NameTag };

            if pos_ptr.is_null() || vel_ptr.is_null() || tag_ptr.is_null() || entities_ptr.is_null()
            {
                return FfiResult::NULL_POINTER;
            }

            if first_entity_to_despawn.is_null() {
                first_entity_to_despawn = unsafe { *entities_ptr };
            }

            for i in 0..count {
                unsafe {
                    let p = &mut *pos_ptr.add(i);
                    let v = &*vel_ptr.add(i);
                    let tag = &*tag_ptr.add(i);
                    assert!(tag.label.starts_with("entity_"));
                    p.x += v.x;
                    p.y += v.y;
                    p.z += v.z;
                }
            }

            total_processed += count;
        }

        // 2. Queue deferred operations using pre-resolved `BamComponent` handle (`C-05`)
        let mut spawned_entity = FfiEntity::NULL;
        assert_eq!(
            unsafe { host_cmd_spawn_empty(commands, &mut spawned_entity) },
            FfiResult::OK
        );
        assert!(!spawned_entity.is_null());

        let new_pos = Position {
            x: 777.0,
            y: 888.0,
            z: 999.0,
        };
        let insert_res = unsafe {
            host_cmd_insert(
                commands,
                spawned_entity,
                data.pos_comp,
                (&new_pos as *const Position).cast::<u8>(),
                core::mem::size_of::<Position>(),
            )
        };
        assert_eq!(insert_res, FfiResult::OK);

        if !first_entity_to_despawn.is_null() {
            let despawn_res = unsafe { host_cmd_despawn(commands, first_entity_to_despawn) };
            assert_eq!(despawn_res, FfiResult::OK);
        }

        data.total_processed = total_processed;
        data.first_despawned = first_entity_to_despawn;
        data.spawned_entity = spawned_entity;

        FfiResult::OK
    }

    #[test]
    fn test_host_runtime_10k_entities_soa_and_deferred_commands() {
        const ENTITY_COUNT: usize = 10_000;

        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.init_resource::<ChangedPositionCounter>();
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let vel_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();
        let tag_h = app
            .register_mod_component::<NameTag>(ComponentOptions::new())
            .unwrap();

        let initial_entity_count = app.world_mut().query::<Entity>().iter(app.world()).count();

        for i in 0..ENTITY_COUNT {
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
                NameTag {
                    label: format!("entity_{i}"),
                },
            ));
        }

        // Flush initial spawn ticks so `count_changed_positions` measures strictly the guest system's mutation
        app.update();

        let registry = app.world().resource::<ModRegistry>().clone();
        let query_specs = vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![
                BamTermDesc::write(pos_h),
                BamTermDesc::read(vel_h),
                BamTermDesc::read(tag_h),
            ],
        }];

        let mut user_data = Box::new(Runtime10kUserData {
            pos_comp: pos_h,
            total_processed: 0,
            first_despawned: FfiEntity::NULL,
            spawned_entity: FfiEntity::NULL,
        });

        let guest_sys = GuestSystem::new(
            "gate_2_mutator",
            query_specs,
            host_runtime_10k_guest_system,
            (&mut *user_data as *mut Runtime10kUserData).cast::<c_void>(),
            registry,
        );

        app.add_systems(Update, (guest_sys, count_changed_positions).chain());
        app.update();

        assert_eq!(user_data.total_processed, ENTITY_COUNT);
        assert_eq!(
            app.world().resource::<ChangedPositionCounter>().0,
            ENTITY_COUNT
        );

        let living_entity_count = app.world_mut().query::<Entity>().iter(app.world()).count();
        assert_eq!(living_entity_count, initial_entity_count + ENTITY_COUNT);

        let position_entity_count = app
            .world_mut()
            .query::<&Position>()
            .iter(app.world())
            .count();
        assert_eq!(position_entity_count, ENTITY_COUNT);

        assert!(!user_data.first_despawned.is_null());
        let despawned_bevy = Entity::from_bits(user_data.first_despawned.to_bits());
        assert!(app.world().get_entity(despawned_bevy).is_err());

        assert!(!user_data.spawned_entity.is_null());
        let spawned_bevy = Entity::from_bits(user_data.spawned_entity.to_bits());
        assert_eq!(
            app.world().get::<Position>(spawned_bevy),
            Some(&Position {
                x: 777.0,
                y: 888.0,
                z: 999.0,
            })
        );
    }
}
