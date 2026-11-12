//! Heterogeneous alignment, byte strides, ZSTs, dynamic component layouts, and allocator isolation.

mod heterogeneous_alignment_and_isolation {
    #![allow(unused_imports, dead_code)]
    use crate::support::*;
    use bevy_abi_mod_host::{AbiModPlugin, ModAppExt, ModRegistry};
    use bevy_abi_mod_host_sys::{
        ComponentAccessDescOwned, DynamicCSystem, host_iter_column_mutable,
        host_iter_column_readonly, host_iter_column_stride, host_iter_mark_changed,
        host_iter_next_chunk,
    };
    use bevy_abi_mod_macros::ModExposable;
    use bevy_abi_mod_sys::{
        ComponentAccessMode, FfiEntity, FfiResult, RawChunkIter, RawCommandQueue,
    };
    use bevy_app::App;
    use bevy_ecs::component::Component;
    use bevy_ecs::schedule::{LogLevel, Schedule, ScheduleBuildSettings};
    use bevy_ecs::system::{System, SystemAccess};
    use bevy_ecs::world::World;
    use libloading::{Library, Symbol};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ============================================================================
    // Alignment Verification Components
    // ============================================================================

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
    #[repr(C, align(1))]
    pub struct AlignByte {
        pub val: u8,
    }

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
    #[repr(C, align(4))]
    pub struct AlignWord {
        pub val: u32,
    }

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
    #[repr(C, align(8))]
    pub struct AlignDWord {
        pub val: u64,
    }

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
    #[repr(C, align(16))]
    pub struct AlignSimd {
        pub val: [f32; 4],
    }

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
    #[repr(C, align(64))]
    pub struct AlignCacheLine {
        pub val: [u64; 8],
    }

    static ALIGNMENT_VERIFIED_COUNT: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn alignment_verifier_callback(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut core::ffi::c_void,
    ) -> FfiResult {
        let alignments = [1, 4, 8, 16, 64];

        loop {
            // SAFETY: C-ABI iterator call within dynamic system execution
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }

            for (col_idx, &align) in alignments.iter().enumerate() {
                // SAFETY: Valid column indices 0..5
                let ptr = unsafe { host_iter_column_readonly(iter, col_idx) };
                let stride = unsafe { host_iter_column_stride(iter, col_idx) };

                assert!(!ptr.is_null(), "Column {col_idx} pointer must not be null");
                assert_eq!(
                    (ptr as usize) % align,
                    0,
                    "Base column pointer must satisfy {align}-byte alignment"
                );
                assert_eq!(
                    stride % align,
                    0,
                    "Stride {stride} must be multiple of alignment {align}"
                );

                for i in 0..count {
                    let elem_ptr = unsafe { ptr.add(i * stride) };
                    assert_eq!(
                        (elem_ptr as usize) % align,
                        0,
                        "Element {i} in column {col_idx} must satisfy {align}-byte alignment"
                    );
                }
            }
            ALIGNMENT_VERIFIED_COUNT.fetch_add(count, Ordering::SeqCst);
        }
        FfiResult::OK
    }

    /// Test 1: Alignment verification across 1, 4, 8, 16, 64 bytes.
    #[test]
    fn test_heterogeneous_memory_alignment() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<AlignByte>();
        app.register_mod_component::<AlignWord>();
        app.register_mod_component::<AlignDWord>();
        app.register_mod_component::<AlignSimd>();
        app.register_mod_component::<AlignCacheLine>();

        const COUNT: usize = 128;
        for i in 0..COUNT {
            app.world_mut().spawn((
                AlignByte {
                    val: (i % 255) as u8,
                },
                AlignWord { val: i as u32 },
                AlignDWord {
                    val: 0xDEADBEEFCAFE0000 + i as u64,
                },
                AlignSimd {
                    val: [1.0, 2.0, 3.0, i as f32],
                },
                AlignCacheLine { val: [i as u64; 8] },
            ));
        }

        let registry = app.world().resource::<ModRegistry>().clone();
        let query_descs = vec![
            ComponentAccessDescOwned {
                name: "AlignByte".to_string(),
                mode: ComponentAccessMode::READ,
            },
            ComponentAccessDescOwned {
                name: "AlignWord".to_string(),
                mode: ComponentAccessMode::READ,
            },
            ComponentAccessDescOwned {
                name: "AlignDWord".to_string(),
                mode: ComponentAccessMode::READ,
            },
            ComponentAccessDescOwned {
                name: "AlignSimd".to_string(),
                mode: ComponentAccessMode::READ,
            },
            ComponentAccessDescOwned {
                name: "AlignCacheLine".to_string(),
                mode: ComponentAccessMode::READ,
            },
        ];

        ALIGNMENT_VERIFIED_COUNT.store(0, Ordering::SeqCst);
        let sys = DynamicCSystem::new(
            "alignment_verifier".to_string(),
            query_descs,
            alignment_verifier_callback,
            core::ptr::null_mut(),
            Arc::new(registry),
        );

        app.add_systems(bevy_app::Update, sys);
        app.update();

        assert_eq!(
            ALIGNMENT_VERIFIED_COUNT.load(Ordering::SeqCst),
            COUNT,
            "All 128 entities must have their 1B, 4B, 8B, 16B, and 64B alignments verified"
        );
    }

    static BOUNDS_CALLBACK_RUNS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn bounds_checker_callback(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut core::ffi::c_void,
    ) -> FfiResult {
        // Next chunk on empty world returns 0 immediately
        unsafe {
            let c1 = host_iter_next_chunk(iter);
            assert_eq!(c1, 0, "Empty table must yield 0 entities");

            // Repeated next chunk calls past exhaustion must return 0 idempotently
            let c2 = host_iter_next_chunk(iter);
            assert_eq!(c2, 0, "Exhausted iterator must return 0");
            let c3 = host_iter_next_chunk(iter);
            assert_eq!(c3, 0, "Exhausted iterator must return 0");

            // Requesting columns on exhausted chunk returns null
            let col = host_iter_column_readonly(iter, 0);
            assert!(col.is_null(), "Exhausted column pointer must be null");

            let col_mut = host_iter_column_mutable(iter, 0);
            assert!(
                col_mut.is_null(),
                "Exhausted mutable column pointer must be null"
            );

            // Invalid row index mark changed returns error without panic
            let err = host_iter_mark_changed(iter, 0, 999);
            assert_eq!(err, FfiResult::OUT_OF_RANGE);
        }

        BOUNDS_CALLBACK_RUNS.fetch_add(1, Ordering::SeqCst);
        FfiResult::OK
    }

    /// Test 2: Bounds checking on empty tables (0 entities) and iterator exhaustion.
    #[test]
    fn test_chunk_iterator_bounds_and_empty_tables() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Position>();

        let registry = Arc::new(app.world().resource::<ModRegistry>().clone());
        let query_descs = vec![ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::WRITE,
        }];

        BOUNDS_CALLBACK_RUNS.store(0, Ordering::SeqCst);
        let sys = DynamicCSystem::new(
            "bounds_checker".to_string(),
            query_descs.clone(),
            bounds_checker_callback,
            core::ptr::null_mut(),
            Arc::clone(&registry),
        );

        app.add_systems(bevy_app::Update, sys);

        // 1. Completely empty world (0 entities)
        app.update();
        assert_eq!(BOUNDS_CALLBACK_RUNS.load(Ordering::SeqCst), 1);

        // 2. Spawn and immediately despawn -> creates an empty table with 0 entities in storages
        let e = app
            .world_mut()
            .spawn(Position {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            })
            .id();
        app.world_mut().despawn(e);

        app.update();
        assert_eq!(BOUNDS_CALLBACK_RUNS.load(Ordering::SeqCst), 2);
    }

    /// Test 5: Memory allocator boundary isolation.
    /// Ensures host-allocated strings in ECS components are managed exclusively by host allocator,
    /// and guest strings passed through VTable mutators are safely copied.
    #[test]
    fn test_memory_allocator_boundary_isolation() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<MetadataTag>();

        const ENTITIES: usize = 1_000;
        let mut entities = Vec::with_capacity(ENTITIES);

        for i in 0..ENTITIES {
            let e = app
                .world_mut()
                .spawn(MetadataTag {
                    label: format!("initial_host_string_index_{i}"),
                })
                .id();
            entities.push(e);
        }

        // Inspect VTable setter
        let registry = app.world().resource::<ModRegistry>();
        let tag_meta = registry.get_by_name("MetadataTag").expect("MetadataTag");
        let tag_vt = unsafe { &*(tag_meta.vtable_ptr.as_ptr() as *const MetadataTagVTable) };

        // Mutate across boundary using guest-provided buffer
        for (i, &e) in entities.iter().enumerate() {
            let tag_mut = app.world_mut().get_mut::<MetadataTag>(e).unwrap();
            let ptr = tag_mut.as_ref() as *const MetadataTag as *mut core::ffi::c_void;

            let guest_msg = format!("guest_payload_{i}");
            unsafe {
                (tag_vt.set_label)(ptr, guest_msg.as_ptr(), guest_msg.len());
            }
        }

        // Verify all host components hold updated strings
        for (i, &e) in entities.iter().enumerate() {
            let tag = app.world().get::<MetadataTag>(e).unwrap();
            assert_eq!(tag.label, format!("guest_payload_{i}"));
        }

        // Despawn all entities on host -> host drops strings with host allocator
        for e in entities {
            app.world_mut().despawn(e);
        }

        // Clean world tick to ensure zero memory corruption
        app.update();
    }
}

mod dynamic_component_lifecycle {
    #![allow(unused_imports, dead_code)]
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

    use bevy_abi_mod_host::plugin::AbiModPlugin;
    use bevy_abi_mod_host_sys::registry::ModRegistry;
    use bevy_abi_mod_host_sys::{
        CommandQueueBridge, ComponentAccessDescOwned, DynamicCSystem, ModInitContext,
        host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty,
        host_iter_column_mutable, host_iter_column_readonly, host_iter_mark_changed,
        host_iter_next_chunk, host_register_component,
    };
    use bevy_abi_mod_sys::{
        ComponentAccessMode, ComponentUuid, FfiEntity, FfiResult, ModComponentDescriptor,
        RawChunkIter, RawCommandQueue,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::entity::Entity;
    use bevy_ecs::system::{System, SystemAccess};
    use bevy_ecs::world::{CommandQueue, World};
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    // ============================================================================
    // Test 1: Dynamic Component Registration in World & ModRegistry
    // ============================================================================

    #[test]
    fn test_dynamic_component_registration() {
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
        assert!(
            world
                .components()
                .get_info(comp_meta.component_id)
                .is_some()
        );

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
    fn test_deferred_dynamic_component_insertion() {
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

        assert!(
            world
                .get_entity(Entity::from_bits(despawn_target.0))
                .is_err()
        );
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
    fn test_dynamic_system_chunk_iteration_and_mutation() {
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
    fn test_change_detection_tracking() {
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

        unsafe {
            host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
        };

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
}

mod column_bounds_and_zst_alignment {
    #![allow(unused_imports, dead_code)]
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

    use bevy_abi_mod_host::ModAppExt;
    use bevy_abi_mod_host::plugin::AbiModPlugin;
    use bevy_abi_mod_host_sys::registry::ModRegistry;
    use bevy_abi_mod_host_sys::{
        CommandQueueBridge, ComponentAccessDescOwned, DynamicCSystem, ModInitContext,
        host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty,
        host_iter_column_mutable, host_iter_column_readonly, host_iter_column_stride,
        host_iter_mark_changed, host_iter_next_chunk, host_register_component,
        reset_drop_trampolines_for_testing,
    };
    use bevy_abi_mod_macros::ModExposable;
    use bevy_abi_mod_sys::{
        ComponentAccessMode, ComponentUuid, FfiEntity, FfiResult, ModComponentDescriptor,
        RawChunkIter, RawCommandQueue,
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
    use std::sync::Arc;

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

            // Out-of-bounds row index in host_iter_mark_changed must return OUT_OF_RANGE
            assert_eq!(
                unsafe { host_iter_mark_changed(iter, 0, count + 100) },
                FfiResult::OUT_OF_RANGE
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
    fn test_column_and_row_boundary_enforcement() {
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
    // CHALLENGE 6: ZERO-SIZED (ZST) & HIGH-ALIGNMENT (64B / 128B) DYNAMIC COMPONENTS
    // ============================================================================

    #[repr(C, align(64))]
    #[derive(Copy, Clone, Debug, PartialEq)]
    struct CacheLineAlignedComp {
        data: [u64; 8], // 64 bytes, 64-byte aligned
    }

    #[test]
    fn test_zst_and_high_alignment_dynamic_components() {
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
    fn test_deferred_command_error_conditions() {
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
            FfiResult::INVALID_ARGUMENT,
            "Size mismatch on host_cmd_insert_component must return INVALID_ARGUMENT"
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
}

mod empirical_pointer_arithmetic_and_decoupling {
    #![allow(unused_imports, dead_code)]
    //! # Milestone 2 Empirical Adversarial Stress Suite
    //!
    //! Written by Challenger 2 for Milestone 2: Host Runtime Dynamic ECS Integration & Concurrency.
    //! Focus Areas:
    //! 1. Zero-Copy Pointer Arithmetic & Alignment (1B, 2B, 4B, 8B, 16B, 32B, 64B, and ZSTs).
    //! 2. Contiguous Table Column Verification & Entity ID Alignment across Multiple Archetype Chunks.
    //! 3. Native Bevy Change Detection Parity (Row-level vs Chunk Bulk Ticks).
    //! 4. Allocator Isolation & Foreign Memory Decoupling (Stack overwrite defense, queue discard leak test).
    //! 5. Drop Trampoline Lifecycle, Unload Safety, and Slot Reuse Challenge.

    use bevy_abi_mod_host::plugin::AbiModPlugin;
    use bevy_abi_mod_host_sys::registry::ModRegistry;
    use bevy_abi_mod_host_sys::{
        CommandQueueBridge, ComponentAccessDescOwned, DynamicCSystem, ModInitContext,
        host_cmd_insert_component, host_cmd_spawn_empty, host_iter_column_mutable,
        host_iter_column_readonly, host_iter_column_stride, host_iter_entities,
        host_iter_mark_changed, host_iter_next_chunk, host_iter_reset, host_register_component,
        reset_drop_trampolines_for_testing,
    };
    use bevy_abi_mod_sys::{
        ComponentAccessMode, ComponentUuid, FfiEntity, FfiResult, ModComponentDescriptor,
        RawChunkIter, RawCommandQueue,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::entity::Entity;
    use bevy_ecs::system::System;
    use bevy_ecs::world::{CommandQueue, World};
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // ============================================================================
    // Focus 1: Alignment and Size Matrix Stress Test
    // ============================================================================

    #[repr(C, align(1))]
    #[derive(Copy, Clone, Debug, PartialEq)]
    struct CompAlign1 {
        val: u8,
    }

    #[repr(C, align(2))]
    #[derive(Copy, Clone, Debug, PartialEq)]
    struct CompAlign2 {
        val: u16,
    }

    #[repr(C, align(4))]
    #[derive(Copy, Clone, Debug, PartialEq)]
    struct CompAlign4 {
        val: u32,
    }

    #[repr(C, align(8))]
    #[derive(Copy, Clone, Debug, PartialEq)]
    struct CompAlign8 {
        val: u64,
    }

    #[repr(C, align(16))]
    #[derive(Copy, Clone, Debug, PartialEq)]
    struct CompAlign16 {
        val: [u64; 2],
    }

    #[repr(C, align(32))]
    #[derive(Copy, Clone, Debug, PartialEq)]
    struct CompAlign32 {
        val: [u64; 4],
    }

    #[repr(C, align(64))]
    #[derive(Copy, Clone, Debug, PartialEq)]
    struct CompAlign64 {
        val: [u64; 8],
    }

    #[test]
    fn test_alignment_and_pointer_arithmetic_matrix() {
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

        // Register all alignments
        macro_rules! register_type {
            ($t:ty, $name:literal) => {{
                let name_cstr = concat!($name, "\0").as_bytes();
                let desc = ModComponentDescriptor::new(
                    name_cstr.as_ptr() as *const core::ffi::c_char,
                    ComponentUuid::from_name($name),
                    core::mem::size_of::<$t>(),
                    core::mem::align_of::<$t>(),
                );
                let res = unsafe {
                    host_register_component(
                        &mut init_ctx as *mut ModInitContext as *mut c_void,
                        &desc,
                    )
                };
                assert_eq!(res, FfiResult::OK, "Failed to register {}", $name);
            }};
        }

        register_type!(CompAlign1, "CompAlign1");
        register_type!(CompAlign2, "CompAlign2");
        register_type!(CompAlign4, "CompAlign4");
        register_type!(CompAlign8, "CompAlign8");
        register_type!(CompAlign16, "CompAlign16");
        register_type!(CompAlign32, "CompAlign32");
        register_type!(CompAlign64, "CompAlign64");

        // Spawn 16 entities with all these components
        let mut command_queue = CommandQueue::default();
        let world_reg = world.resource::<ModRegistry>().clone();
        let mut bridge =
            CommandQueueBridge::new(&mut command_queue, world.entity_allocator(), &world_reg);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        const ENTITY_COUNT: usize = 16;
        let mut entities = Vec::new();

        for i in 0..ENTITY_COUNT {
            let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
            entities.push(ffi_e);

            let c1 = CompAlign1 { val: i as u8 };
            let c2 = CompAlign2 { val: i as u16 * 10 };
            let c4 = CompAlign4 {
                val: i as u32 * 100,
            };
            let c8 = CompAlign8 {
                val: i as u64 * 1000,
            };
            let c16 = CompAlign16 {
                val: [i as u64, i as u64 * 2],
            };
            let c32 = CompAlign32 { val: [i as u64; 4] };
            let c64 = CompAlign64 { val: [i as u64; 8] };

            macro_rules! insert_val {
                ($c:ident, $t:ty, $name:literal) => {{
                    let name_cstr = concat!($name, "\0").as_bytes();
                    let res = unsafe {
                        host_cmd_insert_component(
                            raw_queue,
                            ffi_e,
                            name_cstr.as_ptr() as *const core::ffi::c_char,
                            &$c as *const $t as *const u8,
                            core::mem::size_of::<$t>(),
                        )
                    };
                    assert_eq!(res, FfiResult::OK, "Failed to insert {}", $name);
                }};
            }

            insert_val!(c1, CompAlign1, "CompAlign1");
            insert_val!(c2, CompAlign2, "CompAlign2");
            insert_val!(c4, CompAlign4, "CompAlign4");
            insert_val!(c8, CompAlign8, "CompAlign8");
            insert_val!(c16, CompAlign16, "CompAlign16");
            insert_val!(c32, CompAlign32, "CompAlign32");
            insert_val!(c64, CompAlign64, "CompAlign64");
        }

        command_queue.apply(&mut world);

        // Verify all components in Bevy World are strictly aligned and equal to inserted data
        let reg = world.resource::<ModRegistry>();
        for (i, ffi_e) in entities.iter().enumerate().take(ENTITY_COUNT) {
            let bevy_e = Entity::from_bits(ffi_e.0);
            let e_ref = world.entity(bevy_e);

            macro_rules! check_val {
                ($t:ty, $name:literal, $expected:expr) => {{
                    let comp_id = reg.get_by_name($name).unwrap().component_id;
                    let ptr = e_ref.get_by_id(comp_id).expect($name).as_ptr();
                    assert_eq!(
                        ptr as usize % core::mem::align_of::<$t>(),
                        0,
                        "Pointer for {} is misaligned: {:p} (expected align {})",
                        $name,
                        ptr,
                        core::mem::align_of::<$t>()
                    );
                    let read_val = unsafe { *(ptr as *const $t) };
                    assert_eq!(read_val, $expected, "Value mismatch for {}", $name);
                }};
            }

            check_val!(CompAlign1, "CompAlign1", CompAlign1 { val: i as u8 });
            check_val!(CompAlign2, "CompAlign2", CompAlign2 { val: i as u16 * 10 });
            check_val!(
                CompAlign4,
                "CompAlign4",
                CompAlign4 {
                    val: i as u32 * 100
                }
            );
            check_val!(
                CompAlign8,
                "CompAlign8",
                CompAlign8 {
                    val: i as u64 * 1000
                }
            );
            check_val!(
                CompAlign16,
                "CompAlign16",
                CompAlign16 {
                    val: [i as u64, i as u64 * 2]
                }
            );
            check_val!(
                CompAlign32,
                "CompAlign32",
                CompAlign32 { val: [i as u64; 4] }
            );
            check_val!(
                CompAlign64,
                "CompAlign64",
                CompAlign64 { val: [i as u64; 8] }
            );
        }
    }

    // ============================================================================
    // Focus 2: Dynamic System Chunk Iteration Pointer Arithmetic & Reset
    // ============================================================================

    static ITER_VERIFIED_COUNT: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn dynamic_chunk_pointer_arithmetic_verifier(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count > 0 {
            // Columns: 0: CompAlign1, 1: CompAlign16, 2: CompAlign64
            let p1 = unsafe { host_iter_column_mutable(iter, 0) };
            let p16 = unsafe { host_iter_column_mutable(iter, 1) };
            let p64 = unsafe { host_iter_column_mutable(iter, 2) };

            assert!(!p1.is_null());
            assert!(!p16.is_null());
            assert!(!p64.is_null());

            let stride1 = unsafe { host_iter_column_stride(iter, 0) };
            let stride16 = unsafe { host_iter_column_stride(iter, 1) };
            let stride64 = unsafe { host_iter_column_stride(iter, 2) };

            assert_eq!(stride1, core::mem::size_of::<CompAlign1>());
            assert_eq!(stride16, core::mem::size_of::<CompAlign16>());
            assert_eq!(stride64, core::mem::size_of::<CompAlign64>());

            let entities_ptr = unsafe { host_iter_entities(iter) };
            assert!(!entities_ptr.is_null());

            for row in 0..count {
                // Check natural alignment for each row
                let ptr1_row = unsafe { p1.add(row * stride1) };
                let ptr16_row = unsafe { p16.add(row * stride16) };
                let ptr64_row = unsafe { p64.add(row * stride64) };

                assert_eq!(ptr1_row as usize % core::mem::align_of::<CompAlign1>(), 0);
                assert_eq!(ptr16_row as usize % core::mem::align_of::<CompAlign16>(), 0);
                assert_eq!(ptr64_row as usize % core::mem::align_of::<CompAlign64>(), 0);

                // Mutate each component via raw pointer
                unsafe {
                    let c1 = &mut *(ptr1_row as *mut CompAlign1);
                    c1.val = c1.val.wrapping_add(1);

                    let c16 = &mut *(ptr16_row as *mut CompAlign16);
                    c16.val[0] += 500;

                    let c64 = &mut *(ptr64_row as *mut CompAlign64);
                    c64.val[0] += 1000;
                }

                ITER_VERIFIED_COUNT.fetch_add(1, Ordering::Relaxed);
            }

            // Test iterator reset capability: reset and verify values reflect the mutation
            let reset_res = unsafe { host_iter_reset(iter) };
            assert_eq!(reset_res, FfiResult::OK);

            let re_count = unsafe { host_iter_next_chunk(iter) };
            assert_eq!(re_count, count);

            let re_p1 = unsafe { host_iter_column_readonly(iter, 0) };
            assert_eq!(re_p1, p1 as *const u8);
        }

        FfiResult::OK
    }

    #[test]
    fn test_chunk_pointer_arithmetic_and_reset() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        let is_active = Arc::new(AtomicBool::new(true));
        let mut init_ctx = ModInitContext::with_world(
            Arc::new(app.world().resource::<ModRegistry>().clone()),
            Arc::clone(&is_active),
            app.world_mut() as *mut World,
        );
        let _guard = init_ctx.make_active();

        // Register CompAlign1, CompAlign16, CompAlign64
        macro_rules! register_type {
            ($t:ty, $name:literal) => {{
                let name_cstr = concat!($name, "\0").as_bytes();
                let desc = ModComponentDescriptor::new(
                    name_cstr.as_ptr() as *const core::ffi::c_char,
                    ComponentUuid::from_name($name),
                    core::mem::size_of::<$t>(),
                    core::mem::align_of::<$t>(),
                );
                unsafe {
                    host_register_component(
                        &mut init_ctx as *mut ModInitContext as *mut c_void,
                        &desc,
                    )
                };
            }};
        }
        register_type!(CompAlign1, "ChunkIterComp1");
        register_type!(CompAlign16, "ChunkIterComp16");
        register_type!(CompAlign64, "ChunkIterComp64");

        // Spawn 8 entities
        let mut command_queue = CommandQueue::default();
        let world_reg = app.world().resource::<ModRegistry>().clone();
        let mut bridge = CommandQueueBridge::new(
            &mut command_queue,
            app.world().entity_allocator(),
            &world_reg,
        );
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        let mut entities = Vec::new();
        for i in 0..8 {
            let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
            entities.push(ffi_e);

            let c1 = CompAlign1 { val: i as u8 };
            let c16 = CompAlign16 {
                val: [i as u64, i as u64 * 2],
            };
            let c64 = CompAlign64 { val: [i as u64; 8] };

            macro_rules! insert_val {
                ($c:ident, $t:ty, $name:literal) => {{
                    let name_cstr = concat!($name, "\0").as_bytes();
                    unsafe {
                        host_cmd_insert_component(
                            raw_queue,
                            ffi_e,
                            name_cstr.as_ptr() as *const core::ffi::c_char,
                            &$c as *const $t as *const u8,
                            core::mem::size_of::<$t>(),
                        );
                    }
                }};
            }
            insert_val!(c1, CompAlign1, "ChunkIterComp1");
            insert_val!(c16, CompAlign16, "ChunkIterComp16");
            insert_val!(c64, CompAlign64, "ChunkIterComp64");
        }
        command_queue.apply(app.world_mut());

        let sys = DynamicCSystem::new(
            "pointer_arith_verifier".to_string(),
            vec![
                ComponentAccessDescOwned {
                    name: "ChunkIterComp1".to_string(),
                    mode: ComponentAccessMode::WRITE,
                },
                ComponentAccessDescOwned {
                    name: "ChunkIterComp16".to_string(),
                    mode: ComponentAccessMode::WRITE,
                },
                ComponentAccessDescOwned {
                    name: "ChunkIterComp64".to_string(),
                    mode: ComponentAccessMode::WRITE,
                },
            ],
            dynamic_chunk_pointer_arithmetic_verifier,
            core::ptr::null_mut(),
            Arc::new(world_reg),
        );
        app.add_systems(Update, sys);

        ITER_VERIFIED_COUNT.store(0, Ordering::SeqCst);
        app.update();

        assert_eq!(ITER_VERIFIED_COUNT.load(Ordering::SeqCst), 8);

        // Verify mutated values in Bevy World
        let reg = app.world().resource::<ModRegistry>();
        let id1 = reg.get_by_name("ChunkIterComp1").unwrap().component_id;
        let id16 = reg.get_by_name("ChunkIterComp16").unwrap().component_id;
        let id64 = reg.get_by_name("ChunkIterComp64").unwrap().component_id;

        for (i, &ffi_e) in entities.iter().enumerate() {
            let bevy_e = Entity::from_bits(ffi_e.0);
            let e_ref = app.world().entity(bevy_e);

            let v1 = unsafe { *(e_ref.get_by_id(id1).unwrap().as_ptr() as *const CompAlign1) };
            let v16 = unsafe { *(e_ref.get_by_id(id16).unwrap().as_ptr() as *const CompAlign16) };
            let v64 = unsafe { *(e_ref.get_by_id(id64).unwrap().as_ptr() as *const CompAlign64) };

            assert_eq!(v1.val, (i as u8).wrapping_add(1));
            assert_eq!(v16.val[0], (i as u64) + 500);
            assert_eq!(v64.val[0], (i as u64) + 1000);
        }
    }

    // ============================================================================
    // Focus 3: Zero-Sized Types (ZST) Stress Test
    // ============================================================================

    static ZST_CHUNK_COUNT: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn zst_system_callback(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        loop {
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }

            // Col 0: ZST align 1, Col 1: ZST align 16, Col 2: CompAlign4
            let p_zst1 = unsafe { host_iter_column_readonly(iter, 0) };
            let p_zst16 = unsafe { host_iter_column_mutable(iter, 1) };
            let p_comp4 = unsafe { host_iter_column_mutable(iter, 2) };

            assert!(!p_zst1.is_null());
            assert!(!p_zst16.is_null());
            assert!(!p_comp4.is_null());

            let stride_zst1 = unsafe { host_iter_column_stride(iter, 0) };
            let stride_zst16 = unsafe { host_iter_column_stride(iter, 1) };
            let stride_comp4 = unsafe { host_iter_column_stride(iter, 2) };

            assert_eq!(stride_zst1, 0, "ZST stride must be 0");
            assert_eq!(stride_zst16, 0, "ZST stride must be 0");
            assert_eq!(stride_comp4, 4);

            ZST_CHUNK_COUNT.fetch_add(count, Ordering::Relaxed);
        }
        FfiResult::OK
    }

    #[test]
    fn test_zst_components_lifecycle_and_iteration() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        let is_active = Arc::new(AtomicBool::new(true));
        let mut init_ctx = ModInitContext::with_world(
            Arc::new(app.world().resource::<ModRegistry>().clone()),
            Arc::clone(&is_active),
            app.world_mut() as *mut World,
        );
        let _guard = init_ctx.make_active();

        // Register ZSTs: size 0, align 1 and size 0, align 16
        let desc_zst1 = ModComponentDescriptor::new(
            c"ZstAlign1".as_ptr(),
            ComponentUuid::from_name("ZstAlign1"),
            0,
            1,
        );
        let desc_zst16 = ModComponentDescriptor::new(
            c"ZstAlign16".as_ptr(),
            ComponentUuid::from_name("ZstAlign16"),
            0,
            16,
        );
        let desc_c4 = ModComponentDescriptor::new(
            c"PayloadC4".as_ptr(),
            ComponentUuid::from_name("PayloadC4"),
            4,
            4,
        );

        unsafe {
            assert_eq!(
                host_register_component(
                    &mut init_ctx as *mut ModInitContext as *mut c_void,
                    &desc_zst1
                ),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(
                    &mut init_ctx as *mut ModInitContext as *mut c_void,
                    &desc_zst16
                ),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(
                    &mut init_ctx as *mut ModInitContext as *mut c_void,
                    &desc_c4
                ),
                FfiResult::OK
            );
        }

        // Spawn 5 entities possessing the ZST components and payload
        let mut command_queue = CommandQueue::default();
        let world_reg = app.world().resource::<ModRegistry>().clone();
        let mut bridge = CommandQueueBridge::new(
            &mut command_queue,
            app.world().entity_allocator(),
            &world_reg,
        );
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        for i in 0..5 {
            let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };

            // For ZST, data can be non-null dangling pointer or null with len=0
            let dummy_byte = 0u8;
            let res1 = unsafe {
                host_cmd_insert_component(raw_queue, ffi_e, c"ZstAlign1".as_ptr(), &dummy_byte, 0)
            };
            assert_eq!(res1, FfiResult::OK);

            let res16 = unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    ffi_e,
                    c"ZstAlign16".as_ptr(),
                    core::ptr::null(),
                    0,
                )
            };
            assert_eq!(res16, FfiResult::OK);

            let payload = i as u32 * 7;
            let resc4 = unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    ffi_e,
                    c"PayloadC4".as_ptr(),
                    &payload as *const u32 as *const u8,
                    4,
                )
            };
            assert_eq!(resc4, FfiResult::OK);
        }
        command_queue.apply(app.world_mut());

        // Register system querying both ZSTs and Payload
        let sys = DynamicCSystem::new(
            "zst_system".to_string(),
            vec![
                ComponentAccessDescOwned {
                    name: "ZstAlign1".to_string(),
                    mode: ComponentAccessMode::READ,
                },
                ComponentAccessDescOwned {
                    name: "ZstAlign16".to_string(),
                    mode: ComponentAccessMode::WRITE,
                },
                ComponentAccessDescOwned {
                    name: "PayloadC4".to_string(),
                    mode: ComponentAccessMode::WRITE,
                },
            ],
            zst_system_callback,
            core::ptr::null_mut(),
            Arc::new(world_reg),
        );
        app.add_systems(Update, sys);

        ZST_CHUNK_COUNT.store(0, Ordering::SeqCst);
        app.update();

        assert_eq!(ZST_CHUNK_COUNT.load(Ordering::SeqCst), 5);
    }

    // ============================================================================
    // Focus 4: Multi-Archetype / Multi-Chunk Contiguity Verification
    // ============================================================================

    static MULTI_CHUNK_ENTITY_RECORDS: Mutex<Vec<(u64, u32)>> = Mutex::new(Vec::new());

    unsafe extern "C" fn multi_chunk_collector_system(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        loop {
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }

            let p_shared = unsafe { host_iter_column_readonly(iter, 0) };
            assert!(!p_shared.is_null());
            let stride = unsafe { host_iter_column_stride(iter, 0) };
            assert_eq!(stride, 4);

            let entities_ptr = unsafe { host_iter_entities(iter) };
            assert!(!entities_ptr.is_null());

            for row in 0..count {
                let entity_id = unsafe { (*entities_ptr.add(row)).0 };
                let val = unsafe { *(p_shared.add(row * stride) as *const u32) };
                MULTI_CHUNK_ENTITY_RECORDS
                    .lock()
                    .unwrap()
                    .push((entity_id, val));
            }
        }
        FfiResult::OK
    }

    #[test]
    fn test_multi_archetype_chunks_contiguous_traversal() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        let is_active = Arc::new(AtomicBool::new(true));
        let mut init_ctx = ModInitContext::with_world(
            Arc::new(app.world().resource::<ModRegistry>().clone()),
            Arc::clone(&is_active),
            app.world_mut() as *mut World,
        );
        let _guard = init_ctx.make_active();

        // Register SharedComp, MarkerA, MarkerB, MarkerC
        let desc_shared = ModComponentDescriptor::new(
            c"SharedComp".as_ptr(),
            ComponentUuid::from_name("SharedComp"),
            4,
            4,
        );
        let desc_ma = ModComponentDescriptor::new(
            c"MarkerA".as_ptr(),
            ComponentUuid::from_name("MarkerA"),
            4,
            4,
        );
        let desc_mb = ModComponentDescriptor::new(
            c"MarkerB".as_ptr(),
            ComponentUuid::from_name("MarkerB"),
            4,
            4,
        );
        let desc_mc = ModComponentDescriptor::new(
            c"MarkerC".as_ptr(),
            ComponentUuid::from_name("MarkerC"),
            4,
            4,
        );

        unsafe {
            host_register_component(
                &mut init_ctx as *mut ModInitContext as *mut c_void,
                &desc_shared,
            );
            host_register_component(
                &mut init_ctx as *mut ModInitContext as *mut c_void,
                &desc_ma,
            );
            host_register_component(
                &mut init_ctx as *mut ModInitContext as *mut c_void,
                &desc_mb,
            );
            host_register_component(
                &mut init_ctx as *mut ModInitContext as *mut c_void,
                &desc_mc,
            );
        }

        // Spawn 3 groups of entities into 3 different tables:
        // Group 1 (3 entities): [SharedComp, MarkerA]
        // Group 2 (4 entities): [SharedComp, MarkerB]
        // Group 3 (5 entities): [SharedComp, MarkerC]
        let mut command_queue = CommandQueue::default();
        let world_reg = app.world().resource::<ModRegistry>().clone();
        let mut bridge = CommandQueueBridge::new(
            &mut command_queue,
            app.world().entity_allocator(),
            &world_reg,
        );
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        let mut expected_map = std::collections::HashMap::new();

        // Group 1
        for i in 0..3 {
            let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
            let val: u32 = 100 + i;
            expected_map.insert(ffi_e.0, val);
            unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    ffi_e,
                    c"SharedComp".as_ptr(),
                    &val as *const u32 as *const u8,
                    4,
                );
                host_cmd_insert_component(
                    raw_queue,
                    ffi_e,
                    c"MarkerA".as_ptr(),
                    &val as *const u32 as *const u8,
                    4,
                );
            }
        }

        // Group 2
        for i in 0..4 {
            let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
            let val: u32 = 200 + i;
            expected_map.insert(ffi_e.0, val);
            unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    ffi_e,
                    c"SharedComp".as_ptr(),
                    &val as *const u32 as *const u8,
                    4,
                );
                host_cmd_insert_component(
                    raw_queue,
                    ffi_e,
                    c"MarkerB".as_ptr(),
                    &val as *const u32 as *const u8,
                    4,
                );
            }
        }

        // Group 3
        for i in 0..5 {
            let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
            let val: u32 = 300 + i;
            expected_map.insert(ffi_e.0, val);
            unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    ffi_e,
                    c"SharedComp".as_ptr(),
                    &val as *const u32 as *const u8,
                    4,
                );
                host_cmd_insert_component(
                    raw_queue,
                    ffi_e,
                    c"MarkerC".as_ptr(),
                    &val as *const u32 as *const u8,
                    4,
                );
            }
        }
        command_queue.apply(app.world_mut());

        // Register DynamicCSystem querying SharedComp (will visit 3 separate archetype tables)
        let sys = DynamicCSystem::new(
            "multi_chunk_collector".to_string(),
            vec![ComponentAccessDescOwned {
                name: "SharedComp".to_string(),
                mode: ComponentAccessMode::READ,
            }],
            multi_chunk_collector_system,
            core::ptr::null_mut(),
            Arc::new(world_reg),
        );
        app.add_systems(Update, sys);

        MULTI_CHUNK_ENTITY_RECORDS.lock().unwrap().clear();
        app.update();

        let records = MULTI_CHUNK_ENTITY_RECORDS.lock().unwrap().clone();
        assert_eq!(
            records.len(),
            12,
            "Must visit all 12 entities across 3 tables"
        );

        for (e_id, val) in records {
            let expected = expected_map
                .get(&e_id)
                .expect("Entity must be in expected map");
            assert_eq!(val, *expected, "Entity value mismatch in chunk traversal");
        }
    }

    // ============================================================================
    // Focus 5: Native Bevy Change Detection Parity (Row Ticks vs Bulk Ticks)
    // ============================================================================

    unsafe extern "C" fn fine_grained_change_marker_system(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        loop {
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }

            // Use readonly pointer (does NOT touch change ticks)
            let _ptr = unsafe { host_iter_column_readonly(iter, 0) };

            // Mark only ROW 1 as changed
            for row in 0..count {
                if row == 1 {
                    let res = unsafe { host_iter_mark_changed(iter, 0, row) };
                    assert_eq!(res, FfiResult::OK);
                }
            }
        }
        FfiResult::OK
    }

    #[test]
    fn test_native_bevy_change_detection_parity() {
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
            c"TickTrackComp".as_ptr(),
            ComponentUuid::from_name("TickTrackComp"),
            4,
            4,
        );
        unsafe {
            host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
        };

        let mut command_queue = CommandQueue::default();
        let world_reg = world.resource::<ModRegistry>().clone();
        let mut bridge =
            CommandQueueBridge::new(&mut command_queue, world.entity_allocator(), &world_reg);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        let mut entities = Vec::new();
        for i in 0..4 {
            let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
            let val = i as u32;
            unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    ffi_e,
                    c"TickTrackComp".as_ptr(),
                    &val as *const u32 as *const u8,
                    4,
                );
            }
            entities.push(Entity::from_bits(ffi_e.0));
        }
        command_queue.apply(&mut world);

        let comp_id = world
            .resource::<ModRegistry>()
            .get_by_name("TickTrackComp")
            .unwrap()
            .component_id;

        // Advance world change tick to simulate frame 1
        let frame1_tick = world.change_tick();

        // Create dynamic system that only marks row 1 as changed
        let mut sys = DynamicCSystem::new(
            "tick_marker".to_string(),
            vec![ComponentAccessDescOwned {
                name: "TickTrackComp".to_string(),
                mode: ComponentAccessMode::WRITE,
            }],
            fine_grained_change_marker_system,
            core::ptr::null_mut(),
            Arc::new(world_reg),
        );
        sys.initialize(&mut world);

        // Advance world change tick for frame 2
        world.increment_change_tick();
        let frame2_tick = world.change_tick();
        unsafe {
            sys.run_unsafe((), world.as_unsafe_world_cell()).unwrap();
        }

        // Inspect change ticks directly via Bevy ECS entity component ticks accessor!
        let ticks_e0 = world
            .entity(entities[0])
            .get_change_ticks_by_id(comp_id)
            .unwrap();
        let ticks_e1 = world
            .entity(entities[1])
            .get_change_ticks_by_id(comp_id)
            .unwrap();
        let ticks_e2 = world
            .entity(entities[2])
            .get_change_ticks_by_id(comp_id)
            .unwrap();
        let ticks_e3 = world
            .entity(entities[3])
            .get_change_ticks_by_id(comp_id)
            .unwrap();

        // Row 1 (entities[1]) MUST have changed_tick == frame2_tick
        assert_eq!(
            ticks_e1.changed, frame2_tick,
            "Row 1 was explicitly marked changed; must match frame 2 tick"
        );

        // Other rows (0, 2, 3) must NOT have been updated to frame2_tick!
        assert_ne!(
            ticks_e0.changed, frame2_tick,
            "Row 0 was NOT marked changed"
        );
        assert_ne!(
            ticks_e2.changed, frame2_tick,
            "Row 2 was NOT marked changed"
        );
        assert_ne!(
            ticks_e3.changed, frame2_tick,
            "Row 3 was NOT marked changed"
        );

        assert!(
            ticks_e1.is_changed(frame1_tick, frame2_tick),
            "Bevy is_changed must return true for entity 1"
        );
        assert!(
            !ticks_e0.is_changed(frame1_tick, frame2_tick),
            "Bevy is_changed must return false for entity 0"
        );
    }

    // ============================================================================
    // Focus 6: Allocator Isolation & Foreign Stack Memory Decoupling (Rule 2)
    // ============================================================================

    #[test]
    fn test_foreign_stack_memory_overwrite_decoupling() {
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
            c"StackDecoupleComp".as_ptr(),
            ComponentUuid::from_name("StackDecoupleComp"),
            32,
            16,
        );
        unsafe {
            host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
        };

        let mut command_queue = CommandQueue::default();
        let world_reg = world.resource::<ModRegistry>().clone();
        let mut bridge =
            CommandQueueBridge::new(&mut command_queue, world.entity_allocator(), &world_reg);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };

        // Push component from a temporary stack buffer, then immediately smash the stack
        fn insert_and_smash_stack(queue: *mut RawCommandQueue, entity: FfiEntity) {
            let mut stack_buf = [0x55u8; 32];
            stack_buf[0] = 0xAA;
            stack_buf[31] = 0xBB;

            let res = unsafe {
                host_cmd_insert_component(
                    queue,
                    entity,
                    c"StackDecoupleComp".as_ptr(),
                    stack_buf.as_ptr(),
                    32,
                )
            };
            assert_eq!(res, FfiResult::OK);

            // Intentionally overwrite stack memory with zeroes
            stack_buf.fill(0x00);
        }

        insert_and_smash_stack(raw_queue, ffi_e);

        // Apply command queue to World
        command_queue.apply(&mut world);

        // Verify Bevy World has the unpoisoned original data [0xAA, 0x55..., 0xBB]
        let comp_id = world
            .resource::<ModRegistry>()
            .get_by_name("StackDecoupleComp")
            .unwrap()
            .component_id;
        let ptr = world
            .entity(Entity::from_bits(ffi_e.0))
            .get_by_id(comp_id)
            .unwrap()
            .as_ptr();

        let slice = unsafe { core::slice::from_raw_parts(ptr, 32) };
        assert_eq!(slice[0], 0xAA);
        assert_eq!(slice[1], 0x55);
        assert_eq!(slice[31], 0xBB);
    }

    #[test]
    fn test_discarded_command_queue_memory_leak_defense() {
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
            c"HeavyAllocComp".as_ptr(),
            ComponentUuid::from_name("HeavyAllocComp"),
            256,
            32,
        );
        unsafe {
            host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
        };

        // Queue 500 insertions of 256-byte components, then drop the CommandQueue WITHOUT applying
        {
            let mut command_queue = CommandQueue::default();
            let world_reg = world.resource::<ModRegistry>().clone();
            let mut bridge =
                CommandQueueBridge::new(&mut command_queue, world.entity_allocator(), &world_reg);
            let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

            let dummy_data = [0x77u8; 256];
            for _ in 0..500 {
                let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
                unsafe {
                    host_cmd_insert_component(
                        raw_queue,
                        ffi_e,
                        c"HeavyAllocComp".as_ptr(),
                        dummy_data.as_ptr(),
                        256,
                    );
                }
            }
            // command_queue is dropped here without apply
        }
        // If AlignedBuffer leaked or panicked on drop, this would fail or leak under Valgrind/Miri
    }
}
