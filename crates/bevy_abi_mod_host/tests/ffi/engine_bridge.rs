//! Host `ModRegistry`, `AbiModPlugin`, `DynamicCSystem`, `CommandQueueBridge`, and 10k-entity SoA traversal.

mod registry_and_command_bridge {
    #![allow(unused_imports, dead_code)]
    use bevy_abi_mod_host::{AbiModPlugin, ModAppExt, ModRegistry};
    use bevy_abi_mod_host_sys::{
        CommandQueueBridge, DynamicCSystem, HOST_API, HostChunkIter, QueryColumnDesc,
        host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty, host_get_proc_address,
        host_iter_column_mutable, host_iter_column_readonly, host_iter_column_stride,
        host_iter_entities, host_iter_mark_changed, host_iter_next_chunk, host_register_component,
    };
    use bevy_abi_mod_macros::ModExposable;
    use bevy_abi_mod_sys::{
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
        assert_eq!(pos_meta.vtable_ptr.as_ptr(), Position::erased_vtable_ptr());

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
            let raw_queue2 =
                (&mut bridge2 as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();
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
        let mut bridge =
            CommandQueueBridge::new(&mut cmd_queue, cell.entity_allocator(), &registry);
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
            bevy_abi_mod_host_sys::ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            bevy_abi_mod_host_sys::ComponentAccessDescOwned {
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
            bevy_abi_mod_host_sys::ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            bevy_abi_mod_host_sys::ComponentAccessDescOwned {
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
        let query_descs = vec![bevy_abi_mod_host_sys::ComponentAccessDescOwned {
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

    unsafe fn call_register_component_fresh(desc: &ModComponentDescriptor) -> FfiResult {
        let mut world = World::new();
        let reg = Arc::new(ModRegistry::new());
        world.insert_resource((*reg).clone());
        let active = Arc::new(core::sync::atomic::AtomicBool::new(true));
        let mut init_ctx = bevy_abi_mod_host_sys::ModInitContext::with_world(
            reg,
            active,
            &mut world as *mut World,
        );
        let _guard = init_ctx.make_active();
        let ctx_ptr =
            (&mut init_ctx as *mut bevy_abi_mod_host_sys::ModInitContext).cast::<c_void>();
        unsafe { host_register_component(ctx_ptr, desc) }
    }

    #[test]
    fn test_host_register_component_defensive_validation() {
        let name = c"CustomRuntimeComponent".as_ptr();
        let uuid = ComponentUuid::from_name("CustomRuntimeComponent");

        // 1. Valid descriptor and host context
        let valid_desc = ModComponentDescriptor::new(name, uuid, 16, 8);
        let res = unsafe { call_register_component_fresh(&valid_desc) };
        assert_eq!(res, FfiResult::OK);

        // 2. Null host context
        let res = unsafe { host_register_component(core::ptr::null_mut(), &valid_desc) };
        assert_eq!(res, FfiResult::NULL_POINTER);

        // 3. Null descriptor pointer
        let mut world = World::new();
        let mut init_ctx = bevy_abi_mod_host_sys::ModInitContext::with_world(
            Arc::new(ModRegistry::new()),
            Arc::new(core::sync::atomic::AtomicBool::new(true)),
            &mut world as *mut World,
        );
        let _guard = init_ctx.make_active();
        let ctx_ptr =
            (&mut init_ctx as *mut bevy_abi_mod_host_sys::ModInitContext).cast::<c_void>();
        let res = unsafe { host_register_component(ctx_ptr, core::ptr::null()) };
        assert_eq!(res, FfiResult::NULL_POINTER);
        drop(_guard);

        // 4. Truncated descriptor struct_size
        let mut truncated_desc = valid_desc;
        truncated_desc.struct_size = 32;
        let res = unsafe { call_register_component_fresh(&truncated_desc) };
        assert_eq!(res, FfiResult::INVALID_VERSION);

        // 5. Forward-compatible descriptor struct_size (>= 64)
        let mut extended_desc = valid_desc;
        extended_desc.struct_size = 128;
        let res = unsafe { call_register_component_fresh(&extended_desc) };
        assert_eq!(res, FfiResult::OK);

        // 6. Null component name
        let null_name_desc = ModComponentDescriptor::new(core::ptr::null(), uuid, 16, 8);
        let res = unsafe { call_register_component_fresh(&null_name_desc) };
        assert_eq!(res, FfiResult::NULL_POINTER);

        // 7. Invalid UTF-8 in component name
        let invalid_utf8_bytes = [0xff, 0xfe, 0x00];
        let bad_utf8_desc = ModComponentDescriptor::new(
            invalid_utf8_bytes.as_ptr() as *const core::ffi::c_char,
            uuid,
            16,
            8,
        );
        let res = unsafe { call_register_component_fresh(&bad_utf8_desc) };
        assert_eq!(res, FfiResult::INVALID_UTF8);

        // 8. Invalid zero alignment
        let mut zero_align = valid_desc;
        zero_align.align = 0;
        let res = unsafe { call_register_component_fresh(&zero_align) };
        assert_eq!(res, FfiResult::INVALID_ARGUMENT);

        // 9. Invalid non-power-of-two alignment
        let mut non_pot_align = valid_desc;
        non_pot_align.align = 6;
        let res = unsafe { call_register_component_fresh(&non_pot_align) };
        assert_eq!(res, FfiResult::INVALID_ARGUMENT);

        // 10. Invalid misaligned size (size % align != 0)
        let mut misaligned = valid_desc;
        misaligned.size = 15;
        misaligned.align = 8;
        let res = unsafe { call_register_component_fresh(&misaligned) };
        assert_eq!(res, FfiResult::INVALID_ARGUMENT);

        // 11. Valid Zero-Sized Type (ZST)
        let zst_desc = ModComponentDescriptor::new(name, uuid, 0, 1);
        let res = unsafe { call_register_component_fresh(&zst_desc) };
        assert_eq!(res, FfiResult::OK);

        // 12. Valid over-aligned ZST
        let overaligned_zst = ModComponentDescriptor::new(name, uuid, 0, 16);
        let res = unsafe { call_register_component_fresh(&overaligned_zst) };
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
        let mut world = World::new();
        let reg = Arc::new(ModRegistry::new());
        world.insert_resource((*reg).clone());
        let mut init_ctx = bevy_abi_mod_host_sys::ModInitContext::with_world(
            reg,
            Arc::new(core::sync::atomic::AtomicBool::new(true)),
            &mut world as *mut World,
        );
        let _guard = init_ctx.make_active();
        let ctx_ptr =
            (&mut init_ctx as *mut bevy_abi_mod_host_sys::ModInitContext).cast::<c_void>();
        let name = c"HealthPoints".as_ptr();
        let uuid = ComponentUuid::from_name("HealthPoints");
        let desc = ModComponentDescriptor::new(name, uuid, 4, 4);

        let res = unsafe { (HOST_API.register_component)(ctx_ptr, &desc) };
        assert_eq!(res, FfiResult::OK);
    }
}

mod plugin_and_typed_commands {
    #![allow(unused_imports, dead_code)]
    use bevy_abi_mod_host::{AbiModPlugin, ModAppExt, ModHost, ModRegistry};
    use bevy_abi_mod_host_sys::{
        CommandQueueBridge, host_cmd_insert_component, host_cmd_spawn_empty,
    };
    use bevy_abi_mod_macros::ModExposable;
    use bevy_abi_mod_sys::FfiResult;
    use bevy_app::App;
    use bevy_ecs::component::Component;

    #[derive(Component, ModExposable, Copy, Clone, Debug, PartialEq)]
    #[repr(C)]
    struct Health {
        hp: f32,
        max_hp: f32,
    }

    #[derive(Component, ModExposable, Copy, Clone, Debug, PartialEq)]
    #[repr(C)]
    struct Velocity {
        vx: f32,
        vy: f32,
    }

    #[test]
    fn test_abi_mod_plugin_traits() {
        let plugin = AbiModPlugin::default();
        let debug_str = format!("{:?}", plugin);
        assert_eq!(debug_str, "AbiModPlugin");

        // Test Copy
        let plugin_copy = plugin;
        // Test Clone
        let plugin_clone = plugin.clone();

        let mut app = App::new();
        app.add_plugins(plugin_copy);
        assert!(app.world().get_resource::<ModRegistry>().is_some());
        assert!(app.world().get_resource::<ModHost>().is_some());

        // Adding plugin or running update works
        app.update();
        let _ = plugin_clone;
    }

    #[test]
    fn test_abi_mod_plugin_default_construction_and_app_lifecycle() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin::default());

        // Verify resources were inserted
        assert!(
            app.world().contains_resource::<ModRegistry>(),
            "ModRegistry must be initialized by AbiModPlugin::default()"
        );
        assert!(
            app.world().contains_resource::<ModHost>(),
            "ModHost must be initialized by AbiModPlugin::default()"
        );

        // Register mod components via ModAppExt
        app.register_mod_component::<Health>();
        app.register_mod_component::<Velocity>();

        {
            let registry = app.world().resource::<ModRegistry>();
            assert!(registry.get_by_name("Health").is_some());
            assert!(registry.get_by_name("Velocity").is_some());
            assert_eq!(
                registry.get_by_name("Health").unwrap().size,
                core::mem::size_of::<Health>()
            );
        }

        // Run multiple update cycles
        for _ in 0..100 {
            app.update();
        }
    }

    #[test]
    fn test_deferred_command_bridge_with_plugin_registered_types() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin::default());
        app.register_mod_component::<Health>();

        let mut queue = bevy_ecs::world::CommandQueue::default();
        let world = app.world_mut();

        let registry = world.resource::<ModRegistry>().clone();
        let cell = world.as_unsafe_world_cell();

        let mut bridge = CommandQueueBridge::new(&mut queue, cell.entity_allocator(), &registry);
        let raw_queue = &mut bridge as *mut _ as *mut bevy_abi_mod_sys::RawCommandQueue;

        // Spawn entity deferred
        let spawned_ffi = unsafe { host_cmd_spawn_empty(raw_queue) };
        assert_ne!(spawned_ffi.0, 0);

        // Insert Health component deferred
        let health = Health {
            hp: 75.0,
            max_hp: 100.0,
        };
        let insert_res = unsafe {
            host_cmd_insert_component(
                raw_queue,
                spawned_ffi,
                c"Health".as_ptr(),
                &health as *const Health as *const u8,
                core::mem::size_of::<Health>(),
            )
        };
        assert_eq!(insert_res, FfiResult::OK);

        // Flush commands to World
        queue.apply(world);

        // Verify entity was spawned and component initialized correctly
        let bevy_entity = bevy_ecs::entity::Entity::from_bits(spawned_ffi.to_bits());
        let world_health = world.get::<Health>(bevy_entity);
        assert!(world_health.is_some());
        assert_eq!(world_health.unwrap(), &health);
    }
}

mod runtime_10k_soa_pipeline {
    #![allow(unused_imports, dead_code)]
    use bevy_abi_mod_host::{AbiModPlugin, ModAppExt, ModRegistry};
    use bevy_abi_mod_host_sys::{
        DynamicCSystem, host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty,
        host_iter_column_mutable, host_iter_column_readonly, host_iter_column_stride,
        host_iter_entities, host_iter_next_chunk,
    };
    use bevy_abi_mod_macros::ModExposable;
    use bevy_abi_mod_sys::{
        ComponentAccessMode, FfiEntity, FfiResult, RawChunkIter, RawCommandQueue,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::prelude::{ResMut, Resource};
    use bevy_ecs::query::Changed;
    use bevy_ecs::schedule::IntoScheduleConfigs;
    use bevy_ecs::system::Query;
    use core::ffi::c_void;
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

    #[derive(Component, Debug, Clone, PartialEq, ModExposable)]
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
        total_processed: usize,
        first_despawned: FfiEntity,
        spawned_entity: FfiEntity,
    }

    /// Simulated guest mod C-ABI callback satisfying Validation Gate 2:
    /// - Verifies exact byte strides matching `core::mem::size_of::<T>()`.
    /// - Traverses table columns via SoA chunk iteration across 10,000 entities.
    /// - Mutates Position with Velocity byte strides, triggering Changed<Position>.
    /// - Enqueues deferred entity spawn, component insertion, and entity despawn.
    unsafe extern "C" fn host_runtime_10k_guest_system(
        iter: *mut RawChunkIter,
        commands: *mut RawCommandQueue,
        user_data: *mut c_void,
    ) -> FfiResult {
        // 1. Verify byte strides for each query column
        // SAFETY: iter is a valid RawChunkIter pointer passed by DynamicCSystem.
        let stride_pos = unsafe { host_iter_column_stride(iter, 0) };
        let stride_vel = unsafe { host_iter_column_stride(iter, 1) };
        let stride_tag = unsafe { host_iter_column_stride(iter, 2) };
        assert_eq!(stride_pos, core::mem::size_of::<Position>());
        assert_eq!(stride_vel, core::mem::size_of::<Velocity>());
        assert_eq!(stride_tag, core::mem::size_of::<NameTag>());

        let mut total_processed = 0usize;
        let mut first_entity_to_despawn = FfiEntity(0);

        loop {
            // SAFETY: host_iter_next_chunk advances chunk traversal.
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }

            // SAFETY: Obtaining column pointers and entities slice for current chunk.
            let entities_ptr = unsafe { host_iter_entities(iter) };
            let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut Position };
            let vel_ptr = unsafe { host_iter_column_readonly(iter, 1) as *const Velocity };
            let tag_ptr = unsafe { host_iter_column_readonly(iter, 2) as *const NameTag };

            if pos_ptr.is_null() || vel_ptr.is_null() || tag_ptr.is_null() || entities_ptr.is_null()
            {
                return FfiResult::NULL_POINTER;
            }

            if first_entity_to_despawn.0 == 0 {
                // Pick the first encountered entity to despawn via command queue
                first_entity_to_despawn = unsafe { *entities_ptr };
            }

            for i in 0..count {
                // SAFETY: In-bounds pointer arithmetic within active chunk of size `count`.
                unsafe {
                    let p = &mut *pos_ptr.add(i);
                    let v = &*vel_ptr.add(i);
                    let tag = &*tag_ptr.add(i);

                    // Verify NameTag string is intact and accessible
                    assert!(tag.label.starts_with("entity_"));

                    // Update position using velocity
                    p.x += v.x;
                    p.y += v.y;
                    p.z += v.z;
                }
            }

            total_processed += count;
        }

        // 2. Queue deferred operations:
        // A) Spawn a new entity
        // SAFETY: commands is a valid RawCommandQueue passed by DynamicCSystem.
        let spawned_entity = unsafe { host_cmd_spawn_empty(commands) };
        assert_ne!(spawned_entity.0, 0);

        // B) Insert Position component on the spawned entity
        let pos_comp_name = CString::new("Position").unwrap();
        let new_pos = Position {
            x: 777.0,
            y: 888.0,
            z: 999.0,
        };
        // SAFETY: Passing valid entity, component name, and buffer pointer.
        let insert_res = unsafe {
            host_cmd_insert_component(
                commands,
                spawned_entity,
                pos_comp_name.as_ptr(),
                &new_pos as *const Position as *const u8,
                core::mem::size_of::<Position>(),
            )
        };
        assert_eq!(insert_res, FfiResult::OK);

        // C) Despawn the chosen entity
        if first_entity_to_despawn.0 != 0 {
            // SAFETY: commands is a valid RawCommandQueue pointer.
            let despawn_res = unsafe { host_cmd_despawn(commands, first_entity_to_despawn) };
            assert_eq!(despawn_res, FfiResult::OK);
        }

        // Save tracking results into user_data
        if !user_data.is_null() {
            // SAFETY: user_data points to a valid Runtime10kUserData struct.
            unsafe {
                let data = &mut *(user_data as *mut Runtime10kUserData);
                data.total_processed = total_processed;
                data.first_despawned = first_entity_to_despawn;
                data.spawned_entity = spawned_entity;
            }
        }

        FfiResult::OK
    }

    #[test]
    fn test_host_runtime_10k_entities_soa_and_deferred_commands() {
        const ENTITY_COUNT: usize = 10_000;

        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.init_resource::<ChangedPositionCounter>();
        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();
        app.register_mod_component::<NameTag>();

        let initial_entity_count = app.world_mut().query::<Entity>().iter(app.world()).count();

        // Spawn 10,000 entities possessing (Position, Velocity, NameTag(String))
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
            bevy_abi_mod_host_sys::ComponentAccessDescOwned {
                name: "NameTag".to_string(),
                mode: ComponentAccessMode::READ,
            },
        ];

        let mut user_data = Runtime10kUserData {
            total_processed: 0,
            first_despawned: FfiEntity(0),
            spawned_entity: FfiEntity(0),
        };

        let dynamic_sys = DynamicCSystem::new(
            "gate_2_mutator".to_string(),
            query_descs,
            host_runtime_10k_guest_system,
            &mut user_data as *mut Runtime10kUserData as *mut c_void,
            Arc::new(registry),
        );

        // Dynamic system chained before native Bevy observer system
        app.add_systems(Update, (dynamic_sys, count_changed_positions).chain());

        // Execute 1 schedule tick
        app.update();

        // 1. Verify chunk iteration processed all 10,000 entities
        assert_eq!(
            user_data.total_processed, ENTITY_COUNT,
            "Chunk iterator must process all 10,000 entities"
        );

        // 2. Verify native Changed<Position> triggered for all 10,000 entities
        let changed_count = app.world().resource::<ChangedPositionCounter>().0;
        assert_eq!(
            changed_count, ENTITY_COUNT,
            "All 10,000 entities mutated via iter_column_mutable must trigger Changed<Position>"
        );

        // 3. Verify deferred commands flushed cleanly to Bevy World
        // Total entity count should equal initial_entity_count + 10,000 (- 1 despawned + 1 spawned)
        let living_entity_count = app.world_mut().query::<Entity>().iter(app.world()).count();
        assert_eq!(
            living_entity_count,
            initial_entity_count + ENTITY_COUNT,
            "Net entity count must remain initial + 10,000 after 1 spawn and 1 despawn"
        );

        // Net count of entities with Position must remain exactly 10,000
        let position_entity_count = app
            .world_mut()
            .query::<&Position>()
            .iter(app.world())
            .count();
        assert_eq!(
            position_entity_count, ENTITY_COUNT,
            "Net count of entities possessing Position must remain 10,000"
        );

        // Verify despawned entity no longer exists
        assert_ne!(user_data.first_despawned.0, 0);
        let despawned_bevy = Entity::from_bits(user_data.first_despawned.0);
        assert!(
            app.world().get_entity(despawned_bevy).is_err(),
            "Despawned entity must no longer exist in Bevy World"
        );

        // Verify spawned entity exists and has Position(777, 888, 999)
        assert_ne!(user_data.spawned_entity.0, 0);
        let spawned_bevy = Entity::from_bits(user_data.spawned_entity.0);
        let spawned_entity_ref = app
            .world()
            .get_entity(spawned_bevy)
            .expect("Spawned entity must exist in Bevy World");
        let spawned_pos = spawned_entity_ref
            .get::<Position>()
            .expect("Spawned entity must have Position component");
        assert_eq!(
            *spawned_pos,
            Position {
                x: 777.0,
                y: 888.0,
                z: 999.0,
            },
            "Spawned entity must have Position component inserted via CommandQueueBridge"
        );
    }
}
